// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Blocking (i.e. non spinning) reader-writer locks ([`BrwLockPi`] and [`BrwLockNoPi`]), plus the
//! `ksync` raw locks built on them ([`TrackedBrwLockPi`] and [`TrackedBrwLockNoPi`]) and their
//! lock policies, which are re-exported from `crate::ksync_kernel`.

use affine::Round;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{align_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::{ExclusiveLockPolicy, LockPolicy, LockdepTracked, RawLock, RawSharedLock};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_status_t;

use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{blocking_disallowed, ints_disabled};
use crate::kconcurrent::{
    ChainLockProof, ChainLockTransaction, ChainLockTransactionToken, ChainLocked,
    EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION, OwnerGuard, PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
    TransactionResult, clt_tag,
};
use crate::kernel::auto_preempt_disabler::AnnotatedAutoPreemptDisabler;
use crate::kernel::deadline::Deadline;
use crate::kernel::lock_trace::{lock_trace_duration, lock_trace_flow_begin, lock_trace_flow_end};
use crate::kernel::mutex::Mutex;
use crate::kernel::owned_wait_queue::{OwnedWaitQueue, ThreadUnblockList};
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler;
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::kernel::spin_tracing::{
    EncodedLockId, IdStringRef, RWLOCK_NAME_TRACING_ENABLED, RwLockNameStorage,
};
use crate::kernel::task_runtime_timers::ContentionTimer;
use crate::kernel::thread::{self, PreemptDisabledToken, Thread, ThreadPtr};
use crate::kernel::wait_queue::{BrwLockOps, Interruptible, ResourceOwnership, WaitQueue};
use crate::platform_rs::timer::{
    InstantMonoTicks, timer_current_mono_ticks, timer_get_ticks_to_time_ratio,
};
use crate::top::debug::get_frame;

// Used only with priority inheritance, which requires 16-byte atomics (see `enable_pi::Yes`).
#[cfg(target_has_atomic = "128")]
use crate::kernel::auto_preempt_disabler::AnnotatedAutoEagerReschedDisabler;
#[cfg(target_has_atomic = "128")]
use crate::kernel::owned_wait_queue::{BAAOLockingDetails, IWakeRequeueHook, WakeOption};
#[cfg(target_has_atomic = "128")]
use crate::kernel::wait_queue::ForceInheritance;
#[cfg(target_has_atomic = "128")]
use core::sync::atomic::{AtomicU128, AtomicUsize};

/// Magic value identifying a [`BrwLock`] (`'RWLK'`).
const MAGIC: u32 = fbl::magic(b"RWLK");

/// Whether `LK_DEBUGLEVEL > 0`.
const LK_DEBUGLEVEL_GT_0: bool = !cfg!(lk_debuglevel = "0");

/// State value when no readers, writers, or waiters are present.
const BRW_LOCK_UNLOCKED: u64 = 0;
/// We count readers in the low 32 bits of the state.
const BRW_LOCK_READER: u64 = 1;
/// Mask of active/optimistic readers in the state word.
const BRW_LOCK_READER_MASK: u64 = 0xFFFFFFFF;
/// We count waiters in all but the MSB of the high 32 bits of the state.
const BRW_LOCK_WAITER: u64 = 1u64 << 32;
/// Mask of waiters in the state word.
const BRW_LOCK_WAITER_MASK: u64 = 0x7FFFFFFF00000000;
/// Exclusive writer flag in the MSB of the state word.
const BRW_LOCK_WRITER: u64 = 1u64 << 63;

static_assert!((BRW_LOCK_READER_MASK & BRW_LOCK_WAITER_MASK) == 0);
static_assert!((BRW_LOCK_READER_MASK & BRW_LOCK_WRITER) == 0);
static_assert!((BRW_LOCK_WAITER_MASK & BRW_LOCK_WRITER) == 0);
static_assert!((BRW_LOCK_READER_MASK | BRW_LOCK_WAITER_MASK | BRW_LOCK_WRITER) == u64::MAX);

/// Returns whether `state` has at least one active or optimistic reader.
#[inline(always)]
const fn state_has_readers(state: u64) -> bool {
    (state & BRW_LOCK_READER_MASK) != 0
}

/// Returns whether `state` has an active writer.
#[inline(always)]
const fn state_has_writer(state: u64) -> bool {
    (state & BRW_LOCK_WRITER) != 0
}

/// Returns whether `state` has at least one blocked or speculative waiter.
#[inline(always)]
const fn state_has_waiters(state: u64) -> bool {
    (state & BRW_LOCK_WAITER_MASK) != 0
}

/// Returns the active/optimistic reader count encoded in `state`.
#[inline(always)]
const fn state_reader_count(state: u64) -> u32 {
    (state & BRW_LOCK_READER_MASK) as u32
}

/// Computes the monotonic tick deadline at which a contended spin phase starting at `now_ticks`
/// should give up and block.
#[inline]
fn spin_until_ticks(now_ticks: InstantMonoTicks) -> InstantMonoTicks {
    let spin_max_duration = Mutex::SPIN_MAX_DURATION;
    let time_to_ticks = timer_get_ticks_to_time_ratio().inverse();
    InstantMonoTicks(affine::clamp_add(
        now_ticks.0,
        time_to_ticks.scale::<{ Round::DOWN }>(spin_max_duration.0),
    ))
}

mod sealed {
    /// Prevents types outside of this module from implementing the `BrwLock` selector traits.
    pub trait Sealed {}
}

/// The atomic state word of a [`BrwLock`], and (when priority inheritance is enabled) the current
/// writer.
///
/// Every operation that loads or stores the state or writer is relaxed or as ordered by its
/// caller, matching the individual accesses made by [`BrwLock`].
pub trait BrwLockState: sealed::Sealed + Send + Sync + Sized + 'static {
    /// Creates a state holding `state` and no writer.
    fn new(state: u64) -> Self;

    /// Returns the state word.
    fn state(&self) -> &AtomicU64;

    /// Returns the current writer (`Relaxed` load), if any. Always `None` when priority
    /// inheritance is disabled, as the writer is then not tracked.
    fn writer(&self) -> Option<NonNull<Thread>>;

    /// Records `writer` as the current writer (`Relaxed` store). Does nothing when priority
    /// inheritance is disabled, as the writer is then not tracked.
    fn set_writer(&self, writer: Option<NonNull<Thread>>);

    /// Atomically replaces the state `current_state` (with no writer) by `new_state` with the
    /// writer `new_writer`, returning the previous state on success and the observed state on
    /// failure. When priority inheritance is enabled, the state and writer are compared and
    /// exchanged together.
    fn compare_exchange(
        &self,
        current_state: u64,
        new_state: u64,
        new_writer: Option<NonNull<Thread>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64>;
}

/// The [`BrwLockState`] of a [`BrwLock`] with priority inheritance: the state word followed by
/// the current writer, which are exchanged together by a single 16-byte compare and swap.
///
/// The fields are stored in one [`AtomicU128`] (state in the low 8 bytes, writer in the high 8
/// bytes), from which the 8-byte atomic views of each field are derived so that every access has
/// provenance over all 16 bytes. The writer is stored as an address with exposed provenance.
///
/// The 8-byte accesses through those views race with the 16-byte compare and swap of both
/// fields. The Rust memory model does not define such mixed-size atomic accesses; this relies on
/// the 16-byte compare and swap lowering to a native single-copy atomic instruction (`lock
/// cmpxchg16b` on x86-64, and `casp` or an `ldaxp`/`stxp` loop on arm64), which x86-64 and arm64
/// guarantee to be atomic with respect to the overlapping 8-byte atomic accesses.
#[cfg(target_has_atomic = "128")]
#[repr(C, align(16))]
pub struct BrwLockStatePi {
    raw: AtomicU128,
}

#[cfg(target_has_atomic = "128")]
static_assert!(
    size_of::<BrwLockStatePi>() == 16,
    "PI BrwLockState expected to be exactly 16 bytes"
);
#[cfg(target_has_atomic = "128")]
static_assert!(align_of::<BrwLockStatePi>() == 16);
static_assert!(cfg!(target_endian = "little"), "PI BrwLockState assumes little endian ordering");

#[cfg(target_has_atomic = "128")]
impl BrwLockStatePi {
    /// Returns the 8-byte atomic view of the writer, the high 8 bytes of `raw`.
    #[inline(always)]
    fn writer_atomic(&self) -> &AtomicUsize {
        // SAFETY: `raw.as_ptr()` points to 16 live, 16-byte aligned bytes owned by `self`, so the
        // high 8 bytes are valid and 8-byte aligned for the lifetime of `&self`, and are only
        // ever accessed atomically. These 8-byte accesses race with the 16-byte
        // `compare_exchange` below, a mixed-size atomic access which the Rust memory model does
        // not define; see the type documentation for why this relies on the hardware instead.
        unsafe { AtomicUsize::from_ptr(self.raw.as_ptr().cast::<usize>().wrapping_add(1)) }
    }
}

#[cfg(target_has_atomic = "128")]
impl sealed::Sealed for BrwLockStatePi {}

#[cfg(target_has_atomic = "128")]
impl BrwLockState for BrwLockStatePi {
    #[inline(always)]
    fn new(state: u64) -> Self {
        Self { raw: AtomicU128::new(u128::from(state)) }
    }

    #[inline(always)]
    fn state(&self) -> &AtomicU64 {
        // SAFETY: As in `writer_atomic`, for the low 8 bytes of `raw`, which hold the state as
        // the target is little endian.
        unsafe { AtomicU64::from_ptr(self.raw.as_ptr().cast::<u64>()) }
    }

    #[inline(always)]
    fn writer(&self) -> Option<NonNull<Thread>> {
        // Ordering: `Relaxed`. The writer is only consumed by `lock_for_block`, under the queue's
        // lock, as the priority inheritance target (see the comment in `write_release` on why it
        // is fine to observe a stale value there), and by the writer's own debug checks of the
        // value it stored itself.
        NonNull::new(ptr::with_exposed_provenance_mut(self.writer_atomic().load(Ordering::Relaxed)))
    }

    #[inline(always)]
    fn set_writer(&self, writer: Option<NonNull<Thread>>) {
        // Ordering: `Relaxed`, as for `writer`.
        let addr = writer.map_or(0, |writer| writer.as_ptr().expose_provenance());
        self.writer_atomic().store(addr, Ordering::Relaxed);
    }

    #[inline(always)]
    fn compare_exchange(
        &self,
        current_state: u64,
        new_state: u64,
        new_writer: Option<NonNull<Thread>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64> {
        let current = u128::from(current_state);
        let new_writer = new_writer.map_or(0, |writer| writer.as_ptr().expose_provenance());
        let new = u128::from(new_state) | ((new_writer as u128) << 64);
        self.raw
            .compare_exchange(current, new, success, failure)
            .map(|prev| prev as u64)
            .map_err(|observed| observed as u64)
    }
}

/// The [`BrwLockState`] of a [`BrwLock`] without priority inheritance: just the state word.
#[repr(C)]
pub struct BrwLockStateNoPi {
    state: AtomicU64,
}

static_assert!(
    size_of::<BrwLockStateNoPi>() == 8,
    "Non PI BrwLockState expected to be exactly 8 bytes"
);

impl sealed::Sealed for BrwLockStateNoPi {}

impl BrwLockState for BrwLockStateNoPi {
    #[inline(always)]
    fn new(state: u64) -> Self {
        Self { state: AtomicU64::new(state) }
    }

    #[inline(always)]
    fn state(&self) -> &AtomicU64 {
        &self.state
    }

    #[inline(always)]
    fn writer(&self) -> Option<NonNull<Thread>> {
        None
    }

    #[inline(always)]
    fn set_writer(&self, _writer: Option<NonNull<Thread>>) {}

    #[inline(always)]
    fn compare_exchange(
        &self,
        current_state: u64,
        new_state: u64,
        _new_writer: Option<NonNull<Thread>>,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64> {
        self.state.compare_exchange(current_state, new_state, success, failure)
    }
}

/// The wait queue of a [`BrwLock`]: an [`OwnedWaitQueue`] with priority inheritance, and a plain
/// [`WaitQueue`] without.
pub trait BrwLockWaitQueue: sealed::Sealed + ChainLocked + Send + Sync + Sized + 'static {
    /// In-place pinned initializer for the queue.
    fn init_queue() -> impl PinInit<Self, Infallible>;

    /// Returns `true` if no threads are currently blocked in the queue.
    fn queue_is_empty(queue: &ChainLockProof<'_, Self>) -> bool;
}

impl sealed::Sealed for OwnedWaitQueue {}

impl BrwLockWaitQueue for OwnedWaitQueue {
    #[inline(always)]
    fn init_queue() -> impl PinInit<Self, Infallible> {
        OwnedWaitQueue::init()
    }

    #[inline(always)]
    fn queue_is_empty(queue: &ChainLockProof<'_, Self>) -> bool {
        queue.is_empty()
    }
}

impl sealed::Sealed for WaitQueue {}

impl BrwLockWaitQueue for WaitQueue {
    #[inline(always)]
    fn init_queue() -> impl PinInit<Self, Infallible> {
        WaitQueue::init()
    }

    #[inline(always)]
    fn queue_is_empty(queue: &ChainLockProof<'_, Self>) -> bool {
        queue.is_empty()
    }
}

/// Selects whether a [`BrwLock`] respects priority inheritance ([`enable_pi::Yes`] or
/// [`enable_pi::No`]), along with the types and the parts of the blocking and waking slow paths
/// that differ between the two.
pub trait BrwLockEnablePi: sealed::Sealed + Sized + 'static {
    /// Whether priority inheritance is enabled.
    const ENABLED: bool;

    /// The state of the lock.
    type State: BrwLockState;

    /// The wait queue of the lock.
    type WaitQueue: BrwLockWaitQueue;

    /// Details of the locks obtained by [`Self::lock_for_block`], other than the current thread's
    /// lock, which are consumed by [`Self::block_locked`].
    type BlockOpLockDetails;

    /// Obtains the locks, in addition to `queue`'s, needed to block `current_thread` in `lock`'s
    /// wait queue. On success, returns the details of the locks obtained along with the guard of
    /// the current thread's lock. On failure, returns `None` after dropping every lock obtained.
    ///
    /// # Safety
    ///
    /// `queue` must be `lock`'s wait queue, the active transaction must not be finalized,
    /// `current_thread` must be the current, running thread, no lock other than `queue`'s may be
    /// held, and the caller must have registered as a waiter in `lock.state` with an `Acquire` RMW
    /// while holding `queue`'s lock. On success, the caller must finalize the transaction and pass
    /// the results to [`Self::block_locked`] without intervening mutations.
    unsafe fn lock_for_block<'a>(
        lock: &BrwLock<Self>,
        queue: &mut ChainLockProof<'_, Self::WaitQueue>,
        current_thread: &'a Thread,
        token: &'a ChainLockTransactionToken,
    ) -> Option<(Self::BlockOpLockDetails, OwnerGuard<'a, Thread>)>;

    /// Blocks the current thread in the wait queue held by `queue_guard`, releasing the queue's
    /// lock and the locks described by `lock_details`, then releases the current thread's lock
    /// once it wakes.
    ///
    /// # Safety
    ///
    /// `lock_details` and `current_thread_guard` must be the result of a successful
    /// [`Self::lock_for_block`] on `queue_guard`'s queue in the current, finalized transaction,
    /// with no intervening mutations. The current thread must be running, hold no spinlocks, and be
    /// in no wait queue, and the queue must stay live until `block_locked` returns.
    unsafe fn block_locked(
        queue_guard: OwnerGuard<'_, Self::WaitQueue>,
        current_thread_guard: OwnerGuard<'_, Thread>,
        lock_details: &Self::BlockOpLockDetails,
        reason: ResourceOwnership,
        token: &ChainLockTransactionToken,
    ) -> Result<(), Status>;

    /// The part of [`BrwLock::try_wake`] that runs once `queue` is known to have waiters.
    ///
    /// # Safety
    ///
    /// As for [`BrwLock::try_wake`].
    unsafe fn try_wake_locked(
        lock: &BrwLock<Self>,
        queue: &mut ChainLockProof<'_, Self::WaitQueue>,
        token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<ResourceOwnership>;
}

/// The [`BrwLockEnablePi`] selectors.
pub mod enable_pi {
    /// Priority inheritance is enabled.
    pub enum Yes {}
    /// Priority inheritance is disabled.
    pub enum No {}
}

/// A callback class used to select which threads we want to wake during a wake
/// operation. Generally, if the first waiter is a writer, we wake just the
/// writer and assign ownership to them. Otherwise, we wake as many readers as we
/// can and stop either when we run out, or when we encounter the first writer.
#[cfg(target_has_atomic = "128")]
#[derive(Default)]
struct WakeOpHooks {
    op: Operation,
}

/// The wake operation selected by [`WakeOpHooks`].
#[cfg(target_has_atomic = "128")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Operation {
    #[default]
    None,
    WakeWriter,
    WakeReaders,
}

#[cfg(target_has_atomic = "128")]
impl WakeOpHooks {
    fn op(&self) -> Operation {
        self.op
    }
}

#[cfg(target_has_atomic = "128")]
impl IWakeRequeueHook for WakeOpHooks {
    /// This hook is called during the locking phase of the wake operation, and
    /// allows us to determine which threads we want to wake based on their current
    /// state.  Returning true selects a thread for waking, and continues the
    /// enumeration of threads (if any).  Returning false will stop the
    /// enumeration.
    ///
    /// Note that we have not committed to waking any threads yet.  We first need
    /// to obtain all of the locks needed for the wake operation.  If we fail to
    /// obtain any of the locks, we are going to need to back off from the wake
    /// operation and try again after releasing our queue lock.
    ///
    /// # Safety
    ///
    /// As for [`IWakeRequeueHook::allow`].
    unsafe fn allow(&mut self, t: &Thread) -> bool {
        // SAFETY: Forwarded from the caller: `t` is currently blocked in a wait queue whose lock
        // is held by the active transaction.
        let state = unsafe { t.state_in_wait_queue() };

        // We have not considered any threads yet, so we don't know if we are going
        // to be waking a single writer, or a set of readers.  Figure this out now.
        if self.op == Operation::None {
            // If the first thread is blocked for writing, then we are waking a single
            // writer.  Record this and approve this thread.
            if state == ThreadStateKind::Blocked {
                self.op = Operation::WakeWriter;
                return true;
            }

            // If not writing then we must be blocked for reading
            debug_assert!(state == ThreadStateKind::BlockedReadLock);
            self.op = Operation::WakeReaders;
        }

        // If we are waking readers, and the next thread is a reader, accept it,
        // otherwise stop.
        if self.op == Operation::WakeReaders {
            return state == ThreadStateKind::BlockedReadLock;
        }

        // We must be waking a single writer.  Reject the next thread
        // and stop the enumeration.
        debug_assert!(self.op == Operation::WakeWriter);
        false
    }
}

#[cfg(target_has_atomic = "128")]
impl sealed::Sealed for enable_pi::Yes {}

// 16-byte atomics are required for the 16-byte compare and swap of the state and writer, which is
// why `enable_pi::Yes` is only implemented where they are available. We only care that
// compare_exchange be atomic; `BrwLockStatePi` never performs a 16-byte atomic load.
#[cfg(target_has_atomic = "128")]
impl BrwLockEnablePi for enable_pi::Yes {
    const ENABLED: bool = true;
    type State = BrwLockStatePi;
    type WaitQueue = OwnedWaitQueue;
    type BlockOpLockDetails = BAAOLockingDetails;

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::lock_for_block`].
    #[inline]
    unsafe fn lock_for_block<'a>(
        lock: &BrwLock<Self>,
        queue: &mut ChainLockProof<'_, OwnedWaitQueue>,
        current_thread: &'a Thread,
        token: &'a ChainLockTransactionToken,
    ) -> Option<(BAAOLockingDetails, OwnerGuard<'a, Thread>)> {
        let new_owner = lock.state.writer();
        // SAFETY: A recorded writer holds the lock for write. It cannot finish releasing the
        // lock, and exit, while we hold the queue's lock: we registered ourselves as a waiter
        // while holding the queue's lock, so a writer releasing after that observes the waiter
        // and must wake us through `release_wakeup`, which needs the queue's lock. A writer which
        // released before that cleared the recorded writer before publishing its release. Both
        // callers register as a waiter with an `Acquire` RMW (the conversion to a waiter in
        // `contended_read_acquire`, and the speculative waiter registration in
        // `contended_write_acquire`), which orders this load after the state that registration
        // observed, so the writer seen is the one whose bit it showed, or none if that writer has
        // since cleared it (or none at all if its bit was clear). `new_owner_ref` is not used
        // after the locks are obtained or dropped below.
        let new_owner_ref: Option<&Thread> = new_owner.map(|writer| unsafe { writer.as_ref() });
        // SAFETY: Forwarded from the caller: the transaction is not finalized, `current_thread`
        // is the current thread, and only `queue`'s lock is held. On success, the caller
        // finalizes the transaction and passes the results to `block_locked`.
        unsafe { queue.try_lock_for_baao_operation_locked(current_thread, new_owner_ref, token) }
    }

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::block_locked`].
    #[inline]
    unsafe fn block_locked(
        queue_guard: OwnerGuard<'_, OwnedWaitQueue>,
        mut current_thread_guard: OwnerGuard<'_, Thread>,
        lock_details: &BAAOLockingDetails,
        reason: ResourceOwnership,
        token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // Changes in ownership of node in a PI graph have the potential to affect
        // the running/runnable status of multiple threads at the same time.
        // Because of this, the OwnedWaitQueue class requires that we disable eager
        // rescheduling in order to optimize a situation where we might otherwise
        // send multiple (redundant) IPIs to the same CPUs during one PI graph
        // mutation.
        //
        // Note that this is an optimization, not a requirement.  What _is_ a
        // requirement is that we keep preemption disabled during the PI
        // propagation.  Currently, all of the invariants of OwnedWaitQueues and PI
        // graphs are protected by a single global "thread lock" which must be held
        // when a thread calls into the scheduler.  If a change to a PI graph would
        // cause the current scheduler to choose a different thread to run on that
        // CPU, however, the current thread will be preempted, and the ownership of
        // the thread lock will be transferred to the newly selected thread.  As the
        // new thread unwinds, it is going to drop the thread lock and return to
        // executing, leaving the first thread in the middle of what was supposed to
        // be an atomic operation.
        //
        // Because of this, it is critically important that local preemption be
        // disabled (at a minimum) when mutating a PI graph.  In the case that a
        // thread eventually blocks, the OWQ code will make sure that all invariants
        // will be restored before the thread finally blocks (and eventually wakes
        // and unwinds).
        let eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        // SAFETY: Forwarded from the caller: `current_thread_guard` holds the lock of the current
        // thread, which holds no spinlocks, `lock_details` came from the successful lock
        // acquisition with no intervening mutations, and the transaction is finalized with
        // exactly those locks held. The `BrwLock`, and so its queue, stays live while the current
        // thread is blocked in it, as it cannot be destroyed while it has waiters.
        let ret = unsafe {
            OwnedWaitQueue::block_and_assign_owner_locked(
                queue_guard,
                &mut current_thread_guard.proof(),
                &const { Deadline::infinite() },
                lock_details,
                reason,
                Interruptible::No,
                ForceInheritance::No,
                token,
                eager_resched_disabler.token(),
            )
        };
        current_thread_guard.release();
        ret
    }

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::try_wake_locked`].
    unsafe fn try_wake_locked(
        lock: &BrwLock<Self>,
        queue: &mut ChainLockProof<'_, OwnedWaitQueue>,
        token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<ResourceOwnership> {
        let mut hooks = WakeOpHooks::default();
        pin_init::stack_pin_init!(let unblock_list = ThreadUnblockList::init());
        // SAFETY: The transaction is not finalized and only `queue`'s lock is held (per the
        // caller), and `unblock_list` is empty. On success, the transaction is finalized and
        // `unblock_list` passed to `wake_threads_locked` below.
        let locked = unsafe {
            queue.lock_for_wake_operation_locked(u32::MAX, &mut hooks, unblock_list.as_mut(), token)
        };

        // We get back a list only if we succeeded in locking all of the threads we
        // want to wake. Otherwise, we need to back off and try again.
        if !locked {
            return None;
        }

        // Great, we have locked all of the threads we need to lock.
        ChainLockTransaction::finalize(token);

        // Fix up our state, granting the lock to the set of readers, or single
        // writer, we are going to wake  before we actually perform the wake
        // operation.
        debug_assert!(hooks.op() != Operation::None);
        let wake_opt = if hooks.op() == Operation::WakeWriter {
            WakeOption::AssignOwner
        } else {
            WakeOption::None
        };

        // TODO(johngro): optimize this; we should not be doing O(n) counts here, especially since
        // we already did one while obtaining the locks.
        let to_wake = unblock_list.iter().count() as u32;
        if hooks.op() == Operation::WakeWriter {
            debug_assert!(to_wake == 1);
            let new_owner =
                unblock_list.front_ptr().expect("waking a writer with no thread to wake");

            lock.state.set_writer(NonNull::new(new_owner.as_raw()));
            // Ordering: `AcqRel`. The `Acquire` half synchronizes with the `Release` of the
            // previous holders, and the woken threads synchronize with this thread through the
            // thread locks released as they are unblocked, so their critical sections
            // happen-after those of the previous holders.
            lock.state
                .state()
                .fetch_add(BRW_LOCK_WRITER.wrapping_sub(BRW_LOCK_WAITER), Ordering::AcqRel);
        } else {
            debug_assert!(hooks.op() == Operation::WakeReaders);
            debug_assert!(to_wake >= 1);
            // Ordering: `AcqRel`, as for waking a writer above.
            lock.state.state().fetch_add(
                BRW_LOCK_READER.wrapping_sub(BRW_LOCK_WAITER).wrapping_mul(u64::from(to_wake)),
                Ordering::AcqRel,
            );
        }

        if crate::ktrace_rs::category_enabled!("kernel:contention") {
            for thread in unblock_list.iter() {
                // SAFETY: Every thread in `unblock_list` is locked by the active transaction, so
                // it cannot be woken, and stays live.
                lock_trace_flow_end!("contend_rwlock", unsafe { thread.lock_flow_id() });
            }
        }

        // Now actually do the wake.
        //
        // SAFETY: `unblock_list` was populated by the successful lock-for-wake operation on
        // `queue` above with no intervening modifications, no proof or guard for its threads is
        // live, the transaction is finalized, and `WakeOption::AssignOwner` is only used when
        // waking a single writer.
        let results = unsafe {
            queue.wake_threads_locked(
                unblock_list.as_mut(),
                &mut hooks,
                wake_opt,
                ForceInheritance::No,
                token,
                preempt_disabled_token,
            )
        };
        if hooks.op() == Operation::WakeWriter {
            debug_assert!(results.woken == 1);
            debug_assert!(
                ((results.still_waiting == 0) && results.owner.is_none())
                    || results.owner.is_some()
            );
            Some(ResourceOwnership::Normal)
        } else {
            debug_assert!(hooks.op() == Operation::WakeReaders);
            debug_assert!(results.woken >= 1);
            debug_assert!(results.owner.is_none());
            Some(ResourceOwnership::Reader)
        }
    }
}

impl sealed::Sealed for enable_pi::No {}

impl BrwLockEnablePi for enable_pi::No {
    const ENABLED: bool = false;
    type State = BrwLockStateNoPi;
    type WaitQueue = WaitQueue;
    type BlockOpLockDetails = ();

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::lock_for_block`].
    #[inline]
    unsafe fn lock_for_block<'a>(
        _lock: &BrwLock<Self>,
        _queue: &mut ChainLockProof<'_, WaitQueue>,
        current_thread: &'a Thread,
        token: &'a ChainLockTransactionToken,
    ) -> Option<((), OwnerGuard<'a, Thread>)> {
        OwnerGuard::acquire_or_backoff(current_thread, token)
            .map(|current_thread_guard| ((), current_thread_guard))
    }

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::block_locked`].
    #[inline]
    unsafe fn block_locked(
        queue_guard: OwnerGuard<'_, WaitQueue>,
        mut current_thread_guard: OwnerGuard<'_, Thread>,
        _lock_details: &(),
        reason: ResourceOwnership,
        token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        let _preempt_disable = AnnotatedAutoPreemptDisabler::new();
        // SAFETY: Forwarded from the caller: `current_thread_guard` holds the lock of the current,
        // running thread, which holds no spinlocks and is in no wait queue, and the transaction is
        // finalized with the queue's lock and the current thread's lock as the only held locks.
        // The `BrwLock`, and so its queue, stays live while the current thread is blocked in it,
        // as it cannot be destroyed while it has waiters, and the deadline is infinite.
        let ret = unsafe {
            WaitQueue::block_etc(
                queue_guard,
                &mut current_thread_guard.proof(),
                &const { Deadline::infinite() },
                0,
                reason,
                Interruptible::No,
                token,
            )
        };
        current_thread_guard.release();
        ret
    }

    /// # Safety
    ///
    /// As for [`BrwLockEnablePi::try_wake_locked`].
    unsafe fn try_wake_locked(
        lock: &BrwLock<Self>,
        queue: &mut ChainLockProof<'_, WaitQueue>,
        token: &ChainLockTransactionToken,
        _preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<ResourceOwnership> {
        // Try to lock the set of threads we need to wake.  Either the next writer,
        // or the next contiguous span of readers.
        pin_init::stack_pin_init!(let unblock_list = ThreadUnblockList::init());
        // SAFETY: The transaction is not finalized (per the caller) and `unblock_list` is empty.
        // On success, the transaction is finalized and `unblock_list` passed to
        // `scheduler::unblock_list` below.
        let lock_result = unsafe { BrwLockOps::lock_for_wake(queue, unblock_list.as_mut(), token) };

        // If we failed to lock, backoff and try again.
        let count = lock_result?;

        // Great, we have locked all of the threads we need to lock.  Finalize our
        // transaction, then update our bookkeeping and send the threads off to the
        // scheduler to finish unblocking.
        ChainLockTransaction::finalize(token);
        debug_assert!(count > 0);
        debug_assert!(!unblock_list.is_empty());

        if crate::ktrace_rs::category_enabled!("kernel:contention") {
            for thread in unblock_list.iter() {
                // SAFETY: Every thread in `unblock_list` is locked by the active transaction, so
                // it cannot be woken, and stays live.
                lock_trace_flow_end!("contend_rwlock", unsafe { thread.lock_flow_id() });
            }
        }

        let first_wake =
            unblock_list.front_ptr().expect("BrwLockOps::lock_for_wake locked no threads");
        // SAFETY: `first_wake` is locked by the active transaction, so it stays live.
        let first_wake: &Thread = unsafe { first_wake.as_ref() };
        debug_assert!(first_wake.lock().is_held(token));
        let first_wake_state = {
            // SAFETY: `first_wake`'s lock was acquired by `lock_for_wake` in this transaction
            // (checked above in debug builds), and no other proof for it is live.
            let first_wake_proof = unsafe { ChainLockProof::assume_held_unchecked(first_wake) };
            first_wake_proof.state()
        };

        if first_wake_state == ThreadStateKind::BlockedReadLock {
            // Ordering: `AcqRel`. The `Acquire` half synchronizes with the `Release` of the
            // previous holders, and the woken threads synchronize with this thread through the
            // thread locks released as they are unblocked, so their critical sections
            // happen-after those of the previous holders.
            lock.state.state().fetch_add(
                BRW_LOCK_READER.wrapping_sub(BRW_LOCK_WAITER).wrapping_mul(u64::from(count)),
                Ordering::AcqRel,
            );
            // SAFETY: Every thread in `unblock_list` was locked and dequeued by `lock_for_wake`,
            // the active transaction is finalized, and no proof or guard for any of them is live
            // (`first_wake_proof` above is scoped).
            unsafe { scheduler::unblock_list(unblock_list.as_mut(), token) };
            Some(ResourceOwnership::Reader)
        } else {
            debug_assert!(first_wake_state == ThreadStateKind::Blocked);
            debug_assert!(count == 1);
            // Ordering: `AcqRel`, as for waking readers above.
            lock.state
                .state()
                .fetch_add(BRW_LOCK_WRITER.wrapping_sub(BRW_LOCK_WAITER), Ordering::AcqRel);
            // SAFETY: As above.
            unsafe { scheduler::unblock_list(unblock_list.as_mut(), token) };
            Some(ResourceOwnership::Normal)
        }
    }
}

/// Blocking (i.e. non spinning) reader-writer lock. Readers and writers are
/// ordered by priority (i.e. their wait_queue release order) and otherwise
/// readers and writers are treated equally and will fall back to FIFO ordering
/// at some priority.
/// The lock optionally respects priority inheritance. Not supporting PI is more
/// efficient as the current active writer does not have to be tracked. Enabling PI
/// creates an additional restriction that readers must not take any additional
/// locks or otherwise block whilst holding the read lock.
///
/// Use the [`BrwLockPi`] and [`BrwLockNoPi`] aliases rather than naming `P` directly.
///
/// Every field of `BrwLock` is interior-mutable: `name_storage` and `canary` are `UnsafeCell`s,
/// `state` is atomics, and the wait queues document that every one of their bytes is
/// interior-mutable (see `OwnedWaitQueue` and `WaitQueueBase`). The only `Freeze` bytes are the
/// alignment padding between `canary` and `state`, which is never accessed. Unlike [`Mutex`],
/// `BrwLock` does not synchronize teardown against an in-flight `release_wakeup`, so callers must
/// keep the `BrwLock` live until [`BrwLock::read_release`] and [`BrwLock::write_release`] return.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct BrwLock<P: BrwLockEnablePi> {
    /// Storage for the lock name and encoded lock ID used by lock tracing; zero-sized unless lock
    /// name tracing or scheduler lock spin tracing is enabled. Only mutated during
    /// initialization.
    name_storage: UnsafeCell<RwLockNameStorage>,
    /// Structure guard (`'RWLK'` while live, zeroed on drop).
    canary: UnsafeCell<fbl::Canary<MAGIC>>,
    state: P::State,
    #[pin]
    wait: P::WaitQueue,
}

// SAFETY: `BrwLock` has no thread-local ownership invariants and is `!Unpin` (its wait queue is),
// so ownership of a pinned `BrwLock` can be transferred across threads.
unsafe impl<P: BrwLockEnablePi> Send for BrwLock<P> {}
// SAFETY: Shared accesses through `&BrwLock` operate on the `state` atomics and the wait queue
// (guarded by its lock), or perform volatile reads of `canary`. `name_storage` is only mutated
// during initialization, before the lock is shared, and `canary` only during teardown.
unsafe impl<P: BrwLockEnablePi> Sync for BrwLock<P> {}

/// A [`BrwLock`] which respects priority inheritance.
#[cfg(not(target_arch = "riscv64"))]
pub type BrwLockPi = BrwLock<enable_pi::Yes>;
/// A [`BrwLock`] which respects priority inheritance, except on RISC-V, where it is a
/// [`BrwLockNoPi`].
// TODO(https://fxbug.dev/42075694): Optimally implement BrwLock
// Workaround for lack of doubleword CAS in the brwlock implementation on RISC-V that is only
// used in the PI path. Simply switch to using the no PI version for this architecture.
#[cfg(target_arch = "riscv64")]
pub type BrwLockPi = BrwLockNoPi;

/// A [`BrwLock`] which does not respect priority inheritance.
pub type BrwLockNoPi = BrwLock<enable_pi::No>;

// `BrwLockPi` is `BrwLock<enable_pi::Yes>` on every architecture but RISC-V, so every other
// architecture must provide the 16-byte atomics which `enable_pi::Yes` requires.
static_assert!(cfg!(target_arch = "riscv64") || cfg!(target_has_atomic = "128"));

impl<P: BrwLockEnablePi> BrwLock<P> {
    /// Creates an unlocked `BrwLock` with no lock class name.
    pub fn init() -> impl PinInit<Self, Infallible> {
        Self::init_with_name_storage(RwLockNameStorage::<RWLOCK_NAME_TRACING_ENABLED>::new())
    }

    /// Creates an unlocked `BrwLock` with the given lock name storage.
    fn init_with_name_storage(name_storage: RwLockNameStorage) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            name_storage: UnsafeCell::new(name_storage),
            canary: UnsafeCell::new(fbl::Canary::new()),
            state: P::State::new(BRW_LOCK_UNLOCKED),
            wait <- P::WaitQueue::init_queue(),
        })
    }

    /// Acquires the lock for read, blocking while it is held for write.
    ///
    /// # Deadlocks and Priority Inheritance
    ///
    /// The caller must not already hold this lock for write (doing so deadlocks). When priority
    /// inheritance is enabled, preemption remains disabled until [`BrwLock::read_release`], so the
    /// caller should not block or acquire any blocking lock while holding the read lock (doing so
    /// degrades priority inheritance).
    ///
    /// # Panics
    ///
    /// Panics if contention exhausts adaptive spinning (entering the wait queue) while blocking is
    /// disallowed, any spinlocks are held, or a `ChainLockTransaction` is active on the current CPU
    /// (and in debug builds on the fast path if blocking is disallowed, or on any contended call if
    /// blocking is disallowed or any spinlocks are held).
    ///
    /// # Safety
    ///
    /// Must be called from kernel thread context, and the acquired read lock must be released on
    /// the same thread with [`BrwLock::read_release`].
    #[inline(always)]
    pub unsafe fn read_acquire(&self) {
        debug_assert!(!blocking_disallowed());
        self.canary().assert();
        if P::ENABLED {
            // As readers are not recorded and do not receive boosting from blocking
            // writers they must not block or otherwise cease to run, otherwise
            // our PI will be violated.
            thread::preempt_disable();
        }
        // Attempt the optimistic grab
        //
        // Ordering: `Acquire`, so that the critical section happens-after the lock is acquired.
        let prev = self.state.state().fetch_add(BRW_LOCK_READER, Ordering::Acquire);
        // See if there are only readers
        if (prev & BRW_LOCK_READER_MASK) != prev {
            core::hint::cold_path();
            // SAFETY: Forwarded from the caller, and we hold an optimistic reader count.
            unsafe { self.contended_read_acquire() };
        }
    }

    /// Acquires the lock for write, blocking while it is held.
    ///
    /// # Deadlocks
    ///
    /// The caller must not already hold this lock for read or write (doing so deadlocks).
    ///
    /// # Panics
    ///
    /// Panics if contention exhausts adaptive spinning (entering the wait queue) while blocking is
    /// disallowed, any spinlocks are held, or a `ChainLockTransaction` is active on the current CPU
    /// (and in debug builds on the fast path if blocking is disallowed, or on any contended call if
    /// blocking is disallowed or any spinlocks are held).
    ///
    /// # Safety
    ///
    /// Must be called from kernel thread context, and the acquired write lock must be released on
    /// the same thread with [`BrwLock::write_release`].
    #[inline(always)]
    pub unsafe fn write_acquire(&self) {
        debug_assert!(!blocking_disallowed());
        self.canary().assert();
        // When acquiring the write lock we require there be no-one else using
        // the lock.
        let current_thread = if P::ENABLED { thread::current_get() } else { ptr::null_mut() };
        if self.atomic_write_acquire(current_thread).is_err() {
            core::hint::cold_path();
            // SAFETY: Forwarded from the caller.
            unsafe { self.contended_write_acquire() };
        }
    }

    /// Releases the lock held for write by the current thread.
    ///
    /// # Panics
    ///
    /// Panics if waking contended waiters when blocking is disallowed, any spinlocks are held, or
    /// a `ChainLockTransaction` is active on the current CPU, or when `LK_DEBUGLEVEL > 0` and
    /// priority inheritance is enabled if the current thread is not the recorded writer.
    ///
    /// # Safety
    ///
    /// The current thread must hold this lock for write, acquired by [`BrwLock::write_acquire`],
    /// and `self` must remain live until this call returns.
    #[inline(always)]
    pub unsafe fn write_release(&self) {
        self.canary().assert();

        if LK_DEBUGLEVEL_GT_0 && P::ENABLED {
            let holder = self.state.writer();
            let ct = thread::current_get();
            if NonNull::new(ct) != holder {
                core::hint::cold_path();
                // SAFETY: The caller holds this lock on the current thread, so `ct` is the live
                // current thread.
                unsafe { self.panic_write_release_not_owner(ct, holder) };
            }
        }

        // For correct PI handling we need to ensure that up until a higher priority
        // thread can acquire the lock we will correctly be considered the owner.
        // Other threads are able to acquire the lock *after* we call release_wakeup,
        // prior to that we could be racing with a higher priority acquirer and it
        // could be our responsibility to wake them up, and so up until release_wakeup
        // is called they must be able to observe us as the owner.
        //
        // If we hold off on changing writer till after release_wakeup we will then be
        // racing with others who may be acquiring, or be granted the write lock in
        // release_wakeup, and so we would have to CAS writer to not clobber the new
        // holder. CAS is much more expensive than just a 'store', so to avoid that
        // we instead disable preemption. Disabling preemption effectively gives us the
        // highest priority, and so it is fine if acquirers observe writer to be null
        // and 'fail' to treat us as the owner.
        if P::ENABLED {
            thread::preempt_disable();

            self.state.set_writer(None);
        }
        // Ordering: `Release`, so that the critical section happens-before the next acquisition.
        let prev = self.state.state().fetch_sub(BRW_LOCK_WRITER, Ordering::Release);

        if state_has_waiters(prev) {
            core::hint::cold_path();
            // There are waiters, we need to wake them up
            self.contended_write_release();
        }

        if P::ENABLED {
            // SAFETY: Pairs with the `preempt_disable` above, and no `PreemptDisabledToken` was
            // minted from that disable scope.
            unsafe { thread::preempt_reenable() };
        }
    }

    /// Releases the lock held for read by the current thread.
    ///
    /// # Panics
    ///
    /// Panics if waking contended waiters when blocking is disallowed, any spinlocks are held, or
    /// a `ChainLockTransaction` is active on the current CPU.
    ///
    /// # Safety
    ///
    /// The current thread must hold this lock for read, acquired by [`BrwLock::read_acquire`],
    /// have no live [`PreemptDisabledToken`] whose validity relies on [`BrwLock::read_acquire`]
    /// having disabled preemption, and `self` must remain live until this call returns.
    #[inline(always)]
    pub unsafe fn read_release(&self) {
        self.canary().assert();
        // Ordering: `Release`, so that the critical section happens-before the next acquisition.
        let prev = self.state.state().fetch_sub(BRW_LOCK_READER, Ordering::Release);
        if (prev & BRW_LOCK_READER_MASK) == 1 && (prev & BRW_LOCK_WAITER_MASK) != 0 {
            core::hint::cold_path();
            // there are no readers but still some waiters, becomes our job to wake them up
            self.contended_read_release();
        }
        if P::ENABLED {
            // SAFETY: Pairs with the `preempt_disable` in `read_acquire`, and no
            // `PreemptDisabledToken` is live.
            unsafe { thread::preempt_reenable() };
        }
    }

    /// Traces a contended write release and wakes any waiting threads.
    ///
    /// Outlined from [`BrwLock::write_release`] so the `lock_trace_duration!` scope's stack slot
    /// is not allocated on the uncontended fast path.
    #[cold]
    #[inline(never)]
    fn contended_write_release(&self) {
        lock_trace_duration!("ContendedWriteRelease", "name" => self.class_name_ref());
        self.release_wakeup();
    }

    /// Traces a contended read release and wakes any waiting threads.
    ///
    /// Outlined from [`BrwLock::read_release`] so the `lock_trace_duration!` scope's stack slot is
    /// not allocated in every caller into which `read_release` is inlined.
    #[cold]
    #[inline(never)]
    fn contended_read_release(&self) {
        lock_trace_duration!("ContendedReadRelease", "name" => self.class_name_ref());
        self.release_wakeup();
    }
    // Note: read_upgrade() is intentionally omitted because it is unused across the codebase
    // outside of tests and may be reconsidered when a concrete use case arises.

    /// Returns the encoded lock ID used by lock tracing.
    #[inline(always)]
    pub fn encoded_lock_id(&self) -> EncodedLockId {
        self.name_storage().encoded_lock_id()
    }

    /// Returns the string reference for this lock's class name.
    #[inline(always)]
    pub fn class_name_ref(&self) -> IdStringRef {
        self.name_storage().class_name_ref()
    }

    /// Returns the lock name storage.
    #[inline(always)]
    fn name_storage(&self) -> &RwLockNameStorage {
        // SAFETY: `name_storage` is only mutated during initialization, before the lock is used.
        unsafe { &*self.name_storage.get() }
    }

    /// Returns the structure guard.
    #[inline(always)]
    fn canary(&self) -> &fbl::Canary<MAGIC> {
        // SAFETY: `canary` is only mutated during teardown, after the lock is no longer used.
        unsafe { &*self.canary.get() }
    }

    /// Blocks the current thread in this lock's wait queue, for write if `write`, and otherwise
    /// for read.
    ///
    /// # Safety
    ///
    /// `queue_guard` must hold this lock's queue's lock, and `lock_details` and
    /// `current_thread_guard` must be the result of a successful `P::lock_for_block` on it in the
    /// current, finalized transaction, with no intervening mutations. The current thread must be
    /// running, hold no spinlocks, and be in no wait queue, and `self` must stay live until `block`
    /// returns.
    unsafe fn block(
        &self,
        queue_guard: OwnerGuard<'_, P::WaitQueue>,
        current_thread_guard: OwnerGuard<'_, Thread>,
        lock_details: &P::BlockOpLockDetails,
        write: bool,
        token: &ChainLockTransactionToken,
    ) {
        let reason = if write { ResourceOwnership::Normal } else { ResourceOwnership::Reader };

        debug_assert!(ptr::eq(current_thread_guard.owner(), thread::current_get()));
        // SAFETY: Blocking a thread runs after multi-threading has been initialized, and the
        // current thread is live while it runs. Using `ThreadPtr::current()` preserves raw mutable
        // provenance over `Thread` rather than re-tagging from `&Thread`.
        let flow_id = unsafe { ThreadPtr::current().take_next_lock_flow_id() };
        lock_trace_flow_begin!("contend_rwlock", flow_id);

        // SAFETY: Forwarded from the caller.
        let ret = unsafe {
            P::block_locked(queue_guard, current_thread_guard, lock_details, reason, token)
        };

        if let Err(status) = ret {
            core::hint::cold_path();
            Self::panic_block_failed(status.into_raw(), self, thread::current_get(), get_frame!());
        }
    }

    /// Panics because blocking the current thread `current_thread` on the lock `this` returned
    /// the error `status`.
    ///
    /// This and the other `panic_*` functions are kept out of line and cold so that the formatting
    /// machinery does not inflate the stack frames of the paths that call them.
    #[cold]
    #[inline(never)]
    fn panic_block_failed(
        status: zx_status_t,
        this: *const Self,
        current_thread: *const Thread,
        frame: *const c_void,
    ) -> ! {
        panic!(
            "BrwLock<{}>::Block: Block returned with error {} lock {:p}, thr {:p}, sp {:p}",
            i32::from(P::ENABLED),
            status,
            this,
            current_thread,
            frame,
        );
    }

    /// Panics because the current thread `ct` tried to release this lock, which it does not hold
    /// for write (`observed_holder` was observed in `self.state.writer()` without synchronization
    /// and is never dereferenced directly).
    ///
    /// # Safety
    ///
    /// `ct` must point to the live current thread.
    #[cold]
    #[inline(never)]
    unsafe fn panic_write_release_not_owner(
        &self,
        ct: *mut Thread,
        observed_holder: Option<NonNull<Thread>>,
    ) -> ! {
        // SAFETY: The caller guarantees `ct` points to the live current thread.
        let ct_name = unsafe { Thread::name_copy_ptr(ct) };

        // Synchronize reading any live writer's name under `self.wait`'s lock + a temporary
        // `Acquire` waiter registration so a racing writer on another CPU cannot finish
        // `write_release` and exit while its name is copied (and avoid starting a nested
        // transaction if the caller erroneously held one).
        let mut holder = observed_holder;
        let mut holder_name = None;
        if !(ints_disabled() && !PerCpu::get_current().active_cl_transaction().is_null()) {
            ChainLockTransaction::until_done(
                PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
                clt_tag!("BrwLock<PI>::PanicWriteReleaseNotOwner"),
                |token| {
                    let queue_guard = OwnerGuard::new(&self.wait, token);
                    ChainLockTransaction::finalize(token);
                    let prev = self.state.state().fetch_add(BRW_LOCK_WAITER, Ordering::Acquire);
                    if state_has_writer(prev)
                        && let Some(live_holder) = self.state.writer()
                    {
                        holder = Some(live_holder);
                        // SAFETY: `state_has_writer(prev)` observed an active writer while holding
                        // `self.wait`'s lock with `BRW_LOCK_WAITER` set, so `live_holder` cannot
                        // complete `release_wakeup` or exit until `queue_guard` is released below.
                        holder_name = Some(unsafe { Thread::name_copy_ptr(live_holder.as_ptr()) });
                    }
                    self.state.state().fetch_sub(BRW_LOCK_WAITER, Ordering::Relaxed);
                    queue_guard.release();
                    TransactionResult::DONE
                },
            );
        }

        let holder_name_str = match (&holder_name, holder) {
            (Some(name), _) => name.as_str(),
            (None, Some(_)) => "<unknown>",
            (None, None) => "none",
        };
        let holder_ptr = holder.map_or(ptr::null_mut(), NonNull::as_ptr);
        panic!(
            "BrwLock<PI>::WriteRelease: thread {:p} ({}) tried to release brwlock {:p} it doesn't \
             own. Ownedby {:p} ({})",
            ct,
            ct_name.as_str(),
            self,
            holder_ptr,
            holder_name_str,
        );
    }

    /// `try_wake` requires that there be an active `ChainLockTransaction` in progress,
    /// and will finalize that transaction if (and only if) the wake operation
    /// succeeds.
    ///
    /// # Safety
    ///
    /// `queue` must be this lock's wait queue, the active transaction must not be finalized, no
    /// lock other than `queue`'s may be held, and no `ChainLockProof` or `OwnerGuard` for any
    /// waiter thread or downstream PI node of `queue` may be live.
    unsafe fn try_wake(
        &self,
        queue: &mut ChainLockProof<'_, P::WaitQueue>,
        token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<ResourceOwnership> {
        // If there is no one to wake by the time we make it into the queue's lock, we
        // should fail the operation, signaling to the caller that they should retry.
        if P::WaitQueue::queue_is_empty(queue) {
            return None;
        }

        // SAFETY: Forwarded from the caller.
        unsafe { P::try_wake_locked(self, queue, token, preempt_disabled_token) }
    }

    /// Attempts to acquire the locks needed to block in `self.wait` and, on success, finalizes
    /// `token` and blocks the current thread (for write if `write`, and otherwise for read). On
    /// lock conflict, releases `queue_guard` (after removing the caller's speculative waiter count
    /// if `write`) and returns `false`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `queue_guard` holds `self.wait`'s lock, which is the only chain lock currently held, and
    ///   `token`'s transaction is not yet finalized.
    /// - The caller registered as a waiter in `self.state` with an `Acquire` RMW while holding
    ///   `queue_guard` (or `self.state.writer()` was updated on the current thread while holding
    ///   `queue_guard` with the caller's waiter count still present).
    /// - `current_thread` is the live, currently running thread, holds no spinlocks, and is in no
    ///   wait queue, and `self` remains live until `try_block` returns.
    #[inline(always)]
    unsafe fn try_block(
        &self,
        mut queue_guard: OwnerGuard<'_, P::WaitQueue>,
        current_thread: &Thread,
        write: bool,
        token: &ChainLockTransactionToken,
    ) -> bool {
        // SAFETY: Forwarded from the caller: `queue_guard` holds `self.wait`'s lock (the only lock
        // held) with a synchronized waiter registration in `self.state`, `token` is not finalized,
        // and `current_thread` is the live current thread.
        let maybe_lock_details =
            unsafe { P::lock_for_block(self, &mut queue_guard.proof(), current_thread, token) };

        if let Some((lock_details, current_thread_guard)) = maybe_lock_details {
            ChainLockTransaction::finalize(token);
            // SAFETY: `lock_details` and `current_thread_guard` were just returned by
            // `P::lock_for_block` on `queue_guard`, `token` is finalized with no intervening
            // mutations, and the caller upholds the thread, spinlock, wait-queue, and `self`
            // liveness requirements.
            unsafe { self.block(queue_guard, current_thread_guard, &lock_details, write, token) };
            true
        } else {
            if write {
                // Remove our speculative waiter count, then back off and try again.
                //
                // Ordering: `Relaxed`; this only undoes our own speculative waiter count, and the
                // queue's lock orders it against the other waiters.
                self.state.state().fetch_sub(BRW_LOCK_WAITER, Ordering::Relaxed);
            }
            // Back off and try again.
            queue_guard.release();
            false
        }
    }

    /// Acquires the read lock after the optimistic grab in [`BrwLock::read_acquire`] observed a
    /// writer or waiters, holding the optimistic reader count it added.
    ///
    /// # Safety
    ///
    /// As for [`BrwLock::read_acquire`], with an optimistic `BRW_LOCK_READER` count held in
    /// `self.state`.
    #[inline(never)]
    unsafe fn contended_read_acquire(&self) {
        debug_assert!(!blocking_disallowed() && num_spinlocks_held() == 0);
        lock_trace_duration!("ContendedReadAcquire", "name" => self.class_name_ref());

        // Remember the last call to current_ticks.
        let mut now_ticks = timer_current_mono_ticks();
        // SAFETY: We run in thread context, where the current thread is live while it runs.
        let current_thread: &Thread = unsafe { &*thread::current_get() };
        // SAFETY: `current_thread` is the current thread.
        let _timer = unsafe { ContentionTimer::new(current_thread, now_ticks) };

        let spin_until_ticks = spin_until_ticks(now_ticks);

        loop {
            // Ordering: `Acquire`, so that the critical section happens-after the lock is
            // acquired if no writer is observed.
            let state = self.state.state().load(Ordering::Acquire);

            // If there are any waiters, implying another thread exhausted its spin phase on the
            // same lock, break out of the spin phase early.
            if state_has_waiters(state) {
                break;
            }

            // If there are only readers now, return holding the lock for read, leaving the
            // optimistic reader count in place.
            if !state_has_writer(state) {
                return;
            }

            // Give the arch a chance to relax the CPU.
            libarch::r#yield();
            now_ticks = timer_current_mono_ticks();
            if now_ticks >= spin_until_ticks {
                break;
            }
        }

        assert!(!blocking_disallowed() && num_spinlocks_held() == 0);

        const READER_TO_WAITER: u64 = BRW_LOCK_WAITER.wrapping_sub(BRW_LOCK_READER);
        const WAITER_TO_READER: u64 = BRW_LOCK_READER.wrapping_sub(BRW_LOCK_WAITER);

        // Enter our wait queue's lock and figure out what to do next.  We don't really know what
        // we need to do yet, so we need to be prepared for needing to back off and try again.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables interrupts and eager
            // rescheduling for the duration of this transaction, so it cannot be preempted.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            let mut queue_guard = OwnerGuard::new(&self.wait, token);

            // Before blocking, remove our optimistic reader from the count,
            // and put a waiter on there instead.
            //
            // Ordering: This must be `Acquire`. If `prev` shows no writer after our fast path saw
            // one, this RMW read from the writer's `Release` fetch_sub in `write_release` (or a
            // later RMW in its release sequence), and the `!state_has_waiters(prev)` path below
            // keeps the lock for read without any other `Acquire` operation, so this is what
            // synchronizes-with the writer, as the `Acquire` load in the spin loop and the
            // `Acquire` conversion after `try_wake` do on the other paths. It also orders the
            // `Relaxed` load of the writer in `lock_for_block` after the state observed here (and
            // synchronizes with any prior `write_release` when `prev` has no writer), so the PI
            // owner we see is either the writer whose bit `prev` showed or `None`, and never an
            // older (possibly exited) writer.
            let prev = self.state.state().fetch_add(READER_TO_WAITER, Ordering::Acquire);

            if state_has_writer(prev) {
                // Try to grab all of our needed locks and block.  If we succeed, return,
                // otherwise loop around and try again.
                //
                // SAFETY: `queue_guard` holds `self.wait`'s lock (the only chain lock held),
                // `token` is not finalized, we registered as a waiter with `Acquire` above while
                // holding `queue_guard`, `current_thread` is the live running thread with no
                // spinlocks or wait queue membership, and `self` stays live until `try_block`
                // returns.
                if unsafe { self.try_block(queue_guard, current_thread, false, token) } {
                    return TransactionResult::DONE;
                }
                // Make sure to convert our waiter back to an optimistic reader
                //
                // Ordering: `Relaxed`; this only undoes our own conversion to a waiter, and the
                // lock is not acquired on this path (the retry observes the state again).
                self.state.state().fetch_add(WAITER_TO_READER, Ordering::Relaxed);
                return TransactionResult::BACKOFF;
            }

            // If we raced and there is in fact no one waiting then we can switch to
            // having the lock
            if !state_has_waiters(prev) {
                // Ordering: `Relaxed`; the `Acquire` conversion to a waiter above already
                // synchronized-with the previous writer's `Release` in `write_release`.
                self.state.state().fetch_add(WAITER_TO_READER, Ordering::Relaxed);
                queue_guard.release();
                return TransactionResult::DONE;
            }

            // If there are no current readers then we need to wake somebody up
            if state_reader_count(prev) == 1 {
                // SAFETY: `queue_guard` holds this lock's queue's lock, the only lock held, and
                // the transaction is not finalized.
                let maybe_ownership = unsafe {
                    self.try_wake(&mut queue_guard.proof(), token, &preempt_disabled_token)
                };
                let woke_someone = maybe_ownership.is_some();

                // Convert our waiter count back to a reader count.  If we woke a writer instead of
                // a reader, then this is an "optimistic" count, and we need to drop our locks
                // before looping around to try again.  Otherwise, if we woke readers, then we are
                // now a member of the reader pool and can return.
                if woke_someone && maybe_ownership == Some(ResourceOwnership::Reader) {
                    // Ordering: `Acquire`, so that our critical section happens-after the readers
                    // were granted the lock.
                    self.state.state().fetch_add(WAITER_TO_READER, Ordering::Acquire);
                    queue_guard.release();
                    return TransactionResult::DONE;
                }

                // Need to back off and try again.  If we successfully woke someone, then
                // our transaction was finalized and we need restart it before going
                // again.
                //
                // Ordering: `Relaxed`; this only undoes our own conversion to a waiter, and the
                // lock is not acquired on this path (the retry observes the state again).
                self.state.state().fetch_add(WAITER_TO_READER, Ordering::Relaxed);
                queue_guard.release();
                return if woke_someone {
                    TransactionResult::RESTART
                } else {
                    TransactionResult::BACKOFF
                };
            }

            // So, at this point, we know that when we converted our optimistic reader count to a
            // waiter, the state which was there indicated:
            //
            // ++ There were no writers.
            // ++ There were waiters (or at least threads trying to block).
            // ++ There was at least one other thread trying to become a reader (their count is
            //    optimistic).
            //
            // Try to join the blocking pool.  Eventually the final optimistic reader should
            // attempt to wake the threads who have actually managed to block, waking either a
            // writer or a set of readers as needed.

            // SAFETY: `queue_guard` holds `self.wait`'s lock (the only chain lock held), `token`
            // is not finalized, we registered as a waiter with `Acquire` above while holding
            // `queue_guard`, `current_thread` is the live running thread with no spinlocks or wait
            // queue membership, and `self` stays live until `try_block` returns.
            if unsafe { self.try_block(queue_guard, current_thread, false, token) } {
                return TransactionResult::DONE;
            }

            // We failed to block, restore our optimistic reader count, drop our locks, and try
            // again.
            //
            // Ordering: `Relaxed`; this only undoes our own conversion to a waiter, and the lock
            // is not acquired on this path (the retry observes the state again).
            self.state.state().fetch_add(WAITER_TO_READER, Ordering::Relaxed);
            TransactionResult::BACKOFF
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("BrwLock<PI>::ContendedReadAcquire"),
            do_transaction,
        );
    }

    /// Acquires the write lock after the fast-path attempt in [`BrwLock::write_acquire`] failed.
    ///
    /// # Safety
    ///
    /// As for [`BrwLock::write_acquire`].
    #[inline(never)]
    unsafe fn contended_write_acquire(&self) {
        debug_assert!(!blocking_disallowed() && num_spinlocks_held() == 0);
        lock_trace_duration!("ContendedWriteAcquire", "name" => self.class_name_ref());

        // Remember the last call to current_ticks.
        let mut now_ticks = timer_current_mono_ticks();
        let current_thread_ptr = thread::current_get();
        // SAFETY: We run in thread context, where the current thread is live while it runs.
        let current_thread: &Thread = unsafe { &*current_thread_ptr };
        // SAFETY: `current_thread` is the current thread.
        let _timer = unsafe { ContentionTimer::new(current_thread, now_ticks) };

        let spin_until_ticks = spin_until_ticks(now_ticks);

        loop {
            // Spin with a read-only load (TTAS) so spinning writers keep the cache line shared
            // until the lock looks unlocked instead of issuing exclusive 16-byte CAS RMWs on every
            // iteration.
            //
            // Ordering: `Relaxed`; acquisition still goes through the `Acquire` CAS below.
            let mut state = self.state.state().load(Ordering::Relaxed);
            if state == BRW_LOCK_UNLOCKED {
                match self.atomic_write_acquire(current_thread_ptr) {
                    Ok(()) => {
                        if P::ENABLED {
                            debug_assert!(
                                self.state.writer() == Some(NonNull::from(current_thread))
                            );
                        }
                        return;
                    }
                    Err(observed) => state = observed,
                }
            }

            // If there are any waiters, implying another thread exhausted its spin phase on the
            // same lock, break out of the spin phase early.
            if state_has_waiters(state) {
                break;
            }

            // Give the arch a chance to relax the CPU.
            libarch::r#yield();
            now_ticks = timer_current_mono_ticks();
            if now_ticks >= spin_until_ticks {
                break;
            }
        }

        assert!(!blocking_disallowed() && num_spinlocks_held() == 0);

        // Enter our wait queue's lock and figure out what to do next.  We don't really know what
        // we need to do yet, so we need to be prepared for needing to back off and try again.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            let mut queue_guard = OwnerGuard::new(&self.wait, token);

            // There were either readers or writers just as we were attempting to
            // acquire the lock.  Now that we are on the slow path and have obtained the
            // queue lock, mark ourselves as a speculative waiter, then re-examine what
            // the state of the BRW lock was as we did so.
            //
            // Ordering: This must be `Acquire`. It orders the `Relaxed` load of the writer in
            // `lock_for_block` after the state observed here (and synchronizes with any prior
            // `write_release` when `prev` has no writer), so the PI owner we see is either the
            // writer whose bit `prev` showed or `None`, and never an older (possibly exited)
            // writer, as the `Acquire` conversion to a waiter does on the read path.
            let prev = self.state.state().fetch_add(BRW_LOCK_WAITER, Ordering::Acquire);

            // If there were still readers or writers when we marked ourselves as
            // waiting, we need to try to block.  If we fail to block (because of a
            // locking conflict), we need to remove our speculative waiter count and try
            // again.  try_block has already dropped the queue lock.
            if state_has_writer(prev) || state_has_readers(prev) {
                // SAFETY: `queue_guard` holds `self.wait`'s lock (the only chain lock held),
                // `token` is not finalized, we registered as a waiter with `Acquire` above while
                // holding `queue_guard`, `current_thread` is the live running thread with no
                // spinlocks or wait queue membership, and `self` stays live until `try_block`
                // returns.
                return if unsafe { self.try_block(queue_guard, current_thread, true, token) } {
                    TransactionResult::DONE
                } else {
                    TransactionResult::BACKOFF
                };
            }

            // OK, there were no readers, no writers.  Were there waiters when we tried
            // to add ourselves to the waiter pool?
            if !state_has_waiters(prev) {
                // There were no waiters, we are done acquiring locks.
                ChainLockTransaction::finalize(token);

                // Since we have flagged ourselves as a speculative waiter, no one else
                // can currently grab the lock for write without following the slow path
                // and obtaining the queue lock, which we currently hold.  So, we are free
                // to simply declare ourselves the owner of the BRW lock, convert our
                // waiter status to writer, then get out.
                if P::ENABLED {
                    self.state.set_writer(Some(NonNull::from(current_thread)));
                }
                // Ordering: `AcqRel`. The `Acquire` half makes our critical section happen-after
                // the `Release` of the previous holders.
                self.state
                    .state()
                    .fetch_add(BRW_LOCK_WRITER.wrapping_sub(BRW_LOCK_WAITER), Ordering::AcqRel);
                queue_guard.release();
                return TransactionResult::DONE;
            }

            // OK, it looks like there were no readers or writers, but there were
            // waiters who were not us.
            //
            // SAFETY: `queue_guard` holds this lock's queue's lock, the only lock held, and the
            // transaction is not finalized.
            if unsafe { self.try_wake(&mut queue_guard.proof(), token, &preempt_disabled_token) }
                .is_some()
            {
                // We succeeded in waking one or more threads, giving them the lock in the
                // process.  Since someone else now owns the lock we need to try to block.
                // If we succeed, then when we wake up, we should have been granted the
                // lock and can just return.
                //
                // Note that try_wake finalized our transaction after it succeeded in
                // obtaining the locks needed to wake up a thread.  We are still holding
                // the wait queue's lock, but we need to restart the transaction before
                // attempting to block (which will require holding a new set of locks).
                ChainLockTransaction::active_ref(token)
                    .restart(clt_tag!("BrwLock<PI>::ContendedWriteAcquire (restart)"), token);
                // SAFETY: `queue_guard` holds `self.wait`'s lock (the only chain lock held after
                // `try_wake` released woken thread locks), `token` was just restarted above, our
                // `Acquire` waiter registration from above remains active across `try_wake`,
                // `current_thread` is the live running thread with no spinlocks or wait queue
                // membership, and `self` stays live until `try_block` returns.
                if unsafe { self.try_block(queue_guard, current_thread, true, token) } {
                    return TransactionResult::DONE;
                }

                // We didn't manage to block, but try_block has removed our speculative
                // wait count and dropped the queue lock for us, we don't need to do it
                // here.
            } else {
                // We failed to wake anyone, remove our speculative wait count, drop the
                // queue lock, and try again.
                //
                // Ordering: `Relaxed`; this only undoes our own speculative waiter count.
                self.state.state().fetch_sub(BRW_LOCK_WAITER, Ordering::Relaxed);
                queue_guard.release();
            }
            TransactionResult::BACKOFF
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("BrwLock<PI>::ContendedWriteAcquire"),
            do_transaction,
        );
        if P::ENABLED {
            debug_assert!(self.state.writer() == Some(NonNull::from(current_thread)));
        }
    }

    /// Wakes the waiters which can now acquire the lock, after a release observed waiters.
    #[inline(never)]
    fn release_wakeup(&self) {
        assert!(!blocking_disallowed() && num_spinlocks_held() == 0);
        // Don't reschedule whilst we're waking up all the threads as if there are
        // several readers available then we'd like to get them all out of the wait
        // queue.
        //
        // See the comment in `block_locked` for why this eager reschedule
        // disabled is important.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables interrupts and eager
            // rescheduling for the duration of this transaction, so it cannot be preempted.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            let mut queue_guard = OwnerGuard::new(&self.wait, token);

            // Ordering: `Relaxed`; the queue's lock orders this against the waiters.
            let count = self.state.state().load(Ordering::Relaxed);
            let finished = if state_has_waiters(count)
                && !state_has_writer(count)
                && !state_has_readers(count)
            {
                // If try_wake succeeds, it will finalize our transaction for us once it
                // obtains all of the locks it needs.
                //
                // SAFETY: `queue_guard` holds this lock's queue's lock, the only lock held, and
                // the transaction is not finalized.
                unsafe { self.try_wake(&mut queue_guard.proof(), token, &preempt_disabled_token) }
                    .is_some()
            } else {
                ChainLockTransaction::finalize(token);
                true
            };

            queue_guard.release();
            if finished { TransactionResult::DONE } else { TransactionResult::BACKOFF }
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("BrwLock<PI>::ReleaseWakeup"),
            do_transaction,
        );
    }

    /// Attempts to acquire the unlocked write lock, recording `current_thread` as the writer on
    /// success and returning `Err(observed_state)` on failure.
    #[inline(always)]
    fn atomic_write_acquire(&self, current_thread: *mut Thread) -> Result<(), u64> {
        // To prevent a race between setting the BRW_LOCK_WRITER bit and the writer
        // we perform a 16-byte compare and swap of both values. This ensures that
        // block can never fail to see a writer. Other possibilities are:
        //
        //   * Disable interrupts: This would be correct, but disabling interrupts
        //     is more expensive than a 16-byte CAS.
        //
        //   * thread_preempt_disable: Cheaper than disabling interrupts but is
        //     **INCORRECT** as when preemption happens we must take the
        //     thread_lock to proceed, but block must hold the thread lock until it
        //     observes that writer has been set, thus resulting in deadlock.
        //
        // Ordering: `Acquire` on success, so that the critical section happens-after the lock is
        // acquired; `Relaxed` on failure, as nothing is acquired.
        self.state
            .compare_exchange(
                BRW_LOCK_UNLOCKED,
                BRW_LOCK_WRITER,
                NonNull::new(current_thread),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map(|_| ())
    }
}

#[pinned_drop]
impl<P: BrwLockEnablePi> PinnedDrop for BrwLock<P> {
    fn drop(self: Pin<&mut Self>) {
        // Ordering: `Relaxed`; the lock is being destroyed, so no other thread may use it.
        debug_assert!(self.state.state().load(Ordering::Relaxed) == 0);
    }
}

/// Policy for acquiring and releasing a [`BrwLock`] in shared (read) mode.
pub struct BrwLockReaderPolicy;

// SAFETY: `BrwLock::read_acquire` and `BrwLock::read_release` enforce shared reader
// synchronization on the `BrwLock`, excluding writers.
unsafe impl<P: BrwLockEnablePi> LockPolicy<BrwLock<P>> for BrwLockReaderPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::acquire`] contract.
    #[inline(always)]
    unsafe fn acquire(lock: &BrwLock<P>, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Kernel code runs in thread context, and the caller pairs `acquire` with
        // `release` on the same thread via the `!Send` pinned guard (`state`). Ambient blocking /
        // spinlock / `ChainLockTransaction` restrictions are enforced by runtime panics in
        // `BrwLock`, not UB preconditions.
        unsafe { lock.read_acquire() };
    }

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::reacquire`] contract.
    #[inline(always)]
    unsafe fn reacquire(lock: &BrwLock<P>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { Self::acquire(lock, state, ()) };
    }

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::release`] contract (`lock` must be currently held in
    /// shared (read) mode by the calling thread).
    #[inline(always)]
    unsafe fn release(lock: &BrwLock<P>, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees the current thread holds the read lock (acquired through
        // `read_acquire` by this policy, which mints no `PreemptDisabledToken`), and `lock`
        // borrows the `BrwLock` for the duration of `release`.
        unsafe { lock.read_release() };
    }
}

/// Policy for acquiring and releasing a [`BrwLock`] in exclusive (write) mode.
pub struct BrwLockWriterPolicy;

// SAFETY: `BrwLock::write_acquire` and `BrwLock::write_release` enforce exclusive mutual exclusion
// on the `BrwLock`.
unsafe impl<P: BrwLockEnablePi> LockPolicy<BrwLock<P>> for BrwLockWriterPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::acquire`] contract.
    #[inline(always)]
    unsafe fn acquire(lock: &BrwLock<P>, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Kernel code runs in thread context, and the caller pairs `acquire` with
        // `release` on the same thread via the `!Send` pinned guard (`state`). Ambient blocking /
        // spinlock / `ChainLockTransaction` restrictions are enforced by runtime panics in
        // `BrwLock`, not UB preconditions.
        unsafe { lock.write_acquire() };
    }

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::reacquire`] contract.
    #[inline(always)]
    unsafe fn reacquire(lock: &BrwLock<P>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { Self::acquire(lock, state, ()) };
    }

    /// # Safety
    ///
    /// Caller must uphold the [`LockPolicy::release`] contract (`lock` must be currently held in
    /// exclusive (write) mode by the calling thread).
    #[inline(always)]
    unsafe fn release(lock: &BrwLock<P>, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees the current thread holds the write lock (acquired through
        // `write_acquire` by this policy), and `lock` borrows the `BrwLock` for the duration of
        // `release`.
        unsafe { lock.write_release() };
    }
}

