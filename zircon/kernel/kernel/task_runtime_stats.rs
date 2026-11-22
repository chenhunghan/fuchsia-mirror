// Copyright 2020 The Fuchsia Authors
// Copyright 2021 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Types and utilities for efficiently accumulating and aggregating task runtime
//! stats.
//!
//! Runtime stats are maintained at three levels: thread, process, and job.
//! Threads maintain and update their runtime stats as threads change state.
//! Terminating threads roll up to their owning process, and terminating
//! processes roll up to their owning job, however queries of process and job
//! stats require summing all of the currently running thread stats with the
//! rolled up stats for terminated threads.
//!
//! Per-thread stats are maintained by [`ThreadRuntimeStats`], which provides a
//! sequence locked snapshot of the runtime stats with an affordance to
//! compensate for unaccounted run-time/queue-time when a thread is in a runnable
//! state (i.e. ready or running).

use core::ffi::c_void;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, offset_of};
use core::ops;
use core::sync::atomic::{AtomicI64, AtomicU8, Ordering};

use affine::{Ratio, Round};
use ksync::{
    ConstRawLock, ExclusiveLockPolicy, LockClassRegistration, LockFlags, LockPolicy,
    LockdepTracked, LockdepTrackedGuardState, RawLock, declare_interned_string,
};
use pin_init::{PinInit, pin_data, pin_init};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_types::{zx_duration_mono_ticks_t, zx_info_task_runtime_t, zx_instant_mono_ticks_t};

use crate::arch_rs::{
    InterruptSavedState, arch_interrupt_restore, arch_interrupt_save, blocking_disallowed,
    ints_disabled, set_blocking_disallowed,
};
use crate::kconcurrent::{
    ReadTransactionToken, SYNC_OPT_FENCE, SeqLock, SeqLockPayload, WriteGuard,
};
use crate::kernel::relaxed_atomic::RelaxedAtomic;
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::platform_rs::timer::{
    GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS, GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES,
    GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS, GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES,
    timer_current_mono_ticks_synchronized, timer_get_ticks_to_time_ratio,
};

/// Runtime stats of a thread, process, or job.
///
/// Not safe for concurrent use by multiple threads.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TaskRuntimeStats {
    /// The total duration (in ticks) spent running on a CPU.
    pub cpu_ticks: zx_duration_mono_ticks_t,

    /// The total duration (in ticks) spent ready to start running.
    pub queue_ticks: zx_duration_mono_ticks_t,

    /// The total duration (in ticks) spent handling page faults.
    pub page_fault_ticks: zx_duration_mono_ticks_t,

    /// The total duration (in ticks) spent contended on kernel locks.
    pub lock_contention_ticks: zx_duration_mono_ticks_t,
}

zr::static_assert_size_and_align!(TaskRuntimeStats, 32, 8);
zr::static_assert!(offset_of!(TaskRuntimeStats, cpu_ticks) == 0);
zr::static_assert!(offset_of!(TaskRuntimeStats, queue_ticks) == 8);
zr::static_assert!(offset_of!(TaskRuntimeStats, page_fault_ticks) == 16);
zr::static_assert!(offset_of!(TaskRuntimeStats, lock_contention_ticks) == 24);

impl ops::AddAssign<&TaskRuntimeStats> for TaskRuntimeStats {
    /// Adds another `TaskRuntimeStats` to this one, saturating each tick count at
    /// `ZX_TIME_INFINITE` / `ZX_TIME_INFINITE_PAST` on overflow (`zx_ticks_add_ticks`).
    fn add_assign(&mut self, other: &TaskRuntimeStats) {
        self.cpu_ticks = self.cpu_ticks.saturating_add(other.cpu_ticks);
        self.queue_ticks = self.queue_ticks.saturating_add(other.queue_ticks);
        self.page_fault_ticks = self.page_fault_ticks.saturating_add(other.page_fault_ticks);
        self.lock_contention_ticks =
            self.lock_contention_ticks.saturating_add(other.lock_contention_ticks);
    }
}

impl ops::AddAssign<TaskRuntimeStats> for TaskRuntimeStats {
    fn add_assign(&mut self, other: TaskRuntimeStats) {
        *self += &other;
    }
}

/// Conversion to [`zx_info_task_runtime_t`], scaling each tick count to nanoseconds.
impl From<TaskRuntimeStats> for zx_info_task_runtime_t {
    #[inline]
    fn from(stats: TaskRuntimeStats) -> Self {
        let ticks_to_time: Ratio = timer_get_ticks_to_time_ratio();
        Self {
            cpu_time: ticks_to_time.scale::<{ Round::DOWN }>(stats.cpu_ticks),
            queue_time: ticks_to_time.scale::<{ Round::DOWN }>(stats.queue_ticks),
            page_fault_time: ticks_to_time.scale::<{ Round::DOWN }>(stats.page_fault_ticks),
            lock_contention_time: ticks_to_time
                .scale::<{ Round::DOWN }>(stats.lock_contention_ticks),
        }
    }
}

