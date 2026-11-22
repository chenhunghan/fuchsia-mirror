// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Types and structures for kernel scheduler state (`include/kernel/scheduler_state.h`).

use crate::kernel::scheduler::{BaseProfile, SchedDeadlineParams, SchedDuration};
use crate::kernel::types::{cpu_mask_t, cpu_num_t};
use crate::platform_rs::timer::{DurationMono, InstantMono};
use object_constants_rs as object_constants;

/// The scheduling state of a thread matching C++ `enum thread_state : uint8_t`.
// LINT.IfChange(ThreadStateKind)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadStateKind {
    /// Thread has been created but not yet made runnable.
    Initial = 0,
    /// Thread is ready to run and in a run queue.
    Ready = 1,
    /// Thread is currently running on a CPU.
    Running = 2,
    /// Thread is blocked waiting on an event, mutex, or condition.
    Blocked = 3,
    /// Thread is blocked on a read lock.
    BlockedReadLock = 4,
    /// Thread is sleeping.
    Sleeping = 5,
    /// Thread is suspended.
    Suspended = 6,
    /// Thread is dead and awaiting cleanup.
    Death = 7,
}

zr::static_assert!(core::mem::size_of::<ThreadStateKind>() == 1);
zr::static_assert!(core::mem::align_of::<ThreadStateKind>() == 1);

impl TryFrom<u8> for ThreadStateKind {
    type Error = u8;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Initial),
            1 => Ok(Self::Ready),
            2 => Ok(Self::Running),
            3 => Ok(Self::Blocked),
            4 => Ok(Self::BlockedReadLock),
            5 => Ok(Self::Sleeping),
            6 => Ok(Self::Suspended),
            7 => Ok(Self::Death),
            other => Err(other),
        }
    }
}

// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:thread_state)

/// Facade for the C++ `SchedulerState` class.
///
/// # Safety Invariant
///
/// Only shared references to `SchedulerState` are formed, and any `&SchedulerState` must point to
/// a valid, initialized C++ `SchedulerState` instance and be formed in one of two ways:
/// - With the owning [`Thread`](crate::kernel::thread::Thread)'s lock held for the lifetime of the
///   reference. Every accessor may be used on such a reference (subject to its own `# Safety`).
/// - For a thread blocked in a wait queue, with that wait queue's lock held for the lifetime of
///   the reference (the WAVL tree hooks of the queue's collection). A blocked thread's
///   `effective_profile` is only written with both locks held, so this excludes writers of
///   `effective_profile` but not of the other fields: such a reference may only be used with the
///   `effective_profile_*` accessors and must not reach code that uses any other accessor.
///
/// [`Self::curr_cpu`], [`Self::last_cpu`], and [`Self::hard_affinity`] are safe functions that rely
/// on the first form. The restriction on the second form is upheld by the code that forms such
/// references (callers of `Thread::scheduler_state_unchecked`), not by the type system.
#[repr(transparent)]
pub struct SchedulerState {
    // Safety invariant: `_storage` is private so safe code cannot construct a `SchedulerState` by
    // value; any reference points to a live, initialized `SchedulerState` and is formed as the
    // type's safety invariant describes. Spanning the whole mirrored struct gives a
    // `&SchedulerState` provenance over every field the accessors below read, and `Opaque` marks
    // `SchedulerState` as `!Freeze` and `!Unpin`, since the scheduler mutates the fields under its
    // own locks.
    _storage: zr::Opaque<crate::kernel::scheduler::SchedulerState>,
}

impl SchedulerState {
    /// Raw pointer to the mirrored `crate::kernel::scheduler::SchedulerState` this facade stands
    /// for.
    ///
    /// Never dereferenced into a reference to the whole struct: its fields are guarded by
    /// different locks (see `Thread::scheduler_state`), so each accessor below projects only the
    /// field it reads.
    #[inline]
    fn raw(&self) -> *const crate::kernel::scheduler::SchedulerState {
        self._storage.get().cast_const()
    }