// SAFETY: `BrwLockWriterPolicy` enforces exclusive mutual exclusion on the `BrwLock`.
unsafe impl<P: BrwLockEnablePi> ExclusiveLockPolicy<BrwLock<P>> for BrwLockWriterPolicy {}

// SAFETY: `BrwLock` is a physical kernel reader-writer lock with a unique storage address.
unsafe impl<P: BrwLockEnablePi> RawLock for BrwLock<P> {
    type DefaultPolicy = BrwLockWriterPolicy;

    /// Initializes the lock in place, assigning it a unique lock ID and the lock class name of
    /// `class_id` for lock tracing (when enabled).
    ///
    /// # Safety
    ///
    /// `class_id` must be either null or a valid static `lockdep::LockClassId` pointer.
    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, Infallible> {
        Self::init_with_name_storage({
            let mut name_storage = RwLockNameStorage::<RWLOCK_NAME_TRACING_ENABLED>::new();
            // SAFETY: The caller guarantees `class_id` is either null or a valid static
            // `LockClassId` (`*const LockClassState`), whose `#[repr(C)]` first field at offset 0
            // is a null or `'static InternedString` pointer in `__fxt_interned_string_table`.
            unsafe { name_storage.set_lock_class_id(class_id) };
            name_storage
        })
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast::<c_void>()
    }
}

