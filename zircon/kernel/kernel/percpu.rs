// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::pin::Pin;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, compiler_fence};

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
use crate::kernel::thread::{AutoPreemptDisabler, Linebuffer};
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
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub struct PlatformCpuResumeState {
    /// CNTKCTL_EL1 saved by `platform_suspend_timer_curr_cpu` and restored on resume.
    pub cntkctl_el1: AtomicU64,
}

#[cfg(target_arch = "aarch64")]
impl PlatformCpuResumeState {
    fn new() -> Self {
        Self { cntkctl_el1: AtomicU64::new(0) }
    }
}

/// Per-CPU line and flush staging state for kernel stdout (`lib/io`).
#[repr(C)]
pub struct PerCpuStdoutState {
    /// Line buffer for stdout writes when blocking is disallowed (e.g. IRQ context).
    pub irq_linebuffer: UnsafeCell<Linebuffer>,

    /// Staging buffers for flushing stdout lines without stack allocation.
    /// The holder of a slot must not be rescheduled while it holds the slot.
    pub flush_buffers: [UnsafeCell<[u8; Linebuffer::BUFFER_SIZE]>; Self::NUM_FLUSH_BUFFERS],

    /// Nesting depth of active stdout flushes on this CPU. A flush is only counted
    /// while its holder cannot be rescheduled.
    // Safety invariant: Only stored to by `lib/io` when entering or leaving an active flush on
    // this CPU from a context that cannot be rescheduled. A live flush uses `flush_buffers[i]`
    // only while the value is above `i`.
    flush_depth: AtomicUsize,
}

impl PerCpuStdoutState {
    /// Number of per-CPU staging buffers. Deeper nested flushes use the stack.
    pub const NUM_FLUSH_BUFFERS: usize = 1;

    /// Maximum nested stdout flushes on one CPU before lines are dropped.
    pub const MAX_FLUSH_DEPTH: usize = 8;

    fn new() -> Self {
        Self {
            irq_linebuffer: UnsafeCell::new(Linebuffer::new()),
            flush_buffers: [const { UnsafeCell::new([0; Linebuffer::BUFFER_SIZE]) };
                Self::NUM_FLUSH_BUFFERS],
            flush_depth: AtomicUsize::new(0),
        }
    }

    /// Returns the nesting depth of active stdout flushes on this CPU.
    #[inline(always)]
    pub fn flush_depth(&self) -> usize {
        // `Ordering::Relaxed` suffices because `flush_depth` is only stored to on the local CPU;
        // a read from another CPU sees a stale value at worst.
        self.flush_depth.load(Ordering::Relaxed)
    }

    /// Sets the nesting depth of active stdout flushes on this CPU.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` is the current CPU's stdout state.
    /// - The caller cannot be rescheduled during this call, and if increasing the depth, until it
    ///   restores the previous depth.
    /// - No flush that is still live uses a staging buffer at index `depth` or above.
    #[inline(always)]
    pub unsafe fn set_flush_depth(&self, depth: usize) {
        // `Ordering::Relaxed` suffices because `flush_depth` is only stored to on the local CPU;
        // `compiler_fence`s prevent reordering staging-buffer or preemption-state accesses across
        // the store relative to same-CPU interrupt handlers.
        compiler_fence(Ordering::SeqCst);
        self.flush_depth.store(depth, Ordering::Relaxed);
        compiler_fence(Ordering::SeqCst);
    }
}