/// [`SeqLock`] synchronization mode used for [`ThreadRuntimeStats`].
const RUNTIME_STATS_SYNC_TYPE: u8 = SYNC_OPT_FENCE;

/// Raw `SeqLock` wrapper implementing [`RawLock`] so it can be wrapped in [`LockdepTracked`].
#[pin_data]
#[repr(transparent)]
struct RawRuntimeStatsSeqLock {
    inner: SeqLock<RUNTIME_STATS_SYNC_TYPE>,
}

// SAFETY: `RawRuntimeStatsSeqLock` wraps a 4-byte `SeqLock` with a unique storage address.
unsafe impl RawLock for RawRuntimeStatsSeqLock {
    type DefaultPolicy = RuntimeStatsValidationPolicy;

    /// # Safety
    ///
    /// `_class_id` is unused by the underlying `SeqLock`.
    #[inline]
    unsafe fn init(_class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self { inner: SeqLock::new() })
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        core::ptr::from_ref(self).cast_mut().cast()
    }
}

impl ConstRawLock for RawRuntimeStatsSeqLock {
    const INIT: Self = Self { inner: SeqLock::new() };
}

/// Lock policy delegating lockdep validation to [`LockdepTracked`] while the surrounding RAII
/// guards manage interrupt state, `blocking_disallowed`, and the underlying [`SeqLock`].
struct RuntimeStatsValidationPolicy;

// SAFETY: `RawRuntimeStatsSeqLock` and `RuntimeStatsValidationPolicy` are module-private, never
// used with `KMutex` or `KCell`, and only ever driven via
// `LockdepTracked::post_validate_acquire` and `LockdepTracked::release` inside
// `SeqLockExclusiveGuard` and `SeqLockSharedIrqSaveGuard`, which acquire and release the
// underlying `SeqLock<SYNC_OPT_FENCE>` directly while interrupts are disabled.
unsafe impl LockPolicy<RawRuntimeStatsSeqLock> for RuntimeStatsValidationPolicy {
    type AcquireArgs = ();
    type GuardState = ();

    /// # Safety
    ///
    /// No-op: the surrounding guard manages the underlying [`SeqLock`].
    #[inline]
    unsafe fn acquire(
        _lock: &RawRuntimeStatsSeqLock,
        _state: &mut Self::GuardState,
        _args: Self::AcquireArgs,
    ) {
    }

    /// # Safety
    ///
    /// No-op: the surrounding guard manages the underlying [`SeqLock`].
    #[inline]
    unsafe fn reacquire(_lock: &RawRuntimeStatsSeqLock, _state: &mut Self::GuardState) {}

    /// # Safety
    ///
    /// No-op: the surrounding guard manages the underlying [`SeqLock`].
    #[inline]
    unsafe fn release(_lock: &RawRuntimeStatsSeqLock, _state: &mut Self::GuardState) {}
}

// SAFETY: `RawRuntimeStatsSeqLock` and `RuntimeStatsValidationPolicy` are module-private, never
// used with `KMutex` or `KCell`, and only ever driven via
// `LockdepTracked::post_validate_acquire` and `LockdepTracked::release` inside
// `SeqLockExclusiveGuard` and `SeqLockSharedIrqSaveGuard`, which acquire and release the
// underlying `SeqLock<SYNC_OPT_FENCE>` directly.
unsafe impl ExclusiveLockPolicy<RawRuntimeStatsSeqLock> for RuntimeStatsValidationPolicy {}

/// Lockdep flags for [`ThreadRuntimeStats::seq_lock`].
const THREAD_RUNTIME_STATS_SEQ_LOCK_FLAGS: LockFlags =
    lockdep::LOCK_FLAGS_IRQ_SAFE | lockdep::LOCK_FLAGS_REPORTING_DISABLED;

declare_interned_string!(THREAD_RUNTIME_STATS_SEQ_LOCK_STRING_REG, "ThreadRuntimeStats::seq_lock");