    /// Returns the CPU the thread is currently assigned to, or `INVALID_CPU` if it is not
    /// currently assigned to a CPU.
    ///
    /// Requires a reference formed with the owning thread's lock held (see the type's safety
    /// invariant).
    #[inline]
    pub fn curr_cpu(&self) -> cpu_num_t {
        // SAFETY: `self.raw()` points to the underlying `SchedulerState` that `self` spans, and
        // `&raw const` projects the `curr_cpu` field place without forming a reference to the
        // whole struct. By `SchedulerState`'s safety invariant, a reference used with this
        // accessor was formed with the owning thread's lock held, and `curr_cpu` is only written
        // with that lock held exclusively, so the read does not race with a write.
        unsafe { (&raw const (*self.raw()).curr_cpu).read() }
    }

    /// Returns the CPU the thread last ran on, or `INVALID_CPU` if it has never run.
    ///
    /// Requires a reference formed with the owning thread's lock held (see the type's safety
    /// invariant).
    #[inline]
    pub fn last_cpu(&self) -> cpu_num_t {
        // SAFETY: As in `curr_cpu`; `last_cpu` is likewise only written with the owning thread's
        // lock held exclusively.
        unsafe { (&raw const (*self.raw()).last_cpu).read() }
    }

    /// Returns the hard CPU affinity mask of the thread.
    ///
    /// Requires a reference formed with the owning thread's lock held (see the type's safety
    /// invariant).
    #[inline]
    pub fn hard_affinity(&self) -> cpu_mask_t {
        // SAFETY: By `SchedulerState`'s safety invariant, a reference used with this accessor was
        // formed with the owning thread's lock held, which guards `hard_affinity`, and only that
        // field place is projected.
        unsafe { (&raw const (*self.raw()).hard_affinity).read() }
    }

    /// Raw place of the `effective_profile` field of this scheduler state without forming a
    /// reference to the whole `crate::kernel::scheduler::SchedulerState`.
    #[inline]
    fn effective_profile_ptr(&self) -> *const crate::kernel::scheduler::EffectiveProfile {
        // SAFETY: `self.raw()` points to the underlying `SchedulerState` that `self` spans, and
        // `&raw const` projects the `effective_profile` field place without forming a reference
        // to the whole struct.
        unsafe { &raw const (*self.raw()).effective_profile }
    }

    /// Returns whether the effective profile is a deadline profile
    /// (`effective_profile().IsDeadline()`).
    #[inline]
    pub fn effective_profile_is_deadline(&self) -> bool {
        // SAFETY: Either form of reference in `SchedulerState`'s safety invariant excludes
        // concurrent mutation of `effective_profile` (the owning thread's lock, or the lock of the
        // wait queue the thread is blocked in), and only the `effective_profile` field is
        // referenced.
        unsafe { (*self.effective_profile_ptr()).is_deadline() }
    }

    /// Returns the relative deadline of the effective profile
    /// (`effective_profile().deadline().deadline_ns`); only meaningful when
    /// [`Self::effective_profile_is_deadline`] is `true`.
    ///
    /// # Panics
    ///
    /// Debug builds will panic (`debug_assert!`) if the effective profile is not a deadline
    /// profile. Callers must check [`Self::effective_profile_is_deadline`] first.
    #[inline]
    pub fn effective_profile_deadline_ns(&self) -> DurationMono {
        // SAFETY: As in `effective_profile_is_deadline`.
        let deadline = unsafe { (*self.effective_profile_ptr()).deadline() };
        DurationMono(deadline.deadline_ns.raw_value())
    }

