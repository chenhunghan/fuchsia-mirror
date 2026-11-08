// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! MP (multiprocessor) subsystem interface.

use super::deadline::{Deadline, TimerSlack};
use super::event::Event;
use super::idle_power_thread::State as IdlePowerThreadState;
use super::percpu::PerCpu;
use super::thread::{self, AutoPreemptDisabler, Thread};
use super::types::{highest_cpu_set, is_valid_cpu_num, remove_cpu_from_mask};
use super::wait_queue::Interruptible;
use super::{scheduler, stats};
use crate::arch_rs::{
    CpuAlignMarker, arch_interrupt_restore, arch_interrupt_save, blocking_disallowed, curr_cpu_num,
    ints_disabled, max_num_cpus, set_blocking_disallowed,
};
use crate::kconcurrent::{IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned};
use crate::lockup_detector::{
    can_dump_registers_and_backtrace, dump_registers_and_backtrace_stdout, lockup_percpu_shutdown,
};
use crate::platform_rs::power::{
    PanicStartHaltOtherCpus, PlatformHaltAction, ZirconCrashReason, platform_halt,
    platform_panic_start_with_option,
};
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::ffi::c_void;
use core::fmt::Write as _;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use debug::ltracef;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, StringBuffer};
use ksync::{KCell, LockToken};
use lazy_init::LazyInit;

pub use super::types::{cpu_mask_t, cpu_num_t};
use zx_status::Status;
use zx_types::{ZX_TIME_INFINITE, zx_instant_mono_t, zx_status_t};

const LOCAL_TRACE: u32 = 0;

// Counts the number of times a CPU reschedules using a self-IPI.
crate::counters::define_kcounter!(COUNTER_RESCHEDULE_SELF, "mp.reschedule_self", Sum);

/// Maximum number of CPUs the kernel is built to support; the Rust view of the
/// C++ `SMP_MAX_CPUS` build parameter.
pub const SMP_MAX_CPUS: usize =
    zr::parse_usize(env!("SMP_MAX_CPUS")).expect("SMP_MAX_CPUS invalid");

pub const INVALID_CPU: cpu_num_t = super::types::INVALID_CPU;
pub const CPU_MASK_ALL: cpu_mask_t = super::types::CPU_MASK_ALL;

/// C++ `MpUnplugEvent`, opaque here.  An unplugging CPU is handed one and signals
/// it once its state is flushed.
#[repr(C)]
pub struct MpUnplugEvent {
    _opaque: [u8; 0],
}

/// Signals `event`, releasing the CPU that requested this CPU's unplug.
///
/// # Safety
///
/// `event` must point to a valid initialized `MpUnplugEvent` (or `AutounsignalMpUnplugEvent`),
/// the calling CPU must have an initialized current `Thread`, and no active
/// `ChainLockTransaction` may be held (`MpUnplugEvent::Signal` asserts
/// `!ChainLockTransaction::Active()` and acquires `WaitQueue`'s lock).
pub unsafe fn unplug_event_signal(event: *mut MpUnplugEvent) {
    // SAFETY: Forwarded to `MpUnplugEvent::Signal()` with caller-verified pointer, thread, and
    // lock-state preconditions.
    unsafe { cpp_mp_unplug_event_signal(event) };
}

/// Clears any pending signal on `event`.
///
/// # Safety
///
/// `event` must point to a valid initialized `MpUnplugEvent` (or `AutounsignalMpUnplugEvent`),
/// the calling CPU must have an initialized current `Thread`, and no active
/// `ChainLockTransaction` may be held.
#[inline]
unsafe fn unplug_event_unsignal(event: *mut MpUnplugEvent) -> Result<(), Status> {
    // SAFETY: Caller guarantees `event` is valid and initialized, the current `Thread` is
    // initialized, and no `ChainLockTransaction` is active.
    Status::ok(unsafe { cpp_mp_unplug_event_unsignal(event) })
}

/// Blocks until `event` is signaled or `deadline` expires.
///
/// # Safety
///
/// `event` must point to a valid initialized `MpUnplugEvent` (or `AutounsignalMpUnplugEvent`)
/// that remains live for the duration of the wait, the calling CPU must have an initialized
/// current `Thread` and execute in a context where blocking is permitted, and no spinlocks or
/// active `ChainLockTransaction` may be held.
#[inline]
unsafe fn unplug_event_wait_deadline(
    event: *mut MpUnplugEvent,
    deadline: zx_instant_mono_t,
    interruptible: Interruptible,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees `event` remains valid for the wait duration, the current `Thread`
    // is initialized, blocking is permitted, and no spinlocks or `ChainLockTransaction` are held.
    Status::ok(unsafe { cpp_mp_unplug_event_wait_deadline(event, deadline, interruptible) })
}

/// Opaque representation of C++ `AutounsignalMpUnplugEvent`.
///
/// Uses [`zr::Opaque`] so interior C++ mutation is sound, uninitialized padding bytes are modeled
/// via `MaybeUninit`, and the type is `!Unpin`.
#[repr(transparent)]
pub struct AutounsignalMpUnplugEvent(
    zr::Opaque<mp_unplug_event_bindings::AutounsignalMpUnplugEvent>,
);

// SAFETY: `AutounsignalMpUnplugEvent` is synchronized internally by `WaitQueue` and its spinlock.
unsafe impl Sync for AutounsignalMpUnplugEvent {}

impl AutounsignalMpUnplugEvent {
    /// Returns a mutable raw pointer to the underlying C++ `MpUnplugEvent` base subobject.
    #[inline]
    pub fn as_raw_mut(&self) -> *mut MpUnplugEvent {
        self.0.get().cast::<MpUnplugEvent>()
    }

    /// Signals the event, waking any thread waiting in [`Self::wait_deadline`].
    ///
    /// # Safety
    ///
    /// `self` must wrap an initialized C++ `AutounsignalMpUnplugEvent` at a pinned address, the
    /// calling CPU must have an initialized current `Thread`, and no active
    /// `ChainLockTransaction` may be held.
    #[inline]
    pub unsafe fn signal(&self) {
        // SAFETY: Caller guarantees `self` wraps a valid initialized event, the current `Thread`
        // is initialized, and no `ChainLockTransaction` is active.
        unsafe { unplug_event_signal(self.as_raw_mut()) };
    }

    /// Clears any pending signal on the event.
    ///
    /// # Safety
    ///
    /// `self` must wrap an initialized C++ `AutounsignalMpUnplugEvent` at a pinned address, the
    /// calling CPU must have an initialized current `Thread`, and no active
    /// `ChainLockTransaction` may be held.
    #[inline]
    pub unsafe fn unsignal(&self) -> Result<(), Status> {
        // SAFETY: Caller guarantees `self` wraps a valid initialized event, the current `Thread`
        // is initialized, and no `ChainLockTransaction` is active.
        unsafe { unplug_event_unsignal(self.as_raw_mut()) }
    }

    /// Blocks until the event is signaled or `deadline` expires.
    ///
    /// # Safety
    ///
    /// `self` must wrap an initialized C++ `AutounsignalMpUnplugEvent` at a pinned address that
    /// remains live for the duration of the wait, the calling CPU must execute in a normal thread
    /// context where blocking is permitted, and no spinlocks or active `ChainLockTransaction` may
    /// be held.
    #[inline]
    pub unsafe fn wait_deadline(
        &self,
        deadline: zx_instant_mono_t,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // SAFETY: Caller upholds all event validity, thread-context, and lock-state
        // preconditions for blocking on `WaitQueue::BlockEtc`.
        unsafe { unplug_event_wait_deadline(self.as_raw_mut(), deadline, interruptible) }
    }
}

