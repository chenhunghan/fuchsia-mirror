// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::enable_ints;
pub use crate::backtrace::Backtrace;
use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_void};
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use fbl::{
    DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, RefPtr,
    SinglyLinkedListContainable, SinglyLinkedListNode,
};
use ksync::{KCell, LockToken};
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pinned_drop};
use thread_layout_bindings as layout;
use zr::{Opaque, static_assert};
use zx_status::Status;
use zx_types::{
    ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_THREAD, ZX_TIME_INFINITE_PAST, zx_instant_mono_t,
    zx_koid_t, zx_rseq_t, zx_status_t,
};

use crate::kconcurrent::ChainLock;
use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::dpc::{Dpc, DpcRunner, QueueType};
use crate::kernel::idle_power_thread::{IdlePowerThread, State as IdlePowerState};
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::owned_wait_queue_pool::OwnedWaitQueuePool;
use crate::kernel::percpu::PerCpu;
use crate::kernel::relaxed_atomic::{RelaxedAtomicI64, RelaxedAtomicU32, RelaxedAtomicU64};
use crate::kernel::restricted_state::RestrictedState;
use crate::kernel::scheduler;
use crate::kernel::scheduler_state::{
    IDLE_PRIORITY, SchedulerState, SchedulerStateBaseProfile, ThreadStateKind,
};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::kernel::types::{SMP_MAX_CPUS, cpu_mask_t, cpu_num_t, cpu_num_to_mask};
use crate::kernel::wait_queue::{
    ResourceOwnership, ThreadState, WaitQueue, WaitQueueBase, WaitQueueLockClass,
};
use crate::ksync_kernel::{IrqSavePolicy, NoIrqSavePolicy};
use crate::vm::kstack::KernelStack;
use crate::vm::vm_aspace::VmAspace;

pub use crate::kconcurrent::CallsiteInfo as CltTag;
use crate::kconcurrent::{
    Action, ChainLockProof, ChainLockTransaction, ChainLockTransactionToken, ChainLocked,
    ClassedChainLock, DONE, IRQ_SAVE_OPTION, IrqSave, NO_IRQ_SAVE_OPTION, OwnerGuard,
    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, SingleChainLockGuard, SingleChainLockScope,
    TransactionResult, acquire_both_or_backoff, clt_tag, single_chain_lock_guard,
    single_chain_lock_guard_owned,
};

const LOCAL_TRACE: u32 = 0;

/// Lock class for data guarded by a [`Thread`]'s chain lock.
///
/// A marker only: tokens of this class exist solely inside a [`ChainLockProof`] for a [`Thread`]
/// owner, never via `KMutex`.
pub enum ThreadLockClass {}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_thread_current_get() -> *mut Thread;
}

/// Stage at which a migration function may be executed (`Thread::MigrateStage`).
// LINT.IfChange(ThreadMigrateStage)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateStage {
    /// Before the thread has migrated between CPUs. Called from the old CPU.
    Save = 0,
    /// After the thread has migrated between CPUs. Called from the new CPU.
    Restore = 1,
    /// The thread is exiting. May be called from any CPU.
    Exiting = 2,
}
// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:ThreadMigrateStage)

zr::static_assert_size_and_align!(MigrateStage, 1, 1);

/// Inline capture capacity (in bytes) for [`MigrateFn`] and [`ContextSwitchFn`].
///
/// Two pointer-sized words suffice for both Rust closures and the C++ `(func, ctx)` FFI shim,
/// keeping `Option<MigrateFn>` and `Option<ContextSwitchFn>` to 3 words (24 bytes on 64-bit)
/// via `zr::SendInlineFnMut`'s static-vtable niche optimization.
pub const THREAD_CALLBACK_INLINE_SIZE: usize = 2 * size_of::<usize>();

/// Owned inline migration callback stored in [`Thread`] (`Thread::SetMigrateFn`).
pub type MigrateFn = zr::SendInlineFnMut<
    'static,
    zr::Hrtb<fn(zr::RefArg<Thread>, zr::ValArg<MigrateStage>)>,
    THREAD_CALLBACK_INLINE_SIZE,
>;

/// Owned inline context-switch callback stored in [`Thread`] (`Thread::SetContextSwitchFn`).
pub type ContextSwitchFn = zr::SendInlineFnMut<'static, fn(), THREAD_CALLBACK_INLINE_SIZE>;

/// Raw C callback signature passed across FFI from `Thread::SetMigrateFn[Locked]`.
pub type RawMigrateFn =
    unsafe extern "C" fn(ctx: *mut c_void, thread: *mut Thread, stage: MigrateStage);
/// Raw C callback signature passed across FFI from `Thread::SetContextSwitchFn[Locked]`.
pub type RawContextSwitchFn = unsafe extern "C" fn(ctx: *mut c_void);

/// FFI-safe inline storage for a Rust-owned [`Option<T>`] callback inside `#[repr(C)] struct
/// Thread`.
///
/// Wrapping `Option<T>` in `MaybeUninit<layout::ThreadCallbackStorage>` keeps `#[repr(C)]
/// struct Thread` FFI-safe across `extern "C"` boundaries without `#[allow(improper_ctypes)]`,
/// while still owning and dropping the `Option<T>` value in Rust.
///
/// # Safety Invariant
///
/// Every live `CallbackSlot<T>` holds a valid, initialized `Option<T>` in `storage`.
#[repr(C)]
struct CallbackSlot<T> {
    storage: MaybeUninit<layout::ThreadCallbackStorage>,
    _marker: PhantomData<Option<T>>,
}

impl<T> CallbackSlot<T> {
    #[inline]
    const fn assert_layout() {
        const {
            assert!(size_of::<Option<T>>() == size_of::<layout::ThreadCallbackStorage>());
            assert!(align_of::<Option<T>>() == align_of::<layout::ThreadCallbackStorage>());
        }
    }

    /// Creates a new `CallbackSlot<T>` initialized to `None`.
    #[inline]
    fn none() -> Self {
        Self::assert_layout();
        let mut storage = MaybeUninit::<layout::ThreadCallbackStorage>::uninit();
        // SAFETY: `Self` and `ThreadCallbackStorage` have the exact size and alignment of
        // `Option<T>` (`assert_layout`), and `storage` is local writable storage.
        unsafe { storage.as_mut_ptr().cast::<Option<T>>().write(None) };
        Self { storage, _marker: PhantomData }
    }

    #[inline]
    fn as_option(&self) -> &Option<T> {
        Self::assert_layout();
        // SAFETY: By the type invariant, `self.storage` holds an initialized `Option<T>` with
        // matching size and alignment.
        unsafe { &*self.storage.as_ptr().cast::<Option<T>>() }
    }

    #[inline]
    fn as_option_mut(&mut self) -> &mut Option<T> {
        Self::assert_layout();
        // SAFETY: By the type invariant, `self.storage` holds an initialized `Option<T>` with
        // matching size and alignment, and `&mut self` grants exclusive access.
        unsafe { &mut *self.storage.as_mut_ptr().cast::<Option<T>>() }
    }
}

impl<T> Drop for CallbackSlot<T> {
    fn drop(&mut self) {
        Self::assert_layout();
        // SAFETY: By the type invariant, `self.storage` holds an initialized `Option<T>`, and
        // `drop` runs at most once.
        unsafe { ptr::drop_in_place(self.storage.as_mut_ptr().cast::<Option<T>>()) };
    }
}

/// Wrapper around a raw `*mut c_void` context pointer passed from C++ callback shims.
struct CppCallbackCtx(*mut c_void);

impl CppCallbackCtx {
    #[inline]
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

// SAFETY: C++ callers of `rust_thread_set_{migrate,context_switch}_fn[_locked]` guarantee that
// the packed context value is safe to send across CPUs and invoke under the owning `Thread`'s lock.
unsafe impl Send for CppCallbackCtx {}

/// Wraps a C++ `(func, ctx)` migration callback pair into an owned [`MigrateFn`].
///
/// # Safety
///
/// If `func` is `Some(f)`, `f(ctx, thread, stage)` must be sound to invoke whenever the scheduler
/// calls the migration callback with `thread`'s lock held and interrupts disabled, and `ctx` must
/// remain valid until the callback is cleared or the thread is destroyed.
#[inline]
unsafe fn wrap_cpp_migrate_fn(func: Option<RawMigrateFn>, ctx: *mut c_void) -> Option<MigrateFn> {
    func.map(|f| {
        let ctx = CppCallbackCtx(ctx);
        MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
            // SAFETY: Upheld by the C++ caller of `rust_thread_set_migrate_fn[_locked]`.
            unsafe { f(ctx.as_ptr(), ptr::from_ref(thread).cast_mut(), stage) };
        })
    })
}

/// Wraps a C++ `(func, ctx)` context-switch callback pair into an owned [`ContextSwitchFn`].
///
/// # Safety
///
/// If `func` is `Some(f)`, `f(ctx)` must be sound to invoke whenever the scheduler calls the
/// context-switch callback with the thread's lock held and interrupts disabled, and `ctx` must
/// remain valid until the callback is cleared or the thread is destroyed.
#[inline]
unsafe fn wrap_cpp_context_switch_fn(
    func: Option<RawContextSwitchFn>,
    ctx: *mut c_void,
) -> Option<ContextSwitchFn> {
    func.map(|f| {
        let ctx = CppCallbackCtx(ctx);
        ContextSwitchFn::new(move || {
            // SAFETY: Upheld by the C++ caller of `rust_thread_set_context_switch_fn[_locked]`.
            unsafe { f(ctx.as_ptr()) };
        })
    })
}

/// Stall accounting state of a thread.
///
/// Kernel-only threads are never considered for the purpose of memory stall accounting
/// (`IgnoredKernelOnly`).
///
/// If a user thread is not signalling a stall, it will contribute to the current CPU's memory
/// stall accumulator as `Progressing` while running and do not contribute (`Inactive`) in any
/// other state.
///
/// If a user thread signals that a stall is ongoing, it will contribute to the current CPU's
/// accumulator as `Stalling`. If preempted, the contribution will persist on the last CPU's
/// accumulator. Normal operation is resumed when the thread signals that the stall is over.
///
/// Safety invariant: a thread only enters `Stalling` while it is the running thread on its CPU
/// ([`ScopedMemoryStall`] on the current thread), so a `Stalling` thread's `last_cpu` is a
/// valid CPU number (`< PerCpu::processor_count()`); `last_cpu` is only written at context
/// switch and idle init and never reset.
// LINT.IfChange(ThreadStallState)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStallState {
    IgnoredKernelOnly = 0,
    Inactive = 1,
    Progressing = 2,
    Stalling = 3,
}
// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:ThreadStallState)

// `enum class ThreadStallState` has no fixed underlying type, so it is `int`.
zr::static_assert!(core::mem::size_of::<ThreadStallState>() == 4);
zr::static_assert!(core::mem::align_of::<ThreadStallState>() == 4);

// A thread's `memory_stall_state` is guarded by `preempt_disabled_token`. The accessors below do
// not take a `PreemptDisabledToken` because the scheduler, their only caller, does not formally
// disable preemption when it switches threads, but in fact it is disabled, because the scheduler
// does not preempt itself.

/// Returns `thread`'s stall accounting state.
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for the duration of the
/// call, with no concurrent writes to its `memory_stall_state`. The state is guarded by
/// `preempt_disabled_token`, so the caller must have preemption (or interrupts, as during a
/// scheduler context switch) disabled on the current CPU.
#[inline]
pub unsafe fn memory_stall_state(thread: *const Thread) -> ThreadStallState {
    // SAFETY: `thread` points to a valid `Thread` per the caller, projecting the
    // `memory_stall_state` field place without forming a reference to the whole `Thread`, and
    // preemption or interrupts are disabled with no concurrent writes.
    unsafe { *(*thread).memory_stall_state.get() }
}

/// Sets `thread`'s stall accounting state.
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for the duration of the
/// call, with exclusive access to its `memory_stall_state_`. The state is guarded by
/// `preempt_disabled_token`, so the caller must have preemption (or interrupts, as during a
/// scheduler context switch) disabled on the current CPU.
///
/// If `value == ThreadStallState::Stalling`, `thread`'s `SchedulerState::last_cpu()` must be
/// `< PerCpu::processor_count()`, to uphold the safety invariant on [`ThreadStallState`].
#[inline]
pub unsafe fn set_memory_stall_state(thread: *mut Thread, value: ThreadStallState) {
    // SAFETY: `thread` points to a valid `Thread` per the caller, projecting the
    // `memory_stall_state` field place without forming a reference to the whole `Thread`, and
    // preemption or interrupts are disabled with exclusive access to `memory_stall_state`.
    unsafe { *(*thread).memory_stall_state.get() = value }
}

/// Returns a reference to `thread`'s [`SchedulerState`].
///
/// # Safety
///
/// `thread` must point to a valid, initialized `Thread` that remains alive for `'a`, and the
/// caller must hold `thread`'s lock for `'a` with no concurrent mutable access to its
/// `scheduler_state_`.
#[inline]
pub unsafe fn scheduler_state<'a>(thread: *const Thread) -> &'a SchedulerState {
    // SAFETY: The caller guarantees `thread` is a valid, live `Thread` for `'a` and holds its lock
    // for `'a`, which excludes concurrent mutation of `scheduler_state`.
    unsafe { (*thread).scheduler_state_unchecked() }
}

// LINT.IfChange(thread_flags_and_signals)
/// Thread is detached and will not be joined.
pub const THREAD_FLAG_DETACHED: u32 = 1 << 0;
/// `Thread` struct is heap-allocated and should be freed on exit.
pub const THREAD_FLAG_FREE_STRUCT: u32 = 1 << 1;
/// Thread is a per-CPU idle/power thread.
pub const THREAD_FLAG_IDLE: u32 = 1 << 2;
/// Thread runs a guest VCPU.
pub const THREAD_FLAG_VCPU: u32 = 1 << 3;
/// A restricted-mode kick is pending on this thread.
pub const THREAD_FLAG_RESTRICTED_KICK_PENDING: u32 = 1 << 4;

/// Thread has been signaled to terminate.
pub const THREAD_SIGNAL_KILL: u32 = 1 << 0;
/// Thread has been signaled to suspend.
pub const THREAD_SIGNAL_SUSPEND: u32 = 1 << 1;
/// Thread has a pending job policy exception to deliver on syscall exit.
pub const THREAD_SIGNAL_POLICY_EXCEPTION: u32 = 1 << 2;
/// Thread has been kicked out of restricted mode.
pub const THREAD_SIGNAL_RESTRICTED_KICK: u32 = 1 << 3;
/// Thread has been signaled to sample its user stack.
pub const THREAD_SIGNAL_SAMPLE_STACK: u32 = 1 << 4;
/// Thread has been signaled to check its restartable sequence before returning to usermode.
pub const THREAD_SIGNAL_CHECK_RSEQ: u32 = 1 << 5;
/// Thread is being killed due to a fatal job policy violation.
pub const THREAD_SIGNAL_POLICY_KILL: u32 = 1 << 6;
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:thread_flags_and_signals,
//   //zircon/kernel/kernel/thread_ffi.cc:thread_flags_and_signals,
// )

/// `ZX_ERR_INTERNAL_INTR_KILLED`; `zx_status::Status` has no named constant for it.
pub const ZX_ERR_INTERNAL_INTR_KILLED: Status =
    Status::err_from_raw(zx_types::ZX_ERR_INTERNAL_INTR_KILLED);
/// `ZX_ERR_INTERNAL_INTR_RETRY`, an alias of `ZX_ERR_INTERRUPTED_RETRY`.
pub const ZX_ERR_INTERNAL_INTR_RETRY: Status = Status::INTERRUPTED_RETRY;

/// Process and thread koids for a tracing thread reference.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FxtRef {
    pub pid: u64,
    pub tid: u64,
}

/// Returns the slice of `buf` preceding the first NUL byte.
#[inline]
pub(crate) fn name_buf_as_bytes(buf: &[u8; ZX_MAX_NAME_LEN]) -> &[u8] {
    let mut len = 0;
    while len < ZX_MAX_NAME_LEN && buf[len] != 0 {
        len += 1;
    }
    &buf[..len]
}

/// Snapshot of a thread's name buffer.
pub struct ThreadName {
    buf: [u8; ZX_MAX_NAME_LEN],
    len: usize,
}

impl ThreadName {
    /// Returns an empty [`ThreadName`].
    #[inline]
    pub const fn empty() -> Self {
        Self { buf: [0u8; ZX_MAX_NAME_LEN], len: 0 }
    }

    /// Returns the name bytes before the first NUL byte.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Returns the name as a UTF-8 string slice, or `""` if it contains invalid UTF-8.
    #[inline]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl core::fmt::Display for ThreadName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&zr::from_utf8_lossy(self.as_bytes()), f)
    }
}

/// Tag type for a [`Thread`]'s membership in the global migrate list (`migrate_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrateListTag;

/// Tag type for a [`Thread`]'s membership in the global thread list (`thread_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ThreadListTag;

/// Tag type for a [`Thread`]'s membership in a temporary batch unblock list (`unblock_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnblockListTag;

/// Tag type for a [`Thread`]'s membership in a save state list (`save_state_list_node`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SaveStateListTag;

/// The global list of threads with migrate functions.
pub type MigrateList = DoublyLinkedList<NonNull<Thread>, MigrateListTag>;
/// The global thread list.
pub type ThreadList = DoublyLinkedList<NonNull<Thread>, ThreadListTag>;
/// A temporary batch unblock list.
pub type UnblockList = DoublyLinkedList<NonNull<Thread>, UnblockListTag>;
/// A save state list.
pub type SaveStateList = fbl::SinglyLinkedList<NonNull<Thread>, SaveStateListTag>;
/// The list of `OwnedWaitQueue`s a [`Thread`] owns.
pub type OwnedWaitQueueList = DoublyLinkedList<NonNull<OwnedWaitQueue>>;

// `OwnedWaitQueueList`, `ThreadList`, and `MigrateList` are `fbl::DoublyLinkedList`s whose only
// non-ZST field is their `head` pointer (relied upon by `for_each_thread_raw` and
// `call_migrate_fn_for_cpu` for raw-pointer traversal, and matched by `Thread::owned_wait_queues_`
// `OpaqueStorage` in `thread.h`).
static_assert!(size_of::<OwnedWaitQueueList>() == size_of::<*mut c_void>());
static_assert!(align_of::<OwnedWaitQueueList>() == align_of::<*mut c_void>());
static_assert!(size_of::<ThreadList>() == size_of::<*mut c_void>());
static_assert!(align_of::<ThreadList>() == align_of::<*mut c_void>());
static_assert!(size_of::<MigrateList>() == size_of::<*mut c_void>());
static_assert!(align_of::<MigrateList>() == align_of::<*mut c_void>());

// kernel counters.
// The counters below never decrease.
//
// counts the number of Threads successfully created.
crate::counters::define_kcounter!(THREAD_CREATE_COUNT, "thread.create", Sum);
// counts the number of detached Threads that exited. Never decreases.
crate::counters::define_kcounter!(THREAD_DETACHED_EXIT_COUNT, "thread.detached_exit", Sum);
// counts the number of Threads joined. Never decreases.
crate::counters::define_kcounter!(THREAD_JOIN_COUNT, "thread.join", Sum);
// counts the number of calls to resume() that succeeded.
crate::counters::define_kcounter!(THREAD_RESUME_COUNT, "thread.resume", Sum);
// counts the number of calls to suspend() that succeeded.
crate::counters::define_kcounter!(THREAD_SUSPEND_COUNT, "thread.suspend", Sum);
// counts the number of calls to restricted_kick() that succeeded.
crate::counters::define_kcounter!(THREAD_RESTRICTED_KICK_COUNT, "thread.restricted_kick", Sum);
// counts the number of failed samples
crate::counters::define_kcounter!(THREAD_SAMPLING_FAILED, "thread.sampling_failed", Sum);
crate::counters::define_kcounter!(RSEQ_BAD_IP, "rseq.bad_ip", Sum);

/// Per-thread preemption state.
#[repr(C)]
pub struct PreemptionState {
    /// `state` contains three fields:
    ///
    ///  * a 15-bit preempt disable counter (bits 0-14)
    ///  * a 15-bit eager resched disable counter (bits 15-29)
    ///  * a 2-bit for TimesliceExtensionFlags (bits 30-31)
    ///
    /// This is a single field so that both counters and the flags can be compared
    /// against zero with a single memory access and comparison.
    ///
    /// `state`'s counts are modified by interrupt handlers, but the counts are
    /// always restored to their original value before the interrupt handler
    /// returns, so modifications are not visible to the interrupted thread.
    state: RelaxedAtomicU32,

    /// `preempts_pending` tracks pending reschedules to both local and remote CPUs
    /// due to activity in the context of the current thread.
    ///
    /// This value can be changed asynchronously by an interrupt handler.
    ///
    /// `preempts_pending` should only be non-zero:
    ///  * if `preempt_disable_count()` or `eager_resched_disable_count()` are non-zero, or
    ///  * after `preempt_disable_count()` or `eager_resched_disable_count()` have been
    ///    decremented, while `preempts_pending` is being checked.
    preempts_pending: RelaxedAtomicU32,

    /// The maximum duration of the thread's timeslice extension.
    ///
    /// This field is only valid when `state`'s
    /// `TIMESLICE_EXTENSION_PRESENT` flag is set.
    ///
    /// This field may only be accessed by its owning thread or in an interrupt
    /// context of the owning thread.  When reading this field, be sure to issue a
    /// `compiler_fence` (Rust's `atomic_signal_fence`; a compiler barrier) with acquire semantics
    /// after observing the Present flag.  Likewise, when writing this field, use a
    /// `compiler_fence` with release semantics prior to setting the Present
    /// flag.  By using these fences, we ensure the flag and field value remain in
    /// sync.
    timeslice_extension: RelaxedAtomicI64,

    /// The deadline at which the thread timeslice extension expires.
    ///
    /// This field is only valid when `TIMESLICE_EXTENSION_ACTIVE` flag is
    /// set.
    ///
    /// This field may only be accessed by its owning thread or in an interrupt
    /// context of the owning thread.  When reading this field, be sure to issue a
    /// `compiler_fence` (Rust's `atomic_signal_fence`; a compiler barrier) with acquire semantics
    /// after observing the Active flag.  Likewise, when writing this field, use a
    /// `compiler_fence` with release semantics prior to setting the Active
    /// flag.  By using these fences, we ensure the flag and field value remain in
    /// sync.
    timeslice_extension_deadline: RelaxedAtomicI64,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Flush: u32 {
        const LOCAL = 0x1;
        const REMOTE = 0x2;
        const ALL = Self::LOCAL.bits() | Self::REMOTE.bits();
    }
}

// counts the number of times a thread's timeslice extension was activated (see
// `PreemptionState::set_timeslice_extension`).
crate::counters::define_kcounter!(THREAD_TIMESLICE_EXTENDED, "thread.timeslice_extended", Sum);

impl PreemptionState {
    /// Creates a new zero-initialized [`PreemptionState`].
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: RelaxedAtomicU32::new(0),
            preempts_pending: RelaxedAtomicU32::new(0),
            timeslice_extension: RelaxedAtomicI64::new(0),
            timeslice_extension_deadline: RelaxedAtomicI64::new(0),
        }
    }

    /// Counters contained in state_ are limited to 15 bits.
    const MAX_COUNT_VALUE: u32 = 0x7fff;
    /// The preempt disable count is in the lowest 15 bits.
    const PREEMPT_DISABLE_MASK: u32 = Self::MAX_COUNT_VALUE;
    /// The eager resched disable count is in the next highest 15 bits.
    const EAGER_RESCHED_DISABLE_SHIFT: u32 = 15;
    /// Mask selecting the eager resched disable count.
    const EAGER_RESCHED_DISABLE_MASK: u32 =
        Self::MAX_COUNT_VALUE << Self::EAGER_RESCHED_DISABLE_SHIFT;
    /// Finally, the timeslice extension flags are the highest 2 bits.
    const TIMESLICE_EXTENSION_FLAGS_SHIFT: u32 = 30;
    /// Mask selecting the timeslice extension flags.
    const TIMESLICE_EXTENSION_FLAGS_MASK: u32 =
        !(Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK);
    /// Thread has a timeslice extension that may or may not be Active.
    const TIMESLICE_EXTENSION_PRESENT: u32 = 0b01 << Self::TIMESLICE_EXTENSION_FLAGS_SHIFT;
    /// Thread has an Active (in use) timeslice extension.
    const TIMESLICE_EXTENSION_ACTIVE: u32 = 0b10 << Self::TIMESLICE_EXTENSION_FLAGS_SHIFT;

    /// Returns true if preemption is enabled for this thread: both counts are zero and there is
    /// no runtime extension.
    #[inline]
    pub fn preempt_is_enabled(&self) -> bool {
        let state = self.state.load();
        state == 0
    }

    /// Returns the thread's preempt disable count.
    #[inline]
    pub fn preempt_disable_count(&self) -> u32 {
        Self::preempt_disable_count_of(self.state.load())
    }

    /// Returns the thread's eager resched disable count.
    #[inline]
    pub fn eager_resched_disable_count(&self) -> u32 {
        Self::eager_resched_disable_count_of(self.state.load())
    }

    /// Returns the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending(&self) -> cpu_mask_t {
        self.preempts_pending.load()
    }

    /// Clears the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending_clear(&self) {
        self.preempts_pending.store(0);
    }

    /// Adds `mask` to the thread's pending preemptions mask.
    #[inline]
    pub fn preempts_pending_add(&self, mask: cpu_mask_t) {
        self.preempts_pending.fetch_or(mask);
    }

    // `<kernel/thread.h>` duplicates these inline fast paths for C++ callers; keep them in sync.
    // LINT.IfChange(PreemptionStateFastPaths)
    /// PreemptDisable() increments the preempt disable counter for the current
    /// thread. While preempt disable is non-zero, preemption of the thread is
    /// disabled, including preemption from interrupt handlers. During this time,
    /// any call to Reschedule() will only record that a reschedule is pending, and
    /// won't do a context switch.
    ///
    /// Note that this does not disallow blocking operations (e.g.
    /// mutex.Acquire()). Disabling preemption does not prevent switching away from
    /// the current thread if it blocks.
    ///
    /// A call to `preempt_disable()` must be matched by a later call to
    /// `preempt_reenable()` to decrement the preempt disable counter.
    #[inline]
    pub fn preempt_disable(&self) {
        let old_state = self.state.fetch_add(1);
        assert!(
            Self::preempt_disable_count_of(old_state) < Self::MAX_COUNT_VALUE,
            "preempt disable count overflow: old_state: {old_state:#x}"
        );
    }

    /// PreemptReenable() decrements the preempt disable counter and flushes any
    /// pending local preemption operation.  Callers must ensure that they are
    /// calling from a context where blocking is allowed, as the call may result in
    /// the immediate preemption of the calling thread.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the flush path, as in C++).
    ///
    /// # Safety
    ///
    /// - `self` must be the *current* thread's preemption state: the flush may reschedule the
    ///   current thread, and `&self` survives that only because the current thread outlives its
    ///   own run.
    /// - The call must pair with a prior, still unmatched [`Self::preempt_disable`] on the
    ///   current thread, and it must not drop preemption back to enabled while any
    ///   [`PreemptDisabledToken`] is live.
    #[inline]
    pub unsafe fn preempt_reenable(&self) {
        let old_state = self.state.fetch_sub(1);
        // Fast path: count drops to zero, no eager resched count, no extension, nothing pending.
        if old_state == 1 {
            // Order the state RMW before the preempts_pending load for same-CPU interrupts.
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            if self.preempts_pending.load() != 0 {
                // SAFETY: The `self`, pairing, and token obligations are forwarded from the
                // caller, and `old_state == 1` satisfies the slow-path counter preconditions.
                unsafe { self.preempt_reenable_slow(old_state) };
            }
            return;
        }
        // Are either of the counters non-zero?  Only take the early return when the preempt
        // disable count was at least one: the underflow case (count already zero) must fall
        // through to the slow path, which asserts on it.
        if Self::preempt_disable_count_of(old_state) != 0
            && (old_state & (Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK)) > 1
        {
            // We've got a non-zero count in one of the counters.
            return;
        }
        // SAFETY: The `self`, pairing, and token obligations are forwarded from the caller, and
        // `old_state` was produced by the `fetch_sub(1)` above with the non-zero counter cases
        // already returned early.
        unsafe { self.preempt_reenable_slow(old_state) }
    }

    /// The uncommon remainder of [`Self::preempt_reenable`], after the inline fast-path checks;
    /// `old_state` is the `state` value before the decrement. Kept out of line so that the hot
    /// entry point stays small enough to inline.
    ///
    /// # Safety
    ///
    /// As [`Self::preempt_reenable`], and the caller has already decremented the preempt disable
    /// count once, observing `old_state` with
    /// `Self::eager_resched_disable_count_of(old_state) == 0 &&
    /// Self::preempt_disable_count_of(old_state) == 1` (or an underflow with
    /// `Self::preempt_disable_count_of(old_state) == 0`, which panics).
    #[cold]
    #[inline(never)]
    unsafe fn preempt_reenable_slow(&self, old_state: u32) {
        assert!(
            Self::preempt_disable_count_of(old_state) != 0,
            "preempt disable count underflow: unmasked preemption state: {old_state:#x}"
        );

        // First, check for the expected situation of dropping the preempt count to zero
        // with a zero eager resched disable count and no timeslice extension.  The inline
        // wrapper only gets here in that case when a preemption is pending, so flush directly.
        if old_state == 1 {
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending_continued(Flush::LOCAL) };
            return;
        }

        // Things must be more complicated.  The inline wrapper already returned when either
        // counter was still non-zero, so the counters are both zero.  At this point, we must
        // have a timeslice extension installed.  This extension may be inactive, active and
        // not-yet-expired, or active and expired.

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // It has.  We can flush.
                debug_assert!(self.preempt_is_enabled());
                // SAFETY: Forwarded from the caller.
                unsafe { self.flush_pending(Flush::LOCAL) };
            }
        }

        // We have an extension that's either inactive or active+unexpired.
    }

    /// PreemptReenableDelayFlush() decrements the preempt disable counter, but
    /// deliberately does _not_ flush any pending local preemption operation.
    /// Instead, if local preemption has become enabled again after the count
    /// drops, and the local pending bit is set, the method will clear the bit and
    /// return true.  Otherwise, it will return false.
    ///
    /// This method may only be called when interrupts are disabled and blocking is
    /// not allowed.
    ///
    /// Callers of this method are "taking" ownership of the responsibility to
    /// ensure that preemption on the local CPU takes place in the near future
    /// after the call if the method returns true.
    ///
    /// Use of this method is strongly discouraged outside of top-level interrupt
    /// glue and early threading setup.
    ///
    /// TODO(johngro): Consider replacing the bool return type with a move-only
    /// RAII type which wraps the bool, and ensures that preemption event _must_
    /// happen, either by having the user call a method on the object to manually
    /// force the preemption event, or when the object destructs.
    ///
    /// # Safety
    ///
    /// - `self` must be the *current* thread's preemption state, with interrupts disabled and
    ///   blocking disallowed.
    /// - The call must pair with a prior, still unmatched [`Self::preempt_disable`] on the current
    ///   thread, and it must not drop preemption back to enabled while any [`PreemptDisabledToken`]
    ///   is live.
    #[must_use]
    #[inline]
    pub unsafe fn preempt_reenable_delay_flush(&self) -> bool {
        debug_assert!(crate::arch_rs::ints_disabled());
        debug_assert!(crate::arch_rs::blocking_disallowed());

        let old_state = self.state.fetch_sub(1);

        // First, check for the expected situation of dropping the preempt count to zero
        // with a zero eager resched disable count and no timeslice extension.
        if old_state == 1 {
            let local_mask = crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
            let prev_mask = self.preempts_pending.fetch_and(!local_mask);
            return (local_mask & prev_mask) != 0;
        }
        // Are either of the counters non-zero?  Only take the early return when the preempt
        // disable count was at least one: the underflow case (count already zero) must fall
        // through to the slow path, which asserts on it.
        if Self::preempt_disable_count_of(old_state) != 0
            && (old_state & (Self::PREEMPT_DISABLE_MASK | Self::EAGER_RESCHED_DISABLE_MASK)) > 1
        {
            // We've got a non-zero count in one of the counters.
            return false;
        }
        self.preempt_reenable_delay_flush_slow(old_state)
    }

    /// The uncommon remainder of [`Self::preempt_reenable_delay_flush`], after the inline
    /// fast-path checks; `old_state` is the `state` value before the decrement. Kept out of line
    /// so that the hot entry point stays small enough to inline. Only reached when
    /// `Self::eager_resched_disable_count_of(old_state) == 0 &&
    /// Self::preempt_disable_count_of(old_state) == 1` and `old_state != 1` (or on underflow with
    /// `Self::preempt_disable_count_of(old_state) == 0`, which panics).
    #[must_use]
    #[cold]
    #[inline(never)]
    fn preempt_reenable_delay_flush_slow(&self, old_state: u32) -> bool {
        assert!(
            Self::preempt_disable_count_of(old_state) != 0,
            "preempt disable count underflow: unmasked preemption state: {old_state:#x}"
        );
        debug_assert!(Self::has_timeslice_extension(old_state));

        // The inline wrapper already returned when either counter was still non-zero, so the
        // counters are both zero.  At this point, we must have a timeslice
        // extension installed.  This extension may be inactive, active and
        // not-yet-expired, or active and expired.

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // It has.
                debug_assert!(self.preempt_is_enabled());
                let local_mask =
                    crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
                let prev_mask = self.preempts_pending.fetch_and(!local_mask);
                return (local_mask & prev_mask) != 0;
            }
        }

        // We have an extension that's either inactive or active+unexpired.
        false
    }

    /// EagerReschedDisable() increments the eager resched disable counter for the
    /// current thread. When early resched disable is non-zero, issuing local and
    /// remote preemptions is disabled, including from interrupt handlers. During
    /// this time, any call to Reschedule() or other scheduler entry points that
    /// imply a reschedule will only record the pending reschedule for the affected
    /// CPU, but will not perform reschedule IPIs or a local context switch.
    ///
    /// As with PreemptDisable, blocking operations are still allowed while
    /// eager resched disable is non-zero.
    ///
    /// A call to `eager_resched_disable()` must be matched by a later call to
    /// `eager_resched_reenable()` to decrement the eager resched disable counter.
    #[inline]
    pub fn eager_resched_disable(&self) {
        let old_state = self.state.fetch_add(1 << Self::EAGER_RESCHED_DISABLE_SHIFT);
        assert!(
            Self::eager_resched_disable_count_of(old_state) < Self::MAX_COUNT_VALUE,
            "eager resched disable count overflow: old_state: {old_state:#x}"
        );
    }

    /// EagerReschedReenable() decrements the eager resched disable counter and
    /// flushes pending local and/or remote preemptions if enabled, respectively.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the local flush path, as in C++).
    ///
    /// # Safety
    ///
    /// - `self` must be the *current* thread's preemption state (see [`Self::preempt_reenable`]).
    /// - The call must pair with a prior, still unmatched [`Self::eager_resched_disable`] on the
    ///   current thread, and it must not drop preemption back to enabled while any
    ///   [`PreemptDisabledToken`] is live.
    #[inline]
    pub unsafe fn eager_resched_reenable(&self) {
        let old_state = self.state.fetch_sub(1 << Self::EAGER_RESCHED_DISABLE_SHIFT);
        // Fast path: count drops to zero, no preempt disable count, no extension, nothing pending.
        if old_state == 1 << Self::EAGER_RESCHED_DISABLE_SHIFT {
            // Order the state RMW before the preempts_pending load for same-CPU interrupts.
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            if self.preempts_pending.load() != 0 {
                // SAFETY: The `self`, pairing, and token obligations are forwarded from the
                // caller, and `old_state` has eager resched disable count 1.
                unsafe { self.eager_resched_reenable_slow(old_state) };
            }
            return;
        }
        if Self::eager_resched_disable_count_of(old_state) > 1 {
            // Nothing to do since eager resched disable implies preempt disable.
            return;
        }
        // SAFETY: The `self`, pairing, and token obligations are forwarded from the caller, and
        // `old_state` was produced by the `fetch_sub` above with the
        // `eager_resched_disable_count_of(old_state) > 1` case already returned early.
        unsafe { self.eager_resched_reenable_slow(old_state) }
    }

    /// The uncommon remainder of [`Self::eager_resched_reenable`], after the inline fast-path
    /// checks; `old_state` is the `state` value before the decrement. Kept out of line so that
    /// the hot entry point stays small enough to inline.
    ///
    /// # Safety
    ///
    /// As [`Self::eager_resched_reenable`], and the caller has already decremented the eager
    /// resched disable count once, observing `old_state` with
    /// `Self::eager_resched_disable_count_of(old_state) == 1` (or an underflow with
    /// `Self::eager_resched_disable_count_of(old_state) == 0`, which panics).
    #[cold]
    #[inline(never)]
    unsafe fn eager_resched_reenable_slow(&self, old_state: u32) {
        assert!(
            Self::eager_resched_disable_count_of(old_state) != 0,
            "eager resched disable count underflow: unmasked preemption state: {old_state:#x}"
        );

        // First check the expected case.
        if old_state == 1 << Self::EAGER_RESCHED_DISABLE_SHIFT {
            // Counts are both zero and there's no timeslice extension.  The inline wrapper only
            // gets here in that case when a preemption is pending, so flush directly.
            //
            // Flushing all might reschedule this CPU, make sure it's OK to block.
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending_continued(Flush::ALL) };
            return;
        }

        // The inline wrapper already returned when the eager resched disable count was still
        // non-zero (nothing to do since eager resched disable implies preempt disable).

        // We know we can at least flush remote.  Can we also flush local?
        if Self::preempt_disable_count_of(old_state) > 0 {
            // Nope, we've got a non-zero preempt disable count.
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending(Flush::REMOTE) };
            return;
        }

        // Is there an active extension?
        if Self::has_active_timeslice_extension(old_state) {
            // Has it expired?
            if self.clear_active_timeslice_extension_if_expired() {
                // Yes, preempt disable count is zero and the active extension has
                // expired.  We can flush all.
                debug_assert!(self.preempt_is_enabled());
                // SAFETY: Forwarded from the caller.
                unsafe { self.flush_pending(Flush::ALL) };
                return;
            }
            // Extension is active, can't flush local.
        }

        // We have an inactive extension or an unexpired active extension.  Either
        // way, we can flush remote, but not local.
        // SAFETY: Forwarded from the caller.
        unsafe { self.flush_pending(Flush::REMOTE) };
    }
    // LINT.ThenChange(//zircon/kernel/include/kernel/thread.h:PreemptionStateFastPaths)

    /// Sets a timeslice extension if one is not already set.
    ///
    /// This method should only be called in normal thread context.
    ///
    /// Returns false if a timeslice extension was already present or if the
    /// supplied duration is <= 0.
    ///
    /// Note: It OK to call this from a context where preemption is (hard)
    /// disabled.  If preemption is requested while the preempt disable count is
    /// non-zero and a timeslice extension is in place, the extension will be
    /// activated, but preemption will not occur until the count has dropped to
    /// zero and the extension has expired or has been clear.
    #[inline]
    pub fn set_timeslice_extension(
        &self,
        extension_duration: zx_types::zx_duration_mono_t,
    ) -> bool {
        if extension_duration <= 0 {
            return false;
        }

        let state = self.state.load();
        if Self::has_timeslice_extension(state) {
            return false;
        }
        self.timeslice_extension.store(extension_duration);
        // Make sure that the timeslice extension value becomes visible to an
        // interrupt handler in this thread prior to the state flag becoming
        // visible.  See comment at |timeslice_extension|.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.state.fetch_or(Self::TIMESLICE_EXTENSION_PRESENT);
        true
    }

    /// Unconditionally clears any timeslice extension.
    ///
    /// This method must be called in normal thread context because it may trigger
    /// local preemption.
    ///
    /// Blocking must be allowed (`debug_assert!`ed on the flush path, as in C++).
    ///
    /// # Safety
    ///
    /// `self` must be the *current* thread's preemption state (see [`Self::preempt_reenable`]).
    #[inline]
    pub unsafe fn clear_timeslice_extension(&self) {
        // Clear any present timeslice extension.
        let old_state = self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
        // Are the counters both zero?
        if (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0 {
            // SAFETY: Forwarded from the caller.
            unsafe { self.flush_pending(Flush::LOCAL) };
        }
    }

    /// PreemptSetPending() marks a pending preemption for the given CPUs.
    ///
    /// This is similar to Reschedule(), except that it may only be used inside an
    /// interrupt handler while interrupts and preemption are disabled, between
    /// PreemptDisable() and PreemptReenable(). It is similar to Reschedule(),
    /// except that it does not need to be called with thread's lock held.
    pub fn preempt_set_pending(&self, reschedule_mask: cpu_mask_t) {
        debug_assert!(crate::arch_rs::ints_disabled());
        debug_assert!(crate::arch_rs::blocking_disallowed());
        debug_assert!(!self.preempt_is_enabled());

        // Latch the local CPU's mask before the atomic RMW below (interrupts are disabled, so the
        // CPU cannot change) so that the compiler can share it with the caller's copy.
        let local_cpu_mask = crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());

        self.preempts_pending.fetch_or(reschedule_mask);

        // Are we pending for the local CPU?
        let local_cpu_in_resched_mask = (reschedule_mask & local_cpu_mask) != 0;
        if !local_cpu_in_resched_mask {
            // Nope.
            return;
        }

        self.evaluate_timeslice_extension();
    }

    /// Evaluate the thread's timeslice extension (if present), activating or
    /// expiring it as necessary.
    ///
    /// Returns whether preemption is enabled.
    #[inline]
    pub fn evaluate_timeslice_extension(&self) -> bool {
        let old_state = self.state.load();
        if old_state == 0 {
            // No counts, no extension.  The common case.
            return true;
        }
        self.evaluate_timeslice_extension_slow(old_state)
    }

    /// The uncommon remainder of [`Self::evaluate_timeslice_extension`], after the inline
    /// fast-path check; `old_state` is the `state` value just loaded. Kept out of line so that the
    /// hot entry point stays small enough to inline.
    #[cold]
    #[inline(never)]
    fn evaluate_timeslice_extension_slow(&self, old_state: u32) -> bool {
        if !Self::has_timeslice_extension(old_state) {
            // No extension, but we have a non-zero count.
            return false;
        }

        if Self::has_active_timeslice_extension(old_state) {
            if !self.clear_active_timeslice_extension_if_expired() {
                return false;
            }
            // The active extension has expired.  If the counts are both zero, then
            // we're ready for preemption.
            return (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0;
        }

        // We have a not-yet-active extension.  Time to activate it.
        //
        // See comment at |timeslice_extension| for why the signal fence is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        let extension_duration = self.timeslice_extension.load();
        if extension_duration <= 0 {
            // Already expired.
            self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
            return (old_state & !Self::TIMESLICE_EXTENSION_FLAGS_MASK) == 0;
        }
        let deadline = (crate::platform_rs::timer::current_mono_time()
            + crate::platform_rs::timer::DurationMono(extension_duration))
        .0;
        self.timeslice_extension_deadline.store(deadline);
        // See comment at |timeslice_extension_deadline| for why the signal fence
        // is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.state.fetch_or(Self::TIMESLICE_EXTENSION_ACTIVE);
        Self::set_preemption_timer_for_extension(deadline);
        false
    }

    /// Resets the preemption state. This should only be called when reviving an
    /// idle/power thread that halted while taking a CPU offline.
    ///
    /// # Safety
    ///
    /// The owning thread must be halted (not running on any CPU) with its thread lock held
    /// exclusively and no [`PreemptDisabledToken`] for it live; `reset` exists only for
    /// [`revive_idle_power_thread`].
    unsafe fn reset(&self) {
        // `Relaxed` ordering suffices because the owning idle/power thread is halted (not running
        // on any CPU) and its thread lock is held exclusively by `revive_idle_power_thread`, which
        // releases the lock before the thread can be scheduled again.
        self.state.store(0);
        self.preempts_pending.store(0);
        self.timeslice_extension.store(0);
        self.timeslice_extension_deadline.store(0);
    }

    const fn eager_resched_disable_count_of(state: u32) -> u32 {
        (state & Self::EAGER_RESCHED_DISABLE_MASK) >> Self::EAGER_RESCHED_DISABLE_SHIFT
    }

    const fn preempt_disable_count_of(state: u32) -> u32 {
        state & Self::PREEMPT_DISABLE_MASK
    }

    const fn has_timeslice_extension(state: u32) -> bool {
        (state & Self::TIMESLICE_EXTENSION_PRESENT) != 0
    }

    const fn has_active_timeslice_extension(state: u32) -> bool {
        (state & Self::TIMESLICE_EXTENSION_ACTIVE) != 0
    }

    /// Sets the preemption timer when a timeslice has been extended.
    fn set_preemption_timer_for_extension(deadline: zx_instant_mono_t) {
        // Interrupts must be disabled when calling PreemptReset.
        let _interrupt_disable = crate::arch_rs::InterruptDisableGuard::new();
        // SAFETY: Interrupts are disabled for the call, as `TimerQueue::PreemptReset` requires,
        // and `PerCpu::get_current()` returns the current CPU's `PerCpu`.
        unsafe {
            crate::kernel::percpu::PerCpu::get_current()
                .timer_queue
                .preempt_reset(InstantMono(deadline))
        };
        THREAD_TIMESLICE_EXTENDED.add(1);
    }

    /// Checks whether the active timeslice extension has expired and if so, clears
    /// it and returns true.
    ///
    /// Should only be called when there is an active timeslice extension.
    fn clear_active_timeslice_extension_if_expired(&self) -> bool {
        // Has the extension expired?
        //
        // See comment at |timeslice_extension_deadline| for why the signal fence is needed.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        if crate::platform_rs::timer::current_mono_time()
            >= crate::platform_rs::timer::InstantMono(self.timeslice_extension_deadline.load())
        {
            self.state.fetch_and(!Self::TIMESLICE_EXTENSION_FLAGS_MASK);
            return true;
        }
        false
    }

    /// Flushes local, remote, or all pending preemptions.
    ///
    /// This method is kept split from [`Self::flush_pending_continued`] so that the early-out
    /// case of no pending preemptions stays small and inlinable.
    ///
    /// # Safety
    ///
    /// `self` must be the *current* thread's preemption state: flushing a local preemption
    /// reschedules the current thread, and `&self` stays valid across that only because the
    /// current thread outlives its own run.
    #[inline]
    unsafe fn flush_pending(&self, flush: Flush) {
        // Early out to avoid unnecessarily taking the thread lock. This check races
        // any potential flush due to context switch, however, the context switch can
        // only clear bits that would have been flushed below, no new pending
        // preemptions are possible in the mask bits indicated by |flush|.
        if self.preempts_pending.load() == 0 {
            return;
        }
        core::hint::cold_path();
        // SAFETY: Forwarded from the caller.
        unsafe { self.flush_pending_continued(flush) }
    }

    /// The out-of-line part of [`Self::flush_pending`], entered only when a preemption is
    /// pending.
    ///
    /// # Safety
    ///
    /// As [`Self::flush_pending`].
    #[cold]
    #[inline(never)]
    unsafe fn flush_pending_continued(&self, flush: Flush) {
        // If we're flushing the local CPU, make sure OK to block since flushing
        // local may trigger a reschedule.
        debug_assert!(!flush.contains(Flush::LOCAL) || !crate::arch_rs::blocking_disallowed());

        let _interrupt_disable = crate::arch_rs::InterruptDisableGuard::new();
        // Recheck, pending preemptions could have been flushed by a context switch
        // before interrupts were disabled.
        let pending_mask = self.preempts_pending.load();

        // If there is a pending local preemption the scheduler will take care of
        // flushing all pending reschedules.
        let current_cpu_mask =
            crate::kernel::types::cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
        if (pending_mask & current_cpu_mask) != 0 && flush.contains(Flush::LOCAL) {
            // Clear the local preempt pending flag before calling preempt.  Failure
            // to do this can cause recursion during scheduler::preempt if any code
            // (such as debug tracing code) attempts to disable and re-enable
            // preemption during the scheduling operation.
            self.preempts_pending.fetch_and(!current_cpu_mask);

            // TODO(johngro): We cannot be holding the current thread's lock while we do
            // this.  Is there a good way to enforce this requirement via either static
            // annotation or dynamic checks?
            //
            // `scheduler::preempt` requires that no `ChainLockTransaction` is active on this CPU
            // (`TA_EXCL(chainlock_transaction_token)` in C++).  Like the C++ `DEBUG_ASSERT` via
            // `ChainLockTransaction::AssertNoTransactionActive()`, this is a debug-only check:
            // `scheduler::preempt` enters `single_chain_lock_guard!`, whose
            // `ChainLockTransaction::register` asserts the same condition in release builds.
            crate::kconcurrent::debug_assert_no_transaction_active();
            // SAFETY: No `ChainLockTransaction` is active on the current CPU (debug-asserted
            // above, and asserted unconditionally by `ChainLockTransaction::register` inside
            // `scheduler::preempt`'s `single_chain_lock_guard!`), which is `scheduler::preempt`'s
            // only precondition. `self` is the *current* thread's preemption state (the caller's
            // contract), and the current thread outlives its own run, so `&self` stays valid
            // across the resulting context switch; `PreemptionState` is all relaxed atomics, so
            // the scheduler's and interrupt handlers' writes through it meanwhile are sound.
            unsafe {
                crate::kernel::scheduler::preempt(
                    crate::kernel::scheduler::PreemptType::FlushPending,
                );
            }
        } else if flush.contains(Flush::REMOTE) {
            // The current cpu is ignored by mp_reschedule if present in the mask.
            crate::kernel::mp::reschedule(pending_mask, 0);
            self.preempts_pending.fetch_and(current_cpu_mask);
        }
    }
}

// Compile-time layout and constant assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<PreemptionState>() == size_of::<layout::PreemptionStateStorage>());
static_assert!(align_of::<PreemptionState>() == align_of::<layout::PreemptionStateStorage>());
static_assert!(offset_of!(PreemptionState, state) == layout::PreemptionStateLayout_kStateOffset);
static_assert!(
    offset_of!(PreemptionState, preempts_pending)
        == layout::PreemptionStateLayout_kPreemptsPendingOffset
);
static_assert!(
    offset_of!(PreemptionState, timeslice_extension)
        == layout::PreemptionStateLayout_kTimesliceExtensionOffset
);
static_assert!(
    offset_of!(PreemptionState, timeslice_extension_deadline)
        == layout::PreemptionStateLayout_kTimesliceExtensionDeadlineOffset
);
static_assert!(PreemptionState::MAX_COUNT_VALUE == layout::PreemptionStateLayout_kMaxCountValue);
static_assert!(
    PreemptionState::PREEMPT_DISABLE_MASK == layout::PreemptionStateLayout_kPreemptDisableMask
);
static_assert!(
    PreemptionState::EAGER_RESCHED_DISABLE_SHIFT
        == layout::PreemptionStateLayout_kEagerReschedDisableShift
);
static_assert!(
    PreemptionState::EAGER_RESCHED_DISABLE_MASK
        == layout::PreemptionStateLayout_kEagerReschedDisableMask
);
static_assert!(
    PreemptionState::TIMESLICE_EXTENSION_FLAGS_SHIFT
        == layout::PreemptionStateLayout_kTimesliceExtensionFlagsShift
);
static_assert!(
    PreemptionState::TIMESLICE_EXTENSION_FLAGS_MASK
        == layout::PreemptionStateLayout_kTimesliceExtensionFlagsMask
);
static_assert!(
    PreemptionState::TIMESLICE_EXTENSION_PRESENT
        == layout::PreemptionStateLayout_kTimesliceExtensionPresent
);
static_assert!(
    PreemptionState::TIMESLICE_EXTENSION_ACTIVE
        == layout::PreemptionStateLayout_kTimesliceExtensionActive
);

/// `TaskState` is responsible for running the task defined by `entry(arg)`, and reporting its
/// return value to any joining threads.
///
/// TODO: the detached state in `Thread::flags` probably belongs here.
#[pin_data]
#[repr(C)]
pub struct TaskState {
    // Safety invariant: written once by `TaskState::init` before the owning `Thread` is published;
    // never mutated afterwards.
    /// The Thread's entry point, and its argument.
    entry: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    arg: *mut c_void,

    // Safety invariant: written by the exiting thread under its own lock (`current_exit`) and read
    // by a joiner under the same lock after observing `ThreadStateKind::Death` (`ThreadPtr::join`),
    // so the thread's lock orders the two; see `retcode`.
    /// Storage for the return code.
    ///
    /// TODO: This is logically protected by our thread's lock.  What is a
    /// good way to represent this with a static annotation?
    retcode: UnsafeCell<i32>,

    /// Other threads waiting to join this Thread.
    #[pin]
    retcode_wait_queue: WaitQueue,
}

impl TaskState {
    /// Returns an in-place initializer for a default [`TaskState`].
    #[inline]
    fn init_default() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            entry: None,
            arg: ptr::null_mut(),
            retcode: UnsafeCell::new(0),
            retcode_wait_queue <- WaitQueue::init(),
        })
    }

    /// The thread's entry point; `None` for threads driven by an alternate trampoline, which
    /// `create_etc` allows to be created without one.
    #[inline]
    fn entry(&self) -> Option<unsafe extern "C" fn(*mut c_void) -> i32> {
        self.entry
    }

    /// The argument passed to the entry point.
    #[inline]
    pub(crate) fn arg(&self) -> *mut c_void {
        self.arg
    }

    /// The thread's return code.
    ///
    /// # Safety
    ///
    /// The caller must exclude a concurrent write by the exiting thread (`current_exit`): either it
    /// holds the owning thread's lock (the return code is written under that lock, and
    /// `ThreadPtr::join` reads it under the same lock after observing `ThreadStateKind::Death`), or
    /// the thread has not started running yet.
    #[inline]
    pub unsafe fn retcode(&self) -> i32 {
        // SAFETY: The caller excludes concurrent writers; no reference is formed.
        unsafe { self.retcode.get().read() }
    }

    /// Records the thread's entry point and argument.
    ///
    /// Called from `create_etc` on a fully constructed but not yet published `Thread`, so the
    /// fields are written through the raw pointer and no `&TaskState` is formed.
    ///
    /// # Safety
    ///
    /// `this` points to a valid `TaskState` not yet visible to any other thread, with no live Rust
    /// reference to it.
    #[inline]
    unsafe fn init(
        this: *mut Self,
        entry: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
        arg: *mut c_void,
    ) {
        // SAFETY: The caller guarantees the pointer is valid for writes with no live Rust
        // reference; projecting the fields through the raw pointer forms none.
        unsafe {
            (&raw mut (*this).entry).write(entry);
            (&raw mut (*this).arg).write(arg);
        }
    }

    /// Blocks `current_thread` in this thread's `retcode_wait_queue` until that thread exits or
    /// `deadline` passes.
    ///
    /// The queue's lock is released by this call because the C++ is `TA_REL(get_lock())`;
    /// `current_thread`'s lock stays held on return (`TA_REQ`).
    ///
    /// There is deliberately no `&self`: the target thread (and so its `TaskState`) may be torn
    /// down while the joiner is blocked (`Detach` wakes joiners with `BAD_STATE` and a detached
    /// target then exits and frees itself, possibly before the woken joiner runs), so no reference
    /// into it may be live across `scheduler::block`. The `&WaitQueue` and its [`OwnerGuard`] are
    /// scoped to [`WaitQueue::block_etc_preamble`], which releases the queue lock before the
    /// thread blocks, and [`WaitQueueBase::block_etc_postamble`] dereferences the returned
    /// `base_ptr` only once, before `scheduler::block`.
    ///
    /// # Safety
    ///
    /// - `this` points to a valid `TaskState` at the time of the call, and its
    ///   `retcode_wait_queue` remains valid while `current_thread` remains blocked in it (and at
    ///   least until both the queue lock and `current_thread`'s lock are released during the block
    ///   operation).
    /// - `current_thread.owner()` is the currently executing thread in `THREAD_RUNNING`, not queued
    ///   in any wait queue, in a context where blocking is permitted (no spinlocks held, blocking
    ///   not disallowed).
    /// - The active transaction is finalized with `(*this).retcode_wait_queue`'s lock and
    ///   `current_thread`'s lock as the only held chain locks, with no other guard or proof for
    ///   the queue lock live; this call takes over releasing the queue lock.
    pub unsafe fn join(
        this: *const Self,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: InstantMono,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        let deadline = Deadline::no_slack(deadline.into());
        // The `&WaitQueue` and its guard live only inside this block: `block_etc_preamble`
        // releases the queue lock before the thread blocks, and only the raw `base_ptr` is passed
        // to `block_etc_postamble`.
        let base_ptr = {
            // SAFETY: `this` is valid at the time of the call per the caller; projecting the field
            // forms no reference to the `TaskState` itself, and the `&WaitQueue` does not outlive
            // this block.
            let queue = unsafe { &(*this).retcode_wait_queue };
            // SAFETY: The queue's lock is held by the active transaction with no other guard or
            // proof (precondition), and this guard takes over releasing it.
            let guard = unsafe { OwnerGuard::adopt_unchecked(queue, clt_token) };
            // SAFETY: `current_thread` is the running thread, not queued, in a context where
            // blocking is permitted; `deadline` is monotonic; the active transaction is finalized
            // with `guard`'s and `current_thread`'s locks as the only held chain locks; and the
            // queue remains valid while `current_thread` is blocked in it.
            unsafe {
                WaitQueue::block_etc_preamble(
                    guard,
                    current_thread,
                    &deadline,
                    0,
                    ResourceOwnership::Normal,
                    Interruptible::No,
                    clt_token,
                )?
            }
        };
        // SAFETY: `block_etc_preamble` returned `Ok(base_ptr)` with `current_thread` inserted in
        // `base_ptr` and the queue lock released; `base_ptr` remains valid while `current_thread`
        // is blocked in it; and `current_thread`'s lock is the only held chain lock in the
        // finalized transaction.
        unsafe {
            WaitQueueBase::block_etc_postamble(
                base_ptr,
                current_thread.reborrow(),
                &deadline,
                clt_token,
            )
        }
    }

    /// Returns the lock of the queue that joiners wait in (`TaskState::get_lock`).
    #[inline]
    fn get_lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        self.retcode_wait_queue.lock()
    }

    /// Attempt to wake all of our joiners.  This operation might fail because of a
    /// conflict while attempting to obtain the required locks, in which case the
    /// caller needs to back off (drop all currently held chain locks) and try
    /// again.
    ///
    /// [`current_exit`] restarts its finalized transaction before calling this in a loop with its
    /// own lock held. On `true`, `wake_all_locked` finalizes the transaction and `guard.release()`
    /// drops the queue lock (`ChainLock::release` does not require an unfinalized transaction); on
    /// `false`, either `acquire_or_backoff` or `wake_all_locked` backed off without finalizing.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction is not already finalized on entry.
    /// - Neither `self.retcode_wait_queue`'s lock nor any joiner thread's lock in
    ///   `self.retcode_wait_queue` is currently held by the active transaction, no `&mut Thread`,
    ///   [`OwnerGuard<'_, Thread>`], or [`ChainLockProof<'_, Thread>`] for any joiner thread in
    ///   `self.retcode_wait_queue` is live across this call, and no `&Thread` for any joiner
    ///   thread in `self.retcode_wait_queue` is live across this call unless the caller otherwise
    ///   keeps that thread alive.
    unsafe fn try_wake_joiners(
        &self,
        status: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> bool {
        // Attempt to lock the queue first.
        let Some(mut guard) = OwnerGuard::acquire_or_backoff(&self.retcode_wait_queue, clt_token)
        else {
            core::hint::cold_path();
            return false;
        };

        // Now, try to lock and wake all of our waiting threads.
        // SAFETY: Forwarded from the caller.
        let result =
            unsafe { guard.proof().wake_all_locked(status, clt_token, preempt_disabled_token) };

        // No matter what just happened, we need to drop the queue's lock.
        guard.release();

        // If we got a result, then we succeeded in waking the threads we wanted to
        // wake.
        result.is_some()
    }
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<TaskState>() == size_of::<layout::TaskStateStorage>());
static_assert!(align_of::<TaskState>() == align_of::<layout::TaskStateStorage>());
static_assert!(offset_of!(TaskState, entry) == layout::TaskStateLayout_kEntryOffset);
static_assert!(offset_of!(TaskState, arg) == layout::TaskStateLayout_kArgOffset);
static_assert!(offset_of!(TaskState, retcode) == layout::TaskStateLayout_kRetcodeOffset);
static_assert!(
    offset_of!(TaskState, retcode_wait_queue) == layout::TaskStateLayout_kRetcodeWaitQueueOffset
);

/// Keeps track of whether a thread is allowed to allocate memory.
///
/// A thread's |MemoryAllocationState| should only be accessed by that thread itself or interrupt
/// handlers running in the thread's context.
#[repr(C)]
pub struct MemoryAllocationState {
    /// Notice that we aren't using atomic operations to access the field.  We don't need atomic
    /// operations here as long as...
    ///
    /// 1. We use atomic_signal_fence to prevent compiler reordering.
    ///
    /// 2. We use volatile to ensure the compiler actually generates loads and stores for the value
    ///    (so the interrupt handler can see what the thread see, and vice versa).
    ///
    /// 3. Upon completion, an interrupt handler that modified the field restores it to the value
    ///    it held at the start of the interrupt.
    // Accessed only by the owning thread and its interrupt handlers.
    disable_count: UnsafeCell<u32>,
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(
    size_of::<MemoryAllocationState>() == size_of::<layout::MemoryAllocationStateStorage>()
);
static_assert!(
    align_of::<MemoryAllocationState>() == align_of::<layout::MemoryAllocationStateStorage>()
);
static_assert!(
    offset_of!(MemoryAllocationState, disable_count)
        == layout::MemoryAllocationStateLayout_kDisableCountOffset
);

impl MemoryAllocationState {
    /// Creates a new [`MemoryAllocationState`] with allocations enabled (`disable_count == 0`).
    #[inline]
    pub const fn new() -> Self {
        Self { disable_count: UnsafeCell::new(0) }
    }

    /// Increments the memory-allocation disable count for this thread.
    ///
    /// # Safety
    ///
    /// Must only be called on the current thread's [`MemoryAllocationState`] and must be balanced
    /// with a corresponding call to [`Self::enable`] on the same thread.
    #[inline]
    pub unsafe fn disable(&self) {
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        // SAFETY: `self.disable_count` is only accessed by the owning thread or interrupt handlers
        // running in its context, using volatile loads/stores surrounded by `SeqCst` compiler
        // fences without forming references to the `u32`.
        unsafe {
            let count = core::ptr::read_volatile(self.disable_count.get());
            core::ptr::write_volatile(self.disable_count.get(), count + 1);
        }
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
    }

    /// Decrements the memory-allocation disable count for this thread.
    ///
    /// # Safety
    ///
    /// Must only be called on the current thread's [`MemoryAllocationState`] to balance a prior
    /// call to [`Self::disable`] on the same thread.
    #[inline]
    pub unsafe fn enable(&self) {
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        // SAFETY: As in `disable`.
        unsafe {
            let count = core::ptr::read_volatile(self.disable_count.get());
            debug_assert!(count > 0);
            core::ptr::write_volatile(self.disable_count.get(), count - 1);
        }
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
    }

    /// Returns true if memory allocation is allowed.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        // SAFETY: As in `disable`.
        let count = unsafe { core::ptr::read_volatile(self.disable_count.get()) };
        count == 0
    }
}

/// Buffering for Debuglog output (C++ `Thread::Linebuffer`).
#[repr(C)]
#[derive(Debug)]
pub struct Linebuffer {
    /// Number of bytes of `buffer` in use.
    pub pos: usize,
    /// Output buffered until the next newline or until the buffer is full.
    pub buffer: [u8; Self::BUFFER_SIZE],
}

impl Linebuffer {
    /// Size in bytes of `buffer`.
    pub const BUFFER_SIZE: usize = 128;

    /// Creates an empty line buffer.
    pub const fn new() -> Self {
        Self { pos: 0, buffer: [0; Self::BUFFER_SIZE] }
    }
}

impl Default for Linebuffer {
    fn default() -> Self {
        Self::new()
    }
}

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(Linebuffer::BUFFER_SIZE == layout::ThreadLinebufferLayout_kBufferSize);
static_assert!(size_of::<Linebuffer>() == size_of::<layout::ThreadLinebufferStorage>());
static_assert!(align_of::<Linebuffer>() == align_of::<layout::ThreadLinebufferStorage>());
static_assert!(offset_of!(Linebuffer, pos) == layout::ThreadLinebufferLayout_kPosOffset);
static_assert!(offset_of!(Linebuffer, buffer) == layout::ThreadLinebufferLayout_kBufferOffset);

/// A Zircon kernel thread.
///
/// Its layout is shared with `<kernel/thread.h>` and checked against `<kernel/thread_layout.h>`.
/// `Thread` is always constructed and destroyed in place in Rust (via `pin_init` /
/// `kalloc::Box::try_pin_init` / [`Thread::init_in_place`]) and is never moved by value. Members
/// Rust does not access directly are `zr::Opaque` storage of the corresponding size and alignment.
///
/// `KCell<_, ThreadLockClass>` marks members guarded by the thread's `ChainLock`; they are
/// reached only through accessors on `ChainLockProof<'_, Thread>`. Comments on the remaining
/// interior-mutable members state their synchronization rules.
///
/// # Safety Invariant
///
/// Any `&Thread` or `&mut Thread` reference must point to a valid, initialized `Thread`.
// LINT.IfChange(ThreadLayout)
#[derive(DoublyLinkedListContainable, SinglyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Thread {
    // The `ChainLockable` base: the lock guarding the `KCell<_, ThreadLockClass>` members.
    // `ClassedChainLock` is `repr(transparent)` over `ChainLock`, so a plain `ChainLock` sits at
    // offset 0.
    lock: ClassedChainLock<ThreadLockClass>,

    canary: fbl::Canary<{ fbl::magic(b"thrd") }>,

    // These fields are among the most active in the thread. They are grouped
    // together near the front to improve cache locality.
    //
    // `scheduler_variable_lock_` (a zero-sized `fbl::NullLock`) has no storage and is omitted.
    //
    // Relaxed atomic. Each flag is an independent bit: the owning thread mutates
    // `THREAD_FLAG_RESTRICTED_KICK_PENDING` lock-free, `THREAD_FLAG_IDLE`/`THREAD_FLAG_VCPU` are
    // set during setup, and `THREAD_FLAG_DETACHED`/`THREAD_FLAG_FREE_STRUCT` are set at creation
    // or under `get_lock()`.
    flags: RelaxedAtomicU32,
    signals: RelaxedAtomicU32,
    // Guard: split (see `scheduler::SchedulerState`). `state`, `base_profile`,
    // `inherited_profile_values` and the `effective_profile` dirty flags are written under
    // `get_lock()`; the queue-related members (`start_time`, `finish_time`, time slices,
    // `activation_count`, the run-queue node, `subtree_invariants`) under the owning scheduler's
    // `queue_lock_`; `curr_cpu`/`last_cpu` are read lock-free. Read through
    // `ChainLockProof::<Thread>::scheduler_state`.
    #[pin]
    scheduler_state: Opaque<layout::ThreadSchedulerStateStorage>,
    // Guard: `scheduler_variable_lock_` (a `fbl::NullLock`), i.e. the scheduler's own discipline.
    #[pin]
    scheduler_queue_state: Opaque<layout::ThreadSchedulerQueueStateStorage>,
    // Guard: `get_lock()`; each member of `ThreadState` carries its own cell type.
    #[pin]
    pub(super) wait_queue_state: ThreadState,
    // Guard: `get_lock()`. Matches `Thread::owned_wait_queues_` `OpaqueStorage` in `thread.h`.
    #[pin]
    owned_wait_queues: KCell<OwnedWaitQueueList, ThreadLockClass>,
    #[pin]
    task_state: TaskState,
    preemption_state: PreemptionState,
    memory_allocation_state: MemoryAllocationState,

    /// Must only be accessed by "this" thread.
    // Owned `RestrictedState` pointer; reached through `current_restricted_state`.
    restricted_state: UnsafeCell<*mut c_void>,

    /// state for runtime lock validation when in thread context
    #[cfg(with_lock_dep)]
    #[pin]
    lock_state: Opaque<layout::ThreadLockStateStorage>,

    /// The current address space this thread is associated with.
    /// This can be null if this is a kernel thread.
    /// See the comments near active_aspace() for more details.
    aspace: AtomicPtr<VmAspace>,

    // Guard: the owning thread (set and cleared around a ktrace test). Atomic because interrupt
    // handlers on the owning thread's CPU read it while the thread may be updating it.
    #[cfg(ktest)]
    rust_ktrace_override: AtomicPtr<c_void>,

    /// Saved by SignalPolicyException() to store the type of policy error, and
    /// passed to exception dispatch in ProcessPendingSignals().
    extra_policy_exception_code: KCell<u32, ThreadLockClass>,
    extra_policy_exception_data: KCell<u32, ThreadLockClass>,

    /// Is this thread allowed to own wait queues?  Set to false by a thread as
    /// it exits, but before it reaches the DEAD state.
    can_own_wait_queues: KCell<bool, ThreadLockClass>,

    /// Strong reference to user thread if one exists for this thread.
    /// In the common case freeing Thread will also free ThreadDispatcher when this
    /// reference is dropped.
    // `RefPtr<ThreadDispatcher>` storage; stored once with `Release` ordering during user-thread
    // setup (`set_usermode_thread`) and loaded lock-free afterwards (`Acquire` by cross-thread
    // `thread_list` readers via `Thread::user_thread`, or `Relaxed` on self-thread and scheduler
    // paths via `Thread::user_thread_ptr` where `set_usermode_thread` + `resume()` under the
    // thread lock already establishes happens-before).
    user_thread: AtomicPtr<crate::object::ThreadDispatcher>,

    /// When user_thread_ is set, these values are copied from ThreadDispatcher and
    /// its parent ProcessDispatcher. Kernel threads maintain an independent tid.
    // Written during thread setup (or `SetUserThread` before the thread runs) and read lock-free
    // with relaxed ordering.
    tid: RelaxedAtomicU64,
    pid: RelaxedAtomicU64,

    /// Architecture-specific stuff.
    #[pin]
    arch: Opaque<layout::ThreadArchStorage>,

    /// `KernelStack` storage. Constructed in place by Rust (`Thread::init`), initialized via
    /// `stack_init` / `KernelStack::init`, read through `stack`, and torn down on `Thread` drop.
    #[pin]
    stack: Opaque<layout::ThreadStackStorage>,

    /// Per-thread deferred object deletion state (`deferred_deleter` and legacy
    /// `dispatcher.cc:SafeDeleter`).
    // Guard: the owning thread.
    recursive_object_deletion_list: UnsafeCell<*mut c_void>,

    /// This always includes the trailing NUL.
    // NUL-terminated name buffer, byte-wise relaxed atomics: `set_name` (and C++
    // `Thread::Current::SetName`, which forwards to it) writes it without a lock and readers
    // (`name_bytes`, `name_bytes_ptr`, C++ `name()`) copy or read it lock-free, tolerating a torn
    // name. Same layout as `char[ZX_MAX_NAME_LEN]`.
    #[pin]
    name: [AtomicU8; ZX_MAX_NAME_LEN],

    /// Buffering for Debuglog output.
    // Guard: the owning thread, when blocking is allowed.
    linebuffer: UnsafeCell<Linebuffer>,

    /// The flow id allocated before blocking on the last lock.
    lock_flow_id: RelaxedAtomicU64,

    /// Indicates whether user register state (debug, vector, fp regs, etc.) has been saved to the
    /// arch_thread_t as part of thread suspension / exception handling.
    ///
    /// When a user thread is suspended or generates an exception (synthetic or architectural) that
    /// might be observed by another process, we save user register state to the thread's
    /// arch_thread_t so that it may be accessed by a debugger.  Upon leaving a suspended or
    /// exception state, we restore user register state.
    user_state_saved: KCell<bool, ThreadLockClass>,

    /// For threads with migration functions, indicates whether a migration is in progress. When
    /// true, the migrate function has been called with Before but not yet with After.
    ///
    /// TODO: What to do about this?  Should it be protected by the
    /// thread's lock?  Does the thread's lock need to be held exclusively when a
    /// thread's migrate function is called?
    migrate_pending: KCell<bool, ThreadLockClass>,

    /// Provides a way to execute custom logic when a thread must be migrated between CPUs.
    // Wrapped in `ManuallyDrop` so `PinnedDrop::drop` tears down callbacks before `rseq_accessor`
    // and `canary`, and Rust's automatic field drop glue skips them.
    migrate_fn: KCell<ManuallyDrop<CallbackSlot<MigrateFn>>, ThreadLockClass>,

    /// Provides a way to execute custom logic when a thread is context switched to or away from.
    // Wrapped in `ManuallyDrop` (see `migrate_fn`).
    context_switch_fn: KCell<ManuallyDrop<CallbackSlot<ContextSwitchFn>>, ThreadLockClass>,

    /// An Accessor for the thread's restartable sequence structure, if any.
    ///
    /// This is used to detect when a thread is preempted or enters a waiting state
    /// while in a restartable sequence.  When this happens, the restartable sequence
    /// is interrupted, and the thread will restart the sequence when it resumes.
    ///
    /// Kernel threads do not use restartable sequences, so this will always be !IsValid() for
    /// kernel threads.
    ///
    /// Must be accessed only by the current thread.
    // Wrapped in `ManuallyDrop` so `PinnedDrop::drop` tears it down after callbacks and before
    // `canary`, and Rust's automatic field drop glue skips it.
    rseq_accessor: UnsafeCell<ManuallyDrop<crate::page_map::Accessor<'static, zx_rseq_t>>>,

    /// The sampling session this thread is associated with.
    sampling_session: RelaxedAtomicU64,

    /// Used to track threads that have set `migrate_fn`. This is used to migrate
    /// threads before a CPU is taken offline.
    // Guard: `Thread::get_list_lock()`.
    // Safety invariant: while linked, this node is linked only into `MIGRATE_LIST`.
    #[pin]
    #[dll_node(tag = MigrateListTag)]
    migrate_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the global thread list.
    // Guard: `Thread::get_list_lock()`.
    // Safety invariant: while linked, this node is linked only into the global thread list.
    #[pin]
    #[dll_node(tag = ThreadListTag)]
    thread_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the temporary batch unblock list.
    // Guard: `get_lock()`; the node's own `UnsafeCell`s stand in for a `KCell`, which the list
    // traits cannot see through.
    #[pin]
    #[dll_node(tag = UnblockListTag)]
    unblock_list_node: DoublyLinkedListNode<Thread>,

    /// Node storage for existing on the save state list.
    // Guard: `get_lock()`; as for `unblock_list_node`.
    #[pin]
    #[sll_node(tag = SaveStateListTag)]
    save_state_list_node: SinglyLinkedListNode<Thread>,

    /// State of the thread with respect to memory stalls.
    ///
    /// It can only be accessed by the thread's current CPU. Hence, preemption must
    /// be disabled while accessing it.
    // Guard: `preempt_disabled_token`; see `memory_stall_state` / `set_memory_stall_state` above.
    memory_stall_state: UnsafeCell<ThreadStallState>,
}
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:ThreadLayout,
//   //zircon/kernel/include/kernel/thread_layout.h:ThreadLayout,
// )

// Compile-time layout assertions against <kernel/thread_layout.h>.
static_assert!(size_of::<Thread>() == size_of::<layout::ThreadStorage>());
static_assert!(align_of::<Thread>() == align_of::<layout::ThreadStorage>());
static_assert!(offset_of!(Thread, lock) == 0);
static_assert!(offset_of!(Thread, lock) == layout::ThreadLayout_kLockOffset);
static_assert!(offset_of!(Thread, canary) == layout::ThreadLayout_kCanaryOffset);
static_assert!(offset_of!(Thread, flags) == layout::ThreadLayout_kFlagsOffset);
static_assert!(offset_of!(Thread, signals) == layout::ThreadLayout_kSignalsOffset);
static_assert!(offset_of!(Thread, scheduler_state) == layout::ThreadLayout_kSchedulerStateOffset);
static_assert!(
    size_of::<scheduler::SchedulerState>() == size_of::<layout::ThreadSchedulerStateStorage>()
);
static_assert!(
    align_of::<scheduler::SchedulerState>() == align_of::<layout::ThreadSchedulerStateStorage>()
);
static_assert!(
    offset_of!(Thread, scheduler_queue_state) == layout::ThreadLayout_kSchedulerQueueStateOffset
);
static_assert!(
    size_of::<scheduler::SchedulerQueueState>()
        == size_of::<layout::ThreadSchedulerQueueStateStorage>()
);
static_assert!(
    align_of::<scheduler::SchedulerQueueState>()
        == align_of::<layout::ThreadSchedulerQueueStateStorage>()
);
static_assert!(offset_of!(Thread, wait_queue_state) == layout::ThreadLayout_kWaitQueueStateOffset);
static_assert!(
    offset_of!(Thread, owned_wait_queues) == layout::ThreadLayout_kOwnedWaitQueuesOffset
);
static_assert!(offset_of!(Thread, task_state) == layout::ThreadLayout_kTaskStateOffset);
static_assert!(offset_of!(Thread, preemption_state) == layout::ThreadLayout_kPreemptionStateOffset);
static_assert!(
    offset_of!(Thread, memory_allocation_state)
        == layout::ThreadLayout_kMemoryAllocationStateOffset
);
static_assert!(offset_of!(Thread, restricted_state) == layout::ThreadLayout_kRestrictedStateOffset);
#[cfg(with_lock_dep)]
static_assert!(offset_of!(Thread, lock_state) == layout::ThreadLayout_kLockStateOffset);
#[cfg(with_lock_dep)]
static_assert!(
    size_of::<crate::lockdep_rs::ThreadLockState>() == size_of::<layout::ThreadLockStateStorage>()
);
#[cfg(with_lock_dep)]
static_assert!(
    align_of::<crate::lockdep_rs::ThreadLockState>()
        == align_of::<layout::ThreadLockStateStorage>()
);
static_assert!(offset_of!(Thread, aspace) == layout::ThreadLayout_kAspaceOffset);
#[cfg(ktest)]
static_assert!(
    offset_of!(Thread, rust_ktrace_override) == layout::ThreadLayout_kRustKtraceOverrideOffset
);
static_assert!(
    offset_of!(Thread, extra_policy_exception_code)
        == layout::ThreadLayout_kExtraPolicyExceptionCodeOffset
);
static_assert!(
    offset_of!(Thread, extra_policy_exception_data)
        == layout::ThreadLayout_kExtraPolicyExceptionDataOffset
);
static_assert!(
    offset_of!(Thread, can_own_wait_queues) == layout::ThreadLayout_kCanOwnWaitQueuesOffset
);
static_assert!(offset_of!(Thread, user_thread) == layout::ThreadLayout_kUserThreadOffset);
static_assert!(offset_of!(Thread, tid) == layout::ThreadLayout_kTidOffset);
static_assert!(offset_of!(Thread, pid) == layout::ThreadLayout_kPidOffset);
static_assert!(offset_of!(Thread, arch) == layout::ThreadLayout_kArchOffset);
static_assert!(offset_of!(Thread, stack) == layout::ThreadLayout_kStackOffset);
static_assert!(size_of::<KernelStack>() == size_of::<layout::ThreadStackStorage>());
static_assert!(align_of::<KernelStack>() == align_of::<layout::ThreadStackStorage>());
static_assert!(
    offset_of!(Thread, recursive_object_deletion_list)
        == layout::ThreadLayout_kRecursiveObjectDeletionListOffset
);
static_assert!(offset_of!(Thread, name) == layout::ThreadLayout_kNameOffset);
static_assert!(size_of::<[AtomicU8; ZX_MAX_NAME_LEN]>() == size_of::<[c_char; ZX_MAX_NAME_LEN]>());
static_assert!(
    align_of::<[AtomicU8; ZX_MAX_NAME_LEN]>() == align_of::<[c_char; ZX_MAX_NAME_LEN]>()
);
static_assert!(offset_of!(Thread, linebuffer) == layout::ThreadLayout_kLinebufferOffset);
static_assert!(offset_of!(Thread, lock_flow_id) == layout::ThreadLayout_kLockFlowIdOffset);
static_assert!(offset_of!(Thread, user_state_saved) == layout::ThreadLayout_kUserStateSavedOffset);
static_assert!(offset_of!(Thread, migrate_pending) == layout::ThreadLayout_kMigratePendingOffset);
static_assert!(offset_of!(Thread, migrate_fn) == layout::ThreadLayout_kMigrateFnOffset);
static_assert!(size_of::<CallbackSlot<MigrateFn>>() == size_of::<layout::ThreadCallbackStorage>());
static_assert!(
    align_of::<CallbackSlot<MigrateFn>>() == align_of::<layout::ThreadCallbackStorage>()
);
static_assert!(size_of::<Option<MigrateFn>>() == size_of::<layout::ThreadCallbackStorage>());
static_assert!(align_of::<Option<MigrateFn>>() == align_of::<layout::ThreadCallbackStorage>());
static_assert!(
    offset_of!(Thread, context_switch_fn) == layout::ThreadLayout_kContextSwitchFnOffset
);
static_assert!(
    size_of::<CallbackSlot<ContextSwitchFn>>() == size_of::<layout::ThreadCallbackStorage>()
);
static_assert!(
    align_of::<CallbackSlot<ContextSwitchFn>>() == align_of::<layout::ThreadCallbackStorage>()
);
static_assert!(size_of::<Option<ContextSwitchFn>>() == size_of::<layout::ThreadCallbackStorage>());
static_assert!(
    align_of::<Option<ContextSwitchFn>>() == align_of::<layout::ThreadCallbackStorage>()
);
static_assert!(offset_of!(Thread, rseq_accessor) == layout::ThreadLayout_kRseqAccessorOffset);
static_assert!(offset_of!(Thread, sampling_session) == layout::ThreadLayout_kSamplingSessionOffset);
static_assert!(
    offset_of!(Thread, migrate_list_node) == layout::ThreadLayout_kMigrateListNodeOffset
);
static_assert!(offset_of!(Thread, thread_list_node) == layout::ThreadLayout_kThreadListNodeOffset);
static_assert!(
    offset_of!(Thread, unblock_list_node) == layout::ThreadLayout_kUnblockListNodeOffset
);
static_assert!(
    offset_of!(Thread, save_state_list_node) == layout::ThreadLayout_kSaveStateListNodeOffset
);
static_assert!(
    offset_of!(Thread, memory_stall_state) == layout::ThreadLayout_kMemoryStallStateOffset
);

// SAFETY: Every interior mutation Rust performs through `&Thread` is either on an atomic, on a
// `KCell<_, ThreadLockClass>` reached through a `ChainLockProof<'_, Thread>` (which proves `lock`
// is held), or on `wait_queue_state`'s tree bookkeeping from WAVL tree hooks that run under the
// containing wait queue's lock. The remaining `UnsafeCell` and `Opaque` members are accessed only
// under the synchronization rules on each field.
unsafe impl Sync for Thread {}

// SAFETY: `lock` guards every `KCell<_, ThreadLockClass>` member of `Thread` (including those of
// `wait_queue_state`), and is a plain field so the same instance is returned every time.
unsafe impl ChainLocked for Thread {
    type Class = ThreadLockClass;

    /// Returns the lock guarding this thread.
    fn lock(&self) -> &ClassedChainLock<ThreadLockClass> {
        &self.lock
    }
}

crate::ksync_kernel::declare_singleton_spinlock!(
    /// Singleton spinlock guarding `THREAD_LIST` and `MIGRATE_LIST` (`Thread::get_list_lock()`).
    pub ThreadListLockClass
);

/// Wrapper around an intrusive list stored inside `KCell<_, ThreadListLockClass>`.
#[pin_data]
#[repr(transparent)]
struct SyncList<L> {
    #[pin]
    list: L,
}

// SAFETY: `SyncList` is only used inside `KCell<_, ThreadListLockClass>` for the global thread
// and migrate lists, where all accesses are serialized by `ThreadListLockClass`.
unsafe impl<L> Send for SyncList<L> {}

/// The global thread list (`thread_list`).
///
/// This is a `LazyInit` type, since initial thread code manipulates the list before global
/// constructors are run. It is initialized by [`thread_lists_init`] in [`init_early`].
///
/// The unmangled `"thread_list"` symbol name is preserved for
/// `zircon/kernel/scripts/vmzircon-gdb.py`.
#[unsafe(export_name = "thread_list")]
static THREAD_LIST: LazyInit<KCell<SyncList<ThreadList>, ThreadListLockClass>, lazy_init::NoCheck> =
    LazyInit::uninit();

/// The global list of threads with migrate functions (`MIGRATE_LIST`).
static MIGRATE_LIST: LazyInit<
    KCell<SyncList<MigrateList>, ThreadListLockClass>,
    lazy_init::NoCheck,
> = LazyInit::uninit();

#[pinned_drop]
impl PinnedDrop for Thread {
    fn drop(self: Pin<&mut Self>) {
        // At this point, the thread must not be on the global thread list or migrate
        // list.
        debug_assert!(!self.thread_list_node.in_container());
        debug_assert!(!self.migrate_list_node.in_container());

        // Explicitly tear down callbacks, `rseq_accessor`, and `Opaque`/raw-owned resources in
        // reverse declaration order before Rust's automatic forward field drops run `canary`
        // (ensuring `canary` remains valid while callbacks, `rseq_accessor`, `stack`,
        // `user_thread`, and `restricted_state` drop).
        let this = self.project();

        // Drop registered migration and context-switch callbacks before `rseq_accessor` and other
        // fields (both are wrapped in `ManuallyDrop`, so field drop glue skips them), matching C++
        // `rust_thread_callbacks_drop(&migrate_fn_, &context_switch_fn_)`.
        // SAFETY: `migrate_fn` and `context_switch_fn` are initialized and wrapped in
        // `ManuallyDrop`; this is their sole destruction.
        unsafe {
            ManuallyDrop::drop(this.migrate_fn.get_inner_mut());
            ManuallyDrop::drop(this.context_switch_fn.get_inner_mut());
        }

        // Invalidate any rseq mapping before `stack` and `user_thread` teardown (`rseq_accessor`
        // is wrapped in `ManuallyDrop`, so field drop glue skips it).
        // SAFETY: `rseq_accessor` is initialized and wrapped in `ManuallyDrop`; this is its sole
        // destruction.
        unsafe { ManuallyDrop::drop(this.rseq_accessor.get_mut()) };

        // SAFETY: `stack` was initialized in place by `Thread::init_raw` as a `KernelStack`
        // (matching size and alignment asserted above), and `Opaque` does not drop its contents
        // automatically, so this is its sole destruction (calling `KernelStack::teardown()`).
        unsafe { ptr::drop_in_place(this.stack.get().cast::<KernelStack>()) };

        // Drop the strong reference to `ThreadDispatcher`, if any. `PinnedDrop::drop` holds
        // exclusive `Pin<&mut Self>`, and the thread join / `free_dpc` handoff already provides
        // happens-before with `set_usermode_thread`, so a non-atomic `get_mut` take suffices.
        let user_thread = core::mem::take(this.user_thread.get_mut());
        if !user_thread.is_null() {
            // SAFETY: `user_thread` was stored via `RefPtr::into_raw` in `set_usermode_thread`,
            // and replacing with null ensures it is released at most once.
            drop(unsafe { RefPtr::from_raw(user_thread) });
        }

        // Destroy `lock_state` (asserting `acquired_locks` is empty).
        #[cfg(with_lock_dep)]
        // SAFETY: `lock_state` was initialized in place by `ThreadLockState::init()` (matching
        // size and alignment asserted above), and `Opaque` does not drop its contents
        // automatically, so this is its sole destruction.
        unsafe {
            ptr::drop_in_place(this.lock_state.get().cast::<crate::lockdep_rs::ThreadLockState>());
        }

        // Destroy `RestrictedState`, if any.
        let rs = core::mem::take(this.restricted_state.get_mut());
        if !rs.is_null() {
            core::hint::cold_path();
            // SAFETY: `rs` was allocated via `kalloc::Box` in `RestrictedState::create` and
            // installed via `current_set_restricted_state`.
            drop(unsafe { kalloc::Box::from_raw(rs.cast::<RestrictedState>()) });
        }

        // Destroy `scheduler_queue_state` (`WavlTreeNode::drop` debug-asserts `!in_container()`).
        // SAFETY: `scheduler_queue_state` was initialized in place by `SchedulerQueueState::init()`
        // (matching size and alignment asserted above), and `Opaque` does not drop its contents
        // automatically, so this is its sole destruction.
        unsafe {
            ptr::drop_in_place(
                this.scheduler_queue_state.get().cast::<scheduler::SchedulerQueueState>(),
            );
        }

        // `owned_wait_queues` is a `DoublyLinkedList` of unmanaged pointers. Its `PinnedDrop`
        // impl debug-asserts if it is not empty when it destructs; we do not need to do so here.
        // Remaining fields (`canary`, `wait_queue_state`, `owned_wait_queues`, `task_state`, and
        // the intrusive list nodes) are dropped automatically after this method returns.
    }
}

impl Thread {
    /// The sampling session value of a thread not associated with any session.
    pub const INVALID_SAMPLING_SESSION: u64 = u64::MAX;

    /// Returns an in-place initializer for an `Opaque<T>` whose valid default representation is
    /// all-zero bytes, avoiding materializing a stack temporary.
    ///
    /// # Safety
    ///
    /// All-zero bytes must be a valid initialized representation of the underlying type stored in
    /// `Opaque<T>`.
    #[inline]
    unsafe fn init_zeroed_opaque<T>() -> impl PinInit<Opaque<T>, core::convert::Infallible> {
        // SAFETY: `pin_init_from_closure` guarantees `slot` is non-null, aligned, and valid for
        // writes of `Opaque<T>`, and the caller guarantees all-zero bytes are valid for `T`.
        unsafe {
            pin_init::pin_init_from_closure(|slot: *mut Opaque<T>| {
                ptr::write_bytes(slot, 0, 1);
                Ok(())
            })
        }
    }

    /// Returns an in-place initializer for an `Opaque<S>` backed by a `PinInit<T, Infallible>`.
    ///
    /// # Safety
    ///
    /// `T` must have the exact size and alignment of `Opaque<S>`, and if `T` needs drop, the owner
    /// of the `Opaque<S>` must explicitly drop it in place.
    #[inline]
    unsafe fn init_opaque_pin_init<S, T>(
        init: impl PinInit<T, core::convert::Infallible>,
    ) -> impl PinInit<Opaque<S>, core::convert::Infallible> {
        const {
            assert!(size_of::<T>() == size_of::<Opaque<S>>());
            assert!(align_of::<T>() == align_of::<Opaque<S>>());
        }
        // SAFETY: `pin_init_from_closure` guarantees `slot` is non-null, aligned, and valid for
        // writes of `Opaque<S>`. `Opaque<S>` is `#[repr(transparent)]` over
        // `MaybeUninit<UnsafeCell<S>>` with the exact size and alignment of `T` (asserted above),
        // so `PinInit::__pinned_init` on `slot.cast::<T>()` initializes `slot` in place without
        // fabricating an intermediate `&'static mut MaybeUninit<T>`.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Opaque<S>| {
                let Ok(()) = init.__pinned_init(slot.cast::<T>());
                Ok(())
            })
        }
    }

    /// Returns an in-place initializer for `Thread::name`, zero-initializing the buffer and
    /// copying `name` (truncated to `ZX_MAX_NAME_LEN - 1` bytes with trailing NUL).
    #[inline]
    fn init_name(
        name: &[u8],
    ) -> impl PinInit<[AtomicU8; ZX_MAX_NAME_LEN], core::convert::Infallible> {
        // SAFETY: `pin_init_from_closure` guarantees `slot` is non-null, aligned, and valid for
        // writes of `[AtomicU8; ZX_MAX_NAME_LEN]`. Zero-filling establishes a valid initialized
        // `[AtomicU8; ZX_MAX_NAME_LEN]` so `&*slot` is sound before delegating to
        // `write_name_bytes`.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut [AtomicU8; ZX_MAX_NAME_LEN]| {
                ptr::write_bytes(slot, 0, 1);
                Self::write_name_bytes(&*slot, name);
                Ok(())
            })
        }
    }

    /// Initializes a `Thread` directly in place at `slot`.
    ///
    /// Outlined so every `Thread` construction path (`init`, `init_default`, `init_in_place`,
    /// `try_alloc`, `create_for_test`) shares a single initialization body and never materializes a
    /// ~2.6 KiB `Thread` on the kernel stack.
    ///
    /// When `init_lock_state` is `true`, registers the thread's `lock_state` via
    /// `system_init_thread_lock_state` when lockdep is enabled.
    ///
    /// Note that this overwrites every field in `slot` in place without running any prior field
    /// destructors (in particular, `stack` is overwritten with `KernelStack::default()`).
    ///
    /// # Safety
    ///
    /// `slot` must be non-null, aligned to `align_of::<Self>()`, valid for writes of `Self`, not
    /// concurrently accessed (except that on RISC-V secondary CPU bringup, the `tp` register may
    /// already point into `(*slot).arch` before `SecondaryCpuInitEarly` enters `init_raw`, which
    /// `PinInit::__pinned_init` preserves soundly by initializing via raw pointers without
    /// fabricating `&mut Self`), must not hold a `Thread` that is linked into any intrusive list
    /// or otherwise referenced elsewhere, and must remain pinned at its address for the lifetime
    /// of the constructed `Thread`. Any pre-existing owned resources in `slot` are overwritten
    /// without running destructors (leaking them if not already torn down).
    #[inline(never)]
    unsafe fn init_raw(slot: *mut Self, name: &[u8], init_lock_state: bool) {
        debug_assert!(!slot.is_null() && slot.is_aligned());
        #[cfg(not(with_lock_dep))]
        let _ = init_lock_state;

        let init = pin_init::pin_init!(Self {
            lock: ClassedChainLock::new(),
            canary: fbl::Canary::new(),
            flags: RelaxedAtomicU32::new(0),
            signals: RelaxedAtomicU32::new(0),
            // SAFETY: `SchedulerState` has the exact size and alignment of
            // `ThreadSchedulerStateStorage` (statically asserted above) and needs no drop.
            scheduler_state <- unsafe {
                Self::init_opaque_pin_init::<_, scheduler::SchedulerState>(
                    scheduler::SchedulerState::init(),
                )
            },
            // SAFETY: `SchedulerQueueState` has the exact size and alignment of
            // `ThreadSchedulerQueueStateStorage` (statically asserted above), and `Thread::drop`
            // drops it in place.
            scheduler_queue_state <- unsafe {
                Self::init_opaque_pin_init::<_, scheduler::SchedulerQueueState>(
                    scheduler::SchedulerQueueState::init(),
                )
            },
            wait_queue_state <- ThreadState::init(),
            owned_wait_queues <- KCell::pin_init(OwnedWaitQueueList::new()),
            task_state <- TaskState::init_default(),
            preemption_state: PreemptionState::new(),
            memory_allocation_state: MemoryAllocationState::new(),
            restricted_state: UnsafeCell::new(ptr::null_mut()),
            #[cfg(with_lock_dep)]
            // SAFETY: `Opaque<ThreadLockStateStorage>` is `#[repr(transparent)]` over
            // `MaybeUninit<UnsafeCell<ThreadLockStateStorage>>` with the exact size and alignment
            // of `ThreadLockState` (statically asserted above). `PinInit::__pinned_init`
            // initializes `slot` in place via raw pointers without fabricating `&'static mut`,
            // after which `&*lock_state_ptr` is valid for `system_init_thread_lock_state`, and
            // `Thread::drop` later drops it in place.
            lock_state <- unsafe {
                pin_init::pin_init_from_closure(
                    move |slot: *mut Opaque<layout::ThreadLockStateStorage>| {
                        let lock_state_ptr = slot.cast::<crate::lockdep_rs::ThreadLockState>();
                        let Ok(()) = crate::lockdep_rs::ThreadLockState::init()
                            .__pinned_init(lock_state_ptr);
                        if init_lock_state {
                            crate::lockdep_rs::system_init_thread_lock_state(&*lock_state_ptr);
                        }
                        Ok::<(), core::convert::Infallible>(())
                    },
                )
            },
            aspace: AtomicPtr::new(ptr::null_mut()),
            #[cfg(ktest)]
            rust_ktrace_override: AtomicPtr::new(ptr::null_mut()),
            extra_policy_exception_code: KCell::new(0),
            extra_policy_exception_data: KCell::new(0),
            can_own_wait_queues: KCell::new(true),
            user_thread: AtomicPtr::new(ptr::null_mut()),
            tid: RelaxedAtomicU64::new(crate::kernel::koid::generate()),
            pid: RelaxedAtomicU64::new(ZX_KOID_INVALID),
            // SAFETY: `arch_thread` is a POD struct whose valid default representation is
            // all-zero bytes.
            arch <- unsafe { Self::init_zeroed_opaque::<layout::ThreadArchStorage>() },
            // SAFETY: `Opaque<ThreadStackStorage>` is `#[repr(transparent)]` over
            // `MaybeUninit<UnsafeCell<ThreadStackStorage>>` with the exact size and alignment of
            // `KernelStack` (statically asserted above), and `Thread::drop` drops it in place.
            stack <- unsafe {
                pin_init::pin_init_from_closure(
                    |slot: *mut Opaque<layout::ThreadStackStorage>| {
                        slot.cast::<KernelStack>().write(KernelStack::default());
                        Ok::<(), core::convert::Infallible>(())
                    },
                )
            },
            recursive_object_deletion_list: UnsafeCell::new(ptr::null_mut()),
            name <- Self::init_name(name),
            linebuffer: UnsafeCell::new(Linebuffer::new()),
            lock_flow_id: RelaxedAtomicU64::new(0),
            user_state_saved: KCell::new(false),
            migrate_pending: KCell::new(false),
            migrate_fn: KCell::new(ManuallyDrop::new(CallbackSlot::none())),
            context_switch_fn: KCell::new(ManuallyDrop::new(CallbackSlot::none())),
            rseq_accessor: UnsafeCell::new(ManuallyDrop::new(crate::page_map::Accessor::default())),
            sampling_session: RelaxedAtomicU64::new(Self::INVALID_SAMPLING_SESSION),
            migrate_list_node: DoublyLinkedListNode::new(),
            thread_list_node: DoublyLinkedListNode::new(),
            unblock_list_node: DoublyLinkedListNode::new(),
            save_state_list_node: SinglyLinkedListNode::new(),
            memory_stall_state: UnsafeCell::new(ThreadStallState::IgnoredKernelOnly),
        });

        // SAFETY: Caller guarantees `slot` is non-null, aligned, writable, exclusively owned
        // (aside from RISC-V's early `tp` pointing into `(*slot).arch` without dereferencing it),
        // and pinned for the lifetime of the constructed `Thread`. Calling
        // `PinInit::__pinned_init` directly avoids fabricating `&'static mut MaybeUninit<Self>`.
        let Ok(()) = unsafe { init.__pinned_init(slot) };
    }

    /// Returns an in-place initializer for `Thread` delegating to [`Self::init_raw`].
    #[inline]
    fn init_impl(
        name: &[u8],
        init_lock_state: bool,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `pin_init_from_closure` guarantees `slot` is non-null, aligned, writable,
        // exclusively owned, and pinned; `init_raw` initializes every field of `Self` in place.
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                Self::init_raw(slot, name, init_lock_state);
                Ok(())
            })
        }
    }

    /// Returns an in-place initializer for a named `Thread` with lockdep thread state
    /// registration enabled.
    #[inline]
    pub fn init(name: &[u8]) -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_impl(name, true)
    }

    /// Returns an in-place initializer for a default `Thread` (empty name, without lockdep thread
    /// state registration).
    #[inline]
    pub fn init_default() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_impl(b"", false)
    }

    /// Constructs a named `Thread` directly in place in `storage`.
    ///
    /// Overwrites every field in `storage` in place without running any prior field destructors
    /// (in particular, `stack` is overwritten with `KernelStack::default()`).
    ///
    /// # Safety
    ///
    /// `storage` must be non-null, aligned to `align_of::<Self>()`, valid for writes of `Self`,
    /// not concurrently accessed (except that on RISC-V secondary CPU bringup, the `tp` register
    /// may already point into `(*storage).arch` before `SecondaryCpuInitEarly` calls
    /// `init_in_place`, which `init_raw` preserves via raw-pointer initialization without
    /// fabricating `&mut Self`), must not hold a `Thread` that is linked into any intrusive list
    /// or otherwise referenced elsewhere, and must remain pinned at its address for the lifetime
    /// of the constructed `Thread`. Any pre-existing owned resources in `storage` are overwritten
    /// without running destructors (leaking them if not already torn down).
    #[inline]
    pub unsafe fn init_in_place(storage: *mut MaybeUninit<Self>, name: &[u8]) {
        // SAFETY: Caller guarantees `storage` is non-null, aligned, writable, exclusively owned,
        // not linked into any intrusive list or referenced elsewhere, and stays pinned for the
        // lifetime of the constructed `Thread`.
        unsafe { Self::init_raw(storage.cast::<Self>(), name, true) };
    }

    /// Allocates and constructs a `Thread` in place on the kernel heap using fallible allocation.
    fn try_alloc_with(name: &[u8], init_lock_state: bool) -> Option<NonNull<Self>> {
        let mut storage = kalloc::Box::<Self>::try_new_uninit().ok()?;
        // SAFETY: `storage` is freshly heap-allocated, aligned, exclusively owned, uninitialized,
        // and stays pinned at its heap address until destroyed in place and freed.
        unsafe { Self::init_raw(storage.as_mut_ptr(), name, init_lock_state) };
        NonNull::new(kalloc::Box::into_raw(storage).cast::<Self>())
    }

    /// Allocates and constructs a named `Thread` in place on the kernel heap using fallible
    /// allocation.
    #[inline]
    fn try_alloc(name: &[u8]) -> Option<NonNull<Self>> {
        Self::try_alloc_with(name, true)
    }

    /// Runs [`Thread`]'s destructor in place without freeing the underlying storage.
    ///
    /// Outlined so [`Self::delete`] and [`free_thread_resources`] share a single teardown body.
    ///
    /// # Safety
    ///
    /// `thread` must be non-null, aligned, and point to a constructed `Thread` that is off all
    /// global lists, not scheduled, not in any wait queue, not referenced by anything else, and
    /// not yet destroyed.
    #[inline(never)]
    unsafe fn destroy_in_place(thread: *mut Self) {
        debug_assert!(!thread.is_null() && thread.is_aligned());
        // SAFETY: Caller guarantees `thread` points to a valid, exclusively owned, live `Thread`.
        unsafe { ptr::drop_in_place(thread) };
    }

    /// Returns a reference to the thread's `TaskState`.
    #[inline]
    pub(crate) fn task_state(&self) -> &TaskState {
        &self.task_state
    }

    /// Returns a reference to the thread's `PreemptionState`.
    #[inline]
    pub(crate) fn preemption_state(&self) -> &PreemptionState {
        &self.preemption_state
    }

    /// Allocates and constructs an empty `Thread` on the kernel heap without initializing its
    /// kernel stack or placing it in the global thread list.
    #[inline]
    pub fn create_empty() -> Option<NonNull<Self>> {
        Self::try_alloc(b"")
    }

    /// Initializes the kernel stack of `thread`.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid, live `Thread` instance whose stack has not yet been
    /// initialized, and nothing may concurrently access its `KernelStack` during this call.
    #[inline]
    pub unsafe fn stack_init(thread: *mut Self) -> Result<(), Status> {
        debug_assert!(!thread.is_null());
        // SAFETY: `thread` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `stack` field place without forming a reference to the whole `Thread`.
        let stack = unsafe { &raw const (*thread).stack };
        // SAFETY: `stack` holds the thread's `KernelStack`, constructed in place by
        // `Thread::init_raw`, and its size and alignment are asserted above. The caller guarantees
        // exclusive access to the uninitialized `KernelStack`.
        unsafe { (*(*stack).get().cast::<KernelStack>()).init() }
    }

    /// Deletes a heap-allocated `Thread` instance previously allocated with [`Self::create_empty`],
    /// [`create_etc`] (with a null `t` parameter), [`rust_thread_create_empty_named`], or
    /// `Self::create_for_test` (or does nothing if `thread` is null).
    ///
    /// # Safety
    ///
    /// `thread` must be either null or a pointer to a `Thread` allocated on the kernel heap by
    /// [`Self::create_empty`], [`create_etc`] (with a null `t` parameter),
    /// [`rust_thread_create_empty_named`], or `Self::create_for_test`, that is not on any list,
    /// not scheduled, not in any wait queue, not referenced by anything else, and not deleted yet.
    #[inline]
    pub unsafe fn delete(thread: *mut Self) {
        if thread.is_null() {
            core::hint::cold_path();
            return;
        }
        // SAFETY: The caller guarantees `thread` is an unreferenced heap-allocated `Thread`
        // created by `try_alloc_with` that has not been destroyed yet. Destroying in place via
        // `destroy_in_place` and freeing as `Box<MaybeUninit<Self>>` shares teardown with
        // `free_thread_resources` and avoids a second drop.
        unsafe {
            Self::destroy_in_place(thread);
            drop(kalloc::Box::from_raw(thread.cast::<MaybeUninit<Self>>()));
        }
    }

    /// Domain-specific conversion: returns raw pointer for [`Thread`].
    #[inline]
    pub fn as_raw(&self) -> *mut Self {
        core::ptr::from_ref(self).cast_mut()
    }

    /// Raw place of the [`ClassedChainLock`] embedded in `this`, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn lock_ptr(this: *const Self) -> *const ClassedChainLock<ThreadLockClass> {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `lock` field place without forming a reference to the whole `Thread`.
        unsafe { &raw const (*this).lock }
    }

    /// Raw place of the [`scheduler::SchedulerState`] embedded in `this`, without forming a
    /// `&Thread` (`Thread::scheduler_state()`).
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`]. The result is a projection only: which of
    /// the fields behind it the caller may read or write is governed by the split locking rules
    /// described on `scheduler_state` (the thread's lock, or the owning scheduler's
    /// `queue_lock_`).
    #[inline]
    pub unsafe fn scheduler_state_raw(this: *const Self) -> *mut scheduler::SchedulerState {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `scheduler_state` field place without forming a reference to the whole `Thread`.
        let storage = unsafe { &raw const (*this).scheduler_state };
        // SAFETY: `storage` is the live `Thread`'s `scheduler_state` field (see above); `get` only
        // yields the pointer to its interior.
        unsafe { (*storage).get().cast::<scheduler::SchedulerState>() }
    }

    /// Raw place of the [`scheduler::SchedulerQueueState`] embedded in `this`, without forming a
    /// `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`]. The queue state is guarded by the owning
    /// scheduler's `queue_lock_` (and, for its disposition, also by the thread's lock), which the
    /// caller must hold when dereferencing the result.
    #[inline]
    pub unsafe fn scheduler_queue_state_raw(
        this: *const Self,
    ) -> *mut scheduler::SchedulerQueueState {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `scheduler_queue_state` field place without forming a reference to the whole `Thread`.
        let storage = unsafe { &raw const (*this).scheduler_queue_state };
        // SAFETY: `storage` is the live `Thread`'s `scheduler_queue_state` field (see above);
        // `get` only yields the pointer to its interior.
        unsafe { (*storage).get().cast::<scheduler::SchedulerQueueState>() }
    }

    /// Asserts that `self`'s canary magic value is valid.
    #[inline]
    pub fn assert_canary(&self) {
        self.canary.assert();
    }

    /// Like [`Self::assert_canary`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn assert_canary_ptr(this: *const Self) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `canary` field place without forming a reference to the whole `Thread`.
        let canary = unsafe { &raw const (*this).canary };
        // SAFETY: `canary` is the live `Thread`'s canary field (see above).
        unsafe { (*canary).assert() };
    }

    /// Checks whether `this`'s canary magic value is valid, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must be aligned and point to readable memory of at least `size_of::<Thread>()` bytes
    /// (for example, a node linked in the global thread list).
    #[inline]
    unsafe fn canary_valid_ptr(this: *const Self) -> bool {
        // SAFETY: `this` is aligned and points to readable `Thread` storage per the caller, and
        // `fbl::Canary` is `#[repr(C)]` with a single `u32` `magic` field at offset 0.
        unsafe { (&raw const (*this).canary).cast::<u32>().read_volatile() == fbl::magic(b"thrd") }
    }

    /// Like [`Self::is_idle`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn is_idle_ptr(this: *const Self) -> bool {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `flags` field place without forming a reference to the whole `Thread`.
        let flags = unsafe { &raw const (*this).flags };
        // SAFETY: `flags` is the live `Thread`'s flags field (see above).
        unsafe { (*flags).load() & THREAD_FLAG_IDLE != 0 }
    }

    /// Like [`Self::preemption_state`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn preemption_state_ptr(this: *const Self) -> *const PreemptionState {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `preemption_state` field place without forming a reference to the whole `Thread`.
        unsafe { &raw const (*this).preemption_state }
    }

    /// Returns the koid of the associated `ThreadDispatcher` for user threads or an independent
    /// koid for kernel threads.
    #[inline]
    pub fn tid(&self) -> zx_koid_t {
        self.tid.load()
    }

    /// Like [`Self::tid`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(crate) unsafe fn tid_ptr(this: *const Self) -> zx_koid_t {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `tid` field place without forming a reference to the whole `Thread`.
        let tid = unsafe { &raw const (*this).tid };
        // SAFETY: `tid` is the live `Thread`'s `tid` field (see above).
        unsafe { (*tid).load() }
    }

    /// Returns the koid of the associated `ProcessDispatcher` for user threads or
    /// `ZX_KOID_INVALID` for kernel threads.
    #[inline]
    pub fn pid(&self) -> zx_koid_t {
        self.pid.load()
    }

    /// Like [`Self::pid`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(crate) unsafe fn pid_ptr(this: *const Self) -> zx_koid_t {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `pid` field place without forming a reference to the whole `Thread`.
        let pid = unsafe { &raw const (*this).pid };
        // SAFETY: `pid` is the live `Thread`'s `pid` field (see above).
        unsafe { (*pid).load() }
    }

    /// Checks whether the kill or suspend signal (excluding signals in `signal_mask`) has been
    /// raised. If kill has been raised, returns `Err(ZX_ERR_INTERNAL_INTR_KILLED)`. If suspend has
    /// been raised, returns `Err(ZX_ERR_INTERNAL_INTR_RETRY)`. Otherwise, returns `Ok(())`.
    #[inline]
    pub fn check_kill_or_suspend_signal_mask(&self, signal_mask: u32) -> Result<(), Status> {
        let current_signals = self.signals() & !signal_mask;
        if current_signals & THREAD_SIGNAL_KILL != 0 {
            core::hint::cold_path();
            return Err(ZX_ERR_INTERNAL_INTR_KILLED);
        }
        if current_signals & THREAD_SIGNAL_SUSPEND != 0 {
            core::hint::cold_path();
            return Err(ZX_ERR_INTERNAL_INTR_RETRY);
        }
        Ok(())
    }

    /// Checks whether the kill or suspend signal has been raised.
    ///
    /// Returns `Err(ZX_ERR_INTERNAL_INTR_KILLED)` if kill has been raised,
    /// `Err(ZX_ERR_INTERNAL_INTR_RETRY)` if suspend has been raised, and `Ok(())` otherwise.
    #[inline]
    pub fn check_kill_or_suspend_signal(&self) -> Result<(), Status> {
        self.check_kill_or_suspend_signal_mask(0)
    }

    // Accessors for specific flags bits.
    /// Whether the thread is detached.
    #[inline]
    pub fn detached(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_DETACHED) != 0
    }
    /// Sets or clears the detached flag.
    ///
    /// # Safety
    ///
    /// Callers must preserve the detached-vs-joinable lifecycle invariant: a joined or being-joined
    /// thread must not be marked detached, and a detached thread's exit path assumes it will not be
    /// joined.
    #[inline]
    unsafe fn set_detached(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_DETACHED);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_DETACHED);
        }
    }
    /// Whether the `Thread` struct is freed when the thread exits.
    #[inline]
    pub fn free_struct(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_FREE_STRUCT) != 0
    }
    /// Sets or clears the free-struct flag.
    ///
    /// # Safety
    ///
    /// Callers must ensure this matches how the `Thread` storage was allocated (`new` vs
    /// caller-owned storage) and is only changed before the thread's exit path inspects it.
    #[inline]
    pub(crate) unsafe fn set_free_struct(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_FREE_STRUCT);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_FREE_STRUCT);
        }
    }
    /// Whether this is an idle thread; read lock-free by the scheduler.
    #[inline]
    pub fn is_idle(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_IDLE) != 0
    }
    /// Sets or clears the idle flag.
    ///
    /// # Safety
    ///
    /// Callers must only set this on a CPU's dedicated per-CPU idle thread during initialization,
    /// as the scheduler relies on `is_idle()` to identify the idle thread.
    #[inline]
    unsafe fn set_idle(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_IDLE);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_IDLE);
        }
    }
    /// Whether this thread runs a VCPU.
    #[inline]
    pub fn vcpu(&self) -> bool {
        (self.flags.load() & THREAD_FLAG_VCPU) != 0
    }
    /// Sets or clears the VCPU flag.
    #[inline]
    pub(crate) fn set_vcpu(&self, value: bool) {
        if value {
            self.flags.fetch_or(THREAD_FLAG_VCPU);
        } else {
            self.flags.fetch_and(!THREAD_FLAG_VCPU);
        }
    }

    /// Whether a restricted kick is pending for the next attempt to enter restricted mode, without
    /// forming a `&Thread`.
    ///
    /// These should only be accessed from the current thread; lock-free `Relaxed` accesses to
    /// `THREAD_FLAG_RESTRICTED_KICK_PENDING` do not race because only the thread itself reads or
    /// updates this bit.
    ///
    /// # Safety
    ///
    /// `this` must point to the valid, live current [`Thread`].
    #[inline]
    unsafe fn restricted_kick_pending(this: *const Self) -> bool {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `flags` field place without forming a reference to the whole `Thread`.
        let flags = unsafe { &raw const (*this).flags };
        // SAFETY: `flags` is the live `Thread`'s `flags` field (see above).
        unsafe { ((*flags).load() & THREAD_FLAG_RESTRICTED_KICK_PENDING) != 0 }
    }
    /// Sets or clears the restricted kick pending flag, without forming a `&Thread`.
    ///
    /// These should only be accessed from the current thread; lock-free `Relaxed` RMWs on
    /// `THREAD_FLAG_RESTRICTED_KICK_PENDING` do not race because only the thread itself updates
    /// this bit.
    ///
    /// # Safety
    ///
    /// `this` must point to the valid, live current [`Thread`].
    #[inline]
    unsafe fn set_restricted_kick_pending(this: *const Self, value: bool) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `flags` field place without forming a reference to the whole `Thread`.
        let flags = unsafe { &raw const (*this).flags };
        // SAFETY: `flags` is the live `Thread`'s `flags` field (see above).
        unsafe {
            if value {
                (*flags).fetch_or(THREAD_FLAG_RESTRICTED_KICK_PENDING);
            } else {
                (*flags).fetch_and(!THREAD_FLAG_RESTRICTED_KICK_PENDING);
            }
        }
    }

    /// Access to the entire flags value, for diagnostics.
    #[inline]
    pub fn flags(&self) -> u32 {
        self.flags.load()
    }

    // TODO(https://fxbug.dev/42077109): Write down memory order requirements for accessing
    // signals.
    /// Returns the thread's pending signals.
    ///
    /// Uses `Relaxed` ordering: signal bitmask reads are polled opportunistically or re-checked
    /// under the thread's lock, and do not synchronize other memory accesses.
    #[inline]
    pub fn signals(&self) -> u32 {
        self.signals.load()
    }
    /// Raises `bits` in the thread's pending signals.
    #[inline]
    pub(crate) fn set_signals(&self, bits: u32) {
        self.signals.fetch_or(bits);
    }
    /// Clears `bits` from the thread's pending signals.
    #[inline]
    pub(crate) fn clear_signals(&self, bits: u32) {
        self.signals.fetch_and(!bits);
    }
    /// Like [`Self::signals`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(crate) unsafe fn signals_ptr(this: *const Self) -> u32 {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `signals` field place without forming a reference to the whole `Thread`.
        let signals = unsafe { &raw const (*this).signals };
        // SAFETY: `signals` is the live `Thread`'s `signals` field (see above).
        unsafe { (*signals).load() }
    }
    /// Like [`Self::set_signals`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    unsafe fn set_signals_ptr(this: *const Self, bits: u32) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `signals` field place without forming a reference to the whole `Thread`.
        let signals = unsafe { &raw const (*this).signals };
        // SAFETY: `signals` is the live `Thread`'s `signals` field (see above).
        unsafe { (*signals).fetch_or(bits) };
    }
    /// Like [`Self::clear_signals`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    unsafe fn clear_signals_ptr(this: *const Self, bits: u32) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `signals` field place without forming a reference to the whole `Thread`.
        let signals = unsafe { &raw const (*this).signals };
        // SAFETY: `signals` is the live `Thread`'s `signals` field (see above).
        unsafe { (*signals).fetch_and(!bits) };
    }

    /// Records a failed rseq abort-IP update out of line so LLVM does not hoist the kcounter
    /// offset calculation into the signal loop preheader.
    #[cold]
    #[inline(never)]
    fn record_rseq_bad_ip() {
        RSEQ_BAD_IP.add(1);
    }

    /// Checks whether `this` was interrupted inside a restartable sequence and restarts it if so.
    ///
    /// # Safety
    ///
    /// - Interrupts must be disabled (`ints_disabled()`).
    /// - `this` must point to the valid, live current [`Thread`].
    /// - `source` must be a non-[`None`](crate::arch_rs::GeneralRegsSource::None)
    ///   [`GeneralRegsSource`](crate::arch_rs::GeneralRegsSource) supported by the target
    ///   architecture, and `gregs` must point to a valid, properly aligned, mutable
    ///   general-registers frame matching `source` with exclusive access (no live Rust references
    ///   to `*gregs`).
    unsafe fn check_restartable_sequence(
        this: *mut Self,
        source: crate::arch_rs::GeneralRegsSource,
        gregs: *mut c_void,
    ) {
        debug_assert!(crate::arch_rs::ints_disabled());

        // SAFETY: `this` points to the current `Thread`, which has exclusive access to its own
        // `rseq_accessor` while running with interrupts disabled. `&raw const` projects the
        // `rseq_accessor` field place without forming a reference to the whole `Thread`.
        let rseq = unsafe {
            let rseq_cell = &raw const (*this).rseq_accessor;
            &*UnsafeCell::raw_get(rseq_cell)
        };

        // SAFETY: `rseq` is the current thread's `rseq_accessor`, and the caller guarantees
        // `source` and `gregs`.
        unsafe { Self::check_restartable_sequence_with_accessor(rseq, source, gregs) };
    }

    /// Helper for [`Self::check_restartable_sequence`] that accepts `rseq` as a shared reference
    /// parameter so LLVM can apply `noalias` across `barrier_after_copy_to` and avoid reloading
    /// `rseq.object` on every field access.
    ///
    /// # Safety
    ///
    /// Same preconditions on `source` and `gregs` as [`Self::check_restartable_sequence`], with
    /// interrupts disabled.
    #[inline]
    unsafe fn check_restartable_sequence_with_accessor(
        rseq: &crate::page_map::Accessor<'_, zx_rseq_t>,
        source: crate::arch_rs::GeneralRegsSource,
        gregs: *mut c_void,
    ) {
        // Check if the restartable sequence has been removed since the signal was generated.
        //
        // This situation can occur if the thread was processing a `zx_thread_set_rseq` syscall
        // that was going to clear its accessor when it was preempted. When the scheduler resumes
        // the thread, the `rseq_accessor` will be valid, and the scheduler will raise the signal
        // to check the restartable sequence. However, the `zx_thread_set_rseq` syscall will resume
        // and clear the accessor before the signal handler is called, so we need to check if the
        // accessor is still valid.
        if !rseq.is_valid() {
            return;
        }

        // Update the CPU ID in the restartable sequence structure.
        let cpu_id: cpu_num_t = crate::arch_rs::curr_cpu_num();
        crate::page_map::write_field!(*rseq, cpu_id, cpu_id);

        // SAFETY: The caller guarantees `gregs` points to a valid register frame matching
        // `source`.
        let ip: usize =
            unsafe { crate::arch_rs::get_instruction_pointer(source, gregs.cast_const()) };

        let start_ip: usize = crate::page_map::read_field!(*rseq, start_ip) as usize;
        let post_commit_offset: usize =
            crate::page_map::read_field!(*rseq, post_commit_offset) as usize;

        if ip.wrapping_sub(start_ip) >= post_commit_offset {
            // The restartable sequence has completed.
            return;
        }

        // The restartable sequence was interrupted. Restart it.
        core::hint::cold_path();
        let abort_ip: usize = crate::page_map::read_field!(*rseq, abort_ip) as usize;

        // SAFETY: The caller guarantees `gregs` points to a valid mutable register frame matching
        // `source` with exclusive access and a non-`None` `source`.
        let status =
            unsafe { crate::arch_rs::set_return_instruction_pointer(source, gregs, abort_ip) };
        if status.is_err() {
            core::hint::cold_path();
            // TODO(https://fxbug.dev/449980934): Decide what we should do if we cannot set the
            // instruction pointer. Perhaps raise a policy exception?
            Self::record_rseq_bad_ip();
        }
    }

    /// Copies the name buffer (always NUL-terminated) into `out`.
    #[inline]
    pub fn name_bytes(&self, out: &mut [u8; ZX_MAX_NAME_LEN]) {
        // Like the C++ lock-free readers (ktrace, `DumpLocked`), the copy may observe a
        // concurrent `set_name` of another thread renaming itself; byte-wise relaxed loads make
        // that a torn name rather than a data race.
        for i in 0..ZX_MAX_NAME_LEN {
            out[i] = self.name[i].load(Ordering::Relaxed);
        }
    }

    /// Like [`Self::name_bytes`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub(super) unsafe fn name_bytes_ptr(this: *const Self, out: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `name` field place without forming a reference to the whole `Thread`.
        let name = unsafe { &raw const (*this).name };
        for (i, byte) in out.iter_mut().enumerate() {
            // SAFETY: `name` is the live `Thread`'s `[AtomicU8; ZX_MAX_NAME_LEN]` field (see
            // above).
            *byte = unsafe { (*name)[i].load(Ordering::Relaxed) };
        }
    }

    /// Returns a snapshot of the thread's name.
    #[inline]
    pub fn name(&self) -> ThreadName {
        // SAFETY: `self` is a valid, live `Thread`.
        unsafe { Self::name_copy_ptr(core::ptr::from_ref(self)) }
    }

    /// Like [`Self::name`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn name_copy_ptr(this: *const Self) -> ThreadName {
        let mut res = ThreadName::empty();
        // SAFETY: `this` points to a valid, live `Thread` per the caller.
        unsafe { Self::name_bytes_ptr(this, &mut res.buf) };
        res.len = name_buf_as_bytes(&res.buf).len();
        res
    }

    /// Raw place of `this`'s [`crate::lockdep_rs::ThreadLockState`] (`Thread::lock_state`),
    /// without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[cfg(with_lock_dep)]
    #[inline]
    pub unsafe fn lock_state_ptr(this: *mut Self) -> *mut crate::lockdep_rs::ThreadLockState {
        // SAFETY: `this` points to a valid `Thread` per the caller, `&raw mut` projects the
        // `lock_state` field place without forming a reference to `Thread` or `lock_state`, and
        // `ThreadLockState` has the same size and alignment as `ThreadLockStateStorage`.
        unsafe { (&raw mut (*this).lock_state).cast() }
    }

    /// Returns a pointer to `this`'s [`Linebuffer`], without forming a `&Thread`.
    ///
    /// Uses wrapping byte arithmetic so callers (such as `lib/io` during panics or early
    /// boot) can compute the field's address from [`current_get`] and validate it before
    /// dereferencing it. Only code running on this thread when blocking is allowed may
    /// access the buffer.
    #[inline]
    pub fn linebuffer_ptr(this: *mut Self) -> *mut Linebuffer {
        this.wrapping_byte_add(offset_of!(Self, linebuffer)).cast::<Linebuffer>()
    }

    /// Raw slot of `this`'s `recursive_object_deletion_list`, without forming a `&Thread`.
    ///
    /// Only the owning thread may read or write the returned slot.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn recursive_object_deletion_list_ptr(this: *mut Self) -> *mut *mut c_void {
        // SAFETY: `this` points to a valid, live `Thread` per the caller, and `&raw const` projects
        // the `recursive_object_deletion_list` field place without forming a reference to the whole
        // `Thread`.
        unsafe { UnsafeCell::raw_get(&raw const (*this).recursive_object_deletion_list) }
    }

    /// Writes `name` into `dst`, truncated so that it (including a trailing NUL byte) fits in
    /// `ZX_MAX_NAME_LEN` bytes, and NUL-fills the remainder.
    #[inline(never)]
    fn write_name_bytes(dst: &[AtomicU8; ZX_MAX_NAME_LEN], name: &[u8]) {
        let len = core::cmp::min(name.len(), ZX_MAX_NAME_LEN - 1);
        for (slot, &byte) in dst[..len].iter().zip(&name[..len]) {
            slot.store(byte, Ordering::Relaxed);
        }
        for slot in &dst[len..] {
            slot.store(0, Ordering::Relaxed);
        }
    }

    /// Sets the thread's name to `name`.
    ///
    /// This may truncate `name`, so that it (including a trailing NUL byte) fits in
    /// `ZX_MAX_NAME_LEN` bytes. Writes to `name` are unsynchronized: concurrent `set_name` calls
    /// may interleave, and readers (`name_bytes`, C++ `name()`) tolerate a torn name.
    pub fn set_name(&self, name: &[u8]) {
        Self::write_name_bytes(&self.name, name);
    }

    /// Sets the thread's name without creating a `&Thread` reference (for raw-pointer callers such
    /// as [`ThreadPtr::set_name`] and [`current_become_idle`] that avoid forming a whole-struct
    /// `&Thread`).
    ///
    /// # Safety
    ///
    /// `thread` must point to allocated, suitably aligned `Thread` storage whose `name` bytes are
    /// initialized, with no concurrent non-atomic access to `name`.
    unsafe fn set_name_in_place(thread: *mut Self, name: &[u8]) {
        // SAFETY: `&raw const` projects the `name` place without forming a reference to the
        // surrounding `Thread`; the caller guarantees `name` is initialized and only accessed
        // atomically.
        let dst = unsafe { &raw const (*thread).name };
        // SAFETY: `dst` points to the initialized `[AtomicU8; ZX_MAX_NAME_LEN]` field (see above).
        Self::write_name_bytes(unsafe { &*dst }, name);
    }

    /// Raw read of the thread's scheduling state without its lock.
    ///
    /// # Safety
    ///
    /// The thread must currently be a member of a wait queue whose lock the caller holds: a queued
    /// thread's state is set under both locks when it enqueues and only changes again after it
    /// has been removed under the queue's lock, so the queue lock alone makes this read race-free.
    #[inline]
    pub(super) unsafe fn state_in_wait_queue(&self) -> ThreadStateKind {
        let p = self.scheduler_state.get().cast::<scheduler::SchedulerState>();
        // SAFETY: `p` points at the live storage of the thread's `scheduler::SchedulerState`;
        // only the `state` place is read (the caller excludes concurrent writers of it) and no
        // reference to the surrounding struct is formed.
        unsafe { (&raw const (*p).state).read() }
    }

    /// Returns the [`SchedulerState`] facade over this thread's `scheduler_state` member.
    ///
    /// # Safety
    ///
    /// The caller must either hold this thread's lock for the lifetime of the returned reference,
    /// or, if this thread is blocked in a wait queue, hold that queue's lock for the lifetime of
    /// the reference (as the WAVL tree hooks of a wait queue do), use the reference only with the
    /// `effective_profile_*` accessors, and not let it reach code that uses any other accessor (see
    /// [`SchedulerState`]'s safety invariant).
    #[inline]
    pub(super) unsafe fn scheduler_state_unchecked(&self) -> &SchedulerState {
        // SAFETY: `scheduler_state` is the storage of the thread's `SchedulerState`, which is
        // live for as long as `self` is. `SchedulerState` is a `#[repr(transparent)]`
        // `Opaque<scheduler::SchedulerState>` with the same size as that storage (asserted above),
        // so the reference stays in bounds and covers every field. The caller holds one of the
        // locks that `SchedulerState`'s safety invariant requires and restricts its use
        // accordingly.
        unsafe { &*self.scheduler_state.get().cast::<SchedulerState>() }
    }

    /// Returns the [`KernelStack`] in `this`'s `stack` member, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to suitably aligned `Thread`-sized storage whose `stack` field holds an
    /// initialized [`KernelStack`], and nothing may mutate that `KernelStack` while the returned
    /// reference is live.
    #[inline(always)]
    unsafe fn stack<'a>(this: *const Self) -> &'a KernelStack {
        debug_assert!(!this.is_null() && this.is_aligned());
        // SAFETY: `this` points to aligned `Thread`-sized storage per the caller, and `&raw const`
        // projects the `stack` field place without forming a reference to the whole `Thread`.
        let stack = unsafe { &raw const (*this).stack };
        // SAFETY: `Opaque` is `#[repr(transparent)]` over `UnsafeCell<ThreadStackStorage>`, and
        // `stack` holds an initialized `KernelStack` (with size and alignment asserted after the
        // struct definition). Casting `stack` directly avoids forming an intermediate `&Opaque`.
        // The caller ensures it is not mutated while the reference is live.
        unsafe { &*stack.cast::<KernelStack>() }
    }

    /// Allocates a default-constructed, unscheduled test-only [`Thread`] on the heap, returning
    /// null on allocation failure.
    #[cfg(ktest)]
    pub(super) fn create_for_test() -> *mut Self {
        Self::try_alloc_with(b"", false).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    /// Destroys a test-only [`Thread`] previously returned by [`Self::create_for_test`] (or does
    /// nothing if `ptr` is null).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is either null or was returned by
    /// [`Self::create_for_test`], has not already been destroyed, is not queued or scheduled
    /// anywhere, and has no live references to `*ptr` or any of its fields.
    #[cfg(ktest)]
    pub(super) unsafe fn destroy_for_test(ptr: *mut Self) {
        // SAFETY: The caller upholds the pointer validity, exclusivity, and single-free contract.
        unsafe { Self::delete(ptr) };
    }

    /// Raw pointer to the [`scheduler::SchedulerState`] storage of this thread for unit tests.
    ///
    /// Never dereference this pointer into a whole-struct `&scheduler::SchedulerState` or
    /// `&mut scheduler::SchedulerState` on a live/scheduled thread: `SchedulerState` has split
    /// lock guardianship (`Thread::lock` vs. `Scheduler::queue_lock_` vs. lock-free `curr_cpu` and
    /// `last_cpu` reads). Forming a whole-struct reference or mutating fields directly is only
    /// sound on an unscheduled, unqueued test-only `Thread` when no other references to its
    /// `SchedulerState` or any of its fields exist.
    #[cfg(ktest)]
    #[inline]
    pub(super) fn scheduler_state_ptr(&self) -> *mut scheduler::SchedulerState {
        self.scheduler_state.get().cast::<scheduler::SchedulerState>()
    }

    /// Returns a reference to the global thread list spinlock (`Thread::get_list_lock()`).
    #[inline]
    pub fn get_list_lock() -> &'static crate::ksync_kernel::KSpinlock<ThreadListLockClass> {
        ThreadListLockClass::get()
    }

    /// Dump the information of a single thread (`Thread::Dump`).
    ///
    /// If `full` is true then a multi-line verbose dump of much information is performed, otherwise
    /// a single short summary line is dumped.
    pub fn dump(&self, full: bool) {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            _guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::Dump")
        );
        // SAFETY: `self` is a valid `Thread` with its lock held by `_guard`.
        unsafe { dump_thread_raw(core::ptr::from_ref(self), None, full) };
    }

    /// Dump the information of a single thread without acquiring its lock during a panic
    /// (`Thread::DumpDuringPanic`).
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live, initialized [`Thread`] whose `user_thread`, if non-null,
    /// points to a live `ThreadDispatcher`, and this function must only be called during a kernel
    /// panic, where normal locking is unsafe or unavailable and reads are best effort.
    #[inline]
    pub unsafe fn dump_during_panic(this: *const Self, full: bool) {
        // SAFETY: Caller guarantees `this` is a valid, live, initialized `Thread` (with live
        // `user_thread` if non-null) and we are in a kernel panic.
        unsafe { dump_thread_raw(this, None, full) };
    }

    /// Dump debugging info about all threads (`Thread::DumpAllLocked`).
    ///
    /// # Safety
    ///
    /// [`init_early`] (specifically [`thread_lists_init`]) must have completed.
    unsafe fn dump_all_locked(
        match_owner: Option<&CStr>,
        full: bool,
        _list_token: &LockToken<'_, ThreadListLockClass>,
    ) {
        let visitor = |t: *const Thread| {
            // SAFETY: `_list_token` keeps `t` live on `thread_list`, and `for_each_thread_raw`
            // validated `t`'s alignment and canary before invoking this closure.
            let thread = unsafe { &*t };
            {
                crate::kconcurrent::single_chain_lock_guard_owned!(
                    _guard,
                    thread,
                    IRQ_SAVE_OPTION,
                    clt_tag!("Thread::DumpAllLocked")
                );
                // SAFETY: `t` is a valid, live `Thread` in `thread_list` with its lock held by
                // `_guard`.
                unsafe { dump_thread_raw(t, match_owner, full) };
            }
            core::ops::ControlFlow::Continue(())
        };
        // SAFETY: Caller guarantees `init_early()` (`thread_lists_init()`) has completed, and
        // `_list_token` proves `Thread::get_list_lock()` is held, keeping `thread_list` and every
        // `Thread` node on it live and unmodified for the duration of the walk.
        unsafe { for_each_thread_raw(visitor) };
    }

    /// Finds all threads whose [`Self::owner_name`] is the same as `match_owner`, or just all
    /// threads if `match_owner` is `None`, and calls [`Self::dump`] on them (`Thread::DumpAll`).
    ///
    /// # Safety
    ///
    /// [`init_early`] must have completed.
    pub unsafe fn dump_all(match_owner: Option<&CStr>, full: bool) {
        ksync::lock!(let list_guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>());
        // SAFETY: Caller guarantees `init_early()` has completed.
        unsafe { Self::dump_all_locked(match_owner, full, list_guard.token()) };
    }

    /// Dumps all threads matching `match_owner` (or all threads if `None`) without acquiring
    /// `Thread::get_list_lock()` or per-thread locks during a panic (`Thread::DumpAllDuringPanic`).
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - Because this method bypasses `Thread::get_list_lock()` and per-thread chain locks, it must
    ///   only be called during a kernel panic when normal locking is unsafe or unavailable and
    ///   reads are best effort.
    pub unsafe fn dump_all_during_panic(match_owner: Option<&CStr>, full: bool) {
        // Skip grabbing the list lock and per-thread locks if we are panic'ing.
        let visitor = |t: *const Thread| {
            // SAFETY: `t` is a `Thread` node in `THREAD_LIST` whose canary passed, and caller
            // guarantees panic context where unlocked best-effort reads are permitted.
            unsafe { dump_thread_raw(t, match_owner, full) };
            core::ops::ControlFlow::Continue(())
        };
        // SAFETY: Caller guarantees `thread_lists_init()` has completed and we are in a kernel
        // panic where list and thread locking are bypassed and traversal/dump reads are best
        // effort.
        unsafe { for_each_thread_raw(visitor) };
    }

    /// Finds a thread whose `tid()` matches the provided `tid` and calls `dump` on it
    /// (`Thread::DumpTid`).
    ///
    /// # Safety
    ///
    /// [`init_early`] must have completed.
    pub unsafe fn dump_tid(tid: zx_koid_t, full: bool) {
        ksync::lock!(let list_guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>());
        // SAFETY: Caller guarantees `init_early()` has completed.
        unsafe { Self::dump_tid_locked(tid, full, list_guard.token()) };
    }

    /// Finds a thread whose `tid()` matches `tid` while holding `Thread::get_list_lock()` and dumps
    /// it (`Thread::DumpTidLocked`).
    ///
    /// # Safety
    ///
    /// [`init_early`] (specifically [`thread_lists_init`]) must have completed.
    unsafe fn dump_tid_locked(
        tid: zx_koid_t,
        full: bool,
        _list_token: &LockToken<'_, ThreadListLockClass>,
    ) {
        let visitor = |t: *const Thread| {
            // SAFETY: `_list_token` keeps `t` live on `thread_list`, and `for_each_thread_raw`
            // validated `t`'s alignment and canary before invoking this closure.
            if unsafe { Thread::tid_ptr(t) } != tid {
                return core::ops::ControlFlow::Continue(());
            }

            // TODO(https://fxbug.dev/584127903): Is it ok to stop searching now? In theory, no
            // two threads should share a TID.
            // SAFETY: `_list_token` keeps `t` live on `thread_list` and its canary passed.
            unsafe { &*t }.dump(full);
            core::ops::ControlFlow::Break(())
        };
        // SAFETY: Caller guarantees `init_early()` (`thread_lists_init()`) has completed, and
        // `_list_token` proves `Thread::get_list_lock()` is held, keeping `thread_list` and every
        // `Thread` node on it live and unmodified for the duration of the walk.
        unsafe { for_each_thread_raw(visitor) };
    }

    /// Dumps the thread with `tid` without acquiring `Thread::get_list_lock()` or the thread's lock
    /// during a panic (`Thread::DumpTidDuringPanic`).
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - Because this method bypasses `Thread::get_list_lock()` and per-thread chain locks, it must
    ///   only be called during a kernel panic when normal locking is unsafe or unavailable and
    ///   reads are best effort.
    pub unsafe fn dump_tid_during_panic(tid: zx_koid_t, full: bool) {
        // Skip grabbing the list lock and per-thread locks if we are panic'ing.
        let visitor = |t: *const Thread| {
            // SAFETY: `t` is a `Thread` node in `THREAD_LIST` whose canary passed, and caller
            // guarantees panic context.
            if unsafe { Thread::tid_ptr(t) } != tid {
                return core::ops::ControlFlow::Continue(());
            }
            // SAFETY: `t` is a `Thread` node in `THREAD_LIST` whose canary passed, and caller
            // guarantees panic context where unlocked best-effort reads are permitted.
            unsafe { dump_thread_raw(t, None, full) };
            core::ops::ControlFlow::Break(())
        };
        // SAFETY: Caller guarantees `thread_lists_init()` has completed and we are in a kernel
        // panic where list and thread locking are bypassed and traversal/dump reads are best
        // effort.
        unsafe { for_each_thread_raw(visitor) };
    }

    /// Return the name of the "owner" of the thread (`Thread::OwnerName`).
    ///
    /// Returns "kernel" if there is no owner.
    #[inline]
    pub fn owner_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `self` is a valid, live `Thread`.
        unsafe { Self::owner_name_ptr(core::ptr::from_ref(self), out_name) };
    }

    /// Like [`Self::owner_name`], for a raw pointer, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    unsafe fn owner_name_ptr(this: *const Self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `this` points to a valid `Thread` per the caller, and projecting the
        // `user_thread` field place avoids forming a reference to the whole `Thread`. `Acquire`
        // ordering synchronizes with `set_usermode_thread`'s `Release` store when called from
        // cross-thread `thread_list` readers.
        let ut = unsafe { (*this).user_thread.load(Ordering::Acquire) };
        if !ut.is_null() {
            // SAFETY: `ut` holds a strong `RefPtr<ThreadDispatcher>` for the lifetime of `this`.
            let status = unsafe { (*ut).process().get_name(out_name) };
            debug_assert!(status.is_ok());
            return;
        }
        out_name[..7].copy_from_slice(b"kernel\0");
    }

    /// Generate a backtrace for `self` (`Thread::GetBacktrace`).
    ///
    /// `self` must be blocked, sleeping or suspended (i.e. not running).
    ///
    /// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot be
    /// obtained, it will be left empty.
    pub fn get_backtrace(&self, out_bt: &mut Backtrace) {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::GetBacktrace")
        );

        // Get the starting point if it's in a usable state.
        let fp = match guard.proof().state() {
            ThreadStateKind::Blocked
            | ThreadStateKind::BlockedReadLock
            | ThreadStateKind::Sleeping
            | ThreadStateKind::Suspended => {
                // Thread is blocked, so ask the arch code to get us a starting point.
                // SAFETY: `self` is a valid, non-running thread in a blocked/sleeping/suspended
                // state with its lock held by `guard`.
                unsafe { crate::arch_rs::ops::thread_get_blocked_fp(self) }
            }
            _ => {
                // Not in a valid state, can't get a backtrace.  Reset it so the caller
                // doesn't inadvertently use a previous value.
                out_bt.reset();
                return;
            }
        };

        // SAFETY: `self` is a valid, non-running thread with its lock held by `guard`, so its
        // stack is not concurrently mutated.
        unsafe { get_backtrace_common(core::ptr::from_ref(self), fp, out_bt) };
    }

    /// Changes the base profile of the thread to `profile`, propagating any priority inheritance
    /// side effects (`Thread::SetBaseProfile`).
    #[inline]
    pub fn set_base_profile(&self, profile: &scheduler::BaseProfile) {
        self.assert_canary();
        OwnedWaitQueue::set_thread_base_profile_and_propagate(self, profile);
    }

    /// Acquires the thread's lock and returns a snapshot of its base profile
    /// (`Thread::SnapshotBaseProfile`).
    pub fn snapshot_base_profile(&self) -> scheduler::BaseProfile {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            crate::kconcurrent::IRQ_SAVE_OPTION,
            crate::kconcurrent::clt_tag!("Thread::SnapshotBaseProfile")
        );
        *guard.proof().base_profile()
    }

    /// Acquires the thread's lock and returns a snapshot of its effective profile
    /// (`Thread::SnapshotEffectiveProfile`).
    pub fn snapshot_effective_profile(&self) -> scheduler::EffectiveProfile {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            crate::kconcurrent::IRQ_SAVE_OPTION,
            crate::kconcurrent::clt_tag!("Thread::SnapshotEffectiveProfile")
        );
        *guard.proof().effective_profile()
    }

    /// Returns the hard CPU affinity mask of this thread (`Thread::GetCpuAffinity`).
    ///
    /// Hard affinity will always be respected by the scheduler; the scheduler will panic if it
    /// cannot satisfy this affinity.
    pub fn get_cpu_affinity(&self) -> cpu_mask_t {
        self.assert_canary();
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::GetCpuAffinity")
        );
        guard.proof().hard_affinity()
    }

    /// Returns the soft CPU affinity mask of this thread (`Thread::GetSoftCpuAffinity`).
    ///
    /// Soft affinity indicates where the thread should ideally be scheduled. The scheduler will
    /// respect the mask unless there are no other options (e.g., the soft affinity and hard
    /// affinity don't contain any common CPUs). If the two masks conflict, the hard affinity wins.
    pub fn get_soft_cpu_affinity(&self) -> cpu_mask_t {
        self.assert_canary();
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::GetSoftCpuAffinity")
        );
        guard.proof().soft_affinity()
    }

    /// Returns the number of nanoseconds a thread has been running for (`Thread::Runtime`).
    ///
    /// This takes the thread's lock to ensure there are no races while calculating
    /// the runtime of the thread.
    pub fn runtime(&self) -> DurationMono {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::Runtime")
        );
        guard.proof().runtime()
    }

    /// Returns the last CPU the given thread was run on, or `INVALID_CPU` if the
    /// thread has never run (`Thread::LastCpu`).
    pub fn last_cpu(&self) -> cpu_num_t {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::LastCpu")
        );
        guard.proof().last_cpu()
    }

    /// Returns both the thread's total runtime and the last CPU it was run on under a single
    /// acquisition of the thread's lock.
    pub fn runtime_and_last_cpu(&self) -> (DurationMono, cpu_num_t) {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::RuntimeAndLastCpu")
        );
        let proof = guard.proof();
        (proof.runtime(), proof.last_cpu())
    }

    /// Set the pointer to the user-mode thread, this will receive callbacks:
    /// `ThreadDispatcher::Exiting()`
    /// `ThreadDispatcher::Suspending()` / `Resuming()`
    ///
    /// This also caches the associated koids of the thread and process
    /// dispatchers associated with the given `ThreadDispatcher` (`Thread::SetUsermodeThread`).
    ///
    /// # Safety
    ///
    /// - `self` must be in the [`ThreadStateKind::Initial`] state with no `user_thread` attached
    ///   yet, and must never be joined (as ownership of thread termination and cleanup is
    ///   transferred to `ThreadDispatcher`).
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    pub unsafe fn set_usermode_thread(
        &self,
        user_thread: fbl::RefPtr<crate::object::ThreadDispatcher>,
    ) {
        self.assert_canary();

        crate::kconcurrent::single_chain_lock_guard_owned!(
            thread_guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::SetUsermodeThread")
        );

        debug_assert!(thread_guard.proof().state() == ThreadStateKind::Initial);
        debug_assert!(self.user_thread().is_null());

        self.tid.store(user_thread.get_koid());
        self.pid.store(user_thread.process().get_koid());
        // Ownership of the `RefPtr` reference is transferred to `self.user_thread` (released by
        // `Thread::drop`).
        self.user_thread.store(fbl::RefPtr::into_raw(user_thread).cast_mut(), Ordering::Release);

        // All user mode threads are detached since they are responsible for cleaning themselves up.
        // We can set this directly because we've checked that we are in the initial state.
        // SAFETY: By caller's `# Safety` contract, `self` is in `ThreadStateKind::Initial` (under
        // `thread_guard`) and is a usermode thread that cleans itself up on exit and is never
        // joined.
        unsafe { self.set_detached(true) };
    }

    /// Returns the associated `ThreadDispatcher`, or null if this is a kernel-only thread
    /// (`Thread::user_thread`).
    ///
    /// Uses `Acquire` ordering to synchronize with [`Self::set_usermode_thread`]'s `Release` store
    /// when called from cross-thread `thread_list` readers that hold only
    /// `Thread::get_list_lock()` (such as `ktrace_report_live_threads`) without acquiring
    /// `self.get_lock()`.
    #[inline]
    pub fn user_thread(&self) -> *mut crate::object::ThreadDispatcher {
        self.user_thread.load(Ordering::Acquire)
    }

    /// Like [`Self::user_thread`], for a raw pointer on self-thread or scheduler hot paths,
    /// without forming a `&Thread`.
    ///
    /// Uses `Relaxed` ordering: callers are either inspecting the currently running thread
    /// (`current_fault`) or an already-scheduled thread from the scheduler (`is_user_thread`,
    /// `update_runtime_stats_ptr`), for which [`Self::set_usermode_thread`] + `resume()` under
    /// `self.get_lock()` has already established a happens-before relationship. Cross-thread
    /// `thread_list` walkers that inspect threads which may still be in `Initial` must use
    /// [`Self::user_thread`] (which loads with `Acquire` ordering).
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn user_thread_ptr(this: *const Self) -> *mut crate::object::ThreadDispatcher {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `user_thread` field place without forming a reference to the whole `Thread`. `Relaxed`
        // ordering suffices because `set_usermode_thread` runs once under `self.get_lock()` while
        // in `Initial` before `resume()` (synchronizing with the thread's own execution and all
        // scheduler callers).
        let user_thread = unsafe { &raw const (*this).user_thread };
        // SAFETY: `user_thread` is the live `Thread`'s `user_thread` field (see above).
        unsafe { (*user_thread).load(Ordering::Relaxed) }
    }

    /// Called by the scheduler to update runtime stats for a raw pointer, without forming a
    /// `&Thread`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, live [`Thread`].
    /// - Interrupts must be disabled on the current CPU (`ThreadDispatcher::update_runtime_stats`
    ///   asserts `ints_disabled()` and acquires its spinlock with `NO_IRQ_SAVE`).
    #[inline]
    pub unsafe fn update_runtime_stats_ptr(this: *const Self, new_state: ThreadStateKind) {
        // SAFETY: `this` points to a valid, live `Thread` per the caller.
        let user_thread = unsafe { Self::user_thread_ptr(this) };
        if !user_thread.is_null() {
            // SAFETY: `this`'s `user_thread` holds a strong `RefPtr<ThreadDispatcher>` for the
            // lifetime of `this`, so `user_thread` is valid while `this` is live, and the caller
            // guarantees interrupts are disabled on the current CPU.
            unsafe { (*user_thread).update_runtime_stats(new_state) };
        }
    }

    /// Returns a `RefPtr` to this thread's current address space as long as the thread has not
    /// entered the [`ThreadStateKind::Death`] state (`Thread::GetAspaceRef`).
    ///
    /// Rules for the `aspace` member:
    ///
    /// 1) Raw `VmAspace` pointers held by a `Thread` object are guaranteed to always be "alive"
    ///    (the object has not been destructed, and the memory is still alive). The actual
    ///    `VmAspace` object is a ref-counted object which is guaranteed to be alive because:
    ///    - 1a) It is a member of a thread's process (the main aspace, as well as the restricted
    ///      mode aspace), and a thread's process object cannot be destroyed while the thread is
    ///      still alive. Or --
    ///    - 1b) It is a special statically allocated aspace object, such as the EFI address space,
    ///      or the kernel address space.
    /// 2) Only the current thread is permitted to mutate its own `aspace` member. The only
    ///    exception to this is during initialization, before the thread is running, by
    ///    `VmAspace::attach_to_thread`.
    /// 3) The currently running thread is allowed to access its own currently assigned aspace
    ///    without needing any locks.
    /// 4) Any other thread who wishes to access a thread's aspace must do so via either
    ///    [`Thread::get_aspace_ref`] or [`ChainLockProof::get_aspace_ref_locked`], which will
    ///    return a `RefPtr` to the thread's current aspace as long as the thread has not entered
    ///    the [`ThreadStateKind::Death`] state.
    /// 5) Other threads who access a thread's aspace via [`Thread::get_aspace_ref`] cannot assume
    ///    that either the thread or the aspace is still alive after the thread's lock is dropped.
    ///    At best, they know that the `RefPtr` is keeping the `VmAspace` object alive, nothing
    ///    more.
    /// 6) The scheduler and low level context switch routines are allowed to directly access the
    ///    `aspace` of both the old and new threads involved in the context switch. Neither one is
    ///    currently really running (the `aspace` members cannot change), and both threads have to
    ///    be alive enough that their process is still alive, and therefore so is their `aspace`
    ///    (see #1). Note; it is possible that the old thread involved in a context switch is in
    ///    the process of dying, but we know that its process cannot exit (destroying its aspace in
    ///    the process) until after it has context switched for the last time.
    #[inline]
    pub fn get_aspace_ref(&self) -> Option<fbl::RefPtr<VmAspace>> {
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            self,
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::GetAspaceRef")
        );
        guard.proof().get_aspace_ref_locked()
    }

    /// Returns the last flow id allocated by [`Self::take_next_lock_flow_id_ptr`] for `this`
    /// (`Thread::lock_flow_id`), without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    unsafe fn lock_flow_id_ptr(this: *const Self) -> u64 {
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `lock_flow_id` field place without forming a reference to the whole `Thread`.
        let lock_flow_id = unsafe { &raw const (*this).lock_flow_id };
        // SAFETY: `lock_flow_id` is the live `Thread`'s `lock_flow_id` field (see above).
        unsafe { (*lock_flow_id).load() }
    }

    /// Returns a unique flow id for lock contention tracing (`Thread::TakeNextLockFlowId`),
    /// without forming a `&Thread`.
    ///
    /// The same value is returned by [`Self::lock_flow_id_ptr`] until another id is allocated for
    /// this thread by calling this method again.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`].
    #[inline]
    unsafe fn take_next_lock_flow_id_ptr(this: *mut Self) -> u64 {
        let id = flow_id::generate();
        // SAFETY: `this` points to a valid `Thread` per the caller, and `&raw const` projects the
        // `lock_flow_id` field place without forming a reference to the whole `Thread`.
        let lock_flow_id = unsafe { &raw const (*this).lock_flow_id };
        // SAFETY: `lock_flow_id` is the live `Thread`'s `lock_flow_id` field (see above).
        unsafe { (*lock_flow_id).store(id) };
        id
    }

    /// Erase this thread from all global lists, where applicable.
    ///
    /// `list_token` proves that `Thread::get_list_lock()`, which guards both global lists and
    /// this thread's nodes in them, is held (`TA_REQ(get_list_lock())`).
    ///
    /// # Safety
    ///
    /// Must be called at most once on `self`, during final thread teardown (`join`, `forget`, or a
    /// detached thread's exit path), when `self` is still linked in `thread_list` and will not have
    /// `Thread::SetMigrateFn` called on it afterwards.
    unsafe fn erase_from_lists_locked(&self, list_token: &mut LockToken<'_, ThreadListLockClass>) {
        // Every constructed thread is on the global thread list until it is erased here.
        debug_assert!(self.thread_list_node.in_container());
        // SAFETY: `thread_lists_init()` completed during early boot (before any thread could
        // be inserted into `THREAD_LIST`), `thread_list_node` is only ever linked into
        // `THREAD_LIST`, and it is in a container (guaranteed by the caller), so `self` is a
        // member of that list.
        let erased = unsafe { thread_list(list_token).erase(self) };
        debug_assert!(erased.is_some());
        if self.migrate_list_node.in_container() {
            // SAFETY: `thread_lists_init()` completed during early boot, `migrate_list_node`
            // is only ever linked into `MIGRATE_LIST`, and it is in a container (checked above),
            // so `self` is a member of that list.
            let _ = unsafe { migrate_list(list_token).erase(self) };
        }
    }

    /// Dpc callback used for cleaning up a detached Thread's resources.
    ///
    /// called back in the DPC worker thread to free the stack and/or the thread structure
    /// itself for a thread that is exiting on its own.
    ///
    /// # Safety
    ///
    /// `dpc` must be the `Dpc` that [`current_exit`] queued for its detached thread (as copied and
    /// invoked by the DPC worker), whose `arg` is that exiting thread; nothing else may free that
    /// thread.
    unsafe fn free_dpc(dpc: &Dpc) {
        let t = dpc
            .arg::<Thread>()
            .expect("Thread::free_dpc is always queued with the exiting thread as its argument");

        {
            // SAFETY: `t` is the exiting thread, which stays allocated until
            // `free_thread_resources` below; this `&Thread` does not outlive this block.
            let thread = unsafe { t.as_ref() };
            thread.assert_canary();

            // grab and release the thread's lock, which effectively serializes us with
            // the thread that is queuing itself for destruction.  It ensures that:
            //
            // 1) The thread is no longer on any lists or referenced in any way.
            // 2) The thread is no longer assigned to any scheduler, and it has already
            //    entered the scheduler for the final time.
            crate::kconcurrent::single_chain_lock_guard_owned!(
                guard,
                thread,
                IRQ_SAVE_OPTION,
                clt_tag!("Thread::FreeDpc")
            );
            debug_assert!(guard.proof().state() == ThreadStateKind::Death);
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
        }

        // SAFETY: The thread is dead, off every list, and has entered the scheduler for the last
        // time (synchronized with by taking its lock above), so nothing else references it.
        unsafe { free_thread_resources(t.as_ptr()) };
    }

    /// Handles expiration of a thread sleep timer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `timer` is in an active timer callback context.
    /// - `this` points to the valid, live [`Thread`] associated with `timer`, which remains live
    ///   until either `timer.trylock_or_cancel` observes cancellation or [`ThreadState::unsleep`]
    ///   releases `this`'s lock.
    /// - No [`ChainLockTransaction`] is currently active on the current CPU.
    unsafe fn handle_sleep(this: ThreadPtr, timer: &Timer, _now: InstantMono) {
        let try_lock_or_cancel = |lock: &ChainLock| -> bool {
            // SAFETY: Called from `timer`'s timer callback within an active, unfinalized
            // `ChainLockTransaction` that does not yet hold `lock`.
            unsafe { timer.trylock_or_cancel(lock).is_ok() }
        };

        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            {
                // SAFETY: The caller guarantees `this` points to a valid, live `Thread` associated
                // with `timer`; the thread cannot exit until either `try_lock_or_cancel` sees the
                // timer was canceled or `ThreadState::unsleep` releases the thread's lock below.
                // `thread` and `thread_guard` are scoped to this block so no `&Thread` remains
                // live across `ThreadState::unsleep`.
                let thread: &Thread = unsafe { this.as_ref() };
                thread.assert_canary();
                // SAFETY: `try_lock_or_cancel` calls `Timer::trylock_or_cancel(lock)` and returns
                // `true` iff `lock` was acquired for the active transaction.
                let Some(mut thread_guard) =
                    (unsafe { OwnerGuard::try_acquire_with(thread, token, try_lock_or_cancel) })
                else {
                    core::hint::cold_path();
                    return DONE.into();
                };

                // We only need to be holding the thread's lock in order "unsleep" it. It is not
                // waiting in a wait queue, so there is no `WaitQueue` lock which needs to be held.
                ChainLockTransaction::finalize(token);

                if thread_guard.proof().state() != ThreadStateKind::Sleeping {
                    core::hint::cold_path();
                    thread_guard.release();
                    return DONE.into();
                }

                let _ = thread_guard.into_unguarded();
            }

            // Unblock the thread, regardless of whether the sleep was interruptible. Note that
            // calling `unsleep` will cause the scheduler to drop the thread's lock for us after it
            // has been successfully assigned to a scheduler instance.
            // SAFETY: `this` is a live, non-idle thread in `ThreadStateKind::Sleeping` (idle
            // threads never sleep), its lock is held by the active, finalized transaction `token`
            // with interrupts disabled by `IRQ_SAVE_OPTION`, no reference, guard, or proof for
            // `this` is live, and `this` is not dereferenced after this call.
            unsafe { ThreadState::unsleep(this, Ok(()), token) };
            DONE.into()
        };
        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("HandleSleep"), do_transaction);
    }

    /// Returns `true` if `this` has a migration callback registered (`Thread::has_migrate_fn`),
    /// without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`], and the caller must hold either
    /// `(*this).lock()` or the owning scheduler's `queue_lock_` (which [`Self::set_migrate_fn`]
    /// acquires when mutating `migrate_fn`).
    #[inline]
    pub unsafe fn has_migrate_fn_ptr(this: *const Self) -> bool {
        // SAFETY: `this` points to a valid `Thread` and the caller holds `(*this).lock()` or the
        // owning scheduler's `queue_lock_`. `KCell` is `#[repr(transparent)]` over
        // `UnsafeCell<CallbackSlot<MigrateFn>>`, whose `storage` holds an initialized
        // `Option<MigrateFn>` at offset 0.
        unsafe { (*(&raw const (*this).migrate_fn).cast::<Option<MigrateFn>>()).is_some() }
    }

    /// Returns `true` if a migration is currently in progress for `this`
    /// (`Thread::migrate_pending`), without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live [`Thread`], and the caller must hold `(*this).lock()`.
    #[inline]
    pub unsafe fn migrate_pending_ptr(this: *const Self) -> bool {
        // SAFETY: `this` points to a valid `Thread` and the caller holds `(*this).lock()`. `KCell`
        // is `#[repr(transparent)]` over `UnsafeCell<bool>`.
        unsafe { *(&raw const (*this).migrate_pending).cast::<bool>() }
    }

    /// Sets or clears the migration callback for this thread (`Thread::SetMigrateFn`).
    ///
    /// The migrate function will be invoked twice when a thread is migrated between CPUs: first
    /// when the thread is removed from the old CPU's scheduler, and second when the thread is
    /// rescheduled on the new CPU. When the migrate function is called, the thread's lock is held.
    /// Any previously registered callback is dropped after releasing the locks and restoring
    /// interrupts.
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - `self` must not have been erased via [`Self::erase_from_lists_locked`].
    /// - The caller must not hold `self`'s lock, `Thread::get_list_lock()`, any scheduler
    ///   `queue_lock_`, or have an active `ChainLockTransaction` on the current CPU
    ///   (`TA_EXCL(get_list_lock(), get_lock())`).
    pub unsafe fn set_migrate_fn(&self, migrate_fn: Option<MigrateFn>) {
        let old = {
            ksync::lock!(let mut list_guard = Self::get_list_lock().lock_policy::<IrqSavePolicy>());
            crate::kconcurrent::single_chain_lock_guard_owned!(
                thread_guard,
                self,
                NO_IRQ_SAVE_OPTION,
                clt_tag!("Thread::SetMigrateFn")
            );
            // SAFETY: Forwarded from the caller's safety preconditions.
            unsafe {
                thread_guard
                    .proof()
                    .set_migrate_fn_locked(migrate_fn, list_guard.as_mut().token_mut())
            }
        };
        drop(old);
    }

    /// Invokes the thread's migrate callback (if set) for `stage`, without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, live [`Thread`].
    /// - The caller must hold `(*this).lock()` exclusively (`TA_REQ(get_lock())`) with no live
    ///   `&mut` references to `(*this).migrate_fn` or `(*this).migrate_pending` or any reference
    ///   that the callback would alias.
    /// - Interrupts must be disabled, and the stage ordering invariants must hold (`Save` on
    ///   `last_cpu`, `Restore` on `curr_cpu` after `Save`, `Exiting` while exiting with
    ///   `Thread::get_list_lock()` held).
    /// - The registered migration callback must not access `(*this).migrate_fn` re-entrantly.
    #[inline]
    pub unsafe fn call_migrate_fn_locked_ptr(this: *mut Self, stage: MigrateStage) {
        // SAFETY: `this` points to a valid `Thread` and the caller holds `(*this).lock()`.
        if unsafe { Self::has_migrate_fn_ptr(this) } {
            // SAFETY: `this` is valid, the caller holds `(*this).lock()`, and `migrate_fn` is set.
            unsafe { Self::call_migrate_fn_locked_slow(this, stage) };
        }
    }

    /// Out-of-line slow path of [`Self::call_migrate_fn_locked_ptr`] when a migrate function is
    /// present.
    ///
    /// # Safety
    ///
    /// Same as [`Self::call_migrate_fn_locked_ptr`].
    #[cold]
    #[inline(never)]
    unsafe fn call_migrate_fn_locked_slow(this: *mut Self, stage: MigrateStage) {
        debug_assert!(!this.is_null() && this.is_aligned());
        // SAFETY: Caller holds `(*this).lock()` exclusively and `this` is a valid, aligned
        // `Thread` pointer. Forming `thread_ref` first and deriving `migrate_fn` through
        // `KCell::get_mut` (`UnsafeCell::get`) yields a child mutable borrow of `migrate_fn` that
        // remains valid while `thread_ref` is passed to `cb.call(thread_ref, stage)`.
        unsafe {
            let thread_ref: &Thread = &*this;
            let mut token = LockToken::<ThreadLockClass>::new();
            let migrate_pending = thread_ref.migrate_pending.get_mut(&mut token);
            let should_call = match stage {
                MigrateStage::Save => {
                    if !*migrate_pending {
                        // We are leaving our last CPU and calling our migration function as we
                        // go.  Assert that we are running on the proper CPU, and clear our last
                        // cpu bookkeeping to indicate that the migration has started.
                        let ss =
                            thread_ref.scheduler_state.get().cast::<scheduler::SchedulerState>();
                        let last_cpu = (*ss).last_cpu;
                        let curr_cpu = crate::arch_rs::curr_cpu_num();
                        debug_assert!(
                            last_cpu == curr_cpu,
                            "Attempting to run Before stage of migration on a CPU \
                             which is not the last CPU the thread ran on (last cpu = \
                             {}, curr cpu = {})\n",
                            last_cpu,
                            curr_cpu,
                        );
                        *migrate_pending = true;
                        true
                    } else {
                        false
                    }
                }
                MigrateStage::Restore => {
                    if *migrate_pending {
                        *migrate_pending = false;
                        true
                    } else {
                        false
                    }
                }
                MigrateStage::Exiting => true,
            };
            if should_call
                && let Some(cb) = thread_ref.migrate_fn.get_mut(&mut token).as_option_mut()
            {
                cb.call(thread_ref, stage);
            }
        }
    }

    /// Calls `migrate_fn` for each non-running thread that was last run on `cpu`
    /// (`Thread::CallMigrateFnForCpu`).
    ///
    /// Go over every non-running thread which has a migrate function assigned to it
    /// and last ran on `cpu`. `cpu` is becoming unavailable, and the currently
    /// running unplug thread is the last thing going to run on `cpu` before it goes
    /// offline. Any blocked threads or pinned READY threads remaining queued on
    /// `cpu` that need to record per-CPU state (e.g., VCPU threads) must run their
    /// Save stage now before `cpu` powers down.
    ///
    /// Note: no locks should be held during this operation, and interrupts need to
    /// be disabled.
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - Interrupts must be disabled (`ints_disabled()`).
    /// - The caller must not hold `Thread::get_list_lock()`, any thread's lock, or have an active
    ///   `ChainLockTransaction` on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_list_lock())`).
    /// - `cpu` must be the current CPU (`cpu == curr_cpu_num()`).
    /// - No reference that any registered migration callback would alias may be live in the caller.
    pub unsafe fn call_migrate_fn_for_cpu(cpu: cpu_num_t) {
        debug_assert!(crate::arch_rs::ints_disabled());
        debug_assert_eq!(cpu, crate::arch_rs::curr_cpu_num());
        ksync::lock!(let _list_guard = Self::get_list_lock().lock_policy::<NoIrqSavePolicy>());

        // Walk `MIGRATE_LIST` via raw `*mut Thread` pointers and lock only the `lock` field so no
        // whole-struct `&Thread` is live across `call_migrate_fn_locked_ptr`.
        // SAFETY: Caller guarantees `thread_lists_init()` has completed, and `_list_guard`
        // holds `Thread::get_list_lock()`. `KCell` and `SyncList` are `#[repr(transparent)]`, and
        // `MigrateList` is `#[repr(C)]` with `head: *mut Thread` as its only non-ZST field
        // (asserted via `size_of::<MigrateList>() == size_of::<*mut c_void>()`).
        let mut cur: *mut Thread = unsafe { MIGRATE_LIST.as_ptr().cast::<*mut Thread>().read() };
        while fbl::valid_sentinel_ptr(cur) {
            debug_assert!(cur.is_aligned());
            // SAFETY: Every `Thread` linked in `MIGRATE_LIST` is a live `Thread` aligned to
            // `align_of::<Thread>()`.
            unsafe { core::hint::assert_unchecked(cur.is_aligned()) };
            // SAFETY: `cur` is a live `Thread` linked in `MIGRATE_LIST` (kept live by
            // `_list_guard`), and `UnsafeCell<*mut Thread>` is `#[repr(transparent)]`.
            let next =
                unsafe { (&raw const (*cur).migrate_list_node.next).cast::<*mut Thread>().read() };
            // SAFETY: Project only the `lock` field without forming a whole-struct `&Thread`
            // across the migration callback.
            let lock: &ChainLock = unsafe { (*Thread::lock_ptr(cur)).as_chain_lock() };
            single_chain_lock_guard!(
                _thread_guard,
                NO_IRQ_SAVE_OPTION,
                lock,
                clt_tag!("Thread::CallMigrateFnForCpu")
            );
            // SAFETY: `_thread_guard` holds `(*cur).lock()` exclusively, guarding
            // `scheduler_state`'s `state` and `last_cpu` fields.
            let should_save = unsafe {
                let ss = (*cur).scheduler_state.get().cast::<scheduler::SchedulerState>();
                (*ss).state != ThreadStateKind::Running && (*ss).last_cpu == cpu
            };
            if should_save {
                // SAFETY: `cur` is a live non-running `Thread` that last ran on `cpu` (the current
                // CPU), interrupts are disabled, `_thread_guard` holds `(*cur).lock()` exclusively,
                // and no `&Thread` or conflicting reference is live across the callback.
                unsafe { Self::call_migrate_fn_locked_ptr(cur, MigrateStage::Save) };
            }
            cur = next;
        }
    }

    /// Sets or clears the context-switch callback for this thread (`Thread::SetContextSwitchFn`).
    ///
    /// The context switch function will be invoked when a thread is context switched to or away
    /// from. This will be called when a thread is about to be run on a CPU, after it's stopped
    /// from running on a CPU, or about to exit. Any previously registered callback is dropped
    /// after releasing the thread lock and restoring interrupts.
    pub fn set_context_switch_fn(&self, context_switch_fn: Option<ContextSwitchFn>) {
        self.assert_canary();
        let old = {
            crate::kconcurrent::single_chain_lock_guard_owned!(
                guard,
                self,
                IRQ_SAVE_OPTION,
                clt_tag!("Thread::SetContextSwitchFn")
            );
            guard.proof().set_context_switch_fn_locked(context_switch_fn)
        };
        drop(old);
    }

    /// Invokes `context_switch_fn` (if set) for `this` (`Thread::CallContextSwitchFnLocked`),
    /// without forming a `&Thread`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, live [`Thread`].
    /// - The caller must hold `(*this).lock()` (`TA_REQ(get_lock())`) with no live `&mut`
    ///   reference to `(*this).context_switch_fn` or any reference that the callback would alias.
    #[inline]
    pub unsafe fn call_context_switch_fn_locked_ptr(this: *mut Self) {
        // SAFETY: `this` points to a valid `Thread` and the caller holds `(*this).lock()`. `KCell`
        // is `#[repr(transparent)]` over `UnsafeCell<CallbackSlot<ContextSwitchFn>>`, whose
        // `storage` holds an initialized `Option<ContextSwitchFn>` at offset 0.
        unsafe {
            let slot = &mut *(&raw mut (*this).context_switch_fn).cast::<Option<ContextSwitchFn>>();
            if let Some(cb) = slot.as_mut() {
                core::hint::cold_path();
                cb.call();
            }
        }
    }
}

/// Returns a shared reference to the global thread list (`THREAD_LIST`).
///
/// `list_token` proves that [`Thread::get_list_lock`], which guards the list, is held; the
/// returned borrow is bounded by the shared borrow of the token.
///
/// # Safety
///
/// [`thread_lists_init`] must have completed.
#[inline]
unsafe fn thread_list_ref<'a>(
    list_token: &'a LockToken<'_, ThreadListLockClass>,
) -> &'a ThreadList {
    // SAFETY: Caller guarantees `thread_lists_init()` has completed, and access is
    // synchronized by `list_token`.
    unsafe { &THREAD_LIST.get_unchecked().get(list_token).list }
}

/// Walks the global thread list via raw pointers without forming `&ThreadList` or `&Thread`
/// before validating each node's canary.
///
/// If a node pointer is misaligned or outside kernel virtual address space, or fails its canary
/// check, diagnostic output is printed (with a hexdump for an aligned kernel pointer with bad
/// magic) and iteration stops. Otherwise `f` is invoked with the node pointer; returning
/// `ControlFlow::Break(())` stops iteration.
///
/// # Safety
///
/// - [`thread_lists_init`] must have completed.
/// - The caller must either hold `Thread::get_list_lock()` (preventing concurrent mutation or
///   deallocation of nodes in `THREAD_LIST`) or be in a kernel panic where normal list locking is
///   unavailable and traversal is best effort.
unsafe fn for_each_thread_raw(mut f: impl FnMut(*const Thread) -> core::ops::ControlFlow<()>) {
    // Read `head` directly through `THREAD_LIST.as_ptr()` so we never form a `&ThreadList` (which
    // could alias a live `Pin<&mut ThreadList>` if a panic struck mid-update).
    // SAFETY: Caller guarantees `thread_lists_init()` has completed. `KCell` and `SyncList`
    // are `#[repr(transparent)]`, and `ThreadList` is `#[repr(C)]` with `head: *mut Thread` as its
    // only non-ZST field (asserted via `size_of::<ThreadList>() == size_of::<*mut c_void>()`).
    let mut cur: *const Thread =
        unsafe { THREAD_LIST.as_ptr().cast_const().cast::<*mut Thread>().read().cast_const() };

    while fbl::valid_sentinel_ptr(cur) {
        if !cur.is_aligned() || !crate::arch_rs::vm::is_kernel_address(cur.addr()) {
            debug::dprintf!(INFO, "bad thread pointer {:p} in thread_list, aborting.\n", cur);
            break;
        }
        // SAFETY: `cur` is an aligned kernel address linked in `THREAD_LIST`; the caller holds
        // `Thread::get_list_lock()` or is in a kernel panic.
        if !unsafe { Thread::canary_valid_ptr(cur) } {
            debug::dprintf!(INFO, "bad magic on thread struct {:p}, aborting.\n", cur);
            // SAFETY: `cur` is an aligned kernel address from `THREAD_LIST`; reading
            // `size_of::<Thread>()` bytes is in-bounds for a `Thread` allocation (and best effort
            // during a kernel panic).
            unsafe {
                let _ = pretty::hexdump_very_ex_raw(
                    &mut debug::dprintf::KernelConsoleWriter,
                    cur.cast::<u8>(),
                    size_of::<Thread>(),
                    cur.addr() as u64,
                );
            }
            break;
        }
        // Read `next` before invoking `f`. `UnsafeCell<*mut Thread>` is `#[repr(transparent)]`,
        // so casting `&raw const (*cur).thread_list_node.next` avoids forming any reference.
        // SAFETY: `cur` is an aligned kernel `Thread` node in `THREAD_LIST` whose canary passed.
        let next = unsafe {
            (&raw const (*cur).thread_list_node.next).cast::<*mut Thread>().read().cast_const()
        };
        if f(cur).is_break() {
            break;
        }
        cur = next;
    }
}

/// Returns a pinned mutable reference to the global thread list (`THREAD_LIST`).
///
/// `list_token` proves that [`Thread::get_list_lock`], which guards the list, is held; the
/// returned borrow is bounded by the exclusive borrow of the token.
///
/// # Safety
///
/// [`thread_lists_init`] must have completed.
#[inline]
unsafe fn thread_list<'a>(
    list_token: &'a mut LockToken<'_, ThreadListLockClass>,
) -> Pin<&'a mut ThreadList> {
    // SAFETY: Caller guarantees `thread_lists_init()` has completed, and exclusive access is
    // synchronized by `list_token`.
    unsafe { THREAD_LIST.get_unchecked().get_pinned_mut_locked(list_token).project().list }
}

/// Returns a pinned mutable reference to the global list of threads with migrate functions
/// (`MIGRATE_LIST`).
///
/// `list_token` proves that [`Thread::get_list_lock`], which guards the list, is held; the
/// returned borrow is bounded by the exclusive borrow of the token.
///
/// # Safety
///
/// [`thread_lists_init`] must have completed.
#[inline]
unsafe fn migrate_list<'a>(
    list_token: &'a mut LockToken<'_, ThreadListLockClass>,
) -> Pin<&'a mut MigrateList> {
    // SAFETY: Caller guarantees `thread_lists_init()` has completed, and exclusive access is
    // synchronized by `list_token`.
    unsafe { MIGRATE_LIST.get_unchecked().get_pinned_mut_locked(list_token).project().list }
}

/// Releases the resources of a thread that is being torn down (`free_thread_resources`).
///
/// # Safety
///
/// `t` must point to a constructed `Thread` that is off the global lists, not scheduled, not in
/// any wait queue, and not referenced by anything else; it must not be used after this call.
unsafe fn free_thread_resources(t: *mut Thread) {
    // Free the thread structure itself unless marked otherwise.  If not freeing
    // it, manually trigger the Thread destructor so that DEBUG_ASSERTs present
    // in the owned_wait_queues member get triggered in all cases.
    OwnedWaitQueuePool::get().shrink();
    // SAFETY: `t` points to a constructed `Thread` that nothing else references (per the caller),
    // and the temporary `&Thread` ends with this statement.
    let free_struct = unsafe { (*t).free_struct() };
    // SAFETY: `t` is a constructed `Thread` off all lists and not referenced by anything else.
    unsafe { Thread::destroy_in_place(t) };
    if free_struct {
        // SAFETY: `THREAD_FLAG_FREE_STRUCT` is only set on heap-allocated `Thread`s (by
        // `create_etc` on a `Thread` allocated by `Thread::try_alloc`, or by `set_free_struct`,
        // whose contract requires the flag to match the allocation), and `destroy_in_place` just
        // ran its destructor so freeing as `Box<MaybeUninit<Thread>>` avoids a second drop.
        drop(unsafe { kalloc::Box::from_raw(t.cast::<MaybeUninit<Thread>>()) });
    }
}

/// Converts a raw scheduling state byte to its short display string (`thread_state_to_str`).
fn thread_state_to_str(raw_state: u8) -> &'static str {
    match ThreadStateKind::try_from(raw_state) {
        Ok(ThreadStateKind::Initial) => "init",
        Ok(ThreadStateKind::Suspended) => "susp",
        Ok(ThreadStateKind::Ready) => "rdy",
        Ok(ThreadStateKind::Running) => "run",
        Ok(ThreadStateKind::Blocked | ThreadStateKind::BlockedReadLock) => "blok",
        Ok(ThreadStateKind::Sleeping) => "slep",
        Ok(ThreadStateKind::Death) => "deth",
        Err(_) => "unkn",
    }
}

/// Emits the verbose multi-line thread dump (`full_dump == true` path of [`dump_thread_raw`]).
///
/// Outlined so the verbose multi-line dump stays off [`dump_thread_raw`]'s stack frame on the
/// summary path.
///
/// # Safety
///
/// Same requirements as [`dump_thread_raw`], and `ss` must point to `t`'s `SchedulerState`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn dump_thread_full_raw(
    t: *const Thread,
    ss: *const scheduler::SchedulerState,
    oname_bytes: &[u8],
    tname_bytes: &[u8],
    raw_state: u8,
    state: Result<ThreadStateKind, u8>,
    profile_bytes: &[u8],
    runtime: i64,
    owq_empty: bool,
    pid: zx_koid_t,
    tid: zx_koid_t,
) {
    // Dominating alignment checks fold repeated raw-pointer dereference UB checks in debug builds.
    debug_assert!(!t.is_null() && t.is_aligned());
    debug_assert!(!ss.is_null() && ss.is_aligned());

    if debug::dprintf::dprintf_enabled(debug::dprintf::INFO) {
        // SAFETY: `ss` points to `t`'s `SchedulerState` storage; fields are read by value through
        // the raw pointer.
        let (curr_cpu, last_cpu, hard_affinity, soft_affinity, remaining_time_slice) = unsafe {
            (
                (*ss).curr_cpu as i32,
                (*ss).last_cpu as i32,
                (*ss).hard_affinity,
                (*ss).soft_affinity,
                ((*ss).time_slice_ns - (*ss).time_slice_used_ns).raw_value(),
            )
        };
        kprint::kprint!("dump_thread: t {:p} ({:s}:{:s})\n", t, oname_bytes, tname_bytes);
        kprint::kprint!(
            "\tstate {:s}, curr/last cpu {}/{}, hard_affinity {:#x}, soft_cpu_affinity {:#x}, \
             {:s}, remaining time slice {}\n",
            thread_state_to_str(raw_state),
            curr_cpu,
            last_cpu,
            hard_affinity,
            soft_affinity,
            profile_bytes,
            remaining_time_slice,
        );
        kprint::kprint!(
            "\truntime_ns {}, runtime_s {}\n",
            runtime,
            runtime / DurationMono::from_seconds(1).into_nanos(),
        );
    }

    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
    unsafe { Thread::stack(t) }.dump_info(debug::dprintf::INFO);

    if debug::dprintf::dprintf_enabled(debug::dprintf::INFO) {
        // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller; `entry` is an
        // `Option<ThreadStartRoutine>` (guaranteed null-pointer optimized to pointer size) read as
        // a raw `*const c_void` for diagnostic printing.
        let (flags, entry, arg) = unsafe {
            (
                (*t).flags.load(),
                (&raw const (*t).task_state.entry).cast::<*const c_void>().read(),
                (*t).task_state.arg,
            )
        };
        // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
        let (blocking_wq, blocked_status, interruptible) =
            unsafe { ThreadState::dump_fields_raw(&raw const (*t).wait_queue_state) };
        // Read the raw `aspace` pointer directly (returning null if in `Death`, matching
        // `GetAspaceRefLocked().get()`) without bumping/dropping the `RefPtr` refcount during a
        // potentially unlocked panic dump.
        let aspace = if state == Ok(ThreadStateKind::Death) {
            core::ptr::null_mut()
        } else {
            // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
            unsafe { (*t).aspace.load(Ordering::Relaxed) }
        };
        // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
        let user_thread = unsafe { Thread::user_thread_ptr(t) };
        kprint::kprint!(
            "\tentry {:p}, arg {:p}, flags 0x{:x} {:s}{:s}{:s}{:s}\n",
            entry,
            arg,
            flags,
            if (flags & THREAD_FLAG_DETACHED) != 0 { "Dt" } else { "" },
            if (flags & THREAD_FLAG_FREE_STRUCT) != 0 { "Ft" } else { "" },
            if (flags & THREAD_FLAG_IDLE) != 0 { "Id" } else { "" },
            if (flags & THREAD_FLAG_VCPU) != 0 { "Vc" } else { "" },
        );
        kprint::kprint!(
            "\twait queue {:p}, blocked_status {}, interruptible {:s}, wait queues owned {:s}\n",
            blocking_wq,
            blocked_status,
            if interruptible { "yes" } else { "no" },
            if owq_empty { "no" } else { "yes" },
        );
        kprint::kprint!("\taspace {:p}\n", aspace);
        kprint::kprint!("\tuser_thread {:p}, pid {:u}, tid {:u}\n", user_thread, pid, tid);
    }

    // SAFETY: `t` points to a valid, live, initialized `Thread` and the caller either holds `t`'s
    // lock or is in a kernel panic.
    unsafe { crate::arch_rs::ops::dump_thread(t) };
}

/// Dump debugging info about the specified thread (`ThreadDumper::DumpLocked`).
///
/// Operates via raw-pointer field projections on `t` without forming a whole-struct `&Thread` or
/// `&SchedulerState`, as this function is called both with `t`'s lock held (`Thread::dump`,
/// `Thread::dump_all`, `Thread::dump_tid`) and unlocked during a panic
/// (`Thread::dump_during_panic`, `Thread::dump_all_during_panic`, `Thread::dump_tid_during_panic`).
///
/// # Safety
///
/// `t` must point to a valid, live, initialized [`Thread`] whose `user_thread`, if non-null,
/// points to a live `ThreadDispatcher`. The caller must either hold `t`'s lock or be in a kernel
/// panic, where the reads are best effort and may observe concurrent updates.
#[inline(never)]
unsafe fn dump_thread_raw(t: *const Thread, match_owner: Option<&CStr>, full_dump: bool) {
    // Dominating alignment check folds repeated raw-pointer dereference UB checks in debug builds.
    debug_assert!(!t.is_null() && t.is_aligned());

    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
    if !unsafe { Thread::canary_valid_ptr(t) }
        && debug::dprintf::dprintf_enabled(debug::dprintf::INFO)
    {
        kprint::kprint!("dump_thread WARNING: thread at {:p} has bad magic\n", t);
    }

    let mut oname = [0u8; ZX_MAX_NAME_LEN];
    // SAFETY: `t` points to a valid, live, initialized `Thread` whose `user_thread`, if non-null,
    // points to a live `ThreadDispatcher`.
    unsafe { Thread::owner_name_ptr(t, &mut oname) };
    let oname_bytes = name_buf_as_bytes(&oname);
    if let Some(match_owner) = match_owner
        && oname_bytes != match_owner.to_bytes()
    {
        return;
    }
    let mut tname = [0u8; ZX_MAX_NAME_LEN];
    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
    unsafe { Thread::name_bytes_ptr(t, &mut tname) };
    let tname_bytes = name_buf_as_bytes(&tname);

    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller; projecting
    // `scheduler_state` via raw pointer avoids forming a whole-struct `&SchedulerState`.
    let ss = unsafe { (*t).scheduler_state.get().cast::<scheduler::SchedulerState>() };
    debug_assert!(!ss.is_null() && ss.is_aligned());
    // SAFETY: Reading `state` as a raw `u8` avoids enum discriminant UB if `t` is corrupted during
    // a panic dump.
    let raw_state = unsafe { (&raw const (*ss).state).cast::<u8>().read() };
    let state = ThreadStateKind::try_from(raw_state);

    // SAFETY: `ss` points to `t`'s `SchedulerState` storage; copy the `Copy` value out before
    // calling `.raw_value()` so no reference into `*ss` is formed.
    let mut runtime = DurationMono(unsafe { (*ss).runtime_ns }.raw_value());
    if state == Ok(ThreadStateKind::Running) {
        // SAFETY: `ss` points to `t`'s `SchedulerState` storage; copy the `Copy` value out before
        // calling `.raw_value()` so no reference into `*ss` is formed.
        let recent =
            current_mono_time() - InstantMono(unsafe { (*ss).last_started_running }.raw_value());
        runtime = runtime + recent;
    }
    let runtime = runtime.0;

    // SAFETY: `ss` points to `t`'s `SchedulerState` storage; `EffectiveProfile` raw helpers read
    // individual fields without forming `&EffectiveProfile`.
    let ep = unsafe { &raw const (*ss).effective_profile };
    let mut profile_buf = [0u8; 64];
    // SAFETY: `ep` points to `t`'s `EffectiveProfile` storage.
    let profile_bytes = if unsafe { scheduler::EffectiveProfile::discipline_raw(ep) }
        == scheduler::SchedDiscipline::Fair
    {
        // SAFETY: `ep` points to `t`'s `EffectiveProfile` storage; copy the `Copy` value out
        // before calling `.raw_value()` so no reference into `*ep` is formed.
        let weight = unsafe { (*ep).weight }.raw_value();
        kprint::kformat!(&mut profile_buf, "Fair (w {})", weight)
    } else {
        // SAFETY: `ep` points to `t`'s `EffectiveProfile` storage.
        debug_assert!(
            unsafe { scheduler::EffectiveProfile::discipline_raw(ep) }
                == scheduler::SchedDiscipline::Deadline
        );
        // SAFETY: `ep` is a deadline `EffectiveProfile`.
        let dl = unsafe { scheduler::EffectiveProfile::deadline_raw(ep) };
        let capacity = dl.capacity_ns.raw_value();
        let deadline = dl.deadline_ns.raw_value();
        kprint::kformat!(&mut profile_buf, "Deadline (c,d = {},{})", capacity, deadline)
    };

    // Read `head` directly through the raw pointer so we never form an `&OwnedWaitQueueList`
    // (which could alias a live `Pin<&mut OwnedWaitQueueList>` if a panic struck mid-update).
    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller, `KCell` is
    // `#[repr(transparent)]` over `OwnedWaitQueueList`, and `OwnedWaitQueueList` is `#[repr(C)]`
    // with `head: *mut OwnedWaitQueue` as its only non-ZST field (asserted above).
    let owq_empty = fbl::is_sentinel_ptr(unsafe {
        (&raw const (*t).owned_wait_queues).cast::<*const OwnedWaitQueue>().read()
    });
    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
    let pid = unsafe { Thread::pid_ptr(t) };
    // SAFETY: `t` points to a valid, live, initialized `Thread` per the caller.
    let tid = unsafe { Thread::tid_ptr(t) };

    if full_dump {
        // SAFETY: Forwarded from caller; `ss` is `t`'s `SchedulerState`.
        unsafe {
            dump_thread_full_raw(
                t,
                ss,
                oname_bytes,
                tname_bytes,
                raw_state,
                state,
                profile_bytes,
                runtime,
                owq_empty,
                pid,
                tid,
            );
        }
    } else {
        kprint::kprint!(
            "thr {:p} st {:>4s} owq {} {:s} pid {:u} tid {:u} run_ns {} ({:s}:{:s})\n",
            t,
            thread_state_to_str(raw_state),
            u8::from(!owq_empty),
            profile_bytes,
            pid,
            tid,
            runtime,
            oname_bytes,
            tname_bytes,
        );
    }
}

/// Finds a thread based on the thread ID (`thread_id_to_thread_slow`).
///
/// NOTE: Used only for debugging; it's a slow linear search through the
/// global thread list.
///
/// Note: Any thread reference borrowed by this function is only guaranteed to be
/// alive as long as [`Thread::get_list_lock`] is held.
///
/// # Safety
///
/// [`init_early`] must have completed.
#[inline]
pub unsafe fn thread_id_to_thread_slow<'a>(
    tid: zx_koid_t,
    list_token: &'a LockToken<'_, ThreadListLockClass>,
) -> Option<&'a Thread> {
    // SAFETY: Caller guarantees `init_early()` (`thread_lists_init()`) has completed.
    unsafe { thread_list_ref(list_token) }.forward_iter().find(|t| t.tid() == tid)
}

/// Used by ktrace at the start of a trace to ensure that all
/// the running threads, processes, and their names are known.
///
/// # Safety
///
/// [`init_early`] must have completed.
pub unsafe fn ktrace_report_live_threads() {
    ksync::lock!(let guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>());
    // SAFETY: Caller guarantees `init_early()` (`thread_lists_init()`) has completed.
    for t in unsafe { thread_list_ref(guard.token()) }.forward_iter() {
        t.assert_canary();
        let ut = t.user_thread();
        let job_koid = if !ut.is_null() {
            // SAFETY: `t` holds a strong `RefPtr<ThreadDispatcher>` in `user_thread` for its
            // lifetime.
            unsafe { (*ut).process().job().get_koid() }
        } else {
            0
        };
        let name = t.name();
        crate::ktrace_rs::kernel_object_always!(
            t.tid(),
            ZX_OBJ_TYPE_THREAD,
            name.as_bytes(),
            "process" => crate::ktrace_rs::Koid(t.pid()),
            "job" => crate::ktrace_rs::Koid(job_koid)
        );
    }
}

/// Reads a `usize` word at `ptr` from a kernel stack with bounds
/// `[stack_base, stack_max + size_of::<usize>())` if `ptr` lies within `[stack_base, stack_max]`
/// (`ReadStack`).
///
/// # Safety
///
/// `[stack_base, stack_max + size_of::<usize>())` must be the live kernel stack range of a
/// [`Thread`] whose stack is not concurrently being torn down or mutated at
/// `[ptr, ptr + size_of::<usize>())` (e.g., the calling thread or a locked non-running thread).
// TODO(https://fxbug.dev/584127903): Consider moving this method to the KernelStack class. That's
// probably a better home for it.
#[inline]
unsafe fn read_stack(stack_base: usize, stack_max: usize, ptr: usize) -> Option<usize> {
    if !crate::arch_rs::vm::is_kernel_address(ptr) || ptr < stack_base || ptr > stack_max {
        return None;
    }
    // SAFETY: `[ptr, ptr + size_of::<usize>())` was just verified to be in-bounds of the live
    // kernel stack; `read_unaligned` does not require `ptr` to be aligned to `usize`.
    Some(unsafe { core::ptr::with_exposed_provenance::<usize>(ptr).read_unaligned() })
}

/// Unwinds `thread`'s stack starting from `fp` and populates `out_bt` (`GetBacktraceCommon`).
///
/// # Safety
///
/// If `thread` is non-null and `fp != 0`, `thread` must point to a valid, live [`Thread`] whose
/// stack is not concurrently being torn down or mutated in the unwound frames (e.g., `thread` is
/// the calling thread or a locked non-running thread).
unsafe fn get_backtrace_common(thread: *const Thread, mut fp: usize, out_bt: &mut Backtrace) {
    // Be sure that all paths out of this function leave with `out_bt` either
    // properly filled in or empty.
    out_bt.reset();

    // Without frame pointers, don't even try.  The compiler should optimize out
    // the body of all the callers if it's not present.
    if !crate::arch_rs::with_frame_pointers() {
        return;
    }

    // Perhaps we don't yet have a thread context?
    if thread.is_null() {
        return;
    }

    if fp == 0 {
        return;
    }

    // SAFETY: `thread` is a valid, live `Thread` per the caller, and its `KernelStack` bounds
    // remain invariant across the unwind loop.
    let stack = unsafe { Thread::stack(thread) };
    let stack_base = stack.base();
    // Guard against zero or tiny stack bounds (e.g., CPU 0's bootstrap idle thread has an
    // unmapped `KernelStack` with base and top equal to 0).
    let Some(stack_max) = stack.top().checked_sub(size_of::<usize>()) else {
        return;
    };

    for _ in 0..Backtrace::MAX_SIZE {
        #[allow(unused_mut)]
        let mut actual_fp = fp;

        // RISC-V has a nonstandard frame pointer which points to the CFA instead of
        // the previous frame pointer. Since the frame pointer and return address are
        // always just below the CFA, subtract 16 bytes to get to the actual frame pointer.
        #[cfg(target_arch = "riscv64")]
        {
            actual_fp = actual_fp.wrapping_sub(16);
        }

        // SAFETY: `[stack_base, stack_max + size_of::<usize>())` is `thread`'s live kernel stack.
        let Some(pc) = (unsafe { read_stack(stack_base, stack_max, actual_fp.wrapping_add(8)) })
        else {
            break;
        };
        out_bt.push_back(pc);
        // SAFETY: `[stack_base, stack_max + size_of::<usize>())` is `thread`'s live kernel stack.
        let Some(next_fp) = (unsafe { read_stack(stack_base, stack_max, actual_fp) }) else {
            break;
        };
        fp = next_fp;
    }
}

/// Accessors and locked operations for a [`Thread`] guarded by its chain lock.
impl<'p> ChainLockProof<'p, Thread> {
    /// Returns a shared reference to this thread's [`SchedulerState`].
    #[inline]
    pub fn scheduler_state(&self) -> &SchedulerState {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `scheduler_state`.
        unsafe { self.owner().scheduler_state_unchecked() }
    }

    /// Raw pointer to the [`scheduler::SchedulerState`] storage of this thread.
    ///
    /// Never dereferenced into a reference to the whole `scheduler::SchedulerState`: some of its
    /// fields (`start_time`, `finish_time`, `time_slice_*`, `activation_count`, and
    /// `subtree_invariants`) are guarded by the owning scheduler's `queue_lock_` rather than by
    /// the thread's lock, and `curr_cpu`/`last_cpu` are read lock-free. Use the field-projecting
    /// accessors below to reach the fields guarded by the thread's lock.
    #[inline]
    fn scheduler_state_ptr(&self) -> *mut scheduler::SchedulerState {
        self.owner().scheduler_state.get().cast::<scheduler::SchedulerState>()
    }

    /// Raw place of the `state` member of this thread's [`scheduler::SchedulerState`].
    ///
    /// Only the single field is projected: a reference covering the whole struct would also
    /// cover members guarded by the scheduler's `queue_lock_` (`start_time`, `finish_time`,
    /// `activation_count`, `subtree_invariants`, the run-queue node) and members read lock-free
    /// (`curr_cpu`/`last_cpu`), none of which the thread lock alone may touch.
    #[inline]
    fn state_ptr(&self) -> *mut ThreadStateKind {
        let p = self.scheduler_state_ptr();
        // SAFETY: `p` points at the live storage of the thread's `scheduler::SchedulerState`;
        // `&raw mut` projects the field place without forming a reference to the surrounding
        // struct.
        unsafe { &raw mut (*p).state }
    }

    /// Returns the scheduling state of this thread.
    #[inline]
    pub fn state(&self) -> ThreadStateKind {
        // SAFETY: `self` proves the thread lock is held, which is the only lock `state` is
        // written under, and no reference to the surrounding struct is formed.
        unsafe { *self.state_ptr() }
    }

    /// Sets this thread's address space pointer to `aspace` and returns the previous one, without
    /// switching the hardware address space. Only the current thread may change its own address
    /// space pointer; the one exception is initialization, before the thread is running.
    ///
    /// # Safety
    ///
    /// This thread has not started running. Until `aspace` is replaced, or this thread is
    /// destroyed or has context-switched for the last time, `aspace` is statically allocated or
    /// a reference to it is held elsewhere (normally by this thread's process): other threads take
    /// new references to it through [`Self::get_aspace_ref_locked`].
    #[inline]
    pub unsafe fn switch_aspace(&mut self, aspace: NonNull<VmAspace>) -> Option<NonNull<VmAspace>> {
        debug_assert!(self.state() == ThreadStateKind::Initial);
        // `Relaxed` is enough: other threads read `aspace` only while holding the thread lock,
        // which `self` proves is held, and this thread reads it only after it starts running, which
        // takes that lock again.
        let old_aspace = self.owner().aspace.load(Ordering::Relaxed);
        self.owner().aspace.store(aspace.as_ptr(), Ordering::Relaxed);
        NonNull::new(old_aspace)
    }

    /// Sets the thread's state to `ThreadStateKind::Blocked`.
    #[inline]
    pub fn set_blocked(&mut self) {
        // SAFETY: As in `state`; the unique borrow of `self` excludes other writers under the
        // thread lock.
        unsafe { *self.state_ptr() = ThreadStateKind::Blocked };
    }

    /// Sets the thread's state to `ThreadStateKind::BlockedReadLock`.
    #[inline]
    pub fn set_blocked_read_lock(&mut self) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = ThreadStateKind::BlockedReadLock };
    }

    /// Sets the thread's state to `ThreadStateKind::Sleeping`.
    #[inline]
    pub fn set_sleeping(&mut self) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = ThreadStateKind::Sleeping };
    }

    /// Sets the thread's state to `ThreadStateKind::Suspended`.
    #[inline]
    fn set_suspended(&mut self) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = ThreadStateKind::Suspended };
    }

    /// Sets the thread's scheduling state (for unit tests).
    #[cfg(ktest)]
    #[inline]
    pub(crate) fn set_state(&mut self, state: ThreadStateKind) {
        // SAFETY: As in `set_blocked`.
        unsafe { *self.state_ptr() = state };
    }

    /// Returns this thread's base profile.
    #[inline]
    pub(crate) fn base_profile(&self) -> &scheduler::BaseProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held, which guards `base_profile`, and
        // `&(*p).base_profile` projects only the field place without forming a reference to the
        // whole `scheduler::SchedulerState`.
        unsafe { &(*p).base_profile }
    }

    /// Returns this thread's base profile mutably.
    #[inline]
    pub(crate) fn base_profile_mut(&mut self) -> &mut scheduler::BaseProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: As in `base_profile`; the unique borrow of `self` ensures no other reference to
        // `base_profile` is live.
        unsafe { &mut (*p).base_profile }
    }

    /// Returns this thread's inherited profile values.
    #[inline]
    pub(crate) fn inherited_profile_values(&self) -> &scheduler::InheritedProfileValues {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held, which guards
        // `inherited_profile_values`, and `&(*p).inherited_profile_values` projects only that
        // field.
        unsafe { &(*p).inherited_profile_values }
    }

    /// Returns this thread's inherited profile values mutably.
    #[inline]
    pub(crate) fn inherited_profile_values_mut(
        &mut self,
    ) -> &mut scheduler::InheritedProfileValues {
        let p = self.scheduler_state_ptr();
        // SAFETY: As in `inherited_profile_values`; the unique borrow of `self` ensures no other
        // reference to `inherited_profile_values` is live.
        unsafe { &mut (*p).inherited_profile_values }
    }

    /// Returns this thread's effective profile.
    #[inline]
    pub(crate) fn effective_profile(&self) -> &scheduler::EffectiveProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held, which guards `effective_profile`,
        // and `&(*p).effective_profile` projects only that field.
        unsafe { &(*p).effective_profile }
    }

    /// Returns this thread's effective profile mutably (for example, to mark it dirty).
    #[inline]
    pub(crate) fn effective_profile_mut(&mut self) -> &mut scheduler::EffectiveProfile {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively, which guards writes to
        // `effective_profile`, `&mut (*p).effective_profile` projects only that field, and the
        // unique borrow of `self` ensures no other reference to it is live.
        unsafe { &mut (*p).effective_profile }
    }

    /// Recomputes this thread's effective profile from its `base_profile` and
    /// `inherited_profile_values` without forming a reference to the whole
    /// [`scheduler::SchedulerState`].
    #[inline]
    pub(crate) fn recompute_effective_profile(&mut self) {
        let p = self.scheduler_state_ptr();
        // SAFETY: `self` proves `self.owner().lock()` is held exclusively, which guards
        // `base_profile`, `inherited_profile_values`, and `effective_profile`. Each field place is
        // projected individually from the raw pointer without forming a reference to the
        // surrounding `scheduler::SchedulerState`, and the three fields are disjoint.
        unsafe {
            let bp = &(*p).base_profile;
            let ipv = &(*p).inherited_profile_values;
            let ep = &mut (*p).effective_profile;
            ep.recompute(bp, ipv);
        }
    }

    /// Returns this thread's dynamic scheduling parameters, as
    /// `(start_time, finish_time, time_slice_ns, time_slice_used_ns)`.
    ///
    /// These fields are guarded by the owning scheduler's `queue_lock_`, so the values are only
    /// stable if the thread is not in a run queue (for example, because it is blocked, or because
    /// it is the current thread and preemption is disabled).
    #[inline]
    pub(crate) fn dynamic_parameters(
        &self,
    ) -> (
        scheduler::SchedTime,
        scheduler::SchedTime,
        scheduler::SchedDuration,
        scheduler::SchedDuration,
    ) {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`. The fields are read by
        // value through the raw pointer without forming a reference to the whole struct, matching
        // the C++ reads made while holding only the thread's lock.
        unsafe {
            ((*ss).start_time, (*ss).finish_time, (*ss).time_slice_ns, (*ss).time_slice_used_ns)
        }
    }

    /// Returns this thread's remaining time slice (`remaining_time_slice_ns()`).
    #[inline]
    pub(crate) fn remaining_time_slice_ns(&self) -> scheduler::SchedDuration {
        let (_, _, time_slice_ns, time_slice_used_ns) = self.dynamic_parameters();
        scheduler::SchedDuration::from(time_slice_ns - time_slice_used_ns)
    }

    /// Returns this thread's activation count (`activation_count()`).
    #[inline]
    pub(crate) fn activation_count(&self) -> u64 {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, and `activation_count` is
        // read by value through the raw pointer without forming a reference to the whole struct.
        unsafe { (*ss).activation_count }
    }

    /// Returns the time at which this thread last started running (`last_started_running()`).
    #[inline]
    pub(crate) fn last_started_running(&self) -> scheduler::SchedTime {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, and
        // `last_started_running` is read by value through the raw pointer without forming a
        // reference to the whole struct.
        unsafe { (*ss).last_started_running }
    }

    /// Returns this thread's accumulated runtime in nanoseconds (`runtime_ns()`).
    #[inline]
    fn runtime_ns(&self) -> scheduler::SchedDuration {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, holding `self.owner()`'s
        // `ChainLock` (`self`) prevents concurrent scheduler writes to `runtime_ns`, and reading by
        // value through the raw pointer avoids forming a reference to the whole struct.
        unsafe { (*ss).runtime_ns }
    }

    /// Returns the CPU this thread is currently running on, or `INVALID_CPU` if it is not running
    /// (`curr_cpu()`).
    #[inline]
    pub fn curr_cpu(&self) -> cpu_num_t {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, holding `self.owner()`'s
        // `ChainLock` (`self`) prevents concurrent scheduler writes to `curr_cpu`, and reading by
        // value through the raw pointer avoids forming a reference to the whole struct.
        unsafe { (*ss).curr_cpu }
    }

    /// Returns the CPU this thread last ran on, or `INVALID_CPU` if it has never run
    /// (`Thread::LastCpuLocked`).
    #[inline]
    pub fn last_cpu(&self) -> cpu_num_t {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, holding `self.owner()`'s
        // `ChainLock` (`self`) prevents concurrent scheduler writes to `last_cpu`, and reading by
        // value through the raw pointer avoids forming a reference to the whole struct.
        unsafe { (*ss).last_cpu }
    }

    /// Returns the hard CPU affinity mask of this thread (`hard_affinity()`).
    #[inline]
    fn hard_affinity(&self) -> cpu_mask_t {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, holding `self.owner()`'s
        // `ChainLock` (`self`) prevents concurrent scheduler writes to `hard_affinity`, and reading
        // by value through the raw pointer avoids forming a reference to the whole struct.
        unsafe { (*ss).hard_affinity }
    }

    /// Returns the soft CPU affinity mask of this thread (`soft_affinity()`).
    #[inline]
    fn soft_affinity(&self) -> cpu_mask_t {
        let ss = self.scheduler_state_ptr();
        // SAFETY: `ss` points to `self.owner()`'s live `SchedulerState`, holding `self.owner()`'s
        // `ChainLock` (`self`) prevents concurrent scheduler writes to `soft_affinity`, and reading
        // by value through the raw pointer avoids forming a reference to the whole struct.
        unsafe { (*ss).soft_affinity }
    }

    /// Returns a `RefPtr` to this thread's current address space as long as the thread has not
    /// Computes the total time the thread has spent running on a CPU while holding its lock.
    #[inline]
    pub fn runtime(&self) -> DurationMono {
        let mut runtime = DurationMono(self.runtime_ns().raw_value());
        if self.state() == ThreadStateKind::Running {
            let recent = crate::platform_rs::timer::current_mono_time()
                - InstantMono(self.last_started_running().raw_value());
            runtime = runtime + recent;
        }
        runtime
    }

    /// Returns a strong reference to this thread's address space, or `None` if it has no address
    /// space or has entered the [`ThreadStateKind::Death`] state (`Thread::GetAspaceRefLocked`).
    #[inline]
    pub fn get_aspace_ref_locked(&self) -> Option<fbl::RefPtr<VmAspace>> {
        // `Relaxed` ordering suffices because holding `self.owner()`'s lock (`self`) while checking
        // `self.state() != ThreadStateKind::Death` synchronizes with `Thread::Current::Exit`
        // clearing `aspace` and transitioning the thread to `Death` under the thread lock.
        let cur_aspace = self.owner().aspace.load(Ordering::Relaxed);
        if cur_aspace.is_null() || self.state() == ThreadStateKind::Death {
            return None;
        }

        // SAFETY: Per rule #1 for `aspace` in `Thread::get_aspace_ref`, holding `self.owner()`'s
        // lock while `self.state() != ThreadStateKind::Death` guarantees that `cur_aspace` points
        // to a live, ref-counted `VmAspace` (`VmAspace: Recyclable`, kept alive by the thread's
        // process or static storage) for the duration of this call.
        Some(unsafe { VmAspace::clone_from_ptr(cur_aspace) })
    }

    /// Returns a reference to this thread's [`ThreadState`].
    #[inline]
    pub fn wait_queue_state(&self) -> &'p ThreadState {
        // `ThreadState`'s own members are `KCell`s and tree bookkeeping with their own protocols,
        // so handing out the reference needs no lock proof beyond the thread outliving `'p`.
        &self.owner().wait_queue_state
    }

    /// Returns the list of `OwnedWaitQueue`s this thread owns.
    pub(crate) fn owned_wait_queues(&self) -> &OwnedWaitQueueList {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `owned_wait_queues`.
        unsafe { self.owner().owned_wait_queues.get(self.token()) }
    }

    /// Returns the list of `OwnedWaitQueue`s this thread owns mutably.
    pub(crate) fn owned_wait_queues_mut(&mut self) -> Pin<&mut OwnedWaitQueueList> {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `owned_wait_queues`; the unique borrow of `self` ensures no other
        // reference to `owned_wait_queues` is live. `Pin::new_unchecked`: every `Thread` is pinned
        // in memory for its whole life, so the embedded list never moves.
        unsafe { Pin::new_unchecked(thread.owned_wait_queues.get_mut(token)) }
    }

    /// Returns the policy exception code saved by [`signal_policy_exception`].
    fn extra_policy_exception_code(&self) -> u32 {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `extra_policy_exception_code`.
        *unsafe { self.owner().extra_policy_exception_code.get(self.token()) }
    }

    /// Returns the policy exception code mutably.
    fn extra_policy_exception_code_mut(&mut self) -> &mut u32 {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `extra_policy_exception_code`; the unique borrow of `self` ensures no
        // other reference is live.
        unsafe { thread.extra_policy_exception_code.get_mut(token) }
    }

    /// Returns the policy exception data saved by [`signal_policy_exception`].
    fn extra_policy_exception_data(&self) -> u32 {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `extra_policy_exception_data`.
        *unsafe { self.owner().extra_policy_exception_data.get(self.token()) }
    }

    /// Returns the policy exception data mutably.
    fn extra_policy_exception_data_mut(&mut self) -> &mut u32 {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `extra_policy_exception_data`; the unique borrow of `self` ensures no
        // other reference is live.
        unsafe { thread.extra_policy_exception_data.get_mut(token) }
    }

    /// Returns whether this thread is allowed to own wait queues.
    pub(crate) fn can_own_wait_queues(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `can_own_wait_queues`.
        *unsafe { self.owner().can_own_wait_queues.get(self.token()) }
    }

    /// Returns whether this thread is allowed to own wait queues mutably.
    pub(crate) fn can_own_wait_queues_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `can_own_wait_queues`; the unique borrow of `self` ensures no other
        // reference is live.
        unsafe { thread.can_own_wait_queues.get_mut(token) }
    }

    /// Returns whether user register state has been saved to the thread's `arch_thread`.
    fn user_state_saved(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `user_state_saved`.
        *unsafe { self.owner().user_state_saved.get(self.token()) }
    }

    /// Returns whether user register state has been saved mutably.
    fn user_state_saved_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `user_state_saved`; the unique borrow of `self` ensures no other
        // reference is live.
        unsafe { thread.user_state_saved.get_mut(token) }
    }

    /// Saves the current user thread's register state into its `arch_thread` if not already saved.
    ///
    /// Returns `true` if the state was saved by this call, or `false` if it was already saved.
    ///
    /// # Safety
    ///
    /// - Interrupts must be disabled (`ints_disabled()`).
    /// - `current` must equal `self.owner()` and point to the currently executing user thread
    ///   (`current_get()`, `!user_thread().is_null()`) with initialized architectural user-state
    ///   storage and original mutable provenance for `arch_save_user_state`.
    #[inline]
    #[must_use]
    unsafe fn save_user_state_locked(&mut self, current: *mut Thread) -> bool {
        debug_assert!(core::ptr::eq(self.owner(), current));
        debug_assert!(core::ptr::eq(current, current_get()));
        debug_assert!(!self.owner().user_thread().is_null());

        if self.user_state_saved() {
            core::hint::cold_path();
            return false;
        }
        *self.user_state_saved_mut() = true;
        // SAFETY: Interrupts are disabled, `current` is the currently executing user thread with
        // initialized architectural user-state storage, and `self` proves its lock is held.
        unsafe { crate::arch_rs::save_user_state(current) };
        true
    }

    /// Restores the current user thread's register state from its `arch_thread`.
    ///
    /// # Safety
    ///
    /// - Interrupts must be disabled (`ints_disabled()`).
    /// - `current` must equal `self.owner()` and point to the currently executing user thread
    ///   (`current_get()`, `!user_thread().is_null()`) with initialized architectural user-state
    ///   storage, original mutable provenance for `arch_restore_user_state`, and
    ///   `self.user_state_saved() == true` from a prior [`Self::save_user_state_locked`] call.
    #[inline]
    unsafe fn restore_user_state_locked(&mut self, current: *mut Thread) {
        debug_assert!(core::ptr::eq(self.owner(), current));
        debug_assert!(core::ptr::eq(current, current_get()));
        debug_assert!(!self.owner().user_thread().is_null());

        debug_assert!(self.user_state_saved());
        *self.user_state_saved_mut() = false;
        // SAFETY: Interrupts are disabled, `current` is the currently executing user thread whose
        // register state was saved, and `self` proves its lock is held.
        unsafe { crate::arch_rs::restore_user_state(current) };
    }

    /// Returns `true` if it decides to kill the thread, which must be the current thread.
    ///
    /// TODO: move this to CurrentThread, once that becomes a subclass of Thread.
    #[inline]
    fn check_kill_signal(&self) -> bool {
        if (self.owner().signals() & THREAD_SIGNAL_KILL) != 0 {
            core::hint::cold_path();
            // Ensure we don't recurse into thread_exit.
            debug_assert!(self.state() != ThreadStateKind::Death);
            true
        } else {
            false
        }
    }

    /// Returns whether a migration is in progress for this thread (`Thread::migrate_pending`).
    #[inline]
    pub fn migrate_pending(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `migrate_pending`.
        *unsafe { self.owner().migrate_pending.get(self.token()) }
    }

    /// Returns whether a migration is in progress mutably.
    #[inline]
    fn migrate_pending_mut(&mut self) -> &mut bool {
        let (thread, token) = self.split_mut();
        // SAFETY: As in `migrate_pending`; the unique borrow of `self` ensures no other reference
        // is live.
        unsafe { thread.migrate_pending.get_mut(token) }
    }

    /// Returns whether this thread has a migration callback registered (`Thread::has_migrate_fn`).
    #[inline]
    pub fn has_migrate_fn(&self) -> bool {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `migrate_fn`.
        unsafe { self.owner().migrate_fn.get(self.token()) }.as_option().is_some()
    }

    /// Returns the migration callback registered for this thread mutably.
    #[inline]
    fn migrate_fn_mut(&mut self) -> &mut Option<MigrateFn> {
        let (thread, token) = self.split_mut();
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `migrate_fn`; the
        // unique borrow of `self` ensures no other reference is live.
        unsafe { thread.migrate_fn.get_mut(token) }.as_option_mut()
    }

    /// Returns the context-switch callback registered for this thread mutably.
    #[inline]
    fn context_switch_fn_mut(&mut self) -> &mut Option<ContextSwitchFn> {
        let (thread, token) = self.split_mut();
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `context_switch_fn`; the unique borrow of `self` ensures no other reference is live.
        unsafe { thread.context_switch_fn.get_mut(token) }.as_option_mut()
    }

    /// Sets or clears the migration callback for this thread while holding
    /// `Thread::get_list_lock()` and this thread's lock (`Thread::SetMigrateFnLocked`).
    ///
    /// Returns the previously registered callback (if any) so the caller can drop it after
    /// releasing locks and restoring interrupts.
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - This thread must not have been erased via [`Thread::erase_from_lists_locked`].
    /// - The caller must not currently hold any scheduler `queue_lock_`.
    #[must_use]
    pub unsafe fn set_migrate_fn_locked(
        mut self,
        migrate_fn: Option<MigrateFn>,
        list_token: &mut LockToken<'_, ThreadListLockClass>,
    ) -> Option<MigrateFn> {
        let thread_cpu = self.curr_cpu();
        let queue_guard = core::pin::pin!(ksync::GuardSlot::uninit());
        if thread_cpu != crate::kernel::types::INVALID_CPU {
            // SAFETY: `thread_cpu` is a valid, initialized CPU index because `curr_cpu` is only
            // set to an allocated CPU number or `INVALID_CPU`.
            let scheduler = unsafe { &crate::kernel::percpu::PerCpu::get(thread_cpu).scheduler };
            let _ = ksync::GuardSlot::init(
                queue_guard,
                scheduler.queue_lock().lock_policy_with::<NoIrqSavePolicy>(ksync::source_tag!()),
            );
        }

        // We should never be assigning a new migration function while there is a
        // migration still in progress.
        debug_assert!(migrate_fn.is_none() || !self.migrate_pending());

        let had_migrate_fn = self.has_migrate_fn();
        let has_new_migrate_fn = migrate_fn.is_some();

        // If `migrate_fn` was previously set, remove `this` from `migrate_list`.
        if had_migrate_fn {
            // SAFETY: Caller guarantees `thread_lists_init()` has completed.
            // `migrate_list_node` is only ever linked into `MIGRATE_LIST`, and it is in a
            // container whenever `migrate_fn` is `Some` (maintained by this function and
            // `erase_from_lists_locked`).
            let erased = unsafe { migrate_list(list_token).erase(self.owner()) };
            debug_assert!(erased.is_some());
        }

        let old = core::mem::replace(self.migrate_fn_mut(), migrate_fn);

        // Clear stale state when (un) setting the migrate fn.
        //
        // TODO(https://fxbug.dev/42164826): Cleanup the migrate fn feature and associated state
        // and clearly define and check invariants.
        *self.migrate_pending_mut() = false;

        // If `migrate_fn` is valid, add `this` to `migrate_list`.
        if has_new_migrate_fn {
            let ptr = NonNull::from(self.owner());
            // SAFETY: Caller guarantees `thread_lists_init()` has completed. `self.owner()`
            // is a valid, pinned `Thread` not currently in `MIGRATE_LIST` (either it was not in
            // `MIGRATE_LIST` or it was just erased above), and remains allocated until erased in
            // `set_migrate_fn_locked` or `erase_from_lists_locked`.
            unsafe { migrate_list(list_token).push_front_raw(ptr) };
        }

        old
    }

    /// Sets or clears the context-switch callback for this thread while holding its lock
    /// (`Thread::SetContextSwitchFnLocked`).
    ///
    /// Returns the previously registered callback (if any) so the caller can drop it after
    /// releasing the thread lock and restoring interrupts.
    #[inline]
    #[must_use]
    pub fn set_context_switch_fn_locked(
        mut self,
        context_switch_fn: Option<ContextSwitchFn>,
    ) -> Option<ContextSwitchFn> {
        core::mem::replace(self.context_switch_fn_mut(), context_switch_fn)
    }
}

/// Enters restricted mode on the current thread using the given vector table pointer and context.
///
/// # Errors
/// - `Status::INVALID_ARGS`: `vector_table_ptr` is not a valid user-accessible address.
/// - `Status::BAD_STATE`: No `RestrictedState` is bound to the current thread, or state is invalid.
/// - `Status::INTERRUPTED_RETRY`: Current thread has pending signals that must be processed.
///
/// # Safety
///
/// * The calling thread must be an initialized thread not currently in restricted mode, with
///   exclusive access to its bound [`RestrictedState`] (if any) and no live references to it.
/// * If a [`RestrictedState`] is bound, the calling thread must belong to a user process
///   ([`crate::object::ProcessDispatcher::get_current`]).
pub unsafe fn restricted_enter(vector_table_ptr: usize, context: usize) -> Result<(), Status> {
    // SAFETY: Upheld by the caller.
    unsafe { crate::kernel::restricted::restricted_enter(vector_table_ptr, context) }
}

/// Type-safe wrapper around a raw pointer to a Zircon kernel Thread.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadPtr(NonNull<Thread>);

// SAFETY: A ThreadPtr is just a pointer to a kernel thread, which can be safely passed
// between threads to perform join or kill operations.
unsafe impl Send for ThreadPtr {}
unsafe impl Sync for ThreadPtr {}

impl From<&Thread> for ThreadPtr {
    #[inline]
    fn from(thread: &Thread) -> Self {
        Self(NonNull::from(thread))
    }
}

/// The outcome of a successful call to [`ThreadPtr::detach_internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachResult {
    /// The thread has been flagged as detached, and will release its memory
    /// once execution is done.
    Detached,
    /// The thread had already exited, so it has been joined and its memory has
    /// been released.  The caller must not touch the thread again.
    Joined,
}

/// Operation selector for [`ThreadPtr::suspend_or_kill_internal`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum SuspendOrKillOp {
    Suspend,
    Kill,
}

impl ThreadPtr {
    /// Creates a `ThreadPtr` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a live kernel thread.
    pub const unsafe fn from_raw(ptr: *mut Thread) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    #[inline]
    pub fn as_raw(self) -> *mut Thread {
        let ptr = self.0.as_ptr();
        // SAFETY: `ThreadPtr` always points to a valid, live `Thread`, which is 64-byte aligned.
        unsafe { core::hint::assert_unchecked(ptr.is_aligned()) };
        ptr
    }

    /// Returns the raw const pointer.
    pub const fn as_ptr(self) -> *const Thread {
        self.0.as_ptr()
    }

    /// Returns a shared reference to the underlying [`Thread`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the underlying [`Thread`] is valid and remains alive for `'a`.
    #[inline]
    pub const unsafe fn as_ref<'a>(self) -> &'a Thread {
        // SAFETY: The caller guarantees the underlying `Thread` is valid and remains alive for
        // `'a`.
        unsafe { self.0.as_ref() }
    }

    /// Makes a suspended thread executable (`Thread::Resume`).
    ///
    /// Called to mark a thread as schedulable.
    ///
    /// This function is called to start a thread which has just been
    /// created with [`create`] or which has been suspended with
    /// [`ThreadPtr::suspend`]. It can not fail.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// * The thread is non-idle and has not been joined or destroyed. If `self` is detached, it
    ///   must still be in [`ThreadStateKind::Initial`] with nothing concurrently starting or
    ///   freeing it before `resume` locks it (otherwise it could exit and be freed by `FreeDpc`
    ///   before `resume` acquires its lock), and `self` must not be used after this call since it
    ///   may exit and be freed by another CPU as soon as its lock is released.
    /// * Either `num_spinlocks_held() == 0` or local preemption is disabled
    ///   (`!preempt_is_enabled()`).
    /// * The caller does not have an active `ChainLockTransaction`
    ///   (`TA_EXCL(chainlock_transaction_token)`).
    pub unsafe fn resume(self) {
        // SAFETY: The caller guarantees the thread has not been joined or destroyed.
        unsafe { Thread::assert_canary_ptr(self.as_raw()) };

        // We cannot allow a resume to happen if we are holding any spinlocks, unless
        // local preemption has been disabled.  If we have local preemption enabled,
        // and a spinlock held, then it is theoretically possible for our current CPU
        // to be chosen as the target for the thread being resumed, triggering a local
        // preemption event.  This is illegal; being preempted while holding a
        // spinlock means that we might lose our CPU while holding a spinlock.
        //
        // So, assert this here.  Either we have no spinlocks, or preemption has
        // already been disabled (presumably to the point where we have dropped all of
        // our spinlocks)
        debug_assert!(
            (crate::arch_rs::ops::num_spinlocks_held() == 0) || !preempt_is_enabled(),
            "It is illegal to Resume a thread when any spinlocks are held unless local \
             preemption is disabled.  (spinlocks held {}, preemption enabled {})",
            crate::arch_rs::ops::num_spinlocks_held(),
            i32::from(preempt_is_enabled()),
        );

        // Disable preemption before grabbing the target thread's lock.  This will
        // ensure that we have dropped the target thread's lock before any local
        // reschedule takes place, helping to avoid lock thrash as the target thread
        // becomes scheduled for the first time.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            let should_unblock = {
                // SAFETY: The thread has not been joined or destroyed (the caller's contract). The
                // reference is formed per attempt rather than captured, and ends with this block
                // before releasing the thread's lock.
                let this = unsafe { self.as_ref() };
                let mut guard = OwnerGuard::new(this, token);
                ChainLockTransaction::finalize(token);

                if guard.proof().state() == ThreadStateKind::Death {
                    core::hint::cold_path();
                    // The thread is dead, resuming it is a no-op.
                    let _ = guard.into_unguarded();
                    // SAFETY: `self.as_raw()` is the locked thread and `guard` was consumed above,
                    // so no guard, proof, or `&Thread` reference is live across the unlock.
                    unsafe { (*Thread::lock_ptr(self.as_raw())).as_chain_lock().release(token) };
                    return DONE.into();
                }

                // Emit the thread metadata the first time the thread is resumed so that trace
                // events written by this thread have the correct name and process association.
                if guard.proof().state() == ThreadStateKind::Initial {
                    // Only called from the trace macro arguments below, which are evaluated only
                    // when the trace category is enabled.
                    let job_koid = || {
                        // SAFETY: `self` is a live `Thread`, and its `user_thread_` reference (if
                        // any) keeps the `ThreadDispatcher` alive for as long as the `Thread` is.
                        match unsafe { self.user_thread_raw().as_ref() } {
                            Some(user_thread) => user_thread.process().job().get_koid(),
                            None => 0,
                        }
                    };
                    crate::ktrace_rs::kernel_object!(
                        "kernel:meta",
                        this.tid(),
                        ZX_OBJ_TYPE_THREAD,
                        this.name().as_bytes(),
                        "process" => crate::ktrace_rs::Koid(this.pid()),
                        "job" => crate::ktrace_rs::Koid(job_koid()),
                    );
                }

                // Clear the suspend signal in case there is a pending suspend
                this.clear_signals(THREAD_SIGNAL_SUSPEND);
                let state = guard.proof().state();
                // Take over the lock from `guard` while it is still held, so that no `&Thread` is
                // passed across its release: a detached thread may be freed as soon as its lock is
                // released.
                let _ = guard.into_unguarded();
                state == ThreadStateKind::Initial || state == ThreadStateKind::Suspended
            };

            if should_unblock {
                // Wake up the new thread, putting it in a run queue on a cpu.
                // SAFETY: `self` is the stored pointer to the live, non-idle thread, whose lock
                // the active transaction (finalized above) holds as the sole lock with no
                // conflicting reference, guard, or proof live (`this` and `guard` ended with the
                // block above); an initial or suspended thread is not running and is in no wait
                // queue or run queue; interrupts and local preemption are disabled by
                // `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`; and `self` is not dereferenced
                // afterwards.
                unsafe { scheduler::unblock(self, token) };
            } else {
                // SAFETY: `self.as_raw()` is the locked thread and `this` and `guard` ended with
                // the block above, so no guard, proof, or `&Thread` reference is live across the
                // unlock.
                unsafe { (*Thread::lock_ptr(self.as_raw())).as_chain_lock().release(token) };
            }

            THREAD_RESUME_COUNT.add(1);
            DONE.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Thread::Resume"),
            do_transaction,
        );
    }

    /// Marks a thread as detached, in this state its memory will be released once
    /// execution is done (`Thread::Detach`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// * The thread has not been joined, detached, or destroyed, and no other thread joins,
    ///   detaches, or forgets it concurrently.
    /// * `self` is not used after `detach()` returns unless the thread is still in its initial
    ///   state (i.e. it has not been resumed yet). If the thread has already exited, `detach()`
    ///   synchronously joins it (`join(InstantMono(0))`) and frees the [`Thread`] before returning;
    ///   once a detached thread has been resumed, it may exit and be freed at any time.
    /// * The caller does not hold `self`'s lock or `Thread::get_list_lock()`, and does not have an
    ///   active `ChainLockTransaction`, since `detach` creates its own transaction and may call
    ///   [`ThreadPtr::join`].
    pub unsafe fn detach(self) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller.
        unsafe { self.detach_internal() }.map(|_| ())
    }

    /// Common implementation of [`ThreadPtr::detach`] and [`ThreadPtr::detach_and_resume`]
    /// (`Thread::DetachInternal`).
    ///
    /// # Safety
    ///
    /// The caller must uphold the [`ThreadPtr::detach`] preconditions.
    unsafe fn detach_internal(self) -> Result<DetachResult, Status> {
        // SAFETY: The caller guarantees the thread has not been joined, detached, or destroyed.
        unsafe { Thread::assert_canary_ptr(self.as_raw()) };

        #[derive(PartialEq, Eq)]
        enum JoinThread {
            No,
            Yes,
        }

        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<JoinThread> {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            // SAFETY: The thread is neither detached nor joined yet, so it is not freed while this
            // transaction holds its lock.
            let this = unsafe { self.as_ref() };
            // Lock the target thread's state before proceeding.
            let mut guard = OwnerGuard::new(this, token);

            // Try to wake our joiners with the error "BAD_STATE" (the thread is now
            // detached and can no longer be joined).  If we fail, we need to drop our
            // locks and try again.  If we succeed, our transaction will be finalized
            // and we can finish updating all of our state.
            // SAFETY: The transaction is not yet finalized and only `this`'s lock is held (a
            // thread cannot be blocked in its own `retcode_wait_queue`).
            if !unsafe {
                this.task_state.try_wake_joiners(Err(Status::BAD_STATE), token, &preempt_disabled)
            } {
                return Action::Backoff.into();
            }

            // `flags` is relaxed; the thread's lock, held here, orders these updates with
            // [`ThreadPtr::join`] and `Thread::Current::Exit`, which read `DETACHED` under it.
            //
            // If the target thread is not yet dead, flag it as detached and get out.
            if guard.proof().state() != ThreadStateKind::Death {
                this.flags.fetch_or(THREAD_FLAG_DETACHED);
                let _ = guard.into_unguarded();
                // SAFETY: `self.as_raw()` is the locked thread and `guard` was consumed above, so
                // no guard, proof, or `&Thread` reference is live across the unlock, after which
                // the newly-detached thread may immediately exit and be freed on another CPU.
                unsafe { (*Thread::lock_ptr(self.as_raw())).as_chain_lock().release(token) };
                return TransactionResult::Value(JoinThread::No);
            }

            // Looks like the thread has already died.  Make sure the DETACHED flag has
            // been cleared (so that `join` continues).
            this.flags.fetch_and(!THREAD_FLAG_DETACHED); // makes sure `join` continues
            // Drop our lock and `join` to finish the cleanup.
            TransactionResult::Value(JoinThread::Yes)
        };

        let result = ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Thread::Detach"),
            do_transaction,
        );

        if result == JoinThread::Yes {
            // The thread was already dead, meaning we are the only ones positioned to do the
            // cleanup.
            //
            // This `join` cannot be moved inside `do_transaction`, as it creates its own
            // ChainLockTransaction, and nested transactions are not allowed.
            //
            // SAFETY: The thread is dead and was not detached (`DETACHED` was just cleared), and
            // the caller guarantees nobody else joins, detaches, or forgets it. Since the thread
            // is dead, `join` does not block and the caller is not that thread. `detach`'s
            // contract excludes holding the thread's lock or the list lock, and no transaction is
            // active here.
            unsafe { self.join(InstantMono(0)) }?;
            return Ok(DetachResult::Joined);
        }
        Ok(DetachResult::Detached)
    }

    /// Detaches and resumes a newly created thread (`Thread::DetachAndResume`).
    ///
    /// If the thread has already exited, it is joined and freed by the detach and is not
    /// resumed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// * The [`ThreadPtr::detach`] preconditions hold.
    /// * Either the thread has already exited, or it is still in its initial state and nothing
    ///   else can start it (no concurrent `resume` or `kill`) before this call returns; otherwise
    ///   a detached thread could exit and be freed between the detach and the resume.
    /// * `self` is not used after this call, since the detached thread may exit and be freed as
    ///   soon as it is resumed.
    /// * The caller does not have an active `ChainLockTransaction`.
    pub unsafe fn detach_and_resume(self) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller.
        let result = unsafe { self.detach_internal() }?;
        // If the thread had already exited, then `detach_internal` has joined it and
        // released its memory.  Resuming a dead thread is a no-op anyway, so skip the
        // `resume` rather than touch the freed thread.
        if result == DetachResult::Detached {
            // SAFETY: `detach_internal` returned `Detached`, so the thread had not exited, and the
            // caller guarantees nothing could start it in the meantime, so it has not been freed.
            unsafe { self.resume() };
        }
        Ok(())
    }

    /// Waits for the thread to reach [`ThreadStateKind::Death`], reads its return code, and
    /// unlinks it from the global thread lists (`Thread::Join` wait phase).
    ///
    /// Outlined from [`ThreadPtr::join`] so its `ChainLockTransaction`, `SpinLock` guard, and
    /// callee-saved register state are popped before synchronous thread and kernel-stack VMO
    /// teardown runs in [`free_thread_resources`].
    ///
    /// # Safety
    ///
    /// The caller must uphold the [`ThreadPtr::join`] preconditions. On `Ok`, the target thread is
    /// in [`ThreadStateKind::Death`], has been unlinked from the global thread lists, and is ready
    /// to be torn down by [`free_thread_resources`].
    #[inline(never)]
    unsafe fn join_wait_for_death(self, deadline: InstantMono) -> Result<i32, Status> {
        // SAFETY: The caller guarantees the thread has not been joined, forgotten, or freed.
        unsafe { Thread::assert_canary_ptr(self.as_raw()) };

        // No thread should be attempting to join itself.
        let current_thread = current_get();
        debug_assert!(self.as_raw() != current_thread);
        // SAFETY: `current_get()` returns the running thread, which outlives this call.
        let current_thread: &Thread = unsafe { &*current_thread };

        let mut retcode = 0;
        let mut finished = false;
        while !finished {
            let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<Status> {
                // SAFETY: Only this joiner frees the thread (the caller excludes concurrent
                // join, detach, and forget). The reference is still formed per attempt and is not
                // held across `TaskState::join`, as that function requires.
                let this = unsafe { self.as_ref() };
                // Start by locking the global thread list lock, and the target thread's lock.
                ksync::lock!(
                    let mut list_guard = Thread::get_list_lock().lock_policy::<NoIrqSavePolicy>()
                );
                let mut guard = OwnerGuard::new(this, token);

                if this.detached() {
                    // the thread is detached, go ahead and exit
                    return TransactionResult::Value(Status::BAD_STATE);
                }

                // If we need to wait for our thread to die, we need exchange our current
                // locks (the target thread's lock and the list lock) for the current thread
                // and the wait_queue's lock.  If this operation fails with a back-off
                // error, we need to drop all of our locks and try again.
                if guard.proof().state() != ThreadStateKind::Death {
                    // SAFETY: The raw locks are only acquired here; no guard or proof for the
                    // current thread's lock or the target's retcode wait queue lock is live.
                    let acquired = unsafe {
                        acquire_both_or_backoff(
                            current_thread.lock().as_chain_lock(),
                            this.task_state.get_lock().as_chain_lock(),
                            token,
                        )
                    };
                    if !acquired {
                        core::hint::cold_path();
                        return Action::Backoff.into();
                    }
                    // SAFETY: The current thread's lock was just acquired by the active
                    // transaction, and no other guard or proof for it is live.
                    let mut current_guard =
                        unsafe { OwnerGuard::adopt_unchecked(current_thread, token) };

                    // We now have all of the locks we need for this operation.
                    ChainLockTransaction::finalize(token);

                    // Now go ahead and drop our other locks, and block in the wait queue.
                    // This will release the wait queue's lock for us, and will release and
                    // re-acquire our thread's lock (as we block and eventually become
                    // rescheduled).  Once we are done waiting, drop the current thread's
                    // lock, then either start again, or propagate any wait error we encounter
                    // up the stack.
                    guard.release();
                    list_guard.as_mut().release();
                    // SAFETY: The current thread is running, not in any wait queue, and may
                    // block (the caller's contract). The finalized transaction holds exactly the
                    // retcode wait queue's lock, with no guard or proof for it live, and the
                    // current thread's lock. The queue stays valid while we are blocked in it:
                    // the target is only freed by its joiner (us), and `this` is not used after
                    // `TaskState::join` returns.
                    let status = unsafe {
                        TaskState::join(
                            &raw const this.task_state,
                            &mut current_guard.proof(),
                            deadline,
                            token,
                        )
                    };
                    current_guard.release();
                    if let Err(status) = status {
                        return TransactionResult::Value(status);
                    }

                    // break out of the lock-acquisition loop, but do not set the finished
                    // flag.  This way, we will start a ChainLockTransaction on the new
                    // check of the thread's state instead of attempting to use our already
                    // finalized transaction.
                    return TransactionResult::Value(Status::SHOULD_WAIT);
                }

                this.assert_canary();
                let proof = guard.proof();
                debug_assert!(proof.state() == ThreadStateKind::Death);
                proof.wait_queue_state().assert_not_blocked(proof.token());

                // save the return code
                // SAFETY: The thread's lock is held, and the thread is dead, so its exit path has
                // already written the return code.
                retcode = unsafe { this.task_state.retcode() };

                // remove it from global lists
                // SAFETY: The thread is dead and is being torn down here by its sole joiner.
                unsafe { this.erase_from_lists_locked(list_guard.as_mut().token_mut()) };

                // Our canary will be cleared out in free_thread_resources, which
                // explicitly drops the `Thread`.  Set the finished flag and break out of our
                // retry loop, dropping our locks in the process.
                finished = true;
                TransactionResult::Value(Status::SHOULD_WAIT)
            };
            let result = ChainLockTransaction::until_done(
                IRQ_SAVE_OPTION,
                clt_tag!("Thread::Join"),
                do_transaction,
            );
            if result != Status::SHOULD_WAIT {
                return Err(result);
            }
        }

        Ok(retcode)
    }

    /// Waits `deadline` time for a thread to complete execution then releases its memory
    /// (`Thread::Join`).
    ///
    /// Returns the thread's return code on success.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// * The thread has not been joined or forgotten, has not been detached (unless it is still in
    ///   [`ThreadStateKind::Initial`] with nothing concurrently resuming it), and no other thread
    ///   joins, detaches, or forgets it concurrently.
    /// * The caller is not the thread itself.
    /// * No `&Thread` references to `self` remain live across `join`, since `free_thread_resources`
    ///   drops the `Thread` and frees the allocation when `join` returns `Ok`.
    /// * `self` is invalidated when `join` returns `Ok` and must not be used afterwards.
    /// * The caller does not hold `self`'s lock or `Thread::get_list_lock()`, and is in a context
    ///   where blocking is permitted with no active `ChainLockTransaction`.
    pub unsafe fn join(self, deadline: InstantMono) -> Result<i32, Status> {
        // SAFETY: Forwarded from the caller; no `&Thread` reference is held across this call or
        // `free_thread_resources` below.
        let retcode = unsafe { self.join_wait_for_death(deadline) }?;

        THREAD_JOIN_COUNT.add(1);

        // SAFETY: `join_wait_for_death` returned `Ok`, so the thread is in `Death` and off the
        // global lists, and only this joiner references it.
        unsafe { free_thread_resources(self.as_raw()) };

        Ok(retcode)
    }

    /// Remove this thread from the scheduler, discarding its execution state
    /// (`Thread::Forget`).
    ///
    /// This is almost certainly not the function you want.  In the general case,
    /// this is incredibly unsafe.
    ///
    /// This will free any resources allocated by [`create_etc`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` that is not the current
    /// thread, in a state where forgetting it is permitted (e.g. initialized but never started),
    /// not in any wait queue, and not referenced by anything else, and that `self` is not used
    /// after this call.
    pub unsafe fn forget(self) {
        debug_assert!(current_get() != self.as_raw());

        {
            // SAFETY: The caller guarantees `self` points to a valid `Thread`; the reference does
            // not outlive this block.
            let this = unsafe { self.as_ref() };
            ksync::lock!(
                let mut list_guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>()
            );
            crate::kconcurrent::single_chain_lock_guard_owned!(
                guard,
                this,
                NO_IRQ_SAVE_OPTION,
                clt_tag!("Thread::Forget")
            );
            let proof = guard.proof();
            debug_assert!(!proof.wait_queue_state().in_wait_queue(proof.token()));
            // SAFETY: The thread is being forgotten during final teardown and will not have
            // `SetMigrateFn` called on it afterwards.
            unsafe { this.erase_from_lists_locked(list_guard.as_mut().token_mut()) };
        }

        // SAFETY: The thread is off the global lists, not in any wait queue, and the caller hands
        // over the last reference.
        unsafe { free_thread_resources(self.as_raw()) };
    }

    /// Suspends or kills an initialized/ready/running thread.
    ///
    /// Returns `Ok(())` on success, or `Err(Status::BAD_STATE)` if the thread is dead.
    ///
    /// # Safety
    ///
    /// - The caller must ensure `self` points to a valid, live non-idle [`Thread`].
    /// - Neither `self`'s lock nor an active `ChainLockTransaction` may be held on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    unsafe fn suspend_or_kill_internal(self, op: SuspendOrKillOp) -> Result<(), Status> {
        // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
        unsafe {
            Thread::assert_canary_ptr(self.as_raw());
            debug_assert!(!Thread::is_idle_ptr(self.as_raw()));
        }

        let suspend_op = op == SuspendOrKillOp::Suspend;
        let wakeup_status =
            if suspend_op { ZX_ERR_INTERNAL_INTR_RETRY } else { ZX_ERR_INTERNAL_INTR_KILLED };
        let set_signals = |this: &Thread| {
            if suspend_op {
                this.set_signals(THREAD_SIGNAL_SUSPEND);
                THREAD_SUSPEND_COUNT.add(1);
            } else {
                this.set_signals(THREAD_SIGNAL_KILL);
            }
        };

        enum LockedAction {
            Return(Result<(), Status>),
            Backoff,
            Unblock,
            Ipi(cpu_mask_t),
            UnblockFromWaitQueue(NonNull<WaitQueueBase>),
            Unsleep,
        }

        // Disable preemption to defer rescheduling until the transaction is completed.
        let do_transaction =
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
                // duration of this transaction.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };
                let action = {
                    // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
                    // `this` and `thread_guard` are scoped to this block so no `&Thread` or guard
                    // remains live across the lock release or unblock operations below.
                    let this = unsafe { self.as_ref() };
                    let mut thread_guard = OwnerGuard::new(this, token);
                    let cur_state = thread_guard.proof().state();

                    // If this is a kill operation, and we are killing ourself, just set our
                    // signal bits and get out.  We will process the pending signal (eventually)
                    // as we unwind.
                    if !suspend_op && core::ptr::eq(this, current_get()) {
                        debug_assert!(cur_state == ThreadStateKind::Running);
                        ChainLockTransaction::finalize(token);
                        set_signals(this);
                        let _ = thread_guard.into_unguarded();
                        LockedAction::Return(Ok(()))
                    }
                    // If the thread is in the DEATH state, it cannot be suspended.  Do not set
                    // any signals, or increment any kcounters on our way out.
                    else if cur_state == ThreadStateKind::Death {
                        core::hint::cold_path();
                        ChainLockTransaction::finalize(token);
                        let _ = thread_guard.into_unguarded();
                        LockedAction::Return(Err(Status::BAD_STATE))
                    } else {
                        // If the thread is sleeping, or it is blocked, then we can only wake it up
                        // for the kill or suspend operation if it is currently interruptible.
                        // Check this; if the thread is not interruptible, simply set the signals
                        // and get out. Someday, when the thread finally unblocks, it can deal with
                        // the signals.
                        let is_blocked = cur_state == ThreadStateKind::Blocked
                            || cur_state == ThreadStateKind::BlockedReadLock;
                        if (is_blocked || cur_state == ThreadStateKind::Sleeping) && {
                            let proof = thread_guard.proof();
                            proof.wait_queue_state().interruptible(proof.token())
                                == Interruptible::No
                        } {
                            ChainLockTransaction::finalize(token);
                            set_signals(this);
                            let _ = thread_guard.into_unguarded();
                            LockedAction::Return(Ok(()))
                        }
                        // Unblocking a thread who is blocked in a wait queue requires that we hold
                        // the entire PI chain starting from the thread before proceeding.
                        // Attempting to obtain this chain of locks might result in a conflict which
                        // requires us to back off, releasing all locks (including the initial
                        // thread's) before trying again.
                        else if is_blocked
                            // SAFETY: The transaction is not yet finalized and no downstream locks
                            // in `this`'s PI chain are held yet; if `Ok` is returned,
                            // `WaitQueueBase::unblock_thread` below unlocks all downstream locks.
                            && unsafe {
                                OwnedWaitQueue::lock_pi_chain_from_thread(
                                    &mut thread_guard.proof(),
                                    token,
                                )
                            } == crate::kconcurrent::ChainLockResult::Backoff
                        {
                            let _ = thread_guard.into_unguarded();
                            LockedAction::Backoff
                        } else {
                            // OK, we have now obtained all of our locks and are committed to the
                            // operation.  We can go ahead and set our signal state and increment
                            // our kcounters now.
                            ChainLockTransaction::finalize(token);
                            set_signals(this);

                            let action = match cur_state {
                                ThreadStateKind::Initial => {
                                    // TODO(johngro); Figure out what to do about this.  The
                                    // original versions of suspend and kill seem to disagree about
                                    // whether or not it is safe to be suspending or killing a
                                    // thread at this point.  The original suspend had the comment
                                    // below, indicating that it should be OK to unblock the thread.
                                    // The Kill code, however, had this comment:
                                    //
                                    // ```
                                    // thread hasn't been started yet.
                                    // not really safe to wake it up, since it's only in this state
                                    // because it's under construction by the creator thread.
                                    // ```
                                    //
                                    // indicating that the thread was not yet safe to wake.  It
                                    // really feels like this should be an ASSERT (people should not
                                    // be suspending/killing threads in the initial state), or it
                                    // should just leave the thread as is.  The signal is set, and
                                    // will be handled when the thread ends up running for the first
                                    // time.

                                    // Thread hasn't been started yet, add it to the run queue to
                                    // transition properly through the INITIAL -> READY state
                                    // machine first, then it will see the signal and go to SUSPEND
                                    // before running user code.
                                    //
                                    // Though the state here is still INITIAL, the higher-level code
                                    // has already executed ThreadDispatcher::start() so all the
                                    // userspace entry data has been initialized and will be ready
                                    // to go as soon as the thread is unsuspended.
                                    LockedAction::Unblock
                                }
                                ThreadStateKind::Ready => {
                                    // thread is ready to run and not blocked or suspended.
                                    // will wake up and deal with the signal soon.
                                    LockedAction::Return(Ok(()))
                                }
                                ThreadStateKind::Running => {
                                    // thread is running (on another cpu)
                                    // The following call is not essential.  It just makes the
                                    // thread suspension/kill happen sooner rather than at the next
                                    // timer interrupt or syscall.
                                    let curr_cpu =
                                        thread_guard.proof().scheduler_state().curr_cpu();
                                    LockedAction::Ipi(crate::kernel::types::cpu_num_to_mask(
                                        curr_cpu,
                                    ))
                                }
                                ThreadStateKind::Blocked | ThreadStateKind::BlockedReadLock => {
                                    // thread is blocked on something and marked interruptible.  We
                                    // are holding the entire PI chain at this point, and calling
                                    // unblock_thread on our blocking wait queue will release the
                                    // entire PI chain for us as part of the unblock operation.
                                    let proof = thread_guard.proof();
                                    let wq = proof
                                        .wait_queue_state()
                                        .blocking_wait_queue(proof.token())
                                        .unwrap();
                                    LockedAction::UnblockFromWaitQueue(wq)
                                }
                                ThreadStateKind::Sleeping => {
                                    // thread is sleeping and interruptible
                                    LockedAction::Unsleep
                                }
                                ThreadStateKind::Suspended => {
                                    // If this is a kill operation, and the thread is suspended,
                                    // resume it so it can get the kill signal.  Otherwise, do
                                    // nothing.  The thread was already suspended, and it still is.
                                    if !suspend_op {
                                        LockedAction::Unblock
                                    } else {
                                        LockedAction::Return(Ok(()))
                                    }
                                }
                                ThreadStateKind::Death => {
                                    core::hint::cold_path();
                                    panic!("Unexpected thread state {}", cur_state as u32);
                                }
                            };
                            let _ = thread_guard.into_unguarded();
                            action
                        }
                    }
                };

                match action {
                    LockedAction::Return(_) | LockedAction::Backoff | LockedAction::Ipi(_) => {
                        // SAFETY: `self.as_raw()` is the locked thread (`lock_pi_chain_from_thread`
                        // released any downstream locks on `Backoff`), and `this` and
                        // `thread_guard` ended with the block above, so no guard, proof, or
                        // `&Thread` reference is live across the unlock.
                        unsafe {
                            (*Thread::lock_ptr(self.as_raw())).as_chain_lock().release(token)
                        };
                        match action {
                            LockedAction::Return(res) => TransactionResult::Value(res),
                            LockedAction::Backoff => {
                                core::hint::cold_path();
                                Action::Backoff.into()
                            }
                            LockedAction::Ipi(mask) => {
                                crate::kernel::mp::interrupt(
                                    crate::kernel::mp::MpIpiTarget::Mask,
                                    mask,
                                );
                                TransactionResult::Value(Ok(()))
                            }
                            _ => unreachable!(),
                        }
                    }
                    LockedAction::Unblock => {
                        // SAFETY: `self` is a live, non-idle thread in `Initial` or `Suspended`
                        // whose lock is held as the sole lock in the finalized transaction `token`
                        // with no conflicting reference, guard, or proof live; interrupts and local
                        // preemption are disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                        unsafe { scheduler::unblock(self, token) };
                        TransactionResult::Value(Ok(()))
                    }
                    LockedAction::UnblockFromWaitQueue(wq) => {
                        // SAFETY: `self` is a live, non-idle thread blocked in `wq`, the
                        // transaction is finalized with `self` and its entire downstream PI chain
                        // (including `wq`) locked, no conflicting reference, guard, or proof is
                        // live, and interrupts and preemption are disabled by
                        // `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                        let _ = unsafe {
                            WaitQueueBase::unblock_thread(
                                wq,
                                self,
                                Err(wakeup_status),
                                token,
                                &preempt_disabled,
                            )
                        };
                        TransactionResult::Value(Ok(()))
                    }
                    LockedAction::Unsleep => {
                        // SAFETY: `self` is a live, non-idle sleeping thread not in any run queue
                        // or wait queue, its lock is held as the sole lock in the finalized
                        // transaction `token` with no conflicting reference, guard, or proof live,
                        // and interrupts and preemption are disabled by
                        // `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                        unsafe { ThreadState::unsleep(self, Err(wakeup_status), token) };
                        TransactionResult::Value(Ok(()))
                    }
                }
            };

        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Thread::SuspendOrKillInternal"),
            do_transaction,
        )
    }

    /// Delivers a kill signal to a thread.
    ///
    /// # Safety
    ///
    /// - The caller must ensure `self` points to a valid, live non-idle [`Thread`] (`!is_idle()`).
    /// - Neither `self`'s lock nor an active `ChainLockTransaction` may be held on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn kill(self) {
        // Ignore the result here; the target thread may already be entering the
        // `ThreadStateKind::Death` state (if it is racing with us, and calling `current_exit`),
        // in which case, mission accomplished.
        //
        // SAFETY: Forwarded from the caller.
        let _ = unsafe { self.suspend_or_kill_internal(SuspendOrKillOp::Kill) };
    }

    /// Suspends an initialized/ready/running thread.
    ///
    /// Returns `Ok(())` on success, `Err(Status::BAD_STATE)` if the thread is dead.
    ///
    /// # Safety
    ///
    /// - The caller must ensure `self` points to a valid, live non-idle [`Thread`] (`!is_idle()`).
    /// - Neither `self`'s lock nor an active `ChainLockTransaction` may be held on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn suspend(self) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller.
        unsafe { self.suspend_or_kill_internal(SuspendOrKillOp::Suspend) }
    }

    /// Kicks the thread out of restricted mode into normal mode if it is currently in restricted
    /// mode, or remembers the kick state for the next attempt to enter restricted mode.
    ///
    /// # Safety
    ///
    /// - The caller must ensure `self` points to a valid, live non-idle [`Thread`] (`!is_idle()`).
    /// - Neither `self`'s lock nor an active `ChainLockTransaction` may be held on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    pub unsafe fn restricted_kick(self) -> Result<(), Status> {
        debug::ltrace_entry!();
        // SAFETY: The caller guarantees `self.as_raw()` points to a valid, live kernel thread.
        unsafe {
            Thread::assert_canary_ptr(self.as_raw());
            debug_assert!(!Thread::is_idle_ptr(self.as_raw()));
        }

        let kicking_myself = core::ptr::eq(current_get(), self.as_raw());

        // Hold the thread's lock while we examine its state and figure out what to
        // do.
        let mut ipi_mask: cpu_mask_t = 0;
        let status = ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::RestrictedKick"),
            |token| -> TransactionResult<Result<(), Status>> {
                let res = {
                    // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
                    // `this` and `guard` are scoped to this block so no `&Thread` is live across
                    // the lock release below.
                    let this = unsafe { self.as_ref() };
                    let mut guard = OwnerGuard::new(this, token);
                    ChainLockTransaction::finalize(token);

                    let cur_state = guard.proof().state();
                    if cur_state == ThreadStateKind::Death {
                        core::hint::cold_path();
                        let _ = guard.into_unguarded();
                        Err(Status::BAD_STATE)
                    } else {
                        this.set_signals(THREAD_SIGNAL_RESTRICTED_KICK);

                        if cur_state == ThreadStateKind::Running && !kicking_myself {
                            // thread is running (on another cpu)
                            // Send an IPI after dropping this thread's lock to make sure that the
                            // thread processes the new signals. If the thread executes a regular
                            // syscall or is rescheduled the signals will also be processed then,
                            // but there's no upper bound on how long that could take.
                            //
                            // TODO(johngro) - what about a thread which is in a transient state
                            // (active migration or being stolen).  Should we skip the IPI and just
                            // make sure to force the scheduler to always re-evaluate pending
                            // signals after exiting a transient state?
                            ipi_mask = crate::kernel::types::cpu_num_to_mask(
                                guard.proof().scheduler_state().curr_cpu(),
                            );
                        }
                        let _ = guard.into_unguarded();
                        Ok(())
                    }
                };
                // SAFETY: `self.as_raw()` is the locked thread and `this` and `guard` ended with
                // the block above, so no guard, proof, or `&Thread` reference is live across the
                // unlock.
                unsafe { (*Thread::lock_ptr(self.as_raw())).as_chain_lock().release(token) };
                TransactionResult::Value(res)
            },
        );
        status?;

        if ipi_mask != 0 {
            crate::kernel::mp::interrupt(crate::kernel::mp::MpIpiTarget::Mask, ipi_mask);
        }

        THREAD_RESTRICTED_KICK_COUNT.add(1);
        Ok(())
    }

    /// Starts memory stall accounting for the thread by moving it out of the
    /// `IgnoredKernelOnly` stall state and into `Inactive`.
    ///
    /// # Safety
    ///
    /// - The caller must ensure `self` points to a valid, live kernel [`Thread`] in the
    ///   [`ThreadStateKind::Initial`] state prior to [`Self::resume`].
    /// - The thread must currently be in the [`ThreadStallState::IgnoredKernelOnly`] stall state,
    ///   and the caller must have exclusive access to its `memory_stall_state` (guaranteed while
    ///   the thread is unstarted in `Initial` before `resume()`).
    #[inline]
    pub unsafe fn set_memory_stall_state_inactive(self) {
        let ptr = self.as_raw();
        // SAFETY: The caller guarantees `self` points to a valid, unstarted `Thread` in `Initial`
        // with exclusive access to `memory_stall_state`.
        debug_assert!(unsafe { memory_stall_state(ptr) } == ThreadStallState::IgnoredKernelOnly);
        // SAFETY: As above.
        unsafe { set_memory_stall_state(ptr, ThreadStallState::Inactive) };
    }

    /// Checks if the thread is currently blocked.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed or joined.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    pub unsafe fn is_blocked(self) -> bool {
        // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
        let thread = unsafe { self.as_ref() };
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            thread,
            IRQ_SAVE_OPTION,
            clt_tag!("ThreadPtr::is_blocked")
        );
        matches!(guard.proof().state(), ThreadStateKind::Blocked | ThreadStateKind::BlockedReadLock)
    }

    /// Returns a snapshot of the thread's name (`Thread::name`), which is at most
    /// `ZX_MAX_NAME_LEN - 1` bytes long.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn name(self) -> ThreadName {
        // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
        unsafe { Thread::name_copy_ptr(self.as_ptr()) }
    }

    /// Sets the thread's name (`Thread::set_name`).
    ///
    /// This may truncate `name`, so that it (including a trailing NUL byte) fits in
    /// `ZX_MAX_NAME_LEN` bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid, live [`Thread`].
    #[inline]
    pub unsafe fn set_name(self, name: &[u8]) {
        // SAFETY: The caller guarantees `self` points to a valid, live `Thread`.
        unsafe { Thread::set_name_in_place(self.as_raw(), name) }
    }

    /// Returns a `ThreadPtr` representing the currently executing thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called after multi-threading has been
    /// initialized (i.e. after LK_INIT_LEVEL_THREADING).
    pub unsafe fn current() -> Self {
        unsafe { Self::from_raw(cpp_thread_current_get()) }.unwrap()
    }

    /// Returns the pid/tid of the thread as a tracing thread reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    #[inline]
    pub unsafe fn fxt_ref(self) -> FxtRef {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { FxtRef { pid: self.pid(), tid: self.tid() } }
    }

    /// Sets the base profile of the thread (`Thread::SetBaseProfile`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The thread pointer is still valid and the underlying thread has not been destroyed.
    /// - This function is called from normal thread context with no spinlocks, `self`'s lock, or
    ///   active `ChainLockTransaction` held on the current CPU.
    #[inline]
    pub unsafe fn set_base_profile(self, profile: &SchedulerStateBaseProfile) {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread` and that the calling
        // context is a normal thread context with no active `ChainLockTransaction` or spinlocks
        // held.
        unsafe { self.as_ref() }.set_base_profile(profile);
    }

    /// Returns the current hard CPU affinity mask of the thread (`Thread::GetCpuAffinity`).
    ///
    /// Affinity comes in two flavours:
    /// - **hard affinity**, which will always be respected by the scheduler. The scheduler will
    ///   panic if it can't satisfy this affinity.
    /// - **soft affinity**, indicating where the thread should ideally be scheduled. The scheduler
    ///   will respect the mask unless there are no other options (e.g., the soft affinity and hard
    ///   affinity don't contain any common CPUs).
    ///
    /// If the two masks conflict, the hard affinity wins.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn get_cpu_affinity(self) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { self.as_ref() }.get_cpu_affinity()
    }

    /// Sets the hard CPU affinity mask of the thread and returns its previous affinity mask
    /// (`Thread::SetCpuAffinity`).
    ///
    /// If a new mask is set, the thread will be migrated to satisfy the new constraint. Hard
    /// affinity will always be respected by the scheduler; the scheduler will panic if it cannot
    /// satisfy this affinity. Setting the affinity mask returns the previous value to make pinning
    /// and restoring operations simpler and more efficient.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock, any scheduler queue lock, or have an active
    ///   `ChainLockTransaction` on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn set_cpu_affinity(self, affinity: cpu_mask_t) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`; `assert_canary_ptr`
        // does not form a `&Thread` that would live across `Scheduler::set_cpu_affinity`.
        unsafe { Thread::assert_canary_ptr(self.as_ptr()) };
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread` with no active
        // `ChainLockTransaction` or `self`'s lock held.
        unsafe {
            scheduler::Scheduler::set_cpu_affinity(
                self.as_raw(),
                affinity,
                scheduler::AffinityType::Hard,
            )
        }
    }

    /// Returns the current soft CPU affinity mask of the thread (`Thread::GetSoftCpuAffinity`).
    ///
    /// Soft affinity indicates where the thread should ideally be scheduled. The scheduler will
    /// respect the mask unless there are no other options (e.g., the soft affinity and hard
    /// affinity don't contain any common CPUs). If the two masks conflict, the hard affinity wins.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn get_soft_cpu_affinity(self) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { self.as_ref() }.get_soft_cpu_affinity()
    }

    /// Sets the soft CPU affinity mask of the thread and returns its previous soft affinity mask.
    ///
    /// If a new mask is set, the thread will be migrated to satisfy the new constraint if possible.
    /// Soft affinity indicates where the thread should ideally be scheduled; the scheduler will
    /// respect the mask unless there are no other options (e.g., the soft affinity and hard
    /// affinity don't contain any common CPUs). If the two masks conflict, the hard affinity wins.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock, any scheduler queue lock, or have an active
    ///   `ChainLockTransaction` on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn set_soft_cpu_affinity(self, affinity: cpu_mask_t) -> cpu_mask_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`; `assert_canary_ptr`
        // does not form a `&Thread` that would live across `Scheduler::set_cpu_affinity`.
        unsafe { Thread::assert_canary_ptr(self.as_ptr()) };
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread` with no active
        // `ChainLockTransaction` or `self`'s lock held.
        unsafe {
            scheduler::Scheduler::set_cpu_affinity(
                self.as_raw(),
                affinity,
                scheduler::AffinityType::Soft,
            )
        }
    }

    /// Returns the total time the thread has spent running on a CPU (`Runtime`).
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn runtime(self) -> DurationMono {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { self.as_ref() }.runtime()
    }

    /// Returns the CPU the thread was last scheduled on (`LastCpu`).
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn last_cpu(self) -> cpu_num_t {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { self.as_ref() }.last_cpu()
    }

    /// Returns both the thread's total runtime and the last CPU it was scheduled on under a single
    /// acquisition of the thread's lock.
    ///
    /// # Safety
    ///
    /// - The caller must ensure that the thread pointer is still valid and the underlying thread
    ///   has not been destroyed.
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn runtime_and_last_cpu(self) -> (DurationMono, cpu_num_t) {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread`.
        unsafe { self.as_ref() }.runtime_and_last_cpu()
    }

    /// Returns a reference to this thread's [`ChainLock`].
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `self` points to a valid `Thread` which remains alive for as
    ///   long as the returned reference is used.
    /// - The caller must not use the returned [`ChainLock`] to release the lock or to create
    ///   overlapping guards (`ChainLockGuard::adopt`/`unguard`) while any
    ///   `ChainLockProof<'_, Thread>` for this thread is live (`ThreadLockGuard::proof`,
    ///   `OwnerGuard::proof`, `single_chain_lock_guard_owned!`).
    #[inline]
    pub unsafe fn get_lock(&self) -> &ChainLock {
        // SAFETY: `self` points to a valid `Thread` by the caller's guarantee.
        // `as_chain_lock`'s requirements about overlapping guards are forwarded to the caller.
        unsafe { self.as_ref().lock().as_chain_lock() }
    }

    /// Associates this thread with the given `ThreadDispatcher` (`Thread::SetUsermodeThread`).
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `self` points to a valid `Thread` in the
    ///   [`ThreadStateKind::Initial`] state with no `user_thread` attached yet, and that `self` is
    ///   never joined (as ownership of thread termination is transferred to `ThreadDispatcher`).
    /// - The caller must not hold `self`'s lock or have an active `ChainLockTransaction` on the
    ///   current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`).
    #[inline]
    pub unsafe fn set_usermode_thread(
        self,
        user_thread: fbl::RefPtr<crate::object::ThreadDispatcher>,
    ) {
        // SAFETY: Caller upholds `Thread::set_usermode_thread`'s preconditions on `self`.
        unsafe { self.as_ref().set_usermode_thread(user_thread) };
    }

    /// Returns the associated `ThreadDispatcher`, or null if this is a kernel-only thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn user_thread_raw(self) -> *mut crate::object::ThreadDispatcher {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::user_thread_ptr(self.as_ptr()) }
    }

    /// Returns the koid of the associated `ThreadDispatcher` for user threads or an independent
    /// koid for kernel threads.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn tid(self) -> zx_koid_t {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::tid_ptr(self.as_ptr()) }
    }

    /// Returns the koid of the associated `ProcessDispatcher` for user threads or
    /// `ZX_KOID_INVALID` for kernel threads.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn pid(self) -> zx_koid_t {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::pid_ptr(self.as_ptr()) }
    }

    /// Returns the last flow id allocated by [`ThreadPtr::take_next_lock_flow_id`] for this thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn lock_flow_id(self) -> u64 {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::lock_flow_id_ptr(self.as_ptr()) }
    }

    /// Returns a unique flow id for lock contention tracing (`TakeNextLockFlowId()`).
    ///
    /// The same value is returned by [`ThreadPtr::lock_flow_id`] until another id is allocated
    /// for this thread by calling this method again.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread`.
    #[inline]
    pub unsafe fn take_next_lock_flow_id(self) -> u64 {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { Thread::take_next_lock_flow_id_ptr(self.as_raw()) }
    }

    /// Generate a backtrace for the calling thread.
    ///
    /// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot be obtained, it
    /// will be left empty.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` that is blocked, sleeping, or
    /// suspended, and that the caller does not hold `self`'s thread lock or an active chain lock
    /// transaction.
    #[inline]
    pub unsafe fn get_backtrace(self, out_bt: &mut Backtrace) {
        // SAFETY: `self` points to a valid `Thread`, and the caller upholds the thread state and
        // locking preconditions.
        unsafe { self.as_ref() }.get_backtrace(out_bt);
    }

    /// Dump the information of a single thread. If `full` is true then a multi-line verbose dump of
    /// much information is performed, otherwise a single short summary line is dumped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` and that the caller does not
    /// hold `self`'s thread lock or an active chain lock transaction.
    #[inline]
    pub unsafe fn dump(self, full: bool) {
        // SAFETY: `self` points to a valid `Thread` and the caller upholds the locking
        // preconditions.
        unsafe { self.as_ref() }.dump(full);
    }

    /// Sets or clears the migrate callback invoked when this thread is migrated between CPUs or
    /// exits.
    ///
    /// The callback is invoked by the scheduler with interrupts disabled, an active
    /// `ChainLockTransaction`, and `self`'s thread lock held, so it must not block or re-acquire
    /// `self`'s thread lock.
    ///
    /// # Safety
    ///
    /// - [`thread_lists_init`] must have completed.
    /// - `self` must point to a valid `Thread` that has not been erased via
    ///   [`Thread::erase_from_lists_locked`].
    /// - The caller must not hold `self`'s thread lock, `Thread::get_list_lock()`, any scheduler
    ///   `queue_lock_`, or have an active `ChainLockTransaction` on the current CPU.
    #[inline]
    pub unsafe fn set_migrate_fn(self, migrate_fn: Option<MigrateFn>) {
        // SAFETY: `self` points to a valid `Thread`, and the caller upholds all preconditions of
        // `Thread::set_migrate_fn`.
        unsafe { self.as_ref().set_migrate_fn(migrate_fn) };
    }

    /// Sets or clears the context switch callback invoked when this thread switches context.
    ///
    /// The callback is invoked by the scheduler during context switch with interrupts disabled, an
    /// active `ChainLockTransaction`, and `self`'s thread lock held, so it must not block or
    /// re-acquire `self`'s thread lock.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid `Thread` and that the caller does not
    /// hold `self`'s thread lock or an active chain lock transaction.
    #[inline]
    pub unsafe fn set_context_switch_fn(self, context_switch_fn: Option<ContextSwitchFn>) {
        // SAFETY: `self` points to a valid `Thread`.
        unsafe { self.as_ref() }.set_context_switch_fn(context_switch_fn);
    }

    /// Sets or clears the restartable sequence (rseq) accessor for this thread.
    ///
    /// When installing a valid accessor, this also raises `THREAD_SIGNAL_CHECK_RSEQ` so the
    /// `cpu_id` field is written before returning to user mode.
    ///
    /// Because dropping the previous accessor may destroy its mapping when releasing the last
    /// reference, this may only be called from thread context where acquiring VM locks is safe.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid, live `Thread`, that this is called
    /// from thread context where acquiring VM locks is permitted, and that `self` is the currently
    /// executing thread (or that exclusive access to `self`'s `rseq_accessor_` is otherwise
    /// guaranteed).
    #[inline]
    pub unsafe fn set_rseq_accessor(self, accessor: crate::page_map::Accessor<'static, zx_rseq_t>) {
        let raw = self.as_raw();
        let is_valid = accessor.is_valid();
        // Disable interrupts across the `core::ptr::replace` so a timer preemption cannot enter
        // `Scheduler::context_switch` -> `signal_check_restartable_sequence_if_needed()` and form
        // `&Accessor` while `replace` holds `&mut Accessor` on the same slot. The guard is dropped
        // before `drop(old)` so the old accessor's `page_map::Entry` reference is released with
        // interrupts enabled in thread context.
        let old = {
            let _irq_guard = crate::arch_rs::InterruptDisableGuard::new();
            // SAFETY: Caller guarantees `self` points to a valid, live `Thread` with exclusive
            // access to `rseq_accessor` in thread context, and interrupts are disabled above.
            unsafe {
                let rseq_cell = &raw const (*raw).rseq_accessor;
                let slot = UnsafeCell::raw_get(rseq_cell);
                let old = core::ptr::replace(slot, ManuallyDrop::new(accessor));
                if is_valid {
                    // Set the thread signal to ensure we write out the cpu_id field before
                    // returning to user mode.
                    Thread::set_signals_ptr(raw, THREAD_SIGNAL_CHECK_RSEQ);
                }
                old
            }
        };
        drop(ManuallyDrop::into_inner(old));
    }

    /// Raises the thread signal to check the restartable sequence before returning to usermode if
    /// an rseq accessor is registered.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a valid, live `Thread`, and that `self` is the
    /// currently executing thread (or that exclusive access to `self`'s `rseq_accessor_` is
    /// otherwise guaranteed, as during a scheduler context switch to `self`).
    #[inline]
    pub unsafe fn signal_check_restartable_sequence_if_needed(self) {
        // SAFETY: Caller guarantees `self` points to a valid, live `Thread` with exclusive access
        // to `rseq_accessor`.
        unsafe {
            let rseq_cell = &raw const (*self.as_raw()).rseq_accessor;
            let rseq = &*UnsafeCell::raw_get(rseq_cell);
            if !rseq.is_valid() {
                return;
            }
            core::hint::cold_path();
            Thread::set_signals_ptr(self.as_raw(), THREAD_SIGNAL_CHECK_RSEQ);
        }
    }
}

// The C++ `page_map::Accessor<zx_rseq_t>` is two pointers.
static_assert!(
    size_of::<crate::page_map::Accessor<'static, zx_rseq_t>>() == 2 * size_of::<usize>()
);
static_assert!(align_of::<crate::page_map::Accessor<'static, zx_rseq_t>>() == align_of::<usize>());

/// Validated `(thread, tag)` target for [`thread_lock_guard!`].
///
/// Constructed exclusively via [`ThreadLockGuard::target`], which requires the caller to verify
/// the thread-lifetime, chain-lock transaction, and non-blocking preconditions inside an `unsafe`
/// block.
pub struct ThreadLockTarget {
    thread: NonNull<Thread>,
    tag: CltTag,
}

/// A thread's lock, held with interrupts disabled until the guard is dropped: the
/// `SingleChainLockGuard{IrqSaveOption, thread->get_lock(), tag}` that a C++ scope would hold.
///
/// Acquire via [`thread_lock_guard!`], which declares the hidden stack slot and guard together in
/// the caller's lexical scope so the slot cannot be overwritten or outlived and the guard cannot
/// be passed to `core::mem::forget`.
#[must_use = "if unused, the thread lock is released immediately"]
pub struct ThreadLockGuard<'slot> {
    _scope: SingleChainLockScope<'slot, 'static, IrqSave>,
    thread: NonNull<Thread>,
}

impl ThreadLockGuard<'_> {
    /// Validates `thread` and `tag` into a [`ThreadLockTarget`] for [`thread_lock_guard!`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `thread` is non-null and points to a valid `Thread` allocation that remains live until
    ///   the `ThreadLockGuard` constructed from the returned [`ThreadLockTarget`] has finished
    ///   dropping (not merely while the guard is in scope, since releasing the lock allows another
    ///   CPU to acquire it and free the `Thread`).
    /// - When passed to [`thread_lock_guard!`], the calling CPU is not already in a chain lock
    ///   transaction, none starts while the guard lives, and execution does not block on this CPU
    ///   before the guard is dropped.
    #[inline]
    pub unsafe fn target(thread: *mut Thread, tag: CltTag) -> ThreadLockTarget {
        debug_assert!(!thread.is_null());
        // SAFETY: Caller guarantees `thread` is non-null.
        ThreadLockTarget { thread: unsafe { NonNull::new_unchecked(thread) }, tag }
    }
}

impl<'slot> ThreadLockGuard<'slot> {
    /// Internal acquisition entry point used by [`thread_lock_guard!`].
    ///
    /// # Safety
    ///
    /// `slot` must be a stack local that outlives the returned guard and is not overwritten,
    /// moved, or deallocated before the guard is dropped, and the returned guard must be dropped
    /// (never `core::mem::forget`ted) on this CPU before anything that can block.
    #[doc(hidden)]
    #[inline]
    pub unsafe fn lock_target(
        slot: &'slot mut MaybeUninit<SingleChainLockGuard<'static, IrqSave>>,
        target: ThreadLockTarget,
    ) -> ThreadLockGuard<'slot> {
        let ThreadLockTarget { thread, tag } = target;
        // SAFETY:
        // - `Thread::lock_ptr(thread.as_ptr())`: projects only the `lock` field without forming a
        //   whole-struct `&Thread`.
        // - `ClassedChainLock::as_chain_lock()`: the returned `&ChainLock` is handed directly to
        //   `SingleChainLockGuard::new_in_place` and never escapes this function, so it is never
        //   used to release the lock outside `_scope` or create overlapping guards while this
        //   `ThreadLockGuard` or any proof minted from it is live.
        // - Lifetime extension to `&'static ChainLock`: `_scope` stores this reference privately
        //   and only dereferences it during lock acquisition and release in `Drop`; the caller of
        //   `ThreadLockGuard::target` guarantees `thread`'s allocation stays live until `Drop` has
        //   returned, and the reference is never exposed to callers.
        // - `SingleChainLockGuard::new_in_place`: `slot` is valid writable storage that stays
        //   pinned until the returned `SingleChainLockScope` drops on this CPU (never forgotten),
        //   and no chain lock transaction is active on this CPU per `ThreadLockGuard::target`'s
        //   contract.
        let scope = unsafe {
            let lock: &'static ChainLock = (*Thread::lock_ptr(thread.as_ptr())).as_chain_lock();
            SingleChainLockGuard::new_in_place(slot, IrqSave, lock, tag)
        };
        Self { _scope: scope, thread }
    }

    /// Returns the raw pointer to the locked thread.
    #[inline]
    pub fn thread(&self) -> *mut Thread {
        self.thread.as_ptr()
    }

    /// Returns a pointer to the architecture-specific state (`arch_thread`) of the locked thread.
    #[inline]
    pub fn arch(&self) -> *mut c_void {
        // SAFETY: `ThreadLockGuard::target`'s caller guarantees the thread is a live `Thread`.
        unsafe { get_arch(self.thread()) }
    }

    /// Checks whether user state is saved for the locked thread, like C++
    /// `Thread::IsUserStateSavedLocked()`.
    #[inline]
    pub fn is_user_state_saved_locked(&self) -> bool {
        // SAFETY: `self.thread` is a live `Thread` (`ThreadLockGuard::target`'s contract) whose
        // lock `_scope` acquired and holds until drop; the temporary proof does not escape this
        // call.
        unsafe { ChainLockProof::from_acquiring_guard(self.thread.as_ref()) }.user_state_saved()
    }

    /// Returns a reference to the [`SchedulerState`] of the locked thread.
    #[inline]
    pub fn scheduler_state(&self) -> &SchedulerState {
        // SAFETY: `ThreadLockGuard::target`'s caller guarantees the thread is a live `Thread`, and
        // this guard holds its lock.
        unsafe { scheduler_state(self.thread()) }
    }

    /// Mints a [`ChainLockProof`] proving that the locked thread's chain lock is held.
    #[inline]
    pub fn proof(&mut self) -> ChainLockProof<'_, Thread> {
        // SAFETY: `self.thread` points to a live `Thread` for the lifetime of this guard
        // (`ThreadLockGuard::target`'s contract), and `_scope` acquired (never adopted) its lock
        // and holds it until drop. The proof is bounded by the unique borrow of `self`.
        unsafe { ChainLockProof::from_acquiring_guard(self.thread.as_ref()) }
    }
}

/// Acquires the thread lock described by `$target` ([`ThreadLockTarget`]) with interrupts disabled
/// in a hidden stack slot and binds `$guard` as `&mut ThreadLockGuard<'_>` for the remainder of
/// the enclosing lexical scope.
///
/// `$target` is evaluated outside the macro's internal `unsafe` block, so callers must construct
/// it via `unsafe { ThreadLockGuard::target(thread, tag) }` with an explicit `// SAFETY:` comment.
///
/// # Example
/// ```ignore
/// // SAFETY: `thread` stays live past this scope, no CLT is active, and we do not block.
/// thread_lock_guard!(guard, unsafe { ThreadLockGuard::target(thread, clt_tag!("example")) });
/// let proof = guard.proof();
/// ```
#[macro_export]
macro_rules! thread_lock_guard {
    ($guard:ident, $target:expr $(,)?) => {
        let mut __thread_lock_guard_slot = ::core::mem::MaybeUninit::uninit();
        let __thread_lock_guard_target: $crate::kernel::thread::ThreadLockTarget = $target;
        // SAFETY: `__thread_lock_guard_scope` is bound in the same lexical scope immediately after
        // `__thread_lock_guard_slot` and cannot be named, moved, or passed to `core::mem::forget`
        // by the caller, ensuring it drops on this CPU before `__thread_lock_guard_slot` goes out
        // of scope. The thread-lifetime, CLT, and non-blocking preconditions were discharged when
        // constructing `__thread_lock_guard_target`.
        let mut __thread_lock_guard_scope = unsafe {
            $crate::kernel::thread::ThreadLockGuard::lock_target(
                &mut __thread_lock_guard_slot,
                __thread_lock_guard_target,
            )
        };
        #[allow(unused_variables)]
        let $guard: &mut $crate::kernel::thread::ThreadLockGuard<'_> =
            &mut __thread_lock_guard_scope;
    };
}
pub use thread_lock_guard;

/// Creates a thread with `name` that will execute `entry` at [`DEFAULT_PRIORITY`]. `arg` will be
/// passed to `entry` when executed, and the return value of `entry` will be passed to `Exit()`.
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or [`ThreadPtr::detach`], one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// # Safety
///
/// The caller must ensure that `name` is a valid NUL-terminated string, that `entry` and `arg`
/// are safe to run on a new thread, and that it may block and does not have an active
/// `ChainLockTransaction`.
pub unsafe fn create(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    // SAFETY: Forwarded from the caller.
    unsafe { create_with_priority(name, entry, arg, DEFAULT_PRIORITY) }
}

/// Kernel thread priority levels matching Zircon C++ definitions in `<kernel/thread.h>`.
pub const LOW_PRIORITY: i32 = 8;
pub const DEFAULT_PRIORITY: i32 = 16;
pub const HIGH_PRIORITY: i32 = 24;

/// Creates a thread with `name` that will execute `entry` at `priority`. `arg` will be passed to
/// `entry` when executed, and the return value of `entry` will be passed to `Exit()`
/// (`Thread::Create`).
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or [`ThreadPtr::detach`], one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// Thread priority is an integer from 0 (lowest) to 31 (highest). Standard priorities include
/// [`HIGH_PRIORITY`], [`DEFAULT_PRIORITY`], and [`LOW_PRIORITY`].
///
/// # Safety
///
/// The caller must ensure that `name` is a valid NUL-terminated string, that `entry` and `arg`
/// are safe to run on a new thread, and that it may block and does not have an active
/// `ChainLockTransaction`.
pub unsafe fn create_with_priority(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
    priority: i32,
) -> Result<ThreadPtr, Status> {
    // SAFETY: The caller guarantees `name` is a valid NUL-terminated string.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    // SAFETY: A null `t` makes `create_etc` allocate the `Thread`; the default trampoline runs
    // `entry`, which the caller guarantees is safe to run with `arg`; and the caller may block
    // with no active `ChainLockTransaction`.
    unsafe {
        create_etc(
            core::ptr::null_mut(),
            name,
            Some(entry),
            arg,
            &scheduler::BaseProfile::new_fair(priority),
            None,
        )
    }
}

/// Creates a thread with `name` that will execute `entry` with the given base `profile`. `arg`
/// will be passed to `entry` when executed, and the return value of `entry` will be passed to
/// `Exit()` (`Thread::Create`).
///
/// This call allocates a thread and places it in the global thread list. This memory will be freed
/// by either [`ThreadPtr::join`] or [`ThreadPtr::detach`], one of which MUST be called.
///
/// The thread will not be scheduled until [`ThreadPtr::resume`] is called.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread, and that it may
/// block and does not have an active `ChainLockTransaction`.
pub unsafe fn create_with_profile(
    name: &[u8],
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
    profile: &SchedulerStateBaseProfile,
) -> Result<ThreadPtr, Status> {
    // SAFETY: A null `t` makes `create_etc` allocate the `Thread`; the default trampoline runs
    // `entry`, which the caller guarantees is safe to run with `arg`; and the caller may block
    // with no active `ChainLockTransaction`.
    unsafe { create_etc(core::ptr::null_mut(), name, Some(entry), arg, profile, None) }
}

/// Create a new thread (`Thread::CreateEtc`).
///
/// This function creates a new thread.  The thread is initially suspended, so you
/// need to call [`ThreadPtr::resume`] to execute it.
///
/// - `t`: If not null, use the supplied Thread
/// - `name`: Name of thread
/// - `entry`: Entry point of thread
/// - `arg`: Arbitrary argument passed to entry(). It can be null.
/// - `profile`: Scheduling profile for the thread.
/// - `alt_trampoline`: If not `None`, an alternate trampoline for the thread to start on.
///
/// Returns the new thread, or an error on failure.
///
/// # Errors
///
/// Returns the error from growing the `OwnedWaitQueuePool` or initializing the thread's kernel
/// stack, or [`Status::NO_MEMORY`] if the `Thread` itself cannot be allocated.
///
/// # Safety
///
/// The caller must ensure that:
/// - `t` is either null or points to suitably aligned raw/unmanaged storage (e.g.
///   `MaybeUninit<Thread>`) valid for reads and writes of a `Thread`, holding no live value with
///   an automatic destructor, and not otherwise in use. If `t` is non-null: on `Ok`, the storage
///   must not be freed, moved, or reused until the returned thread has been destroyed in place by
///   [`ThreadPtr::join`], [`ThreadPtr::forget`], or (if detached) its exit path; on `Err`, `*t` is
///   either left untouched (if `OwnedWaitQueuePool::grow` fails) or constructed and already
///   destroyed in place (if `Thread::stack_init` fails), so the caller must not drop `*t`.
/// - `entry`, `arg`, and `alt_trampoline` are valid to execute on the new thread, and if
///   `alt_trampoline` is provided it upholds `Scheduler::TrampolineLockHandoff` and never returns.
/// - The caller may block and does not have an active `ChainLockTransaction`.
pub unsafe fn create_etc(
    t: *mut Thread,
    name: &[u8],
    entry: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    arg: *mut c_void,
    profile: &scheduler::BaseProfile,
    alt_trampoline: Option<unsafe extern "C" fn() -> !>,
) -> Result<ThreadPtr, Status> {
    // Ensure pool capacity for this thread. Do this before allocating
    // the thread itself to avoid complex unwinding.
    OwnedWaitQueuePool::get().grow()?;

    let mut flags: u32 = 0;

    let t = if let Some(t) = NonNull::new(t) {
        // assert that t is at least as aligned as the Thread is supposed to be
        debug_assert!(t.is_aligned());
        // SAFETY: The caller guarantees `t` is suitably aligned, pinned storage for a `Thread`
        // that is not otherwise in use.
        unsafe { Thread::init_in_place(t.as_ptr().cast::<MaybeUninit<Thread>>(), name) };
        t
    } else {
        flags |= THREAD_FLAG_FREE_STRUCT;
        let Some(t) = Thread::try_alloc(name) else {
            core::hint::cold_path();
            OwnedWaitQueuePool::get().shrink();
            return Err(Status::NO_MEMORY);
        };
        t
    };

    // save whether or not we need to free the thread struct and/or stack. This
    // must happen before anything below can fail, so that free_thread_resources
    // frees a heap-allocated thread struct on the error path.
    // SAFETY: `t` is a constructed `Thread` that no other thread can reach yet, and the temporary
    // `&Thread` ends with this statement.
    unsafe { t.as_ref().flags.store(flags) };

    {
        // Do not hold the thread's lock while we initialize it.  We are going to
        // perform some operations (like dynamic memory allocation) which will
        // require us to obtain potentially contested mutexes, something we cannot
        // safely do while holding any spinlocks, or chainlocks.
        //
        // Not holding the thread's lock here _should_ be OK.  We are basically in
        // the process of constructing the thread, no one else knows about this
        // memory just yet.  There are a limited number of ways that other CPUs in
        // the system could find out about this thread, and they should all involve
        // synchronizing via a lock which should establish proper memory ordering
        // semantics.
        //
        // 1) After we add the thread to the global thread list, debug code could
        //    find the thread, but only after synchronizing via the global thread
        //    list lock.
        // 2) After we add the thread to the scheduler (after create_etc), other CPUs
        //    can find out about it, but only after synchronizing via the
        //    scheduler's queue lock.
        //
        // `Scheduler::initialize_thread` and `arch_thread_initialize` nominally require the
        // thread's lock (`TA_REQ(thread->get_lock())`); exclusive ownership of the unpublished
        // thread while constructing it stands in for it here, as both of their contracts allow.

        // SAFETY: `t` is a constructed `Thread` that no other thread can reach yet, and no Rust
        // reference to it is live.
        unsafe { TaskState::init(&raw mut (*t.as_ptr()).task_state, entry, arg) };
        // SAFETY: As above; the caller owns the unpublished thread while constructing it.
        unsafe { scheduler::Scheduler::initialize_thread(t.as_ptr(), profile) };

        // SAFETY: As above; nothing else accesses the thread's `KernelStack`.
        if let Err(status) = unsafe { Thread::stack_init(t.as_ptr()) } {
            core::hint::cold_path();
            // SAFETY: `t` was constructed above and was never published.
            unsafe { free_thread_resources(t.as_ptr()) };
            return Err(status);
        }

        let alt_trampoline = match alt_trampoline {
            Some(alt_trampoline) => {
                core::hint::cold_path();
                alt_trampoline
            }
            None => rust_thread_trampoline,
        };

        // set up the initial stack frame
        // SAFETY: Early CPU 0 architectural initialization has completed (blocking is permitted per
        // `create_etc`'s contract, and `Thread::stack_init` succeeded above, allocating a
        // page-aligned kernel stack of `>= DEFAULT_STACK_SIZE` writable bytes and aligned
        // `unsafe_sp` / shadow-call stack); `t` is not yet visible to any scheduler and is uniquely
        // owned by the caller while constructing it; and `alt_trampoline` is a valid thread
        // trampoline that never returns.
        unsafe { crate::arch_rs::ops::thread_initialize(t.as_ptr(), alt_trampoline as usize) };
    }

    // SAFETY: `t` is a constructed `Thread`, which is only freed through `join`, `detach`, or
    // `forget` of the `ThreadPtr` returned below.
    let thread = unsafe { t.as_ref() };

    // Add it to the global thread list.  Be sure to grab the locks in the proper
    // order; global thread list lock first, then the thread's lock second.
    {
        ksync::lock!(let mut list_guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>());
        crate::kconcurrent::single_chain_lock_guard_owned!(
            thread_guard,
            thread,
            NO_IRQ_SAVE_OPTION,
            clt_tag!("Thread::CreateEtc")
        );
        // SAFETY: `thread_lists_init()` has completed (per `create_etc` running after early
        // thread initialization), `t` was just constructed so it is in no list, and it stays
        // allocated until it is erased from the list by `erase_from_lists_locked`.
        unsafe { thread_list(list_guard.as_mut().token_mut()).push_front_raw(t) };
    }

    THREAD_CREATE_COUNT.add(1);
    Ok(ThreadPtr(t))
}

/// Spawns a new kernel thread with default priority and resumes it.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread,
/// and that the thread is joined before any borrowed data in `arg` is destroyed.
pub unsafe fn spawn(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { create(name, entry, arg)? };
    unsafe { thread.resume() };
    Ok(thread)
}

/// Terminate the current thread.
///
/// Current thread exits with the specified return code.
///
/// This function does not return.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU's per-CPU state is initialized and `construct_first_thread` has completed,
///   with the current thread being a valid, live, non-idle [`Thread`] in
///   `ThreadStateKind::Running`.
/// - Blocking is permitted (`!blocking_disallowed()`), and no [`ChainLockTransaction`], thread
///   lock, `Thread::get_list_lock()`, scheduler lock, or spinlock is held on the calling CPU.
/// - If the current thread has an attached `user_thread` (`ThreadDispatcher`), interrupts are
///   enabled (`!ints_disabled()`).
pub unsafe fn current_exit(retcode: i32) -> ! {
    let current_thread = current_get();
    debug_assert!(!current_thread.is_null() && current_thread.is_aligned());
    // SAFETY: Caller guarantees `current_thread` points to a valid, live, non-idle `Thread`.
    unsafe { Thread::assert_canary_ptr(current_thread) };
    // SAFETY: `current_thread` is a valid, live `Thread`.
    debug_assert!(!unsafe { Thread::is_idle_ptr(current_thread) });

    // SAFETY: `current_thread` is a valid, live `Thread`, and its `user_thread` holds a strong
    // `fbl::RefPtr<ThreadDispatcher>` that stays alive until `Thread::drop` runs in
    // `free_thread_resources`.
    if let Some(user_thread) = unsafe { Thread::user_thread_ptr(current_thread).as_ref() } {
        debug_assert!(!crate::arch_rs::ints_disabled());
        // SAFETY: `user_thread` is the `ThreadDispatcher` of the currently exiting user thread,
        // which will never reenter userspace, and the caller guarantees interrupts are enabled,
        // blocking is permitted, and no spinlocks or chain locks are held.
        unsafe { user_thread.exiting_current() };
    }

    // Start by locking this thread, and setting the flag which indicates that it
    // can no longer be the owner of any OwnedWaitQueues in the system.  This will
    // ensure that the OWQ code no longer allows the thread to become the owner of
    // any queues.
    //
    let aapd = AnnotatedAutoPreemptDisabler::new();
    let _irqd = crate::arch_rs::InterruptDisableGuard::new();
    {
        // SAFETY: `current_thread` is the valid, live running thread; `&Thread` is scoped to this
        // block and does not escape to any callback or reschedule.
        let thread_ref: &Thread = unsafe { &*current_thread };
        single_chain_lock_guard_owned!(
            guard,
            thread_ref,
            NO_IRQ_SAVE_OPTION,
            clt_tag!("Thread::Current::Exit deny WQ ownership")
        );
        *guard.proof().can_own_wait_queues_mut() = false;
    }

    // Now that we can no longer own queues, we can disown any queues that we
    // currently happen to own.
    // SAFETY: `current_thread` is the exiting running thread with `can_own_wait_queues == false`
    // and is not blocked in any wait queue; no `ChainLockTransaction` is active.
    unsafe {
        OwnedWaitQueue::disown_all_queues(&*current_thread);
    }

    // Now, to complete the exit operation, we must be holding this thread's lock in
    // addition to the global thread list lock.
    let mut queue_free_dpc = false;
    {
        ksync::lock!(
            let mut list_guard = Thread::get_list_lock().lock_policy::<NoIrqSavePolicy>()
        );
        // SAFETY: Project only `current_thread`'s `lock` field without forming a whole-struct
        // `&Thread` across `call_migrate_fn_locked_ptr`, `try_wake_joiners`, or
        // `scheduler::reschedule_internal`. No `ChainLockProof`, `OwnerGuard`,
        // `OwnedSingleChainLockScope`, or `ThreadLockGuard` for `current_thread` is live, and the
        // returned `&ChainLock` is only passed to `single_chain_lock_guard!` below.
        let lock: &ChainLock = unsafe { (*Thread::lock_ptr(current_thread)).as_chain_lock() };
        single_chain_lock_guard!(
            guard,
            NO_IRQ_SAVE_OPTION,
            lock,
            clt_tag!("Thread::Current::Exit finish in list_guard")
        );

        // enter the dead state and finish any migration.
        scheduler::Scheduler::run_in_locked_current_scheduler(|_sched| {
            // SAFETY: `guard` holds `current_thread`'s lock in the active, finalized
            // `ChainLockTransaction` (`guard.token()`), and the current scheduler's lock is held.
            unsafe {
                let ss =
                    (*current_thread).scheduler_state.get().cast::<scheduler::SchedulerState>();
                (*ss).state = ThreadStateKind::Death;
            }
        });
        // SAFETY: `guard` holds `current_thread`'s lock exclusively; writing `retcode` via raw
        // field projection forms no `&Thread`.
        unsafe {
            let task_state = &raw const (*current_thread).task_state;
            (*task_state).retcode.get().write(retcode);
        }
        // SAFETY: `current_thread` is a valid, live exiting `Thread`, interrupts are disabled,
        // `guard` holds `current_thread`'s lock, `list_guard` holds `Thread::get_list_lock()`,
        // and no `&Thread` or conflicting reference is live across the callback.
        unsafe { Thread::call_migrate_fn_locked_ptr(current_thread, MigrateStage::Exiting) };

        // if we're detached, then do our teardown here
        // SAFETY: `current_thread` is valid and `flags` is an atomic field. `Relaxed` ordering is
        // sufficient because `THREAD_FLAG_DETACHED` and `THREAD_FLAG_FREE_STRUCT` are synchronized
        // by `current_thread`'s lock, which `guard` holds exclusively.
        let flags = unsafe {
            let flags_ptr = &raw const (*current_thread).flags;
            (*flags_ptr).load()
        };
        if (flags & THREAD_FLAG_DETACHED) != 0 {
            THREAD_DETACHED_EXIT_COUNT.add(1);

            // remove it from global lists and drop the global list lock.
            // SAFETY: `current_thread` is still alive and linked in `THREAD_LIST` (not yet
            // erased, no `SetMigrateFn` can run again), and `list_guard` holds
            // `Thread::get_list_lock()`. The temporary `&Thread` ends before `list_guard` is
            // released or `reschedule_internal` runs.
            unsafe {
                (*current_thread).erase_from_lists_locked(list_guard.as_mut().token_mut());
            }
            list_guard.as_mut().release();

            // Remember to queue a dpc to free the stack and, optionally, the thread
            // structure
            //
            // We are detached and will need to queue a DPC in order to clean up our
            // thread structure and stack after we are gone.  Queueing a DPC will
            // involve signaling the DPC thread, so we (technically) need to drop our
            // thread's lock and end our current chain lock transaction before doing
            // so.
            //
            // Preemption is disabled, interrupts are off, and DPC threads always run
            // on the CPU they were queued from.  Because of this, we can be sure that
            // the DPC thread is not going to preempt us and free our stack out from
            // under us before we manage to complete our final reschedule and context
            // switch.
            //
            // TODO(https://fxbug.dev/584127903): I cannot think of a way where waking up a DPC
            // thread should result in us deadlocking in a lock cycle.  Logically, our
            // current thread is no longer visible to anyone.
            //
            // 1) We are the currently running thread in a scheduler which cannot
            //    currently reschedule (until we drop into our final reschedule).
            // 2) We are not in any wait queues.
            // 3) We have become disconnected from our ThreadDispatcher and all
            //    handles referring to it.
            //
            // Waking the DPC thread would involve holding the DPC thread's lock and
            // its wait queues lock in order to remove the thread from the queue,
            // followed by holding our schedulers lock to insert the DPC thread.
            //
            // In _theory_ it should be OK to use our existing ChainLockTransaction to
            // queue the DPC while holding our thread's lock.  If we can prove that
            // this is safe, it may be a good idea to come back here later and queue
            // the DPC while holding the thread's lock, just to simplify the structure
            // of the code around here.
            // SAFETY: `current_thread` is valid and its stack is not concurrently initialized or
            // torn down while it is running and exiting.
            let has_stack_or_free_struct = unsafe { Thread::stack(current_thread).base() != 0 }
                || (flags & THREAD_FLAG_FREE_STRUCT) != 0;
            if has_stack_or_free_struct {
                queue_free_dpc = true;
            }
        } else {
            // Looks like we didn't need the global list lock after all.  Drop it and
            // signal anyone who is waiting to join us.
            list_guard.as_mut().release();

            // Our thread is both running, and exiting.  It is not a member of any
            // wait queue, and cannot be rescheduled (preemption is disabled).  It
            // should not be possible for it to be involved in any lock cycles, so we
            // are free to keep trying to wake our joiners until we finally succeed.
            //
            // Do not drop our thread's lock here, we *must* hold it until after the
            // final reschedule is complete.  Failure to do this means that a joiner
            // thread might wake up and free our stack out from under us before our
            // final reschedule.
            //
            // TODO(https://fxbug.dev/584127903): `TaskState::try_wake_joiners` may need to lock a
            // wait queue and more than one thread in order to wake the current set of joiners.
            // We have already finalized our transaction at this point, and are still holding
            // a lock.  Bending the rules here requires that we do something we really
            // don't want to be doing, which is to undo the Finalized state of the
            // transaction while we are still holding a lock.
            //
            // It would be *much* cleaner if we didn't have to do this.  We would
            // rather be in a position to drop our thread's lock and allow an
            // unconditional wake of our joiners instead.
            //
            // So, look into introducing a new lock (the thread cleanup spinlock?) which
            // could provide us the protection we would need.  The idea would be:
            //
            // 1) Earlier in this function, obtain this new cleanup lock.
            // 2) When we get to here, drop the thread's lock for the wakeup
            //    operation.
            // 3) Once wakeup is complete, re-obtain the thread's lock.
            // 4) Drop the cleanup lock.
            // 5) Enter the final reschedule.
            //
            // On the joiner/cleanup side of things, the last joiner out of the door
            // would just need to:
            //
            // 1) Obtain the cleanup lock.
            // 2) Obtain the thread's lock.
            // 3) Drop all locks.  We have now synced up with everyone else and should
            //    be free to cleanup the memory now.
            ChainLockTransaction::active_ref(guard.token())
                .restart(clt_tag!("Thread::Current::Exit wake joiners"), guard.token());
            // SAFETY: Project only the `task_state` field without forming a whole-struct
            // `&Thread`; this borrow ends at the close of this `else` block before
            // `scheduler::reschedule_internal` runs.
            let task_state: &TaskState = unsafe {
                let task_state_ptr = &raw const (*current_thread).task_state;
                &*task_state_ptr
            };
            // SAFETY: The active transaction was restarted above (not finalized), preemption is
            // disabled (`aapd.token()`), neither `task_state`'s queue lock nor any joiner's lock
            // is held, and no `&Thread` or lock proof for any joiner is live across this call.
            while !unsafe { task_state.try_wake_joiners(Ok(()), guard.token(), aapd.token()) } {
                core::hint::cold_path();
                // Note, we are still holding the current thread's lock, so we cannot
                // fully relax the active CLT (which would demand that we hold zero
                // locks).  See the note above about potentially finding a way to be
                // able to drop the current thread's lock here.
                libarch::r#yield();
            }
        }

        if !queue_free_dpc {
            // Drop into the final reschedule while still holding our lock.  There
            // should be no coming back from this.  We may have gotten here because we
            // were still joinable, or because we were detached but had a statically
            // allocated stack/thread-struct, but either way, we don't need to drop and
            // re-acquire our thread's lock in order to finish exiting at this point.
            // SAFETY: `current_thread` is the current running thread now in `Death` state, its
            // lock is the sole lock held in the finalized transaction `guard.token()` with no live
            // `&Thread` or `ChainLockProof` (only the projected `&ChainLock` guard), interrupts
            // are disabled (`_irqd`), `list_guard` has been released (`num_spinlocks_held() == 0`),
            // and `reschedule_internal` never returns.
            unsafe {
                scheduler::reschedule_internal(current_thread, guard.token());
            }
            panic!("somehow fell through thread_exit()");
        }
    }

    // It should not be possible to get to this point without needing to drop our
    // locks and queue a DPC to clean ourselves up.
    assert!(queue_free_dpc);
    extern "C" fn free_dpc_callback(dpc: &Dpc) {
        // SAFETY: Only passed to `Dpc::new` below for the detached exiting `current_thread`
        // (already unlinked from global lists, so nothing else may free it), and invoked by the
        // DPC worker thread on its local copy of `dpc`.
        unsafe { Thread::free_dpc(dpc) };
    }
    let free_dpc = Dpc::new(free_dpc_callback, NonNull::new(current_thread));
    // SAFETY: Preemption is disabled (`aapd`), interrupts are disabled (`_irqd`), and DPC worker
    // threads run on the CPU they were queued from, so `free_dpc` cannot be dequeued before we
    // enter `reschedule_internal` below and never return (keeping `free_dpc` pinned on this stack
    // until the DPC worker copies it out under `DpcLock` and synchronizes on `current_thread`'s
    // lock before freeing the stack).
    let status = unsafe { DpcRunner::enqueue(&free_dpc, QueueType::General) };
    debug_assert!(status.is_ok());

    // Relock our thread one last time before dropping into our final reschedule
    // operation.
    // SAFETY: Project only `current_thread`'s `lock` field without forming a whole-struct
    // `&Thread` across `scheduler::reschedule_internal`. No `ChainLockProof`, `OwnerGuard`,
    // `OwnedSingleChainLockScope`, or `ThreadLockGuard` for `current_thread` is live, and the
    // returned `&ChainLock` is only passed to `single_chain_lock_guard!` below.
    let lock: &ChainLock = unsafe { (*Thread::lock_ptr(current_thread)).as_chain_lock() };
    single_chain_lock_guard!(
        guard,
        NO_IRQ_SAVE_OPTION,
        lock,
        clt_tag!("Thread::Current::Exit final reschedule")
    );
    // SAFETY: `current_thread` is the current running thread in `Death` state, its lock is the
    // sole lock held in the finalized transaction `guard.token()` with no live `&Thread` or
    // `ChainLockProof` (only the projected `&ChainLock` guard), interrupts are disabled (`_irqd`),
    // `num_spinlocks_held() == 0`, and `reschedule_internal` never returns.
    unsafe {
        scheduler::reschedule_internal(current_thread, guard.token());
    }
    panic!("somehow fell through thread_exit()");
}

/// Marks the current thread for termination. The thread will actually terminate when the kernel
/// stack unwinds.
///
/// # Safety
///
/// - The calling CPU's current `Thread` pointer must be initialized (after `ConstructFirstThread`)
///   and point to a valid, live non-idle [`Thread`].
/// - Neither the current thread's lock nor an active `ChainLockTransaction` may be held on the
///   calling CPU (`TA_EXCL(chainlock_transaction_token, current_thread->get_lock())`).
pub unsafe fn current_kill() {
    // SAFETY: Caller guarantees `current_get()` points to a valid, live non-idle current `Thread`.
    let current = unsafe { &*current_get() };
    current.assert_canary();
    debug_assert!(!current.is_idle());
    crate::kconcurrent::single_chain_lock_guard_owned!(
        guard,
        current,
        IRQ_SAVE_OPTION,
        clt_tag!("Thread::Current::Kill")
    );
    debug_assert!(guard.proof().state() == ThreadStateKind::Running);
    current.set_signals(THREAD_SIGNAL_KILL);
}

/// Finish suspending the current thread.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU's current `Thread` pointer is initialized and points to a valid, running
///   non-idle thread.
/// - Interrupts are enabled (`!ints_disabled()`), blocking is permitted (`!blocking_disallowed()`),
///   and no spinlocks are held (`num_spinlocks_held() == 0`).
/// - Neither the current thread's lock nor an active `ChainLockTransaction` is held on the
///   current CPU (`TA_EXCL(chainlock_transaction_token, current_thread->get_lock())`).
unsafe fn current_do_suspend() {
    let current_thread = current_get();

    // SAFETY: `current_thread` is a valid, live `Thread`, and its `user_thread_` `RefPtr` (if any)
    // is immutable while `current_thread` is running and keeps the `ThreadDispatcher` alive.
    let user_thread = unsafe { Thread::user_thread_ptr(current_thread).as_ref() };

    // Note: After calling this callback, we must not return without
    // calling `user_thread.resuming()`.  That is because those callbacks act
    // as barriers which control when it is safe for the
    // zx_thread_read_state()/zx_thread_write_state() syscalls to access the
    // userland register state kept by Thread.
    if let Some(user_thread) = user_thread {
        debug_assert!(!crate::arch_rs::ints_disabled());
        user_thread.suspending();
    }

    let mut do_exit = false;
    {
        // Hold the thread's lock while we set up the suspend operation.
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Thread::Current::DoSuspend"),
            |token| {
                let should_suspend = {
                    // SAFETY: `current_thread` is the valid, live current thread. `current` and
                    // `guard` are scoped to this block so neither a `&Thread` nor an `OwnerGuard`
                    // is held across `scheduler::reschedule_internal` (which drops and
                    // re-acquires `current_thread`'s lock while suspended).
                    let current: &Thread = unsafe { &*current_thread };
                    let mut guard = OwnerGuard::new(current, token);
                    ChainLockTransaction::finalize(token);
                    let mut proof = guard.proof();

                    // Load `signals` once under the thread lock to check kill and suspend without
                    // a second atomic load.
                    let signals = current.signals();

                    // make sure we haven't been killed while the lock was dropped for the user
                    // callback
                    let suspend = if (signals & THREAD_SIGNAL_KILL) != 0 {
                        core::hint::cold_path();
                        // Ensure we don't recurse into thread_exit.
                        debug_assert!(proof.state() != ThreadStateKind::Death);
                        // Exit the thread, but only after we have dropped our locks and
                        // re-enabled IRQs.
                        do_exit = true;
                        false
                    }
                    // Make sure the suspend signal wasn't cleared while we were running the
                    // callback.
                    else if (signals & THREAD_SIGNAL_SUSPEND) != 0 {
                        scheduler::Scheduler::run_in_locked_current_scheduler(|_sched| {
                            proof.set_suspended();
                        });
                        current.clear_signals(THREAD_SIGNAL_SUSPEND);
                        true
                    } else {
                        false
                    };
                    let _ = guard.into_unguarded();
                    suspend
                };

                if should_suspend {
                    // directly invoke the context switch, since we've already manipulated
                    // this thread's state. Note that our thread's lock will be dropped and
                    // re-acquired as we block and later become re-scheduled.
                    //
                    // SAFETY: `current_thread` is the valid, running current thread whose state
                    // was set to `Suspended` above, its lock is held as the sole lock in the
                    // finalized transaction `token` with no live `&Thread`, guard, or proof,
                    // interrupts are disabled by `IRQ_SAVE_OPTION`, and the caller guarantees
                    // `num_spinlocks_held() == 0` and `!blocking_disallowed()`.
                    unsafe {
                        scheduler::reschedule_internal(current_thread, token);
                    }

                    // If the thread was killed while we were suspended, we should not allow
                    // it to resume.  We shouldn't call `user_thread.resuming()` in this
                    // case, because there might not have been any request to resume the
                    // thread.
                    {
                        // SAFETY: By `scheduler::reschedule_internal`'s postcondition,
                        // `current_thread` is valid and its lock is re-acquired as the sole lock
                        // in `token`.
                        let current: &Thread = unsafe { &*current_thread };
                        current.lock().assert_held(token);
                        // SAFETY: By `scheduler::reschedule_internal`'s postcondition,
                        // `current.lock()` is held under `token` with no conflicting
                        // `ChainLockProof` live.
                        let proof = unsafe { ChainLockProof::assume_held_unchecked(current) };
                        if proof.check_kill_signal() {
                            core::hint::cold_path();
                            // Exit the thread, but only after we have dropped our locks and
                            // re-enabled IRQs.
                            do_exit = true;
                        }
                    }
                }

                // SAFETY: `current_thread`'s lock is held under `token` (either from the initial
                // acquisition or re-acquired by `scheduler::reschedule_internal`), and no
                // `&Thread`, guard, or proof is live.
                unsafe { (*Thread::lock_ptr(current_thread)).as_chain_lock().release(token) };
                DONE
            },
        );
    }

    if do_exit {
        // SAFETY: Interrupts are enabled, blocking is permitted, no locks or transactions are
        // held, and the current thread is a live running non-idle thread.
        unsafe { current_exit(0) };
    }

    if let Some(user_thread) = user_thread {
        debug_assert!(!crate::arch_rs::ints_disabled());
        user_thread.resuming();
    }
}

/// The current thread's [`PreemptionState`] (`Thread::Current::preemption_state()`), as a pointer
/// to the field alone so that no `&Thread` is formed.
///
/// The returned pointer remains valid for dereferencing as long as the calling thread continues
/// executing (for example, while preemption is disabled on the calling CPU).
///
/// # Safety
///
/// The calling CPU's current `Thread` pointer must have been initialized (i.e., after
/// `ConstructFirstThread` on the calling CPU).
#[inline]
pub unsafe fn current_preemption_state() -> *const PreemptionState {
    let current = current_get();
    // SAFETY: Caller guarantees `current` is the valid, live current thread; projecting the field
    // through the raw pointer forms no reference.
    unsafe { &raw const (*current).preemption_state }
}

/// Formats `"<prefix><cpu_num>"` into a 16-byte buffer, truncating to at most 15 bytes plus a
/// trailing NUL byte, and returns the formatted byte slice (excluding the NUL terminator).
#[cold]
#[inline(never)]
fn format_cpu_name<'a>(buf: &'a mut [u8; 16], prefix: &[u8], mut cpu_num: cpu_num_t) -> &'a [u8] {
    const { assert!(core::mem::size_of::<cpu_num_t>() == core::mem::size_of::<u32>()) };
    const MAX_LEN: usize = 15;

    let mut len = 0usize;
    for (&src, dst) in prefix.iter().zip(buf[..MAX_LEN].iter_mut()) {
        *dst = src;
        len += 1;
    }

    let mut digits = [0u8; 10];
    let mut start = digits.len();
    loop {
        start -= 1;
        // `digits` holds up to 10 decimal digits (`u32::MAX` is 10 digits), so `start < 10` on
        // every iteration.
        if let Some(slot) = digits.get_mut(start) {
            *slot = b'0' + (cpu_num % 10) as u8;
        }
        cpu_num /= 10;
        if cpu_num == 0 {
            break;
        }
    }

    if let Some(rest) = buf[..MAX_LEN].get_mut(len..)
        && let Some(digit_slice) = digits.get(start..)
    {
        for (&src, dst) in digit_slice.iter().zip(rest.iter_mut()) {
            *dst = src;
            len += 1;
        }
    }

    // `len <= MAX_LEN < buf.len()` is guaranteed by the zipped slices over `&mut buf[..MAX_LEN]`.
    if let Some(nul) = buf.get_mut(len) {
        *nul = 0;
    }
    buf.get(..len).unwrap_or(&[])
}

/// Thread entry point wrapper for secondary CPUs' per-CPU idle/power threads.
///
/// # Safety
///
/// Must be invoked exclusively as the entry point of a per-CPU kernel-only idle/power thread
/// (installed by [`create_idle_thread`] and verified by [`revive_idle_power_thread`]).
unsafe extern "C" fn idle_power_thread_entry(arg: *mut c_void) -> i32 {
    // SAFETY: Caller guarantees this runs exclusively as the entry point of a per-CPU kernel-only
    // idle/power thread.
    unsafe { IdlePowerThread::run(arg) }
}

/// Sets up the initial [`Thread`] for each CPU from its running state.
///
/// For the boot CPU (`IS_BOOT_CPU == true`), this must take care not to touch any global locks
/// when invoked by early init code that runs before global ctors are called. `THREAD_LIST` is
/// safe to mutate before global ctors are run.
///
/// For secondary CPUs (`IS_BOOT_CPU == false`), this must use normal locking to guard both this
/// [`Thread`] object and `THREAD_LIST`.
///
/// # Safety
///
/// - Interrupts must be disabled (`ints_disabled()`).
/// - On x86-64, `%gs.base` must be initialized to the calling CPU's `x86_percpu`; on ARM64 and
///   RISC-V, the architectural thread pointer must still point to the early-boot fake `arch_thread`
///   (or `t` itself on RISC-V secondary CPUs).
/// - `t` must be non-null, properly aligned to `align_of::<Thread>()`, and point to valid writable
///   storage for a [`Thread`] dedicated to the calling CPU (not concurrently accessed on any other
///   CPU and not already in `THREAD_LIST`) that remains pinned and live for as long as it stays
///   installed as the current thread / in `THREAD_LIST`.
/// - [`thread_lists_init`] must have completed.
/// - On the boot CPU (`IS_BOOT_CPU == true`), this must be called single-threaded on CPU 0 during
///   early boot after `PerCpu::initialize_boot()`, before global constructors or secondary CPUs.
/// - On a secondary CPU (`IS_BOOT_CPU == false`), `PerCpu::initialize_secondary_finish()` must
///   have completed on this CPU, and neither `Thread::get_list_lock()` nor any
///   `ChainLockTransaction` may be held.
unsafe fn construct_first_thread<const IS_BOOT_CPU: bool>(t: *mut Thread, name: &[u8]) {
    debug_assert!(crate::arch_rs::ints_disabled());
    debug_assert!(!t.is_null() && t.is_aligned());

    // TODO(https://fxbug.dev/584127903): On secondary CPUs (`IS_BOOT_CPU == false`), x86 and
    // riscv64 initialize `t`'s `KernelStack` before calling `secondary_cpu_init_early`, and
    // `init_in_place` overwrites `stack` with `KernelStack::default()` without tearing down the
    // prior `KernelStack` (matching the pre-existing C++ placement-new `new (t) Thread(name)`
    // behavior; secondary CPU bootstrap threads are immortal). arm64 keeps each secondary CPU's
    // stack outside its `Thread`, so `t`'s is empty there. Clean this up in a follow-up once the
    // C++ port is complete.
    // SAFETY: Caller guarantees `t` points to properly aligned, writable, pinned storage for a
    // `Thread` dedicated to this CPU that is not concurrently accessed and not in any intrusive
    // list; any prior `KernelStack` state on secondary CPUs is intentionally overwritten in place
    // without running its destructor (see TODO above).
    unsafe { Thread::init_in_place(t.cast::<MaybeUninit<Thread>>(), name) };

    // Install the real thread pointer before any code that may consult
    // Thread::Current::Get(). Until this point the thread pointer refers to a
    // minimal fake arch_thread set up by early boot code (so that stack-protector
    // and safe-stack code can run), which is not a valid Thread.
    // SAFETY: `t` was just constructed above and remains live while installed as this CPU's
    // current thread, interrupts are disabled, and caller guarantees early arch per-CPU /
    // thread-pointer state is valid for this CPU.
    unsafe { crate::arch_rs::ops::thread_construct_first(t) };

    let finish_init = |list_token: &mut LockToken<'_, ThreadListLockClass>| {
        // `Relaxed` ordering suffices because `t` is not yet published in `THREAD_LIST` or visible
        // to any other CPU until the locks below are released (or secondary CPUs start).
        // SAFETY: `t` is this CPU's newly constructed initial thread (never joined); raw field
        // projection avoids forming a whole-struct `&Thread` across `initialize_first_thread(t)`.
        unsafe { (*t).flags.fetch_or(THREAD_FLAG_DETACHED) };

        // Set up the scheduler state.
        // SAFETY: Interrupts are disabled, `t` is this CPU's newly constructed initial `Thread`,
        // no whole-struct `&Thread` is live, the current CPU's `queue_lock_` is not held, and
        // either we are single-threaded on the boot CPU before global ctors or `_thread_guard`
        // holds `t`'s lock in an active `ChainLockTransaction`.
        unsafe { scheduler::Scheduler::initialize_first_thread(t) };

        // Start out with preemption disabled to avoid attempts to reschedule until
        // threading is fully enabled.  This simplifies code paths shared between
        // initialization and runtime (e.g. logging).  Preemption is enabled when the
        // idle thread for the current CPU is ready.
        // SAFETY: `t` was just constructed in place above; raw field projection avoids forming a
        // whole-struct `&Thread`.
        unsafe { (*t).preemption_state.preempt_disable() };

        // Push onto the global thread list after the Thread has been fully initialized (and, for
        // secondary CPUs, with all the locks still held).
        // SAFETY: `thread_lists_init()` has completed, `t` is a valid pinned static/per-CPU
        // `Thread` not currently in `THREAD_LIST`, and `list_token` either represents exclusive
        // early-boot CPU 0 access or proves `Thread::get_list_lock()` is held.
        unsafe { thread_list(list_token).push_front_raw(NonNull::new_unchecked(t)) };
    };

    if IS_BOOT_CPU {
        // Appear to take all the locks that the secondary CPUs actually will, without touching
        // any global locks before global ctors are called.
        // SAFETY: Boot CPU early init runs single-threaded on CPU 0 before secondary CPUs exist.
        let mut list_token = unsafe { LockToken::<ThreadListLockClass>::new() };
        finish_init(&mut list_token);
    } else {
        // The guard members actually hold all the necessary locks: global thread list lock first,
        // then the thread's lock.
        ksync::lock!(let mut list_guard = Thread::get_list_lock().lock_policy::<IrqSavePolicy>());
        // SAFETY: Project only `t`'s `lock` field without forming a whole-struct `&Thread` across
        // `initialize_first_thread(t)` or `push_front_raw`. No `ChainLockProof`, `OwnerGuard`,
        // `OwnedSingleChainLockScope`, or `ThreadLockGuard` for `t` is live, and the returned
        // `&ChainLock` is only passed to `single_chain_lock_guard!`.
        let lock: &ChainLock = unsafe { (*Thread::lock_ptr(t)).as_chain_lock() };
        single_chain_lock_guard!(
            _thread_guard,
            NO_IRQ_SAVE_OPTION,
            lock,
            clt_tag!("thread_construct_first")
        );
        finish_init(list_guard.as_mut().token_mut());
    }
}

/// Initializes the threading system.
///
/// This function is called once, from `lk_main()`.
///
/// # Safety
///
/// Must be called at most once, on the boot CPU (`curr_cpu_num() == 0`) with interrupts disabled
/// and early arch thread-pointer / `%gs.base` state initialized, during single-threaded early boot
/// before global constructors, threading, or secondary CPUs are active.
#[inline(never)]
pub unsafe fn init_early() {
    debug_assert!(crate::arch_rs::curr_cpu_num() == 0);

    // Initialize the thread lists. This needs to be done manually now, since initial thread code
    // manipulates the lists before global constructors are run.
    // SAFETY: Called once during single-threaded early boot on CPU 0 before any threads are
    // inserted into `THREAD_LIST` or `MIGRATE_LIST`.
    unsafe { thread_lists_init() };

    // Init the boot percpu data.
    // SAFETY: Called once on CPU 0 with interrupts disabled during single-threaded early boot
    // prior to any concurrent per-CPU accesses.
    unsafe { PerCpu::initialize_boot() };

    // create a thread to cover the current running state
    // SAFETY: `PerCpu::initialize_boot()` just initialized CPU 0's `PerCpu`.
    let t = unsafe { PerCpu::get(0) }.idle_power_thread.thread_ptr();
    // SAFETY: Interrupts are disabled, early arch thread-pointer / `%gs.base` state is valid,
    // `t` points to CPU 0's pinned, uninitialized `'static` `IdlePowerThread` `Thread` storage
    // inside `PerCpu`, `thread_lists_init()` and `PerCpu::initialize_boot()` have completed, and
    // we are running single-threaded on CPU 0 before global constructors or secondary CPUs.
    unsafe { construct_first_thread::<true>(t, b"bootstrap") };
}

/// Creates an idle thread for a secondary CPU.
///
/// # Safety
///
/// `cpu_num` must be in `1..SMP_MAX_CPUS` and `< PerCpu::processor_count()`, and this function
/// must be called at most once per secondary CPU before that CPU starts executing, from a context
/// where blocking is permitted with no active `ChainLockTransaction`.
pub unsafe fn create_idle_thread(cpu_num: cpu_num_t) -> Option<ThreadPtr> {
    debug_assert!(cpu_num != 0 && (cpu_num as usize) < SMP_MAX_CPUS);

    let mut name_buf = [0u8; 16];
    let name = format_cpu_name(&mut name_buf, b"idle ", cpu_num);

    let profile = scheduler::BaseProfile::new_fair(IDLE_PRIORITY);
    // SAFETY: Caller guarantees `cpu_num < PerCpu::processor_count()` and this CPU's idle thread
    // has not yet been created.
    let idle_thread_ptr = unsafe { PerCpu::get(cpu_num) }.idle_power_thread.thread_ptr();
    // SAFETY: `idle_thread_ptr` points to aligned, default-constructed `Thread` storage inside
    // `'static` `PerCpu` that lives forever (never destroyed automatically) and is initialized at
    // most once per secondary CPU; `idle_power_thread_entry` is valid for this per-CPU idle/power
    // thread; and blocking is permitted with no active `ChainLockTransaction`.
    let Ok(t) = (unsafe {
        create_etc(
            idle_thread_ptr,
            name,
            Some(idle_power_thread_entry),
            core::ptr::null_mut(),
            &profile,
            None,
        )
    }) else {
        return None;
    };

    {
        let t_raw = t.as_raw();
        // SAFETY: Project only `t_raw`'s `lock` field without forming a whole-struct `&Thread`
        // across `unblock_idle(t_raw)`. `t` was just constructed by `create_etc` and has not been
        // started yet, no `ChainLockProof` or guard for it is live, and the returned `&ChainLock`
        // is only passed to `single_chain_lock_guard!`.
        let lock: &ChainLock = unsafe { (*Thread::lock_ptr(t_raw)).as_chain_lock() };
        single_chain_lock_guard!(
            _guard,
            IRQ_SAVE_OPTION,
            lock,
            clt_tag!("Thread::CreateIdleThread")
        );

        // `Relaxed` ordering suffices because `flags` is published by `_guard` and `queue_lock` in
        // `unblock_idle` before any CPU can observe this idle thread.
        // SAFETY: `_guard` holds `t_raw`'s lock exclusively in an active `ChainLockTransaction`,
        // and raw field projections avoid forming a whole-struct `&Thread` across `unblock_idle`.
        unsafe {
            (*t_raw).flags.fetch_or(THREAD_FLAG_IDLE | THREAD_FLAG_DETACHED);
            let ss = (*t_raw).scheduler_state.get().cast::<scheduler::SchedulerState>();
            (*ss).hard_affinity = cpu_num_to_mask(cpu_num);
            // SAFETY: `_guard` holds `t_raw`'s lock with interrupts disabled in an active
            // `ChainLockTransaction`, `THREAD_FLAG_IDLE` and single-CPU `hard_affinity` were just
            // set, and the target scheduler's `queue_lock_` is not held.
            scheduler::unblock_idle(t_raw);
        }
    }
    Some(t)
}

/// Revives the idle/power thread for the given CPU so that it starts fresh after halting while
/// taking the CPU offline. Most state is preserved, including stack memory locations (useful for
/// debugging) and accumulated runtime stats.
///
/// # Safety
///
/// - `cpu_num` must be in `1..SMP_MAX_CPUS` and `< PerCpu::processor_count()`.
/// - `cpu_num`'s idle/power thread must have been created by [`create_idle_thread`], must
///   currently be halted (not running on any CPU) after taking `cpu_num` offline, and no
///   `PreemptDisabledToken` for it may be live.
/// - No `ChainLockTransaction` may be active on the calling CPU
///   (`TA_EXCL(chainlock_transaction_token)`).
#[cold]
unsafe fn revive_idle_power_thread(cpu_num: cpu_num_t) {
    debug_assert!(cpu_num != 0 && (cpu_num as usize) < SMP_MAX_CPUS);
    // SAFETY: Caller guarantees `cpu_num < PerCpu::processor_count()` and its idle/power `Thread`
    // was previously created and is currently halted.
    let thread_ptr = unsafe { PerCpu::get(cpu_num) }.idle_power_thread.thread_ptr();
    // SAFETY: `thread_ptr` points to the per-CPU idle/power `Thread` constructed at boot, which
    // lives forever.
    let thread = unsafe { &*thread_ptr };

    single_chain_lock_guard_owned!(
        guard,
        thread,
        IRQ_SAVE_OPTION,
        clt_tag!("Thread::ReviveIdlePowerThread")
    );

    debug_assert!(thread.is_idle());
    debug_assert!(guard.proof().hard_affinity() == cpu_num_to_mask(cpu_num));
    debug_assert!(thread.task_state.entry().is_some_and(|e| {
        core::ptr::fn_addr_eq(
            e,
            idle_power_thread_entry as unsafe extern "C" fn(*mut c_void) -> i32,
        )
    }));

    // SAFETY: Early CPU 0 architectural initialization has completed, `thread_ptr` is a live,
    // halted per-CPU idle/power `Thread` whose lock is held exclusively by `guard` with an
    // initialized, aligned kernel stack, and `rust_thread_trampoline` is a valid non-returning
    // trampoline.
    unsafe {
        crate::arch_rs::ops::thread_initialize(
            thread_ptr,
            rust_thread_trampoline as *const () as usize,
        );
    }
    // SAFETY: Caller guarantees the idle/power thread is halted (not running on any CPU), `guard`
    // holds its thread lock exclusively, and no `PreemptDisabledToken` for it is live.
    unsafe { thread.preemption_state.reset() };
}

/// The last routine called on the secondary CPU's bootstrap thread.
///
/// # Safety
///
/// Must be called at the end of a secondary CPU's bringup on its initial bootstrap thread after
/// [`secondary_cpu_init_early`] has completed and its idle/power thread has been created via
/// [`create_idle_thread`], with `blocking_disallowed()` true and no locks or
/// `ChainLockTransaction` held.
pub unsafe fn secondary_cpu_entry() -> ! {
    debug_assert!(crate::arch_rs::blocking_disallowed());

    // SAFETY: This secondary CPU's scheduler has been initialized, `queue_lock_` is not held, and
    // this bootstrap thread is hard-pinned to the current CPU (so it cannot migrate).
    unsafe { scheduler::Scheduler::set_curr_cpu_active(true) };

    let current_cpu = PerCpu::get_current();

    // Signal the idle/power thread to transition to active but don't wait for it, since it cannot
    // run until this thread either blocks or exits below. The idle thread will run immediately
    // upon exit and complete the transition, if necessary.
    // SAFETY: `current_cpu.idle_power_thread` is initialized, this CPU's bootstrap `Thread` is
    // initialized, blocking is permitted because `Scheduler::set_curr_cpu_active(true)` above
    // cleared `blocking_disallowed()`, and no spinlocks or `ChainLockTransaction` are held.
    let result = unsafe {
        current_cpu.idle_power_thread.transition_offline_to_active(ZX_TIME_INFINITE_PAST)
    };

    // The first time a secondary CPU becomes active after boot the CPU power thread is already in
    // the active state. If the CPU power thread is not in its initial active state, it is being
    // returned to active from offline and needs to be revived to resume its normal function.
    if result.starting_state != IdlePowerState::Active {
        // SAFETY: `curr_cpu_num()` is a secondary CPU in `1..SMP_MAX_CPUS` and
        // `< PerCpu::processor_count()` whose idle/power thread was created by
        // `create_idle_thread`, halted while going offline (so it is not running on any CPU and
        // has no live `PreemptDisabledToken`), and no `ChainLockTransaction` is active.
        unsafe { revive_idle_power_thread(crate::arch_rs::curr_cpu_num()) };
    }

    // CAREFUL: This must happen after the idle/power thread is revived, since creating the DPC
    // thread can contend on VM locks and could cause this CPU to go idle.
    current_cpu.dpc_runner.init_for_current_cpu();

    // Remove ourselves from the Scheduler's bookkeeping.
    {
        let current_ptr = current_get();
        // SAFETY: Project only `current_ptr`'s `lock` field without forming a whole-struct
        // `&Thread` across `remove_first_thread(current_ptr)`. No `ChainLockProof`, `OwnerGuard`,
        // `OwnedSingleChainLockScope`, or `ThreadLockGuard` for `current_ptr` is live, and the
        // returned `&ChainLock` is only passed to `single_chain_lock_guard!`.
        let lock: &ChainLock = unsafe { (*Thread::lock_ptr(current_ptr)).as_chain_lock() };
        single_chain_lock_guard!(
            _guard,
            IRQ_SAVE_OPTION,
            lock,
            clt_tag!("thread_secondary_cpu_entry")
        );
        // SAFETY: `_guard` holds `current_ptr`'s lock with interrupts disabled in an active
        // `ChainLockTransaction`, `current_ptr` is this CPU's active first thread, no
        // whole-struct `&Thread` is live, and `queue_lock_` is not held.
        unsafe { scheduler::Scheduler::remove_first_thread(current_ptr) };
    }

    crate::kernel::mp::signal_curr_cpu_ready();

    // Exit from our bootstrap thread, and enter the scheduler on this cpu.
    // SAFETY: This secondary CPU's per-CPU state is initialized, `construct_first_thread` has
    // completed, the current bootstrap thread is a valid live non-idle kernel-only `Thread` (no
    // `user_thread`) in `ThreadStateKind::Running`, blocking is permitted (enabled by
    // `set_curr_cpu_active(true)`), and no locks or `ChainLockTransaction` are held.
    unsafe { current_exit(0) }
}

/// Becomes an idle thread.
///
/// This function marks the current thread as the idle thread -- the one which executes when there
/// is nothing else to do. This function does not return. This is called once at boot on the first
/// CPU.
///
/// # Safety
///
/// Interrupts must be disabled, the caller must be the boot CPU's initial bootstrap thread (with
/// the boot CPU already marked active in the scheduler and preemption disabled once by
/// `construct_first_thread`), and no locks or `ChainLockTransaction` may be held.
pub unsafe fn current_become_idle() -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());

    let t = current_get();
    let curr_cpu = crate::arch_rs::curr_cpu_num();

    {
        // Hold our own lock while we fix up our bookkeeping
        // SAFETY: Project only `t`'s `lock` field without forming a whole-struct `&Thread` across
        // `remove_first_thread(t)` or `unblock_idle(t)`, which mutate `t`'s `scheduler_state` via
        // raw pointers derived from `t`. No `ChainLockProof`, `OwnerGuard`,
        // `OwnedSingleChainLockScope`, or `ThreadLockGuard` for `t` is live, and the returned
        // `&ChainLock` is only passed to `single_chain_lock_guard!`.
        let lock: &ChainLock = unsafe { (*Thread::lock_ptr(t)).as_chain_lock() };
        single_chain_lock_guard!(
            _thread_guard,
            NO_IRQ_SAVE_OPTION,
            lock,
            clt_tag!("Thread::Current::BecomeIdle")
        );

        // Set our name
        let mut name_buf = [0u8; 16];
        let name = format_cpu_name(&mut name_buf, b"idle ", curr_cpu);
        // SAFETY: `t` is the boot CPU's live initial bootstrap thread, and `set_name_in_place`
        // writes only `(*t).name` via raw field projection without forming `&Thread`.
        unsafe { Thread::set_name_in_place(t, name) };

        // Mark ourselves as idle
        // `Relaxed` ordering suffices because `flags` is published by `_thread_guard` and
        // `queue_lock` in `unblock_idle` before any other CPU can observe this thread as idle.
        // SAFETY: `t` is the boot CPU's dedicated per-CPU idle/power thread transitioning to idle
        // once at boot; raw field projection avoids forming a whole-struct `&Thread`.
        unsafe { (*t).flags.fetch_or(THREAD_FLAG_IDLE) };

        // Now that we are the idle thread, make sure that we drop out of the
        // scheduler's bookkeeping altogether.
        // SAFETY: Interrupts are disabled, `_thread_guard` holds `t`'s lock in an active
        // `ChainLockTransaction`, `t` is the current CPU's active first thread (now marked idle
        // with single-CPU hard affinity from `initialize_first_thread`), no whole-struct `&Thread`
        // is live, and the current CPU's `queue_lock_` is not held.
        unsafe {
            scheduler::Scheduler::remove_first_thread(t);
            scheduler::unblock_idle(t);
            let ss = (*t).scheduler_state.get().cast::<scheduler::SchedulerState>();
            (*ss).state = ThreadStateKind::Running;
        }

        // Cpu must be marked active by now, indicate that it is idle as well.
        debug_assert!(scheduler::peek_is_active(curr_cpu));
        // SAFETY: `PerCpu::get_current().scheduler` has had `this_cpu` initialized during early
        // boot and interrupts are disabled.
        unsafe { PerCpu::get_current().scheduler.set_idle(true) };

        // Pend a preemption to ensure a reschedule.
        crate::arch_rs::set_blocking_disallowed(true);
        // SAFETY: `t` is the live current thread; raw field projection avoids forming `&Thread`.
        unsafe { (*t).preemption_state.preempt_set_pending(cpu_num_to_mask(curr_cpu)) };
        crate::arch_rs::set_blocking_disallowed(false);
    }

    // Re-enable interrupts, re-enable preemption, drop into our idle routine, and
    // never return.
    enable_ints();
    // SAFETY: `t` is the boot CPU's `'static` idle/power thread; `preempt_reenable()` pairs with
    // the `preempt_disable()` called in `construct_first_thread` for this initial thread, with no
    // `PreemptDisabledToken` live.
    unsafe { (*t).preemption_state.preempt_reenable() };
    // SAFETY: `t` is the boot CPU's live idle/power thread.
    debug_assert!(unsafe { (*t).preemption_state.preempt_is_enabled() });

    // SAFETY: This thread is now CPU 0's per-CPU kernel-only idle/power thread.
    unsafe { IdlePowerThread::run(core::ptr::null_mut()) }
}

/// Increments the preempt disable counter for the current thread.
///
/// While preempt disable is non-zero, preemption of the thread is disabled, including preemption
/// from interrupt handlers. During this time, any call to `Reschedule()` will only record that a
/// reschedule is pending, and won't do a context switch.
///
/// Note: [`preempt_disable`] does not preclude a thread from blocking (e.g., waiting on a lock).
/// Disabling preemption does not prevent switching away from the current thread if it blocks.
///
/// A call to [`preempt_disable`] must be matched by a later call to [`preempt_reenable`] to
/// decrement the preempt disable counter.
#[inline]
pub fn preempt_disable() {
    // SAFETY: The current thread is alive for as long as code runs on it, and `PreemptionState`
    // is all relaxed atomics, so a shared reference to the field is sound while C++ and interrupt
    // handlers mutate it.
    unsafe { (*current_preemption_state()).preempt_disable() }
}

/// Decrements the preempt disable counter and flushes any pending local preemption operation.
///
/// Callers must ensure that they are calling from a context where blocking is allowed, as the call
/// may result in the immediate preemption of the calling thread.
///
/// # Safety
///
/// The call must pair with a prior, still unmatched [`preempt_disable`] on the current thread,
/// and it must not drop preemption back to enabled while any [`PreemptDisabledToken`] is live.
#[inline]
pub unsafe fn preempt_reenable() {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` is by construction the
    // current thread's preemption state, and the pairing and token obligations are forwarded
    // from the caller.
    unsafe { (*current_preemption_state()).preempt_reenable() }
}

/// Returns the pending preemption CPU mask on the current thread's `PreemptionState`
/// (`PreemptionState::preempts_pending`).
#[inline]
pub fn preempts_pending() -> cpu_mask_t {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` points to the current thread's
    // `PreemptionState`, which consists solely of relaxed atomics.
    unsafe { (*current_preemption_state()).preempts_pending() }
}

/// Returns the pending preemption CPU mask on `thread`'s `PreemptionState`
/// (`PreemptionState::preempts_pending`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live `Thread`.
#[inline]
pub unsafe fn thread_preempts_pending(thread: *const Thread) -> cpu_mask_t {
    // SAFETY: Caller guarantees `thread` points to a live `Thread`, and `PreemptionState` consists
    // solely of relaxed atomics.
    unsafe { (*Thread::preemption_state_ptr(thread)).preempts_pending() }
}

/// Adds `mask` to the pending preemption CPU mask on the current thread's `PreemptionState`
/// (`PreemptionState::preempts_pending_add`).
#[inline]
pub fn preempts_pending_add(mask: cpu_mask_t) {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` points to the current thread's
    // `PreemptionState`, which consists solely of relaxed atomics.
    unsafe { (*current_preemption_state()).preempts_pending_add(mask) }
}

/// Adds `mask` to the pending preemption CPU mask on `thread`'s `PreemptionState`
/// (`PreemptionState::preempts_pending_add`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live `Thread`.
#[inline]
pub unsafe fn thread_preempts_pending_add(thread: *mut Thread, mask: cpu_mask_t) {
    // SAFETY: Caller guarantees `thread` points to a live `Thread`, and `PreemptionState` consists
    // solely of relaxed atomics.
    unsafe { (*Thread::preemption_state_ptr(thread)).preempts_pending_add(mask) }
}

/// Increments the eager resched disable counter for the current thread.
///
/// When eager resched disable is non-zero, issuing local and remote preemptions is disabled,
/// including from interrupt handlers. During this time, any call to `Reschedule()` or other
/// scheduler entry points that imply a reschedule will only record the pending reschedule for the
/// affected CPU, but will not perform reschedule IPIs or a local context switch.
///
/// As with [`preempt_disable`], blocking operations are still allowed while eager resched disable
/// is non-zero.
///
/// A call to [`eager_resched_disable`] must be matched by a later call to
/// [`eager_resched_reenable`] to decrement the eager resched disable counter.
#[inline]
pub fn eager_resched_disable() {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).eager_resched_disable() }
}

/// Decrements the eager resched disable counter and flushes pending local and/or remote
/// preemptions if enabled, respectively.
///
/// # Safety
///
/// The call must pair with a prior, still unmatched [`eager_resched_disable`] on the current
/// thread, and it must not drop preemption back to enabled while any [`PreemptDisabledToken`] is
/// live.
#[inline]
pub unsafe fn eager_resched_reenable() {
    // SAFETY: As in `preempt_disable`, `current_preemption_state()` is by construction the
    // current thread's preemption state, and the pairing and token obligations are forwarded
    // from the caller.
    unsafe { (*current_preemption_state()).eager_resched_reenable() }
}

/// Returns the current thread's preemption disable count.
#[inline]
pub fn preempt_disable_count() -> u32 {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).preempt_disable_count() }
}

/// Returns the current thread's eager resched disable count.
#[inline]
pub fn eager_resched_disable_count() -> u32 {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).eager_resched_disable_count() }
}

/// Marks a pending preemption for the current CPU on the current thread's preemption state.
///
/// This is similar to `Reschedule()`, except that it may only be used inside an interrupt handler
/// while interrupts and preemption are disabled, between [`preempt_disable`] and
/// [`preempt_reenable`]. Unlike `Reschedule()`, it does not need to be called with the thread's
/// lock held.
///
/// Supplies the C++ default argument `cpu_num_to_mask(arch_curr_cpu_num())`.
#[inline]
pub fn preempt_set_pending() {
    // SAFETY: As in `preempt_disable`.
    unsafe {
        (*current_preemption_state()).preempt_set_pending(crate::kernel::types::cpu_num_to_mask(
            crate::arch_rs::curr_cpu_num(),
        ))
    }
}

/// Marks a pending preemption for `reschedule_mask` on the current thread's preemption state.
#[inline]
pub fn preempt_set_pending_mask(reschedule_mask: cpu_mask_t) {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).preempt_set_pending(reschedule_mask) }
}

/// Returns `true` if preemption is enabled for the current thread.
#[inline]
pub fn preempt_is_enabled() -> bool {
    // SAFETY: As in `preempt_disable`.
    unsafe { (*current_preemption_state()).preempt_is_enabled() }
}

/// Zero-sized capability token proving that preemption is disabled on the current thread.
///
/// Corresponds to C++ `preempt_disabled_token`. This token is `!Send` and `!Sync` because
/// preemption state is CPU- and thread-local.
#[derive(Debug)]
pub struct PreemptDisabledToken {
    // Safety invariant: Preemption is disabled on the current thread (`!preempt_is_enabled()`) for
    // the lifetime of this token, and it is `!Send` and `!Sync`.
    _private: PhantomData<*mut ()>,
}

zr::static_assert!(size_of::<PreemptDisabledToken>() == 0);
zr::static_assert!(align_of::<PreemptDisabledToken>() == 1);

impl PreemptDisabledToken {
    /// Creates a new capability token proving that preemption is disabled on the current thread.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that preemption is currently disabled on the current thread
    /// (`!preempt_is_enabled()`) and remains disabled for the lifetime of the returned token.
    #[inline]
    pub const unsafe fn new() -> Self {
        Self { _private: PhantomData }
    }

    /// Asserts in debug builds that preemption is currently disabled on the current thread.
    #[inline]
    pub fn assert_held(&self) {
        debug_assert!(!preempt_is_enabled());
    }
}

/// Sets a timeslice extension if one is not already set.
///
/// This function should only be called in normal thread context.
///
/// Returns `false` if a timeslice extension was already present or if the supplied duration is
/// `<= 0`.
///
/// Note: It is OK to call this from a context where preemption is (hard) disabled. If preemption
/// is requested while the preempt disable count is non-zero and a timeslice extension is in place,
/// the extension will be activated, but preemption will not occur until the count has dropped to
/// zero and the extension has expired or has been cleared.
#[inline]
pub fn preempt_set_timeslice_extension(duration: DurationMono) -> bool {
    // SAFETY: As in `preempt_disable`, and `current_preemption_state()` is by construction the
    // current thread's non-null, aligned preemption state.
    unsafe {
        NonNull::new_unchecked(current_preemption_state().cast_mut())
            .as_ref()
            .set_timeslice_extension(duration.0)
    }
}

/// Unconditionally clears any timeslice extension.
///
/// This function must be called in normal thread context because it may trigger local preemption.
#[inline]
pub fn preempt_clear_timeslice_extension() {
    // SAFETY: As in `preempt_disable`, and `current_preemption_state()` is by construction the
    // current thread's non-null, aligned preemption state.
    unsafe {
        NonNull::new_unchecked(current_preemption_state().cast_mut())
            .as_ref()
            .clear_timeslice_extension()
    }
}

pub use super::auto_preempt_disabler::{
    AnnotatedAutoEagerReschedDisabler, AnnotatedAutoPreemptDisabler, AutoEagerReschedDisabler,
    AutoExpiringPreemptDisabler, AutoPreemptDisabler,
};

/// RAII helper to enforce that a block of code does not allocate memory.
///
/// See `Thread::Current::memory_allocation_state()`.
pub struct ScopedMemoryAllocationDisabled {
    _not_send_sync: PhantomData<*mut ()>,
}

impl ScopedMemoryAllocationDisabled {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // SAFETY: `ScopedMemoryAllocationDisabled` is only constructed on an initialized thread
        // and is `!Send`, so `drop` balances this `disable()` on the same thread.
        unsafe { (*current_memory_allocation_state()).disable() };
        Self { _not_send_sync: PhantomData }
    }
}

impl Drop for ScopedMemoryAllocationDisabled {
    fn drop(&mut self) {
        // SAFETY: Balances the `disable()` call in `ScopedMemoryAllocationDisabled::new` on the
        // same thread.
        unsafe { (*current_memory_allocation_state()).enable() };
    }
}

/// RAII class to mark sections of code as memory stalls.
///
/// Nesting is not allowed. It is a programming error to instantiate this class
/// if there exists another instance on the call stack.
pub struct ScopedMemoryStall {
    // The stall is accounted to the thread that created it, so the guard must not be sent to (and
    // dropped on) another thread.
    _not_send_sync: PhantomData<*mut ()>,
}

impl ScopedMemoryStall {
    /// Marks the current thread as experiencing a memory stall until the guard is dropped.
    pub fn new() -> Self {
        let current_thread = current_get();

        let _preempt_disable = AutoPreemptDisabler::new();

        // SAFETY: `current_thread` is the running thread, so it is alive, and preemption is
        // disabled by `_preempt_disable`, which guards its `memory_stall_state_`.
        let state = unsafe { memory_stall_state(current_thread) };
        if state == ThreadStallState::IgnoredKernelOnly {
            return Self { _not_send_sync: PhantomData };
        }

        debug_assert!(state == ThreadStallState::Progressing);
        // SAFETY: As above, and only the current thread writes its own stall state while
        // preemption is disabled. `current_thread` is running on this CPU, so its `last_cpu` is
        // this CPU's number, as required when entering `ThreadStallState::Stalling`.
        unsafe { set_memory_stall_state(current_thread, ThreadStallState::Stalling) };
        crate::kernel::percpu::PerCpu::get_current().memory_stall_accumulator.update(-1, 1);
        Self { _not_send_sync: PhantomData }
    }
}

impl Drop for ScopedMemoryStall {
    fn drop(&mut self) {
        let current_thread = current_get();

        let _preempt_disable = AutoPreemptDisabler::new();

        // SAFETY: `current_thread` is the running thread, so it is alive, and preemption is
        // disabled by `_preempt_disable`, which guards its `memory_stall_state_`.
        let state = unsafe { memory_stall_state(current_thread) };
        if state == ThreadStallState::IgnoredKernelOnly {
            return;
        }

        debug_assert!(state == ThreadStallState::Stalling);
        // SAFETY: As above, and only the current thread writes its own stall state while
        // preemption is disabled.
        unsafe { set_memory_stall_state(current_thread, ThreadStallState::Progressing) };
        crate::kernel::percpu::PerCpu::get_current().memory_stall_accumulator.update(1, -1);
    }
}

/// RAII helper that installs/removes an exception context and saves/restores user register state.
/// The struct operates on the current thread.
///
/// When a thread takes an exception, this struct is used to make user register state available to
/// debuggers and exception handlers.
///
/// Note, `ScopedThreadExceptionContext` keeps track of whether the state has already been saved so
/// it's safe to nest calls to [`with_exception_context`]:
///
/// ```ignore
/// fn foo() {
///     with_exception_context(context, || {
///         bar();
///     });
/// }
///
/// fn bar() {
///     with_exception_context(context, || {
///         baz();
///     });
/// }
/// ```
///
/// Kept private behind [`with_exception_context`] so safe code cannot call `core::mem::forget` on
/// the guard and leave `Thread::arch().suspended_general_regs` dangling into popped stack memory.
struct ScopedThreadExceptionContext {
    // Safety invariant: points to the valid, currently executing user `Thread` on the calling CPU
    // (`NonNull<Thread>` is `!Send + !Sync`).
    thread: NonNull<Thread>,
    need_to_remove: bool,
    need_to_restore: bool,
}

impl ScopedThreadExceptionContext {
    /// Installs `context` and saves user state on the current thread.
    ///
    /// # Safety
    ///
    /// Same requirements as [`with_exception_context`].
    unsafe fn new(context: &crate::arch_rs::ArchExceptionContext) -> Self {
        // SAFETY: Caller guarantees `current_get()` is a valid, non-null running user `Thread`.
        let thread = unsafe { NonNull::new_unchecked(current_get()) };
        // SAFETY: `thread` is the valid, live current user thread.
        let thread_ref = unsafe { thread.as_ref() };
        let (need_to_remove, need_to_restore) = {
            crate::kconcurrent::single_chain_lock_guard_owned!(
                guard,
                thread_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("ScopedThreadExceptionContext")
            );
            let mut proof = guard.proof();

            // It's possible that the context and state have been installed/saved earlier in the
            // call chain. If so, then it's some other object's responsibility to remove/restore.
            //
            // SAFETY: `thread` is the valid current thread with its lock held, and the caller
            // guarantees `context` (and any non-null `context.frame`) is valid and outlives this
            // guard.
            let need_to_remove =
                unsafe { crate::arch_rs::arch_install_exception_context(thread.as_ptr(), context) };
            // SAFETY: Interrupts are disabled by `IRQ_SAVE_OPTION`, `thread` is the current user
            // thread with initialized user-state storage, and `proof` proves its lock is held.
            let need_to_restore = unsafe { proof.save_user_state_locked(thread.as_ptr()) };
            (need_to_remove, need_to_restore)
        };
        Self { thread, need_to_remove, need_to_restore }
    }
}

impl Drop for ScopedThreadExceptionContext {
    fn drop(&mut self) {
        let thread = self.thread;
        let need_to_restore = self.need_to_restore;
        let need_to_remove = self.need_to_remove;
        // SAFETY: By `self.thread`'s safety invariant, `thread` points to the valid, live current
        // user thread on this CPU.
        let thread_ref = unsafe { thread.as_ref() };
        crate::kconcurrent::single_chain_lock_guard_owned!(
            guard,
            thread_ref,
            IRQ_SAVE_OPTION,
            clt_tag!("~ScopedThreadExceptionContext")
        );
        let mut proof = guard.proof();

        // Did we save the state?  If so, then it's our job to restore it.
        if need_to_restore {
            // SAFETY: Interrupts are disabled by `IRQ_SAVE_OPTION`, and `need_to_restore` is true
            // only when `save_user_state_locked()` in `ScopedThreadExceptionContext::new` saved
            // the user state on `thread`.
            unsafe { proof.restore_user_state_locked(thread.as_ptr()) };
        }
        // Did we install the exception context? If so, then it's our job to remove it.
        if need_to_remove {
            // SAFETY: `need_to_remove` is true only when `arch_install_exception_context` in
            // `ScopedThreadExceptionContext::new` installed the exception context on `thread`.
            unsafe { crate::arch_rs::arch_remove_exception_context(thread.as_ptr()) };
        }
    }
}

/// Installs `context` on the current user thread for the duration of `f`, making its user register
/// state available to debuggers and exception handlers while the thread is in an exception.
///
/// # Safety
///
/// * `current_get()` must point to a valid, initialized running user [`Thread`]
///   (`!user_thread().is_null()`).
/// * `context` must be a valid, initialized `arch_exception_context_t` for the current thread.
/// * If `context.frame` is non-null, it must point to a valid, initialized, writable `iframe_t`
///   that outlives this call with no conflicting Rust references while installed.
/// * Neither the current thread's lock nor an active `ChainLockTransaction` may be held on the
///   current CPU when entering `with_exception_context` or when `f` returns (or unwinds), and `f`
///   must not leave the current thread's saved user state or installed exception context
///   unbalanced when `f` returns (or unwinds).
#[inline]
pub unsafe fn with_exception_context<R>(
    context: &crate::arch_rs::ArchExceptionContext,
    f: impl FnOnce() -> R,
) -> R {
    // SAFETY: Forwarded from the caller of `with_exception_context`.
    let guard = unsafe { ScopedThreadExceptionContext::new(context) };
    let result = f();
    drop(guard);
    result
}

pub use crate::kernel::wait_queue::Interruptible;

/// Yield the CPU to another thread (`Thread::Current::Yield`).
///
/// This function places the current thread at the end of the run queue and yields the CPU to
/// another waiting thread (if any).
///
/// This function will return at some later time. Possibly immediately if no other threads are
/// waiting to execute.
///
/// # Panics
///
/// Must be called from a non-idle thread context after threading is initialized, with blocking
/// allowed (`!blocking_disallowed()`), no spinlocks held, and no active [`ChainLockTransaction`]
/// on the current CPU.
pub fn r#yield() {
    let current_thread = current_get();

    // SAFETY: `current_thread` is the valid, live thread currently executing on this CPU.
    unsafe { Thread::assert_canary_ptr(current_thread) };
    // This is a safe fn, and both preconditions are decided by the caller, so they are checked in
    // all builds rather than trusted. The spinlock check also covers `queue_lock_`, which is a
    // spinlock and must not be held when entering the scheduler; nothing else enforces that in
    // release builds.
    assert!(
        !crate::arch_rs::blocking_disallowed() && crate::arch_rs::ops::num_spinlocks_held() == 0
    );

    // SAFETY: Project only the `lock` field without forming a whole-struct `&Thread` across the
    // context switch inside `scheduler::yield_thread`.
    let lock: &ChainLock = unsafe { (*Thread::lock_ptr(current_thread)).as_chain_lock() };
    single_chain_lock_guard!(_guard, IRQ_SAVE_OPTION, lock, clt_tag!("Thread::Current::Yield"));
    // SAFETY: `current_thread` is locked by `_guard`.
    debug_assert!(unsafe { is_running(current_thread) });

    crate::kernel::stats::inc_yields();
    // SAFETY: `current_thread` is the running thread on this CPU with its lock held as the only
    // chain lock in the active transaction, interrupts are disabled by `IRQ_SAVE_OPTION`, and no
    // `&Thread` or `&mut Thread` is live across the call. The assert above established that
    // blocking is allowed and that no spinlock, and therefore no `queue_lock_`, is held.
    unsafe { scheduler::yield_thread(current_thread) };
}

/// Preempt the current thread from an interrupt (`Thread::Current::Preempt`).
///
/// This function places the current thread at the head of the run queue and then yields the CPU to
/// another thread.
///
/// # Panics
///
/// Must be called from thread context after threading is initialized, with blocking allowed
/// (`!blocking_disallowed()`) and no active [`ChainLockTransaction`] on the current CPU.
pub fn preempt() {
    let current_thread = current_get();

    // SAFETY: `current_thread` is the valid, live thread currently executing on this CPU.
    unsafe { Thread::assert_canary_ptr(current_thread) };
    debug_assert!(!crate::arch_rs::blocking_disallowed());

    // SAFETY: `current_thread` is valid and `flags` is an atomic field.
    if !unsafe { Thread::is_idle_ptr(current_thread) } {
        // only track when a meaningful preempt happens
        crate::kernel::stats::inc_preempts();
    }

    // SAFETY: `current_thread` was verified above with blocking allowed, and no chain locks are
    // held on the current CPU.
    unsafe { scheduler::preempt(scheduler::PreemptType::Irq) };
}

/// Reevaluate the run queue on the current CPU.
///
/// This function places the current thread at the head of the run queue and then yields the CPU to
/// another thread. Similar to [`preempt`], but intended to be used at non-interrupt context.
///
/// # Panics
///
/// Must be called from thread context after threading is initialized, with blocking allowed
/// (`!blocking_disallowed()`) and no active [`ChainLockTransaction`] on the current CPU.
pub fn reschedule() {
    let current_thread = current_get();

    // SAFETY: `current_thread` is the valid, live thread currently executing on this CPU.
    unsafe { Thread::assert_canary_ptr(current_thread) };
    // This is a safe fn, and whether blocking is allowed here is decided by the caller, so the
    // precondition is checked in all builds rather than trusted.
    assert!(!crate::arch_rs::blocking_disallowed());

    // SAFETY: Project only the `lock` field without forming a whole-struct `&Thread` across the
    // context switch inside `scheduler::reschedule`.
    let lock: &ChainLock = unsafe { (*Thread::lock_ptr(current_thread)).as_chain_lock() };
    single_chain_lock_guard!(
        _guard,
        IRQ_SAVE_OPTION,
        lock,
        clt_tag!("Thread::Current::Reschedule")
    );

    // SAFETY: `current_thread` is locked by `_guard`.
    debug_assert!(unsafe { is_running(current_thread) });
    // SAFETY: `current_thread` is the running thread on this CPU with its lock held as the only
    // chain lock in the active transaction, interrupts are disabled by `IRQ_SAVE_OPTION`, and no
    // `&Thread` or `&mut Thread` is live across the call. Blocking is allowed per the assert
    // above.
    unsafe { scheduler::reschedule(current_thread) };
}

/// Timer callback to wake up a sleeping thread.
///
/// # Safety
///
/// `timer` must point to a valid [`Timer`] in an active timer callback context, `arg` must be a
/// non-null pointer to the valid, live [`Thread`] associated with `timer` (which remains live
/// until either `timer.trylock_or_cancel` observes cancellation or [`ThreadState::unsleep`]
/// releases its lock), and no [`ChainLockTransaction`] may be active on the current CPU.
unsafe extern "C" fn sleep_handler(timer: *mut Timer, now: zx_instant_mono_t, arg: *mut c_void) {
    debug_assert!(!timer.is_null());
    debug_assert!(!arg.is_null());
    // SAFETY: The caller guarantees `timer` points to a valid `Timer` in an active timer
    // callback, and all mutable `Timer` state uses interior mutability.
    let timer = unsafe { &*timer };
    // SAFETY: The caller guarantees `arg` is a non-null pointer to a valid, live `Thread`.
    let t = unsafe { ThreadPtr::from_raw(arg.cast::<Thread>()).unwrap_unchecked() };
    // SAFETY: Forwarded from the timer callback preconditions.
    unsafe { Thread::handle_sleep(t, timer, InstantMono(now)) };
}

const MIN_SLEEP_SLACK: DurationMono = DurationMono::from_micros(1);
const MAX_SLEEP_SLACK: DurationMono = DurationMono::from_seconds(1);
const DIV_SLEEP_SLACK: i64 = 10;

/// Computes the amount of slack the `thread_sleep` timer will use.
fn sleep_slack(deadline: InstantMono, now: InstantMono) -> DurationMono {
    if deadline < now {
        return MIN_SLEEP_SLACK;
    }
    let slack = DurationMono((deadline - now).0 / DIV_SLEEP_SLACK);
    MIN_SLEEP_SLACK.max(slack.min(MAX_SLEEP_SLACK))
}

/// Put thread to sleep; deadline specified in ns.
///
/// This function puts the current thread to sleep until the specified `deadline` has occurred.
///
/// Note that this function could continue to sleep after the specified deadline if other threads
/// are running. When the deadline occurs, this thread will be placed at the head of the run queue.
///
/// `interruptible` argument allows this routine to return early if the thread was signaled
/// for something (`ZX_ERR_INTERNAL_INTR_RETRY`), or killed (`ZX_ERR_INTERNAL_INTR_KILLED`).
///
/// # Panics
///
/// Must be called from a non-idle thread context after threading is initialized, with blocking
/// allowed (`!blocking_disallowed()`), no spinlocks held, and no active [`ChainLockTransaction`]
/// on the current CPU.
pub fn sleep_etc(
    deadline: &Deadline,
    interruptible: Interruptible,
    now: InstantMono,
) -> Result<(), Status> {
    let current_thread_ptr = current_get();
    // SAFETY: `current_thread_ptr` is the valid, running `Thread` on this CPU and stays alive for
    // the entire duration of `sleep_etc` (the current thread cannot exit while inside `sleep_etc`).
    let current_thread = unsafe { &*current_thread_ptr };

    current_thread.assert_canary();
    debug_assert!(!current_thread.is_idle());
    debug_assert!(!crate::arch_rs::blocking_disallowed());

    // Skip all of the work if the deadline has already passed.
    if deadline.when().0 <= now.0 {
        return Ok(());
    }

    pin_init::stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
    let timer = timer.as_ref();
    let final_blocked_status;
    {
        // SAFETY: `current_thread.lock()` is only acquired by `guard` here; no overlapping
        // `OwnerGuard` or `ChainLockProof` for `current_thread` exists on the stack.
        single_chain_lock_guard!(
            guard,
            IRQ_SAVE_OPTION,
            unsafe { current_thread.lock().as_chain_lock() },
            clt_tag!("Thread::Current::SleepEtc")
        );
        let clt_token = guard.token();
        // SAFETY: `guard` just acquired `current_thread.lock()` for the active transaction, and no
        // other guard or proof for `current_thread` is live.
        let mut proof = unsafe { ChainLockProof::assume_held_unchecked(current_thread) };
        debug_assert!(proof.state() == ThreadStateKind::Running);

        // if we've been killed and going in interruptible, abort here
        if interruptible == Interruptible::Yes {
            let signals = current_thread.signals();
            if signals != 0 {
                core::hint::cold_path();
                if signals & THREAD_SIGNAL_KILL != 0 {
                    return Err(ZX_ERR_INTERNAL_INTR_KILLED);
                } else {
                    return Err(ZX_ERR_INTERNAL_INTR_RETRY);
                }
            }
        }

        // set a one shot timer to wake us up and reschedule
        // SAFETY: `sleep_handler` is a valid timer callback for `current_thread_ptr`, `timer` is
        // pinned on the stack, and `timer.cancel()` is called below before `timer` is dropped.
        unsafe {
            timer.set_deadline(deadline, sleep_handler, current_thread_ptr.cast::<c_void>());
        }

        proof.set_sleeping();
        let wq_state = proof.wait_queue_state();
        // SAFETY: `current_thread` is the currently executing thread on this CPU in
        // `ThreadStateKind::Sleeping`, its lock is the only chain lock held by the finalized
        // transaction `clt_token`, and blocking is permitted on the current CPU.
        unsafe { wq_state.block(&mut proof, interruptible, Ok(()), clt_token) };

        // Once we wake up again, we will be holding the current thread's lock once
        // again. That said, the lock had been dropped, and was re-acquired while we
        // were blocked. Stash our `blocked_status` while we are still holding our lock,
        // but make sure to drop the lock before we call into `timer.cancel()`.
        //
        // When we call into `timer.cancel()`, if we discover a timer callback in
        // flight, we are going to spin until the timer callback flags itself as
        // completed, ensuring that it is safe for the `Timer` on our stack to be
        // destroyed. The timer callback is going to need this lock as well, if it
        // gets run, however. So, if we are still holding our lock when we call
        // cancel, we risk deadlock.
        //
        // always cancel the timer, since we may be racing with the timer tick on
        // other CPUs
        final_blocked_status = wq_state.blocked_status(proof.token());
    }
    let _ = timer.cancel();
    final_blocked_status
}

/// Non-interruptible version of [`sleep_etc`] (`Thread::Current::Sleep`).
pub fn sleep(deadline: InstantMono) -> Result<(), Status> {
    let now = current_mono_time();
    sleep_etc(&Deadline::no_slack(deadline.into()), Interruptible::No, now)
}

/// Non-interruptible relative delay version of [`sleep`] (`Thread::Current::SleepRelative`).
pub fn sleep_relative(delay: DurationMono) -> Result<(), Status> {
    let now = current_mono_time();
    let deadline = Deadline::no_slack((now + delay).into());
    sleep_etc(&deadline, Interruptible::No, now)
}

/// Interruptible version of [`sleep`].
pub fn sleep_interruptible(deadline: InstantMono) -> Result<(), Status> {
    let now = current_mono_time();
    let slack = TimerSlack::new(sleep_slack(deadline, now).into(), SlackMode::Late);
    let slack_deadline = Deadline::new(deadline.into(), slack);
    sleep_etc(&slack_deadline, Interruptible::Yes, now)
}

/// Handles a virtual memory fault in the address space that contains `va`. If there is no aspace
/// that contains `va`, or we don't have access to it, `Status::NOT_FOUND` is returned.
///
/// # Safety
///
/// * Must be called from thread context (`current_get()` non-null and initialized).
/// * If `va` is not a kernel address (`!is_kernel_address(va)`), the current thread must either be
///   a user thread or a kernel thread that has explicitly attached an initialized, non-unified
///   `Type::User` `VmAspace` (e.g. in unit tests via [`current_switch_aspace`]).
/// * The caller must satisfy `resolve_fault`'s locking and blocking preconditions.
#[inline]
unsafe fn current_fault(
    va: usize,
    resolve_fault: impl FnOnce(&VmAspace) -> Result<(), Status>,
) -> Result<(), Status> {
    if crate::arch_rs::vm::is_kernel_address(va) {
        core::hint::cold_path();
        // Kernel addresses should never fault.
        return Err(Status::NOT_FOUND);
    }

    // If this thread is a kernel thread, then it must be running a unit test that set `aspace`
    // explicitly, so use `aspace` to resolve the fault.
    //
    // If this is a user thread in restricted mode, then `aspace` is set to the restricted address
    // space and should be used to resolve the fault.
    //
    // Otherwise, this is a user thread running in normal mode. Therefore, we must consult the
    // process' `aspace_at` function to resolve the fault.
    let t = current_get();
    debug_assert!(!t.is_null());
    // SAFETY: Caller guarantees fault resolution is invoked from thread context on a valid,
    // running `Thread` that cannot be concurrently torn down.
    let t_ref = unsafe { &*t };
    // `Relaxed` ordering suffices because `set_usermode_thread` runs once under `t.get_lock()`
    // while in `Initial` before `resume()`, establishing happens-before with the thread's own
    // execution.
    let user_thread = t_ref.user_thread.load(Ordering::Relaxed);
    // SAFETY: `t` is the running current thread on this CPU, and only the current thread mutates
    // its own `restricted_state` and `in_restricted` flag.
    let containing_aspace: &VmAspace = if user_thread.is_null() || unsafe { in_restricted(t) } {
        let aspace = t_ref.aspace.load(Ordering::Relaxed).cast_const();
        debug_assert!(!aspace.is_null(), "fault on pure kernel thread with no aspace");
        // SAFETY: By caller's `# Safety` contract (and rules #1 and #3 on
        // `Thread::get_aspace_ref`), `aspace` is non-null and kept alive by the thread's process
        // (or the caller that attached a test address space via `current_switch_aspace`) for the
        // duration of this fault call.
        unsafe { aspace.as_ref_unchecked() }
    } else {
        // SAFETY: `user_thread` is non-null and holds a strong `RefPtr<ThreadDispatcher>` owned by
        // the running current thread `t` for the duration of this call.
        let user_thread = unsafe { &*user_thread };
        user_thread.process().aspace_at(va)
    };

    // Call the appropriate fault function on the containing address space.
    resolve_fault(containing_aspace)
}

/// Handles a page fault on the address space containing `va` for the current thread.
///
/// If there is no address space that contains `va`, or we don't have access to it,
/// `Status::NOT_FOUND` is returned.
///
/// # Safety
///
/// * Must be called from thread context (`current_get()` non-null, initialized, and in
///   `THREAD_RUNNING`) with interrupts enabled (`!ints_disabled()`), blocking permitted
///   (`!blocking_disallowed()`), and no spinlocks or active `ChainLockTransaction` held.
/// * If `va` is not a kernel address (`!is_kernel_address(va)`), the current thread must either be
///   a user thread or a kernel thread with an attached initialized, non-unified `Type::User`
///   `VmAspace` (calling this on a pure kernel thread with no `aspace` for a non-kernel address is
///   undefined behavior).
/// * Unless `flags` contains [`fault::flag::ACCESS`](crate::vm::fault::flag::ACCESS), may block on
///   page requests and must be called without locks held. An access fault only updates the
///   accessed flag (see [`VmAspace::accessed_fault`]) and never blocks on page requests, so it
///   only requires that no locks that conflict with `ArchVmAspace`'s internal lock are held.
#[inline]
pub unsafe fn page_fault(va: usize, flags: u32) -> Result<(), Status> {
    let resolve = |aspace: &VmAspace| {
        // SAFETY: `current_fault` returns early on kernel addresses and selects a live,
        // initialized, non-unified `Type::User` `VmAspace` (`aspace_at(va)` / `restricted_aspace`
        // for user threads, or the caller's attached initialized, non-unified `Type::User`
        // `VmAspace` for kernel threads). The caller upholds the thread-state, interrupt,
        // blocking, and locking preconditions of `VmAspace::page_fault`.
        unsafe { aspace.page_fault(va, flags) }
    };
    // SAFETY: Discharged by the caller: called from thread context on a valid running `Thread`,
    // with an attached initialized, non-unified `Type::User` `VmAspace` if a kernel thread faults
    // on a non-kernel address, and satisfying `resolve`'s locking/blocking preconditions.
    unsafe { current_fault(va, resolve) }
}

/// Handles a soft fault on the address space containing `va` for the current thread.
///
/// If there is no address space that contains `va`, or the thread does not have access to it,
/// `Status::NOT_FOUND` is returned.
///
/// # Safety
///
/// * Must be called from thread context (`current_get()` non-null and initialized).
/// * If `va` is not a kernel address (`!is_kernel_address(va)`), the current thread must either be
///   a user thread or a kernel thread with an attached initialized, non-unified `Type::User`
///   `VmAspace` (calling this on a pure kernel thread with no `aspace` for a non-kernel address is
///   undefined behavior).
/// * May block on page requests and must be called without locks held.
#[cold]
pub unsafe fn soft_fault(va: usize, flags: u32) -> Result<(), Status> {
    // SAFETY: Upheld by the caller.
    unsafe { current_fault(va, |aspace| aspace.soft_fault(va, flags)) }
}

/// Handles a soft fault in `[va, va + len)` on the address space containing `va` for the current
/// thread.
///
/// If there is no address space that contains `va`, or we don't have access to it,
/// `Status::NOT_FOUND` is returned.
///
/// # Safety
///
/// * Must be called from thread context (`current_get()` non-null and initialized).
/// * If `va` is not a kernel address (`!is_kernel_address(va)`), the current thread must either be
///   a user thread or a kernel thread with an attached initialized, non-unified `Type::User`
///   `VmAspace` (calling this on a pure kernel thread with no `aspace` for a non-kernel address is
///   undefined behavior).
/// * May block on page requests and must be called without locks held.
#[inline]
pub unsafe fn soft_fault_in_range(va: usize, flags: u32, len: usize) -> Result<(), Status> {
    // SAFETY: Upheld by the caller.
    unsafe { current_fault(va, |aspace| aspace.soft_fault_in_range(va, flags, len)) }
}

/// Handles an accessed fault on the address space containing `va` for the current thread.
///
/// If there is no address space that contains `va`, or we don't have access to it,
/// `Status::NOT_FOUND` is returned.
///
/// # Safety
///
/// * Must be called from thread context (`current_get()` non-null and initialized).
/// * If `va` is not a kernel address (`!is_kernel_address(va)`), the current thread must either be
///   a user thread or a kernel thread with an attached initialized, non-unified `Type::User`
///   `VmAspace` (calling this on a pure kernel thread with no `aspace` for a non-kernel address is
///   undefined behavior).
/// * Must not hold any locks that conflict with `ArchVmAspace`'s internal lock.
#[inline]
pub unsafe fn accessed_fault(va: usize) -> Result<(), Status> {
    // SAFETY: Upheld by the caller.
    unsafe { current_fault(va, |aspace| aspace.accessed_fault(va)) }
}

/// Returns the raw pointer to the current thread.
#[inline]
pub fn current_get() -> *mut Thread {
    // SAFETY: `cpp_thread_current_get` returns either null (before `ConstructFirstThread`) or a
    // pointer to a properly aligned `Thread` (64-byte aligned); null is also aligned.
    unsafe {
        let ptr = cpp_thread_current_get();
        core::hint::assert_unchecked(ptr.is_aligned());
        ptr
    }
}

/// Returns the current thread's associated `ThreadDispatcher`, or null if the current thread
/// is a kernel-only thread.
///
/// Uses `Relaxed` ordering via [`Thread::user_thread_ptr`] because
/// [`Thread::set_usermode_thread`] and `resume()` under the thread's lock already
/// happened-before the thread's own execution.
///
/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`].
#[inline]
pub unsafe fn current_user_thread() -> *mut crate::object::ThreadDispatcher {
    let t = current_get();
    debug_assert!(!t.is_null());
    // SAFETY: Caller guarantees `t` is the valid, live current `Thread` on this CPU.
    unsafe { Thread::user_thread_ptr(t) }
}

/// Returns the total runtime of the current thread.
///
/// # Safety
///
/// - The calling CPU must have a valid, initialized current [`Thread`].
/// - The caller must not hold the current thread's lock or have an active chain lock transaction on
///   the current CPU (`Thread::runtime` acquires the thread's lock via
///   `single_chain_lock_guard_owned!`, which panics if a `ChainLockTransaction` is already active).
#[inline]
pub unsafe fn current_runtime() -> DurationMono {
    // SAFETY: Caller guarantees `current_get` returns a valid pointer to the running thread, which
    // cannot exit while executing this call, and upholds the locking preconditions of
    // `Thread::runtime`.
    unsafe { (*current_get()).runtime() }
}

/// Creates a thread around the current execution context on a secondary CPU.
///
/// `construct_first_thread::<false>` constructs a new `Thread` in place over `thread`, which leaves
/// its stack empty without tearing down whatever it held, so the caller must own the stack the CPU
/// runs on.
// TODO(https://fxbug.dev/584127903): x86 and riscv64 initialize the stack in `thread` itself, so
// this reinitialization (matching the original C++ placement-new `new (t) Thread(name)`) leaks it.
// Clean this up in a follow-up.
///
/// # Safety
///
/// - Interrupts must be disabled (`ints_disabled()`).
/// - On x86-64, `%gs.base` must be initialized to the calling CPU's `x86_percpu`; on ARM64 and
///   RISC-V, the architectural thread pointer must still point to the early-boot fake `arch_thread`
///   (or `thread` itself on RISC-V secondary CPUs).
/// - `thread` must be non-null, properly aligned to `align_of::<Thread>()`, and point to valid
///   storage for the calling secondary CPU's bootstrap `Thread`, not concurrently accessed on any
///   other CPU and not in `THREAD_LIST`, that remains pinned and live for as long as it stays
///   installed as the current thread / in `THREAD_LIST`.
/// - `PerCpu::initialize_secondaries_begin()` and [`thread_lists_init`] must have completed on the
///   boot CPU, and neither `Thread::get_list_lock()` nor any `ChainLockTransaction` may be held.
pub unsafe fn secondary_cpu_init_early(thread: *mut Thread) {
    debug_assert!(crate::arch_rs::ints_disabled());
    debug_assert!(!thread.is_null() && thread.is_aligned());

    // At this point, the CPU isn't far enough along to allow threads to block. Set blocking
    // disallowed to catch bugs where code might block before we're ready.
    crate::arch_rs::set_blocking_disallowed(true);

    // SAFETY: Caller guarantees this runs on a secondary CPU with interrupts disabled after
    // `PerCpu::initialize_secondaries_begin()` has completed on the boot CPU.
    unsafe { PerCpu::initialize_secondary_finish() };

    let mut name_buf = [0u8; 16];
    let name = format_cpu_name(&mut name_buf, b"cpu_init ", crate::arch_rs::curr_cpu_num());
    // SAFETY: Interrupts are disabled, early arch thread-pointer / `%gs.base` state is valid,
    // `thread` is properly aligned writable pinned storage for this secondary CPU's bootstrap
    // `Thread`, `PerCpu::initialize_secondary_finish()` just completed, `thread_lists_init()`
    // completed on CPU 0, and no locks or `ChainLockTransaction` are held.
    unsafe { construct_first_thread::<false>(thread, name) };

    // Emitting the thread metadata usually happens during Thread::Resume(), however, cpu_init
    // threads are never resumed. Emit the metadata here so that the thread name is associated with
    // its tid.
    // SAFETY: `construct_first_thread` just constructed `thread` in place, and this CPU is
    // currently executing on it so it remains live for the duration of this call.
    let this = unsafe { &*thread };
    crate::ktrace_rs::kernel_object!(
        "kernel:meta",
        this.tid(),
        ZX_OBJ_TYPE_THREAD,
        name,
        "process" => crate::ktrace_rs::Koid(this.pid()),
        "job" => crate::ktrace_rs::Koid(0),
    );
}

/// Logs the relevant stack memory addresses of the current thread at the `CRITICAL` debug level.
///
/// This is useful during a thread dump.
///
/// # Safety
///
/// This CPU must have a constructed current [`Thread`] (`ConstructFirstThread` has completed on
/// this CPU), and its stack must not be concurrently initialized or torn down (see
/// [`Thread::stack`]).
pub unsafe fn dump_current_stack() {
    // SAFETY: The caller guarantees `current_get()` returns a valid, constructed `Thread` that
    // meets the requirements of `Thread::stack`.
    unsafe { Thread::stack(current_get()) }.dump_info(debug::dprintf::CRITICAL);
}

/// Request a thread to check if it should sample its backtrace.
///
/// When the thread returns to usermode, it will take a sample of its userstack if sampling is
/// enabled.
///
/// # Safety
///
/// - Interrupts must be disabled (`ints_disabled()`).
/// - The calling CPU must have a valid, initialized current [`Thread`].
/// - This must be called at most once per firing of the current CPU's `sampling_timer` callback.
pub unsafe extern "C" fn signal_sample_stack(
    _timer: *mut Timer,
    _now: zx_instant_mono_t,
    session_id: *mut c_void,
) {
    // Regardless of if the thread is marked to be sampled or not we'll set the sample_stack thread
    // signal. This reduces the time we spend in the interrupt context and means we don't need to
    // grab a lock here. When we handle the thread signal in `process_pending_signals` we'll check
    // if the thread actually needs to be sampled.
    let current_thread = current_get();
    debug_assert!(!current_thread.is_null() && current_thread.is_aligned());
    // SAFETY: `current_thread` is the valid, live running thread on the current CPU.
    unsafe {
        let sampling_session = &raw const (*current_thread).sampling_session;
        (*sampling_session).store(session_id.addr() as u64);
        Thread::assert_canary_ptr(current_thread);
        Thread::set_signals_ptr(current_thread, THREAD_SIGNAL_SAMPLE_STACK);
    }

    // We set the timer here as opposed to when we handle the THREAD_SIGNAL_SAMPLE_STACK as a thread
    // could be suspended or killed before the sample signal is handled.
    // SAFETY: Interrupts are disabled in timer callback context and this is called once per
    // firing of the current CPU's `sampling_timer` callback.
    unsafe {
        crate::thread_sampler::THREAD_SAMPLER.reschedule_marking();
    }
}

/// Records a failed stack-sampling attempt out of line so LLVM does not hoist the kcounter offset
/// calculation into the signal loop preheader.
#[cold]
#[inline(never)]
fn record_thread_sampling_failed() {
    THREAD_SAMPLING_FAILED.add(1);
}

/// Write the current thread's stack to the assigned sampler buffers.
///
/// # Safety
///
/// - Interrupts must be enabled (`!ints_disabled()`).
/// - `current_thread` must point to the valid, live running [`Thread`] on the current CPU, and
///   `has_user_thread` must accurately reflect
///   `!Thread::user_thread_ptr(current_thread).is_null()`.
/// - `gregs` must point to a valid general-registers frame matching `source` for the duration of
///   this call.
unsafe fn current_do_sample_stack(
    current_thread: *mut Thread,
    has_user_thread: bool,
    source: crate::arch_rs::GeneralRegsSource,
    gregs: *mut c_void,
) {
    debug_assert!(!crate::arch_rs::ints_disabled());
    debug_assert!(core::ptr::eq(current_thread, current_get()));
    debug_assert!(!current_thread.is_null() && current_thread.is_aligned());

    // Make sure the sample signal wasn't cleared while we were running the
    // callback.
    // SAFETY: `current_thread` is the valid, live running thread.
    if (unsafe { Thread::signals_ptr(current_thread) } & THREAD_SIGNAL_SAMPLE_STACK) != 0 {
        // SAFETY: `current_thread` is the valid, live running thread.
        unsafe { Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_SAMPLE_STACK) };

        if !has_user_thread {
            // There's no user thread to sample, just move on.
            return;
        }

        // If a thread was marked to be sampled but was first suspended, it may now be long after
        // the sampling session has ended. `ThreadSampler::sample_thread` grabs the global state
        // and checks if it's still valid.
        // SAFETY: Interrupts are enabled, `current_thread` is the valid running thread, and
        // `gregs` points to a valid register frame matching `source`.
        let sampler_result = unsafe {
            let sampling_session = &raw const (*current_thread).sampling_session;
            crate::thread_sampler::THREAD_SAMPLER.sample_thread(
                Thread::pid_ptr(current_thread),
                Thread::tid_ptr(current_thread),
                source,
                gregs.cast_const(),
                (*sampling_session).load(),
            )
        };
        if sampler_result.is_err() {
            core::hint::cold_path();
            record_thread_sampling_failed();
        }
    }
}

/// Process any pending thread signals.
///
/// This method may never return if the thread has a pending kill signal.
///
/// Interrupt state - This method modifies interrupt state.  It is critical that this method be
/// called with interrupts disabled to eliminate a "lost wakeup" race condition.  While
/// interrupts must be disabled prior to calling this method, the method may re-enable them during
/// the processing of certain signals.  This method guarantees that if it does return, it will do
/// so with interrupts disabled.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU's per-CPU state (`%gs.base` on x86-64) is initialized and
///   `ConstructFirstThread` has completed, with the current thread being a live, non-idle
///   [`Thread`] in the `THREAD_RUNNING` state.
/// - Interrupts are disabled on entry (`ints_disabled()`).
/// - `source` is a non-[`None`](crate::arch_rs::GeneralRegsSource::None)
///   [`GeneralRegsSource`](crate::arch_rs::GeneralRegsSource) supported by the target architecture
///   (`Iframe`, or `Syscall` on `x86_64`), and `gregs` is non-null, properly aligned, and points
///   to a valid, initialized, writable architectural register frame matching `source` (`Iframe`
///   for `GeneralRegsSource::Iframe`, `SyscallRegs` for `GeneralRegsSource::Syscall`) on the
///   current thread's kernel stack with no conflicting Rust references (`&` or `&mut`) for the
///   duration of the call (debuggers may inspect or mutate `*gregs` via `suspended_general_regs`
///   while stopped).
/// - Blocking is permitted once interrupts are re-enabled (`!blocking_disallowed()`), and no
///   spinlocks, thread lock, dispatcher locks, lockdep-tracked locks, or active
///   `ChainLockTransaction` are held on the calling CPU.
/// - If the current thread is a user thread, its architectural user-state storage is initialized.
///   `get_restricted_flag()` may only be set on a user thread with a bound `RestrictedState` and
///   exclusive access.
#[inline(always)]
pub unsafe fn process_pending_signals(
    source: crate::arch_rs::GeneralRegsSource,
    gregs: *mut c_void,
) {
    // Prior to calling this method, interrupts must be disabled.  This method may enable
    // interrupts, but if it does, it will disable them prior to returning.
    //
    // TODO(maniscalco): Refer the reader to the to-be-written theory of operations documentation
    // for kernel thread signals.
    debug_assert!(crate::arch_rs::ints_disabled());
    if current_is_signaled() {
        core::hint::cold_path();
        // SAFETY: Upheld by caller.
        unsafe { process_pending_signals_slow(source, gregs) };
    }
}

/// Out-of-line slow path for [`process_pending_signals`] when at least one signal is pending.
///
/// Kept `#[cold] #[inline(never)]` so trap and IRQ entry frames (which share an 8 KiB stack on
/// `arm64`/`riscv64` and run with SafeStack/ShadowCallStack) stay minimal on the `signals == 0`
/// fast path, and so full-LTO never inlines `process_pending_signals_slow`'s stack frame into
/// top-level exception handlers whose frames remain live across nested IRQs.
///
/// # Safety
///
/// Same requirements as [`process_pending_signals`].
#[cold]
#[inline(never)]
unsafe fn process_pending_signals_slow(
    source: crate::arch_rs::GeneralRegsSource,
    gregs: *mut c_void,
) {
    debug_assert!(crate::arch_rs::ints_disabled());

    let current_thread = current_get();
    debug_assert!(!current_thread.is_null() && current_thread.is_aligned());
    debug_assert!(!gregs.is_null() && (gregs.addr() & 7) == 0);

    // IF we're currently in restricted mode, we may decide to exit to normal mode instead as part
    // of signal processing. Store this information in a local so that we can process all signals
    // before actually exiting.
    let mut exit_to_normal_mode = false;
    let mut sampled_stack = false;

    // It's possible for certain signals to be asserted, processed, and then re-asserted during this
    // method so loop until there are no pending signals.
    let mut signals: u32;
    while {
        // SAFETY: `current_thread` is the valid, live running thread.
        signals = unsafe { Thread::signals_ptr(current_thread) };
        signals != 0
    } {
        // THREAD_SIGNAL_KILL
        //
        // We check this signal first because if asserted, the thread will terminate so there's no
        // point in checking other signals before kill.
        if (signals & THREAD_SIGNAL_KILL) != 0 {
            // We're going to call Exit, which generates a user-visible exception on user threads.
            // If this thread has user mode component, call arch_set_suspended_general_regs() to
            // make the general registers available to a debugger during the exception.
            // SAFETY: `current_thread` is the valid, live running thread.
            if unsafe { !Thread::user_thread_ptr(current_thread).is_null() } {
                // SAFETY: `current_thread` is the valid, live running thread, and `&Thread` is
                // scoped to this non-blocking block with interrupts disabled.
                let thread_ref: &Thread = unsafe { &*current_thread };
                // TODO(https://fxbug.dev/42076855): Do we need to hold the thread's lock here?
                single_chain_lock_guard_owned!(
                    guard,
                    thread_ref,
                    NO_IRQ_SAVE_OPTION,
                    clt_tag!("Thread::Current::ProcessPendingSignals")
                );
                let _proof = guard.proof();
                // SAFETY: `_proof` witnesses `current_thread`'s lock, and `gregs` lives on this
                // thread's stack across `current_exit(0)` below, which never returns (so
                // `reset_suspended_general_regs` is not needed).
                unsafe {
                    crate::arch_rs::ops::set_suspended_general_regs(current_thread, source, gregs);
                }
            }

            // Thread::Current::Exit may only be called with interrupts enabled.  Note, this
            // thread is committed to terminating and will never return so we don't need to worry
            // about disabling interrupts post-Exit.
            enable_ints();
            // SAFETY: Interrupts were just enabled, blocking is permitted, no locks or
            // transactions are held, and the current thread is a live running non-idle thread.
            unsafe { current_exit(0) };
        }

        // SAFETY: `current_thread` is the valid, live running thread.
        let has_user_thread = unsafe { !Thread::user_thread_ptr(current_thread).is_null() };

        // THREAD_SIGNAL_POLICY_EXCEPTION
        //
        if (signals & THREAD_SIGNAL_POLICY_EXCEPTION) != 0 {
            // Soundness upgrade from C++ DEBUG_ASSERT: policy exceptions require a user thread.
            assert!(has_user_thread);
            // TODO(https://fxbug.dev/42077109): Consider wrapping this up in a method
            // (e.g. Thread::Current::ClearSignals) and think hard about whether relaxed is
            // sufficient.
            // SAFETY: `current_thread` is the valid, live running thread.
            unsafe { Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_POLICY_EXCEPTION) };

            let (policy_exception_code, policy_exception_data) = {
                // SAFETY: `current_thread` is the valid, live running thread, and `&Thread` is
                // scoped to this non-blocking block with interrupts disabled.
                let thread_ref: &Thread = unsafe { &*current_thread };
                // TODO(https://fxbug.dev/42076855): Do we need to hold the thread's lock here?
                single_chain_lock_guard_owned!(
                    guard,
                    thread_ref,
                    NO_IRQ_SAVE_OPTION,
                    clt_tag!("Thread::Current::ProcessPendingSignals")
                );
                let proof = guard.proof();
                // Policy exceptions are user-visible so must make the general register state
                // available to a debugger.
                // SAFETY: `proof` witnesses `current_thread`'s lock, and `gregs` matches `source`
                // and remains valid until `reset_suspended_general_regs` below.
                unsafe {
                    crate::arch_rs::ops::set_suspended_general_regs(current_thread, source, gregs);
                }
                (proof.extra_policy_exception_code(), proof.extra_policy_exception_data())
            };

            // Call arch_dispatch_user_policy_exception with interrupts enabled, but be sure to
            // disable them afterwards.
            enable_ints();
            // SAFETY: Interrupts are enabled and the current thread is a user thread with no locks
            // held and `suspended_general_regs` pointing to the valid, writable, unaliased `gregs`.
            let status = unsafe {
                crate::arch_rs::dispatch_user_policy_exception(
                    policy_exception_code,
                    policy_exception_data,
                )
            };
            assert!(
                status.is_ok(),
                "arch_dispatch_user_policy_exception() failed: status={}",
                Status::result_into_raw(status)
            );
            crate::arch_rs::disable_ints();

            {
                // SAFETY: `current_thread` is the valid, live running thread, and `&Thread` is
                // scoped to this non-blocking block with interrupts disabled.
                let thread_ref: &Thread = unsafe { &*current_thread };
                // TODO(https://fxbug.dev/42076855): Do we need to hold the thread's lock here?
                single_chain_lock_guard_owned!(
                    guard,
                    thread_ref,
                    NO_IRQ_SAVE_OPTION,
                    clt_tag!("Thread::Current::ProcessPendingSignals")
                );
                let _proof = guard.proof();
                // SAFETY: `_proof` witnesses `current_thread`'s lock.
                unsafe {
                    crate::arch_rs::ops::reset_suspended_general_regs(current_thread);
                }
            }
        }

        // THREAD_SIGNAL_POLICY_KILL
        //
        if (signals & THREAD_SIGNAL_POLICY_KILL) != 0 {
            // Soundness upgrade from C++ DEBUG_ASSERT: `ProcessDispatcher::get_current()` requires
            // a user thread.
            assert!(has_user_thread);
            // TODO(https://fxbug.dev/42077109): Consider wrapping this up in a method
            // (e.g. Thread::Current::ClearSignals) and think hard about whether relaxed is
            // sufficient.
            // SAFETY: `current_thread` is the valid, live running thread.
            unsafe { Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_POLICY_KILL) };

            enable_ints();
            // Kill will begin the sequence, but won't actually terminate the calling thread.
            // Instead, it will raise THREAD_SIGNAL_KILL.  See that the signal was raised.  A
            // subsequent iteration of this loop will process THREAD_SIGNAL_KILL.
            crate::object::ProcessDispatcher::get_current()
                .kill(zx_types::ZX_TASK_RETCODE_POLICY_KILL);
            debug_assert!(
                // SAFETY: `current_thread` is the valid, live running thread.
                (unsafe { Thread::signals_ptr(current_thread) } & THREAD_SIGNAL_KILL) != 0
            );
            crate::arch_rs::disable_ints();
        }

        // THREAD_SIGNAL_SUSPEND
        //
        if (signals & THREAD_SIGNAL_SUSPEND) != 0 {
            if has_user_thread {
                let saved = {
                    // SAFETY: `current_thread` is the valid, live running thread, and `&Thread` is
                    // scoped to this non-blocking block with interrupts disabled.
                    let thread_ref: &Thread = unsafe { &*current_thread };
                    // TODO(https://fxbug.dev/42076855): Do we need to hold the thread's lock here?
                    single_chain_lock_guard_owned!(
                        guard,
                        thread_ref,
                        NO_IRQ_SAVE_OPTION,
                        clt_tag!("Thread::Current::ProcessPendingSignals")
                    );
                    let mut proof = guard.proof();
                    // This thread has been asked to suspend.  When a user thread is suspended,
                    // its full register state (not just the general purpose registers) is
                    // accessible to a debugger.
                    // SAFETY: `proof` witnesses `current_thread`'s lock, and `gregs` matches
                    // `source` and remains valid until `reset_suspended_general_regs` below.
                    unsafe {
                        crate::arch_rs::ops::set_suspended_general_regs(
                            current_thread,
                            source,
                            gregs,
                        );
                    }
                    // SAFETY: Interrupts are disabled, `proof` witnesses `current_thread`'s lock,
                    // and `current_thread` is the running user thread with initialized user-state
                    // storage.
                    unsafe { proof.save_user_state_locked(current_thread) }
                };

                // The enclosing function, is called at the boundary of kernel and user mode (e.g.
                // just before returning from a syscall, timer interrupt, or architectural
                // exception/fault).  We're about the perform a save.  If the save fails (returns
                // false), then we likely have a mismatched save/restore pair, which is a bug.
                debug_assert!(saved);

                enable_ints();
                // SAFETY: Interrupts are enabled and `current_thread` is the running non-idle
                // thread with no locks held.
                unsafe { current_do_suspend() };
                crate::arch_rs::disable_ints();

                {
                    // SAFETY: `current_thread` is the valid, live running thread, and `&Thread` is
                    // scoped to this non-blocking block with interrupts disabled.
                    let thread_ref: &Thread = unsafe { &*current_thread };
                    // TODO(https://fxbug.dev/42076855): Do we need to hold the thread's lock here?
                    single_chain_lock_guard_owned!(
                        guard,
                        thread_ref,
                        NO_IRQ_SAVE_OPTION,
                        clt_tag!("Thread::Current::ProcessPendingSignals")
                    );
                    let mut proof = guard.proof();
                    if saved {
                        // SAFETY: `saved` is true, so the matching `save_user_state_locked` above
                        // saved user state; interrupts are disabled and `proof` witnesses
                        // `current_thread`'s lock.
                        unsafe { proof.restore_user_state_locked(current_thread) };
                    }
                    // SAFETY: `proof` witnesses `current_thread`'s lock.
                    unsafe {
                        crate::arch_rs::ops::reset_suspended_general_regs(current_thread);
                    }
                }
            } else {
                // No user mode component so we don't need to save any register state.
                enable_ints();
                // SAFETY: Interrupts are enabled and `current_thread` is the running non-idle
                // thread with no locks held.
                unsafe { current_do_suspend() };
                crate::arch_rs::disable_ints();
            }
        }

        // THREAD_SIGNAL_RESTRICTED_KICK
        //
        if (signals & THREAD_SIGNAL_RESTRICTED_KICK) != 0 {
            // SAFETY: `current_thread` is the valid, live running thread.
            unsafe { Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_RESTRICTED_KICK) };
            // SAFETY: Per-CPU state is initialized and interrupts are disabled.
            if unsafe { crate::arch_rs::get_restricted_flag() } {
                exit_to_normal_mode = true;
            } else {
                // If we aren't currently in restricted mode, remember that we have a restricted
                // kick pending for the next time we try to enter restricted mode.
                // SAFETY: `current_thread` is the valid, live running thread.
                unsafe { Thread::set_restricted_kick_pending(current_thread, true) };
            }
        }

        // THREAD_SIGNAL_CHECK_RSEQ
        //
        if (signals & THREAD_SIGNAL_CHECK_RSEQ) != 0 {
            // Soundness upgrade from C++ DEBUG_ASSERT: rseq is only valid on user threads.
            assert!(has_user_thread);
            // SAFETY: `current_thread` is the valid, live running thread, interrupts are disabled,
            // and `gregs` matches `source` with exclusive access.
            unsafe {
                Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_CHECK_RSEQ);
                Thread::check_restartable_sequence(current_thread, source, gregs);
            }
        }

        // THREAD_SIGNAL_SAMPLE_STACK
        //
        if (signals & THREAD_SIGNAL_SAMPLE_STACK) != 0 {
            if !sampled_stack {
                sampled_stack = true;
                // Sampling the user stack may page fault as we try to do usercopies.
                enable_ints();
                // SAFETY: Interrupts are enabled, `current_thread` is the running thread,
                // `has_user_thread` matches `user_thread_ptr`, and `gregs` matches `source`.
                unsafe {
                    current_do_sample_stack(current_thread, has_user_thread, source, gregs);
                }
                crate::arch_rs::disable_ints();
            } else {
                // Clear any THREAD_SIGNAL_SAMPLE_STACK re-asserted by the sampling timer while
                // interrupts were enabled. The thread has not executed any new user instructions
                // since `gregs` was captured, and re-sampling the identical register state in a
                // loop can livelock under high-frequency sampling (e.g. in coverage builds).
                // SAFETY: `current_thread` is the valid, live running thread.
                unsafe { Thread::clear_signals_ptr(current_thread, THREAD_SIGNAL_SAMPLE_STACK) };
            }
        }
    }

    // Interrupts must remain disabled for the remainder of this function. If for any reason we need
    // to enable interrupts in handling we have to re-enter the loop above in order to process
    // signals that may have been raised.

    // A policy exception that was delivered after the kick was seen has already left restricted
    // mode: the exception is redirected to normal mode. That can happen when an interrupt raises
    // the exception while interrupts are enabled above. Leaving restricted mode again would
    // overwrite the saved restricted state, so keep the kick for the next time the thread enters
    // restricted mode instead.
    // SAFETY: Per-CPU state is initialized and interrupts are disabled.
    if exit_to_normal_mode && !unsafe { crate::arch_rs::get_restricted_flag() } {
        exit_to_normal_mode = false;
        // SAFETY: `current_thread` is the valid, live running thread.
        unsafe { Thread::set_restricted_kick_pending(current_thread, true) };
    }

    if exit_to_normal_mode {
        match source {
            crate::arch_rs::GeneralRegsSource::Iframe => {
                // SAFETY: `source == Iframe` guarantees `gregs` points to a valid `Iframe`, and
                // `restricted_leave_iframe` is called with interrupts disabled on a thread in
                // restricted mode.
                unsafe {
                    let iframe = &*gregs.cast::<crate::arch_rs::Iframe>();
                    crate::kernel::restricted::restricted_leave_iframe(
                        iframe,
                        zx_types::ZX_RESTRICTED_REASON_KICK,
                    );
                }
            }
            #[cfg(target_arch = "x86_64")]
            crate::arch_rs::GeneralRegsSource::Syscall => {
                // SAFETY: `source == Syscall` guarantees `gregs` points to a valid `SyscallRegs`,
                // and `restricted_leave_syscall` is called with interrupts disabled on a thread in
                // restricted mode.
                unsafe {
                    let syscall_regs = &*gregs.cast::<crate::arch_rs::SyscallRegs>();
                    crate::kernel::restricted::restricted_leave_syscall(
                        syscall_regs,
                        zx_types::ZX_RESTRICTED_REASON_KICK,
                    );
                }
            }
            _ => {
                debug_assert!(false, "invalid source {}", source as u32);
            }
        }
    }
}

/// Byte offset of the architecture-specific state (`arch`) within [`Thread`].
pub const ARCH_OFFSET: usize = offset_of!(Thread, arch);

/// Returns a pointer to the architecture-specific state (`arch_thread`) of `thread`.
///
/// The returned pointer is derived by wrapping byte-offset arithmetic ([`ARCH_OFFSET`]) without
/// dereferencing `thread` or requiring `thread` itself to be in-bounds of an allocation, so this is
/// safe to call before `thread` is fully constructed (including early-boot base-shifted thread
/// pointers); dereferencing the result is the caller's responsibility.
///
/// # Safety
/// Caller must ensure `thread` is a `Thread` pointer (or base-shifted early-boot pointer) whose
/// offset by [`ARCH_OFFSET`] yields the target `arch_thread`.
#[inline(always)]
pub unsafe fn get_arch(thread: *mut Thread) -> *mut c_void {
    thread.wrapping_byte_add(ARCH_OFFSET).cast::<c_void>()
}

/// Returns the top of the stack for `thread`.
///
/// # Safety
///
/// `thread` must point to a valid, constructed `Thread` that meets the requirements of
/// `Thread::stack`, so that its stack is not concurrently initialized or torn down.
#[inline(always)]
pub unsafe fn get_stack_top(thread: *const Thread) -> usize {
    // SAFETY: The caller guarantees `thread` is valid and that the requirements of
    // `Thread::stack` hold.
    unsafe { Thread::stack(thread) }.top()
}

/// Returns the base of the stack for `thread` (`Thread::stack().base()`).
///
/// # Safety
///
/// As for [`get_stack_top`].
#[inline(always)]
pub unsafe fn get_stack_base(thread: *const Thread) -> usize {
    // SAFETY: The caller guarantees `thread` is valid and that the requirements of
    // `Thread::stack` hold.
    unsafe { Thread::stack(thread) }.base()
}

/// Returns the shadow call stack base for `thread`, or 0 if shadow call stacks are disabled.
///
/// # Safety
///
/// As for [`get_stack_top`].
#[inline(always)]
pub unsafe fn get_shadow_call_base(thread: *const Thread) -> usize {
    #[cfg(sanitize = "shadow-call-stack")]
    {
        // SAFETY: As for `get_stack_top`.
        unsafe { Thread::stack(thread) }.shadow_call_base()
    }
    #[cfg(not(sanitize = "shadow-call-stack"))]
    {
        let _ = thread;
        0
    }
}

/// Returns the unsafe stack top for `thread`, or 0 if safe-stack is disabled.
///
/// # Safety
///
/// As for [`get_stack_top`].
#[inline(always)]
pub unsafe fn get_unsafe_stack_top(thread: *const Thread) -> usize {
    #[cfg(sanitize = "safestack")]
    {
        // SAFETY: As for `get_stack_top`.
        unsafe { Thread::stack(thread) }.unsafe_top()
    }
    #[cfg(not(sanitize = "safestack"))]
    {
        let _ = thread;
        0
    }
}

/// Returns the unsafe stack base for `thread`, or 0 if safe-stack is disabled
/// (`Thread::stack().unsafe_base()`).
///
/// # Safety
///
/// As for [`get_stack_top`].
#[inline(always)]
pub unsafe fn get_unsafe_stack_base(thread: *const Thread) -> usize {
    #[cfg(sanitize = "safestack")]
    {
        // SAFETY: As for `get_stack_top`.
        unsafe { Thread::stack(thread) }.unsafe_base()
    }
    #[cfg(not(sanitize = "safestack"))]
    {
        let _ = thread;
        0
    }
}

/// Dumps the information of the current thread during a panic (`Thread::Current::DumpDuringPanic`),
/// skipping thread locks.
///
/// If `full` is `true`, a multi-line verbose dump of thread information is performed; otherwise a
/// single short summary line is dumped.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU's per-CPU state (`%gs.base` on x86-64) is initialized and
///   `ConstructFirstThread` has completed (`!current_get().is_null()` on x86-64).
/// - Interrupts are disabled on the calling CPU, and this function is called during a kernel panic
///   after peer CPUs have been halted (e.g., after `platform_panic_start()`), so no local IRQ or
///   other CPU can concurrently mutate the current thread's fields while `DumpDuringPanic` reads
///   them without acquiring `Thread::get_lock()`.
#[inline]
pub unsafe fn current_dump_during_panic(full: bool) {
    // SAFETY: Discharged by this function's `# Safety` contract: the calling CPU has interrupts
    // disabled and a valid, constructed current `Thread`, and peer CPUs are halted in panic
    // context so lock-skipping reads do not race with concurrent mutations.
    unsafe { Thread::dump_during_panic(current_get(), full) };
}

/// Returns `true` if `thread`'s user state has been saved.
///
/// # Safety
/// Caller must ensure `thread` points to a valid `Thread` instance and that the caller holds
/// `thread`'s lock for the duration of this call, with no conflicting `ChainLockProof` or live
/// borrow of `thread`'s lock-protected state active for the duration of this call.
#[inline(always)]
pub unsafe fn is_user_state_saved_locked(thread: *const Thread) -> bool {
    // SAFETY: `thread` is a valid `Thread` whose lock the caller holds with no conflicting
    // `ChainLockProof` or live borrow of its lock-protected state active (caller's contract); the
    // temporary proof does not escape this synchronous read.
    unsafe { ChainLockProof::assume_held_unchecked(&*thread) }.user_state_saved()
}

/// Checks whether `thread` is currently running.
///
/// # Safety
///
/// Caller must ensure `thread` points to a valid `Thread` instance whose `state` is not being
/// concurrently mutated.
#[inline]
pub unsafe fn is_running(thread: *const Thread) -> bool {
    debug_assert!(!thread.is_null());
    // SAFETY: Caller guarantees `thread` points to a valid `Thread`. Projecting the `state` field
    // through raw pointers as a `u8` avoids forming a `&SchedulerState` reference or materializing
    // an invalid enum discriminant if inspected during a panic dump.
    let raw_state = unsafe {
        let ss = (*thread).scheduler_state.get().cast::<scheduler::SchedulerState>();
        (&raw const (*ss).state).cast::<u8>().read()
    };
    raw_state == ThreadStateKind::Running as u8
}

/// Checks whether `thread` is executing in restricted mode.
///
/// # Safety
///
/// Caller must ensure `thread` points to a valid `Thread` instance whose `restricted_state` is not
/// concurrently mutated.
#[inline(always)]
pub unsafe fn in_restricted(thread: *mut Thread) -> bool {
    debug_assert!(!thread.is_null());
    // SAFETY: Caller guarantees `thread` points to a valid `Thread` with no concurrent mutation of
    // `restricted_state`.
    let rs = unsafe { (*(*thread).restricted_state.get()).cast::<RestrictedState>() };
    // SAFETY: If non-null, `rs` points to the valid `RestrictedState` owned by `thread`, and
    // `in_restricted_ptr` reads `in_restricted` without forming a `&RestrictedState` reference that
    // could alias a live `&mut RestrictedState` during a restricted exception report user copy.
    !rs.is_null() && unsafe { RestrictedState::in_restricted_ptr(rs) }
}

/// Checks whether `thread` is a user thread, i.e. has an associated `ThreadDispatcher`.
///
/// # Safety
///
/// Caller must ensure `thread` points to a valid, live `Thread` instance.
#[inline]
pub unsafe fn is_user_thread(thread: *const Thread) -> bool {
    debug_assert!(!thread.is_null());
    // SAFETY: Caller guarantees `thread` points to a valid, live `Thread`.
    !unsafe { Thread::user_thread_ptr(thread) }.is_null()
}

/// Returns the currently active address space of `thread`, which is the address space currently
/// hosting page tables for the thread.
///
/// The active address space should be used only when context switching. It should not be used for
/// resolving faults, as it may be a unified aspace that does not keep track of its own mappings.
///
/// Kernel-only thread -- This will return null, unless a caller has explicitly set the aspace
/// using [`current_switch_aspace`], which is done by a few kernel unittests.
///
/// User thread -- If the thread is in Restricted Mode, this will return the restricted aspace.
/// Otherwise, it will return the process's normal aspace.
///
/// Note, the normal aspace is, by definition, the aspace that's active when a thread is in Normal
/// Mode. All threads not in Restricted Mode are said to be in Normal Mode. See
/// `ProcessDispatcher::normal_aspace()` for more information.
///
/// Only the thread itself, or the context switch path for the two threads it is switching
/// between, may read a thread's active aspace without holding a reference to it; see the rules for
/// `Thread::aspace` on [`Thread::get_aspace_ref`].
///
/// # Safety
///
/// Caller must ensure `thread` points to a valid `Thread` instance that is either the current
/// thread or one of the two threads of a context switch in progress.
#[inline(always)]
pub unsafe fn active_aspace(thread: *mut Thread) -> *mut VmAspace {
    debug_assert!(!thread.is_null());
    // SAFETY: Caller guarantees `thread` points to a valid `Thread` that is either the current
    // thread or one of the two threads of a context switch in progress. Relaxed ordering suffices
    // because only the thread itself mutates `aspace` (`switch_aspace_raw`), or the thread lock is
    // held during context switch.
    unsafe { (*thread).aspace.load(Ordering::Relaxed) }
}

/// Returns the current thread's restricted mode state pointer.
///
/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`].
#[inline]
pub unsafe fn current_restricted_state() -> *mut RestrictedState {
    let t = current_get();
    debug_assert!(!t.is_null());
    // SAFETY: Caller guarantees `t` is the running current thread on this CPU, and
    // `restricted_state` is only mutated by the current thread itself.
    unsafe { (*(*t).restricted_state.get()).cast::<RestrictedState>() }
}

/// Returns the currently active address space, which is the address space currently hosting page
/// tables for the current thread.
///
/// The active address space should be used only when context switching. It should not be used for
/// resolving faults, as it may be a unified aspace that does not keep track of its own mappings.
///
/// Kernel-only thread -- This will return `None`, unless a caller has explicitly set `aspace_`
/// using [`current_switch_aspace`], which is done by a few kernel unittests.
///
/// User thread -- If the thread is in Restricted Mode, this will return the restricted aspace.
/// Otherwise, it will return the process's normal aspace.
///
/// Note, the normal aspace is, by definition, the aspace that's active when a thread is in Normal
/// Mode. All threads not in Restricted Mode are said to be in Normal Mode. See
/// `ProcessDispatcher::normal_aspace()` for more information.
///
/// # Safety
///
/// - The calling CPU must have a valid, initialized current [`Thread`].
/// - The caller must ensure that the returned address space reference remains valid for lifetime
///   `'a`.
#[inline]
pub unsafe fn current_active_aspace<'a>() -> Option<&'a VmAspace> {
    // SAFETY: Caller guarantees the calling CPU has a valid current thread and that the returned
    // address space reference remains valid for `'a`.
    unsafe { current_active_aspace_ptr().map(|aspace| aspace.as_ref()) }
}

/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`].
#[inline]
pub unsafe fn current_active_aspace_ptr() -> Option<NonNull<VmAspace>> {
    // SAFETY: Caller guarantees `current_get()` is a valid current thread.
    NonNull::new(unsafe { active_aspace(current_get()) })
}

/// Replaces `thread`'s address space pointer with `aspace` and returns the previous pointer without
/// performing a hardware context switch.
///
/// Used by [`set_active_aspace`](crate::vm::vmm::set_active_aspace) so it can reuse an already
/// fetched `current_get()` pointer and avoid branching on `Some`/`None`.
///
/// # Safety
///
/// - `thread` must be the calling CPU's valid, initialized current [`Thread`].
/// - If `aspace` is `Some`, it must point to a valid `VmAspace` that remains alive while attached
///   to `thread`.
/// - If `aspace` is `None`, `thread` must be able to safely run without an attached user address
///   space.
/// - Any replaced address space must remain valid until any hardware MMU switch or reschedule away
///   from it completes.
#[inline(always)]
pub unsafe fn switch_aspace(
    thread: *mut Thread,
    aspace: Option<NonNull<VmAspace>>,
) -> *mut VmAspace {
    let ptr = aspace.map_or(core::ptr::null_mut(), NonNull::as_ptr);
    // SAFETY: Discharged by caller: `thread` is the valid current `Thread`, `ptr` is null or
    // remains valid while attached, and any replaced address space remains valid until any hardware
    // MMU switch or reschedule completes.
    unsafe { switch_aspace_raw(thread, ptr) }
}

/// Switches the current thread's address space pointer without performing a hardware context
/// switch.
///
/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`], and the caller must ensure
/// `aspace` remains valid while attached to the current thread and that any replaced address space
/// remains valid until any hardware MMU switch or reschedule away from it completes.
#[inline]
pub unsafe fn current_switch_aspace(aspace: &fbl::RefPtr<VmAspace>) -> *mut VmAspace {
    let ptr = fbl::RefPtr::as_ptr(aspace).cast_mut();
    // SAFETY: Caller guarantees a valid current `Thread` and that `aspace` remains valid while
    // attached to it.
    unsafe { switch_aspace_raw(current_get(), ptr) }
}

/// Clears the current thread's address space pointer without performing a hardware context
/// switch.
///
/// Note: This helper is only used by test and kernel debugging code (such as `k vmm` console
/// commands) to detach a test address space from a kernel thread.
///
/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`], the caller must ensure the
/// current thread can safely run without an attached user address space, and any replaced address
/// space must remain valid until any hardware MMU switch or reschedule away from it completes.
#[inline]
pub unsafe fn current_clear_aspace() -> *mut VmAspace {
    // SAFETY: Caller guarantees a valid current `Thread` and that clearing its address space
    // pointer (and keeping any replaced address space valid until switch/reschedule completes) is
    // valid.
    unsafe { switch_aspace_raw(current_get(), core::ptr::null_mut()) }
}

/// Replaces `thread`'s `aspace` pointer with `aspace` and returns the previous pointer.
///
/// # Safety
///
/// `thread` must be the calling CPU's valid, initialized current [`Thread`], `aspace` must be null
/// or remain valid while attached to `thread`, and any replaced address space must remain valid
/// until any hardware MMU switch or reschedule away from it completes.
#[inline(always)]
unsafe fn switch_aspace_raw(thread: *mut Thread, aspace: *mut VmAspace) -> *mut VmAspace {
    debug_assert!(!thread.is_null());
    // `Relaxed` ordering suffices: `(*thread).aspace` is only mutated by the current thread (so no
    // concurrent writers exist, and atomic load/store avoids data races without holding `thread`'s
    // lock or the scheduler lock). Cross-CPU readers (`Thread::get_aspace_ref`, `dump_thread_raw`)
    // may load it concurrently via `Relaxed` atomic load; callers must ensure any attached
    // `VmAspace` remains valid while attached and until any hardware MMU switch or reschedule
    // completes.
    // SAFETY: Caller guarantees `thread` is the live current `Thread`; raw field projection avoids
    // forming `&Thread`.
    unsafe {
        let old = (*thread).aspace.load(Ordering::Relaxed);
        (*thread).aspace.store(aspace, Ordering::Relaxed);
        old
    }
}

/// Sets the current thread's restricted mode state pointer.
///
/// # Safety
///
/// - The calling CPU must have a valid, initialized current [`Thread`].
/// - `raw_rs` must be null or a valid `RestrictedState` raw pointer allocated by
///   `RestrictedState::create` (via `Box::into_raw`), and there must be no live references to the
///   current thread's existing `RestrictedState` (if any).
#[inline]
pub unsafe fn current_set_restricted_state(raw_rs: *mut RestrictedState) {
    let t = current_get();
    debug_assert!(!t.is_null());
    // SAFETY: Caller guarantees `t` is the running current thread on this CPU, and
    // `restricted_state` is only mutated by the current thread itself. Replacing the pointer before
    // dropping `old` matches `ktl::unique_ptr::reset`.
    let old = unsafe {
        core::ptr::replace((*t).restricted_state.get(), raw_rs.cast::<c_void>())
            .cast::<RestrictedState>()
    };
    if !old.is_null() {
        // SAFETY: `old` was previously installed via `current_set_restricted_state` from a
        // `Box<RestrictedState>` and is no longer referenced.
        unsafe {
            let _ = kalloc::Box::from_raw(old);
        }
    }
}

/// Returns the current thread's ktrace test override (`Thread::rust_ktrace_override()`), or null
/// if there is none.
///
/// # Safety
///
/// The calling CPU must have a valid current [`Thread`] (`current_get()` is not guaranteed to be
/// null before one is installed).
#[cfg(ktest)]
#[inline(always)]
pub unsafe fn current_rust_ktrace_override() -> *mut c_void {
    // Uses `cpp_thread_current_get` and `AtomicPtr::from_ptr` instead of `current_get()` and
    // `(*ptr).load()` so that builds with debug assertions emit no alignment check at each
    // tracepoint that inlines this read.
    //
    // SAFETY: `cpp_thread_current_get` only requires the per-CPU state it reads (for example,
    // x86 `%gs.base`) to be set up, which a valid current `Thread` implies. The caller guarantees
    // that it returns the live current `Thread` (so `current_get`'s alignment assertion is not
    // needed), and `&raw const` projects the `rust_ktrace_override` field place without forming
    // a reference to the whole `Thread`.
    let ktrace_override = unsafe { &raw const (*cpp_thread_current_get()).rust_ktrace_override };
    // SAFETY: `ktrace_override` is the live current `Thread`'s field (see above), which is a
    // properly aligned `AtomicPtr<c_void>`, and `AtomicPtr<c_void>` has the layout of
    // `*mut c_void`. Every access to this field (Rust `AtomicPtr`, C++ `RelaxedAtomic<void*>`) is
    // a pointer-sized atomic.
    unsafe { AtomicPtr::from_ptr(ktrace_override.cast_mut().cast::<*mut c_void>()) }
        .load(Ordering::Relaxed)
}

/// Sets the current thread's ktrace test override.
///
/// # Safety
///
/// - The calling CPU must have a valid current [`Thread`].
/// - `ktrace_ptr` must be null or point to a live `KTrace` instance that stays live until it is
///   replaced or cleared on the current thread: `KTrace` dereferences the installed pointer
///   whenever thread-local overrides are active.
#[cfg(ktest)]
#[inline]
pub unsafe fn current_set_rust_ktrace_override(ktrace_ptr: *mut c_void) {
    // SAFETY: The caller guarantees that `current_get()` is the live current `Thread`, and
    // `&raw const` projects the `rust_ktrace_override` field place without forming a reference
    // to the whole `Thread`.
    let ktrace_override = unsafe { &raw const (*current_get()).rust_ktrace_override };
    // SAFETY: `ktrace_override` is the live current `Thread`'s field (see above), and the caller
    // guarantees the lifetime of `ktrace_ptr`.
    unsafe { (*ktrace_override).store(ktrace_ptr, Ordering::Relaxed) };
}

/// Returns `true` if the current thread has been signaled.
#[inline(always)]
pub fn current_is_signaled() -> bool {
    // SAFETY: `current_get()` returns the current `Thread`, and `&raw const` projects the `signals`
    // field place without forming a reference to the whole `Thread`.
    let signals = unsafe { &raw const (*current_get()).signals };
    // SAFETY: `signals` is the live current `Thread`'s `signals` field (see above).
    unsafe { (*signals).load() != 0 }
}

/// If a restricted kick is pending on the current thread, clears it and returns `true`.
/// Otherwise returns `false`.
///
/// Must be called with interrupts disabled (semantic correctness requirement to avoid losing a
/// pending kick before entering restricted mode).
///
/// # Safety
///
/// The calling CPU's current `Thread` pointer must have been initialized (i.e., after
/// `ConstructFirstThread` on the calling CPU).
#[inline]
#[must_use]
pub unsafe fn current_check_for_restricted_kick() -> bool {
    debug::ltrace_entry!();
    debug_assert!(crate::arch_rs::ints_disabled());

    let current_thread = current_get();
    // SAFETY: Caller guarantees `current_thread` is a valid pointer to the running thread on the
    // calling CPU.
    if unsafe { Thread::restricted_kick_pending(current_thread) } {
        core::hint::cold_path();
        // SAFETY: Caller guarantees `current_thread` is a valid pointer to the running thread on
        // the calling CPU.
        unsafe { Thread::set_restricted_kick_pending(current_thread, false) };
        return true;
    }

    false
}

/// The current thread's [`MemoryAllocationState`] (`Thread::Current::memory_allocation_state()`),
/// as a pointer to the field alone so that no `&Thread` is formed.
///
/// The returned pointer remains valid for dereferencing as long as the calling thread continues
/// executing.
///
/// # Safety
///
/// The calling CPU's current `Thread` pointer must have been initialized (i.e., after
/// `ConstructFirstThread` on the calling CPU).
#[inline]
pub unsafe fn current_memory_allocation_state() -> *const MemoryAllocationState {
    let current = current_get();
    // SAFETY: Caller guarantees `current` is the valid, live current thread; projecting the field
    // through the raw pointer forms no reference.
    unsafe { &raw const (*current).memory_allocation_state }
}

/// Returns `true` if memory allocation is allowed on the current thread.
#[inline]
pub fn current_memory_allocation_state_is_enabled() -> bool {
    // SAFETY: Only called on an initialized thread; the temporary reference does not escape this
    // call.
    unsafe { (*current_memory_allocation_state()).is_enabled() }
}

/// Signals an exception on the current thread, to be handled when the thread next returns to user
/// mode: when the current syscall exits, or, if this is called from an interrupt, when the
/// interrupt returns.
///
/// Unlike other signals, this is synchronous, in the sense that a thread signals itself. This
/// exists primarily so that we can unwind the stack in order to get the state of userland's
/// callee-saved registers at the point where userland invoked the syscall.
///
/// The lockup detector calls this from the timer interrupt, for
/// `ZX_EXCP_POLICY_CODE_SPIN_DETECTION`. The interrupted thread can be in restricted mode, or in
/// `process_pending_signals` with interrupts enabled, so the signal can be raised while other
/// signals are being processed.
///
/// `policy_exception_code` should be a `ZX_EXCP_POLICY_CODE_*` value.
pub fn signal_policy_exception(policy_exception_code: u32, policy_exception_data: u32) {
    // SAFETY: `current_get()` returns a valid pointer to the running thread on the calling CPU.
    let t: &Thread = unsafe { &*current_get() };
    crate::kconcurrent::single_chain_lock_guard_owned!(
        guard,
        t,
        IRQ_SAVE_OPTION,
        clt_tag!("Thread::Current::SignalPolicyException")
    );
    t.set_signals(THREAD_SIGNAL_POLICY_EXCEPTION);
    let mut proof = guard.proof();
    *proof.extra_policy_exception_code_mut() = policy_exception_code;
    *proof.extra_policy_exception_data_mut() = policy_exception_data;
}

/// Signals that the current process should be killed when the current syscall exits, because a
/// policy with `ZX_POL_ACTION_KILL` was violated.
///
/// Like [`signal_policy_exception`], this is synchronous: the thread signals itself.
pub fn signal_policy_kill() {
    // SAFETY: `current_get()` returns a valid pointer to the running thread on the calling CPU.
    let t: &Thread = unsafe { &*current_get() };
    crate::kconcurrent::single_chain_lock_guard_owned!(
        _guard,
        t,
        IRQ_SAVE_OPTION,
        clt_tag!("Thread::Current::SignalPolicyKill")
    );
    t.set_signals(THREAD_SIGNAL_POLICY_KILL);
}

/// Migrates the current thread to the CPU identified by `target_cpu`, and hard-pins it there.
///
/// # Safety
///
/// The caller must ensure that:
/// - `target_cpu` is a valid, currently active CPU.
/// - This function is called from normal thread context where blocking/rescheduling is allowed,
///   with no spinlocks or active `ChainLockTransaction` held on the current CPU.
#[inline]
pub unsafe fn migrate_to_cpu(target_cpu: cpu_num_t) {
    // SAFETY: `ThreadPtr::current()` is the running current thread, and the caller guarantees
    // `target_cpu` is active and that the calling context is a normal thread context with no active
    // `ChainLockTransaction` or spinlocks held.
    let _ = unsafe {
        ThreadPtr::current().set_cpu_affinity(crate::kernel::types::cpu_num_to_mask(target_cpu))
    };
}

/// Generate a backtrace for the calling thread (`Thread::Current::GetBacktrace`).
///
/// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot
/// be obtained, it will be left empty.
#[inline(never)]
pub fn current_get_backtrace(out_bt: &mut Backtrace) {
    let fp = crate::get_frame!() as usize;
    // SAFETY: `current_get()` is either null (handled by `get_backtrace_common`) or the running
    // calling thread whose stack frames at and above `fp` are live for the duration of this call.
    unsafe { get_backtrace_common(current_get(), fp, out_bt) };

    // (https://fxbug.dev/42179766): Force the function to not tail call `get_backtrace_common`.
    // This will make sure the frame pointer we grabbed at the top
    // of the function is still valid across the call.
    // SAFETY: Empty inline assembly has no side effects.
    unsafe { core::arch::asm!("", options(nomem, nostack, preserves_flags)) };
}

/// Generate a backtrace for the calling thread starting from frame pointer `fp`
/// (`Thread::Current::GetBacktrace`).
///
/// `out_bt` will be `reset()` prior to be filled in and if a backtrace cannot
/// be obtained, it will be left empty.
///
/// # Safety
///
/// The caller must ensure that:
/// - On ARM64 and RISC-V, `ConstructFirstThread` has completed on the calling CPU; on x86-64,
///   `%gs.base` is initialized and `%gs:current_thread` is either null or points to a valid,
///   constructed `Thread` whose `KernelStack` metadata is not concurrently modified.
/// - Whenever `fp` falls within `[stack.base(), stack.top())` on the calling thread, `fp` is a
///   valid, aligned, initialized frame pointer on the calling thread's stack (or mapped stack
///   memory read during crash/panic/NMI dumping with no conflicting live `&mut` borrows over
///   `[stack.base(), stack.top())`).
#[inline]
pub unsafe fn current_get_backtrace_with_fp(fp: usize, out_bt: &mut Backtrace) {
    // SAFETY: `current_get()` is either null or the calling thread, and the caller guarantees
    // per-CPU/thread initialization and that `fp` satisfies `read_stack`'s stack initialization
    // and non-aliasing requirements.
    unsafe { get_backtrace_common(current_get(), fp, out_bt) };
}

/// Forwards C++ `Thread::vcpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_vcpu(thread: &Thread) -> bool {
    thread.vcpu()
}

/// Forwards C++ `Thread::set_vcpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_set_vcpu(thread: &Thread, value: bool) {
    thread.set_vcpu(value);
}

/// Forwards C++ `Thread::name`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_name(thread: &Thread) -> *const c_char {
    thread.name.as_ptr().cast::<c_char>()
}

/// Forwards C++ `Thread::CheckKillOrSuspendSignal`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_check_kill_or_suspend_signal(thread: &Thread) -> zx_status_t {
    Status::result_into_raw(thread.check_kill_or_suspend_signal())
}

/// Forwards C++ `Thread::CreateEtc`, returning null on failure.
///
/// # Safety
///
/// - When `name_len > 0`, `name` must be valid for reads of `name_len` initialized bytes that are
///   not mutated during the call; when `name_len == 0`, `name` is not dereferenced and may be
///   null.
/// - `profile` must be a valid `SchedulerState::BaseProfile`.
/// - `t`, `entry`, `arg`, and `alt_trampoline`, and the calling context, must satisfy the
///   [`create_etc`] preconditions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_create_etc(
    t: *mut Thread,
    name: *const c_char,
    name_len: usize,
    entry: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    arg: *mut c_void,
    profile: &SchedulerStateBaseProfile,
    alt_trampoline: Option<unsafe extern "C" fn() -> !>,
) -> *mut Thread {
    // SAFETY: The caller guarantees `name` is valid for `name_len` unmutated bytes when
    // `name_len > 0`, and `zr::slice_from_raw_parts` handles `name_len == 0` without
    // dereferencing `name`.
    let name = unsafe { zr::slice_from_raw_parts(name.cast::<u8>(), name_len) };
    // SAFETY: Forwarded from the C++ caller.
    unsafe { create_etc(t, name, entry, arg, profile, alt_trampoline) }
        .map_or(core::ptr::null_mut(), ThreadPtr::as_raw)
}

/// Wraps the `this` pointer that a C++ `Thread` method passes to its `rust_thread_*` forwarder.
///
/// The forwarders take a raw pointer rather than `&Thread` because the callee may free the
/// `Thread` before returning (e.g. `join` or `forget` of an exited thread, or a
/// detached thread exiting once resumed), so no reference to it may be live across the call. The
/// pointer is not dereferenced here; the forwarder's own safety contract covers its use.
///
/// # Safety
///
/// `thread` must be non-null (as the C++ `this` pointer always is).
#[inline(always)]
unsafe fn thread_ptr_from_ffi(thread: *mut Thread) -> ThreadPtr {
    debug_assert!(!thread.is_null());
    // SAFETY: The caller guarantees `thread` is non-null.
    ThreadPtr(unsafe { NonNull::new_unchecked(thread) })
}

/// Forwards C++ `Thread::Resume`.
///
/// # Safety
///
/// Callers must uphold the [`ThreadPtr::resume`] preconditions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_resume(thread: *mut Thread) {
    // SAFETY: C++ passes `this`, which is never null.
    let thread = unsafe { thread_ptr_from_ffi(thread) };
    // SAFETY: Forwarded from the C++ caller.
    unsafe { thread.resume() };
}

/// Forwards C++ `Thread::DetachAndResume`.
///
/// # Safety
///
/// Callers must uphold the [`ThreadPtr::detach_and_resume`] preconditions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_detach_and_resume(thread: *mut Thread) -> zx_status_t {
    // SAFETY: C++ passes `this`, which is never null.
    let thread = unsafe { thread_ptr_from_ffi(thread) };
    // SAFETY: Forwarded from the C++ caller.
    Status::result_into_raw(unsafe { thread.detach_and_resume() })
}

/// Forwards C++ `Thread::Join`, storing the return code through `out_retcode` (if non-null) on
/// success.
///
/// # Safety
///
/// - Callers must uphold the [`ThreadPtr::join`] preconditions.
/// - `out_retcode` must be null, or aligned, valid for writes, and not accessed through any other
///   path for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_join(
    thread: *mut Thread,
    out_retcode: Option<&mut i32>,
    deadline: zx_instant_mono_t,
) -> zx_status_t {
    // SAFETY: C++ passes `this`, which is never null.
    let thread = unsafe { thread_ptr_from_ffi(thread) };
    // SAFETY: Forwarded from the C++ caller.
    let result = unsafe { thread.join(InstantMono(deadline)) };
    Status::result_into_raw(result.map(|retcode| {
        if let Some(out_retcode) = out_retcode {
            *out_retcode = retcode;
        }
    }))
}

/// Forwards C++ `Thread::Forget`.
///
/// # Safety
///
/// Callers must uphold the [`ThreadPtr::forget`] preconditions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_forget(thread: *mut Thread) {
    // SAFETY: C++ passes `this`, which is never null.
    let thread = unsafe { thread_ptr_from_ffi(thread) };
    // SAFETY: Forwarded from the C++ caller.
    unsafe { thread.forget() };
}

/// Forwards C++ `Thread::SecondaryCpuInitEarly`.
///
/// # Safety
///
/// Same requirements as [`secondary_cpu_init_early`].
// SAFETY: Symbol name is unique and matches `void rust_thread_secondary_cpu_init_early(Thread*)`
// in `<kernel/thread.h>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_secondary_cpu_init_early(thread: *mut Thread) {
    // SAFETY: Forwarded from C++ `Thread::SecondaryCpuInitEarly` caller.
    unsafe { secondary_cpu_init_early(thread) };
}

/// Forwards C++ `ThreadDispatcher::GetCurrent`.
///
/// # Safety
///
/// The calling CPU must have a valid, initialized current [`Thread`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_current_user_thread() -> *mut crate::object::ThreadDispatcher {
    // SAFETY: Forwarded from C++ caller in thread context.
    unsafe { current_user_thread() }
}

/// Forwards C++ `Thread::SetCpuAffinity`.
///
/// # Safety
///
/// Callers must uphold the [`ThreadPtr::set_cpu_affinity`] preconditions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_cpu_affinity(
    thread: *mut Thread,
    affinity: cpu_mask_t,
) -> cpu_mask_t {
    // SAFETY: C++ passes `this`, which is never null.
    let thread = unsafe { thread_ptr_from_ffi(thread) };
    // SAFETY: Forwarded from the C++ caller.
    unsafe { thread.set_cpu_affinity(affinity) }
}

/// Forwards C++ `Thread::GetCpuAffinity`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_get_cpu_affinity(thread: &Thread) -> cpu_mask_t {
    thread.get_cpu_affinity()
}

/// Forwards C++ `Thread::LastCpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_last_cpu(thread: &Thread) -> cpu_num_t {
    thread.last_cpu()
}

/// Forwards C++ `Thread::LastCpuLocked`.
///
/// # Safety
///
/// The caller must hold shared or exclusive synchronization satisfying
/// `TA_REQ_SHARED(get_lock())` for the duration of the call, such that no concurrent writes or
/// live mutable references to `thread`'s `SchedulerState` exist.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_last_cpu_locked(thread: &Thread) -> cpu_num_t {
    // SAFETY: The caller guarantees read-only synchronization with no concurrent writes or live
    // mutable references to `thread`'s `SchedulerState`, and the temporary proof does not escape
    // this call.
    unsafe { ChainLockProof::assume_synchronized_read_only(thread) }.last_cpu()
}

/// Forwards C++ `Thread::Current::SoftFaultInRange`.
///
/// # Safety
///
/// Same requirements as [`soft_fault_in_range`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_current_soft_fault_in_range(
    va: usize,
    flags: u32,
    len: usize,
) -> zx_status_t {
    // SAFETY: Upheld by the caller.
    Status::result_into_raw(unsafe { soft_fault_in_range(va, flags, len) })
}

/// Forwards C++ `Thread::Current::Yield`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_current_yield() {
    r#yield();
}

/// Forwards C++ `Thread::Current::Sleep`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_current_sleep(deadline: InstantMono) -> zx_status_t {
    Status::result_into_raw(sleep(deadline))
}

/// Forwards C++ `Thread::Current::SleepRelative`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_current_sleep_relative(delay: DurationMono) -> zx_status_t {
    Status::result_into_raw(sleep_relative(delay))
}

/// Forwards C++ `Thread::Current::GetBacktrace(vaddr_t, Backtrace&)`.
///
/// # Safety
///
/// Same requirements as [`current_get_backtrace_with_fp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_current_get_backtrace_with_fp(
    fp: usize,
    out_bt: &mut Backtrace,
) {
    // SAFETY: Forwarded from C++ caller.
    unsafe { current_get_backtrace_with_fp(fp, out_bt) };
}

/// Forwards C++ `Thread::get_list_lock()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_get_list_lock()
-> *mut crate::ksync_kernel::KSpinlock<ThreadListLockClass> {
    core::ptr::from_ref(ThreadListLockClass::get()).cast_mut()
}

/// Initializes the global `THREAD_LIST` and `MIGRATE_LIST` during early boot ([`init_early`]).
///
/// # Safety
///
/// Must be called at most once, during single-threaded early boot in [`init_early`] before
/// any threads are inserted into `THREAD_LIST` or `MIGRATE_LIST`.
unsafe fn thread_lists_init() {
    // SAFETY: Called once during single-threaded early boot in `init_early()`.
    unsafe {
        Pin::static_ref(&THREAD_LIST).init_pin(KCell::pin_init(pin_init::pin_init!(SyncList {
            list <- ThreadList::new(),
        })));
        Pin::static_ref(&MIGRATE_LIST).init_pin(KCell::pin_init(pin_init::pin_init!(SyncList {
            list <- MigrateList::new(),
        })));
    }
}

/// Allocates and constructs an empty named [`Thread`] on the kernel heap without initializing its
/// kernel stack or placing it in the global thread list. Returns null on allocation failure.
///
/// # Safety
///
/// If `name_len != 0`, `name` must be non-null and point to `name_len` consecutive initialized
/// bytes within a single allocated object (`name_len <= isize::MAX`) that are not mutated for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_create_empty_named(
    name: *const c_char,
    name_len: usize,
) -> *mut Thread {
    // SAFETY: Caller guarantees that if `name_len != 0`, `name` is non-null and valid for reads
    // of `name_len` initialized, immutable bytes within a single allocation (`<= isize::MAX`).
    // `zr::slice_from_raw_parts` returns `&[]` when `name_len == 0` without dereferencing `name`.
    let name_bytes = unsafe { zr::slice_from_raw_parts(name.cast::<u8>(), name_len) };
    Thread::try_alloc(name_bytes).map_or(ptr::null_mut(), NonNull::as_ptr)
}

/// Destroys and frees a heap-allocated [`Thread`] previously returned by
/// [`rust_thread_create_empty_named`] (or does nothing if `thread` is null).
///
/// # Safety
///
/// Same requirements as [`Thread::delete`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_delete(thread: *mut Thread) {
    // SAFETY: Forwarded from C++ caller upholding `Thread::delete`'s contract.
    unsafe { Thread::delete(thread) };
}

/// Forwards C++ `Thread::SetMigrateFn`.
///
/// # Safety
///
/// - Same requirements as [`Thread::set_migrate_fn`].
/// - If `func` is `Some(f)`, `f(ctx, thread, stage)` must be sound to invoke whenever the
///   scheduler calls the migration callback with `thread`'s lock held and interrupts disabled, and
///   `ctx` must remain valid until the callback is cleared or the thread is joined/destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_migrate_fn(
    thread: &Thread,
    func: Option<RawMigrateFn>,
    ctx: *mut c_void,
) {
    // SAFETY: Forwarded from C++ `Thread::SetMigrateFn` caller.
    let cb = unsafe { wrap_cpp_migrate_fn(func, ctx) };
    // SAFETY: Forwarded from C++ `Thread::SetMigrateFn` caller.
    unsafe { thread.set_migrate_fn(cb) };
}

/// Forwards C++ `Thread::SetMigrateFnLocked`.
///
/// # Safety
///
/// - The caller must hold `Thread::get_list_lock()` and `thread`'s lock (`TA_REQ(get_list_lock(),
///   get_lock())`) in an active `ChainLockTransaction`, with no conflicting [`ChainLockProof`] for
///   `thread` or [`LockToken<ThreadListLockClass>`] live across this call.
/// - Same remaining requirements as [`ChainLockProof::set_migrate_fn_locked`].
/// - Any previously installed migration callback on `thread` must be safe to drop while holding
///   `get_list_lock()` and `thread`'s lock with interrupts disabled (trivially satisfied for `None`
///   or C++-registered callbacks).
/// - If `func` is `Some(f)`, `f(ctx, thread, stage)` must be sound to invoke whenever the
///   scheduler calls the migration callback with `thread`'s lock held and interrupts disabled, and
///   `ctx` must remain valid until the callback is cleared or the thread is joined/destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_migrate_fn_locked(
    thread: &Thread,
    func: Option<RawMigrateFn>,
    ctx: *mut c_void,
) {
    // SAFETY: The caller holds `Thread::get_list_lock()` and `thread`'s lock (`TA_REQ`) in an
    // active `ChainLockTransaction` (debug-asserted below), no conflicting `ChainLockProof` or
    // `LockToken<ThreadListLockClass>` is live, any replaced callback is safe to drop while
    // locked, and the caller upholds `set_migrate_fn_locked`'s remaining preconditions.
    unsafe {
        let token = ChainLockTransaction::assert_active();
        thread.lock().assert_held(&token);
        let mut list_token = Thread::get_list_lock().assert_held();
        let cb = wrap_cpp_migrate_fn(func, ctx);
        let old = ChainLockProof::assume_held_unchecked(thread)
            .set_migrate_fn_locked(cb, &mut list_token);
        drop(old);
    }
}

/// Forwards C++ `Thread::SetContextSwitchFnLocked`.
///
/// # Safety
///
/// - The caller must hold `thread`'s lock (`TA_REQ(get_lock())`) in an active
///   `ChainLockTransaction`, with no conflicting [`ChainLockProof`] for `thread` live across this
///   call.
/// - Any previously installed context-switch callback on `thread` must be safe to drop while
///   holding `thread`'s lock with interrupts disabled (trivially satisfied for `None` or
///   C++-registered callbacks).
/// - If `func` is `Some(f)`, `f(ctx)` must be sound to invoke whenever the scheduler calls the
///   context-switch callback with `thread`'s lock held and interrupts disabled, and `ctx` must
///   remain valid until the callback is cleared or the thread is joined/destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_set_context_switch_fn_locked(
    thread: &Thread,
    func: Option<RawContextSwitchFn>,
    ctx: *mut c_void,
) {
    // SAFETY: The caller holds `thread`'s lock (`TA_REQ`) in an active `ChainLockTransaction`
    // (debug-asserted below), no conflicting `ChainLockProof` is live, and any replaced callback
    // is safe to drop while locked.
    unsafe {
        let token = ChainLockTransaction::assert_active();
        thread.lock().assert_held(&token);
        let cb = wrap_cpp_context_switch_fn(func, ctx);
        let old = ChainLockProof::assume_held_unchecked(thread).set_context_switch_fn_locked(cb);
        drop(old);
    }
}

/// Forwards C++ `ktrace_report_live_threads`.
///
/// # Safety
///
/// [`init_early`] must have completed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_ktrace_report_live_threads() {
    // SAFETY: Forwarded from C++ `ktrace_report_live_threads` caller.
    unsafe { ktrace_report_live_threads() };
}

/// The default trampoline used when running the Thread. This can be
/// replaced by the `alt_trampoline` parameter to `create_etc()`.
///
/// [`create_etc`] and [`revive_idle_power_thread`] install this function directly as the
/// thread's initial PC (`arch_thread_initialize`), so there is no intermediate frame.
///
/// Thread trampolines are always called with:
///
/// 1. The thread's lock held
/// 2. interrupts disabled.
///
/// Implementers of trampolines must always remember to unconditionally release
/// the current thread's lock, and *then* enable interrupts, before jumping to
/// the thread's entry point.
///
/// Sadly, there does not seem to be a good way to statically annotate this.
///
/// # Safety
///
/// Must only be entered as the initial PC of a newly context-switched thread, with interrupts
/// disabled and the `ChainLockTransaction` registered by `Scheduler::reschedule_common` still
/// active and holding both the previous thread's lock and the current thread's lock under its
/// reserved scheduler token.
unsafe extern "C" fn rust_thread_trampoline() -> ! {
    // Handle the special case of starting a new thread for the first time,
    // releasing the previous thread's lock as well as the current thread's lock
    // in the process, before restoring interrupts and dropping into the thread's
    // entry point.
    //
    // SAFETY: The caller guarantees this is the initial trampoline entry with interrupts disabled
    // and the `reschedule_common` transaction active holding both locks with the reserved scheduler
    // token.
    unsafe { scheduler::trampoline_lock_handoff() };
    enable_ints();

    // Call into the thread's entry point, and call exit when finished.
    let current = current_get();
    // SAFETY: `current` is the valid, live current thread; projecting the field forms no
    // reference, so no `&Thread` is live across the indirect call, and `entry`/`arg` were written
    // once by `TaskState::init` before this thread was published.
    let (entry, arg) = unsafe {
        let task_state = &raw const (*current).task_state;
        ((*task_state).entry, (*task_state).arg)
    };
    // `CreateEtc` installs this default trampoline only when no `alt_trampoline` is given, and
    // every `CreateEtc` caller that passes a null `entry` supplies an `alt_trampoline`, so `entry`
    // is non-null here; a null `entry` is a caller bug, so panic.
    let entry =
        entry.expect("the default trampoline is only installed for threads created with an entry");
    // TODO(https://fxbug.dev/528405969): The C++ `thread_start_routine` typedef in
    // `kernel/thread.h` is `[[clang::cfi_unchecked_callee]]`. Rust has no equivalent opt-out;
    // revisit this indirect call if kernel Rust ever enables KCFI.
    // SAFETY: The caller of `create_etc` / `TaskState::init` guaranteed `entry` and `arg` are
    // valid to invoke on this new thread.
    let ret = unsafe { entry(arg) };
    // SAFETY: `entry` has returned in normal running non-idle thread context with interrupts
    // enabled, blocking permitted, and no locks or `ChainLockTransaction` held.
    unsafe { current_exit(ret) }
}

/// Slow path for C++ `PreemptionState::PreemptReenable`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState` on a running thread
/// (`ThreadState::Running`) in a context where blocking is allowed (`!blocking_disallowed()`), the
/// call pairs with a prior unmatched `PreemptDisable` on the current thread, the caller has
/// already decremented the preempt disable count once (observing `old_state` with
/// `PreemptionState::eager_resched_disable_count_of(old_state) == 0 &&
/// PreemptionState::preempt_disable_count_of(old_state) == 1`, or an underflow with
/// `PreemptionState::preempt_disable_count_of(old_state) == 0`), and the call does not drop
/// preemption back to enabled while any `PreemptDisabledToken` is live.
#[cold]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_preempt_reenable_slow(
    preemption_state: &PreemptionState,
    old_state: u32,
) {
    // SAFETY: The caller guarantees this is the current thread's state and the slow-path
    // preconditions of `preempt_reenable_slow`.
    unsafe { preemption_state.preempt_reenable_slow(old_state) };
}

/// Slow path for C++ `PreemptionState::PreemptReenableDelayFlush`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`, interrupts are disabled and
/// blocking is disallowed, the call pairs with a prior unmatched `PreemptDisable` on the current
/// thread, the caller has already decremented the preempt disable count once (observing
/// `old_state` with `PreemptionState::eager_resched_disable_count_of(old_state) == 0 &&
/// PreemptionState::preempt_disable_count_of(old_state) == 1` and `old_state != 1`, or an
/// underflow with `PreemptionState::preempt_disable_count_of(old_state) == 0`), and the call does
/// not drop preemption back to enabled while any `PreemptDisabledToken` is live.
#[cold]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_preempt_reenable_delay_flush_slow(
    preemption_state: &PreemptionState,
    old_state: u32,
) -> bool {
    preemption_state.preempt_reenable_delay_flush_slow(old_state)
}

/// Slow path for C++ `PreemptionState::EagerReschedReenable`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`, when
/// `PreemptionState::preempt_disable_count_of(old_state) == 0` the current thread is in
/// `ThreadState::Running` in a context where blocking is allowed (`!blocking_disallowed()`), the
/// call pairs with a prior unmatched `EagerReschedDisable` on the current thread, the caller has
/// already decremented the eager resched disable count once (observing `old_state` with
/// `PreemptionState::eager_resched_disable_count_of(old_state) == 1`, or an underflow with
/// `PreemptionState::eager_resched_disable_count_of(old_state) == 0`), and the call does not drop
/// preemption back to enabled while any `PreemptDisabledToken` is live.
#[cold]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_eager_resched_reenable_slow(
    preemption_state: &PreemptionState,
    old_state: u32,
) {
    // SAFETY: The caller guarantees this is the current thread's state and the slow-path
    // preconditions of `eager_resched_reenable_slow`.
    unsafe { preemption_state.eager_resched_reenable_slow(old_state) };
}

/// Forwards C++ `PreemptionState::SetTimesliceExtension`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_set_timeslice_extension(
    preemption_state: &PreemptionState,
    extension_duration: zx_types::zx_duration_mono_t,
) -> bool {
    preemption_state.set_timeslice_extension(extension_duration)
}

/// Forwards C++ `PreemptionState::ClearTimesliceExtension`.
///
/// # Safety
/// `preemption_state` is the current thread's `PreemptionState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_preemption_state_clear_timeslice_extension(
    preemption_state: &PreemptionState,
) {
    // SAFETY: The caller guarantees this is the current thread's state.
    unsafe { preemption_state.clear_timeslice_extension() };
}

/// Forwards C++ `PreemptionState::PreemptSetPending`.
///
/// Interrupts and preemption must be disabled and blocking disallowed (interrupt-handler
/// context), as `debug_assert!`ed by [`PreemptionState::preempt_set_pending`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_preemption_state_preempt_set_pending(
    preemption_state: &PreemptionState,
    reschedule_mask: crate::kernel::types::cpu_mask_t,
) {
    preemption_state.preempt_set_pending(reschedule_mask);
}

/// Unit tests for Thread.
#[cfg(ktest)]
#[unittest::suite(name = "thread")]
mod tests {
    use super::{
        AnnotatedAutoEagerReschedDisabler, AnnotatedAutoPreemptDisabler, AutoPreemptDisabler,
        ChainLocked, ContextSwitchFn, DEFAULT_PRIORITY, Interruptible, LOW_PRIORITY,
        MAX_SLEEP_SLACK, MIN_SLEEP_SLACK, MigrateFn, MigrateStage, SchedulerStateBaseProfile,
        ScopedMemoryAllocationDisabled, ScopedMemoryStall, THREAD_DETACHED_EXIT_COUNT, Thread,
        ThreadLockGuard, ThreadPtr, ThreadStallState, ThreadStateKind, accessed_fault, create_etc,
        create_with_priority, current_exit, current_get, current_get_backtrace,
        current_get_backtrace_with_fp, current_memory_allocation_state, current_runtime,
        eager_resched_disable_count, format_cpu_name, layout, page_fault, reschedule, sleep,
        sleep_etc, sleep_interruptible, sleep_relative, sleep_slack, soft_fault,
        soft_fault_in_range, thread_id_to_thread_slow, thread_state_to_str, r#yield,
    };
    use crate::arch_rs::{curr_cpu_num, enable_ints};
    use crate::backtrace::Backtrace;
    use crate::kconcurrent::{
        ChainLockProof, ChainLockTransaction, DONE, IRQ_SAVE_OPTION, OwnerGuard,
        SingleChainLockGuard, clt_tag, single_chain_lock_guard_owned,
    };
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::event::Event;
    use crate::kernel::mp::{CPU_MASK_ALL, INVALID_CPU, cpu_num_to_mask, get_online_mask};
    use crate::kernel::owned_wait_queue::{DefaultWakeHooks, OwnedWaitQueue};
    use crate::kernel::scheduler::{Scheduler, peek_active_mask, trampoline_lock_handoff};
    use crate::kernel::types::{
        BOOT_CPU_ID, cpu_mask_t, cpu_num_t, highest_cpu_set, lowest_cpu_set,
    };
    use crate::kernel::wait_queue::{
        BlockedThreadTreeTag, MinRelativeDeadlineTraits, PRIMARY_KEY_ZERO, ResourceOwnership,
    };
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
    use core::cell::Cell;
    use core::ffi::{CStr, c_int, c_void};
    use core::marker::{PhantomData, PhantomPinned};
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
    use fbl::{WavlTreeAugmentedInvariantObserverTraits, WavlTreeContainable, WavlTreeKeyable};
    use kprint::kprintln;
    use pin_init::{PinInit, pin_data, pinned_drop};
    use unittest::{
        assert_eq, assert_gt, assert_ne, assert_true, expect_eq, expect_err, expect_false,
        expect_ge, expect_ne, expect_ok, expect_true, unwrap_ok, unwrap_some,
    };
    use zx_status::Status;
    use zx_types::{ZX_ERR_BAD_STATE, ZX_ERR_TIMED_OUT, ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_OK};

    /// Verifies AnnotatedAutoEagerReschedDisabler and ThreadPtr accessors on the current thread.
    #[test]
    fn eager_resched_disabler_and_thread_accessors() {
        let base_count = eager_resched_disable_count();
        {
            let mut disabler = AnnotatedAutoEagerReschedDisabler::new();
            assert_eq!(eager_resched_disable_count(), base_count + 1);
            disabler.enable();
            assert_eq!(eager_resched_disable_count(), base_count);
            // A second `enable` is a no-op.
            disabler.enable();
            assert_eq!(eager_resched_disable_count(), base_count);

            let _default_disabler = AnnotatedAutoEagerReschedDisabler::default();
            assert_eq!(eager_resched_disable_count(), base_count + 1);
        }
        assert_eq!(eager_resched_disable_count(), base_count);

        // SAFETY: Called in a running kernel test thread context.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` points to the valid current thread.
        let _tid = unsafe { current.tid() };
        // SAFETY: `current` points to the valid current thread.
        let _user_thread: *mut crate::object::ThreadDispatcher =
            unsafe { current.user_thread_raw() };

        // SAFETY: `current` points to the valid current thread.
        let flow1 = unsafe { current.take_next_lock_flow_id() };
        // SAFETY: `current` points to the valid current thread.
        let flow_obs = unsafe { current.lock_flow_id() };
        assert_eq!(flow1, flow_obs);

        // SAFETY: `current` points to the valid current thread.
        let flow2 = unsafe { current.take_next_lock_flow_id() };
        assert_ne!(flow1, flow2);

        {
            // SAFETY: `current` points to the valid current thread, which outlives `lock`.
            let lock = unsafe { current.get_lock() };
            let held = SingleChainLockGuard::scope(
                IRQ_SAVE_OPTION,
                lock,
                clt_tag!("thread_rust_test"),
                |guard| lock.is_held(guard.token()),
            );
            assert_true!(held);
            assert_true!(lock.is_unlocked());
        }
    }

    /// Verifies `ThreadLockGuard::proof` and `OwnerGuard::<Thread>` on the current `Thread`.
    #[test]
    fn thread_lock_proof_and_owner_guard() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };
        expect_true!(current.lock().is_unlocked());

        {
            thread_lock_guard!(
                guard,
                // SAFETY: `current_ptr` stays live past this scope, no CLT is active, and we do
                // not block.
                unsafe {
                    ThreadLockGuard::target(current_ptr, clt_tag!("thread_lock_guard_proof"))
                }
            );
            expect_eq!(guard.thread(), current_ptr);
            expect_false!(current.lock().is_unlocked());
            let proof = guard.proof();
            expect_true!(core::ptr::eq(proof.owner(), current));
        }
        expect_true!(current.lock().is_unlocked());

        {
            single_chain_lock_guard_owned!(
                scope,
                current,
                IRQ_SAVE_OPTION,
                clt_tag!("thread_owned_scope")
            );
            expect_true!(current.lock().is_held(scope.token()));
            let proof = scope.proof();
            expect_true!(core::ptr::eq(proof.owner(), current));
        }
        expect_true!(current.lock().is_unlocked());

        let mut held = false;
        let mut owner_matches = false;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("thread_owner_guard"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                held = guard.is_held() && current.lock().is_held(token);
                let proof = guard.proof();
                owner_matches = core::ptr::eq(proof.owner(), current);
                guard.release();
                DONE
            },
        );
        expect_true!(held);
        expect_true!(owner_matches);
        expect_true!(current.lock().is_unlocked());
    }

    /// Verifies the `ChainLockProof<'_, Thread>` accessors on the (unblocked) running thread.
    #[test]
    fn thread_proof_guarded_fields() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };
        // The lock is the `ChainLockable` base subobject, so it lives at the thread's address.
        expect_eq!(core::ptr::from_ref(current.lock()).addr(), current_ptr.addr());
        expect_true!(current.canary.valid());
        current.assert_canary();
        expect_true!(current.check_kill_or_suspend_signal().is_ok());
        expect_true!(current.check_kill_or_suspend_signal_mask(0).is_ok());
        expect_true!(current.check_kill_or_suspend_signal_mask(u32::MAX).is_ok());

        let mut running = false;
        let mut state_transitions_match = false;
        let mut not_blocked = false;
        let mut interruptible = Interruptible::Yes;
        let mut no_owned_wait_queues = false;
        let mut can_own_wait_queues = false;
        let mut user_state_saved = true;
        let mut migrate_pending = true;
        let mut policy_exception = (u32::MAX, u32::MAX);
        let mut scheduler_state_matches = false;
        let mut wait_queue_state_matches = false;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("thread_proof_guarded_fields"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                let mut proof = guard.proof();

                let orig_state = proof.state();
                running = orig_state == ThreadStateKind::Running;
                proof.set_blocked();
                let blocked_ok = proof.state() == ThreadStateKind::Blocked;
                proof.set_blocked_read_lock();
                let blocked_read_ok = proof.state() == ThreadStateKind::BlockedReadLock;
                proof.set_sleeping();
                let sleeping_ok = proof.state() == ThreadStateKind::Sleeping;
                proof.set_state(orig_state);
                state_transitions_match =
                    blocked_ok && blocked_read_ok && sleeping_ok && proof.state() == orig_state;

                // `ThreadState` members (wait_queue.rs accessors).
                let wq_state = proof.wait_queue_state();
                not_blocked = !wq_state.in_wait_queue(proof.token())
                    && wq_state.blocking_wait_queue(proof.token()).is_none();
                // `blocked_status` keeps the last wake status until the next block, so only check
                // that it reads back.
                let _ = wq_state.blocked_status(proof.token());
                interruptible = wq_state.interruptible(proof.token());

                // `Thread` members.
                no_owned_wait_queues = proof.owned_wait_queues().is_empty()
                    && proof.owned_wait_queues_mut().is_empty();
                can_own_wait_queues = proof.can_own_wait_queues();
                user_state_saved = proof.user_state_saved();
                migrate_pending = proof.migrate_pending();
                policy_exception =
                    (proof.extra_policy_exception_code(), proof.extra_policy_exception_data());

                // Writing the current values back through the `_mut` accessors leaves the thread
                // unchanged.
                *proof.extra_policy_exception_code_mut() = policy_exception.0;
                *proof.extra_policy_exception_data_mut() = policy_exception.1;
                *proof.can_own_wait_queues_mut() = can_own_wait_queues;
                *proof.user_state_saved_mut() = user_state_saved;
                *proof.migrate_pending_mut() = migrate_pending;

                // The proof's `SchedulerState` facade and `ThreadState` sit over the members at
                // the offsets `<kernel/thread_layout.h>` declares.
                scheduler_state_matches = core::ptr::from_ref(proof.scheduler_state()).addr()
                    == current_ptr.addr() + layout::ThreadLayout_kSchedulerStateOffset;
                wait_queue_state_matches = core::ptr::from_ref(proof.wait_queue_state()).addr()
                    == current_ptr.addr() + layout::ThreadLayout_kWaitQueueStateOffset;
                guard.release();
                DONE
            },
        );
        expect_true!(running);
        expect_true!(state_transitions_match);
        expect_true!(not_blocked);
        expect_eq!(interruptible, Interruptible::No);
        expect_true!(no_owned_wait_queues);
        expect_true!(can_own_wait_queues);
        expect_false!(user_state_saved);
        expect_false!(migrate_pending);
        expect_eq!(policy_exception.0, 0);
        expect_eq!(policy_exception.1, 0);
        expect_true!(scheduler_state_matches);
        expect_true!(wait_queue_state_matches);
        expect_true!(current.lock().is_unlocked());
    }

    /// Verifies WAVL tree traits and `MinRelativeDeadlineTraits` on an unqueued `Thread`.
    #[test]
    fn wavl_tree_traits_on_thread() {
        let current_ptr = current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &Thread = unsafe { &*current_ptr };

        // The running test thread is not in any wait queue.
        let node = <Thread as WavlTreeContainable<Thread, BlockedThreadTreeTag>>::get_node(current);
        expect_false!(node.in_container());

        let mut key: (InstantMono, usize) = (InstantMono::INFINITE, 0);
        let mut node_deadline = DurationMono::ZERO;
        let mut subtree_deadline = DurationMono::ZERO;
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("wavl_tree_traits_on_thread"),
            |token| {
                let mut guard = OwnerGuard::new(current, token);
                ChainLockTransaction::finalize(token);
                let _proof = guard.proof();
                // SAFETY: The current thread is not in any wait queue and we hold its lock, which
                // satisfies the synchronization preconditions of the WAVL tree trait hooks.
                unsafe {
                    key = <Thread as WavlTreeKeyable<(InstantMono, usize)>>::get_key(current);
                    node_deadline = MinRelativeDeadlineTraits::get_node_value(current);
                    subtree_deadline = MinRelativeDeadlineTraits::get_subtree_value(current);
                }
                guard.release();
                DONE
            },
        );
        // `WaitQueueCollection::Remove` only resets the sort key in debug builds, so a thread
        // that has blocked before may carry a stale key in release builds.
        if cfg!(debug_assertions) {
            expect_eq!(key.0.0, PRIMARY_KEY_ZERO.0);
        }
        expect_eq!(key.1, current_ptr.addr());
        // The running test thread has a fair profile, so its relative deadline is `INFINITE`.
        expect_eq!(node_deadline.0, DurationMono::INFINITE.0);
        expect_eq!(subtree_deadline.0, DurationMono::INFINITE.0);
        expect_eq!(
            MinRelativeDeadlineTraits::combine_values(
                DurationMono::from_millis(10),
                DurationMono::INFINITE
            )
            .0,
            DurationMono::from_millis(10).0
        );
    }

    /// Shared with a helper thread that waits on `event` until `deadline` (monotonic) and exits
    /// with `retcode`, or with the raw status `wait` returned on failure.
    struct WaitCtx {
        event: *const Event,
        deadline: InstantMono,
        retcode: i32,
    }

    impl WaitCtx {
        /// A generous but finite deadline, so a failed test self-terminates via the wait queue's
        /// timeout instead of wedging the join.
        fn ten_seconds(event: &Event) -> Self {
            Self {
                event: core::ptr::from_ref(event),
                deadline: InstantMono(current_mono_time().0 + 10 * 1_000_000_000),
                retcode: zx_types::ZX_OK,
            }
        }
    }

    extern "C" fn wait_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `WaitCtx` the spawning test keeps alive until it joins us.
        let ctx = unsafe { &*arg.cast::<WaitCtx>() };
        // SAFETY: As above for the event the context points at.
        let event = unsafe { &*ctx.event };
        match event.wait_deadline(ctx.deadline, super::Interruptible::No) {
            Ok(()) => ctx.retcode,
            Err(status) => status.into_raw(),
        }
    }

    /// Waits for `waiter` to enter `THREAD_BLOCKED` in a wait queue.
    fn wait_until_blocked(waiter: super::ThreadPtr) -> bool {
        for _ in 0..5000 {
            // SAFETY: Callers have not joined `waiter` yet.
            if unsafe { waiter.is_blocked() } {
                return true;
            }
            let _ = super::sleep_relative(DurationMono(1_000_000)); // 1ms
        }
        false
    }

    /// A helper thread parked in `ctx`'s event queue. Its `Thread` can be inspected and poked
    /// through [`ParkedWaiter::thread`] while it sits there; it is signaled and joined exactly
    /// once, either by [`ParkedWaiter::join`] or on drop, so an early test return never leaves
    /// it live.
    struct ParkedWaiter<'a> {
        ctx: &'a WaitCtx,
        thread: Option<super::ThreadPtr>,
    }

    impl<'a> ParkedWaiter<'a> {
        /// Spawns the waiter and waits for it to block; `None` if either step fails.
        fn spawn(ctx: &'a WaitCtx) -> Option<Self> {
            // SAFETY: `ctx` outlives the thread, which `join`/`Drop` joins.
            let thread = unsafe {
                super::spawn(
                    c"thread_rust_waiter".as_ptr(),
                    wait_entry,
                    core::ptr::from_ref(ctx).cast_mut().cast(),
                )
            }
            .ok()?;
            let waiter = Self { ctx, thread: Some(thread) };
            wait_until_blocked(thread).then_some(waiter)
        }

        fn thread(&self) -> &super::Thread {
            // SAFETY: The waiter has not been joined (it is still in `self.thread`), so its
            // `Thread` is alive for as long as `self` is borrowed.
            unsafe { &*self.ptr().as_ptr() }
        }

        fn ptr(&self) -> super::ThreadPtr {
            self.thread.expect("only `join`/`joined_elsewhere` take the thread, consuming self")
        }

        /// Gives up the waiter without joining it, because some other thread already did.
        fn joined_elsewhere(mut self) {
            self.thread = None;
        }

        /// Opens the event and joins the waiter, returning its exit status.
        fn join(mut self) -> Result<i32, zx_status::Status> {
            let thread = self.thread.take().expect("only `join` takes the thread");
            // SAFETY: `self.event` outlives `self`.
            unsafe { &*self.ctx.event }.signal();
            // SAFETY: The thread was taken out of `self.thread`, so it is joined exactly once.
            unsafe { thread.join(InstantMono::INFINITE) }
        }
    }

    impl Drop for ParkedWaiter<'_> {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                // SAFETY: `self.event` outlives `self`.
                unsafe { &*self.ctx.event }.signal();
                // SAFETY: The thread was taken out of `self.thread`, so it is joined exactly once.
                let _ = unsafe { thread.join(InstantMono::INFINITE) };
            }
        }
    }

    /// Verifies the flag getters and setters on a parked helper thread, restoring its flags.
    // A helper is used rather than the running thread because a blocked thread's flags are not
    // concurrently modified by other systems, while the running thread's might be. Toggling
    // `FREE_STRUCT`, `IDLE`, and `DETACHED` here is safe because `waiter` is parked on `event` and
    // neither exits nor is scheduled until `waiter.join()`, after every flag is restored.
    #[test]
    fn test_thread_flags() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { deadline: InstantMono::INFINITE, ..WaitCtx::ten_seconds(&event) };
        let waiter = ParkedWaiter::spawn(&ctx);
        assert_true!(waiter.is_some(), "failed to spawn and park the waiter");
        let waiter = waiter.expect("checked above");
        let thread = waiter.thread();
        let saved = thread.flags();
        // Put every bit back however the test exits.
        let _restore = zr::defer(|| {
            // SAFETY: Restores the parked waiter's original flags before `waiter` is dropped.
            unsafe {
                thread.set_free_struct(saved & super::THREAD_FLAG_FREE_STRUCT != 0);
                thread.set_idle(saved & super::THREAD_FLAG_IDLE != 0);
                thread.set_detached(saved & super::THREAD_FLAG_DETACHED != 0);
            }
            thread.set_vcpu(saved & super::THREAD_FLAG_VCPU != 0);
        });

        expect_false!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_false!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });

        // SAFETY: `waiter` is parked in `event` and all flags are restored before it wakes/exits.
        unsafe { thread.set_free_struct(true) };
        expect_true!(thread.free_struct());
        expect_true!(thread.flags() & super::THREAD_FLAG_FREE_STRUCT != 0);
        // SAFETY: As above.
        unsafe { thread.set_free_struct(false) };
        expect_false!(thread.free_struct());
        // SAFETY: As above.
        unsafe { thread.set_free_struct(saved & super::THREAD_FLAG_FREE_STRUCT != 0) };
        expect_eq!(thread.free_struct(), saved & super::THREAD_FLAG_FREE_STRUCT != 0);

        // SAFETY: As above.
        unsafe { thread.set_idle(true) };
        expect_true!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_true!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });
        expect_true!(thread.flags() & super::THREAD_FLAG_IDLE != 0);
        // SAFETY: As above.
        unsafe { thread.set_idle(false) };
        expect_false!(thread.is_idle());
        // SAFETY: `thread` is a valid `Thread`.
        expect_false!(unsafe { Thread::is_idle_ptr(core::ptr::from_ref(thread)) });
        // SAFETY: As above.
        unsafe { thread.set_idle(saved & super::THREAD_FLAG_IDLE != 0) };
        expect_eq!(thread.is_idle(), saved & super::THREAD_FLAG_IDLE != 0);

        thread.set_vcpu(true);
        expect_true!(thread.vcpu());
        expect_true!(thread.flags() & super::THREAD_FLAG_VCPU != 0);
        thread.set_vcpu(false);
        expect_false!(thread.vcpu());
        thread.set_vcpu(saved & super::THREAD_FLAG_VCPU != 0);
        expect_eq!(thread.vcpu(), saved & super::THREAD_FLAG_VCPU != 0);

        // SAFETY: As above.
        unsafe { thread.set_detached(true) };
        expect_true!(thread.detached());
        expect_true!(thread.flags() & super::THREAD_FLAG_DETACHED != 0);
        // SAFETY: As above.
        unsafe { thread.set_detached(false) };
        expect_false!(thread.detached());
        // SAFETY: As above.
        unsafe { thread.set_detached(saved & super::THREAD_FLAG_DETACHED != 0) };
        expect_eq!(thread.detached(), saved & super::THREAD_FLAG_DETACHED != 0);

        expect_eq!(thread.flags(), saved);
        drop(_restore);
        expect_eq!(waiter.join().unwrap_or(ZX_ERR_BAD_STATE), ZX_OK);
    }

    /// Verifies the signal accessors and `check_kill_or_suspend_signal` on a parked helper thread.
    #[test]
    fn test_thread_signals() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx::ten_seconds(&event);
        let waiter = ParkedWaiter::spawn(&ctx);
        assert_true!(waiter.is_some(), "failed to spawn and park the waiter");
        let waiter = waiter.expect("checked above");
        let thread = waiter.thread();
        let initial = thread.signals();
        expect_ok!(thread.check_kill_or_suspend_signal());

        thread.set_signals(super::THREAD_SIGNAL_KILL);
        expect_true!(thread.signals() & super::THREAD_SIGNAL_KILL != 0);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_KILLED);
        thread.clear_signals(super::THREAD_SIGNAL_KILL);
        expect_true!(thread.signals() & super::THREAD_SIGNAL_KILL == 0);

        thread.set_signals(super::THREAD_SIGNAL_SUSPEND);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_RETRY);
        expect_err!(thread.check_kill_or_suspend_signal_mask(0), super::ZX_ERR_INTERNAL_INTR_RETRY);
        expect_ok!(thread.check_kill_or_suspend_signal_mask(super::THREAD_SIGNAL_SUSPEND));

        // When both KILL and SUSPEND are raised, KILL takes precedence unless masked.
        thread.set_signals(super::THREAD_SIGNAL_KILL);
        expect_err!(thread.check_kill_or_suspend_signal(), super::ZX_ERR_INTERNAL_INTR_KILLED);
        expect_err!(
            thread.check_kill_or_suspend_signal_mask(super::THREAD_SIGNAL_SUSPEND),
            super::ZX_ERR_INTERNAL_INTR_KILLED
        );
        expect_err!(
            thread.check_kill_or_suspend_signal_mask(super::THREAD_SIGNAL_KILL),
            super::ZX_ERR_INTERNAL_INTR_RETRY
        );
        expect_ok!(thread.check_kill_or_suspend_signal_mask(
            super::THREAD_SIGNAL_KILL | super::THREAD_SIGNAL_SUSPEND
        ));
        thread.clear_signals(super::THREAD_SIGNAL_KILL | super::THREAD_SIGNAL_SUSPEND);
        expect_ok!(thread.check_kill_or_suspend_signal());
        expect_eq!(thread.signals(), initial);

        expect_eq!(waiter.join().unwrap_or(ZX_ERR_BAD_STATE), ZX_OK);
    }

    /// Verifies `set_name` truncation and zero-fill on the current thread, restoring its name.
    #[test]
    fn test_thread_name() {
        let current_ptr = super::current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &super::Thread = unsafe { &*current_ptr };
        let saved = current.name();
        let _restore = zr::defer(|| {
            current.set_name(saved.as_bytes());
        });

        current.set_name(b"short");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..5].copy_from_slice(b"short");
        let mut buf = [0u8; ZX_MAX_NAME_LEN];
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        let mut ptr_buf = [0u8; ZX_MAX_NAME_LEN];
        // SAFETY: `current_ptr` points to the live current thread.
        unsafe { Thread::name_bytes_ptr(current_ptr, &mut ptr_buf) };
        expect_true!(ptr_buf == expected);
        // SAFETY: `current_ptr` points to the live current thread.
        expect_true!(unsafe { Thread::name_copy_ptr(current_ptr) }.as_bytes() == b"short");
        expect_eq!(super::rust_thread_name(current).addr(), current.name.as_ptr().addr());
        // SAFETY: The buffer is NUL-terminated and nothing else writes the test thread's name.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes() == b"short");

        let exact = [b'a'; ZX_MAX_NAME_LEN - 1];
        current.set_name(&exact);
        let mut expected = [b'a'; ZX_MAX_NAME_LEN];
        expected[ZX_MAX_NAME_LEN - 1] = 0;
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_eq!(current.name().as_bytes().len(), ZX_MAX_NAME_LEN - 1);

        let long = [b'b'; ZX_MAX_NAME_LEN + 8];
        current.set_name(&long);
        let mut expected = [b'b'; ZX_MAX_NAME_LEN];
        expected[ZX_MAX_NAME_LEN - 1] = 0;
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_eq!(cstr.to_bytes().len(), ZX_MAX_NAME_LEN - 1);

        // Embedded NUL bytes and non-UTF-8 bytes are copied verbatim into the buffer.
        current.set_name(b"ab\0cd");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..5].copy_from_slice(b"ab\0cd");
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_true!(current.name().as_bytes() == b"ab");
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes() == b"ab");

        current.set_name(b"a\xffb");
        let mut expected = [0u8; ZX_MAX_NAME_LEN];
        expected[..3].copy_from_slice(b"a\xffb");
        current.name_bytes(&mut buf);
        expect_true!(buf == expected);
        expect_true!(current.name().as_bytes() == b"a\xffb");

        current.set_name(b"");
        current.name_bytes(&mut buf);
        expect_true!(buf == [0u8; ZX_MAX_NAME_LEN]);
        expect_true!(current.name().as_bytes().is_empty());
        // SAFETY: As above.
        let cstr = unsafe { CStr::from_ptr(super::rust_thread_name(current).cast()) };
        expect_true!(cstr.to_bytes().is_empty());

        // `ThreadPtr::{set_name, name}` delegates to the underlying `Thread` buffer.
        // SAFETY: Threading is initialized during ktests and `thread_ptr` is the live current
        // thread.
        unsafe {
            let thread_ptr = super::ThreadPtr::current();
            thread_ptr.set_name(b"via-ptr");
            expect_true!(thread_ptr.name().as_bytes() == b"via-ptr");
        }
    }

    /// Verifies that a kernel thread has a nonzero tid and no pid.
    #[test]
    fn test_thread_tid_pid() {
        let current_ptr = super::current_get();
        // SAFETY: `current_get()` returns a valid pointer to the running thread, which stays alive
        // for the duration of this test.
        let current: &super::Thread = unsafe { &*current_ptr };
        expect_true!(current.tid() != 0);
        // SAFETY: `current_ptr` points to the valid current thread.
        expect_eq!(current.tid(), unsafe { super::ThreadPtr::current().tid() });
        expect_eq!(current.pid(), ZX_KOID_INVALID);
        // SAFETY: `current_ptr` points to the valid current thread.
        expect_eq!(current.pid(), unsafe { super::ThreadPtr::current().pid() });
    }

    /// Shared with a helper thread that joins `target` with `deadline` (monotonic) and exits with
    /// the target's retcode, or with the raw status `join` returned on failure; `joined` records
    /// whether the helper's join succeeded (and so consumed `target`).
    struct JoinCtx {
        target: super::ThreadPtr,
        deadline: InstantMono,
        joined: AtomicBool,
    }

    extern "C" fn join_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `JoinCtx` the spawning test keeps alive until it joins us.
        let ctx = unsafe { &*arg.cast::<JoinCtx>() };
        // SAFETY: The spawning test joins `target` itself only if `joined` stays false.
        match unsafe { ctx.target.join(ctx.deadline) } {
            Ok(retcode) => {
                ctx.joined.store(true, Ordering::Release);
                retcode
            }
            Err(status) => status.into_raw(),
        }
    }

    extern "C" fn double_entry(arg: *mut c_void) -> i32 {
        2 * (arg.addr() as i32)
    }

    /// Verifies a created thread's `TaskState` before it runs and its retcode after the trampoline.
    #[test]
    fn test_thread_spawn_and_join() {
        // SAFETY: `double_entry` only uses the value of `arg`.
        let thread = unsafe {
            super::create(
                c"thread_rust_double".as_ptr(),
                double_entry,
                core::ptr::without_provenance_mut(21),
            )
        };
        assert_true!(thread.is_ok(), "failed to create the thread");
        let thread = thread.expect("checked above");
        {
            // SAFETY: The thread is created but not resumed or joined, so its `Thread` is alive
            // and nothing else touches its task state.
            let task_state = unsafe { (*thread.as_ptr()).task_state() };
            expect_true!(task_state.entry().is_some());
            expect_eq!(task_state.arg().addr(), 21);
            // SAFETY: The thread has not started running, so nothing writes the retcode.
            expect_eq!(unsafe { task_state.retcode() }, 0);
        }
        // SAFETY: The thread has not been joined or destroyed.
        unsafe { thread.resume() };
        // SAFETY: The thread is joined exactly once.
        let retcode = unsafe { thread.join(InstantMono::INFINITE) };
        expect_eq!(retcode.unwrap_or(ZX_ERR_BAD_STATE), 42);
    }

    /// Creates and resumes a thread running `double_entry`, then waits until it has exited.
    ///
    /// The thread is neither joined nor detached, so it stays in `THREAD_DEATH` and keeps its
    /// memory until the caller joins or detaches it.
    fn create_exited_thread() -> Option<super::ThreadPtr> {
        // SAFETY: `double_entry` only uses the value of `arg`.
        let thread = unsafe {
            super::create(
                c"thread_rust_exited".as_ptr(),
                double_entry,
                core::ptr::without_provenance_mut(21),
            )
        }
        .ok()?;
        // SAFETY: The thread has not been joined or destroyed.
        unsafe { thread.resume() };
        wait_for_cond(|| {
            // SAFETY: The thread has been neither joined nor detached, so its `Thread` is alive.
            let t: &super::Thread = unsafe { &*thread.as_ptr() };
            single_chain_lock_guard_owned!(
                scope,
                t,
                IRQ_SAVE_OPTION,
                clt_tag!("create_exited_thread")
            );
            scope.proof().state() == ThreadStateKind::Death
        });
        Some(thread)
    }

    /// Verifies that `Thread::Detach` joins and frees a thread that has already exited.
    #[test]
    fn test_thread_detach_exited() {
        let thread = create_exited_thread();
        assert_true!(thread.is_some(), "failed to create the thread");
        let thread = thread.expect("checked above");
        // SAFETY: The thread has exited but has been neither joined nor detached, so it is alive.
        let tid = unsafe { thread.tid() };
        // SAFETY: The thread has been neither joined nor detached, and `thread` is not used again.
        expect_true!(unsafe { thread.detach() }.is_ok());
        expect_false!(on_thread_list(tid));
    }

    /// Verifies that `Thread::DetachAndResume` frees an exited thread without resuming it.
    #[test]
    fn test_thread_detach_and_resume_exited() {
        let thread = create_exited_thread();
        assert_true!(thread.is_some(), "failed to create the thread");
        let thread = thread.expect("checked above");
        // SAFETY: The thread has exited but has been neither joined nor detached, so it is alive.
        let tid = unsafe { thread.tid() };
        // SAFETY: The thread has been neither joined nor detached, and `thread` is not used again.
        expect_true!(unsafe { thread.detach_and_resume() }.is_ok());
        expect_false!(on_thread_list(tid));
    }

    /// Verifies that a joiner blocked in `TaskState::join` is woken with the target's retcode.
    #[test]
    fn test_thread_join_wakes_joiner() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { retcode: 7, ..WaitCtx::ten_seconds(&event) };
        let target = ParkedWaiter::spawn(&ctx);
        assert_true!(target.is_some(), "failed to spawn and park the target");
        let target = target.expect("checked above");

        let join_ctx = JoinCtx {
            target: target.ptr(),
            deadline: ctx.deadline,
            joined: AtomicBool::new(false),
        };
        // SAFETY: `join_ctx` outlives the joiner, which is joined below before returning.
        let joiner = unsafe {
            super::spawn(
                c"thread_rust_joiner".as_ptr(),
                join_entry,
                core::ptr::from_ref(&join_ctx).cast_mut().cast(),
            )
        };
        assert_true!(joiner.is_ok(), "failed to spawn the joiner");
        let joiner = joiner.expect("checked above");
        // No early returns from here until the joiner is joined.
        let joiner_blocked = wait_until_blocked(joiner);
        // Let the target exit; its `current_exit` wakes the joiner with its retcode.
        event.signal();
        // SAFETY: The joiner is joined exactly once.
        let joiner_retcode = unsafe { joiner.join(InstantMono::INFINITE) };
        let target_joined = join_ctx.joined.load(Ordering::Acquire);
        if target_joined {
            target.joined_elsewhere();
        }
        expect_true!(joiner_blocked);
        expect_eq!(joiner_retcode.unwrap_or(ZX_ERR_BAD_STATE), 7);
        expect_true!(target_joined);
    }

    /// Verifies that a detached exiting thread unlinks itself and cleans up via `free_dpc`.
    #[test]
    fn test_thread_detached_exit_runs_free_dpc() {
        let before_detached_exits = THREAD_DETACHED_EXIT_COUNT.sum_across_all_cpus();
        let started = AtomicBool::new(false);

        extern "C" fn detached_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `started`, which the parent does not drop until this
            // `Release` store is observed via `Acquire` load.
            let started = unsafe { &*arg.cast::<AtomicBool>() };
            started.store(true, Ordering::Release);
            0
        }

        let started_ptr: *mut c_void = core::ptr::from_ref(&started).cast_mut().cast();
        // SAFETY: `detached_entry` performs a single `Release` store to `started`, and the parent
        // waits unconditionally to observe it via `Acquire` load before dropping `started`.
        let thread =
            unsafe { super::create(c"thread_detached_exit".as_ptr(), detached_entry, started_ptr) };
        assert_true!(thread.is_ok(), "failed to create detached thread");
        let thread = thread.expect("checked above");
        // SAFETY: `thread` has not been resumed or detached yet.
        let tid = unsafe { thread.tid() };
        expect_true!(on_thread_list(tid));
        // SAFETY: `thread` has been neither joined nor detached, and is not used afterwards if
        // `detach_and_resume` succeeds.
        let detach_res = unsafe { thread.detach_and_resume() };
        expect_ok!(detach_res);
        if detach_res.is_err() {
            // SAFETY: `detach_and_resume` failed before resuming `thread`, so it is still in
            // `Initial` state and owned solely by this test.
            unsafe { thread.forget() };
        } else {
            // Wait unconditionally until `detached_entry` finishes its sole access to `started` so
            // `started` can never be dropped while the detached thread still holds a pointer to
            // it.
            wait_for_cond(|| started.load(Ordering::Acquire));

            let poll_deadline =
                InstantMono(current_mono_time().0 + DurationMono::from_seconds(10).0);
            while on_thread_list(tid) && current_mono_time().0 < poll_deadline.0 {
                let _ = sleep_relative(DurationMono::from_millis(1));
            }
            expect_false!(on_thread_list(tid));
            expect_ge!(THREAD_DETACHED_EXIT_COUNT.sum_across_all_cpus(), before_detached_exits + 1);
        }
    }

    /// Verifies that `current_exit` disowns any `OwnedWaitQueue` owned by the exiting thread.
    #[test]
    fn test_thread_exit_disowns_owned_wait_queues() {
        pin_init::stack_pin_init!(let owq = OwnedWaitQueue::init());
        pin_init::stack_pin_init!(let owner_release = Event::init_unsignaled());

        let owner_ctx = WaitCtx {
            event: core::ptr::from_ref(&*owner_release),
            deadline: InstantMono::INFINITE,
            retcode: ZX_OK,
        };
        let owner = ParkedWaiter::spawn(&owner_ctx);
        assert_true!(owner.is_some(), "failed to spawn and park owner");
        let owner = owner.expect("checked above");
        let owner_ptr = owner.ptr();

        struct WaiterCtx<'a> {
            owq: &'a OwnedWaitQueue,
            owner: ThreadPtr,
        }

        extern "C" fn owq_waiter_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `waiter_ctx`, which outlives this thread (joined below).
            let ctx = unsafe { &*arg.cast::<WaiterCtx<'_>>() };
            let aapd = AnnotatedAutoPreemptDisabler::new();
            // SAFETY: `ctx.owner` is a joinable thread that remains allocated until joined after
            // this thread returns.
            let owner_ref = unsafe { ctx.owner.as_ref() };
            let deadline = Deadline::no_slack(
                InstantMono(current_mono_time().0 + DurationMono::from_seconds(30).0).into(),
            );
            // SAFETY: `ctx.owq` is pinned on the test stack and outlives this thread, `owner_ref`
            // is a valid live `Thread`, preemption is disabled (`aapd.token()`), and no locks or
            // `ChainLockTransaction` are held.
            let res = unsafe {
                ctx.owq.block_and_assign_owner(
                    &deadline,
                    Some(owner_ref),
                    ResourceOwnership::Normal,
                    Interruptible::No,
                    aapd.token(),
                )
            };
            Status::result_into_raw(res)
        }

        let waiter_ctx = WaiterCtx { owq: &owq, owner: owner_ptr };
        // SAFETY: `waiter_ctx` outlives `waiter`, which is joined below before returning.
        let waiter = unsafe {
            super::spawn(
                c"thread_owq_waiter".as_ptr(),
                owq_waiter_entry,
                core::ptr::from_ref(&waiter_ctx).cast_mut().cast(),
            )
        };
        assert_true!(waiter.is_ok(), "failed to spawn owq waiter");
        let waiter = waiter.expect("checked above");

        let observe_owner = || {
            single_chain_lock_guard_owned!(
                scope,
                &*owq,
                IRQ_SAVE_OPTION,
                clt_tag!("thread_exit_owq_observe_owner")
            );
            scope.proof().queue_owner()
        };

        let waiter_blocked = wait_until_blocked(waiter);
        expect_true!(waiter_blocked);
        expect_true!(observe_owner() == Some(owner_ptr));

        // Let the owner exit while still owning `owq`. Do not join `owner` until after `waiter`
        // returns from `block_and_assign_owner`, so `owner` remains allocated for that call.
        owner_release.signal();
        let death_deadline = InstantMono(current_mono_time().0 + DurationMono::from_seconds(10).0);
        let mut owner_dead = false;
        while current_mono_time().0 < death_deadline.0 {
            let is_dead = {
                // SAFETY: `owner` is joinable and has not been joined yet.
                let owner_ref = unsafe { owner_ptr.as_ref() };
                single_chain_lock_guard_owned!(
                    scope,
                    owner_ref,
                    IRQ_SAVE_OPTION,
                    clt_tag!("thread_exit_owq_wait_death")
                );
                scope.proof().state() == ThreadStateKind::Death
            };
            if is_dead {
                owner_dead = true;
                break;
            }
            let _ = sleep_relative(DurationMono::from_millis(1));
        }
        expect_true!(owner_dead);
        expect_true!(observe_owner().is_none());

        let mut hooks = DefaultWakeHooks;
        let wake_res = owq.wake_threads(1, &mut hooks);
        expect_eq!(wake_res.woken, 1);

        // SAFETY: `waiter` is joined exactly once.
        let waiter_ret = unsafe { waiter.join(InstantMono::INFINITE) };
        expect_eq!(waiter_ret.unwrap_or(ZX_ERR_BAD_STATE), 0);
        // `owner` is joined by `ParkedWaiter::drop` here.
    }

    /// Verifies that `TaskState::join` with an already-past deadline fails with `ZX_ERR_TIMED_OUT`.
    #[test]
    fn test_thread_join_timeout() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let ctx = WaitCtx { retcode: 7, ..WaitCtx::ten_seconds(&event) };
        let target = ParkedWaiter::spawn(&ctx);
        assert_true!(target.is_some(), "failed to spawn and park the target");
        let target = target.expect("checked above");

        let join_ctx = JoinCtx {
            target: target.ptr(),
            deadline: InstantMono(current_mono_time().0 - 1),
            joined: AtomicBool::new(false),
        };
        // SAFETY: `join_ctx` outlives the joiner, which is joined below before returning.
        let joiner = unsafe {
            super::spawn(
                c"thread_rust_joiner".as_ptr(),
                join_entry,
                core::ptr::from_ref(&join_ctx).cast_mut().cast(),
            )
        };
        assert_true!(joiner.is_ok(), "failed to spawn the joiner");
        let joiner = joiner.expect("checked above");
        // SAFETY: The joiner is joined exactly once.
        let joiner_retcode = unsafe { joiner.join(InstantMono::INFINITE) };
        let target_joined = join_ctx.joined.load(Ordering::Acquire);
        // The target is normally still parked; release it and collect its retcode ourselves.
        let target_retcode = if target_joined {
            target.joined_elsewhere();
            None
        } else {
            Some(target.join().unwrap_or(ZX_ERR_BAD_STATE))
        };
        expect_eq!(joiner_retcode.unwrap_or(ZX_ERR_BAD_STATE), ZX_ERR_TIMED_OUT);
        expect_false!(target_joined);
        expect_eq!(target_retcode.unwrap_or(ZX_ERR_BAD_STATE), 7);
    }

    /// Returns whether a thread with `tid` is on the global thread list.
    fn on_thread_list(tid: zx_types::zx_koid_t) -> bool {
        ksync::lock!(
            let list_guard = Thread::get_list_lock().lock_policy::<super::IrqSavePolicy>()
        );
        // SAFETY: `init_early()` completed during early boot before kernel tests run.
        unsafe { super::thread_id_to_thread_slow(tid, list_guard.token()) }.is_some()
    }

    /// Verifies `create` lists the thread globally and `forget` unlists and frees it.
    #[test]
    fn test_thread_create_and_forget() {
        // SAFETY: `double_entry` only uses the value of `arg`; the thread is never resumed.
        let thread = unwrap_ok!(unsafe {
            super::create(c"thread_rust_forget".as_ptr(), double_entry, core::ptr::null_mut())
        });
        // SAFETY: The thread is alive until it is forgotten below.
        let tid = unsafe { thread.tid() };
        let listed_before = on_thread_list(tid);
        // SAFETY: The thread was never resumed, so it is in no wait queue and nothing else
        // references it; `thread` is not used afterwards.
        unsafe { thread.forget() };
        expect_true!(listed_before);
        expect_false!(on_thread_list(tid));
    }

    /// Verifies that joining a detached thread fails with `ZX_ERR_BAD_STATE`.
    #[test]
    fn test_thread_join_detached_fails() {
        // SAFETY: `double_entry` only uses the value of `arg`.
        let thread = unwrap_ok!(unsafe {
            super::create(c"thread_rust_detached".as_ptr(), double_entry, core::ptr::null_mut())
        });
        // SAFETY: The thread is still in its initial state, so it stays alive until resumed below.
        let tid = unsafe { thread.tid() };
        // SAFETY: The thread has not been joined or detached, and it stays in its initial state
        // (so alive) until it is resumed below.
        let detached = unsafe { thread.detach() };
        unwrap_ok!(detached);
        // SAFETY: As above; nothing else joins or resumes the thread.
        let joined = unsafe { thread.join(InstantMono::INFINITE) };
        assert_eq!(joined.err().map_or(ZX_OK, Status::into_raw), ZX_ERR_BAD_STATE);
        // SAFETY: The detached thread is still in its initial state and frees itself once it
        // exits; `thread` is not used afterwards.
        unsafe { thread.resume() };
        wait_for_cond(|| !on_thread_list(tid));
    }

    /// Verifies that resuming a thread that has already exited is a no-op.
    #[test]
    fn test_thread_resume_after_exit() {
        let thread = create_exited_thread();
        assert_true!(thread.is_some(), "failed to create the thread");
        let thread = thread.expect("checked above");
        // SAFETY: The thread has exited but has not been joined, so it is still alive.
        unsafe { thread.resume() };
        // SAFETY: The thread is joined exactly once.
        let retcode = unsafe { thread.join(InstantMono::INFINITE) };
        expect_eq!(retcode.unwrap_or(ZX_ERR_BAD_STATE), 42);
    }

    /// Verifies `create_etc` and `join` on caller-supplied `Thread` storage.
    #[test]
    fn test_thread_create_etc_with_storage() {
        // The `Thread` is constructed in place without `THREAD_FLAG_FREE_STRUCT`, so `join` runs
        // its destructor but leaves the storage to its owner.
        let mut storage = unwrap_ok!(kalloc::Box::<Thread>::try_new_uninit());
        let t = storage.as_mut_ptr();
        let profile = SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, true);
        // SAFETY: `t` is suitably aligned storage for a `Thread` that nothing else uses and that
        // outlives the thread (joined below), and `double_entry` only uses the value of `arg`.
        let thread = unwrap_ok!(unsafe {
            create_etc(
                t,
                b"thread_rust_storage",
                Some(double_entry),
                core::ptr::without_provenance_mut(5),
                &profile,
                None,
            )
        });
        expect_eq!(thread.as_raw(), t);
        // SAFETY: The thread is alive until it is joined below.
        expect_false!(unsafe { thread.as_ref() }.free_struct());
        // SAFETY: The thread has not been joined or destroyed.
        unsafe { thread.resume() };
        // SAFETY: The thread is joined exactly once.
        let retcode = unwrap_ok!(unsafe { thread.join(InstantMono::INFINITE) });
        expect_eq!(retcode, 10);
        // `join` ran the `Thread` destructor in place, so `storage` is freed as plain memory.
        drop(storage);
    }

    unsafe extern "C" {
        fn rand() -> c_int;
    }

    /// Wait for condition `cond` to become true, polling with slow exponential backoff
    /// to avoid pegging the CPU.
    fn wait_for_cond(mut cond: impl FnMut() -> bool) {
        let mut wait_duration = DurationMono::from_micros(1);
        while !cond() {
            let _ = sleep_relative(wait_duration);
            // Increase wait_duration time by ~10%.
            wait_duration = DurationMono(wait_duration.0 + (wait_duration.0 / 10 + 1));
        }
    }

    /// Create and manage a spinning thread.
    #[pin_data(PinnedDrop)]
    struct WorkerThread {
        name: Cell<&'static CStr>,
        worker_iterations: AtomicI32,
        worker_should_stop: AtomicI32,
        thread: Cell<Option<ThreadPtr>>,
        #[pin]
        _pin: PhantomData<PhantomPinned>,
    }

    impl WorkerThread {
        fn new(name: &'static CStr) -> impl PinInit<Self> {
            pin_init::pin_init!(Self {
                name: Cell::new(name),
                worker_iterations: AtomicI32::new(0),
                worker_should_stop: AtomicI32::new(0),
                thread: Cell::new(None),
                _pin: PhantomData,
            })
        }

        fn start(&self) {
            let thread = self.thread();
            // SAFETY: `thread` is valid and has not yet been resumed or joined.
            unsafe { thread.resume() };
        }

        fn join(&self) {
            if let Some(thread) = self.thread.take() {
                self.worker_should_stop.store(1, Ordering::SeqCst);
                // SAFETY: `thread` was taken from `self.thread`, so it is joined at most once.
                let result = unsafe { thread.join(InstantMono::INFINITE) };
                assert!(result.is_ok());
            }
        }

        fn wait_for_worker_progress(&self) {
            let start_iterations = self.worker_iterations();
            wait_for_cond(|| start_iterations != self.worker_iterations());
        }

        fn thread(&self) -> ThreadPtr {
            if let Some(thread) = self.thread.get() {
                return thread;
            }
            // SAFETY: `self` is pinned in place and all fields of `WorkerThread` are wrapped in
            // `UnsafeCell` (`Cell` / `AtomicI32`) or ZST (`PhantomData`), so
            // `(&raw const *self).cast_mut()` has shared-read-write provenance and remains valid
            // until `PinnedDrop::drop` joins the worker thread.
            let thread = unsafe {
                create_with_priority(
                    self.name.get().as_ptr(),
                    Self::worker_body,
                    (&raw const *self).cast_mut().cast(),
                    LOW_PRIORITY,
                )
            }
            .expect("Thread::Create failed");
            self.thread.set(Some(thread));
            thread
        }

        fn worker_iterations(&self) -> i32 {
            self.worker_iterations.load(Ordering::SeqCst)
        }

        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            let this = arg.cast::<WorkerThread>();
            // SAFETY: `this` points to a pinned `WorkerThread` that outlives this thread (joined
            // before `WorkerThread` is dropped). Projecting directly to the atomic fields avoids
            // forming a shared reference to the whole `WorkerThread` while the owner thread may
            // mutate `this.thread`.
            let (should_stop, iterations) =
                unsafe { (&(*this).worker_should_stop, &(*this).worker_iterations) };
            while should_stop.load(Ordering::SeqCst) == 0 {
                iterations.fetch_add(1, Ordering::SeqCst);
            }
            0
        }
    }

    #[pinned_drop]
    impl PinnedDrop for WorkerThread {
        fn drop(self: Pin<&mut Self>) {
            self.join();
        }
    }

    /// Tests setting CPU affinity on the current thread across online CPUs.
    #[test]
    fn set_affinity_self_test() {
        // Our worker thread will attempt to schedule itself onto each core, one at
        // a time, and ensure it landed in the right location.
        let mut online_cpus: cpu_mask_t = get_online_mask();
        assert_ne!(online_cpus, 0u32, "Expected at least one CPU to be online.");
        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `online_cpus` on the caller's stack, which remains valid
            // until the caller joins this thread.
            let online_cpus = unsafe { *(arg as *const cpu_mask_t) };
            // SAFETY: Called in a running kernel worker thread context.
            let this = unsafe { ThreadPtr::current() };

            for c in 0..=highest_cpu_set(online_cpus) {
                // Skip offline CPUs.
                if (cpu_num_to_mask(c) & online_cpus) == 0 {
                    continue;
                }

                // Set affinity to the given core.
                // SAFETY: `this` points to the valid current thread.
                let _ = unsafe { this.set_cpu_affinity(cpu_num_to_mask(c)) };

                // Ensure we are on the correct CPU.
                let current_cpu: cpu_num_t = curr_cpu_num();
                if current_cpu != c {
                    kprintln!(
                        "Expected to be running on CPU {}, but actually running on {}.",
                        c,
                        current_cpu
                    );
                    return Status::INTERNAL.into_raw();
                }
            }

            Status::result_into_raw(Ok(()))
        }
        // SAFETY: `worker_body` is a valid thread entry point and `online_cpus` outlives the
        // worker thread (joined below).
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_affinity_self_test_worker".as_ptr(),
                    worker_body,
                    (&raw mut online_cpus).cast(),
                    DEFAULT_PRIORITY,
                )
            },
            "thread_create failed."
        );
        // SAFETY: `worker` has just been created and not yet resumed or joined.
        unsafe { worker.resume() };

        // Wait for the worker thread to test itself.
        // SAFETY: `worker` is valid and has not been joined yet.
        let worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
        expect_eq!(worker_retcode, Status::result_into_raw(Ok(())), "Worker thread failed.");
    }

    /// Tests migrating another running worker thread across online CPUs via `set_cpu_affinity`.
    #[test]
    fn set_affinity_other_test() {
        struct WorkerState {
            current_cpu: AtomicU32,
            should_stop: AtomicI32,
        }
        let mut state = WorkerState {
            current_cpu: AtomicU32::new(INVALID_CPU),
            should_stop: AtomicI32::new(0),
        };

        // Start a worker, which reports the CPU it is running on.
        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `state` on the caller's stack, which remains valid until the
            // caller joins this thread.
            let state = unsafe { &*(arg as *const WorkerState) };
            while state.should_stop.load(Ordering::SeqCst) != 1 {
                state.current_cpu.store(curr_cpu_num(), Ordering::SeqCst);
            }
            0
        }
        // SAFETY: `worker_body` is a valid thread entry point and `state` outlives the worker
        // thread (joined below).
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_affinity_other_test_worker".as_ptr(),
                    worker_body,
                    (&raw mut state).cast(),
                    LOW_PRIORITY,
                )
            },
            "thread_create failed."
        );
        // SAFETY: `worker` has just been created and not yet resumed or joined.
        unsafe { worker.resume() };

        // Migrate the worker task amongst different threads.
        let online_cpus: cpu_mask_t = get_online_mask();
        assert_ne!(online_cpus, 0u32, "Expected at least one CPU to be online.");
        for c in 0..=highest_cpu_set(online_cpus) {
            // Skip offline CPUs.
            if (cpu_num_to_mask(c) & online_cpus) == 0 {
                continue;
            }

            // Set affinity to the given core.
            // SAFETY: `worker` is valid and has not been joined yet.
            let _ = unsafe { worker.set_cpu_affinity(cpu_num_to_mask(c)) };

            // Wait for it to land on the correct CPU.
            wait_for_cond(|| state.current_cpu.load(Ordering::SeqCst) == c);
        }

        // Done.
        state.should_stop.store(1, Ordering::SeqCst);
        // SAFETY: `worker` is valid and has not been joined yet.
        let _worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
    }

    /// Tests that `last_cpu` returns `INVALID_CPU` on a newly created, unstarted thread.
    #[test]
    fn thread_last_cpu_new_thread() {
        // Create a worker, but don't start it.
        pin_init::stack_pin_init!(let worker = WorkerThread::new(c"last_cpu_new_thread"));

        // Ensure we get INVALID_CPU as last cpu.
        assert_eq!(
            // SAFETY: `worker` has not been joined yet.
            unsafe { worker.thread().last_cpu() },
            INVALID_CPU,
            "Last CPU on unstarted thread invalid."
        );

        // Clean up the thread.
        worker.start();
        worker.join();
    }

    /// Tests that `last_cpu` reports the expected CPU as a running thread migrates across cores.
    #[test]
    fn thread_last_cpu_running_thread() {
        pin_init::stack_pin_init!(let worker = WorkerThread::new(c"last_cpu_running_thread"));
        worker.start();

        // Migrate the worker task across different CPUs.
        let online_cpus: cpu_mask_t = get_online_mask();
        assert_ne!(online_cpus, 0u32, "Expected at least one CPU to be online.");
        for c in 0..=highest_cpu_set(online_cpus) {
            // Skip offline CPUs.
            if (cpu_num_to_mask(c) & online_cpus) == 0 {
                continue;
            }

            // Set affinity to the given core.
            // SAFETY: Calling thread does not hold `worker`'s lock or an active chain lock
            // transaction.
            let _ = unsafe { worker.thread().set_cpu_affinity(cpu_num_to_mask(c)) };

            // Ensure it is reported at the correct CPU.
            // SAFETY: `worker` has not been joined yet.
            wait_for_cond(|| unsafe { worker.thread().last_cpu() } == c);
        }
    }

    /// Tests that setting an empty soft CPU affinity mask still allows the thread to make progress.
    #[test]
    fn thread_empty_soft_affinity_mask() {
        pin_init::stack_pin_init!(let worker = WorkerThread::new(c"empty_soft_affinity_mask"));
        worker.start();

        // Wait for the thread to start running.
        worker.wait_for_worker_progress();

        // Set affinity to an invalid (empty) mask.
        // SAFETY: Calling thread does not hold `worker`'s lock or an active chain lock transaction.
        let _ = unsafe { worker.thread().set_soft_cpu_affinity(0) };

        // Ensure that the thread is still running.
        worker.wait_for_worker_progress();
    }

    /// Tests interaction between conflicting soft and hard CPU affinity masks.
    #[test]
    fn thread_conflicting_soft_and_hard_affinity() {
        // Find two different CPUs to run our tests on.
        let online: cpu_mask_t = get_online_mask();
        assert_true!(online != 0, "No CPUs online.");
        let a: cpu_num_t = highest_cpu_set(online);
        let b: cpu_num_t = lowest_cpu_set(online);
        if a == b {
            // Skip test on single CPU machines.
            kprintln!("Only 1 CPU active in this machine. Skipping test.");
            return true;
        }

        pin_init::stack_pin_init!(
            let worker = WorkerThread::new(c"conflicting_soft_and_hard_affinity")
        );
        worker.start();

        // Set soft affinity to CPU A, wait for the thread to start running there.
        // SAFETY: Calling thread does not hold `worker`'s lock or an active chain lock transaction.
        let _ = unsafe { worker.thread().set_soft_cpu_affinity(cpu_num_to_mask(a)) };
        // SAFETY: `worker` has not been joined yet.
        wait_for_cond(|| unsafe { worker.thread().last_cpu() } == a);

        // Set hard affinity to CPU B, ensure the thread migrates there.
        // SAFETY: Calling thread does not hold `worker`'s lock or an active chain lock transaction.
        let _ = unsafe { worker.thread().set_cpu_affinity(cpu_num_to_mask(b)) };
        // SAFETY: `worker` has not been joined yet.
        wait_for_cond(|| unsafe { worker.thread().last_cpu() } == b);

        // Remove the hard affinity. Make sure the thread migrates back to CPU A.
        // SAFETY: Calling thread does not hold `worker`'s lock or an active chain lock transaction.
        let _ = unsafe { worker.thread().set_cpu_affinity(CPU_MASK_ALL) };
        // SAFETY: `worker` has not been joined yet.
        wait_for_cond(|| unsafe { worker.thread().last_cpu() } == a);
    }

    /// Test-only `Send` wrapper for raw pointers to stack/heap state that outlives a worker thread.
    struct SendPtr<T>(*mut T);

    impl<T> SendPtr<T> {
        #[inline]
        fn as_ptr(&self) -> *mut T {
            self.0
        }
    }

    // SAFETY: Tests using `SendPtr` join the worker thread (destroying any installed callbacks)
    // before reading or dropping the pointed-to state, and synchronize concurrent access via the
    // owning `Thread`'s lock or atomics.
    unsafe impl<T> Send for SendPtr<T> {}

    /// Tests that `set_migrate_fn` invokes the migration callback across Save, Restore, and Exit.
    #[test]
    fn set_migrate_fn_test() {
        let active_cpus: cpu_mask_t = peek_active_mask();
        if active_cpus == 0 || active_cpus.is_power_of_two() {
            kprintln!("Expected multiple CPUs to be active.");
            return true;
        }

        // The worker thread will attempt to migrate to another CPU.
        extern "C" fn worker_body(_arg: *mut c_void) -> i32 {
            let current_cpu: cpu_num_t = curr_cpu_num();
            // SAFETY: Called in a running kernel worker thread context.
            let this = unsafe { ThreadPtr::current() };
            // SAFETY: `this` points to the valid current thread.
            let _ =
                unsafe { this.set_cpu_affinity(peek_active_mask() ^ cpu_num_to_mask(current_cpu)) };

            let target_cpu: cpu_num_t = curr_cpu_num();
            if current_cpu == target_cpu {
                kprintln!(
                    "Expected to be running on CPU {}, but actually running on {}.",
                    target_cpu,
                    current_cpu
                );
                return Status::INTERNAL.into_raw();
            }

            Status::result_into_raw(Ok(()))
        }
        // SAFETY: `worker_body` is a valid thread entry point that ignores its argument.
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_migrate_fn_test_worker".as_ptr(),
                    worker_body,
                    core::ptr::null_mut(),
                    DEFAULT_PRIORITY,
                )
            },
            "thread_create failed."
        );

        // Set the migrate function, and begin execution.
        struct MigrateState {
            count: i32,
            last_cpu: cpu_num_t,
            next_stage: MigrateStage,
            success: bool,
        }
        let mut migrate_state = MigrateState {
            count: 0,
            last_cpu: INVALID_CPU,
            next_stage: MigrateStage::Save,
            success: true,
        };
        let state_ptr = SendPtr(&raw mut migrate_state);

        let migrate_fn = MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
            // SAFETY: `set_migrate_fn` callbacks are always invoked by the scheduler with an
            // active `ChainLockTransaction` and `thread`'s lock held, serializing access to
            // `migrate_state`, which outlives `worker` (joined below before being read).
            let state = unsafe { &mut *state_ptr.as_ptr() };
            let token = unsafe { ChainLockTransaction::assert_active() };
            thread.lock().assert_held(&token);
            state.count += 1;

            let current_cpu: cpu_num_t = curr_cpu_num();
            if (stage == MigrateStage::Restore) && (state.last_cpu == current_cpu) {
                kprintln!("Expected to have migrated CPU.");
                state.success = false;
            }
            state.last_cpu = current_cpu;

            if state.next_stage != stage {
                kprintln!(
                    "Expected migrate stage {}, but received migrate stage {}.",
                    state.next_stage as i32,
                    stage as i32
                );
                state.success = false;
            }

            match state.next_stage {
                MigrateStage::Save => {
                    state.next_stage = MigrateStage::Restore;
                }
                MigrateStage::Restore => {
                    state.next_stage = MigrateStage::Exiting;
                    // SAFETY: `thread.lock().assert_held(&token)` above verified that
                    // `thread`'s lock is held under `token`.
                    let proof = unsafe { ChainLockProof::assume_held(thread, &token) };
                    if proof.scheduler_state().last_cpu() != current_cpu {
                        kprintln!("Expected last CPU to be current CPU after migrate.");
                        state.success = false;
                    }
                }
                MigrateStage::Exiting => {}
            }
        });
        // SAFETY: `worker` is valid and has not yet been resumed.
        unsafe {
            worker.set_migrate_fn(Some(migrate_fn));
            worker.resume();
        }

        // Wait for the worker thread to test itself.
        // SAFETY: `worker` is valid and has not been joined yet.
        let worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
        expect_eq!(worker_retcode, Status::result_into_raw(Ok(())), "Worker thread failed.");
        expect_eq!(migrate_state.count, 3, "Migrate function was not called 3 times.");
        expect_true!(
            migrate_state.success,
            "Migrate function was not called with the expected state."
        );
    }

    /// Tests migrating `THREAD_READY` threads queued behind the running thread on the same CPU.
    #[test]
    fn set_migrate_ready_threads_test() {
        let active_cpus: cpu_mask_t = peek_active_mask();
        if active_cpus == 0 || active_cpus.is_power_of_two() {
            kprintln!("Expected multiple CPUs to be active.");
            return true;
        }

        const STARTING_CPU: cpu_num_t = 0;
        const TARGET_CPU: cpu_num_t = 1;

        // The worker thread will validate that it is running on the target CPU.
        extern "C" fn worker_body(_arg: *mut c_void) -> i32 {
            let current_cpu: cpu_num_t = curr_cpu_num();
            if current_cpu != TARGET_CPU {
                kprintln!(
                    "Expected to be running on CPU {}, but actually running on {}.",
                    TARGET_CPU,
                    current_cpu
                );
                return Status::INTERNAL.into_raw();
            }
            Status::result_into_raw(Ok(()))
        }

        let mut workers: [Option<ThreadPtr>; 4] = [None, None, None, None];

        for worker_slot in &mut workers {
            // SAFETY: `worker_body` is a valid thread entry point that ignores its argument.
            let worker = unwrap_ok!(
                unsafe {
                    create_with_priority(
                        c"set_migrate_ready_threads_test_worker".as_ptr(),
                        worker_body,
                        core::ptr::null_mut(),
                        DEFAULT_PRIORITY,
                    )
                },
                "thread_create failed."
            );
            // SAFETY: `worker` was just created and is not locked.
            let _ = unsafe { worker.set_cpu_affinity(cpu_num_to_mask(STARTING_CPU)) };
            *worker_slot = Some(worker);
        }
        let workers: [ThreadPtr; 4] = workers.map(|w| w.expect("initialized above"));

        // Move the test thread to the same CPU that the workers will start on.
        // SAFETY: Called in a running kernel test thread context with no locks held.
        let current_thread = unsafe { ThreadPtr::current() };
        // SAFETY: `current_thread` is the valid running thread.
        let original_affinity: cpu_mask_t = unsafe { current_thread.get_cpu_affinity() };
        // SAFETY: Current thread is not locked and no chain lock transaction is active.
        let _ = unsafe { current_thread.set_cpu_affinity(cpu_num_to_mask(STARTING_CPU)) };
        assert_eq!(curr_cpu_num(), STARTING_CPU, "Failed to move test thread to the starting CPU.");

        let _cleanup = zr::defer(|| {
            // Restore original CPU affinity of the test thread.
            // SAFETY: Current thread is not locked and no chain lock transaction is active.
            let _ = unsafe { current_thread.set_cpu_affinity(original_affinity) };
        });

        {
            let _preempt_disabled_guard = AutoPreemptDisabler::new();
            // SAFETY: Preemption is disabled by `_preempt_disabled_guard`, so the current CPU's
            // `PerCpu` stats remain valid for this read.
            let context_switches_before = unsafe {
                crate::kernel::stats::CpuStats::read_atomic_fields_relaxed(
                    crate::kernel::percpu::PerCpu::get_current().stats.get(),
                )
            }
            .context_switches;

            // Resume the workers with preemption disabled. The workers should stack up
            // behind the current thread in the run queue. BE CAREFUL not to do anything
            // that would block until the workers are validated.
            for worker in workers {
                // SAFETY: `worker` has not yet been resumed or joined.
                unsafe { worker.resume() };

                // Validate the thread state.
                // SAFETY: `worker` is valid and alive until joined below.
                let worker_ref = unsafe { &*worker.as_ptr() };
                let (state, curr_cpu) = {
                    single_chain_lock_guard_owned!(
                        guard,
                        worker_ref,
                        IRQ_SAVE_OPTION,
                        clt_tag!("set_migrate_ready_threads_test")
                    );
                    let proof = guard.proof();
                    (proof.state(), proof.scheduler_state().curr_cpu())
                };
                assert_eq!(state, ThreadStateKind::Ready, "The worker was in the wrong state.");
                assert_eq!(curr_cpu, STARTING_CPU, "The worker was assigned to the wrong CPU.");
            }

            // Migrate the ready threads to a different CPU. BE CAREFUL not to do
            // anything that would block until the workers are migrated.
            for worker in workers {
                // SAFETY: `worker` is valid and alive until joined below.
                let _ = unsafe { worker.set_cpu_affinity(cpu_num_to_mask(TARGET_CPU)) };
            }

            // SAFETY: Preemption is disabled by `_preempt_disabled_guard`, so the current CPU's
            // `PerCpu` stats remain valid for this read.
            let context_switches_after = unsafe {
                crate::kernel::stats::CpuStats::read_atomic_fields_relaxed(
                    crate::kernel::percpu::PerCpu::get_current().stats.get(),
                )
            }
            .context_switches;
            assert_eq!(
                context_switches_before, context_switches_after,
                "The test thread context switched during the critical section."
            );
        }

        // Wait for the worker thread results.
        for worker in workers {
            // SAFETY: `worker` is valid and joined exactly once here.
            let worker_retcode =
                unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
            expect_eq!(worker_retcode, Status::result_into_raw(Ok(())), "Worker thread failed.");
        }
    }

    /// Tests `Scheduler::migrate_unpinned_threads_curr_cpu` via a custom trampoline thread.
    #[test]
    fn migrate_unpinned_threads_test() {
        let active_cpus: cpu_mask_t = peek_active_mask();
        if active_cpus == 0 || active_cpus.is_power_of_two() {
            kprintln!("Expected multiple CPUs to be active.");
            return true;
        }

        const STARTING_CPU: cpu_num_t = 1;
        #[repr(u8)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum TestState {
            WaitingForWorkerStart = 0,
            WaitingForMigration = 1,
            MigrationDone = 2,
        }
        let test_state = AtomicU8::new(TestState::WaitingForWorkerStart as u8);

        // Setup the thread that will be migrated.
        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `test_state` on the caller's stack, which outlives this
            // worker thread (joined below).
            let test_state = unsafe { &*(arg as *const AtomicU8) };

            debug_assert!(
                test_state.load(Ordering::SeqCst) == TestState::WaitingForWorkerStart as u8
            );
            test_state.store(TestState::WaitingForMigration as u8, Ordering::SeqCst);
            while test_state.load(Ordering::SeqCst) != TestState::MigrationDone as u8 {
                let _ = sleep_relative(DurationMono::from_micros(100));
            }
            0
        }

        let state_ptr = SendPtr((&raw const test_state).cast_mut());
        let fn_ = MigrateFn::new(move |_: &Thread, _: MigrateStage| {
            // SAFETY: `test_state` on the caller's stack outlives `worker` (joined below).
            unsafe { &*state_ptr.as_ptr() }.store(TestState::MigrationDone as u8, Ordering::SeqCst);
        });
        // SAFETY: `worker_body` is a valid thread entry point and `test_state` outlives `worker`.
        let worker = unwrap_ok!(unsafe {
            create_with_priority(
                c"worker".as_ptr(),
                worker_body,
                (&raw const test_state).cast_mut().cast(),
                DEFAULT_PRIORITY,
            )
        });
        // SAFETY: `worker` has not yet been resumed.
        unsafe {
            let _ = worker.set_soft_cpu_affinity(cpu_num_to_mask(STARTING_CPU));
            worker.set_migrate_fn(Some(fn_));
            worker.resume();
        }

        while test_state.load(Ordering::SeqCst) != TestState::WaitingForMigration as u8 {
            let _ = sleep_relative(DurationMono::from_micros(100));
        }

        // Setup the thread that will perform the migration.  Note that this is a
        // trampoline function implementation.  It must call
        // Scheduler::TrampolineLockHandoff to deal with dropping locks held across
        // context switches while maintaining ChainLockTransaction invariants.
        //
        /// # Safety
        ///
        /// Must only be invoked as a thread initial entry trampoline immediately following a
        /// scheduler context switch with interrupts disabled and the scheduler lock handoff
        /// pending.
        unsafe extern "C" fn migrate_body() -> ! {
            // SAFETY: This is an alternate thread trampoline invoked right after context switch,
            // so `trampoline_lock_handoff`, `migrate_unpinned_threads_curr_cpu`,
            // `set_curr_cpu_active`, and `enable_ints` satisfy the scheduler trampoline contract.
            unsafe {
                trampoline_lock_handoff();
                Scheduler::migrate_unpinned_threads_curr_cpu();
                Scheduler::set_curr_cpu_active(true);
                enable_ints();
            }
            // SAFETY: Running non-idle thread with interrupts enabled, blocking permitted, and no
            // locks or `ChainLockTransaction` held.
            unsafe { current_exit(0) };
        }
        let profile = SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, true);
        // SAFETY: `migrate_body` is a valid alternate trampoline that performs lock handoff and
        // calls `current_exit(0)`.
        let migrate = unwrap_ok!(unsafe {
            create_etc(
                core::ptr::null_mut(),
                b"migrate",
                None,
                core::ptr::null_mut(),
                &profile,
                Some(migrate_body),
            )
        });
        // SAFETY: `migrate` was just created and has not been resumed or joined yet.
        unsafe {
            let _ = migrate.set_cpu_affinity(cpu_num_to_mask(STARTING_CPU));
            migrate.resume();
        }

        // SAFETY: `migrate` and `worker` are valid and joined exactly once here.
        let retcode = unwrap_ok!(
            unsafe { migrate.join(InstantMono::INFINITE) },
            "Failed to join migrate thread."
        );
        expect_eq!(retcode, ZX_OK, "Migrate thread failed.");
        // SAFETY: `worker` is valid and joined exactly once here.
        let retcode = unwrap_ok!(
            unsafe { worker.join(InstantMono::INFINITE) },
            "Failed to join worker thread."
        );
        expect_eq!(retcode, ZX_OK, "Worker thread failed.");
    }

    /// Stress-tests migrating spinning worker threads across active CPUs.
    #[test]
    #[allow(unreachable_code)]
    fn migrate_stress_test() {
        if true {
            // TODO(https://fxbug.dev/78695): Disabled until root cause of hangs on some hardware
            // can be determined.
            kprintln!("Test disabled due to https://fxbug.dev/42158849");
            return true;
        }

        // Get number of CPUs in the system.
        let active_cpus: usize = peek_active_mask().count_ones() as usize;
        kprintln!("Found {} active CPU(s)", active_cpus);
        if active_cpus <= 1 {
            kprintln!("Test can only proceed with multiple active CPUs.");
            return true;
        }

        // The worker thread body.
        //
        // Each thread spins ensuring that it is only running on a single, particular
        // CPU. The migration function below updates which CPU the thread is allowed
        // to run on.
        #[ksync::guarded]
        struct ThreadState {
            should_stop: AtomicBool,
            started: AtomicBool,

            // Prevent reporting lock violations inside the migration function to avoid reentering
            // the scheduler and interfering with the migration function state.
            // TODO(https://fxbug.dev/42157332): Figure out how to support violation reporting in
            // sensitive code paths.
            #[mutex(flags = lockdep::LOCK_FLAGS_REPORTING_DISABLED)]
            lock: ksync::KMutex<crate::ksync_kernel::TrackedSpinlock>,

            #[guarded_by(lock)]
            allowed_cpu: cpu_num_t,
            #[guarded_by(lock)]
            is_migrating: bool,
        }

        extern "C" fn worker_body(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to this worker's `ThreadState` element in `threads`, which
            // remains pinned in heap storage until all worker threads are joined.
            let state = unsafe { &*(arg as *const ThreadState) };
            state.started.store(true, Ordering::SeqCst);

            // Justification: Relaxed ordering is sufficient because this is a best-effort stop
            // flag polled in a loop.
            while !state.should_stop.load(Ordering::Relaxed) {
                {
                    ksync::lock!(let guard = state.lock_lock());
                    let locked = guard.fields();
                    assert!(
                        !*locked.is_migrating,
                        "Worker thread scheduled between MigrationStage::Before and \
                         MigrationStage::After being called."
                    );
                    assert!(
                        curr_cpu_num() == *locked.allowed_cpu,
                        "Worker thread running on unexpected CPU: expected = {}, running on = {}",
                        *locked.allowed_cpu,
                        curr_cpu_num()
                    );
                }
                r#yield();
            }
            0
        }

        // CPU that all threads start running on. Can be any valid CPU.
        let starting_cpu: cpu_num_t = curr_cpu_num();

        // Create threads.
        let num_threads: usize = active_cpus;
        let threads = fbl::Array::<ThreadState>::try_new_pin_init(num_threads, || {
            pin_init::pin_init!(ThreadState {
                should_stop: AtomicBool::new(false),
                started: AtomicBool::new(false),
                lock <- ksync::KMutex::init(),
                allowed_cpu: starting_cpu.into(),
                is_migrating: false.into(),
            })
        })
        .expect("ThreadState slice allocation failed");
        let mut worker_threads = fbl::Array::<Option<ThreadPtr>>::try_new(num_threads)
            .expect("worker_threads allocation failed");

        for (state_ref, thread_slot) in threads.iter().zip(worker_threads.iter_mut()) {
            let state_ptr: *mut c_void = (&raw const *state_ref).cast_mut().cast();
            // Create the thread.
            // SAFETY: `worker_body` is a valid entry point and `state_ptr` remains valid and
            // pinned in `threads` until the thread is joined below.
            let thread = unwrap_ok!(
                unsafe {
                    create_with_priority(
                        c"migrate_stress_test worker".as_ptr(),
                        worker_body,
                        state_ptr,
                        DEFAULT_PRIORITY,
                    )
                },
                "Thread::Create failed."
            );
            *thread_slot = Some(thread);
            {
                ksync::lock!(let mut guard = state_ref.lock_lock());
                *guard.as_mut().fields_mut().allowed_cpu = starting_cpu;
            }

            let state_for_migrate = SendPtr((&raw const *state_ref).cast_mut());
            let migrate_fn = MigrateFn::new(move |_thread: &Thread, stage: MigrateStage| {
                // SAFETY: `state_for_migrate` remains valid and pinned in `threads` until all
                // workers are joined below.
                let state = unsafe { &*state_for_migrate.as_ptr() };
                ksync::lock!(let mut guard = state.lock_lock());
                let locked = guard.as_mut().fields_mut();
                match stage {
                    MigrateStage::Save => {
                        assert!(!*locked.is_migrating, "Thread is already migrating");
                        assert!(
                            *locked.allowed_cpu == curr_cpu_num(),
                            "Migrate function called on incorrect CPU."
                        );
                        *locked.allowed_cpu = INVALID_CPU;
                        *locked.is_migrating = true;
                    }
                    MigrateStage::Restore => {
                        assert!(*locked.is_migrating);
                        *locked.is_migrating = false;
                        *locked.allowed_cpu = curr_cpu_num();
                    }
                    MigrateStage::Exiting => {}
                }
            });

            // Set the migration function to ensure that `allowed_cpu` keeps up to date.
            // SAFETY: `thread` has not been resumed yet.
            unsafe {
                thread.set_migrate_fn(Some(migrate_fn));

                // Start running on `starting_cpu`.
                let _ = thread.set_soft_cpu_affinity(1 << starting_cpu);
                thread.resume();
            }
        }

        // Wait for the worker threads to execute at least once to ensure the migrate function gets
        // called. Threads that have never executed are moved without calling the migrate function,
        // which could cause this test to incorrectly assert in the worker loop.
        for thread_state in threads.iter() {
            while !thread_state.started.load(Ordering::SeqCst) {
                let _ = sleep_relative(DurationMono::from_micros(100));
            }
        }

        // Mutate threads as they run.
        for i in 0..10_000usize {
            for (j, thread_opt) in worker_threads.iter().enumerate() {
                let affinity: cpu_mask_t = ((i + j) as cpu_mask_t) & peek_active_mask();
                if affinity != 0 {
                    let thread = thread_opt.expect("initialized above");
                    // SAFETY: `thread` is valid and alive until joined below.
                    let _ = unsafe { thread.set_soft_cpu_affinity(affinity) };
                }
            }
            let _ = sleep_relative(DurationMono::from_micros(100));
        }

        // Join threads.
        for (thread_state, thread_opt) in threads.iter().zip(worker_threads.iter()) {
            // Inform the thread to stop.
            thread_state.should_stop.store(true, Ordering::SeqCst);

            // Wait for it to finish.
            let thread = thread_opt.expect("initialized above");
            let deadline = Deadline::after_mono(DurationMono::from_seconds(5), TimerSlack::none());
            // SAFETY: `thread` is valid and joined here.
            let result = unsafe { thread.join(InstantMono(deadline.when().0)) };
            if let Err(status) = result {
                // If the thread has not completed in 5 seconds, it is likely that the
                // thread has hung for an unknown reason.
                //
                // TODO(https://fxbug.dev/42158849): We are currently seeing some flakes in CI/CQ
                // that cannot be reproduced locally. Once resolved, this additional
                // logging can be removed.
                // SAFETY: `thread` is valid and has not exited yet.
                unsafe { thread.dump(true) };
                let mut bt = Backtrace::new();
                // SAFETY: `thread` is valid and `bt` is a valid `Backtrace`.
                unsafe { thread.get_backtrace(&mut bt) };
                bt.print();

                panic!("Failed to join worker thread: {}", status.into_raw());
            }
        }
    }

    /// Stress-tests concurrently setting and clearing `set_migrate_fn` on a running thread.
    #[test]
    fn set_migrate_fn_stress_test() {
        // Get number of CPUs in the system.
        let active_cpus: usize = peek_active_mask().count_ones() as usize;
        kprintln!("Found {} active CPU(s)", active_cpus);
        if active_cpus <= 1 {
            kprintln!("Test can only proceed with multiple active CPUs.");
            return true;
        }

        struct Worker {
            thread: Cell<Option<ThreadPtr>>,
            should_stop: AtomicBool,
        }

        impl Worker {
            fn migrate(_thread: &Thread, _stage: MigrateStage) {}

            extern "C" fn body(arg: *mut c_void) -> i32 {
                // SAFETY: `arg` points to a valid `Worker` element in `threads`, which outlives
                // all worker threads (joined below).
                let state = unsafe { &*(arg as *const Worker) };

                // Justification: Relaxed ordering is sufficient because this is a best-effort stop
                // flag polled in a loop.
                while !state.should_stop.load(Ordering::Relaxed) {
                    // Spin or sleep for 100us.
                    let delay = DurationMono::from_micros(100);
                    // SAFETY: `rand` is a thread-safe C library PRNG function in the kernel.
                    if (unsafe { rand() } & 1) != 0 {
                        let spin_end = InstantMono(current_mono_time().0.saturating_add(delay.0));
                        while current_mono_time() < spin_end {
                            libarch::r#yield();
                        }
                    } else {
                        let _ = sleep_relative(delay);
                    }
                }

                0
            }
        }

        // Create threads.
        let num_threads: usize = active_cpus * 2;
        let mut uninit_threads = kalloc::Box::<[Worker]>::try_new_uninit_slice(num_threads)
            .expect("Worker slice allocation failed");
        for slot in uninit_threads.iter_mut() {
            slot.write(Worker { thread: Cell::new(None), should_stop: AtomicBool::new(false) });
        }
        // SAFETY: Every element of `uninit_threads` was initialized above.
        let threads = unsafe { uninit_threads.assume_init() };

        for worker_ref in threads.iter() {
            let worker_ptr: *mut c_void = (&raw const *worker_ref).cast_mut().cast();
            // SAFETY: `Worker::body` is a valid entry point and `worker_ptr` outlives the thread.
            let thread = unwrap_ok!(
                unsafe {
                    create_with_priority(
                        c"set_migrate_fn_stress_test worker".as_ptr(),
                        Worker::body,
                        worker_ptr,
                        DEFAULT_PRIORITY,
                    )
                },
                "Thread::Create failed."
            );
            worker_ref.thread.set(Some(thread));
            // SAFETY: `thread` was just created and has not been resumed yet.
            unsafe { thread.resume() };
        }

        // Periodically set/unset the migrate fn of the first thread.
        let first_thread = threads[0].thread.get().expect("initialized above");
        for i in 0..10000 {
            let _ = sleep_relative(DurationMono::from_micros(100));
            let callback = if (i & 1) != 0 {
                None
            } else {
                Some(MigrateFn::new(|thread: &Thread, stage: MigrateStage| {
                    Worker::migrate(thread, stage);
                }))
            };
            // SAFETY: `first_thread` is valid and alive until joined below.
            unsafe { first_thread.set_migrate_fn(callback) };
        }

        // Join threads.
        for worker in threads.iter() {
            // Inform the thread to stop.
            worker.should_stop.store(true, Ordering::SeqCst);

            let thread = worker.thread.get().expect("initialized above");
            // Wait for it to finish.
            loop {
                let deadline =
                    Deadline::after_mono(DurationMono::from_seconds(5), TimerSlack::none());
                // SAFETY: `thread` is valid and joined until it succeeds.
                if unsafe { thread.join(InstantMono(deadline.when().0)) }.is_ok() {
                    break;
                }

                // If the thread has not completed in 5 seconds, either the thread has
                // hung for an unknown reason or there may have been lost time due to
                // virtualization. Keep trying until the overall test times out to
                // differentiate between an indefinite hang and virtualization time
                // loss.
                //
                // TODO(https://fxbug.dev/42158849): We are currently seeing some flakes
                // in CI/CQ that cannot be reproduced locally. Once resolved, this
                // additional logging can be removed.
                // SAFETY: `thread` is still alive because `join` timed out.
                unsafe { thread.dump(true) };
                let mut bt = Backtrace::new();
                // SAFETY: `thread` is still alive and `bt` is a valid `Backtrace`.
                unsafe { thread.get_backtrace(&mut bt) };
                bt.print();
            }
        }
    }

    /// Tests that `set_context_switch_fn` invokes the context switch callback as expected.
    #[test]
    fn set_context_switch_fn_test() {
        // The worker thread will attempt to context switch.
        extern "C" fn worker_body(_arg: *mut c_void) -> i32 {
            Status::result_into_raw(sleep_relative(DurationMono::from_millis(1)))
        }
        // SAFETY: `worker_body` is a valid thread entry point that ignores its argument.
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"set_context_switch_fn_test_worker".as_ptr(),
                    worker_body,
                    core::ptr::null_mut(),
                    DEFAULT_PRIORITY,
                )
            },
            "thread_create failed."
        );

        // Set the context switch function, and begin execution.
        let mut context_switches: i32 = 0;
        let switches_ptr = SendPtr(&raw mut context_switches);
        let context_switch_fn = ContextSwitchFn::new(move || {
            // SAFETY: Invoked exclusively under `worker`'s lock, and `context_switches` outlives
            // `worker` (joined below before being read).
            unsafe { *switches_ptr.as_ptr() += 1 };
        });
        // SAFETY: `worker` has not been resumed yet.
        unsafe {
            worker.set_context_switch_fn(Some(context_switch_fn));
            worker.resume();
        }

        // Wait for the worker thread to test itself.
        // SAFETY: `worker` is valid and joined exactly once here.
        let worker_retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
        expect_eq!(worker_retcode, ZX_OK, "Worker thread failed.");
        // There are at least 4 context switches that occur as part of this test:
        // 1. When the thread is initially scheduled.
        // 2. When the thread is blocked for sleep.
        // 3. When the thread is rescheduled.
        // 4. When the thread exits.
        //
        // Additionally context switches may occur to additional scheduling events
        // during regular system operation.
        expect_ge!(context_switches, 4, "Context switch function was not called at least 4 times.");
    }

    /// Verifies that owned callbacks are dropped outside locks on replacement and thread teardown.
    #[test]
    fn callback_drop_test() {
        struct DropTracker {
            drops: SendPtr<AtomicU32>,
            dropped_with_ints_enabled: SendPtr<AtomicBool>,
        }
        impl Drop for DropTracker {
            fn drop(&mut self) {
                if crate::arch_rs::ints_disabled() {
                    // SAFETY: Points to stack-allocated atomic outliving `worker`.
                    unsafe { &*self.dropped_with_ints_enabled.0 }.store(false, Ordering::SeqCst);
                }
                // SAFETY: Points to stack-allocated atomic outliving `worker`.
                unsafe { &*self.drops.0 }.fetch_add(1, Ordering::SeqCst);
            }
        }

        extern "C" fn noop_worker(_arg: *mut c_void) -> i32 {
            0
        }
        // SAFETY: `noop_worker` is a valid thread entry point.
        let worker = unwrap_ok!(
            unsafe {
                create_with_priority(
                    c"callback_drop_test_worker".as_ptr(),
                    noop_worker,
                    core::ptr::null_mut(),
                    DEFAULT_PRIORITY,
                )
            },
            "thread_create failed."
        );

        let migrate_drops = AtomicU32::new(0);
        let migrate_ints_ok = AtomicBool::new(true);
        let cs_drops = AtomicU32::new(0);
        let cs_ints_ok = AtomicBool::new(true);

        let make_migrate = || {
            let tracker = DropTracker {
                drops: SendPtr((&raw const migrate_drops).cast_mut()),
                dropped_with_ints_enabled: SendPtr((&raw const migrate_ints_ok).cast_mut()),
            };
            MigrateFn::new(move |_: &Thread, _: MigrateStage| {
                let _ = &tracker;
            })
        };
        let make_cs = || {
            let tracker = DropTracker {
                drops: SendPtr((&raw const cs_drops).cast_mut()),
                dropped_with_ints_enabled: SendPtr((&raw const cs_ints_ok).cast_mut()),
            };
            ContextSwitchFn::new(move || {
                let _ = &tracker;
            })
        };

        // Install, replace (must drop first outside locks), and leave second installed for
        // `Thread::drop` teardown on `join`.
        // SAFETY: `worker` is valid and not yet resumed.
        unsafe {
            worker.set_migrate_fn(Some(make_migrate()));
            expect_eq!(migrate_drops.load(Ordering::SeqCst), 0);
            worker.set_migrate_fn(Some(make_migrate()));
            expect_eq!(migrate_drops.load(Ordering::SeqCst), 1);

            worker.set_context_switch_fn(Some(make_cs()));
            expect_eq!(cs_drops.load(Ordering::SeqCst), 0);
            worker.set_context_switch_fn(Some(make_cs()));
            expect_eq!(cs_drops.load(Ordering::SeqCst), 1);

            worker.resume();
        }

        // SAFETY: `worker` is valid and joined once here, invoking `Thread`'s `PinnedDrop::drop`.
        let retcode =
            unwrap_ok!(unsafe { worker.join(InstantMono::INFINITE) }, "Failed to join thread.");
        expect_eq!(retcode, ZX_OK);
        expect_eq!(migrate_drops.load(Ordering::SeqCst), 2);
        expect_eq!(cs_drops.load(Ordering::SeqCst), 2);
        expect_true!(migrate_ints_ok.load(Ordering::SeqCst));
        expect_true!(cs_ints_ok.load(Ordering::SeqCst));
    }

    /// Tests enabling and disabling thread memory allocation state and nested guards.
    #[test]
    fn scoped_allocation_disabled_test() {
        // SAFETY: Called in a running kernel test thread context.
        let state = unsafe { &*current_memory_allocation_state() };
        expect_true!(state.is_enabled());

        // SAFETY: Balanced with `state.enable()` on the same thread immediately below.
        unsafe { state.disable() };
        expect_false!(state.is_enabled());
        // SAFETY: Balances `state.disable()` above on the same thread.
        unsafe { state.enable() };
        expect_true!(state.is_enabled());

        {
            expect_true!(state.is_enabled());
            {
                let _smad = ScopedMemoryAllocationDisabled::new();
                expect_false!(state.is_enabled());
                {
                    let _nested_smad = ScopedMemoryAllocationDisabled::new();
                    expect_false!(state.is_enabled());
                }
                expect_false!(state.is_enabled());
            }
            expect_true!(state.is_enabled());
        }
    }

    /// Tests capturing backtraces on the current thread via `current_get_backtrace`.
    #[test]
    fn backtrace_static_method_test() {
        let mut bt = Backtrace::new();
        current_get_backtrace(&mut bt);

        // Make sure we have at least one frames worth.
        assert_gt!(bt.size(), 1usize);

        bt.reset();
        // SAFETY: `crate::get_frame!()` returns the current stack frame pointer.
        unsafe { current_get_backtrace_with_fp(crate::get_frame!() as usize, &mut bt) };
        assert_gt!(bt.size(), 1usize);

        // See that we don't crash.
        bt.reset();
        // SAFETY: `0` is outside the current thread's stack bounds.
        unsafe { current_get_backtrace_with_fp(0, &mut bt) };
        assert_eq!(bt.size(), 0usize);
    }

    /// Tests capturing a backtrace on a running vs. blocked thread instance.
    #[test]
    fn backtrace_instance_method_test() {
        #[pin_data]
        struct Args {
            running: AtomicBool,
            #[pin]
            event: Event,
        }
        pin_init::stack_pin_init!(let args = pin_init::pin_init!(Args {
            running: AtomicBool::new(false),
            event <- Event::init_unsignaled(),
        }));

        extern "C" fn helper(raw_args: *mut c_void) -> i32 {
            // SAFETY: `raw_args` points to `args` pinned on the caller's stack, which outlives
            // this helper thread (joined before `args` is dropped).
            let args = unsafe { &*(raw_args as *const Args) };
            args.running.store(true, Ordering::SeqCst);

            // Busy wait.
            while args.running.load(Ordering::SeqCst) {
                libarch::r#yield();
            }

            // Block.
            let _ = args.event.wait_infinite();

            0
        }

        let args_ptr: *mut c_void = (&raw const *args).cast_mut().cast();
        // SAFETY: `helper` is a valid thread entry point and `args` outlives `t` (joined by
        // `cleanup` or explicitly below).
        let t = unwrap_ok!(unsafe {
            create_with_priority(c"backtrace helper".as_ptr(), helper, args_ptr, DEFAULT_PRIORITY)
        });
        let mut cleanup = zr::defer(|| {
            // SAFETY: `t` is valid and has not been joined if `cleanup` runs.
            let _ = unsafe { t.join(InstantMono::INFINITE) };
        });

        // SAFETY: `t` was just created and has not been resumed yet.
        unsafe { t.resume() };

        // Wait for the thread to run.
        while !args.running.load(Ordering::SeqCst) {
            libarch::r#yield();
        }

        // Can't take the backtrace of a running thread.
        let mut bt = Backtrace::new();
        // SAFETY: `t` points to a valid `Thread` and `bt` is a valid `Backtrace`; `GetBacktrace`
        // checks `t`'s state under its lock and returns an empty backtrace for a running thread.
        unsafe { t.get_backtrace(&mut bt) };
        assert_eq!(0usize, bt.size());

        // Tell the thread to go block.
        args.running.store(false, Ordering::SeqCst);

        // Wait for it to block.
        let mut state = ThreadStateKind::Running;
        while state != ThreadStateKind::Blocked {
            let _ = sleep_relative(DurationMono::from_micros(200));
            // SAFETY: `t` is valid and alive until joined below.
            let t_ref = unsafe { &*t.as_ptr() };
            single_chain_lock_guard_owned!(
                guard,
                t_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("backtrace_instance_method_test")
            );
            state = guard.proof().state();
        }

        // See that we can backtrace a blocked thread.
        bt.reset();
        // SAFETY: `t` is blocked in `args.event.wait_infinite()` and `bt` is a valid `Backtrace`.
        unsafe { t.get_backtrace(&mut bt) };
        assert_gt!(bt.size(), 0usize);

        args.event.signal();

        cleanup.cancel();
        // SAFETY: `cleanup` was cancelled, so `t` is joined exactly once here.
        let result = unwrap_ok!(unsafe { t.join(InstantMono::INFINITE) });
        assert_eq!(ZX_OK, result);
    }

    /// ScopedMemoryStall is currently meant to be a no-op when executed in a kernel-only thread.
    #[test]
    fn scoped_memory_stall_test() {
        // This test verifies that it, indeed, does not modify the thread's state.
        let current_thread: *const Thread = current_get();

        // Verify previous state.
        {
            let _preempt_disable = AnnotatedAutoPreemptDisabler::new();
            // SAFETY: `_preempt_disable` holds preemption disabled on the current thread and no
            // concurrent writes to `memory_stall_state` occur.
            let state = unsafe { super::memory_stall_state(current_thread) };
            assert_eq!(ThreadStallState::IgnoredKernelOnly, state);
        }

        // Enter a memory stall and verify that the state has not changed.
        {
            let _scoped_memory_stall = ScopedMemoryStall::new();
            let _preempt_disable = AnnotatedAutoPreemptDisabler::new();
            // SAFETY: `_preempt_disable` holds preemption disabled on the current thread and no
            // concurrent writes to `memory_stall_state` occur.
            let state = unsafe { super::memory_stall_state(current_thread) };
            assert_eq!(ThreadStallState::IgnoredKernelOnly, state);
        }

        // After exiting the memory stall, verify that the state has not changed.
        {
            let _preempt_disable = AnnotatedAutoPreemptDisabler::new();
            // SAFETY: `_preempt_disable` holds preemption disabled on the current thread and no
            // concurrent writes to `memory_stall_state` occur.
            let state = unsafe { super::memory_stall_state(current_thread) };
            assert_eq!(ThreadStallState::IgnoredKernelOnly, state);
        }
    }

    /// Verify CPU affinity, runtime, last CPU, aspace ref, lock flow ID, and kernel fault checks.
    #[test]
    fn test_phase1_queries_and_accessors() {
        // SAFETY: Threading is initialized when unit tests run.
        let current = unsafe { ThreadPtr::current() };

        // CPU affinity and soft affinity round-trip.
        // SAFETY: Current thread is alive and no locks are held.
        let orig_hard = unsafe { current.get_cpu_affinity() };
        // SAFETY: Same as above.
        let orig_soft = unsafe { current.get_soft_cpu_affinity() };
        let boot_mask = cpu_num_to_mask(BOOT_CPU_ID);
        // SAFETY: Restoring soft affinity on the current thread.
        let prev_soft = unsafe { current.set_soft_cpu_affinity(boot_mask) };
        expect_eq!(prev_soft, orig_soft);
        // SAFETY: Reading soft affinity on the current thread.
        expect_eq!(unsafe { current.get_soft_cpu_affinity() }, boot_mask);
        // SAFETY: Restoring original soft affinity on the current thread.
        expect_eq!(unsafe { current.set_soft_cpu_affinity(orig_soft) }, boot_mask);
        // SAFETY: Reading hard affinity on the current thread.
        expect_eq!(unsafe { current.get_cpu_affinity() }, orig_hard);

        // Runtime and LastCpu queries.
        // SAFETY: Current thread is alive and no locks are held.
        let rt1 = unsafe { current.runtime() };
        // SAFETY: Same as above.
        let rt2 = unsafe { current_runtime() };
        expect_ge!(rt1.0, 0);
        expect_ge!(rt2.0, rt1.0);
        // SAFETY: Current thread is alive and no locks are held.
        let last = unsafe { current.last_cpu() };
        expect_ne!(last, INVALID_CPU);

        // The unittest runner thread has a user address space attached, while a freshly created
        // kernel thread has none.
        // SAFETY: Current thread is alive and no locks are held.
        expect_true!(unsafe { current.as_ref().get_aspace_ref() }.is_some());
        let empty = unwrap_some!(Thread::create_empty());
        // SAFETY: `empty` is a valid, unscheduled `Thread` owned exclusively here.
        expect_true!(unsafe { empty.as_ref().get_aspace_ref() }.is_none());
        // SAFETY: `empty` was allocated by `create_empty` and never scheduled or added to lists.
        unsafe { Thread::delete(empty.as_ptr()) };

        // Lock flow ID allocation.
        // SAFETY: Current thread is alive.
        let prev_flow = unsafe { current.lock_flow_id() };
        // SAFETY: Same as above.
        let next1 = unsafe { current.take_next_lock_flow_id() };
        expect_ne!(next1, 0);
        // SAFETY: Same as above.
        expect_eq!(unsafe { current.lock_flow_id() }, next1);
        // SAFETY: Same as above.
        let next2 = unsafe { current.take_next_lock_flow_id() };
        expect_ne!(next2, next1);
        // SAFETY: Same as above.
        expect_eq!(unsafe { current.lock_flow_id() }, next2);
        // Restore previous flow ID.
        // SAFETY: Current thread is alive.
        unsafe { current.as_ref() }.lock_flow_id.store(prev_flow);

        // Faults on kernel addresses always return ZX_ERR_NOT_FOUND.
        let kernel_va = core::ptr::from_ref(&prev_flow) as usize;
        // SAFETY: Called from an initialized thread context with no locks held; `kernel_va` is a
        // kernel address.
        unsafe {
            expect_err!(page_fault(kernel_va, 0), Status::NOT_FOUND);
            expect_err!(soft_fault(kernel_va, 0), Status::NOT_FOUND);
            expect_err!(soft_fault_in_range(kernel_va, 0, 1), Status::NOT_FOUND);
            expect_err!(accessed_fault(kernel_va), Status::NOT_FOUND);
        }
    }

    /// Verify `sleep_slack` clamping and 10% slack calculation.
    #[test]
    fn test_sleep_slack() {
        let now = InstantMono(1_000_000_000);
        // Past deadline clamps to MIN_SLEEP_SLACK.
        expect_eq!(sleep_slack(InstantMono(500_000_000), now).0, MIN_SLEEP_SLACK.0);
        // Equal deadline clamps to MIN_SLEEP_SLACK.
        expect_eq!(sleep_slack(now, now).0, MIN_SLEEP_SLACK.0);
        // Very small positive delta (5 us -> 0.5 us) clamps to MIN_SLEEP_SLACK (1 us).
        expect_eq!(sleep_slack(now + DurationMono::from_micros(5), now).0, MIN_SLEEP_SLACK.0);
        // 100 us delta -> 10 us slack.
        expect_eq!(
            sleep_slack(now + DurationMono::from_micros(100), now).0,
            DurationMono::from_micros(10).0
        );
        // 20 s delta -> 2 s slack, clamped to MAX_SLEEP_SLACK (1 s).
        expect_eq!(sleep_slack(now + DurationMono::from_seconds(20), now).0, MAX_SLEEP_SLACK.0);
    }

    /// Verify `yield`, `reschedule`, and `sleep*` entry points.
    #[test]
    fn test_sleep_and_scheduler_entrypoints() {
        r#yield();
        reschedule();

        let now = current_mono_time();
        // Past deadline returns immediately without blocking.
        expect_true!(sleep_etc(&Deadline::no_slack(now.into()), Interruptible::No, now).is_ok());
        expect_true!(sleep(InstantMono(0)).is_ok());
        expect_true!(sleep_relative(DurationMono(0)).is_ok());
        expect_true!(sleep_interruptible(InstantMono(0)).is_ok());

        // Short real sleep to exercise timer setup, block, handle_sleep, and unsleep.
        expect_true!(sleep_relative(DurationMono::from_micros(50)).is_ok());
    }

    /// Verifies `Thread::owner_name`, `thread_state_to_str`, and `thread_id_to_thread_slow`.
    #[test]
    fn owner_name_state_str_and_id_lookup() {
        expect_true!(thread_state_to_str(ThreadStateKind::Initial as u8) == "init");
        expect_true!(thread_state_to_str(ThreadStateKind::Suspended as u8) == "susp");
        expect_true!(thread_state_to_str(ThreadStateKind::Ready as u8) == "rdy");
        expect_true!(thread_state_to_str(ThreadStateKind::Running as u8) == "run");
        expect_true!(thread_state_to_str(ThreadStateKind::Blocked as u8) == "blok");
        expect_true!(thread_state_to_str(ThreadStateKind::BlockedReadLock as u8) == "blok");
        expect_true!(thread_state_to_str(ThreadStateKind::Sleeping as u8) == "slep");
        expect_true!(thread_state_to_str(ThreadStateKind::Death as u8) == "deth");
        expect_true!(thread_state_to_str(u8::MAX) == "unkn");

        // SAFETY: Called in a running kernel test thread context.
        let current = unsafe { ThreadPtr::current() };
        let mut owner = [0xffu8; ZX_MAX_NAME_LEN];
        // SAFETY: `current` points to the valid current thread.
        unsafe { current.as_ref() }.owner_name(&mut owner);
        expect_true!(&owner[..7] == b"kernel\0");

        // SAFETY: `current` points to the valid current thread.
        let current_tid = unsafe { current.tid() };
        let (found_ptr, invalid_none) = {
            ksync::lock!(
                let list_guard =
                    Thread::get_list_lock().lock_policy::<super::IrqSavePolicy>()
            );
            // SAFETY: `init_early()` completed during early boot before kernel tests run.
            let found_ptr = unsafe { thread_id_to_thread_slow(current_tid, list_guard.token()) }
                .map_or(core::ptr::null(), core::ptr::from_ref);
            // SAFETY: `init_early()` completed during early boot before kernel tests run.
            let invalid_none =
                unsafe { thread_id_to_thread_slow(ZX_KOID_INVALID, list_guard.token()) }.is_none();
            (found_ptr, invalid_none)
        };
        expect_eq!(found_ptr, current.as_ptr());
        expect_true!(invalid_none);
    }

    /// Verifies `set_rseq_accessor` and `signal_check_restartable_sequence_if_needed`.
    #[test]
    fn rseq_accessor_and_signal_if_needed() {
        use crate::page_map::{Accessor, PageMap};
        use crate::vm::pmm;
        use crate::vm::vm_object_paged::VmObjectPaged;
        use fbl::HasRefCount;
        use zx_types::zx_rseq_t;

        const VMO_OPTIONS: u32 = 0;
        const PMM_ALLOC_FLAGS: u32 = pmm::ALLOC_FLAG_ANY | pmm::ALLOC_FLAG_CAN_WAIT;
        let vmo =
            unwrap_ok!(VmObjectPaged::create(PMM_ALLOC_FLAGS, VMO_OPTIONS, page::SIZE as u64));
        expect_eq!(1, vmo.ref_count().ref_count_debug());

        let raw = Thread::create_for_test();
        assert_true!(!raw.is_null());
        let cleanup = zr::defer(|| {
            // SAFETY: `raw` was allocated by `Thread::create_for_test` and is unscheduled.
            unsafe { Thread::destroy_for_test(raw) };
        });

        // SAFETY: `raw` is a valid, unscheduled test `Thread` owned exclusively by this test.
        unsafe {
            let ptr = unwrap_some!(ThreadPtr::from_raw(raw));

            // Fresh thread has no valid rseq accessor and does not raise THREAD_SIGNAL_CHECK_RSEQ.
            expect_eq!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);
            ptr.signal_check_restartable_sequence_if_needed();
            expect_eq!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);

            // Installing a valid accessor raises `THREAD_SIGNAL_CHECK_RSEQ`, and clearing it then
            // calling `signal_check_restartable_sequence_if_needed` raises it again.
            let accessor = unwrap_ok!(PageMap::get().make_accessor::<zx_rseq_t>(vmo.clone(), 0));
            assert_true!(accessor.is_valid());
            expect_ge!(vmo.ref_count().ref_count_debug(), 2);
            ptr.set_rseq_accessor(accessor);
            expect_ne!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);
            Thread::clear_signals_ptr(raw, super::THREAD_SIGNAL_CHECK_RSEQ);
            ptr.signal_check_restartable_sequence_if_needed();
            expect_ne!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);

            // Clearing the accessor releases the PageMap Entry reference (returning VMO ref-count
            // to 1), does not raise `THREAD_SIGNAL_CHECK_RSEQ`, and stops
            // `signal_check_restartable_sequence_if_needed` from raising it.
            Thread::clear_signals_ptr(raw, super::THREAD_SIGNAL_CHECK_RSEQ);
            ptr.set_rseq_accessor(Accessor::default());
            expect_eq!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);
            expect_eq!(1, vmo.ref_count().ref_count_debug());
            ptr.signal_check_restartable_sequence_if_needed();
            expect_eq!(Thread::signals_ptr(raw) & super::THREAD_SIGNAL_CHECK_RSEQ, 0);

            // Re-install a valid accessor and verify `Thread::drop` releases it on destruction.
            let accessor = unwrap_ok!(PageMap::get().make_accessor::<zx_rseq_t>(vmo.clone(), 0));
            assert_true!(accessor.is_valid());
            expect_ge!(vmo.ref_count().ref_count_debug(), 2);
            ptr.set_rseq_accessor(accessor);
        }

        drop(cleanup);
        expect_eq!(1, vmo.ref_count().ref_count_debug());
    }

    /// Tests formatting idle and cpu_init thread names into a 16-byte stack buffer.
    #[test]
    fn format_cpu_name_test() {
        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"idle ", 0);
        expect_true!(s == b"idle 0");
        expect_eq!(buf[6], 0);

        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"idle ", 7);
        expect_true!(s == b"idle 7");
        expect_eq!(buf[6], 0);

        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"idle ", 10);
        expect_true!(s == b"idle 10");
        expect_eq!(buf[7], 0);

        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"", 42);
        expect_true!(s == b"42");
        expect_eq!(buf[2], 0);

        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"cpu_init ", 123);
        expect_true!(s == b"cpu_init 123");
        expect_eq!(buf[12], 0);

        // `u32::MAX` (10 digits) + `"idle "` (5 bytes) == 15 bytes + NUL fits exactly.
        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"idle ", u32::MAX);
        expect_true!(s == b"idle 4294967295");
        expect_eq!(buf[15], 0);

        // `"cpu_init "` (9 bytes) + `u32::MAX` (10 digits) truncates to 15 bytes + NUL.
        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"cpu_init ", u32::MAX);
        expect_true!(s == b"cpu_init 429496");
        expect_eq!(buf[15], 0);

        // Prefix of 15+ bytes truncates cleanly to 15 bytes + NUL with no room for digits.
        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"0123456789abcde", 9);
        expect_true!(s == b"0123456789abcde");
        expect_eq!(buf[15], 0);

        let mut buf = [0xffu8; 16];
        let s = format_cpu_name(&mut buf, b"0123456789abcdefXYZ", 99);
        expect_true!(s == b"0123456789abcde");
        expect_eq!(buf[15], 0);
    }

    /// Verifies native Rust `Thread` heap allocation and in-place initialization/destruction.
    #[test]
    fn native_thread_create_empty_and_init_in_place_test() {
        let t = unwrap_some!(Thread::create_empty()).as_ptr();

        {
            // SAFETY: `t` is a freshly constructed, exclusively owned `Thread` and stays live
            // until `Thread::delete(t)` below.
            let t_ref = unsafe { &*t };
            expect_ne!(t_ref.tid(), 0);
            expect_eq!(t_ref.pid(), ZX_KOID_INVALID);
            let mut name_buf = [0xffu8; ZX_MAX_NAME_LEN];
            t_ref.name_bytes(&mut name_buf);
            expect_eq!(name_buf[0], 0);
            expect_eq!(t_ref.sampling_session.load(), Thread::INVALID_SAMPLING_SESSION);
            expect_eq!(t_ref.get_soft_cpu_affinity(), CPU_MASK_ALL);

            thread_lock_guard!(
                guard,
                // SAFETY: `t` stays live past this scope, no CLT is active on this CPU, and we do
                // not block.
                unsafe { ThreadLockGuard::target(t, clt_tag!("native_thread_create_empty_test")) }
            );
            {
                let proof = guard.proof();
                expect_eq!(proof.state(), ThreadStateKind::Initial);
                expect_true!(proof.can_own_wait_queues());
            }
            expect_false!(guard.is_user_state_saved_locked());
            let ss = guard.scheduler_state();
            expect_eq!(ss.curr_cpu(), INVALID_CPU);
            expect_eq!(ss.last_cpu(), INVALID_CPU);
            expect_eq!(ss.hard_affinity(), CPU_MASK_ALL);
        }

        // SAFETY: `t` was allocated by `Thread::create_empty()` and has no remaining references.
        unsafe { Thread::delete(t) };

        // Exercise caller-owned in-place construction and destruction on heap-allocated storage
        // (avoiding placing a ~2.6 KiB `Thread` on the kernel stack).
        let mut storage = unwrap_ok!(kalloc::Box::<Thread>::try_new_uninit());
        let storage_ptr: *mut core::mem::MaybeUninit<Thread> = &raw mut *storage;

        // Short name initialization + in-place drop.
        {
            // SAFETY: `storage_ptr` is heap-allocated, aligned, exclusively owned, and stays
            // pinned until `drop_in_place` completes.
            unsafe { Thread::init_in_place(storage_ptr, b"in_place_test") };
            let _drop_thread = zr::defer(|| {
                // SAFETY: `storage_ptr` holds a live `Thread` initialized by
                // `Thread::init_in_place`, and this guard runs before `storage` is freed even on
                // early test failure.
                unsafe { core::ptr::drop_in_place(storage_ptr.cast::<Thread>()) };
            });

            // SAFETY: `storage_ptr` was just initialized by `Thread::init_in_place`.
            let t_ref = unsafe { &*storage_ptr.cast::<Thread>() };
            let mut name_buf = [0xffu8; ZX_MAX_NAME_LEN];
            t_ref.name_bytes(&mut name_buf);
            let got = unwrap_some!(CStr::from_bytes_until_nul(&name_buf).ok()).to_bytes();
            expect_true!(got == b"in_place_test");
        }

        // Overlong name truncation + NUL termination + in-place drop.
        {
            let overlong = b"0123456789abcdef0123456789abcdef_EXTRA_BYTES";
            // SAFETY: `storage_ptr` is writable, aligned, exclusively owned, and stays pinned
            // until `drop_in_place` completes.
            unsafe { Thread::init_in_place(storage_ptr, overlong) };
            let _drop_thread = zr::defer(|| {
                // SAFETY: `storage_ptr` holds a live `Thread` initialized by
                // `Thread::init_in_place`, and this guard runs before `storage` is freed even on
                // early test failure.
                unsafe { core::ptr::drop_in_place(storage_ptr.cast::<Thread>()) };
            });

            // SAFETY: `storage_ptr` was just initialized by `Thread::init_in_place`.
            let t_ref = unsafe { &*storage_ptr.cast::<Thread>() };
            let mut name_buf = [0xffu8; ZX_MAX_NAME_LEN];
            t_ref.name_bytes(&mut name_buf);
            let got = unwrap_some!(CStr::from_bytes_until_nul(&name_buf).ok()).to_bytes();
            expect_eq!(got.len(), ZX_MAX_NAME_LEN - 1);
            expect_true!(got == &overlong[..ZX_MAX_NAME_LEN - 1]);
            expect_eq!(name_buf[ZX_MAX_NAME_LEN - 1], 0);
        }
    }

    /// Verifies native Rust `ThreadLockGuard` acquisition, inspection, release, and re-acquisition.
    #[test]
    fn native_thread_lock_guard_test() {
        let t = unwrap_some!(Thread::create_empty()).as_ptr();

        {
            // SAFETY: `t` is a freshly constructed, exclusively owned `Thread` and stays live
            // until `Thread::delete(t)` below.
            let t_ref = unsafe { &*t };
            expect_true!(t_ref.lock().is_unlocked());

            for _ in 0..2 {
                {
                    thread_lock_guard!(
                        guard,
                        // SAFETY: `t` stays live past this scope, no CLT is active on this CPU, and
                        // we do not block.
                        unsafe {
                            ThreadLockGuard::target(t, clt_tag!("native_thread_lock_guard_test"))
                        }
                    );
                    expect_false!(t_ref.lock().is_unlocked());
                    expect_eq!(guard.thread(), t);
                    expect_true!(core::ptr::eq(guard.proof().owner(), t));
                    expect_false!(guard.is_user_state_saved_locked());
                    expect_eq!(guard.scheduler_state().curr_cpu(), INVALID_CPU);
                }
                expect_true!(t_ref.lock().is_unlocked());
            }
        }

        // SAFETY: `t` was allocated by `Thread::create_empty()` and has no remaining references.
        unsafe { Thread::delete(t) };
    }

    /// Verifies native `current_switch_aspace` and `current_clear_aspace` round-trip.
    #[test]
    fn current_switch_and_clear_aspace_test() {
        let kernel_aspace = crate::vm::vm_aspace::VmAspace::kernel_aspace_ref();
        let kernel_aspace_ptr = fbl::RefPtr::as_ptr(&kernel_aspace).cast_mut();

        // Keep interrupts disabled across the temporary aspace swap so no IRQ or reschedule can
        // observe the test's transient `aspace` state. Restore `orig` before re-enabling
        // interrupts and running `expect_*!` assertions (which may log/allocate).
        let (orig, after_set, cleared, after_clear, after_restore) = {
            let _irqd = crate::arch_rs::InterruptDisableGuard::new();
            // SAFETY: The current thread is live, the kernel aspace is never released (valid even
            // if observed cross-CPU), interrupts are disabled so no reschedule or MMU switch
            // occurs, and we restore `orig` before re-enabling interrupts.
            let orig = unsafe { super::current_switch_aspace(&kernel_aspace) };
            // SAFETY: The current thread is live and interrupts are disabled.
            let after_set = unsafe { super::active_aspace(current_get()) };
            // SAFETY: The current thread is live and interrupts are disabled.
            let cleared = unsafe { super::current_clear_aspace() };
            // SAFETY: The current thread is live and interrupts are disabled.
            let after_clear = unsafe { super::active_aspace(current_get()) };
            // SAFETY: Restoring the original `aspace` pointer before re-enabling interrupts.
            let _ = unsafe { super::switch_aspace_raw(current_get(), orig) };
            // SAFETY: The current thread is live and interrupts are disabled.
            let after_restore = unsafe { super::active_aspace(current_get()) };
            (orig, after_set, cleared, after_clear, after_restore)
        };

        expect_eq!(after_set, kernel_aspace_ptr);
        expect_eq!(cleared, kernel_aspace_ptr);
        expect_true!(after_clear.is_null());
        expect_eq!(after_restore, orig);
    }
}

/// Preemption disable and timeslice extension unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "preempt_disable_tests_rust")]
mod preempt_disable_tests {
    use super::{
        AnnotatedAutoEagerReschedDisabler, AutoEagerReschedDisabler, AutoExpiringPreemptDisabler,
        AutoPreemptDisabler, DEFAULT_PRIORITY, PreemptionState, ThreadLockGuard, ThreadPtr,
        create_with_priority, current_get, current_preemption_state, preempt, reschedule,
        sleep_relative, r#yield,
    };
    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
    use crate::kconcurrent::clt_tag;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::event::Event;
    use crate::kernel::interrupt::{IntHandlerSavedState, int_handler_finish, int_handler_start};
    use crate::kernel::mp::CPU_MASK_ALL;
    use crate::kernel::scheduler;
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t, cpu_num_to_mask};
    use crate::platform_rs::timer::{DurationMono, InstantMono, zx_usec};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};
    use pin_init::{pin_init, stack_pin_init};
    use unittest::{
        assert_eq, assert_ne, assert_true, expect_eq, expect_false, expect_ne, expect_true,
        subtest, unwrap_ok,
    };
    use zx_types::zx_instant_mono_t;

    /// Mirror of C++ `PreemptDisableTestAccess::State`.
    struct State {
        preempts_pending: cpu_mask_t,
    }

    /// Mirror of C++ `PreemptDisableTestAccess::SaveState`.
    fn save_state(preemption_state: &PreemptionState) -> State {
        State { preempts_pending: preemption_state.preempts_pending() }
    }

    /// Mirror of C++ `PreemptDisableTestAccess::RestoreState`.
    fn restore_state(preemption_state: &PreemptionState, state: State) {
        preemption_state.preempts_pending.store(state.preempts_pending);
    }

    /// Mirror of C++ `PreemptDisableTestAccess::ClearPending`.
    fn clear_pending(preemption_state: &PreemptionState) {
        preemption_state.preempts_pending.store(0);
    }

    /// Test that PreemptDisable is set for timer callbacks and that, in this
    /// context, preempts_pending will get set by some functions.
    ///
    /// # Safety
    ///
    /// `arg` must point to the `Event` that `test_in_timer_callback` keeps alive until the timer
    /// has been cancelled, and this must run as a `Timer` callback (interrupt context).
    unsafe extern "C" fn timer_callback_func(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` is the pinned `Event` that `test_in_timer_callback` keeps alive until the
        // timer has been cancelled.
        let event = unsafe { &*arg.cast::<Event>() };

        // The timer should run in interrupt context.  (`assert!` panics like the C++ `ASSERT`;
        // the unittest `expect_*` macros are only usable from the test body itself.)
        assert!(crate::arch_rs::ints_disabled());
        assert!(crate::arch_rs::blocking_disallowed());

        // Entry into interrupt context should disable preemption and eager
        // reschedules.
        // SAFETY: The interrupted thread is the current thread and stays alive for the callback;
        // `PreemptionState` is all relaxed atomics.
        let preemption_state = unsafe { &*current_preemption_state() };
        assert!(preemption_state.preempt_disable_count() > 0);
        assert!(preemption_state.eager_resched_disable_count() > 0);
        let state = save_state(preemption_state);

        // Test that scheduler::reschedule sets the preempt_pending flag when
        // PreemptDisable is set.
        clear_pending(preemption_state);
        assert!(preemption_state.preempts_pending() == 0);
        {
            let current_thread = current_get();
            thread_lock_guard!(
                _guard,
                // SAFETY: `current_thread` is the live current thread (stays live past this
                // scope), no chain lock transaction is active in this timer callback, and we do
                // not block.
                unsafe {
                    ThreadLockGuard::target(
                        current_thread,
                        clt_tag!("PreemptDisableTests timer_callback"),
                    )
                }
            );
            // SAFETY: `current_thread` is the current CPU's active thread, `_guard` holds its lock
            // as the sole lock of a finalized transaction with interrupts disabled and no
            // conflicting `ChainLockProof` or live borrow of `current_thread`'s lock-protected
            // state, and no `queue_lock_` is held in a timer callback.
            unsafe { scheduler::reschedule(current_thread) };
        }
        assert!(preemption_state.preempts_pending() != 0);

        // Test that preemption_state.PreemptSetPending() sets preempts_pending.
        clear_pending(preemption_state);
        assert!(preemption_state.preempts_pending() == 0);

        preemption_state.preempt_set_pending(cpu_num_to_mask(curr_cpu_num()));
        assert!(preemption_state.preempts_pending() != 0);

        restore_state(preemption_state, state);
        event.signal();
    }

    /// Schedule a timer callback and wait for it to complete.
    //
    // Most of the testing is done in the timer callback.
    #[test]
    fn test_in_timer_callback() {
        stack_pin_init!(let event = Event::init_unsignaled());
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));

        // SAFETY: `timer` and `event` are stack-pinned and outlive the timer, which `_cancel`
        // cancels before either goes out of scope (also on an early return from a failed
        // assertion); `timer_callback_func` is a valid timer callback.
        unsafe {
            timer.as_ref().set_oneshot(
                0,
                timer_callback_func,
                core::ptr::from_ref(&*event).cast_mut().cast(),
            );
        }
        // Make sure the timer has fully completed prior to letting it go out of scope.
        let _cancel = zr::defer(|| {
            timer.cancel();
        });
        assert_true!(event.wait_infinite().is_ok());
    }

    /// Test incrementing and decrementing the PreemptDisable and EagerReschedDisable counts.
    #[test]
    fn test_inc_dec_disable_counts() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // Test initial conditions.
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());
        // While preemption is allowed, a preemption should not be pending.
        expect_eq!(preemption_state.preempts_pending(), 0);

        // Test incrementing and decrementing of PreemptDisable.
        preemption_state.preempt_disable();
        expect_eq!(preemption_state.preempt_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: `preemption_state` is the current thread's, tests may block, this pairs with
        // the `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test incrementing and decrementing of EagerReschedDisable.
        preemption_state.eager_resched_disable();
        expect_eq!(preemption_state.eager_resched_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: `preemption_state` is the current thread's, tests may block, this pairs with
        // the `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test nesting: multiple increments and decrements.
        preemption_state.preempt_disable();
        preemption_state.preempt_disable();
        expect_eq!(preemption_state.preempt_disable_count(), 2);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above; each call pairs with one of the two `preempt_disable()`s above.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Test nesting: multiple increments and decrements.
        preemption_state.eager_resched_disable();
        preemption_state.eager_resched_disable();
        expect_eq!(preemption_state.eager_resched_disable_count(), 2);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above; each call pairs with one of the two `eager_resched_disable()`s above.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 1);
        expect_false!(preemption_state.preempt_is_enabled());
        // SAFETY: As above.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_true!(preemption_state.preempt_is_enabled());

        // Every disable above is paired with its reenable and `expect_*` never returns early, so
        // the counts are back to zero however the checks went.
        expect_eq!(preemption_state.preempt_disable_count(), 0);
        expect_eq!(preemption_state.eager_resched_disable_count(), 0);
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Mirror of C++ `test_decrement_clears_preempt_pending`: reenabling flushes pending bits.
    #[test]
    fn test_decrement_clears_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        assert_true!(preemption_state.preempt_is_enabled());
        assert_eq!(preemption_state.preempts_pending(), 0);

        // Test that preemption_state.preempt_reenable() clears preempt_pending.
        preemption_state.preempt_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);

        // Test that preemption_state.eager_resched_reenable() clears preempt_pending.
        preemption_state.eager_resched_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Mirror of C++ `test_blocking_clears_preempt_pending`: blocking clears pending bits.
    #[test]
    fn test_blocking_clears_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // It is OK to block while preemption is disabled. In this case, blocking
        // should clear a pending local preemption.
        preemption_state.preempt_disable();
        reschedule();
        expect_ne!(preemption_state.preempts_pending(), 0);
        {
            let _int_state = InterruptDisableGuard::new();
            let _ = sleep_relative(DurationMono::from_millis(10));
            // Read preempts_pending with interrupts disabled because otherwise an
            // interrupt handler could set it.
            expect_eq!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };

        // It is OK to block while eager rescheduling is disabled. In this case,
        // blocking should clear all pending preemptions.
        preemption_state.eager_resched_disable();
        reschedule();
        {
            let _int_state = InterruptDisableGuard::new();
            let _ = sleep_relative(DurationMono::from_millis(10));
            // Read preempts_pending with interrupts disabled because otherwise an
            // interrupt handler could set it.
            expect_eq!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
    }

    /// Mirror of C++ `test_interrupt_preserves_preempt_pending`.
    //
    // Test that preempts_pending is preserved across an interrupt handler when
    // EagerReschedDisable is set and when the interrupt handler does not cause a
    // preemption. This tests the int_handler_start()/finish() routines rather than
    // the full interrupt handler.
    #[test]
    fn test_interrupt_preserves_preempt_pending() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        preemption_state.eager_resched_disable();
        {
            // Do this with interrupts disabled so that a real interrupt does not
            // clear preempts_pending.
            let _int_state = InterruptDisableGuard::new();
            reschedule();

            // Simulate an interrupt handler invocation.
            let mut state = IntHandlerSavedState::default();
            // SAFETY: The test runs on a valid current thread with interrupts disabled by
            // `_int_state`, and `int_handler_finish` is called below with the same `state`.
            unsafe { int_handler_start(&mut state) };
            expect_eq!(preemption_state.preempt_disable_count(), 1);
            // SAFETY: Pairs with the `int_handler_start` above, interrupts are still disabled,
            // and no `PreemptDisabledToken` exists in this test.
            let do_preempt = unsafe { int_handler_finish(&mut state) };

            expect_false!(do_preempt);
            expect_ne!(preemption_state.preempts_pending(), 0);
        }
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `eager_resched_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.eager_resched_reenable() };
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Timer callback used for testing.
    ///
    /// # Safety
    ///
    /// `arg` must point to the `AtomicBool` that `test_interrupt_with_preempt_disable` keeps alive
    /// until the timer has been cancelled, and this must run as a `Timer` callback.
    unsafe extern "C" fn timer_set_preempt_pending(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` is the `AtomicBool` that `test_interrupt_with_preempt_disable` keeps
        // alive until the timer has been cancelled.
        let timer_ran = unsafe { &*arg.cast::<AtomicBool>() };

        // SAFETY: The interrupted thread is the current thread and stays alive for the callback;
        // `PreemptionState` is all relaxed atomics.
        unsafe {
            (*current_preemption_state()).preempt_set_pending(cpu_num_to_mask(curr_cpu_num()))
        };
        timer_ran.store(true, Ordering::SeqCst);
    }

    /// Mirror of C++ `test_interrupt_with_preempt_disable`.
    #[test]
    fn test_interrupt_with_preempt_disable() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        // Test that interrupt handlers honor PreemptDisable.
        //
        // We test that by setting a timer callback that will set
        // preempt_pending from inside an interrupt handler.  preempt_pending
        // should remain set after the interrupt handler returns.
        //
        // This assumes that timer_set() will run the callback on the same CPU
        // that we invoked it from.  This also assumes that we don't
        // accidentally call any blocking operations that cause our thread to
        // be rescheduled to another CPU.
        preemption_state.preempt_disable();
        let timer_ran = AtomicBool::new(false);
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::after_mono(DurationMono(zx_usec(100)), TimerSlack::none());
        // SAFETY: `timer` is stack-pinned and `timer_ran` outlives it: `_cancel` cancels the timer
        // before either goes out of scope (also on an early return from a failed assertion);
        // `timer_set_preempt_pending` is a valid timer callback.
        unsafe {
            timer.as_ref().set_deadline(
                &deadline,
                timer_set_preempt_pending,
                core::ptr::from_ref(&timer_ran).cast_mut().cast(),
            );
        }
        // Make sure the timer has fully completed prior to letting it go out of scope.
        let _cancel = zr::defer(|| {
            timer.cancel();
        });
        // Spin until timer_ran is set by the interrupt handler.
        while !timer_ran.load(Ordering::SeqCst) {
            libarch::r#yield();
        }
        expect_eq!(preemption_state.preempts_pending(), cpu_num_to_mask(curr_cpu_num()));
        // SAFETY: `preemption_state` is the current thread's, this pairs with the
        // `preempt_disable()` above, and no `PreemptDisabledToken` exists in this test.
        unsafe { preemption_state.preempt_reenable() };
    }

    /// Shared with the `waiter` thread of `test_local_preempt_pending`.
    struct Args {
        event: *const Event,
        started: AtomicBool,
    }

    extern "C" fn waiter(void_args: *mut c_void) -> i32 {
        // SAFETY: `void_args` is the `Args` that `setup_and_run_with` keeps alive (along with the
        // `Event` it points at) until it has joined this thread.
        let (args, event) = unsafe {
            let args = &*void_args.cast::<Args>();
            (args, &*args.event)
        };
        // Let the other thread know that we're up and running and then wait to be signaled.
        args.started.store(true, Ordering::SeqCst);
        let _ = event.wait_infinite();
        0
    }

    /// The common code used in all cases of `test_local_preempt_pending`.
    ///
    /// `setup_and_run_with` is used to setup test conditions and run the `func` test case.
    ///
    /// `func` receives an Event that it should signal to unblock the `waiter` thread (see above).
    fn setup_and_run_with(func: &dyn Fn(&Event) -> bool) -> bool {
        (subtest!(|func: &dyn Fn(&Event) -> bool| {
            // Make sure we restore this thread's affinity.
            // SAFETY: Threading is initialized by the time unit tests run.
            let current = unsafe { ThreadPtr::current() };
            // SAFETY: The current thread is alive for the duration of this test.
            let affinity = unsafe { current.get_cpu_affinity() };
            let _cleanup = zr::defer(move || {
                // SAFETY: The current thread is alive for the duration of this test.
                unsafe { current.set_cpu_affinity(affinity) };
            });

            stack_pin_init!(let event = Event::init_unsignaled());
            let args =
                Args { event: core::ptr::from_ref(&*event), started: AtomicBool::new(false) };

            let target_cpu = BOOT_CPU_ID;

            // Migrate the current thread to the target CPU and bind a |waiter| thread to the same
            // CPU.
            let mask = cpu_num_to_mask(target_cpu);
            // SAFETY: The current thread is alive for the duration of this test.
            unsafe { current.set_cpu_affinity(mask) };
            // SAFETY: `waiter` is a valid thread entry point, and `args` and `event` outlive the
            // thread, which `_cleanup_waiter` joins below.
            let t = unwrap_ok!(unsafe {
                create_with_priority(
                    c"test_local_preempt_pending".as_ptr(),
                    waiter,
                    core::ptr::from_ref(&args).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            });
            let _cleanup_waiter = zr::defer(|| {
                event.signal();
                // SAFETY: `t` is joined exactly once, here.
                let _ = unsafe { t.join(InstantMono::INFINITE) };
            });
            // SAFETY: `t` has not been joined yet.
            unsafe { t.set_cpu_affinity(mask) };

            // Start the |waiter| and spin until we know that it has started running and then
            // blocked.
            // SAFETY: `t` was just created and has not been resumed or joined yet.
            unsafe { t.resume() };
            loop {
                r#yield();
                // SAFETY: `t` has not been joined yet.  (`is_blocked` checks the scheduler state
                // under `t`'s lock, as the C++ `SingleChainLockGuard` scope does.)
                if args.started.load(Ordering::SeqCst) && unsafe { t.is_blocked() } {
                    break;
                }
            }

            // At this point we know the |waiter| is blocked on the event.
            expect_true!(func(&event));
        }))(func)
    }

    /// Unblocking a thread pinned to the current CPU marks the CPU for preemption.
    //
    // Verify that in certain contexts where preemption cannot immediately occur, unblocking a
    // thread pinned to the current CPU will mark the CPU for preemption.
    //
    // This test covers four cases:
    //
    // 1. preemption is disabled
    // 2. eager resched is disabled
    // 3. timeslice extension
    // 4. a spinlock is held
    //
    // See https://fxbug.dev/42051271 for motivation.
    #[test]
    fn test_local_preempt_pending() {
        // Now test each case using the common code above.

        // 1. Preemption disabled should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _apd = AutoPreemptDisabler::new();
            // Unblock the |waiter|.  Because we've got preemption disabled, a preemption event
            // for the local CPU should become pending.
            event.signal();
            // SAFETY: The current thread stays alive for the duration of this test, and
            // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 2. Eager resched disabled should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _aerd = AutoEagerReschedDisabler::new();
            // Unblock the |waiter|.  Because we've got eager resched disabled (which implies
            // preempt disable), a preemption event for the local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 3. A timeslice extension should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            // Unblock the |waiter|.  Because we've got eager resched disabled (which implies
            // preempt disable), a preemption event for the local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));

        // 4. Holding a spinlock should cause a preemption event to become pending.
        expect_true!(setup_and_run_with(&subtest!(|event: &Event| {
            crate::ksync_kernel::declare_singleton_monitored_spinlock!(LocalLock);
            ksync::lock!(let _guard = LocalLock::get().lock_with(ksync::source_tag!()));
            // Unblock the |waiter|.  Because we're holding a spinlock, a preemption event for the
            // local CPU should become pending.
            event.signal();
            // SAFETY: As in case 1.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            expect_ne!(0, pending);
            expect_true!((pending | cpu_num_to_mask(curr_cpu_num())) != 0);
        })));
    }

    /// Mirror of C++ `test_evaluate_timeslice_extension`: extension activation and expiry.
    #[test]
    fn test_evaluate_timeslice_extension() {
        // Nothing preventing preemption.
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        expect_true!(preemption_state.preempt_is_enabled());
        expect_true!(preemption_state.evaluate_timeslice_extension());
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by count).
        {
            let _apd = AutoPreemptDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by eager resched count).
        {
            let _aerd = AnnotatedAutoEagerReschedDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // Disabled (by infinite timeslice extension).
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // In the tests below, the current thread will defer preemption for
        // EPSILON_DURATION and sleep for EPSILON_DURATION.  The only requirement for
        // correctness is that this value is greater than zero.  We use the value 1 to
        // minimize test runtime.
        const EPSILON_DURATION: DurationMono = DurationMono(1);

        // See that the timeslice extension expires.
        {
            let _guard = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            // Note, we cannot reliably assert that preemption is disabled at this point
            // because a preemption request may have already occurred and
            // EPSILON_DURATION may have already elapsed.
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_true!(preemption_state.evaluate_timeslice_extension());
            expect_true!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // AutoPreemptDisabler inside an expired AutoExpiringPreemptDisabler.
        {
            let _guard1 = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            let _guard2 = AutoPreemptDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_false!(preemption_state.evaluate_timeslice_extension());
            // Still false because of the APD.
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());

        // AutoEagerReschedDisabler inside an expired AutoExpiringPreemptDisabler.
        {
            let _guard1 = AutoExpiringPreemptDisabler::new(EPSILON_DURATION);
            let _guard2 = AnnotatedAutoEagerReschedDisabler::new();
            expect_false!(preemption_state.preempt_is_enabled());
            reschedule();
            let _ = sleep_relative(EPSILON_DURATION);
            expect_false!(preemption_state.evaluate_timeslice_extension());
            // Still false because of the AERD.
            expect_false!(preemption_state.preempt_is_enabled());
        }
        expect_true!(preemption_state.preempt_is_enabled());
        expect_eq!(preemption_state.preempts_pending(), 0);
    }

    /// Holder for the `CriticalMutex` of `test_apd_in_critical_mutex`.
    #[ksync::guarded]
    struct CriticalMutexHolder {
        #[mutex]
        mutex: ksync::KMutex<crate::ksync_kernel::TrackedCriticalMutex>,
    }

    /// An AutoPreemptDisabler inside a CriticalMutex does not clear the mutex's extension.
    //
    // Verify that an AutoPreemptDisabler inside a CriticalMutex with a non-expired
    // timeslice extension does not clear the CriticalMutex's extension.
    #[test]
    fn test_apd_in_critical_mutex() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        stack_pin_init!(let holder = pin_init!(CriticalMutexHolder {
            mutex <- ksync::KMutex::init(),
        }));

        {
            // `KMutex<TrackedCriticalMutex>`'s guard is the `Acquire`/`Release(should_clear)` pair.
            ksync::lock!(let _guard = holder.lock_mutex());

            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(preemption_state.evaluate_timeslice_extension());
            expect_false!(preemption_state.preempt_is_enabled());

            {
                let _apd = AutoPreemptDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());
                expect_false!(preemption_state.evaluate_timeslice_extension());
                expect_false!(preemption_state.preempt_is_enabled());
            }
        }
    }

    /// Releasing a `CriticalMutex` clears the timeslice extension set by acquiring it.
    #[test]
    fn test_critical_mutex_release_clears_timeslice_extension() {
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };

        stack_pin_init!(let holder = pin_init!(CriticalMutexHolder {
            mutex <- ksync::KMutex::init(),
        }));

        expect_true!(preemption_state.preempt_is_enabled());
        // Acquire twice, to check that the first release left no extension behind that would
        // prevent the second acquisition from setting its own.
        for _ in 0..2 {
            {
                ksync::lock!(let _guard = holder.lock_mutex());
                expect_false!(preemption_state.preempt_is_enabled());
            }
            expect_true!(preemption_state.preempt_is_enabled());
        }
    }

    /// Mirror of C++ `test_flush_race`: reenabling with an inactive timeslice extension in place.
    #[test]
    fn test_flush_race() {
        // This test simulates a race condition where a preemption is requested (via IPI
        // or timer) concurrent with reenabling preemption / eager rescheduling while an
        // inactive timeslice extension is in place.
        // SAFETY: The current thread stays alive for the duration of this test, and
        // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
        let preemption_state = unsafe { &*current_preemption_state() };
        expect_true!(preemption_state.preempt_is_enabled());

        // In each block below, dropping `_guard` clears the extension with the counts at zero and
        // so flushes the pending local bit the block left behind, as in C++.

        // Test PreemptReenable.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            {
                let _apd = AutoPreemptDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());

                // We'll simulate a race by marking the current CPU as pending for
                // preemption without going through the normal PreemptSetPending path
                // because we want to test the behavior when the extension is
                // inactive, and PreemptSetPending will activate it.
                preemption_state.preempts_pending_add(curr_mask);
                // When we leave this scope and reenable preemption, we'd better not flush.
            }
            expect_false!(preemption_state.preempt_is_enabled());
            // See that we did not flush.
            expect_eq!(curr_mask, preemption_state.preempts_pending());
        }

        // Test PreemptReenableDelayFlush.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            let should_preempt = {
                let _irqd = InterruptDisableGuard::new();
                crate::arch_rs::set_blocking_disallowed(true);
                let _cleanup = zr::defer(|| crate::arch_rs::set_blocking_disallowed(false));
                preemption_state.preempt_disable();
                preemption_state.preempts_pending_add(curr_mask);
                // SAFETY: `preemption_state` is the current thread's state, interrupts are
                // disabled and blocking is disallowed, this pairs with the `preempt_disable()`
                // just above, and no `PreemptDisabledToken` exists in this test.
                unsafe { preemption_state.preempt_reenable_delay_flush() }
            };
            expect_false!(preemption_state.preempt_is_enabled());
            expect_false!(should_preempt);
        }

        // Test EagerReschedReenable.
        {
            let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
            let curr_mask = cpu_num_to_mask(curr_cpu_num());
            {
                let _aerd = AnnotatedAutoEagerReschedDisabler::new();
                expect_false!(preemption_state.preempt_is_enabled());
                preemption_state.preempts_pending_add(CPU_MASK_ALL);
            }
            expect_false!(preemption_state.preempt_is_enabled());
            // See that we flushed the remote CPUs, but not the local.
            expect_eq!(curr_mask, preemption_state.preempts_pending());
        }

        expect_true!(preemption_state.preempt_is_enabled());
        expect_eq!(preemption_state.preempts_pending(), 0);
        expect_false!(crate::arch_rs::blocking_disallowed());
    }

    /// Verify that it's legal to call preempt while holding a spinlock.
    //
    // This is a regression test for https://fxbug.dev/473600952.
    #[test]
    fn test_preempt_with_spinlock() {
        crate::ksync_kernel::declare_singleton_monitored_spinlock!(SpinlockForTest);

        {
            ksync::lock!(let _guard = SpinlockForTest::get().lock_with(ksync::source_tag!()));
            preempt();
            // SAFETY: The current thread stays alive for the duration of this test, and
            // `PreemptionState` is all relaxed atomics; no `&Thread` is formed.
            let pending = unsafe { (*current_preemption_state()).preempts_pending() };
            assert_ne!(0, pending & cpu_num_to_mask(curr_cpu_num()));
        }
    }
}
