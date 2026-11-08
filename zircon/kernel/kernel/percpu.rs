// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::pin::Pin;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use debug::dprintf;
use percpu_bindings as bindings;
use pin_init::{PinInit, pin_data, pin_init};

use crate::arch_rs::MAX_CACHE_LINE;
use crate::counters::CounterArena;
pub use crate::kconcurrent::ChainLockTransaction;
use crate::kernel::cpu_distance_map::CpuDistanceMap;
pub use crate::kernel::cpu_search_set::CpuSearchSet;
pub use crate::kernel::dpc::DpcRunner;
use crate::kernel::idle_power_thread::IdlePowerThread;
use crate::kernel::scheduler::SchedProcessingRate;
pub use crate::kernel::scheduler::Scheduler;
pub use crate::kernel::stats::{CpuStats, GuestStats};
use crate::kernel::thread::AutoPreemptDisabler;
pub use crate::kernel::timer::TimerQueue;
use crate::kernel::timer::{Timer, ZX_CLOCK_BOOT, ZX_CLOCK_MONOTONIC};
use crate::kernel::types::{SMP_MAX_CPUS, cpu_num_t};
#[cfg(with_lock_dep)]
pub use crate::lockdep_rs::ThreadLockState;
#[cfg(with_lock_dep)]
use crate::lockdep_rs::system_init_thread_lock_state;
use crate::stall::StallAccumulator;
use crate::topology::get_performance_class;
use crate::vm::page_state::VmPageCounts;

/// C-style type alias for [`GuestStats`].
#[allow(non_camel_case_types)]
pub type guest_stats = GuestStats;

/// C-style type alias for [`CpuStats`].
#[allow(non_camel_case_types)]
pub type cpu_stats = CpuStats;

/// Holds platform-specific, per-cpu state used to suspend/resume a CPU.
#[repr(C)]
pub struct PlatformCpuResumeState {
    /// CNTKCTL_EL1 saved by `platform_suspend_timer_curr_cpu` and restored on resume.
    #[cfg(target_arch = "aarch64")]
    pub cntkctl_el1: AtomicU64,
    #[cfg(not(target_arch = "aarch64"))]
    pub _opaque: bindings::PlatformCpuResumeState,
}

impl PlatformCpuResumeState {
    fn new() -> Self {
        Self {
            #[cfg(target_arch = "aarch64")]
            cntkctl_el1: AtomicU64::new(0),
            #[cfg(not(target_arch = "aarch64"))]
            _opaque: bindings::PlatformCpuResumeState::default(),
        }
    }
}