unsafe extern "C" {
    fn cpp_mp_unplug_event_signal(event: *mut MpUnplugEvent);
    /// Clears any pending signal on `event`.
    fn cpp_mp_unplug_event_unsignal(event: *mut MpUnplugEvent) -> zx_status_t;
    /// Blocks until `event` is signaled or `deadline` expires.
    fn cpp_mp_unplug_event_wait_deadline(
        event: *mut MpUnplugEvent,
        deadline: zx_instant_mono_t,
        interruptible: Interruptible,
    ) -> zx_status_t;
}

/// When sending inter processor interrupts (IPIs), apis will take a combination of
/// this enum and a bitmask. If `MpIpiTarget::Mask` is used, the mask argument will
/// contain a bitmap of every cpu that should receive the IPI. The other targets
/// serve as shortcuts and potentially optimizations in the lower layers.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpIpiTarget {
    /// Target a specific set of CPUs given by bitmask.
    Mask = 0,
    /// Target all online CPUs.
    All = 1,
    /// Target all online CPUs except the calling CPU.
    ///
    /// Note: Calling with `AllButLocal` requires interrupts to be disabled.
    AllButLocal = 2,
}

impl MpIpiTarget {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Mask,
            1 => Self::All,
            2 => Self::AllButLocal,
            _ => unreachable!("invalid MpIpiTarget: {raw}"),
        }
    }
}

/// IPI types dispatched across processors.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpIpi {
    /// Generic task execution IPI.
    Generic = 0,
    /// Reschedule IPI.
    Reschedule = 1,
    /// Interrupt-only IPI.
    Interrupt = 2,
    /// Halt CPU IPI.
    Halt = 3,
}

zr::static_assert!(core::mem::size_of::<MpIpiTarget>() == 1);
zr::static_assert!(core::mem::size_of::<MpIpi>() == 1);

// Represents a pending task for some number of CPUs to execute.
#[derive(DoublyLinkedListContainable)]
struct MpIpiTask {
    #[dll_node]
    node: DoublyLinkedListNode<MpIpiTask>,
    context: *const MpSyncContext,
}

impl MpIpiTask {
    fn new(context: *const MpSyncContext) -> Self {
        Self { node: DoublyLinkedListNode::new(), context }
    }
}

// Lockdep tracking is disabled on `IpiTaskLock` to match C++ `SpinLock ipi_task_lock`.
// `set_cpu_online` acquires this lock on secondary CPUs during bringup before per-CPU lockdep
// state is initialized.
ksync::declare_singleton_lock!(IpiTaskLock, crate::kernel::spinlock::SpinLock, cpu_align_exclusive,);
// lock for serializing CPU hotplug/unplug operations
crate::ksync_kernel::declare_singleton_mutex!(HotplugLock);

// Global mp state to track what the cpus are up to.
#[repr(C)]
struct MpState {
    _align: CpuAlignMarker,

    // Cpus that are currently online.
    online_cpus: AtomicU32,

    // The list of outstanding tasks for each CPU to execute. Should only be
    // accessed with the ipi_task_lock held
    ipi_task_list:
        LazyInit<KCell<[DoublyLinkedList<NonNull<MpIpiTask>>; SMP_MAX_CPUS], IpiTaskLock>>,

    // Whether each CPU's `ipi_task_list` is non-empty. Only written with `IpiTaskLock` held, so it
    // always matches the list at the moment the lock is released. `sync_exec_raw` reads it without
    // the lock while it waits with interrupts disabled, so the wait loop does not take
    // `IpiTaskLock` on every spin and starve the CPUs that need it to pop or drain tasks.
    ipi_task_pending: [AtomicBool; SMP_MAX_CPUS],

    // Tracks the CPUs that are "ready".
    // Used below in signal_curr_cpu_ready and wait_for_all_cpus_ready.
    ready_cpu_mask: AtomicU32,

    // Signals when all CPUs are ready.
    ready_cpu_event: LazyInit<Event>,
}

// SAFETY: `online_cpus`, `ipi_task_pending`, and `ready_cpu_mask` are atomic, `ready_cpu_event` is
// internally synchronized, `ipi_task_list` is guarded by `IpiTaskLock`, and the `LazyInit` fields
// are initialized once during single-threaded early boot before any concurrent access.
unsafe impl Sync for MpState {}

// The global state structure for the mp subsystem, aligned on cpu cache line to minimize aliasing.
#[unsafe(link_section = ".data.cpu_align_exclusive")]
static MP: MpState = MpState {
    _align: CpuAlignMarker,
    online_cpus: AtomicU32::new(0),
    ipi_task_list: LazyInit::uninit(),
    ipi_task_pending: [const { AtomicBool::new(false) }; SMP_MAX_CPUS],
    ready_cpu_mask: AtomicU32::new(0),
    ready_cpu_event: LazyInit::uninit(),
};

fn initialize_mp_state(_level: init::LkInitLevel) {
    // SAFETY: Single-threaded initialization during early boot at `LK_INIT_LEVEL_EARLIEST`
    // before any secondary CPUs or concurrent threads exist.
    unsafe {
        let _ = Pin::static_ref(&MP.ipi_task_list).init_pin(KCell::pin_init(
            pin_init::pin_init_array_from_fn(|_| DoublyLinkedList::new()),
        ));
        let _ = Pin::static_ref(&MP.ready_cpu_event).init_pin(Event::init_unsignaled());
    }
}

init::lk_init_hook!(mp_early_init, initialize_mp_state, init::LK_INIT_LEVEL_EARLIEST);

/// Converts a CPU ID to a CPU bitmask.
pub const fn cpu_num_to_mask(cpu: cpu_num_t) -> cpu_mask_t {
    if cpu < cpu_mask_t::BITS { 1 << cpu } else { 0 }
}

/// Tracks if a CPU is online and initialized.
///
/// # Safety
///
/// `cpu` must be a valid CPU number.
///
/// When `online` is `false`, any `sync_exec` tasks still queued for `cpu` are drained (i.e.
/// removed and signaled as complete without being executed). Tasks `cpu` has already dequeued are
/// not drained, and must be completed by `cpu` itself, otherwise their callers wait forever. So,
/// either `cpu` must be the calling CPU, with interrupts disabled (and not partway through a
/// dequeued `sync_exec` task), or `cpu` must not be active (and must never have enabled
/// interrupts).
pub unsafe fn set_cpu_online(cpu: cpu_num_t, online: bool) {
    assert!(is_valid_cpu_num(cpu), "cpu {}", cpu);

    // Taking a CPU offline drains its queued tasks (see below), but a task the CPU has already
    // popped can only be completed by the CPU itself.  If the CPU were to stop running partway
    // through such a task (e.g. because it is about to be halted), that task's mp_sync_exec caller
    // would wait forever.  So, either the CPU must be taking itself offline with interrupts disabled
    // (see unplug_current_cpu), or another CPU must be taking it offline after it failed to finish
    // booting (see x86_bringup_aps).  In the latter case, the CPU never enabled interrupts (it does
    // so only after it has reported that it booted), so it cannot have taken a generic IPI, and it
    // was never added to the scheduler.
    debug_assert!(
        online
            || if cpu == curr_cpu_num() {
                ints_disabled()
            } else {
                !scheduler::peek_is_active(cpu)
            }
    );

    ksync::lock!(let mut guard = IpiTaskLock::lock());
    if online {
        MP.online_cpus.fetch_or(cpu_num_to_mask(cpu), Ordering::SeqCst);
    } else {
        MP.online_cpus.fetch_and(!cpu_num_to_mask(cpu), Ordering::SeqCst);
        // Drain any tasks that were queued for this CPU before it went offline, signaling to their
        // sync_exec callers that this CPU is done.  Tasks this CPU already popped are not in the
        // list, and are completed by this CPU (see above).
        // SAFETY: `IpiTaskLock` is the singleton lock guarding `MP.ipi_task_list`, and
        // `MP.ipi_task_list` is pinned in static memory.
        let mut list = unsafe {
            let lists = MP.ipi_task_list.get_mut(guard.as_mut().token_mut());
            Pin::new_unchecked(&mut lists[cpu as usize])
        };
        while let Some(task) = list.as_mut().pop_front() {
            // SAFETY: `task` was enqueued by `sync_exec_raw`, whose stack-allocated `MpSyncContext`
            // remains valid until its bit in `outstanding_cpus` is cleared below. Access via raw
            // pointers so no reference outlives the `fetch_and` release point.
            unsafe {
                let context = (*task.as_ptr()).context;
                (*context).outstanding_cpus.fetch_and(!cpu_num_to_mask(cpu), Ordering::SeqCst);
            }
        }
        MP.ipi_task_pending[cpu as usize].store(false, Ordering::Relaxed);
    }
}