    /// Returns the virtual finish time of the thread (`finish_time()`).
    ///
    /// # Safety
    ///
    /// `self` must have been formed with the owning thread's lock held (see the type's safety
    /// invariant), and the thread must not be in a scheduler run queue, or the caller must also
    /// hold that scheduler's `queue_lock_`: while the thread is enqueued, `finish_time` is written
    /// under `queue_lock_` rather than the thread's lock.
    #[inline]
    pub unsafe fn finish_time(&self) -> InstantMono {
        // SAFETY: The thread's lock is held, and the thread is not in a run queue or its
        // scheduler's `queue_lock_` is held (caller's contract), so nothing writes `finish_time`
        // concurrently; only that field place is projected.
        InstantMono(unsafe { (&raw const (*self.raw()).finish_time).read() }.raw_value())
    }
}

/// Wrapper around [`BaseProfile`] matching C++ `SchedulerState::BaseProfile`.
#[repr(transparent)]
pub struct SchedulerStateBaseProfile(BaseProfile);

zr::static_assert_size_and_align!(
    SchedulerStateBaseProfile,
    object_constants::kSchedulerStateBaseProfileSize,
    object_constants::kSchedulerStateBaseProfileAlign,
);

impl core::ops::Deref for SchedulerStateBaseProfile {
    type Target = BaseProfile;

    /// Returns the wrapped [`BaseProfile`].
    #[inline]
    fn deref(&self) -> &BaseProfile {
        &self.0
    }
}

// thread priority
/// Number of priority levels.
pub const NUM_PRIORITIES: i32 = 32;
/// Lowest priority level.
pub const LOWEST_PRIORITY: i32 = 0;
/// Highest priority level.
pub const HIGHEST_PRIORITY: i32 = NUM_PRIORITIES - 1;
/// Priority level for DPC threads.
pub const DPC_PRIORITY: i32 = NUM_PRIORITIES - 2;
/// Priority level for idle threads.
pub const IDLE_PRIORITY: i32 = LOWEST_PRIORITY;
/// Low priority level.
pub const LOW_PRIORITY: i32 = NUM_PRIORITIES / 4;
/// Default thread priority level.
pub const DEFAULT_PRIORITY: i32 = NUM_PRIORITIES / 2;
/// High priority level.
pub const HIGH_PRIORITY: i32 = (NUM_PRIORITIES / 4) * 3;

impl SchedulerStateBaseProfile {
    /// Creates a fair `SchedulerStateBaseProfile` with the given priority and inheritable flag.
    ///
    /// # Panics
    ///
    /// Panics if `priority` is outside `LOWEST_PRIORITY..=HIGHEST_PRIORITY`.
    pub fn fair(priority: i32, inheritable: bool) -> Self {
        assert!((LOWEST_PRIORITY..=HIGHEST_PRIORITY).contains(&priority));
        let weight = crate::kernel::scheduler::SchedulerState::convert_priority_to_weight(priority);
        Self(BaseProfile::new_fair_with_weight(weight, inheritable))
    }

    /// Creates a deadline `SchedulerStateBaseProfile` with the given capacity and deadline.
    ///
    /// # Panics
    ///
    /// Panics if `capacity.0 <= 0` or `capacity.0 > deadline.0`.
    pub fn deadline(capacity: DurationMono, deadline: DurationMono) -> Self {
        Self::deadline_with_critical(capacity, deadline, false)
    }

    /// Creates a deadline `SchedulerStateBaseProfile` with the given capacity, deadline, and
    /// critical flag.
    ///
    /// # Panics
    ///
    /// Panics if `capacity.0 <= 0` or `capacity.0 > deadline.0`.
    pub fn deadline_with_critical(
        capacity: DurationMono,
        deadline: DurationMono,
        critical: bool,
    ) -> Self {
        assert!(capacity.0 > 0 && capacity.0 <= deadline.0);
        let capacity_ns = SchedDuration::from_raw(capacity.0);
        let deadline_ns = SchedDuration::from_raw(deadline.0);
        Self(BaseProfile::new_deadline(
            SchedDeadlineParams::new(capacity_ns, deadline_ns),
            critical,
        ))
    }
}