/// Rust representation of the C++ `struct percpu`.
#[pin_data]
#[repr(C)]
pub struct PerCpu {
    /// ZST to force alignment to target arch cache lines.
    _align: crate::arch_rs::CpuAlignMarker,
    /// Each CPU maintains a per-cpu queue of timers.
    #[pin]
    pub timer_queue: TimerQueue,
    /// per cpu search set
    #[pin]
    pub search_set: CpuSearchSet,
    /// per cpu scheduler
    #[pin]
    pub scheduler: Scheduler,
    #[cfg(with_lock_dep)]
    /// state for runtime lock validation when in irq context
    #[pin]
    pub lock_state: ThreadLockState,
    /// The state of the currently active chain-lock transaction if any, null
    /// otherwise.
    // Safety invariant: Either null or points to a live, stack-pinned `ChainLockTransaction`
    // registered on this CPU with interrupts disabled and no mutable aliases.
    active_cl_transaction: AtomicPtr<ChainLockTransaction>,
    /// The chainlock conflict ID generator for this CPU.  This field is used by
    /// threads to know when to retry a ChainLockTransaction when a conflict is
    /// encountered.  See lib/kconcurrent for more details.
    pub chain_lock_conflict_id: AtomicU64,
    /// guest entry/exit statistics
    pub gstats: GuestStats,
    /// thread/cpu level statistics.
    ///
    /// Wrapped in [`UnsafeCell`] because [`PerCpu::get`] returns a shared `&PerCpu` reference
    /// while the owning CPU (or another CPU holding the scheduler queue lock) may concurrently
    /// mutate its `stats`. Synchronization requirements vary across fields: energy fields are
    /// guarded by the scheduler queue lock, while time and event counter fields are updated via
    /// relaxed atomic operations and read via [`CpuStats::read_atomic_fields_relaxed`] or
    /// [`CpuStats::copy_from_relaxed`].
    pub stats: UnsafeCell<CpuStats>,
    /// per cpu idle/power thread
    #[pin]
    pub idle_power_thread: IdlePowerThread,
    /// kernel counters arena
    pub counters: *mut i64,
    /// Each cpu maintains a DpcRunner.
    #[pin]
    pub dpc_runner: DpcRunner,
    /// Page state counts are percpu because they change frequently and we don't want to pay for
    /// either heavy synchronization, or transferring the counters between CPUs a lot. Using
    /// percpu relaxed atomics we minimize the need for synchronization (no locks, interrupt or
    /// preemption disabling needed), and avoid cache line bouncing the counters around
    /// different CPUs.
    ///
    /// While it's OK for an observer to temporarily see incorrect values, the counts need to
    /// eventually quiesce. It's important that we don't "drop" changes and that the values don't
    /// drift over time.
    ///
    /// When modifying it is not necessary to disable preemption, and can just use
    /// `PerCpu::get_current()` and then modify the counts. In the unlikely event a CPU migration
    /// happens this does not effect correctness, and paying for the rare cache line transfer
    /// is preferable to consistently paying to avoid migration.
    ///
    /// When reading, use [`PerCpu::for_each_preempt_disable`]. Although it is not possible to
    /// guarantee a consistent snapshot of these counters, it should be good enough for diagnostic
    /// uses.
    pub vm_page_counts: VmPageCounts,
    /// lockup_detector state.
    ///
    /// Every active CPU wakes up periodically (even during periods of suspend-to-idle) to record a
    /// heartbeat, as well as to check to see if any of its peers are showing signs of problems.
    /// The lockup detector timer is the timer used for this.
    ///
    /// This field is not a member of LockupDetectorState because Timer depends on
    /// SpinLock, which depends on lockup_detector.  By pulling it out of
    /// LockupDetectorState we can inline performance critical lockup_detector
    /// functions.  See also gLockupDetectorPerCpuState.
    #[pin]
    pub lockup_detector_timer: Timer,
    /// When thread sampling is enabled, we use this timer to periodically mark that the active
    /// thread should be sampled when safe to do so.
    #[pin]
    pub sampling_timer: Timer,
    /// The accumulated memory stall timers for this CPU.
    #[pin]
    pub memory_stall_accumulator: StallAccumulator,
    /// See [`PlatformCpuResumeState`] type declaration.
    pub resume_state: PlatformCpuResumeState,
}

// SAFETY: `PerCpu` instances live for `'static` and are shared across CPUs via `&'static PerCpu`.
// `counters` is initialized once in `PerCpu::init` before any shared reference exists and is
// read-only thereafter (pointing to atomic `i64` slots in `kcounters_arena`). Every other field
// mutated after construction is wrapped in `UnsafeCell` / `zr::Opaque` / atomic primitives and
// synchronized by its subsystem lock, per-CPU IRQ discipline, or relaxed atomics.
unsafe impl Sync for PerCpu {}

/// Wrapper around the single-entry early-boot processor index table.
#[repr(transparent)]
struct BootIndexStorage(UnsafeCell<*mut PerCpu>);

// SAFETY: Constant single-entry early-boot index table holding the raw pointer to
// `BOOT_PROCESSOR` with mutable provenance; never mutated after link time.
unsafe impl Sync for BootIndexStorage {}