/// Lockdep lock class registration for [`ThreadRuntimeStats::seq_lock`].
// SAFETY: `LockClassRegistration` matches C++ section layout/alignment, uses
// interior-mutable writable storage (`OpaqueBytes`), and holds a valid static
// `InternedString` pointer.
#[unsafe(link_section = "rust_lock_classes")]
#[used]
static THREAD_RUNTIME_STATS_SEQ_LOCK_REGISTRATION: LockClassRegistration =
    LockClassRegistration::with_flags(
        THREAD_RUNTIME_STATS_SEQ_LOCK_STRING_REG,
        THREAD_RUNTIME_STATS_SEQ_LOCK_FLAGS,
    );

/// Lockdep-tracked [`SeqLock`] used by [`ThreadRuntimeStats`].
type RuntimeStatsSeqLock = LockdepTracked<
    RawRuntimeStatsSeqLock,
    RuntimeStatsValidationPolicy,
    THREAD_RUNTIME_STATS_SEQ_LOCK_FLAGS,
>;

/// Sealed trait module preventing external implementations of [`ExclusiveLockOption`].
mod private {
    /// Marker trait sealing [`super::ExclusiveLockOption`].
    pub trait Sealed {}
}

/// Trait representing the exclusive lock acquisition mode (`IrqSave` or `NoIrqSave`) for
/// [`ThreadRuntimeStats::update`].
///
/// # Safety
///
/// Implementations must ensure that after [`Self::save_irq`] returns, interrupts are disabled
/// on the current CPU (`ints_disabled()`) and remain disabled until [`Self::restore_irq`] is
/// called.
pub unsafe trait ExclusiveLockOption: private::Sealed + Copy {
    /// Saved interrupt state for this option (`InterruptSavedState` for `IrqSave`, `()` for
    /// `NoIrqSave`).
    type IrqState: Copy;

    /// Saves or validates interrupt state before lockdep validation.
    fn save_irq() -> Self::IrqState;

    /// Restores interrupt state after lock release.
    fn restore_irq(state: Self::IrqState);
}

/// Exclusive lock policy that saves and disables interrupts around the [`SeqLock`] acquisition.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExclusiveIrqSave;

impl private::Sealed for ExclusiveIrqSave {}

// SAFETY: `save_irq` calls `arch_interrupt_save()`, disabling interrupts on the current CPU until
// `restore_irq` calls `arch_interrupt_restore(state)`.
unsafe impl ExclusiveLockOption for ExclusiveIrqSave {
    type IrqState = InterruptSavedState;

    #[inline(always)]
    fn save_irq() -> Self::IrqState {
        arch_interrupt_save()
    }

    #[inline(always)]
    fn restore_irq(state: Self::IrqState) {
        arch_interrupt_restore(state);
    }
}

/// Exclusive lock policy that requires interrupts to already be disabled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExclusiveNoIrqSave;

impl private::Sealed for ExclusiveNoIrqSave {}

// SAFETY: `save_irq` asserts `ints_disabled()`, ensuring interrupts are already disabled on the
// current CPU and remain disabled across the guard's lifetime.
unsafe impl ExclusiveLockOption for ExclusiveNoIrqSave {
    type IrqState = ();

    #[inline(always)]
    fn save_irq() -> Self::IrqState {
        assert!(ints_disabled());
    }

    #[inline(always)]
    fn restore_irq(_state: Self::IrqState) {}
}

/// Type tag used to select an exclusive lock option for [`ThreadRuntimeStats::update`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LockOption<Opt>(PhantomData<Opt>);

/// RAII guard for exclusive [`SeqLock`] acquisition with lockdep validation and
/// `blocking_disallowed` tracking.
struct SeqLockExclusiveGuard<'a, Opt: ExclusiveLockOption> {
    lock: &'a RuntimeStatsSeqLock,
    lockdep_state: &'a mut LockdepTrackedGuardState<()>,
    write_guard: ManuallyDrop<WriteGuard<'a, RUNTIME_STATS_SYNC_TYPE>>,
    interrupt_state: Opt::IrqState,
    blocking_disallow_state: bool,
}

impl<'a, Opt: ExclusiveLockOption> SeqLockExclusiveGuard<'a, Opt> {
    #[inline(always)]
    fn new(
        lock: &'a RuntimeStatsSeqLock,
        lockdep_state: &'a mut LockdepTrackedGuardState<()>,
    ) -> Self {
        let interrupt_state = Opt::save_irq();
        let blocking_disallow_state = blocking_disallowed();
        set_blocking_disallowed(true);
        // SAFETY: `Opt::save_irq()` has disabled interrupts on the current CPU, `lockdep_state` is
        // a freshly default-initialized stack slot in `ThreadRuntimeStats::update`, and this
        // module-private guard is never passed to `core::mem::forget`, so `Self::drop` is
        // guaranteed to call `release` with interrupts still disabled before `lockdep_state`
        // leaves its stack frame.
        unsafe {
            <RuntimeStatsSeqLock as LockPolicy<RuntimeStatsSeqLock>>::post_validate_acquire(
                lock,
                lockdep_state,
                (),
            );
        }
        let write_guard = ManuallyDrop::new(lock.inner.acquire());
        Self { lock, lockdep_state, write_guard, interrupt_state, blocking_disallow_state }
    }