/// Marks `cpu` as offline.
///
/// Any `sync_exec` tasks still queued for `cpu` are drained (i.e. removed and signaled as complete
/// without being executed).
///
/// # Safety
///
/// `cpu` must be a valid CPU number, and either `cpu` must be the calling CPU with interrupts
/// disabled and not in the middle of executing a dequeued `sync_exec` task, or `cpu` must not be
/// active and must never have enabled interrupts (see [`set_cpu_online`]).
pub unsafe fn set_cpu_offline(cpu: cpu_num_t) {
    // SAFETY: The caller guarantees `cpu` is valid and satisfies the `set_cpu_online` offline
    // preconditions.
    unsafe { set_cpu_online(cpu, false) };
}

/// Marks the calling CPU as online and initialized.
pub fn set_curr_cpu_online() {
    // SAFETY: Bringing the calling CPU online has no safety preconditions.
    unsafe { set_cpu_online(curr_cpu_num(), true) };
}

/// Marks the calling CPU as offline.
///
/// Any `sync_exec` tasks still queued for the calling CPU are drained (i.e. removed and signaled
/// as complete without being executed).
///
/// # Safety
///
/// Interrupts must be disabled on the calling CPU.  Tasks the calling CPU has already dequeued are
/// not drained and can only be completed by the calling CPU, so the caller must not be in the
/// middle of executing one (see `mp_set_cpu_online` in kernel/mp.h).
pub unsafe fn set_curr_cpu_offline() {
    // SAFETY: The caller guarantees that interrupts are disabled and that the calling CPU is not
    // executing a dequeued `sync_exec` task.
    unsafe { set_cpu_online(curr_cpu_num(), false) };
}

/// Returns the bitmask of currently online CPUs.
pub fn get_online_mask() -> cpu_mask_t {
    MP.online_cpus.load(Ordering::SeqCst)
}

/// Checks whether a specific CPU is currently online.
pub fn is_cpu_online(cpu: cpu_num_t) -> bool {
    (get_online_mask() & cpu_num_to_mask(cpu)) != 0
}

/// Initializes the MP subsystem.
pub fn init() {}

/// Trigger reschedules on other CPUs. Used mostly by inner threading
/// and scheduler logic.
pub fn reschedule(mut mask: cpu_mask_t, _flags: u32) {
    debug_assert!(ints_disabled());

    let local_cpu = curr_cpu_num();

    ltracef!("local {}, mask {:#x}\n", local_cpu, mask);

    // mask out cpus that do not have an active scheduler, and the local cpu
    mask &= scheduler::peek_active_mask();
    mask &= !cpu_num_to_mask(local_cpu);

    ltracef!("local {}, post mask target now 0x{:x}\n", local_cpu, mask);

    // if we have no work to do, return
    if mask == 0 {
        return;
    }

    crate::arch_rs::mp_reschedule(mask);
}

/// Trigger a reschedule on the calling CPU using an IPI.
pub fn reschedule_self() {
    debug_assert!(ints_disabled());

    let local_cpu = curr_cpu_num();
    debug_assert!(scheduler::peek_is_active(local_cpu));

    ltracef!("local {}\n", local_cpu);

    COUNTER_RESCHEDULE_SELF.add(1);
    crate::arch_rs::mp_reschedule(cpu_num_to_mask(local_cpu));
}

/// Trigger an interrupt on another cpu without a corresponding reschedule.
/// Used by the hypervisor to trigger a vmexit.
pub fn interrupt(target: MpIpiTarget, mask: cpu_mask_t) {
    crate::arch_rs::mp_send_ipi(target, mask, MpIpi::Interrupt);
}

/// Raw function pointer callback for synchronous cross-CPU task execution.
pub type SyncTask = unsafe extern "C" fn(context: *mut core::ffi::c_void);

struct MpSyncContext {
    task: SyncTask,
    task_context: *mut c_void,
    // Mask of which CPUs need to finish the task
    outstanding_cpus: AtomicU32,
}

/// # Safety
///
/// `context` must point to a valid `MpSyncContext` whose `task` and `task_context` are valid
/// to invoke on the current CPU.
unsafe fn mp_sync_task(context: *const MpSyncContext) {
    // SAFETY: The caller (`sync_exec_raw`) guarantees `context` points to a valid
    // `MpSyncContext` on its stack until this CPU's bit in `outstanding_cpus` is cleared.
    // Project through raw pointers so no shared reference outlives `fetch_and`.
    let (task_fn, task_context) = unsafe { ((*context).task, (*context).task_context) };
    // SAFETY: The caller of `sync_exec_raw` guarantees `task_fn` is safe to call with
    // `task_context`.
    unsafe { task_fn(task_context) };
    // use seq-cst atomic to ensure this update is not seen before the
    // side-effects of context.task
    // SAFETY: `context` remains valid on the caller's stack until this `fetch_and` completes.
    unsafe {
        (*context).outstanding_cpus.fetch_and(!cpu_num_to_mask(curr_cpu_num()), Ordering::SeqCst);
    }
}

