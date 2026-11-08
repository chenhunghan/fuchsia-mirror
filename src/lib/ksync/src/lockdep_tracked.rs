// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::raw_lock::{
    ConstRawLock, ExclusiveLockPolicy, LockPolicy, RawAssertHeldLock, RawLock, RawSharedLock,
};
use core::ffi::c_void;
use core::marker::PhantomData;
use pin_init::{PinInit, pin_data, pin_init};

#[cfg(feature = "lock_dep")]
unsafe extern "C" {
    fn cpp_lock_dep_acquire(
        lock: *mut c_void,
        class_id: *const c_void,
        entry_storage: *mut c_void,
        flags: lockdep::LockFlags,
        order: usize,
        assert_nestable: bool,
    );
    fn cpp_lock_dep_release(entry_storage: *mut c_void, flags: lockdep::LockFlags);
    fn cpp_lock_validate_release(entry_storage: *mut c_void, flags: lockdep::LockFlags);
    fn cpp_lock_validate_acquire(entry_storage: *mut c_void, flags: lockdep::LockFlags);
}

/// Opaque stack entry storage for a lockdep `AcquiredLockEntry`.
///
/// C++ lockdep owns the contents between [`lockdep_acquire`] and the matching
/// [`lockdep_release`] (the entry is linked into the thread's held-lock list and mutated by the
/// validator), so Rust only ever hands out its address and never reads or writes the bytes.
#[cfg(feature = "lock_dep")]
#[derive(Default)]
#[repr(C, align(8))]
struct LockEntryStorage(zr::OpaqueBytes<40>);

/// Records the acquisition of `lock` with lockdep, initializing `entry` with the given `order`.
///
/// `assert_nestable` is set by ordered acquisitions ([`OrderedLockPolicy`]) to have C++
/// debug-assert that the lock's class was registered as nestable, mirroring the C++
/// `Guard(AssertOrderedLock, ...)` constructor; it does not affect how the acquisition is
/// validated or recorded.
///
/// # Safety
///
/// `lock` must be initialized, `entry` must not currently hold a live `AcquiredLockEntry` (it
/// must be freshly `Default`-initialized or have been destroyed by a prior [`lockdep_release`]),
/// and the caller must guarantee that `entry` remains pinned on the stack until the matching
/// [`lockdep_release`] completes.
#[cfg(feature = "lock_dep")]
#[inline]
unsafe fn lockdep_acquire<L: RawLock, LP, const LOCK_FLAGS: lockdep::LockFlags>(
    lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
    entry: &LockEntryStorage,
    order: usize,
    assert_nestable: bool,
) {
    // SAFETY: `lock.class_id` is null or a valid `LockClassId` (`RawLock::init`'s `# Safety`,
    // `const_init`, or the C++ `Lock<>` the lock mirrors; note that `LockClass` and `const_init`
    // are still safe until the follow-up making `LockClass` an `unsafe trait` and `const_init`
    // `unsafe` lands); `entry` holds no live entry and stays pinned on the stack for the duration
    // of the lock hold per this function's `# Safety`.
    unsafe {
        cpp_lock_dep_acquire(
            lock.lock.as_mut_ptr(),
            lock.class_id,
            entry.0.get().cast(),
            LOCK_FLAGS,
            order,
            assert_nestable,
        );
    }
}

/// Records the release of the lock tracked by `entry` with lockdep and destroys the entry.
///
/// # Safety
///
/// `entry` must have been initialized by [`lockdep_acquire`], the current thread must hold the
/// corresponding lock, and `flags` must equal the `LOCK_FLAGS` of that acquisition (it selects
/// the `ThreadLockState` the entry is linked into).
#[cfg(feature = "lock_dep")]
#[inline]
unsafe fn lockdep_release(entry: &LockEntryStorage, flags: lockdep::LockFlags) {
    // SAFETY: `entry` holds a live entry acquired with `flags` on the current thread per this
    // function's `# Safety`.
    unsafe {
        cpp_lock_dep_release(entry.0.get().cast(), flags);
    }
}