    #[inline(always)]
    fn write_guard(&self) -> &WriteGuard<'a, RUNTIME_STATS_SYNC_TYPE> {
        &self.write_guard
    }
}

impl<Opt: ExclusiveLockOption> Drop for SeqLockExclusiveGuard<'_, Opt> {
    #[inline(always)]
    fn drop(&mut self) {
        // Validate the release operation and clear the validator state before releasing the lock
        // and re-enabling interrupts.
        //
        // SAFETY: `self.lockdep_state` was initialized by `post_validate_acquire` in `Self::new`
        // and the current thread holds the lock with interrupts still disabled.
        unsafe {
            <RuntimeStatsSeqLock as LockPolicy<RuntimeStatsSeqLock>>::release(
                self.lock,
                self.lockdep_state,
            );
        }
        // SAFETY: `Drop::drop` runs once and `self.write_guard` is not accessed again.
        unsafe {
            ManuallyDrop::drop(&mut self.write_guard);
        }
        set_blocking_disallowed(self.blocking_disallow_state);
        Opt::restore_irq(self.interrupt_state);
    }
}

/// RAII guard for shared [`SeqLock`] read transactions with interrupts disabled and lockdep
/// validation.
struct SeqLockSharedIrqSaveGuard<'a> {
    lock: &'a RuntimeStatsSeqLock,
    lockdep_state: &'a mut LockdepTrackedGuardState<()>,
    interrupt_state: InterruptSavedState,
    token: ReadTransactionToken,
    result_target: &'a mut bool,
}

impl<'a> SeqLockSharedIrqSaveGuard<'a> {
    #[inline(always)]
    fn new(
        lock: &'a RuntimeStatsSeqLock,
        lockdep_state: &'a mut LockdepTrackedGuardState<()>,
        result_target: &'a mut bool,
    ) -> Self {
        *result_target = false;
        let interrupt_state = arch_interrupt_save();
        // SAFETY: `arch_interrupt_save()` has disabled interrupts on the current CPU,
        // `lockdep_state` is a freshly default-initialized stack slot in
        // `ThreadRuntimeStats::read`, and this module-private guard is never passed to
        // `core::mem::forget`, so `Self::drop` is guaranteed to call `release` with interrupts
        // still disabled before `lockdep_state` leaves its stack frame.
        unsafe {
            <RuntimeStatsSeqLock as LockPolicy<RuntimeStatsSeqLock>>::post_validate_acquire(
                lock,
                lockdep_state,
                (),
            );
        }
        let token = lock.inner.begin_read_transaction();
        Self { lock, lockdep_state, interrupt_state, token, result_target }
    }
}

impl Drop for SeqLockSharedIrqSaveGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        // Validate the release operation and clear the validator state before ending the read
        // transaction and re-enabling interrupts.
        //
        // SAFETY: `self.lockdep_state` was initialized by `post_validate_acquire` in `Self::new`
        // and the current thread holds the lock with interrupts still disabled.
        unsafe {
            <RuntimeStatsSeqLock as LockPolicy<RuntimeStatsSeqLock>>::release(
                self.lock,
                self.lockdep_state,
            );
        }
        *self.result_target = self.lock.inner.end_read_transaction(self.token);
        arch_interrupt_restore(self.interrupt_state);
    }
}

/// Per-thread scheduler runtime statistics.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable)]
struct ThreadStats {
    total_running_ticks: zx_duration_mono_ticks_t,
    total_ready_ticks: zx_duration_mono_ticks_t,
    /// When the thread entered its current state.
    state_change_ticks: zx_duration_mono_ticks_t,
    current_state: u8,
    _pad: [u8; 7],
}

zr::static_assert_size_and_align!(ThreadStats, 32, 8);

/// Snapshot returned by [`ThreadRuntimeStats::read`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReadResult {
    stats: ThreadStats,
    now: zx_instant_mono_ticks_t,
}

/// Manages sequence locked updates and access to per-thread runtime stats.
#[repr(C)]
pub struct ThreadRuntimeStats {
    /// This lock is acquired in the scheduler. Disable reporting to prevent
    /// interacting with the debug log and interfering with the last result, which
    /// can cause lockdep failures.
    seq_lock: RuntimeStatsSeqLock,