/// Execute a task on the specified CPUs, and block on the calling
/// CPU until all CPUs have finished the task.
///
/// If `MpIpiTarget::All` or `MpIpiTarget::AllButLocal` is the target, the online CPU
/// mask will be used to determine actual targets.
///
/// Interrupts must be disabled if calling with `MpIpiTarget::AllButLocal` as target
///
/// The callback in `task` will always be called with `blocking_disallowed()`
/// set to true and preemption disabled.
///
/// # Safety
///
/// The caller must ensure that:
/// - `task` is safe to call concurrently from multiple CPUs with `context`.
/// - `context` remains valid for the duration of the execution.
/// - If `target == MpIpiTarget::AllButLocal`, interrupts are disabled on the calling CPU.
pub unsafe fn sync_exec_raw(
    target: MpIpiTarget,
    mut mask: cpu_mask_t,
    task: SyncTask,
    context: *mut core::ffi::c_void,
) {
    const VALID_CPU_MASK: cpu_mask_t = if SMP_MAX_CPUS < cpu_mask_t::BITS as usize {
        (1 << SMP_MAX_CPUS) - 1
    } else {
        cpu_mask_t::MAX
    };

    if target == MpIpiTarget::AllButLocal {
        // targeting all other CPUs but the current one is hazardous
        // if the local CPU may be changed underneath us
        debug_assert!(ints_disabled());
    }

    // Disable interrupts so our current CPU doesn't change.
    let irqstate = arch_interrupt_save();
    libarch::thread_memory_barrier();

    let local_cpu = curr_cpu_num();

    // Create tasks to enqueue (we need one per target due to each containing
    // a linked list node).
    let sync_context =
        MpSyncContext { task, task_context: context, outstanding_cpus: AtomicU32::new(0) };

    // Create an array up to SMP_MAX_CPUs of tasks to run, initializing only
    // the slots for CPUs actually targeted in `mask` to avoid unnecessary work.
    let mut sync_tasks: [MaybeUninit<MpIpiTask>; SMP_MAX_CPUS] =
        [const { MaybeUninit::uninit() }; SMP_MAX_CPUS];

    let targetting_self: bool;
    {
        // Filter the target mask against the online CPUs and enqueue tasks under `IpiTaskLock`.
        // `set_cpu_online` updates `MP.online_cpus` and drains the offline CPU's `ipi_task_list`
        // under this same lock, guaranteeing that a task is never left queued for an offline CPU.
        //
        // Note, the generic IPI is sent after `IpiTaskLock` is released, so a target CPU may go
        // offline (and have its task drained) before the IPI is sent. That is, while a task is
        // never left queued for an offline CPU, an offline CPU may still be sent a generic IPI.
        // This is harmless, since the CPU's task for this call has already been drained, so if
        // the CPU ever takes the IPI, it will not find that task.
        ksync::lock!(
            let mut guard = IpiTaskLock::get().lock_policy::<crate::ksync_kernel::NoIrqSavePolicy>()
        );

        let online = get_online_mask();
        if target == MpIpiTarget::All {
            mask = online;
        } else if target == MpIpiTarget::AllButLocal {
            mask = online & !cpu_num_to_mask(local_cpu);
        } else {
            // Mask any offline CPUs from target list
            mask &= online;
        }

        // Remove self from target lists, since no need to IPI ourselves.
        targetting_self = (mask & cpu_num_to_mask(local_cpu)) != 0;
        mask &= !cpu_num_to_mask(local_cpu);
        mask &= VALID_CPU_MASK;

        // Relaxed is sufficient here because remote CPUs only observe `outstanding_cpus` after
        // popping a task from their `ipi_task_list` (or after the list is drained by
        // `set_cpu_online`), both of which happen under `IpiTaskLock`, whose release below
        // publishes this store.
        sync_context.outstanding_cpus.store(mask, Ordering::Relaxed);

        // SAFETY: `IpiTaskLock` is the singleton lock guarding `MP.ipi_task_list`.
        let lists = unsafe { MP.ipi_task_list.get_mut(guard.as_mut().token_mut()) };
        let mut remaining = mask;
        while remaining != 0 {
            let cpu_id = remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            sync_tasks[cpu_id].write(MpIpiTask::new(&raw const sync_context));
            // SAFETY: `MaybeUninit::as_mut_ptr` is non-null and preserves mutable provenance.
            let task_ptr = unsafe { NonNull::new_unchecked(sync_tasks[cpu_id].as_mut_ptr()) };
            // SAFETY: `MP.ipi_task_list` is pinned in static memory, `IpiTaskLock` is held, and
            // `task_ptr` points to a freshly initialized `MpIpiTask` on the stack that is removed
            // before return.
            unsafe {
                Pin::new_unchecked(&mut lists[cpu_id]).push_back_raw(task_ptr);
            }
            MP.ipi_task_pending[cpu_id].store(true, Ordering::Relaxed);
        }
    }

    // let CPUs know to begin executing
    crate::arch_rs::mp_send_ipi(MpIpiTarget::Mask, mask, MpIpi::Generic);

    if targetting_self {
        // Because the task may trigger a local reschedule, we must run it with
        // preemption disabled.
        let _apd = AutoPreemptDisabler::new();
        let previous_blocking_disallowed = blocking_disallowed();
        set_blocking_disallowed(true);
        // SAFETY: `sync_context` is valid on the stack and `task` is valid per caller contract.
        unsafe { mp_sync_task(&raw const sync_context) };
        set_blocking_disallowed(previous_blocking_disallowed);
    }
    libarch::thread_memory_barrier();

    // we can take interrupts again once we've executed our task
    arch_interrupt_restore(irqstate);

    let ints_disabled = crate::arch_rs::ints_disabled();
    // wait for all other CPUs to be done with the context
    //
    // Acquire pairs with the fetch_and in `mp_sync_task` / `set_cpu_online`, ensuring that each
    // task's side-effects, and the unlinking of each task from its `ipi_task_list`, are visible
    // before we check `in_container` below and before `sync_tasks` go out of scope.
    while sync_context.outstanding_cpus.load(Ordering::Acquire) != 0 {
        // If interrupts are still disabled, we need to attempt to process any
        // tasks queued for us in order to prevent deadlock.
        if ints_disabled {
            // Check whether our task list has work without taking `IpiTaskLock`, so this loop does
            // not starve the CPUs that need the lock to pop or drain tasks. A stale answer only
            // delays the check to the next spin; mbx_generic_irq takes the lock to pop each task.
            if MP.ipi_task_pending[local_cpu as usize].load(Ordering::Relaxed) {
                core::hint::cold_path();
                // Run mbx_generic_irq with preemption disabled to more closely mimic
                // IRQ context.
                let _apd = AutoPreemptDisabler::new();
                let previous_blocking_disallowed = blocking_disallowed();
                set_blocking_disallowed(true);
                // SAFETY: `ints_disabled` is true and preemption/blocking are disabled above.
                unsafe { mbx_generic_irq() };
                set_blocking_disallowed(previous_blocking_disallowed);
                continue;
            }
        }

        libarch::r#yield();
    }
    libarch::thread_memory_barrier();

    // Every enqueued task has been popped and completed (by `mbx_generic_irq`) or drained when
    // its target CPU went offline (by `set_cpu_online`) before clearing its bit in
    // `outstanding_cpus` (which matters because `sync_tasks` are stack-allocated).
    #[cfg(debug_assertions)]
    {
        let mut remaining = mask;
        while remaining != 0 {
            let cpu_id = remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            // SAFETY: `sync_tasks[cpu_id]` was initialized above for every bit set in `mask`, and
            // all remote CPUs have finished accessing it (`outstanding_cpus == 0` with `Acquire`).
            let task = unsafe { sync_tasks[cpu_id].assume_init_ref() };
            debug_assert!(!task.get_node().in_container());
        }
    }
}

/// Executes a closure synchronously on the specified CPUs and blocks on the calling CPU until all
/// target CPUs have finished executing it.
///
/// If [`MpIpiTarget::All`] or [`MpIpiTarget::AllButLocal`] is the target, the online CPU mask
/// will be used to determine actual targets.
///
/// `f` will always be called with [`blocking_disallowed()`] set to true and preemption disabled.
///
/// # Safety
///
/// If `target == MpIpiTarget::AllButLocal`, interrupts must be disabled on the calling CPU.
pub unsafe fn sync_exec<F: Fn() + Sync>(target: MpIpiTarget, mask: cpu_mask_t, f: F) {
    unsafe extern "C" fn sync_exec_trampoline<F: Fn() + Sync>(context: *mut core::ffi::c_void) {
        // SAFETY: `context` points to a valid `F` on the caller's stack. Because `F: Sync`,
        // multiple CPUs can safely execute `closure()` concurrently via shared reference `&F`.
        let closure = unsafe { &*(context as *const F) };
        closure();
    }

    // SAFETY: `&f` remains valid on the stack for the entire duration of `sync_exec_raw`,
    // which blocks synchronously until all target CPUs have finished executing the trampoline
    // (or, for target CPUs that went offline, until the pending task was drained), and the caller
    // upholds the `AllButLocal` interrupt-disabled precondition.
    unsafe {
        sync_exec_raw(
            target,
            mask,
            sync_exec_trampoline::<F>,
            (&raw const f).cast_mut().cast::<core::ffi::c_void>(),
        );
    }
}