/// Temporarily removes the lock tracked by `entry` from lockdep's held-lock set without
/// destroying the entry (see [`LockPolicy::untrack`]).
///
/// # Safety
///
/// `entry` must have been initialized by [`lockdep_acquire`] and currently be tracked as held,
/// and `flags` must equal the `LOCK_FLAGS` of that acquisition.
#[cfg(feature = "lock_dep")]
#[inline]
unsafe fn lockdep_untrack(entry: &LockEntryStorage, flags: lockdep::LockFlags) {
    // SAFETY: `entry` holds a live, tracked entry acquired with `flags` on the current thread per
    // this function's `# Safety`.
    unsafe {
        cpp_lock_validate_release(entry.0.get().cast(), flags);
    }
}

/// Re-adds the lock tracked by `entry` to lockdep's held-lock set after [`lockdep_untrack`]
/// (see [`LockPolicy::retrack`]). The entry retains the `order` it was acquired with.
///
/// # Safety
///
/// `entry` must have been previously untracked by [`lockdep_untrack`] while held, and `flags`
/// must equal the `LOCK_FLAGS` of the original acquisition.
#[cfg(feature = "lock_dep")]
#[inline]
unsafe fn lockdep_retrack(entry: &LockEntryStorage, flags: lockdep::LockFlags) {
    // SAFETY: `entry` holds a live, currently untracked entry acquired with `flags` on the
    // current thread per this function's `# Safety`.
    unsafe {
        cpp_lock_validate_acquire(entry.0.get().cast(), flags);
    }
}

/// Guard state for a [`LockdepTracked`] lock acquisition, combining lockdep's stack entry storage
/// with the underlying policy's `GuardState`.
#[derive(Default)]
#[repr(C)]
pub struct LockdepTrackedGuardState<S> {
    #[cfg(feature = "lock_dep")]
    entry: LockEntryStorage,
    state: S,
}

/// Wraps a [`RawLock`] and [`LockPolicy`] with optional lockdep tracking state.
#[pin_data]
#[repr(C)]
pub struct LockdepTracked<
    L: RawLock,
    P = <L as RawLock>::DefaultPolicy,
    const LOCK_FLAGS: lockdep::LockFlags = { lockdep::LOCK_FLAGS_NONE },
> {
    #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
    class_id: *const c_void,
    #[pin]
    lock: L,
    _policy: PhantomData<P>,
}

impl<L: RawLock, P, const LOCK_FLAGS: lockdep::LockFlags> LockdepTracked<L, P, LOCK_FLAGS> {
    /// Flags specifying validation rules for the lock class.
    pub const LOCK_FLAGS: lockdep::LockFlags = LOCK_FLAGS;
}

impl<L: ConstRawLock, P, const LOCK_FLAGS: lockdep::LockFlags> LockdepTracked<L, P, LOCK_FLAGS> {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init(core::ptr::null());

    /// Statically initializes a `LockdepTracked` lock in constant context.
    pub const fn const_init(_class_id: *const c_void) -> Self {
        Self {
            #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
            class_id: _class_id,
            lock: L::INIT,
            _policy: PhantomData,
        }
    }
}

impl<L: ConstRawLock, P, const LOCK_FLAGS: lockdep::LockFlags> Default
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    fn default() -> Self {
        Self::INIT
    }
}

impl<L: RawLock, P, const LOCK_FLAGS: lockdep::LockFlags> core::ops::Deref
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    type Target = L;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.lock
    }
}

impl<L: RawLock, P, const LOCK_FLAGS: lockdep::LockFlags> AsRef<L>
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    #[inline]
    fn as_ref(&self) -> &L {
        &self.lock
    }
}