zr::static_assert!(PerCpuStdoutState::NUM_FLUSH_BUFFERS <= PerCpuStdoutState::MAX_FLUSH_DEPTH);

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
    /// ZST that starts a new cache line for the chain-lock fields below, mirroring
    /// `__CPU_ALIGN` on C++ `percpu::active_cl_transaction`.
    ///
    /// `active_cl_transaction` and `chain_lock_conflict_id` are written by this CPU on every
    /// chain-lock transaction, while other CPUs take `scheduler.queue_lock_` and touch the rest
    /// of `scheduler` under it. Keep none of `scheduler` on a line with them.
    // TODO(https://fxbug.dev/581980797): replace with a padded type once PerCpu is Rust-owned.
    _cl_align: crate::arch_rs::CpuAlignMarker,
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
    /// functions.  See also `LOCKUP_DETECTOR_PER_CPU_STATE`.
    #[pin]
    pub lockup_detector_timer: Timer,
    /// When thread sampling is enabled, we use this timer to periodically mark that the active
    /// thread should be sampled when safe to do so.
    #[pin]
    pub sampling_timer: Timer,
    /// The accumulated memory stall timers for this CPU.
    #[pin]
    pub memory_stall_accumulator: StallAccumulator,
    #[cfg(target_arch = "aarch64")]
    /// See [`PlatformCpuResumeState`] type declaration.
    pub resume_state: PlatformCpuResumeState,
    /// Per-CPU line and flush staging state for kernel stdout (`lib/io`).
    /// See [`PerCpuStdoutState`] for synchronization invariants.
    pub stdout_state: PerCpuStdoutState,
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

const fn field_size<T>(_: fn(&bindings::percpu) -> &T) -> usize {
    size_of::<T>()
}

const fn field_align<T>(_: fn(&bindings::percpu) -> &T) -> usize {
    align_of::<T>()
}

macro_rules! assert_percpu_field {
    ($field:ident, $ty:ty) => {
        zr::static_assert!(offset_of!(PerCpu, $field) == offset_of!(bindings::percpu, $field));
        zr::static_assert!(field_size(|p| &p.$field) == size_of::<$ty>());
        zr::static_assert!(field_align(|p| &p.$field) == align_of::<$ty>());
    };
}

// Compile-time layout assertions against C++ `struct percpu` via bindgen.
zr::static_assert!(size_of::<PerCpu>() == size_of::<bindings::percpu>());
zr::static_assert!(align_of::<PerCpu>() == align_of::<bindings::percpu>());

assert_percpu_field!(timer_queue, TimerQueue);
assert_percpu_field!(search_set, CpuSearchSet);
assert_percpu_field!(scheduler, Scheduler);
#[cfg(with_lock_dep)]
assert_percpu_field!(lock_state, ThreadLockState);
assert_percpu_field!(active_cl_transaction, AtomicPtr<ChainLockTransaction>);
assert_percpu_field!(chain_lock_conflict_id, AtomicU64);
// Other CPUs take `scheduler.queue_lock_` to wake threads onto this CPU and to steal work from it,
// while this CPU writes `active_cl_transaction` and `chain_lock_conflict_id` on every chain-lock
// transaction. If they share a cache line, every cross-CPU wake bounces that line away from this
// CPU. A change that moved `queue_lock_` onto that line cost FutexWakeOne 15% on x64. C++
// `percpu::AssertLayout` checks the same thing, including the full extent of `queue_lock_`.
const QUEUE_LOCK_LINE: usize =
    (offset_of!(PerCpu, scheduler) + Scheduler::QUEUE_LOCK_OFFSET) / MAX_CACHE_LINE;
zr::static_assert!(offset_of!(PerCpu, active_cl_transaction) % MAX_CACHE_LINE == 0);
zr::static_assert!(QUEUE_LOCK_LINE < offset_of!(PerCpu, active_cl_transaction) / MAX_CACHE_LINE);
zr::static_assert!(QUEUE_LOCK_LINE < offset_of!(PerCpu, chain_lock_conflict_id) / MAX_CACHE_LINE);
assert_percpu_field!(gstats, GuestStats);
assert_percpu_field!(stats, UnsafeCell<CpuStats>);
assert_percpu_field!(idle_power_thread, IdlePowerThread);
assert_percpu_field!(counters, *mut i64);
assert_percpu_field!(dpc_runner, DpcRunner);
assert_percpu_field!(vm_page_counts, VmPageCounts);
assert_percpu_field!(lockup_detector_timer, Timer);
assert_percpu_field!(sampling_timer, Timer);
assert_percpu_field!(memory_stall_accumulator, StallAccumulator);
#[cfg(target_arch = "aarch64")]
assert_percpu_field!(resume_state, PlatformCpuResumeState);
assert_percpu_field!(stdout_state, PerCpuStdoutState);