/// Unplugs the calling CPU, halting it and taking it offline.
///
/// # Safety
///
/// The caller must ensure that:
/// - This is called on the calling CPU's idle/power thread while taking the calling CPU offline.
/// - Interrupts are disabled on the calling CPU.
/// - No `ChainLockTransaction`, scheduler queue lock, or thread list lock is held.
/// - The calling CPU is not in the middle of executing a dequeued `sync_exec` task.
pub unsafe fn unplug_current_cpu() -> ! {
    // We had better not be holding any OwnedWaitQueues at this point in time
    // (it is unclear how we would have ever obtained any in the first place)
    if cfg!(debug_assertions) {
        // SAFETY: `thread::current_get()` returns a valid pointer to the running thread.
        let current_thread: &Thread = unsafe { &*thread::current_get() };
        single_chain_lock_guard_owned!(
            guard,
            current_thread,
            IRQ_SAVE_OPTION,
            clt_tag!("mp_unplug_current_cpu")
        );
        debug_assert!(guard.proof().owned_wait_queues().is_empty());
    }

    // SAFETY: The caller guarantees interrupts are disabled on the calling CPU.
    unsafe { crate::thread_sampler::sampler_percpu_shutdown() };
    lockup_percpu_shutdown();
    // SAFETY: `PerCpu::get_current().scheduler` is the current CPU's initialized scheduler (whose
    // fields are wrapped in `UnsafeCell`), and the caller upholds the interrupt-disabled and
    // lock-exclusion preconditions.
    unsafe {
        scheduler::Scheduler::migrate_unpinned_threads(
            (&raw const PerCpu::get_current().scheduler).cast_mut(),
        );
    }
    debug_assert!(!scheduler::peek_is_active(curr_cpu_num()));

    // Now that this CPU is no longer active, it is critical that this thread
    // never block.  If this thread blocks, the scheduler may attempt to select
    // this CPU's power thread to run.  Doing so would violate an invariant: tasks
    // may only be scheduled on active CPUs.
    debug_assert!(blocking_disallowed());

    // Mark this CPU as offline and drain any synchronous tasks that were queued for it after it
    // stopped accepting interrupts, signaling to their `sync_exec` callers that this CPU is done.
    // SAFETY: The caller guarantees interrupts are disabled and the current CPU is not mid-task.
    unsafe { set_curr_cpu_offline() };

    // Stop and then shutdown this CPU's platform timer.
    crate::platform_rs::timer::platform_stop_timer();
    crate::platform_rs::timer::platform_shutdown_timer();

    // Shutdown the interrupt controller for this CPU.  On some platforms (arm64 with GIC)
    // receiving an interrupt at a powered off CPU can result in implementation defined behavior
    // (including resetting the whole system).
    // SAFETY: The calling CPU is being shut down with interrupts disabled.
    unsafe { crate::dev_interrupt::shutdown_interrupts_curr_cpu() };

    // Flush all of our caches and signal offline complete.
    // SAFETY: Called on the current CPU's idle/power thread as the final step of CPU offline with
    // interrupts disabled.
    unsafe { PerCpu::get_current().idle_power_thread.flush_and_halt() }
}

/// Hotplug the given cpus.  Blocks until the CPUs are up, or a failure is
/// detected.
///
/// This should be called in a thread context
pub fn hotplug_cpu_mask(mut cpu_mask: cpu_mask_t) -> Result<(), Status> {
    debug_assert!(!ints_disabled());
    ksync::lock!(let _lock = HotplugLock::lock());

    // Make sure all of the requested CPUs are offline
    if (cpu_mask & get_online_mask()) != 0 {
        return Err(Status::BAD_STATE);
    }

    while cpu_mask != 0 {
        let cpu_id = highest_cpu_set(cpu_mask);
        cpu_mask &= !cpu_num_to_mask(cpu_id);

        // SAFETY: `HotplugLock` serializes all hotplug and unplug operations, and `cpu_id` was
        // verified offline above.
        unsafe { platform_mp_cpu_hotplug(cpu_id) }?;
    }

    Ok(())
}

/// Hotplugs a single CPU by ID, blocking until it is online or an error occurs.
pub fn hotplug_cpu(cpu: cpu_num_t) -> Result<(), Status> {
    hotplug_cpu_mask(cpu_num_to_mask(cpu))
}

// Unplug a single CPU.  Must be called while holding the hotplug lock
fn mp_unplug_cpu_mask_single_locked(
    cpu_id: cpu_num_t,
    deadline: zx_instant_mono_t,
    _token: &mut LockToken<'_, HotplugLock>,
) -> Result<(), Status> {
    assert!((cpu_id as usize) < PerCpu::processor_count());
    // SAFETY: `cpu_id < PerCpu::processor_count()` is verified above.
    let percpu_to_unplug = unsafe { PerCpu::get(cpu_id) };

    // Wait for |percpu_to_unplug| to complete any in-progress DPCs and terminate its DPC thread.
    // Later, once nothing is running on it, we'll migrate its queued DPCs to another CPU.
    percpu_to_unplug.dpc_runner.shutdown(InstantMono(deadline))?;

    crate::arch_rs::platform_mp_prep_cpu_unplug(cpu_id)?;

    // Request to take the target offline.
    // SAFETY: `HotplugLock` is held (proven by `_token`), and `TransitionLock` is not held.
    let status =
        unsafe { percpu_to_unplug.idle_power_thread.transition_active_to_offline(deadline) }.status;

    // The request may fail after the target has already gone offline (e.g. if the
    // platform did not report that it was powered off before |deadline|).  Once
    // the target is offline, it is no longer processing tasks and its TimerQueue
    // and DpcRunner must still be migrated, or they would be stranded.
    //
    // Note that if the transition itself timed out, the request remains pending
    // and the target may still go offline after the check below, in which case
    // its TimerQueue and DpcRunner are not migrated.  This is not expected in
    // practice, since it requires the target's idle/power thread to be unable to
    // run before |deadline|.
    let offline = percpu_to_unplug.idle_power_thread.pending_power_work().current()
        == IdlePowerThreadState::Offline;
    if status.is_err() && !offline {
        return status;
    }

    // Now that the cpu is no longer processing tasks, migrate
    // |percpu_to_unplug|'s TimerQueue and DpcRunner to this cpu. Disable
    // preemption so this thread cannot migrate to another CPU between reading
    // `PerCpu::get_current()` and migrating per-CPU state.
    {
        let _apd = AutoPreemptDisabler::new();
        let current_percpu = PerCpu::get_current();
        // SAFETY: Preemption is disabled so `current_percpu` remains the calling CPU's `PerCpu`,
        // and `percpu_to_unplug` belongs to `cpu_id`, which is already offline (`status.is_ok() ||
        // offline`) and therefore distinct from the calling CPU.
        unsafe { current_percpu.timer_queue.transition_off_cpu(&percpu_to_unplug.timer_queue) };
        current_percpu.dpc_runner.transition_off_cpu(&percpu_to_unplug.dpc_runner);
    }

    status?;
    // SAFETY: `transition_active_to_offline` succeeded (`status` is `Ok(())`), so `cpu_id` is not
    // the calling CPU and is already offline/halted and no longer scheduled on.
    unsafe { crate::arch_rs::platform_mp_cpu_unplug(cpu_id) }
}

/// Unplug the given cpus.  Blocks until the CPUs are removed or `deadline` has been reached.
///
/// Partial failure may occur (in which some CPUs are removed but not others).
///
/// This should be called in a thread context.
pub fn unplug_cpu_mask(
    mut cpu_mask: cpu_mask_t,
    deadline: zx_instant_mono_t,
) -> Result<(), Status> {
    debug_assert!(!ints_disabled());
    ksync::lock!(let mut lock = HotplugLock::lock());

    // Make sure all of the requested CPUs are online
    if (cpu_mask & !get_online_mask()) != 0 {
        return Err(Status::BAD_STATE);
    }

    while cpu_mask != 0 {
        let cpu_id = highest_cpu_set(cpu_mask);
        cpu_mask &= !cpu_num_to_mask(cpu_id);

        mp_unplug_cpu_mask_single_locked(cpu_id, deadline, lock.as_mut().token_mut())?;
    }

    Ok(())
}

/// Unplugs a single CPU by ID, waiting indefinitely for it to go offline.
pub fn unplug_cpu(cpu: cpu_num_t) -> Result<(), Status> {
    unplug_cpu_mask(cpu_num_to_mask(cpu), ZX_TIME_INFINITE)
}