    published_stats: SeqLockPayload<ThreadStats, RUNTIME_STATS_SYNC_TYPE>,
    page_fault_ticks: RelaxedAtomic<AtomicI64>,
    lock_contention_ticks: RelaxedAtomic<AtomicI64>,
}

zr::static_assert!(core::mem::align_of::<ThreadRuntimeStats>() == 8);

impl ThreadRuntimeStats {
    /// Update must be locked Exclusive with either `IRQ_SAVE` or `NO_IRQ_SAVE`.
    pub const IRQ_SAVE: LockOption<ExclusiveIrqSave> = LockOption(PhantomData);
    /// Update must be locked Exclusive with either `IRQ_SAVE` or `NO_IRQ_SAVE`.
    pub const NO_IRQ_SAVE: LockOption<ExclusiveNoIrqSave> = LockOption(PhantomData);

    /// Creates a new default-initialized [`ThreadRuntimeStats`].
    #[inline]
    pub const fn new() -> Self {
        Self {
            seq_lock: RuntimeStatsSeqLock::const_init(
                THREAD_RUNTIME_STATS_SEQ_LOCK_REGISTRATION.get(),
            ),
            published_stats: SeqLockPayload::new(ThreadStats {
                total_running_ticks: 0,
                total_ready_ticks: 0,
                state_change_ticks: 0,
                current_state: ThreadStateKind::Initial as u8,
                _pad: [0; 7],
            }),
            page_fault_ticks: RelaxedAtomic::<AtomicI64>::new(0),
            lock_contention_ticks: RelaxedAtomic::<AtomicI64>::new(0),
        }
    }

    /// Updates the [`ThreadStats`] state with the given deltas and last thread state.
    pub fn update<Opt: ExclusiveLockOption>(
        &self,
        new_state: ThreadStateKind,
        _opt: LockOption<Opt>,
    ) {
        // Enter our sequence lock and obtain a mutable reference to our payload so
        // we can update the payload in-place.  We are free to read the contents of
        // the payload without the use of any atomics (the SeqLock behaves like a
        // spinlock in this situation, ensures proper ordering and excluding other
        // writes), however we need to make sure to use relaxed atomic stores when
        // writing to the payload (as we may have concurrent reads taking place from
        // observers).
        let mut lockdep_state = LockdepTrackedGuardState::default();
        let thread_guard = SeqLockExclusiveGuard::<Opt>::new(&self.seq_lock, &mut lockdep_state);
        let stats: *mut ThreadStats =
            self.published_stats.begin_in_place_update(thread_guard.write_guard());

        // SAFETY: `thread_guard` holds `seq_lock` exclusively, so no other writer can modify the
        // payload concurrently. Reading scalar fields via `*stats` without forming a shared
        // reference avoids aliasing with the relaxed atomic stores below.
        let current_state = unsafe { (*stats).current_state };

        // Skip the update if we are already in the new state.
        //
        // This can happen when a thread unblocks, and Scheduler::Unblock changes
        // the state of the thread from BLOCKED to READY, and records the transition
        // in the runtime stats in the process. Eventually, we will reschedule on
        // this CPU, and we will record another state transition in the stats
        // (here), but we actually already recorded the stats previously.
        //
        // TODO(johngro): Look into changing this behavior.  It would be better if
        // we only ever called this method when we _knew_ for a fact that we were
        // changing states.
        if current_state == new_state as u8 {
            return;
        }

        // Make sure that our sampling of the ticks counter takes place between the
        // two stores of the sequence number, and is not allowed to move outside of
        // the update transaction because of pipelined execution.
        let now: zx_instant_mono_ticks_t = timer_current_mono_ticks_synchronized::<
            {
                GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
                    | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES
            },
        >()
        .0;

        // Now go ahead an update our payload, making sure to use relaxed atomic
        // stores when writing to the contents.
        if current_state == ThreadStateKind::Running as u8 {
            // SAFETY: `thread_guard` holds `seq_lock` exclusively.
            // `Relaxed` ordering is sufficient because `SeqLock` fences order payload stores
            // between the sequence number updates.
            unsafe {
                let delta: zx_duration_mono_ticks_t =
                    now.saturating_sub((*stats).state_change_ticks);
                let total = (*stats).total_running_ticks.saturating_add(delta);
                AtomicI64::from_ptr(&raw mut (*stats).total_running_ticks)
                    .store(total, Ordering::Relaxed);
            }
        } else if current_state == ThreadStateKind::Ready as u8 {
            // SAFETY: `thread_guard` holds `seq_lock` exclusively.
            // `Relaxed` ordering is sufficient because `SeqLock` fences order payload stores
            // between the sequence number updates.
            unsafe {
                let delta: zx_duration_mono_ticks_t =
                    now.saturating_sub((*stats).state_change_ticks);
                let total = (*stats).total_ready_ticks.saturating_add(delta);
                AtomicI64::from_ptr(&raw mut (*stats).total_ready_ticks)
                    .store(total, Ordering::Relaxed);
            }
        }

        // SAFETY: `thread_guard` holds `seq_lock` exclusively.
        // `Relaxed` ordering is sufficient because `SeqLock` fences order payload stores between
        // the sequence number updates.
        unsafe {
            AtomicI64::from_ptr(&raw mut (*stats).state_change_ticks).store(now, Ordering::Relaxed);
            AtomicU8::from_ptr(&raw mut (*stats).current_state)
                .store(new_state as u8, Ordering::Relaxed);
        }
    }