// SAFETY: `BrwLockReaderPolicy` acquires the `BrwLock` in shared (read) mode, which allows multiple
// concurrent readers but excludes the exclusive (write) mode acquired by `BrwLockWriterPolicy`,
// the default and only `ExclusiveLockPolicy` for `BrwLock`.
unsafe impl<P: BrwLockEnablePi> RawSharedLock for BrwLock<P> {
    type SharedPolicy = BrwLockReaderPolicy;
}

/// Lockdep-tracked kernel [`BrwLockPi`].
pub type TrackedBrwLockPi = LockdepTracked<BrwLockPi>;

/// Lockdep-tracked kernel [`BrwLockNoPi`].
pub type TrackedBrwLockNoPi = LockdepTracked<BrwLockNoPi>;

/// Size in bytes of a [`BrwLockNoPi`].
const BRWLOCK_NO_PI_SIZE: usize = if RWLOCK_NAME_TRACING_ENABLED { 80 } else { 72 };

static_assert!(size_of::<BrwLockNoPi>() == BRWLOCK_NO_PI_SIZE);
static_assert!(align_of::<BrwLockNoPi>() == 8);

#[cfg(not(target_arch = "riscv64"))]
static_assert!(size_of::<BrwLockPi>() == 128);
#[cfg(not(target_arch = "riscv64"))]
static_assert!(align_of::<BrwLockPi>() == 16);

#[cfg(all(not(target_arch = "riscv64"), not(with_lock_dep)))]
const _: () = {
    assert!(size_of::<TrackedBrwLockPi>() == 128);
    assert!(align_of::<TrackedBrwLockPi>() == 16);
};

#[cfg(all(not(target_arch = "riscv64"), with_lock_dep))]
const _: () = {
    assert!(size_of::<TrackedBrwLockPi>() == 144);
    assert!(align_of::<TrackedBrwLockPi>() == 16);
};

#[cfg(not(with_lock_dep))]
const _: () = {
    assert!(size_of::<TrackedBrwLockNoPi>() == BRWLOCK_NO_PI_SIZE);
    assert!(align_of::<TrackedBrwLockNoPi>() == 8);
};

#[cfg(with_lock_dep)]
const _: () = {
    assert!(size_of::<TrackedBrwLockNoPi>() == BRWLOCK_NO_PI_SIZE + 8);
    assert!(align_of::<TrackedBrwLockNoPi>() == 8);
};