/// Called from arch code during generic task IRQ.
///
/// # Safety
///
/// Must be called with interrupts disabled and preemption/migration disallowed (either from
/// hard-IRQ context or while spinning with interrupts, preemption, and blocking disabled).
pub unsafe fn mbx_generic_irq() {
    debug_assert!(ints_disabled());
    let local_cpu = curr_cpu_num() as usize;
    assert!(local_cpu < SMP_MAX_CPUS);

    stats::inc_generic_ipis();

    let ipi_task_list = &*MP.ipi_task_list;
    let ipi_task_pending = &MP.ipi_task_pending[local_cpu];
    loop {
        let task = {
            ksync::lock!(
                let mut guard =
                    IpiTaskLock::get().lock_policy::<crate::ksync_kernel::NoIrqSavePolicy>()
            );
            // SAFETY: `IpiTaskLock` is the singleton lock guarding `MP.ipi_task_list` and
            // `MP.ipi_task_list` is pinned in static memory.
            let mut list = unsafe {
                let lists = ipi_task_list.get_mut(guard.token_mut());
                Pin::new_unchecked(&mut lists[local_cpu])
            };
            let task = list.as_mut().pop_front();
            if task.is_some() {
                ipi_task_pending.store(!list.is_empty(), Ordering::Relaxed);
            }
            task
        };
        let Some(task) = task else {
            break;
        };

        // SAFETY: `task` was pushed by `sync_exec_raw` and remains valid on the sender's stack
        // until `mp_sync_task` clears this CPU's bit in `outstanding_cpus`. The sender only
        // stops waiting early for a CPU that goes offline, and `set_cpu_online` removes that
        // CPU's queued tasks under `IpiTaskLock` before clearing its bit, so a task popped here
        // has not been drained and the sender is still waiting on it. Read `context` via raw
        // pointer so no `&MpIpiTask` reference outlives `mp_sync_task`.
        unsafe {
            let context = (*task.as_ptr()).context;
            mp_sync_task(context);
        }
    }
}

/// Called from arch code during reschedule IRQ.
///
/// # Safety
///
/// Must be called from hard-IRQ context with interrupts and preemption disabled.
pub unsafe fn mbx_reschedule_irq() {
    debug_assert!(ints_disabled());
    let cpu = curr_cpu_num();

    ltracef!("cpu {}\n", cpu);

    stats::inc_reschedule_ipis();

    if scheduler::peek_is_active(cpu) {
        thread::preempt_set_pending();
    }
}

/// called from arch code during interrupt irq
pub fn mbx_interrupt_irq() {
    ltracef!("cpu {}\n", curr_cpu_num());

    // do nothing, the entire point of this interrupt is to simply have one
    // delivered to the cpu.
}

/// Bring a CPU up and enter it into the scheduler.
///
/// # Safety
///
/// The caller must serialize this with every other hotplug and unplug of `cpu_id`, and `cpu_id`
/// must be fully stopped (not still booting and not partway through halting).
pub unsafe fn platform_mp_cpu_hotplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // SAFETY: Caller upholds the serialization and stopped-CPU preconditions.
    unsafe { crate::arch_rs::mp_cpu_hotplug(cpu_id) }
}

/// We say a CPU is "ready" once it is active and generally ready for tasks.
/// This function signals that the current CPU is ready, allowing for
/// synchronization based on all CPUs reaching this state.
///
/// This function is expected to be called once - and only once - within each
/// CPU's start-up, after it has been set as active.
pub fn signal_curr_cpu_ready() {
    let num = curr_cpu_num();
    debug_assert!(
        scheduler::peek_is_active(num),
        "CPU {} cannot be ready if it is not yet active",
        num
    );
    let mask = cpu_num_to_mask(num);
    let ready = MP.ready_cpu_mask.fetch_or(mask, Ordering::SeqCst) | mask;
    let ready_count = ready.count_ones() as i32;
    let max_count = max_num_cpus() as i32;
    debug_assert!(ready_count <= max_count);
    if ready_count == max_count {
        MP.ready_cpu_event.signal();
    }
}

/// Wait until all of the CPUs in the system are ready (i.e., once all CPUs have
/// called `signal_curr_cpu_ready()`).
///
/// Note: Do not call this until at least `LK_INIT_LEVEL_PLATFORM + 1`, or later.
/// `PLATFORM` is the point at which CPUs check in.  If a call it made to wait
/// before this, there is a chance that we are on the primary CPU and before the
/// point that CPUs have been told to start, or that we are on a secondary CPU
/// during early start-up, and we have not reached our check-in point yet.
/// Calling this function at a point in a situation like that is a guaranteed
/// timeout.
pub fn wait_for_all_cpus_ready(deadline: Deadline) -> Result<(), Status> {
    MP.ready_cpu_event.wait(&deadline)
}

fn mp_all_cpu_startup_sync_hook(_rl: init::LkInitLevel) {
    // Before proceeding any further, wait for a _really_ long time to make sure
    // that all of the CPUs are ready.  We really don't want to start user-mode
    // until we have seen all of our CPUs start up.  In addition, there are
    // decisions to be made while setting up the VDSO which can only be made once
    // we have seen all CPUs start up and check-in.  Specifically, on ARM, we may
    // need to install a version of `zx_get_ticks` which is slower, but may be
    // needed to work around certain errata presented in only some revisions of
    // the CPU silicon (something which can only be determined by the core itself
    // as it comes up).
    const CPU_STARTUP_TIMEOUT: DurationMono = DurationMono::from_seconds(30);
    let status =
        wait_for_all_cpus_ready(Deadline::after_mono(CPU_STARTUP_TIMEOUT, TimerSlack::none()));
    if status.is_ok() {
        return;
    }

    // Something has gone wrong.  One or more of the secondaries has failed to
    // check-in before the timeout.  We can either try to limp along or fail, hard
    // and fast.
    //
    // On development or engineering builds (LK_DEBUGLEVEL > 0), we will emit an
    // oops and continue booting under the assumption that the system is "under
    // development".  By emitting an oops and continuing, we hope to make it
    // easier for the developer to see that there's a problem.  Separately,
    // automated testing infrastructure is designed to look for and flag oops
    // events.
    //
    // On production builds (LK_DEBUGLEVEL == 0) we're going (attempt to) dump
    // some diagnostic data, and then panic.  This can be counter-intuitive.  The
    // thinking here is that it's better to fail hard and fast than to let the
    // system continue on in an unknown or degraded state.  The recovery mechanism
    // is designed to cope with failures that happen early in boot.  Failures that
    // happen later (think after the netstack is up and running) are less likely
    // to trigger the appropriate recovery response.

    // Build masks containing the CPUs that are online+ready, that are merely
    // online, and that should be online+ready so we can report the ones that are
    // missing.  Note, ready implies online.
    let ready_mask = MP.ready_cpu_mask.load(Ordering::Relaxed);
    let online_mask = get_online_mask();
    let mut expected_ready_mask: cpu_mask_t = 0;
    for node in crate::topology::get_system_topology().processors() {
        let processor = node.processor();
        for &id in processor.logical_ids.iter().take(processor.logical_id_count as usize) {
            let logical_id = id as cpu_num_t;
            expected_ready_mask |= cpu_num_to_mask(logical_id);
        }
    }

    // Format a message that we can use in both the oops and panic paths.
    let mut msg = StringBuffer::<200>::new();
    let _ = write!(
        msg,
        "At least one CPU has not declared itself to be started after {} ms \
         (ready {:08x}, online {:08x}, expected {:08x})\n\n",
        CPU_STARTUP_TIMEOUT.into_millis(),
        ready_mask,
        online_mask,
        expected_ready_mask
    );

    // Is this a development build?
    if cfg!(not(lk_debuglevel = "0")) {
        debug::kernel_oops!("{:s}", msg.as_cstr().unwrap_or(c""));
        return;
    }

    // This is a production build.  Try to gather some diagnostic data from the
    // CPUs that failed to check-in.
    //
    // Start the panic process so that anything we print from here on out will go
    // out to serial.  Also, be sure to not attempt to halt any other CPUs since
    // we're about to query them for their state.
    platform_panic_start_with_option(PanicStartHaltOtherCpus::No);
    kprint::kprint!("{:s}", msg.as_cstr().unwrap_or(c""));

    // If this machine has the ability to dump diagnostic state do so for each CPU
    // that failed to check-in.
    if can_dump_registers_and_backtrace() {
        let mut dump_mask = expected_ready_mask & !ready_mask;
        loop {
            let target_cpu = remove_cpu_from_mask(&mut dump_mask);
            if target_cpu == INVALID_CPU {
                break;
            }
            if let Err(status) = dump_registers_and_backtrace_stdout(target_cpu) {
                kprint::kprintln!(
                    "failed to dump state for cpu-{}, status {}",
                    target_cpu,
                    status.into_raw()
                );
            }
        }
    }

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
}