    /// Updates the page fault ticks with the given delta. These values do not
    /// require relative coherence with other state.
    #[inline]
    pub fn add_page_fault_ticks(&self, delta: zx_duration_mono_ticks_t) {
        self.page_fault_ticks.fetch_add(delta);
    }

    /// Updates the lock contention ticks with the given delta. These values do not
    /// require relative coherence with other state.
    #[inline]
    pub fn add_lock_contention_ticks(&self, delta: zx_duration_mono_ticks_t) {
        self.lock_contention_ticks.fetch_add(delta);
    }

    /// Returns the instantaneous runtime stats for the thread, including the time
    /// the thread has spent in its current state (if that state is either READY or
    /// RUNNING).
    pub fn get_compensated_task_runtime_stats(&self) -> TaskRuntimeStats {
        let mut res: ReadResult = self.read();

        // Adjust for the current time if the thread was in a state that we track
        // when we queried its stats.
        if (res.stats.current_state == ThreadStateKind::Running as u8)
            || (res.stats.current_state == ThreadStateKind::Ready as u8)
        {
            let delta: zx_duration_mono_ticks_t =
                res.now.saturating_sub(res.stats.state_change_ticks);
            let counter: &mut zx_duration_mono_ticks_t =
                if res.stats.current_state == ThreadStateKind::Running as u8 {
                    &mut res.stats.total_running_ticks
                } else {
                    &mut res.stats.total_ready_ticks
                };
            *counter = counter.saturating_add(delta);
        }

        TaskRuntimeStats {
            cpu_ticks: res.stats.total_running_ticks,
            queue_ticks: res.stats.total_ready_ticks,
            page_fault_ticks: self.page_fault_ticks.load(),
            lock_contention_ticks: self.lock_contention_ticks.load(),
        }
    }

    /// Returns a coherent snapshot of the [`ThreadStats`] state.
    fn read(&self) -> ReadResult {
        let mut ret = ReadResult::default();
        let mut success = false;
        while !success {
            {
                // TODO(johngro): Look into doing this without disabling ints.  Right
                // now, the following is possible:
                //
                // 1) Code enters the lock-guard, "holding" the SeqLock for read.
                // 2) Before exiting, the preemption timer fires.
                // 3) The scheduler selects a new thread during preemption, and needs to
                //    update stats for the old thread/process.
                // 4) It calls into update which attempts to hold the SeqLock exclusively.
                // 5) Lockdep asserts, because it looks like we are attempting to enter
                //    the same lock class multiple times.
                //
                // Typically, this would be an error, but in the case of a seqlock, it
                // actually isn't.  The read operation (holding the lock with shared
                // access) cannot block the write operation (obtaining the lock
                // exclusively).  The write during the preempt would simply cause the
                // initial read transaction to fail and try again.  Lockdep does not
                // know this however, so it complains.
                //
                // In the short term, we just turn off interrupts during the read.
                // Moving forward, it would be better to teach lockdep about the proper
                // SeqLock semantics, and only have it complain if code attempts to
                // enter the lock exclusively multiple times.
                //
                let mut lockdep_state = LockdepTrackedGuardState::default();
                let _guard = SeqLockSharedIrqSaveGuard::new(
                    &self.seq_lock,
                    &mut lockdep_state,
                    &mut success,
                );
                // Read the payload.
                self.published_stats.read(&mut ret.stats);

                // Make sure that our sampling of the ticks counter takes place between
                // the two reads of the sequence number, and is not allowed to move
                // outside of the region because of pipelined execution.
                ret.now = timer_current_mono_ticks_synchronized::<
                    {
                        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
                            | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
                    },
                >()
                .0;
            }
            if !success {
                libarch::r#yield();
            }
        }

        ret
    }
}