/// The percpu for the boot processor.
// SAFETY: `.data.cpu_align_exclusive` is defined in `kernel.ld` to pad and align its contents to
// `MAX_CACHE_LINE` so no other data shares a cache line with `BOOT_PROCESSOR`.
#[unsafe(link_section = ".data.cpu_align_exclusive")]
static BOOT_PROCESSOR: lazy_init::LazyInit<PerCpu, lazy_init::BasicCheck> =
    lazy_init::LazyInit::uninit();

/// Pointer to heap memory allocated for additional percpu instances.
///
/// Written once during single-threaded early boot on CPU 0 in `initialize_secondaries_begin`.
static SECONDARY_PROCESSORS: AtomicPtr<PerCpu> = AtomicPtr::new(core::ptr::null_mut());

/// Temporary translation table with one entry for use in early boot.
static BOOT_INDEX: BootIndexStorage = BootIndexStorage(UnsafeCell::new(BOOT_PROCESSOR.as_ptr()));

/// Translates from CPU number to percpu instance. Some or all instances
/// of percpu may be discontiguous.
///
/// `Ordering::Relaxed` suffices for reads and writes because `PROCESSOR_INDEX` is updated once on
/// CPU 0 before secondary CPUs are started, so SMP bring-up establishes cross-CPU happens-before.
// TODO(https://fxbug.dev/575987745): Remove `export_name` once all C++ callers of `percpu::Get`
// are migrated to Rust.
// SAFETY: `rust_percpu_processor_index` is uniquely defined here with pointer layout and alignment
// matching `extern "C" percpu** rust_percpu_processor_index` accessed via `ktl::atomic_ref` in
// `<kernel/percpu.h>`.
#[unsafe(export_name = "rust_percpu_processor_index")]
static PROCESSOR_INDEX: AtomicPtr<*mut PerCpu> = AtomicPtr::new(BOOT_INDEX.0.get());

/// Number of percpu entries.
///
/// `Ordering::Relaxed` suffices for reads and writes because `PROCESSOR_COUNT` is updated once on
/// CPU 0 before secondary CPUs are started, so SMP bring-up establishes cross-CPU happens-before.
// TODO(https://fxbug.dev/575987745): Remove `export_name` once all C++ callers of
// `percpu::processor_count` are migrated to Rust.
// SAFETY: `rust_percpu_processor_count` is uniquely defined here with `usize` layout and alignment
// matching `extern "C" size_t rust_percpu_processor_count` accessed via `ktl::atomic_ref` in
// `<kernel/percpu.h>`.
#[unsafe(export_name = "rust_percpu_processor_count")]
static PROCESSOR_COUNT: AtomicUsize = AtomicUsize::new(1);

zr::static_assert!(
    align_of::<lazy_init::LazyInit<PerCpu, lazy_init::BasicCheck>>() == MAX_CACHE_LINE
);

// Compile-time layout assertions against C++ `struct percpu` via bindgen.
zr::static_assert!(size_of::<PerCpu>() == size_of::<bindings::percpu>());
zr::static_assert!(align_of::<PerCpu>() == align_of::<bindings::percpu>());