zr::static_assert!(
    PerCpuStdoutState::NUM_FLUSH_BUFFERS == bindings::PerCpuStdoutStateLayout_kNumFlushBuffers
);
zr::static_assert!(
    PerCpuStdoutState::MAX_FLUSH_DEPTH == bindings::PerCpuStdoutStateLayout_kMaxFlushDepth
);
zr::static_assert!(
    offset_of!(PerCpuStdoutState, irq_linebuffer)
        == bindings::PerCpuStdoutStateLayout_kIrqLinebufferOffset
);
zr::static_assert!(
    offset_of!(PerCpuStdoutState, flush_buffers)
        == bindings::PerCpuStdoutStateLayout_kFlushBuffersOffset
);
zr::static_assert!(
    offset_of!(PerCpuStdoutState, flush_depth)
        == bindings::PerCpuStdoutStateLayout_kFlushDepthOffset
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
            _cl_align: crate::arch_rs::CpuAlignMarker,
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
            #[cfg(target_arch = "aarch64")]
            resume_state: PlatformCpuResumeState::new(),
            stdout_state: PerCpuStdoutState::new(),
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
    /// during early boot (`lk_main` before `current_become_idle()`) prior to any concurrent
    /// per-CPU accesses.
    pub unsafe fn initialize_boot() {
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
    /// (`lk_main` before `thread::current_become_idle()`) after `PerCpu::initialize_boot`, kernel
    /// heap initialization, and `CpuDistanceMap::Initialize` (plus any
    /// `CpuDistanceMap::set_distance_threshold` calls) have succeeded with no concurrent mutation
    /// of the global distance map, and before any secondary processors are started.
    pub unsafe fn initialize_secondaries_begin() {
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
                    // SAFETY: `i < processor_count` indexes an initialized `PerCpu` in `index_ptr`.
                    // Runs single-threaded on CPU 0 with interrupts disabled before secondary CPUs
                    // start, before schedulers become active, and before `CpuSearchSet::initialize`
                    // reads `PERF_SCALES`.
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
        // SAFETY: `initialize_secondaries_begin` runs at most once on CPU 0 with interrupts
        // disabled after `CpuDistanceMap::Initialize` and any `set_distance_threshold` calls have
        // succeeded with no concurrent map mutation, and before any calls to
        // `CpuSearchSet::initialize`, `cluster`, or `dump_clusters`.
        unsafe {
            CpuSearchSet::auto_cluster(processor_count);
        }
        CpuSearchSet::dump_clusters();

        // Initialize the search set for each CPU.
        dprintf!(INFO, "CPU search order:\n");
        for i in 0..(processor_count as cpu_num_t) {
            // SAFETY: `i < processor_count` indexes an initialized `PerCpu` in `index_ptr`.
            let cpu = unsafe { &**index_ptr.add(i as usize) };
            // SAFETY: Called on CPU 0 with interrupts disabled after `CpuDistanceMap`
            // initialization (and `set_distance_threshold`), `set_perf_scale`, and `auto_cluster`
            // have completed with no concurrent mutations. Secondary CPUs have not started,
            // interrupts are disabled (preventing re-entrant IRQ access), and no `const_iterator`
            // slice is live on `cpu.search_set`.
            unsafe {
                cpu.search_set.initialize(i, processor_count);
            }
            cpu.search_set.dump();

            let cluster = cpu.search_set.cluster();
            // SAFETY: Called during single-threaded early boot on CPU 0 before secondary CPUs
            // start or concurrent access to `cpu.scheduler` is possible.
            unsafe {
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
    pub unsafe fn initialize_secondary_finish() {
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