impl Default for ThreadRuntimeStats {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Converts `stats` into a `zx_info_task_runtime_t` written to `out`.
///
/// # Safety
///
/// `stats` must be a valid reference, and `out` must point to valid writable storage for a
/// `zx_info_task_runtime_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_task_runtime_stats_to_info(
    stats: &TaskRuntimeStats,
    out: *mut zx_info_task_runtime_t,
) {
    let info = zx_info_task_runtime_t::from(*stats);
    // SAFETY: Caller guarantees `out` points to valid writable storage for a
    // `zx_info_task_runtime_t`.
    unsafe {
        out.write(info);
    }
}

/// Task runtime stats unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "task_runtime_stats")]
mod tests {
    use unittest::{expect_eq, expect_le, expect_ne, expect_true};
    use zx_types::{
        ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST, zx_duration_mono_ticks_t, zx_info_task_runtime_t,
        zx_instant_mono_ticks_t,
    };

    use super::{ReadResult, TaskRuntimeStats, ThreadRuntimeStats, ThreadStats};
    use crate::kernel::scheduler_state::ThreadStateKind;
    use crate::platform_rs::timer::timer_current_mono_ticks;

    /// Accumulation adds each field independently and saturates instead of wrapping.
    #[test]
    fn test_add_assign_accumulates_and_saturates() {
        let mut stats = TaskRuntimeStats {
            cpu_ticks: 1,
            queue_ticks: 2,
            page_fault_ticks: 3,
            lock_contention_ticks: 4,
        };
        stats += TaskRuntimeStats {
            cpu_ticks: 10,
            queue_ticks: 20,
            page_fault_ticks: 30,
            lock_contention_ticks: 40,
        };
        expect_eq!(stats.cpu_ticks, 11);
        expect_eq!(stats.queue_ticks, 22);
        expect_eq!(stats.page_fault_ticks, 33);
        expect_eq!(stats.lock_contention_ticks, 44);

        // Positive overflow pins at ZX_TIME_INFINITE, negative overflow at ZX_TIME_INFINITE_PAST,
        // and fields that do not overflow are unaffected.
        stats += TaskRuntimeStats {
            cpu_ticks: ZX_TIME_INFINITE,
            queue_ticks: 0,
            page_fault_ticks: ZX_TIME_INFINITE_PAST,
            lock_contention_ticks: 0,
        };
        expect_eq!(stats.cpu_ticks, ZX_TIME_INFINITE);
        expect_eq!(stats.queue_ticks, 22);
        expect_eq!(stats.page_fault_ticks, ZX_TIME_INFINITE_PAST + 33);
        expect_eq!(stats.lock_contention_ticks, 44);
        stats += TaskRuntimeStats { page_fault_ticks: -34, ..TaskRuntimeStats::default() };
        expect_eq!(stats.page_fault_ticks, ZX_TIME_INFINITE_PAST);
    }

    /// Conversion to `zx_info_task_runtime_t` maps zero ticks to zero and keeps fields apart.
    #[test]
    fn test_into_info_zero_and_field_mapping() {
        let info: zx_info_task_runtime_t = TaskRuntimeStats::default().into();
        expect_eq!(info.cpu_time, 0);
        expect_eq!(info.queue_time, 0);
        expect_eq!(info.page_fault_time, 0);
        expect_eq!(info.lock_contention_time, 0);

        // Only the cpu field is populated, so only cpu_time may be non-zero afterwards.
        let info: zx_info_task_runtime_t =
            TaskRuntimeStats { cpu_ticks: 1 << 20, ..TaskRuntimeStats::default() }.into();
        expect_eq!(info.queue_time, 0);
        expect_eq!(info.page_fault_time, 0);
        expect_eq!(info.lock_contention_time, 0);
        expect_true!(info.cpu_time > 0);
    }