zr::static_assert!(offset_of!(PerCpu, timer_queue) == offset_of!(bindings::percpu, timer_queue));
zr::static_assert!(offset_of!(PerCpu, search_set) == offset_of!(bindings::percpu, search_set));
zr::static_assert!(offset_of!(PerCpu, scheduler) == offset_of!(bindings::percpu, scheduler));
#[cfg(with_lock_dep)]
zr::static_assert!(offset_of!(PerCpu, lock_state) == offset_of!(bindings::percpu, lock_state));
zr::static_assert!(
    offset_of!(PerCpu, active_cl_transaction)
        == offset_of!(bindings::percpu, active_cl_transaction)
);
zr::static_assert!(
    offset_of!(PerCpu, chain_lock_conflict_id)
        == offset_of!(bindings::percpu, chain_lock_conflict_id)
);
zr::static_assert!(offset_of!(PerCpu, gstats) == offset_of!(bindings::percpu, gstats));
zr::static_assert!(offset_of!(PerCpu, stats) == offset_of!(bindings::percpu, stats));
zr::static_assert!(
    offset_of!(PerCpu, idle_power_thread) == offset_of!(bindings::percpu, idle_power_thread)
);
zr::static_assert!(offset_of!(PerCpu, counters) == offset_of!(bindings::percpu, counters));
zr::static_assert!(offset_of!(PerCpu, dpc_runner) == offset_of!(bindings::percpu, dpc_runner));
zr::static_assert!(
    offset_of!(PerCpu, vm_page_counts) == offset_of!(bindings::percpu, vm_page_counts)
);
zr::static_assert!(
    offset_of!(PerCpu, lockup_detector_timer)
        == offset_of!(bindings::percpu, lockup_detector_timer)
);
zr::static_assert!(
    offset_of!(PerCpu, sampling_timer) == offset_of!(bindings::percpu, sampling_timer)
);
zr::static_assert!(
    offset_of!(PerCpu, memory_stall_accumulator)
        == offset_of!(bindings::percpu, memory_stall_accumulator)
);
zr::static_assert!(offset_of!(PerCpu, resume_state) == offset_of!(bindings::percpu, resume_state));

// Compile-time layout assertions for member types.
zr::static_assert!(size_of::<TimerQueue>() == size_of::<bindings::TimerQueue>());
zr::static_assert!(align_of::<TimerQueue>() == align_of::<bindings::TimerQueue>());

zr::static_assert!(size_of::<Timer>() == size_of::<bindings::Timer>());
zr::static_assert!(align_of::<Timer>() == align_of::<bindings::Timer>());

zr::static_assert!(size_of::<CpuSearchSet>() == size_of::<bindings::CpuSearchSet>());
zr::static_assert!(align_of::<CpuSearchSet>() == align_of::<bindings::CpuSearchSet>());

zr::static_assert!(size_of::<Scheduler>() == size_of::<bindings::Scheduler>());
zr::static_assert!(align_of::<Scheduler>() == align_of::<bindings::Scheduler>());

#[cfg(with_lock_dep)]
zr::static_assert!(size_of::<ThreadLockState>() == size_of::<bindings::lockdep_ThreadLockState>());
#[cfg(with_lock_dep)]
zr::static_assert!(
    align_of::<ThreadLockState>() == align_of::<bindings::lockdep_ThreadLockState>()
);

zr::static_assert!(size_of::<IdlePowerThread>() == size_of::<bindings::IdlePowerThread>());
zr::static_assert!(align_of::<IdlePowerThread>() == align_of::<bindings::IdlePowerThread>());

zr::static_assert!(size_of::<DpcRunner>() == size_of::<bindings::DpcRunner>());
zr::static_assert!(align_of::<DpcRunner>() == align_of::<bindings::DpcRunner>());

zr::static_assert!(size_of::<StallAccumulator>() == size_of::<bindings::StallAccumulator>());
zr::static_assert!(align_of::<StallAccumulator>() == align_of::<bindings::StallAccumulator>());

zr::static_assert!(
    size_of::<PlatformCpuResumeState>() == size_of::<bindings::PlatformCpuResumeState>()
);
zr::static_assert!(
    align_of::<PlatformCpuResumeState>() == align_of::<bindings::PlatformCpuResumeState>()
);

zr::static_assert!(
    size_of::<ChainLockTransaction>() == size_of::<bindings::ChainLockTransaction>()
);
zr::static_assert!(
    align_of::<ChainLockTransaction>() == align_of::<bindings::ChainLockTransaction>()
);