// SAFETY: `LockdepTracked` is safe to share across threads when the underlying lock `L` is `Sync`.
unsafe impl<L: RawLock + Sync, P, const LOCK_FLAGS: lockdep::LockFlags> Sync
    for LockdepTracked<L, P, LOCK_FLAGS>
{
}
// SAFETY: `LockdepTracked` is safe to send across threads when the underlying lock `L` is `Send`.
unsafe impl<L: RawLock + Send, P, const LOCK_FLAGS: lockdep::LockFlags> Send
    for LockdepTracked<L, P, LOCK_FLAGS>
{
}

// SAFETY: `LockdepTracked` delegates mutual exclusion to `P` (which implements `LockPolicy<L>`)
// while recording lockdep validation state on acquire and release.
unsafe impl<
    L: RawLock,
    P: LockPolicy<L>,
    LP: ExclusiveLockPolicy<L>,
    const LOCK_FLAGS: lockdep::LockFlags,
> LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for LockdepTracked<L, P, LOCK_FLAGS>
{
    type AcquireArgs = P::AcquireArgs;
    type GuardState = LockdepTrackedGuardState<P::GuardState>;
    const EXTERNAL_ORDER: bool = P::EXTERNAL_ORDER;

    #[inline]
    unsafe fn pre_validate(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_validate` contract.
        unsafe {
            P::pre_validate(&lock.lock, &mut state.state, args);
        }
    }

    #[inline]
    unsafe fn pre_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_reacquire` contract.
        unsafe {
            P::pre_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe {
            Self::pre_validate(lock, state, args);
            Self::post_validate_acquire(lock, state, args);
        }
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        args: Self::AcquireArgs,
    ) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: `lock` is initialized, `state.entry` is freshly `Default`-initialized per the
        // `LockPolicy::acquire` contract, and the caller guarantees `state` is pinned on the stack
        // for the duration of the lock hold.
        unsafe {
            lockdep_acquire(lock, &state.entry, 0, false);
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_acquire` contract.
        unsafe {
            P::post_validate_acquire(&lock.lock, &mut state.state, args);
        }
    }

    #[inline]
    unsafe fn reacquire(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::pre_reacquire(lock, state);
            Self::post_validate_reacquire(lock, state);
        }
    }

    #[inline]
    unsafe fn post_validate_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: `lock` is initialized, `state.entry` was destroyed by the `release` that
        // precedes a `reacquire`, and the caller guarantees `state` is pinned on the stack for
        // the duration of the lock hold.
        unsafe {
            lockdep_acquire(lock, &state.entry, 0, false);
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_reacquire` contract.
        unsafe {
            P::post_validate_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn untrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was initialized by `acquire` (with `LOCK_FLAGS`) and
        // is currently held.
        unsafe {
            lockdep_untrack(&state.entry, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::untrack` contract.
        unsafe {
            P::untrack(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn retrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::retrack` contract.
        unsafe {
            P::retrack(&lock.lock, &mut state.state);
        }
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was previously untracked by `untrack` while held;
        // `LOCK_FLAGS` matches the original acquisition.
        unsafe {
            lockdep_retrack(&state.entry, LOCK_FLAGS);
        }
    }

    #[inline]
    unsafe fn release(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: The caller guarantees `state` matches the slot initialized in `acquire` (with
        // `LOCK_FLAGS`) and that the current thread holds the lock.
        unsafe {
            lockdep_release(&state.entry, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::release` contract.
        unsafe {
            P::release(&lock.lock, &mut state.state);
        }
    }
}

// SAFETY: `LockdepTracked` delegates lock acquisition and release to `P`, which guarantees
// exclusive mutual exclusion on `L`. Requiring the target lock's default policy to match `P`
// prevents `LockdepTracked<L, P, LOCK_FLAGS>` from bypassing policy restrictions on a
// `LockdepTracked<L, LP, LOCK_FLAGS>` whose `LP` carries additional invariants (such as
// `TrackedCriticalMutex`).
unsafe impl<L: RawLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags>
    ExclusiveLockPolicy<LockdepTracked<L, P, LOCK_FLAGS>> for LockdepTracked<L, P, LOCK_FLAGS>
{
}

/// Guard state for an ordered [`LockdepTracked`] lock acquisition, combining lockdep's stack
/// entry storage and saved lockdep order with the underlying policy's `GuardState`.
#[derive(Default)]
#[repr(C)]
pub struct OrderedGuardState<S> {
    #[cfg(feature = "lock_dep")]
    entry: LockEntryStorage,
    #[cfg(feature = "lock_dep")]
    order: usize,
    state: S,
}

/// Wraps a [`LockPolicy`] `P` (where `P::AcquireArgs = ()`) to acquire a [`LockdepTracked`] lock
/// with an explicit external lockdep order (`usize`).
///
/// `P` is the *inner* policy on the raw lock `L` (e.g. `OrderedLockPolicy<CriticalMutexPolicy>`,
/// aliased as `CriticalMutexOrderedPolicy`), not the `LockdepTracked` wrapper itself.
///
/// The lock's class must be registered with [`LOCK_FLAGS_NESTABLE`](crate::LOCK_FLAGS_NESTABLE),
/// which is checked at compile time against [`LockClass::FLAGS`](lockdep::LockClass::FLAGS) by
/// the `ksync` guards and debug-asserted at runtime against the registered class by lockdep. The
/// order is preserved (in [`OrderedGuardState`]) across `call_unlocked` so that the
/// re-acquisition is validated at the same position in the nesting, and retained by lockdep
/// across `call_untracked`.
pub struct OrderedLockPolicy<P>(PhantomData<P>);

// SAFETY: `OrderedLockPolicy<P>` delegates mutual exclusion to `P` (which implements
// `LockPolicy<L, AcquireArgs = ()>`) while recording lockdep validation state with the caller's
// explicit `order` on acquire and preserving `order` across `reacquire`.
unsafe impl<
    L: RawLock,
    P: LockPolicy<L, AcquireArgs = ()>,
    LP: ExclusiveLockPolicy<L>,
    const LOCK_FLAGS: lockdep::LockFlags,
> LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for OrderedLockPolicy<P>
{
    type AcquireArgs = usize;
    type GuardState = OrderedGuardState<P::GuardState>;
    const EXTERNAL_ORDER: bool = true;

    #[inline]
    unsafe fn pre_validate(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        _order: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_validate` contract.
        unsafe {
            P::pre_validate(&lock.lock, &mut state.state, ());
        }
    }

    #[inline]
    unsafe fn pre_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::pre_reacquire` contract.
        unsafe {
            P::pre_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        order: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        unsafe {
            Self::pre_validate(lock, state, order);
            Self::post_validate_acquire(lock, state, order);
        }
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
        #[cfg_attr(not(feature = "lock_dep"), allow(unused_variables))] order: Self::AcquireArgs,
    ) {
        #[cfg(feature = "lock_dep")]
        {
            state.order = order;
            // SAFETY: `lock` is initialized, `state.entry` is freshly `Default`-initialized per
            // the `LockPolicy::acquire` contract, and the caller guarantees `state` is pinned on
            // the stack for the duration of the lock hold.
            unsafe {
                lockdep_acquire(lock, &state.entry, order, true);
            }
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_acquire` contract.
        unsafe {
            P::post_validate_acquire(&lock.lock, &mut state.state, ());
        }
    }

    #[inline]
    unsafe fn reacquire(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::pre_reacquire(lock, state);
            Self::post_validate_reacquire(lock, state);
        }
    }

    #[inline]
    unsafe fn post_validate_reacquire(
        lock: &LockdepTracked<L, LP, LOCK_FLAGS>,
        state: &mut Self::GuardState,
    ) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: `lock` is initialized, `state.entry` was destroyed by the `release` that
        // precedes a `reacquire`, and the caller guarantees `state` is pinned on the stack for
        // the duration of the lock hold.
        unsafe {
            lockdep_acquire(lock, &state.entry, state.order, true);
        }
        // SAFETY: Caller upholds the `LockPolicy::post_validate_reacquire` contract.
        unsafe {
            P::post_validate_reacquire(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn untrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was initialized by `acquire` (with `LOCK_FLAGS`) and
        // is currently held.
        unsafe {
            lockdep_untrack(&state.entry, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::untrack` contract.
        unsafe {
            P::untrack(&lock.lock, &mut state.state);
        }
    }

    #[inline]
    unsafe fn retrack(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::retrack` contract.
        unsafe {
            P::retrack(&lock.lock, &mut state.state);
        }
        #[cfg(feature = "lock_dep")]
        // SAFETY: Caller guarantees `state` was previously untracked by `untrack` while held;
        // `LOCK_FLAGS` matches the original acquisition.
        unsafe {
            lockdep_retrack(&state.entry, LOCK_FLAGS);
        }
    }

    #[inline]
    unsafe fn release(lock: &LockdepTracked<L, LP, LOCK_FLAGS>, state: &mut Self::GuardState) {
        #[cfg(feature = "lock_dep")]
        // SAFETY: The caller guarantees `state` matches the slot initialized in `acquire` (with
        // `LOCK_FLAGS`) and that the current thread holds the lock.
        unsafe {
            lockdep_release(&state.entry, LOCK_FLAGS);
        }
        // SAFETY: Caller upholds the `LockPolicy::release` contract.
        unsafe {
            P::release(&lock.lock, &mut state.state);
        }
    }
}

// SAFETY: `OrderedLockPolicy<P>` delegates lock acquisition and release to `P`, which guarantees
// exclusive mutual exclusion on `L`.
unsafe impl<
    L: RawLock,
    P: ExclusiveLockPolicy<L, AcquireArgs = ()>,
    LP: ExclusiveLockPolicy<L>,
    const LOCK_FLAGS: lockdep::LockFlags,
> ExclusiveLockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>> for OrderedLockPolicy<P>
{
}

/// Implements `LockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>>` and
/// `ExclusiveLockPolicy<LockdepTracked<L, LP, LOCK_FLAGS>>` for a concrete policy type `$policy`
/// by delegating to `LockdepTracked<L, $policy, LOCK_FLAGS>`.
///
/// * `impl_lockdep_tracked_lock_policy!($policy)` implements the traits for every `L` and `LP`
///   where `$policy: LockPolicy<L>`.
/// * `impl_lockdep_tracked_lock_policy!($policy, $lock, $default_policy)` only implements the
///   traits for `L = $lock` and `LP = $default_policy`. Use this for a policy that is valid for
///   `$lock` but would bypass the semantics of other default policies on the same `$lock` (for
///   example, acquiring a `CriticalMutex` as a plain `Mutex`).
///
/// This is exported so that raw lock implementations outside of this crate (such as the kernel's
/// `Mutex`) can implement their policies for `LockdepTracked`.
#[cfg(feature = "kernel")]
#[macro_export]
macro_rules! impl_lockdep_tracked_lock_policy {
    (@impl $policy:ty, [$($generics:tt)*], $l:ty, $lp:ty) => {
        // SAFETY: Delegates to `LockdepTracked<$l, $policy, LOCK_FLAGS>`, which performs lockdep
        // validation and forwards mutual exclusion to `$policy`.
        unsafe impl<$($generics)* const LOCK_FLAGS: $crate::LockFlags>
            $crate::LockPolicy<$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>> for $policy
        where
            $policy: $crate::LockPolicy<$l>,
        {
            type AcquireArgs = <$policy as $crate::LockPolicy<$l>>::AcquireArgs;
            type GuardState =
                $crate::LockdepTrackedGuardState<<$policy as $crate::LockPolicy<$l>>::GuardState>;
            const EXTERNAL_ORDER: bool = <$policy as $crate::LockPolicy<$l>>::EXTERNAL_ORDER;

            #[inline]
            unsafe fn pre_validate(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::pre_validate` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::pre_validate(lock, state, args);
                }
            }

            #[inline]
            unsafe fn pre_reacquire(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::pre_reacquire` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::pre_reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn acquire(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::acquire(lock, state, args);
                }
            }

            #[inline]
            unsafe fn post_validate_acquire(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
                args: Self::AcquireArgs,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::post_validate_acquire` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::post_validate_acquire(lock, state, args);
                }
            }

            #[inline]
            unsafe fn reacquire(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn post_validate_reacquire(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::post_validate_reacquire` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::post_validate_reacquire(lock, state);
                }
            }

            #[inline]
            unsafe fn untrack(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::untrack` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::untrack(lock, state);
                }
            }

            #[inline]
            unsafe fn retrack(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::retrack` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::retrack(lock, state);
                }
            }

            #[inline]
            unsafe fn release(
                lock: &$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                state: &mut Self::GuardState,
            ) {
                // SAFETY: Caller upholds the `LockPolicy::release` contract.
                unsafe {
                    <$crate::LockdepTracked<$l, $policy, LOCK_FLAGS> as $crate::LockPolicy<
                        $crate::LockdepTracked<$l, $lp, LOCK_FLAGS>,
                    >>::release(lock, state);
                }
            }
        }

        // SAFETY: Delegates to `LockdepTracked<$l, $policy, LOCK_FLAGS>`, and `$policy` guarantees
        // exclusive mutual exclusion on `$l`.
        unsafe impl<$($generics)* const LOCK_FLAGS: $crate::LockFlags>
            $crate::ExclusiveLockPolicy<$crate::LockdepTracked<$l, $lp, LOCK_FLAGS>> for $policy
        where
            $policy: $crate::ExclusiveLockPolicy<$l>,
        {
        }
    };
    ($policy:ty $(,)?) => {
        $crate::impl_lockdep_tracked_lock_policy!(
            @impl $policy, [L: $crate::RawLock, LP: $crate::ExclusiveLockPolicy<L>,], L, LP
        );
    };
    ($policy:ty, $lock:ty, $default_policy:ty $(,)?) => {
        $crate::impl_lockdep_tracked_lock_policy!(
            @impl $policy, [], $lock, $default_policy
        );
    };
}

// SAFETY: `LockdepTracked` wraps physical lock `L` and forwards `as_mut_ptr` to uniquely identify
// it.
unsafe impl<L: RawLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags> RawLock
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    const LOCK_FLAGS: lockdep::LockFlags = LOCK_FLAGS;
    type DefaultPolicy = Self;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: The caller guarantees `class_id` is null or a valid static `LockClassId`.
            lock <- unsafe { L::init(class_id) },
            #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
            class_id,
            _policy: PhantomData,
        })
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self.lock.as_mut_ptr()
    }
}

impl<L: ConstRawLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags> ConstRawLock
    for LockdepTracked<L, P, LOCK_FLAGS>
{
    const INIT: Self = Self::INIT;
}

// SAFETY: `LockdepTracked<L, L::SharedPolicy, LOCK_FLAGS>` delegates mutual exclusion to
// `L::SharedPolicy`, which `L: RawSharedLock` guarantees is a shared mode excluding all of `L`'s
// exclusive policies. Every `ExclusiveLockPolicy` for `LockdepTracked<L, P, LOCK_FLAGS>` (`P`
// itself, and any policy implemented via `impl_lockdep_tracked_lock_policy!`) delegates to an
// `ExclusiveLockPolicy` of `L`, so it is excluded by `SharedPolicy` as well.
unsafe impl<L: RawSharedLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags>
    RawSharedLock for LockdepTracked<L, P, LOCK_FLAGS>
{
    type SharedPolicy = LockdepTracked<L, L::SharedPolicy, LOCK_FLAGS>;
}

impl<L: RawAssertHeldLock, P: ExclusiveLockPolicy<L>, const LOCK_FLAGS: lockdep::LockFlags>
    RawAssertHeldLock for LockdepTracked<L, P, LOCK_FLAGS>
{
    #[inline]
    fn assert_held(&self) {
        self.lock.assert_held();
    }
}