// Before allowing the system to proceed to the USER init level, wait to be sure
// that all of the CPUs have started and made it to the check-in point (see
// above).
init::lk_init_hook!(
    mp_all_cpu_startup_sync,
    mp_all_cpu_startup_sync_hook,
    init::LK_INIT_LEVEL_SMP_WAIT
);

// FFI exports for C++ callers in `mp_ffi.cc`.

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_init() {
    init();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_reschedule(mask: cpu_mask_t, flags: u32) {
    reschedule(mask, flags);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_reschedule_self() {
    reschedule_self();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_interrupt(target: u8, mask: cpu_mask_t) {
    interrupt(MpIpiTarget::from_raw(target), mask);
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`sync_exec_raw`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_sync_exec(
    target: u8,
    mask: cpu_mask_t,
    task: SyncTask,
    context: *mut c_void,
) {
    // SAFETY: The caller upholds the safety requirements of `sync_exec_raw`.
    unsafe { sync_exec_raw(MpIpiTarget::from_raw(target), mask, task, context) };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_hotplug_cpu_mask(mask: cpu_mask_t) -> zx_status_t {
    Status::result_into_raw(hotplug_cpu_mask(mask))
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`unplug_current_cpu`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_unplug_current_cpu() -> ! {
    // SAFETY: Caller upholds the safety requirements of `unplug_current_cpu`.
    unsafe { unplug_current_cpu() }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_unplug_cpu_mask(
    mask: cpu_mask_t,
    deadline: zx_instant_mono_t,
) -> zx_status_t {
    Status::result_into_raw(unplug_cpu_mask(mask, deadline))
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`mbx_reschedule_irq`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_mbx_reschedule_irq() {
    // SAFETY: Caller upholds the safety requirements of `mbx_reschedule_irq`.
    unsafe { mbx_reschedule_irq() };
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`mbx_generic_irq`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_mbx_generic_irq() {
    // SAFETY: Caller upholds the safety requirements of `mbx_generic_irq`.
    unsafe { mbx_generic_irq() };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_mbx_interrupt_irq() {
    mbx_interrupt_irq();
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`set_cpu_online`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_set_cpu_online(cpu: cpu_num_t, online: bool) {
    // SAFETY: The caller upholds the safety requirements of `set_cpu_online`.
    unsafe { set_cpu_online(cpu, online) };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_get_online_mask() -> cpu_mask_t {
    get_online_mask()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_is_cpu_online(cpu: cpu_num_t) -> bool {
    is_cpu_online(cpu)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_mp_signal_curr_cpu_ready() {
    signal_curr_cpu_ready();
}

/// # Safety
///
/// `deadline` must point to a valid [`Deadline`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_mp_wait_for_all_cpus_ready(deadline: *const Deadline) -> zx_status_t {
    // SAFETY: The caller guarantees `deadline` points to a valid `Deadline`.
    Status::result_into_raw(wait_for_all_cpus_ready(unsafe { *deadline }))
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`platform_mp_cpu_hotplug`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_mp_cpu_hotplug(cpu_id: cpu_num_t) -> zx_status_t {
    // SAFETY: Caller upholds the safety preconditions of `platform_mp_cpu_hotplug`.
    Status::result_into_raw(unsafe { platform_mp_cpu_hotplug(cpu_id) })
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`crate::console::CmdArgs`] elements,
/// where `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_mp(
    argc: core::ffi::c_int,
    argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    use crate::arch_rs::InterruptDisableGuard;
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_types::ZX_OK;

    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} unplug <cpu_id>", argv[0].as_str());
        kprintln!("{:s} hotplug <cpu_id>", argv[0].as_str());
        kprintln!(
            "{:s} reschedule <cpu_id>        : send a reschedule ipi to <cpu_id>",
            argv[0].as_str()
        );
        Status::INTERNAL.into_raw()
    };
    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }

    if argv[1].as_str() == "unplug" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }
        let status = Status::result_into_raw(unplug_cpu(argv[2].arg_uint as cpu_num_t));
        kprintln!(
            "CPU {} unplug {:s} {}",
            argv[2].arg_uint,
            if status == ZX_OK { "succeeded" } else { "failed" },
            status
        );
    } else if argv[1].as_str() == "hotplug" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }
        let status = Status::result_into_raw(hotplug_cpu(argv[2].arg_uint as cpu_num_t));
        kprintln!(
            "CPU {} hotplug {:s} {}",
            argv[2].arg_uint,
            if status == ZX_OK { "succeeded" } else { "failed" },
            status
        );
    } else if argv[1].as_str() == "reschedule" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }

        let target_cpu = argv[2].arg_uint as cpu_num_t;
        if !scheduler::peek_is_active(target_cpu) {
            kprintln!("target cpu {} is not active", target_cpu);
            return ZX_OK;
        }

        let mask = cpu_num_to_mask(target_cpu);
        let sending_cpu;
        {
            // Disable interrupts so that the sending CPU we record here cannot change
            // during our call to mp_reschedule.
            let _irqd = InterruptDisableGuard::new();
            sending_cpu = curr_cpu_num();
            reschedule(mask, 0);
        }

        if sending_cpu == target_cpu {
            kprintln!("sending cpu is same as target cpu, no ipi sent");
        } else {
            kprintln!("sent reschedule ipi to cpu {}", target_cpu);
        }
    } else {
        kprintln!("unknown command");
        return usage();
    }

    ZX_OK
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_MP,
    c"mp".as_ptr(),
    c"mp test commands".as_ptr(),
    cmd_mp,
    crate::console::CMD_AVAIL_NORMAL
);

/// Kernel MP unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "mp_rust")]
mod tests {
    use crate::arch_rs::InterruptDisableGuard;
    use crate::kernel::thread::preempt_is_enabled;
    use crate::platform_rs::timer::InstantUnknown;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use unittest::{assert_false, assert_true, subtest};

    /// Verifies querying online CPU mask and per-CPU online status.
    #[test]
    fn test_mp_online_mask() {
        let mask = get_online_mask();
        unittest::expect_ne!(mask, 0);

        for cpu in 0..32 {
            let is_set = (mask & (1 << cpu)) != 0;
            unittest::expect_eq!(is_cpu_online(cpu), is_set);
        }
    }

    /// Verifies synchronous cross-CPU closure execution via `sync_exec` across various targets.
    #[test]
    fn test_mp_sync_exec() {
        let online = get_online_mask();
        let num_online = online.count_ones();

        // 1. Target with explicit bitmask.
        let count_mask = AtomicU32::new(0);
        // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled.
        unsafe {
            sync_exec(MpIpiTarget::Mask, online, || {
                count_mask.fetch_add(1, Ordering::SeqCst);
            });
        }
        unittest::expect_eq!(count_mask.load(Ordering::SeqCst), num_online);

        // 2. Target all online CPUs.
        let count_all = AtomicU32::new(0);
        // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled.
        unsafe {
            sync_exec(MpIpiTarget::All, 0, || {
                count_all.fetch_add(1, Ordering::SeqCst);
            });
        }
        unittest::expect_eq!(count_all.load(Ordering::SeqCst), num_online);
    }

    /// Verifies waiting for all CPUs to be ready during kernel test execution.
    #[test]
    fn test_mp_ready() {
        let status = wait_for_all_cpus_ready(Deadline::no_slack(InstantUnknown(0)));
        unittest::expect_ok!(status);
    }

    // Verify that reschedule_self() ensures a preemption is pending (almost) immediately after
    // re-enabling interrupts.
    /// mp_reschedule_self
    #[test]
    fn mp_reschedule_self() {
        loop {
            let _apd = AutoPreemptDisabler::new();
            let local_mask = cpu_num_to_mask(curr_cpu_num());
            let preempts_pending = || {
                // SAFETY: `thread::current_get()` returns a valid pointer to the running thread.
                unsafe { &*thread::current_get() }.preemption_state().preempts_pending()
            };
            {
                let _irqd = InterruptDisableGuard::new();

                // Now that interrupts are disabled, make sure that we don't already have a
                // preemption pending.  It's possible that another CPU in the system is trying to
                // preempt this CPU.  If that has happened, just try again.
                if (local_mask & preempts_pending()) != 0 {
                    continue;
                }

                reschedule_self();
                // We've just IPI'd our CPU.  However, interrupts are disabled so that IPI won't
                // have resulted in a pending preemption just yet.
                unittest::assert_eq!(0, local_mask & preempts_pending());
            }
            // We've just now re-enabled interrupts.  Either the IPI should have fired or it should
            // fire soon.
            let mut last = crate::platform_rs::timer::current_mono_time();
            while (local_mask & preempts_pending()) == 0 {
                libarch::r#yield();
                let now = crate::platform_rs::timer::current_mono_time();
                if now > last + DurationMono::from_seconds(5) {
                    kprint::kprintln!("still waiting for preemption...");
                    last = now;
                }
            }

            break;
        }
    }

    // Verify that preemption is disabled during mp_sync_exec'd tasks.
    /// mp_sync_exec_preempt_disabled
    #[test]
    fn mp_sync_exec_preempt_disabled() {
        let test_with_target_mask = subtest!(|target: MpIpiTarget, mask: cpu_mask_t| {
            assert_true!(preempt_is_enabled());
            debug_assert!(target != MpIpiTarget::AllButLocal || ints_disabled());

            let preempt_enabled = AtomicBool::new(false);
            // SAFETY: Every `AllButLocal` invocation below disables interrupts first.
            unsafe {
                sync_exec(target, mask, || {
                    if preempt_is_enabled() {
                        preempt_enabled.store(true, Ordering::Relaxed);
                    }
                });
            }

            assert_false!(preempt_enabled.load(Ordering::Relaxed));

            assert_true!(preempt_is_enabled());
        });

        // 1. ALL
        assert_true!(test_with_target_mask(MpIpiTarget::All, /* ignored */ 0));
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::All, /* ignored */ 0));
        }

        // 2. ALL_BUT_LOCAL
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::AllButLocal, /* ignored */ 0));
        }

        // 3. MASK
        assert_true!(test_with_target_mask(MpIpiTarget::Mask, CPU_MASK_ALL));
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::Mask, CPU_MASK_ALL));
        }
    }
}