impl PerCpu {
    /// Returns an in-place initializer for the [`PerCpu`] instance of `cpu_num`.
    ///
    /// # Safety
    ///
    /// Must only be called once for each `cpu_num` during single-threaded early boot
    /// initialization (`initialize_boot` / `initialize_secondaries_begin`) before concurrent
    /// per-CPU access to `cpu_num`'s state or counters occurs.
    unsafe fn init(cpu_num: cpu_num_t) -> impl PinInit<Self, core::convert::Infallible> {
        assert!((cpu_num as usize) < SMP_MAX_CPUS);
        let base = pin_init!(Self {
            _align: crate::arch_rs::CpuAlignMarker,
            timer_queue <- TimerQueue::init(),
            search_set <- CpuSearchSet::init(),
            scheduler <- Scheduler::init(),
            #[cfg(with_lock_dep)]
            lock_state <- ThreadLockState::init(),
            active_cl_transaction: AtomicPtr::new(core::ptr::null_mut()),
            chain_lock_conflict_id: AtomicU64::new(1),
            gstats: GuestStats::new(),
            stats: UnsafeCell::new(CpuStats::new()),
            idle_power_thread <- IdlePowerThread::init(),
            counters: CounterArena::new().cpu_data(cpu_num as usize),
            dpc_runner <- DpcRunner::new(),
            vm_page_counts: VmPageCounts::default(),
            lockup_detector_timer <- Timer::init(ZX_CLOCK_BOOT),
            sampling_timer <- Timer::init(ZX_CLOCK_MONOTONIC),
            memory_stall_accumulator <- StallAccumulator::init(),
            resume_state: PlatformCpuResumeState::new(),
        });
        // SAFETY: `assert!` above guarantees `cpu_num < SMP_MAX_CPUS`, `base` initializes every
        // member field of `*slot` in declaration order, and the caller guarantees exclusive single-
        // threaded boot initialization for `cpu_num` without ever forming `&mut Self`.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let Ok(()) = PinInit::__pinned_init(base, slot);
                (*slot).scheduler.set_this_cpu(cpu_num);
                (*slot).idle_power_thread.set_this_cpu(cpu_num);

                #[cfg(with_lock_dep)]
                {
                    // Initialize the lockdep tracking state for irq context.
                    system_init_thread_lock_state(&(*slot).lock_state);
                }

                Ok(())
            })
        }
    }

    /// Returns a reference to the percpu instance for given CPU number.
    ///
    /// Prefer [`Self::get_current`] when accessing the calling CPU's instance.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `(cpu_num as usize) < Self::processor_count()`.
    #[inline]
    pub unsafe fn get(cpu_num: cpu_num_t) -> &'static Self {
        debug_assert!((cpu_num as usize) < Self::processor_count());
        // SAFETY: The caller guarantees `(cpu_num as usize) < Self::processor_count()`, and
        // `PROCESSOR_INDEX` points to at least `PROCESSOR_COUNT` valid, initialized `'static`
        // `PerCpu` pointers. `Ordering::Relaxed` suffices because cross-CPU visibility is
        // established before secondary CPUs enter `initialize_secondary_finish` and CPU 0 executes
        // single-threaded in program order.
        unsafe { &**PROCESSOR_INDEX.load(Ordering::Relaxed).add(cpu_num as usize) }
    }

    /// Returns a reference to the percpu instance for the calling CPU.
    ///
    /// This is the preferred accessor when the current CPU is meant, as it reads the architecture
    /// per-CPU pointer directly without indexing `PROCESSOR_INDEX`.
    #[inline(always)]
    pub fn get_current() -> &'static Self {
        crate::arch_rs::get_curr_percpu()
    }

    /// Returns the number of percpu instances.
    #[inline(always)]
    pub fn processor_count() -> usize {
        // `Ordering::Relaxed` suffices because `PROCESSOR_COUNT` is written once on CPU 0 before
        // secondary CPUs start.
        PROCESSOR_COUNT.load(Ordering::Relaxed)
    }

    /// Called once during early init by the boot processor to initialize the percpu data for the
    /// boot processor.
    ///
    /// # Safety
    ///
    /// Must only be called once on the boot processor (`cpu_num == 0`) with interrupts disabled
    /// during early boot (`lk_main` before `Thread::Current::BecomeIdle()`) prior to any
    /// concurrent per-CPU accesses.
    unsafe fn initialize_boot() {
        // SAFETY: Caller guarantees single-threaded early boot execution on CPU 0 with interrupts
        // disabled before secondary CPUs are started.
        unsafe {
            let Ok(()) = Pin::static_ref(&BOOT_PROCESSOR).init_pin(Self::init(0));
            // Install a pointer to the boot CPU's high-level percpu struct in its low-level percpu
            // struct.
            crate::arch_rs::setup_percpu(0, BOOT_PROCESSOR.as_ptr());
        }
    }

    /// Called once by the boot processor after heap init to initialize the percpu data for the
    /// secondary processors.
    ///
    /// # Safety
    ///
    /// Must only be called once on the boot processor (`cpu_num == 0`) with interrupts disabled
    /// (`lk_main` before `Thread::Current::BecomeIdle()`) after `PerCpu::initialize_boot`, kernel
    /// heap initialization, and `CpuDistanceMap::Initialize` have succeeded, and before any
    /// secondary processors are started.
    unsafe fn initialize_secondaries_begin() {
        // SAFETY: Caller guarantees `CpuDistanceMap::Initialize` has succeeded.
        let processor_count = unsafe { CpuDistanceMap::get() }.cpu_count();
        assert!(processor_count > 0 && processor_count <= SMP_MAX_CPUS);

        let index_size = size_of::<*mut Self>() * processor_count;
        let index_layout = Layout::from_size_align(index_size, MAX_CACHE_LINE)
            .expect("invalid percpu index layout");
        // SAFETY: `index_layout` has non-zero size (`index_size > 0`), and slot `0` is inbounds of
        // `index_ptr`.
        let index_ptr = unsafe {
            let ptr = kalloc::alloc(index_layout)
                .expect("failed to allocate percpu processor index")
                .as_ptr()
                .cast::<*mut Self>();
            let _ = BOOT_PROCESSOR.get();
            *ptr = BOOT_PROCESSOR.as_ptr();
            ptr
        };

        zr::static_assert!(MAX_CACHE_LINE.is_multiple_of(align_of::<PerCpu>()));

        let bytes = size_of::<Self>() * (processor_count - 1);
        if bytes != 0 {
            let secondary_layout = Layout::from_size_align(bytes, MAX_CACHE_LINE)
                .expect("invalid secondary percpu layout");
            // SAFETY: `secondary_layout` has non-zero size (`bytes != 0`); runs once on CPU 0 with
            // interrupts disabled before secondary CPUs start.
            let secondary_processors = unsafe {
                let ptr = kalloc::alloc(secondary_layout)
                    .expect("failed to allocate secondary percpu array")
                    .as_ptr()
                    .cast::<Self>();
                // `Ordering::Relaxed` suffices because this store runs once during single-threaded
                // boot on CPU 0 before secondary CPUs start.
                SECONDARY_PROCESSORS.store(ptr, Ordering::Relaxed);
                ptr
            };

            // Construct the secondary percpu instances and add them to the index.
            for i in 1..(processor_count as cpu_num_t) {
                // SAFETY: `i` is in `1..processor_count`, so `i - 1` is inbounds of
                // `secondary_processors` and `i` is inbounds of `index_ptr`. Each `PerCpu` is
                // constructed in place via raw pointer without forming `&mut Self`.
                unsafe {
                    let slot = secondary_processors.add((i - 1) as usize);
                    let Ok(()) = PinInit::__pinned_init(Self::init(i), slot);
                    *index_ptr.add(i as usize) = slot;
                }
            }
        }

        // Compute the processing rate of each CPU.
        {
            if let Ok(mut performance_class) =
                kalloc::Box::<[u8]>::try_new_zeroed_slice(processor_count)
            {
                let mut max_performance_class: u8 = 0;
                for i in 0..(processor_count as cpu_num_t) {
                    let idx = i as usize;
                    performance_class[idx] = get_performance_class(i);
                    max_performance_class =
                        core::cmp::max(performance_class[idx], max_performance_class);
                }

                dprintf!(INFO, "CPU processing rates:\n");
                for i in 0..(processor_count as cpu_num_t) {
                    let processing_rate = SchedProcessingRate::from(ffl::from_ratio(
                        i32::from(performance_class[i as usize]) + 1,
                        i32::from(max_performance_class) + 1,
                    ));
                    // SAFETY: `i < processor_count` indexes an initialized `PerCpu` in `index_ptr`
                    // during single-threaded boot on CPU 0 before schedulers become active.
                    unsafe {
                        (&**index_ptr.add(i as usize))
                            .scheduler
                            .initialize_processing_rate(processing_rate);
                        CpuSearchSet::set_perf_scale(i, processing_rate.raw_value());
                    }
                    dprintf!(INFO, "CPU {:2}: {}\n", i, processing_rate);
                }
            } else {
                dprintf!(
                    INFO,
                    "Failed to allocate temp buffer, using default performance for all CPUs\n"
                );
            }
        }

        // Determine the clusters before initializing the CPU search sets.
        // SAFETY: Called once on CPU 0 after `CpuDistanceMap::Initialize` and before secondary CPUs
        // start.
        unsafe {
            CpuSearchSet::auto_cluster(processor_count);
        }
        CpuSearchSet::dump_clusters();

        // Initialize the search set for each CPU.
        dprintf!(INFO, "CPU search order:\n");
        for i in 0..(processor_count as cpu_num_t) {
            // SAFETY: `i < processor_count` indexes an initialized `PerCpu` in `index_ptr`; called
            // on CPU 0 after `auto_cluster` and before secondary CPUs start, mutating via interior
            // mutability through `&Self` without forming `&mut Self`.
            unsafe {
                let cpu = &**index_ptr.add(i as usize);
                cpu.search_set.initialize(i, processor_count);
                cpu.search_set.dump();

                let cluster = cpu.search_set.cluster();
                cpu.scheduler.set_cluster(cluster);
            }
        }

        // `Ordering::Relaxed` suffices because `initialize_secondaries_begin` completes on CPU 0
        // before secondary CPUs are brought up, establishing cross-CPU visibility without atomic
        // release/acquire synchronization here.
        PROCESSOR_INDEX.store(index_ptr, Ordering::Relaxed);
        PROCESSOR_COUNT.store(processor_count, Ordering::Relaxed);
    }

    /// Called on each secondary processor immediately after booting or on hotplug re-entry.
    ///
    /// # Safety
    ///
    /// Must only be called on a secondary processor with interrupts disabled immediately after
    /// booting or during CPU hotplug re-entry, after `initialize_secondaries_begin` has completed
    /// on the boot processor.
    unsafe fn initialize_secondary_finish() {
        let cpu_num = crate::arch_rs::curr_cpu_num();
        assert!((cpu_num as usize) < Self::processor_count());
        // SAFETY: `assert!` above guarantees `cpu_num < Self::processor_count()` (which also
        // implies `cpu_num < SMP_MAX_CPUS` from `Self::init`), interrupts are disabled on the
        // calling CPU, and `PROCESSOR_INDEX` holds initialized `'static` `PerCpu` pointers.
        unsafe {
            crate::arch_rs::setup_percpu(
                cpu_num,
                *PROCESSOR_INDEX.load(Ordering::Relaxed).add(cpu_num as usize),
            );
        }
    }

    /// Returns the currently active [`ChainLockTransaction`] pointer on this CPU, or null if none
    /// is active.
    #[inline(always)]
    pub fn active_cl_transaction(&self) -> *mut ChainLockTransaction {
        // `Ordering::Relaxed` suffices because `active_cl_transaction` is only accessed on the
        // local CPU while interrupts are disabled.
        self.active_cl_transaction.load(Ordering::Relaxed)
    }

    /// Sets the currently active [`ChainLockTransaction`] pointer on this CPU.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` is the current CPU's [`PerCpu`] instance and interrupts are disabled on the
    ///   current CPU.
    /// - `ptr` is either null or points to a valid, initialized, stack-pinned
    ///   [`ChainLockTransaction`] owned by the current execution context that remains valid,
    ///   pinned, and not mutably aliased (with interrupts disabled) until cleared or replaced by a
    ///   subsequent call to `set_active_cl_transaction`.
    #[inline(always)]
    pub unsafe fn set_active_cl_transaction(&self, ptr: *mut ChainLockTransaction) {
        // `Ordering::Relaxed` suffices because `active_cl_transaction` is only accessed on the
        // local CPU while interrupts are disabled.
        self.active_cl_transaction.store(ptr, Ordering::Relaxed);
    }

    /// Calls `func` with the current CPU's percpu struct with preemption disabled.
    pub fn with_current_preempt_disable<F: FnOnce(&Self)>(func: F) {
        let _preempt_disable = AutoPreemptDisabler::new();
        func(Self::get_current());
    }

    /// Calls `func` once per CPU with each CPU's percpu struct with preemption disabled.
    pub fn for_each_preempt_disable<F: FnMut(cpu_num_t, &Self)>(func: F) {
        let _preempt_disable = AutoPreemptDisabler::new();
        Self::for_each(func);
    }

    /// Returns an iterator over all `(cpu_num, &PerCpu)` pairs in `0..Self::processor_count()`.
    #[inline]
    pub fn iter() -> impl Iterator<Item = (cpu_num_t, &'static Self)> {
        (0..(Self::processor_count() as cpu_num_t)).map(|cpu_num| {
            // SAFETY: `cpu_num` is in `0..Self::processor_count()` by the range bound.
            (cpu_num, unsafe { Self::get(cpu_num) })
        })
    }

    /// Calls `func` once per CPU with each CPU's percpu struct.
    pub fn for_each<F: FnMut(cpu_num_t, &Self)>(mut func: F) {
        Self::iter().for_each(|(cpu_num, percpu)| func(cpu_num, percpu));
    }
}

