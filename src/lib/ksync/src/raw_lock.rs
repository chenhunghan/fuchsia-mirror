// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use pin_init::PinInit;

/// Trait defining a policy for how a lock should be acquired and released. A given `RawLock` may
/// have multiple policy implementations allowing a specific instance of a lock to be acquired, at
/// different times, in different ways.
///
/// # Safety
///
/// Implementors must guarantee that:
/// 1. [`acquire`](Self::acquire) and [`reacquire`](Self::reacquire) establish synchronization
///    (shared or exclusive mutual exclusion) on `lock` until the matching
///    [`release`](Self::release) call completes.
/// 2. The split sequences ([`pre_validate`](Self::pre_validate) followed by
///    [`post_validate_acquire`](Self::post_validate_acquire), and
///    [`pre_reacquire`](Self::pre_reacquire) followed by
///    [`post_validate_reacquire`](Self::post_validate_reacquire)) establish the exact same
///    synchronization guarantees as [`acquire`](Self::acquire) and
///    [`reacquire`](Self::reacquire), respectively.
pub unsafe trait LockPolicy<L: RawLock> {
    /// Arguments passed when acquiring the lock under this policy.
    type AcquireArgs: Default + Copy;

    /// State stored in the pinned guard across lock acquisition and release.
    type GuardState: Default;

    /// Whether this policy acquires the lock with an explicit, caller-supplied lockdep order.
    ///
    /// # Lockdep validity
    ///
    /// This is a lockdep-validity requirement, not a soundness one: violating it degrades lock
    /// validation (or trips a debug assertion) but cannot cause memory unsafety.
    ///
    /// - Such policies require a [`lockdep::LOCK_FLAGS_NESTABLE`] lock class. The `ksync` guards
    ///   check this at compile time against [`LockClass::FLAGS`](lockdep::LockClass::FLAGS), and
    ///   `LockdepTracked` additionally has lockdep debug-assert it against the registered class
    ///   at runtime.
    /// - They cannot be used with `KMutexGuardMultiple`, which sorts its locks by address but
    ///   currently acquires each with lockdep order 0 (see its documentation).
    const EXTERNAL_ORDER: bool = false;

    /// Performs any pre-validation setup (such as disabling and saving interrupts) before
    /// lockdep validation runs.
    ///
    /// # Safety
    ///
    /// Any state changes made by `pre_validate` (such as disabling interrupts) must be paired with
    /// a subsequent [`post_validate_acquire`](Self::post_validate_acquire) and
    /// [`release`](Self::release) on the same thread.
    #[inline]
    unsafe fn pre_validate(_lock: &L, _state: &mut Self::GuardState, _args: Self::AcquireArgs) {}

    /// Re-runs pre-validation on an existing `GuardState` before re-acquiring the lock (e.g. in
    /// `call_unlocked`).
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`pre_validate`](Self::pre_validate).
    #[inline]
    unsafe fn pre_reacquire(_lock: &L, _state: &mut Self::GuardState) {}

    /// Acquires the raw synchronization lock.
    ///
    /// # Safety
    ///
    /// 1. `state` must reference a valid, exclusive, stack-pinned `GuardState` slot initialized
    ///    with `Self::GuardState::default()`.
    /// 2. The caller must ensure that `state` remains pinned on the stack and is not dropped or
    ///    moved until the matching [`release`](Self::release) call completes.
    ///
    /// See [`EXTERNAL_ORDER`](Self::EXTERNAL_ORDER) for the (non-safety) lockdep requirements of
    /// externally ordered policies.
    unsafe fn acquire(lock: &L, state: &mut Self::GuardState, args: Self::AcquireArgs);

    /// Acquires the raw synchronization lock after [`pre_validate`](Self::pre_validate) and
    /// lockdep validation have completed.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`acquire`](Self::acquire), after
    /// [`pre_validate`](Self::pre_validate) has run on `state`.
    #[inline]
    unsafe fn post_validate_acquire(
        lock: &L,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe {
            Self::acquire(lock, state, args);
        }
    }

    /// Re-acquires the raw synchronization lock using existing guard state (e.g. after
    /// `call_unlocked`).
    ///
    /// # Safety
    ///
    /// 1. `state` must reference the exact same pinned stack slot that was initialized by a prior
    ///    [`acquire`](Self::acquire) call and subsequently released by a matching
    ///    [`release`](Self::release) call.
    /// 2. The caller must ensure that `state` remains pinned on the stack and is not dropped or
    ///    moved until the matching [`release`](Self::release) call completes.
    unsafe fn reacquire(lock: &L, state: &mut Self::GuardState);

    /// Re-acquires the raw synchronization lock after [`pre_reacquire`](Self::pre_reacquire) and
    /// lockdep validation have completed.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`reacquire`](Self::reacquire), after
    /// [`pre_reacquire`](Self::pre_reacquire) has run on `state`.
    #[inline]
    unsafe fn post_validate_reacquire(lock: &L, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::reacquire(lock, state);
        }
    }

    /// Temporarily untracks the held lock in lockdep (used by `call_untracked`).
    ///
    /// # Safety
    ///
    /// `state` must be the active `GuardState` initialized by [`acquire`](Self::acquire) for a
    /// lock currently held by the calling thread, and must be paired with a matching
    /// [`retrack`](Self::retrack) call before the lock is released.
    #[inline]
    unsafe fn untrack(_lock: &L, _state: &mut Self::GuardState) {}

    /// Re-tracks the held lock in lockdep after [`untrack`](Self::untrack) (used by
    /// `call_untracked`).
    ///
    /// # Safety
    ///
    /// Must be called on the same `state` after a preceding [`untrack`](Self::untrack) call while
    /// the lock remains held by the calling thread.
    #[inline]
    unsafe fn retrack(_lock: &L, _state: &mut Self::GuardState) {}

    /// Releases the raw synchronization lock, restoring the state.
    ///
    /// # Safety
    ///
    /// 1. `state` must reference the exact same pinned stack slot passed to the corresponding
    ///    [`acquire`](Self::acquire) call.
    /// 2. The caller must guarantee that the current thread actually holds this lock (i.e. we are
    ///    releasing a lock we currently own).
    unsafe fn release(lock: &L, state: &mut Self::GuardState);
}