    /// Tests thread runtime statistics accumulation and compensation.
    #[test]
    fn thread_runtime_test() {
        const CPU_TICKS: zx_duration_mono_ticks_t = 10;
        const QUEUE_TICKS: zx_duration_mono_ticks_t = 20;
        let mut trs: TaskRuntimeStats;
        let stats = ThreadRuntimeStats::new();
        let mut read_res: ReadResult = stats.read();
        let mut sched_stats: &ThreadStats = &read_res.stats;

        expect_eq!(ThreadStateKind::Initial as u8, sched_stats.current_state);
        expect_eq!(0, sched_stats.state_change_ticks);
        expect_eq!(0, sched_stats.total_running_ticks);
        expect_eq!(0, sched_stats.total_ready_ticks);

        trs = stats.get_compensated_task_runtime_stats();
        expect_eq!(0, trs.cpu_ticks);
        expect_eq!(0, trs.queue_ticks);
        expect_eq!(0, trs.page_fault_ticks);
        expect_eq!(0, trs.lock_contention_ticks);

        let delay_ticks = |ticks: zx_duration_mono_ticks_t| {
            let deadline: zx_instant_mono_ticks_t = timer_current_mono_ticks().0 + ticks;
            loop {
                libarch::r#yield();
                if timer_current_mono_ticks().0 >= deadline {
                    break;
                }
            }
        };

        // Test that runtime is calculated as a function of the stats and the current time spent
        // queued. When the state is set to ThreadStateKind::Ready,
        // get_compensated_task_runtime_stats calculates queue_ticks as:
        //   runtime.queue_ticks + (timer_current_mono_ticks() - state_change_ticks)
        //
        stats.update(ThreadStateKind::Ready, ThreadRuntimeStats::IRQ_SAVE);

        // Spin for at least QUEUE_TICKS, so we know that the projected amount of
        // time has to be >= QUEUE_TICKS.
        delay_ticks(QUEUE_TICKS);

        read_res = stats.read();
        sched_stats = &read_res.stats;
        expect_eq!(ThreadStateKind::Ready as u8, sched_stats.current_state);
        expect_ne!(0, sched_stats.state_change_ticks);
        expect_eq!(0, sched_stats.total_running_ticks);
        // Nothing accumulates into queue ticks until we leave the READY state.
        expect_eq!(0, sched_stats.total_ready_ticks);

        trs = stats.get_compensated_task_runtime_stats();
        expect_le!(QUEUE_TICKS, trs.queue_ticks);
        expect_eq!(0, trs.cpu_ticks);
        expect_eq!(0, trs.page_fault_ticks);
        expect_eq!(0, trs.lock_contention_ticks);

        // Test that runtime is calculated as a function of the stats and the
        // current time spent running. When the state is set to ThreadStateKind::Running,
        // get_compensated_task_runtime_stats calculates cpu_ticks as:
        //
        // runtime.cpu_ticks + (timer_current_mono_ticks() - state_change_ticks)
        //
        // Start by changing state to running.
        stats.update(ThreadStateKind::Running, ThreadRuntimeStats::IRQ_SAVE);

        // When we read the non-compensated stats, the total accumulated queue time
        // has to be at least the compensated queue time before we switched to
        // running.  The accumulated CPU time should still be zero, since we have
        // never been in the running state, then switched out.
        let queue_ticks_lower_bound: zx_duration_mono_ticks_t = trs.queue_ticks;

        read_res = stats.read();
        sched_stats = &read_res.stats;
        expect_eq!(ThreadStateKind::Running as u8, sched_stats.current_state);
        expect_ne!(0, sched_stats.state_change_ticks);
        expect_eq!(0, sched_stats.total_running_ticks);
        expect_le!(queue_ticks_lower_bound, sched_stats.total_ready_ticks);

        // Spend some time in this state and make sure that only the compensated
        // stat for the state we are in is advancing.
        delay_ticks(CPU_TICKS);
        trs = stats.get_compensated_task_runtime_stats();
        expect_le!(CPU_TICKS, trs.cpu_ticks);
        expect_eq!(sched_stats.total_ready_ticks, trs.queue_ticks);
        expect_eq!(0, trs.page_fault_ticks);
        expect_eq!(0, trs.lock_contention_ticks);

        // Add other times.
        const PAGE_FAULT_TICKS: zx_duration_mono_ticks_t = 30;
        const LOCK_CONTENTION_TICKS: zx_duration_mono_ticks_t = 40;
        let prev_trs: TaskRuntimeStats = trs;

        stats.add_page_fault_ticks(PAGE_FAULT_TICKS);
        trs = stats.get_compensated_task_runtime_stats();
        expect_le!(prev_trs.cpu_ticks, trs.cpu_ticks);
        expect_eq!(prev_trs.queue_ticks, trs.queue_ticks);
        expect_eq!(PAGE_FAULT_TICKS, trs.page_fault_ticks);
        expect_eq!(0, trs.lock_contention_ticks);

        stats.add_lock_contention_ticks(LOCK_CONTENTION_TICKS);
        trs = stats.get_compensated_task_runtime_stats();
        expect_le!(prev_trs.cpu_ticks, trs.cpu_ticks);
        expect_eq!(prev_trs.queue_ticks, trs.queue_ticks);
        expect_eq!(PAGE_FAULT_TICKS, trs.page_fault_ticks);
        expect_eq!(LOCK_CONTENTION_TICKS, trs.lock_contention_ticks);
    }
}