/// Synchronous IPI unit tests.
#[cfg(ktest)]
#[unittest::suite]
mod sync_ipi_tests {
    use crate::arch_rs::{
        InterruptDisableGuard, blocking_disallowed, curr_cpu_num, ints_disabled, max_num_cpus,
    };
    use crate::kernel::event::Event;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicI32, Ordering};
    use debug::{ltracef, tracef};
    use kprint::kprintln;
    use pin_init::stack_pin_init;

    const LOCAL_TRACE: u32 = 0;

    const TEST_RUNS: u32 = 1000;

    fn inorder_count_task(inorder_counter: &AtomicI32) {
        assert!(ints_disabled());
        assert!(blocking_disallowed());
        let cpu_num = curr_cpu_num();

        let oldval = inorder_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(oldval, cpu_num as i32);
        ltracef!("  CPU {} checked in\n", cpu_num);
    }

    fn counter_task(counter: &AtomicI32) {
        assert!(ints_disabled());
        assert!(blocking_disallowed());
        counter.fetch_add(1, Ordering::SeqCst);
    }

    extern "C" fn deadlock_test_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` that outlives this thread (joined in
        // `deadlock_test` before `gate` goes out of scope).
        let gate = unsafe { &*(arg as *const Event) };
        let _ = gate.wait_infinite();

        let counter = AtomicI32::new(0);
        let _block_interrupts = InterruptDisableGuard::new();
        // SAFETY: Interrupts are disabled by `_block_interrupts`.
        unsafe { sync_exec(MpIpiTarget::AllButLocal, 0, || counter_task(&counter)) };
        0
    }

    fn deadlock_test() {
        // Test for a deadlock caused by multiple CPUs broadcasting concurrently.
        stack_pin_init!(let gate = Event::init_unsignaled());
        let gate = gate.into_ref();
        let gate_ptr = (&raw const *gate).cast_mut().cast::<c_void>();

        let mut threads: [Option<ThreadPtr>; 5] = [None; 5];
        for slot in &mut threads {
            // SAFETY: `deadlock_test_thread` and `gate_ptr` are valid, and all created threads
            // are joined below before `gate` goes out of scope.
            match unsafe {
                thread::create_with_priority(
                    c"sync_ipi_deadlock".as_ptr(),
                    deadlock_test_thread,
                    gate_ptr,
                    DEFAULT_PRIORITY,
                )
            } {
                Ok(t) => {
                    // SAFETY: `t` was just created and has not been resumed or joined yet.
                    unsafe { t.resume() };
                    *slot = Some(t);
                }
                Err(_) => {
                    tracef!("  failed to create thread\n");
                    break;
                }
            }
        }

        gate.signal();

        for thread in threads.into_iter().flatten() {
            // SAFETY: Each thread in `threads` was created above and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }

    /// sync_ipi_tests
    #[test]
    fn sync_ipi_tests() {
        let num_cpus = max_num_cpus();
        let online = get_online_mask();
        let expected_online = 1u32.checked_shl(num_cpus).unwrap_or(0).wrapping_sub(1);
        if online != expected_online {
            kprintln!("Can only run test with all CPUs online");
            return true;
        }

        let runs = TEST_RUNS;

        // Test that we're actually blocking and only signaling the ones we target.
        for _ in 0..runs {
            ltracef!("Sequential test\n");
            let inorder_counter = AtomicI32::new(0);
            for j in 0..num_cpus {
                // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled.
                unsafe {
                    sync_exec(MpIpiTarget::Mask, 1u32 << j, || {
                        inorder_count_task(&inorder_counter)
                    });
                }
                ltracef!("  Finished signaling CPU {}\n", j);
            }
        }

        // Test that we can signal multiple CPUs at the same time.
        for _ in 0..runs {
            ltracef!("Counter test ({} CPUs)\n", num_cpus);
            let counter = AtomicI32::new(0);

            {
                let _irqd = InterruptDisableGuard::new();

                // SAFETY: Interrupts are disabled by `_irqd`.
                unsafe {
                    sync_exec(MpIpiTarget::AllButLocal, 0, || counter_task(&counter));
                }
            }

            ltracef!("  Finished signaling all but local ({})\n", counter.load(Ordering::SeqCst));
            unittest::assert_eq!(counter.load(Ordering::SeqCst) as u32, num_cpus - 1);
        }

        for _ in 0..runs {
            ltracef!("Deadlock test\n");
            deadlock_test();
            ltracef!("Deadlock test passed\n");
        }
    }
}