/// Marker subtrait for [`LockPolicy`] implementations that enforce exclusive mutual exclusion.
///
/// # Safety
///
/// In addition to [`LockPolicy`], implementors must guarantee exclusive mutual exclusion
/// (at most one thread may hold the lock at a time) between acquisition and
/// [`release`](LockPolicy::release).
pub unsafe trait ExclusiveLockPolicy<L: RawLock>: LockPolicy<L> {}

/// Trait defining a raw, un-instrumented synchronization lock abstraction.
///
/// Implementors of `RawLock` supply the platform-specific lock storage, in-place pinning
/// initialization logic, and raw synchronization entry points.
///
/// # Safety
///
/// Implementors must represent a physical synchronization primitive with non-zero size whose
/// [`as_mut_ptr`](Self::as_mut_ptr) uniquely identifies the underlying lock storage for the
/// lifetime of `&self`.
pub unsafe trait RawLock: Sized {
    /// Flags specifying validation rules for the lock class.
    const LOCK_FLAGS: lockdep::LockFlags = lockdep::LOCK_FLAGS_TRACKING_DISABLED;

    /// Default policy that should be used when acquiring this lock.
    type DefaultPolicy: ExclusiveLockPolicy<Self>;

    /// Returns a PinInit block to initialize the raw lock in-place.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `class_id` is either null or points to a valid,
    /// static `LockClassId` that remains valid for the lifetime of the lock.
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible>;

    /// Convert the raw lock reference to a standard c_void pointer for FFI.
    fn as_mut_ptr(&self) -> *mut c_void;
}

/// Trait for [`RawLock`] types that can also be acquired in shared (reader) mode, such as a
/// reader-writer lock. The lock's [`RawLock::DefaultPolicy`] acquires it in exclusive (writer)
/// mode.
///
/// # Safety
///
/// While any thread holds the lock through [`SharedPolicy`](Self::SharedPolicy), no thread may
/// hold it through an [`ExclusiveLockPolicy`] (in particular [`RawLock::DefaultPolicy`]), and vice
/// versa. Multiple threads may hold the lock through `SharedPolicy` concurrently.
pub unsafe trait RawSharedLock: RawLock {
    /// Policy acquiring the lock in shared (reader) mode.
    type SharedPolicy: LockPolicy<Self, AcquireArgs = ()>;
}

/// Trait for [`RawLock`] types that can be statically initialized in a `const` context (e.g. for
/// global singleton locks).
pub trait ConstRawLock: RawLock {
    /// Constant-initialized value for the lock.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self;
}

/// Trait for [`RawLock`] types that can assert (in debug builds) that the lock is currently
/// exclusively held by the caller.
pub trait RawAssertHeldLock: RawLock {
    /// Asserts (in debug builds) that the lock is currently exclusively held by the caller.
    fn assert_held(&self);
}

/// Trait for untracked [`RawLock`] types (not wrapped in [`LockdepTracked`](crate::LockdepTracked))
/// that support manual, guardless release paired with an untracked raw acquisition.
///
/// This must not be implemented for [`LockdepTracked`](crate::LockdepTracked), whose release
/// requires its per-acquisition lockdep guard state.
pub trait RawUntrackedLock: RawAssertHeldLock {
    /// Releases the lock without restoring any saved interrupt state.
    ///
    /// # Safety
    ///
    /// The caller must currently hold this lock exclusively without outstanding references
    /// protected by this acquisition, must satisfy any platform preconditions of the raw lock's
    /// non-interrupt-restoring release (such as interrupts being disabled for a spinlock), and
    /// must keep the allocation backing `self` live until this call returns.
    unsafe fn release(&self);
}

/// Trait for untracked interrupt-saving [`RawLock`] types (such as an untracked kernel spinlock)
/// that support manual, guardless release restoring saved interrupt state.
///
/// Like [`RawUntrackedLock`], this must not be implemented for
/// [`LockdepTracked`](crate::LockdepTracked).
pub trait RawUntrackedIrqLock: RawUntrackedLock {
    /// Saved interrupt state returned by an interrupt-saving raw acquisition.
    type InterruptState;

    /// Releases the lock and restores saved interrupt state `state`.
    ///
    /// # Safety
    ///
    /// The caller must currently hold this lock exclusively without outstanding references
    /// protected by this acquisition, `state` must be the [`Self::InterruptState`] returned by the
    /// corresponding interrupt-saving acquisition on this CPU, and the allocation backing `self`
    /// must remain live until this call returns.
    unsafe fn release_irq_restore(&self, state: Self::InterruptState);
}