/// FFI entry point for `percpu::InitializeBoot()`.
///
/// # Safety
///
/// Must only be called once on the boot processor (`cpu_num == 0`) with interrupts disabled during
/// early boot (`lk_main` before `Thread::Current::BecomeIdle()`) prior to any concurrent per-CPU
/// accesses.
// TODO(https://fxbug.dev/575987745): Remove once C++ `thread_init_early()` is migrated to Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_percpu_initialize_boot() {
    // SAFETY: Caller upholds `PerCpu::initialize_boot` invariants.
    unsafe { PerCpu::initialize_boot() };
}

/// FFI entry point for `percpu::InitializeSecondariesBegin()`.
///
/// # Safety
///
/// Must only be called once on the boot processor (`cpu_num == 0`) with interrupts disabled
/// (`lk_main` before `Thread::Current::BecomeIdle()`) after `percpu::InitializeBoot()`, kernel
/// heap initialization, and `CpuDistanceMap::Initialize` have succeeded, and before any secondary
/// processors are started.
// TODO(https://fxbug.dev/575987745): Remove once C++ `kernel_init()` is migrated to Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_percpu_initialize_secondaries_begin() {
    // SAFETY: Caller upholds `PerCpu::initialize_secondaries_begin` invariants.
    unsafe { PerCpu::initialize_secondaries_begin() };
}

/// FFI entry point for `percpu::InitializeSecondaryFinish()`.
///
/// # Safety
///
/// Must only be called on a secondary processor with interrupts disabled immediately after
/// booting or during CPU hotplug re-entry, after `percpu::InitializeSecondariesBegin()` has
/// completed on the boot processor.
// TODO(https://fxbug.dev/575987745): Remove once C++ `Thread::SecondaryCpuInitEarly()` is migrated
// to Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_percpu_initialize_secondary_finish() {
    // SAFETY: Caller upholds `PerCpu::initialize_secondary_finish` invariants.
    unsafe { PerCpu::initialize_secondary_finish() };
}
