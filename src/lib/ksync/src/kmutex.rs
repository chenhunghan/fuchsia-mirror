// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    AdoptableLockPolicy, ExclusiveLockPolicy, LockPolicy, LockToken, RawAssertHeldLock, RawLock,
    RawUntrackedIrqLock, RawUntrackedLock, RawUntrackedStatefulLock, TryLockPolicy,
};
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::{NonNull, addr_of_mut};
use lockdep::LockClass;
use pin_init::{PinInit, pin_data, pin_init, pin_init_from_closure, pinned_drop};

/// A safe, Zircon-compatible mutual exclusion lock supporting compile-time order validation.
///
/// `KMutex` wraps a platform-specific `RawLock` abstraction. It is pinned in memory to support FFI
/// loop-detector active list registrations safely under the lock class `Class`.
#[repr(transparent)] // Ensure KMutex has the same layout as the underlying RawLock M.
#[pin_data]
pub struct KMutex<Class: LockClass, M> {
    #[pin]
    mutex: M,
    _marker: PhantomData<Class>,
}

impl<Class: LockClass, M> KMutex<Class, M> {
    /// Create a new KMutex with a pre-initialized raw lock.
    pub const fn new(mutex: M) -> Self {
        Self { mutex, _marker: PhantomData }
    }
}

impl<Class: LockClass, M: RawLock> KMutex<Class, M> {
    /// Safe dynamic initialization of the validation lock inside pin context.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: `Self::class_id()` is either null or a valid static `LockClassId`.
            mutex <- unsafe { M::init(Self::class_id()) },
            _marker: PhantomData,
        })
    }

    /// Acquires the lock, using the default policy, and registers the active loop node.
    #[inline]
    pub fn lock(&self) -> impl PinInit<KMutexGuard<'_, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        KMutexGuard::new(self)
    }

    /// Acquires the lock with arguments, using the default policy, and registers the active loop
    /// node.
    #[inline]
    pub fn lock_with(
        &self,
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexGuard<'_, Class, M>, core::convert::Infallible> {
        KMutexGuard::new_with_args(self, args)
    }

    /// Acquires the lock, using the specified policy, and registers the active loop node.
    #[inline]
    pub fn lock_policy<P: ExclusiveLockPolicy<M, AcquireArgs = ()>>(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, Class, M, P>, core::convert::Infallible> {
        KMutexGuard::new(self)
    }

    /// Acquires the lock with arguments, using the specified policy, and registers the active loop
    /// node.
    #[inline]
    pub fn lock_policy_with<P: ExclusiveLockPolicy<M>>(
        &self,
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexGuard<'_, Class, M, P>, core::convert::Infallible> {
        KMutexGuard::new_with_args(self, args)
    }

    /// Acquires the lock, using the default policy, into a pinned [`Adoptable`] guard.
    #[inline]
    pub fn lock_adoptable(&self) -> impl PinInit<Adoptable<'_, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: AdoptableLockPolicy<M, AcquireArgs = ()>,
    {
        Adoptable::new(self)
    }

    /// Acquires the lock with arguments, using the default policy, into a pinned [`Adoptable`]
    /// guard.
    #[inline]
    pub fn lock_adoptable_with(
        &self,
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<Adoptable<'_, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: AdoptableLockPolicy<M>,
    {
        Adoptable::new_with_args(self, args)
    }

    /// Acquires the lock, using the specified policy, into a pinned [`Adoptable`] guard.
    #[inline]
    pub fn lock_adoptable_policy<P: AdoptableLockPolicy<M, AcquireArgs = ()>>(
        &self,
    ) -> impl PinInit<Adoptable<'_, Class, M, P>, core::convert::Infallible> {
        Adoptable::new(self)
    }

    /// Acquires the lock with arguments, using the specified policy, into a pinned [`Adoptable`]
    /// guard.
    #[inline]
    pub fn lock_adoptable_policy_with<P: AdoptableLockPolicy<M>>(
        &self,
        args: P::AcquireArgs,
    ) -> impl PinInit<Adoptable<'_, Class, M, P>, core::convert::Infallible> {
        Adoptable::new_with_args(self, args)
    }

    /// Adopts a lock acquisition held by `adoptable` into a new pinned [`KMutexGuard`], moving the
    /// lock state out of `adoptable` and leaving `adoptable` empty.
    ///
    /// # Panics
    ///
    /// Panics if `adoptable` does not currently hold `self` (`!adoptable.is_locked_by(self)`).
    #[inline]
    pub fn adopt<'a, P: AdoptableLockPolicy<M>>(
        &'a self,
        adoptable: Pin<&mut Adoptable<'_, Class, M, P>>,
    ) -> impl PinInit<KMutexGuard<'a, Class, M, P>, core::convert::Infallible> {
        assert!(
            adoptable.is_locked_by(self),
            "cannot adopt an Adoptable that does not hold this KMutex"
        );
        // SAFETY: `adoptable.is_locked_by(self)` was just verified, so `adoptable` holds `self`
        // and the adopted guard's lifetime `'a` is bounded by `&'a self`.
        unsafe { KMutexGuard::adopt_unbounded(adoptable) }
    }

    /// Acquires this lock and aliases it with `alias`, returning a guard that proves ownership
    /// of both `Class` and `AliasClass`.
    #[inline]
    pub fn aliased_lock<'a, AliasClass: LockClass, M2: RawLock>(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        KMutexAliasedGuard::new(self, alias)
    }

    /// Acquires this lock with arguments and aliases it with `alias`, returning a guard that proves
    /// ownership of both `Class` and `AliasClass`.
    #[inline]
    pub fn aliased_lock_with<'a, AliasClass: LockClass, M2: RawLock>(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new_with_args(self, alias, args)
    }

    /// Acquires this lock with policy `P` and aliases it with `alias`.
    #[inline]
    pub fn aliased_lock_policy<
        'a,
        AliasClass: LockClass,
        M2: RawLock,
        P: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    >(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2, P>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new(self, alias)
    }

    /// Acquires this lock with policy `P` and arguments and aliases it with `alias`.
    #[inline]
    pub fn aliased_lock_policy_with<
        'a,
        AliasClass: LockClass,
        M2: RawLock,
        P: ExclusiveLockPolicy<M>,
    >(
        &'a self,
        alias: &'a KMutex<AliasClass, M2>,
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexAliasedGuard<'a, Class, AliasClass, M, M2, P>, core::convert::Infallible>
    {
        KMutexAliasedGuard::new_with_args(self, alias, args)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order,
    /// using the default policy.
    #[inline]
    pub fn lock_multiple<'a, const N: usize>(
        locks: [&'a Self; N],
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible>
    where
        <M as RawLock>::DefaultPolicy: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        KMutexGuardMultiple::new(locks)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order with
    /// arguments, using the default policy.
    #[inline]
    pub fn lock_multiple_with<'a, const N: usize>(
        locks: [&'a Self; N],
        args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible> {
        KMutexGuardMultiple::new_with_args(locks, args)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order,
    /// using policy `P`.
    #[inline]
    pub fn lock_multiple_policy<'a, const N: usize, P: ExclusiveLockPolicy<M, AcquireArgs = ()>>(
        locks: [&'a Self; N],
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
        KMutexGuardMultiple::new(locks)
    }

    /// Acquires multiple locks of the same class and type in ascending memory address order with
    /// policy `P` and arguments.
    #[inline]
    pub fn lock_multiple_policy_with<'a, const N: usize, P: ExclusiveLockPolicy<M>>(
        locks: [&'a Self; N],
        args: P::AcquireArgs,
    ) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
        KMutexGuardMultiple::new_with_args(locks, args)
    }

    /// Returns a reference to the underlying raw lock.
    #[inline]
    pub fn raw_mutex(&self) -> &M {
        &self.mutex
    }

    const fn class_id() -> *const core::ffi::c_void {
        if cfg!(any(feature = "lock_dep", feature = "lock_metadata_only")) {
            Class::ID
        } else {
            core::ptr::null()
        }
    }
}

impl<Class: LockClass, M: RawAssertHeldLock> KMutex<Class, M> {
    /// Asserts (in debug builds) that this lock is held by the caller and returns a [`LockToken`]
    /// witnessing ownership of `Class`, mirroring C++ `AssertHeld() TA_ASSERT()`.
    ///
    /// # Safety
    ///
    /// - This lock must currently be exclusively held by the caller, satisfying any platform
    ///   preconditions of [`M::assert_held`](RawAssertHeldLock::assert_held) (such as interrupts
    ///   being disabled for a spinlock).
    /// - The returned [`LockToken`] must not outlive that lock acquisition.
    /// - Uses of the returned [`LockToken`] must not overlap any conflicting borrows (shared or
    ///   mutable) derived from another [`LockToken`] or guard for `Class` on the same guarded
    ///   instance (in particular, if used mutably, no other shared or mutable borrows of data
    ///   guarded by `Class` on that instance may be live).
    #[inline]
    pub unsafe fn assert_held(&self) -> LockToken<'_, Class> {
        self.mutex.assert_held();
        // SAFETY: The caller guarantees that this lock is exclusively held, that the returned
        // token will not outlive that acquisition, and that uses of the token will not overlap any
        // conflicting shared or mutable borrows for `Class` on the same guarded instance.
        unsafe { LockToken::new() }
    }
}

impl<Class: LockClass, M: RawUntrackedLock> KMutex<Class, M> {
    /// Releases the underlying untracked lock without restoring interrupt state, consuming `token`
    /// (`TA_REL()`).
    ///
    /// # Safety
    ///
    /// - This lock must currently be exclusively held by the caller via a non-interrupt-saving
    ///   acquisition that `token` witnesses, satisfying any platform preconditions of
    ///   [`M::release`](RawUntrackedLock::release) (such as interrupts being disabled for a
    ///   spinlock).
    /// - No borrows of data guarded by this acquisition (derived from `token` or any other
    ///   [`LockToken`]) may remain live.
    /// - The allocation backing `self` must remain live until this call returns (see
    ///   [`RawUntrackedLock::release`]).
    #[inline]
    pub unsafe fn release(&self, token: LockToken<'_, Class>) {
        let _ = token;
        // SAFETY: The caller guarantees that this lock is held exclusively with any platform
        // preconditions satisfied, that no borrows of data guarded by this acquisition remain
        // live, and that `self` stays live until the call returns.
        unsafe {
            self.mutex.release();
        }
    }
}

impl<Class: LockClass, M: RawUntrackedIrqLock> KMutex<Class, M> {
    /// Releases the underlying untracked lock and restores saved interrupt state `state`, consuming
    /// `token` (`TA_REL()`).
    ///
    /// # Safety
    ///
    /// - This lock must currently be exclusively held by the caller via an interrupt-saving
    ///   acquisition that produced `state`, and `token` must witness that acquisition.
    /// - No borrows of data guarded by this acquisition (derived from `token` or any other
    ///   [`LockToken`]) may remain live.
    /// - The allocation backing `self` must remain live until this call returns (see
    ///   [`RawUntrackedIrqLock::release_irq_restore`]).
    #[inline]
    pub unsafe fn release_irq_restore(
        &self,
        state: M::InterruptState,
        token: LockToken<'_, Class>,
    ) {
        let _ = token;
        // SAFETY: The caller guarantees that this lock is held with saved interrupt state `state`,
        // that no borrows of data guarded by this acquisition remain live, and that `self` stays
        // live until the call returns.
        unsafe {
            self.mutex.release_irq_restore(state);
        }
    }
}

impl<Class: LockClass, M: RawUntrackedStatefulLock> KMutex<Class, M> {
    /// Releases the underlying untracked lock with per-acquisition `state`, consuming `token`
    /// (`TA_REL()`).
    ///
    /// # Safety
    ///
    /// - This lock must currently be exclusively held by the caller via an acquisition that
    ///   produced `state`, and `token` must witness that acquisition, satisfying any platform
    ///   preconditions of [`M::release_with`](RawUntrackedStatefulLock::release_with).
    /// - No borrows of data guarded by this acquisition (derived from `token` or any other
    ///   [`LockToken`]) may remain live.
    /// - The allocation backing `self` must remain live until this call returns (see
    ///   [`RawUntrackedStatefulLock::release_with`]).
    #[inline]
    pub unsafe fn release_with(&self, state: M::State, token: LockToken<'_, Class>) {
        let _ = token;
        // SAFETY: The caller guarantees that this lock is held with `state` and any platform
        // preconditions satisfied, that no borrows of data guarded by this acquisition remain
        // live, and that `self` stays live until the call returns.
        unsafe {
            self.mutex.release_with(state);
        }
    }
}

impl<Class: LockClass, M> core::fmt::Debug for KMutex<Class, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KMutex").field("class", &core::any::type_name::<Class>()).finish()
    }
}

/// A validation guard representing exclusive lock ownership and active list participation.
///
/// The guard is pinned in memory to ensure that its `state` remains safe and valid
/// inside the C++ loop detector active thread list.
#[repr(C)]
pub struct KMutexGuard<
    'a,
    Class: LockClass,
    M: RawLock,
    P: ExclusiveLockPolicy<M> = <M as RawLock>::DefaultPolicy,
> {
    // Safety invariant: When `Some(ptr)`, `ptr` points to a valid, initialized `KMutex<Class, M>`
    // currently held exclusively by the calling thread, whose `GuardState` is active in `state` at
    // this pinned slot's address, and whose storage remains live for as long as `from_raw`
    // requires (through the final `release()` / `Drop`, including across any `call_unlocked`
    // window, subject to what the policy documents for `release_raw`). Stored as
    // `NonNull` rather than `&'a KMutex` so `from_raw` / `*_unbounded` with `'a = 'static` do not
    // fabricate a `'static` Rust reference to a shorter-lived allocation (such as a `RefPtr`).
    mutex: Option<NonNull<KMutex<Class, M>>>,

    // Safety invariant: Pinned in place whenever `mutex` is `Some` (and across `call_unlocked`
    // before `P::reacquire`).
    state: P::GuardState,

    // Safety invariant: Handed out only while `mutex` is `Some`.
    token: LockToken<'a, Class>,

    _pin: PhantomPinned,

    _phantom: PhantomData<&'a KMutex<Class, M>>,
}

impl<'a, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>> KMutexGuard<'a, Class, M, P> {
    /// Creates a new stack-pinned validation guard initialization block.
    #[inline(always)]
    pub fn new(mutex: &'a KMutex<Class, M>) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        Self::new_with_args(mutex, ())
    }

    /// Creates a new stack-pinned validation guard initialization block with policy acquire
    /// arguments.
    #[inline(always)]
    pub fn new_with_args(
        mutex: &'a KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `mutex` is a valid shared reference for `'a`.
        unsafe { Self::from_raw_with_args(core::ptr::from_ref(mutex), args) }
    }

    /// Acquires `*mutex` and initializes a pinned `KMutexGuard` with caller-chosen lifetime `'a`.
    ///
    /// This is an escape hatch for storing a pinned `KMutexGuard<'static, ...>` inside a struct
    /// alongside an owning pointer (such as `RefPtr`) that keeps the mutex alive until the guard
    /// is released or dropped.
    ///
    /// # Safety
    ///
    /// - `mutex` must be non-null, properly aligned, and point to an initialized
    ///   `KMutex<Class, M>` that stays valid:
    ///   1. while this acquisition is held, including for the whole of any
    ///      [`call_unlocked`](Self::call_unlocked), which borrows `*mutex` across its release and
    ///      reacquire;
    ///   2. for the duration of any borrows derived from `'a` or its [`LockToken`]; and
    ///   3. until the final [`release`](Self::release) or `Drop` of this acquisition returns.
    ///
    ///   The final release goes through [`LockPolicy::release_raw`] with no reference to `*mutex`
    ///   held across it, so the third requirement is replaced by whatever weaker requirement `P`
    ///   documents, directly or by delegation, for its `release_raw`.
    /// - All acquisition and release-ordering preconditions of `P` (such as thread blockability,
    ///   disabled interrupts, or stack-LIFO release for interrupt-saving policies) must be upheld.
    ///
    /// # Panics
    ///
    /// Panics if `mutex` is null.
    #[inline(always)]
    pub unsafe fn from_raw(
        mutex: *const KMutex<Class, M>,
    ) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        // SAFETY: Forwarded from the caller.
        unsafe { Self::from_raw_with_args(mutex, ()) }
    }

    /// Acquires `*mutex` with `args` and initializes a pinned `KMutexGuard` with caller-chosen
    /// lifetime `'a`.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`from_raw`](Self::from_raw).
    ///
    /// # Panics
    ///
    /// Panics if `mutex` is null.
    #[inline(always)]
    pub unsafe fn from_raw_with_args(
        mutex: *const KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        const {
            lockdep::validate_lock_flags(Class::FLAGS);
            assert!(
                !P::EXTERNAL_ORDER || (Class::FLAGS & lockdep::LOCK_FLAGS_NESTABLE) != 0,
                "Externally ordered lock policies require a nestable lock class"
            );
        }
        let mutex =
            NonNull::new(mutex.cast_mut()).expect("KMutexGuard::from_raw requires non-null mutex");
        KMutexGuardInit { mutex, args, _policy: PhantomData, _lifetime: PhantomData }
    }

    /// Initializes an uninitialized pinned `this` slot in place.
    ///
    /// # Safety
    ///
    /// `this` must be valid for writes of `Self`, properly aligned, and pinned in memory until its
    /// destructor completes, and `mutex` must satisfy the preconditions of
    /// [`from_raw_with_args`](Self::from_raw_with_args).
    #[inline(always)]
    unsafe fn init_slot(
        this: *mut Self,
        mutex: NonNull<KMutex<Class, M>>,
        args: P::AcquireArgs,
    ) -> Result<(), core::convert::Infallible> {
        // SAFETY: `this` is valid for writes and properly aligned per `# Safety`, so each field
        // projection is valid and non-overlapping. `acquire` is called with `&mut *state_addr`
        // pinned in place, after which the mutex is held for `'a`, justifying `LockToken::new`.
        unsafe {
            let state_addr = addr_of_mut!((*this).state);
            core::ptr::write(state_addr, P::GuardState::default());

            P::acquire(&mutex.as_ref().mutex, &mut *state_addr, args);

            let mutex_addr = addr_of_mut!((*this).mutex);
            core::ptr::write(mutex_addr, Some(mutex));

            let token_addr = addr_of_mut!((*this).token);
            core::ptr::write(token_addr, LockToken::new());

            let pin_addr = addr_of_mut!((*this)._pin);
            core::ptr::write(pin_addr, PhantomPinned);

            let phantom_addr = addr_of_mut!((*this)._phantom);
            core::ptr::write(phantom_addr, PhantomData);
        }

        Ok(())
    }

    /// Attempts to create a new stack-pinned validation guard without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if the lock cannot be immediately acquired.
    #[inline(always)]
    pub fn try_new(mutex: &'a KMutex<Class, M>) -> impl PinInit<Self, ()>
    where
        P: TryLockPolicy<M, AcquireArgs = ()>,
    {
        Self::try_new_with_args(mutex, ())
    }

    /// Attempts to create a new stack-pinned validation guard with policy acquire arguments
    /// without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if the lock cannot be immediately acquired.
    #[inline(always)]
    pub fn try_new_with_args(
        mutex: &'a KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, ()>
    where
        P: TryLockPolicy<M>,
    {
        const {
            lockdep::validate_lock_flags(Class::FLAGS);
            assert!(
                !P::EXTERNAL_ORDER || (Class::FLAGS & lockdep::LOCK_FLAGS_NESTABLE) != 0,
                "Externally ordered lock policies require a nestable lock class"
            );
        }

        KMutexGuardTryInit { mutex, args, _policy: PhantomData }
    }

    /// Attempts to initialize an uninitialized pinned `this` slot in place without blocking.
    ///
    /// # Safety
    ///
    /// `this` must be valid for writes of `Self`, properly aligned, and pinned in memory until its
    /// destructor completes if `Ok(())` is returned.
    #[inline(always)]
    unsafe fn try_init_slot(
        this: *mut Self,
        mutex: &'a KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> Result<(), ()>
    where
        P: TryLockPolicy<M>,
    {
        // SAFETY: `this` is valid for writes and properly aligned per `# Safety`. If `try_acquire`
        // fails, `state_addr` is dropped in place before returning `Err(())` with no other fields
        // initialized; otherwise every field is initialized and the mutex is held for `'a`.
        unsafe {
            let state_addr = addr_of_mut!((*this).state);
            core::ptr::write(state_addr, P::GuardState::default());

            if !P::try_acquire(&mutex.mutex, &mut *state_addr, args) {
                core::ptr::drop_in_place(state_addr);
                return Err(());
            }

            let mutex_addr = addr_of_mut!((*this).mutex);
            core::ptr::write(mutex_addr, Some(NonNull::from(mutex)));

            let token_addr = addr_of_mut!((*this).token);
            core::ptr::write(token_addr, LockToken::new());

            let pin_addr = addr_of_mut!((*this)._pin);
            core::ptr::write(pin_addr, PhantomPinned);

            let phantom_addr = addr_of_mut!((*this)._phantom);
            core::ptr::write(phantom_addr, PhantomData);
        }

        Ok(())
    }

    /// Returns `true` if this guard currently holds a lock (has not been released or moved out).
    #[inline]
    pub fn is_some(&self) -> bool {
        self.mutex.is_some()
    }

    /// Returns `true` if this guard is empty (has been released or moved out).
    #[inline]
    pub fn is_none(&self) -> bool {
        self.mutex.is_none()
    }

    /// Returns the mutex reference held by this guard, or `None` if released.
    #[inline]
    pub fn mutex(&self) -> Option<&KMutex<Class, M>> {
        // SAFETY: By `self.mutex`'s safety invariant, when `Some(ptr)`, `ptr` points to a valid,
        // initialized `KMutex` that stays live for at least the duration of `&self`.
        self.mutex.map(|ptr| unsafe { ptr.as_ref() })
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class> {
        assert!(self.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &self.token
    }

    /// Returns a mutable reference to the lock proof `LockToken` inside this pinned projection.
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        // SAFETY: Modifying the non-pinned raw `token` field does not violate pinning invariants
        // since the token has no drop logic or pointer-location sensitivity.
        let me = unsafe { self.get_unchecked_mut() };
        assert!(me.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &mut me.token
    }

    /// # Safety
    ///
    /// The caller must ensure that this guard currently holds a lock (`self.is_some()`).
    #[inline]
    pub unsafe fn token_unchecked(&self) -> &LockToken<'a, Class> {
        debug_assert!(self.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &self.token
    }

    /// # Safety
    ///
    /// The caller must ensure that this guard currently holds a lock (`self.is_some()`).
    #[inline]
    pub unsafe fn token_mut_unchecked(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        // SAFETY: Modifying the non-pinned raw `token` field does not violate pinning invariants
        // since the token has no drop logic or pointer-location sensitivity.
        let me = unsafe { self.get_unchecked_mut() };
        debug_assert!(me.mutex.is_some(), "cannot access token of a released KMutexGuard");
        &mut me.token
    }

    /// Returns true if this guard currently holds the given `target` mutex instance.
    #[inline]
    pub fn is_locked_by(&self, target: &KMutex<Class, M>) -> bool {
        core::mem::size_of::<M>() > 0
            && self.mutex.is_some_and(|m| core::ptr::eq(m.as_ptr().cast_const(), target))
    }

    /// Explicitly releases the lock before the guard is dropped.
    ///
    /// If the lock has already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        // SAFETY: `state` is pinned in place, and when `self.mutex` is `Some(mutex_ptr)`,
        // `mutex_ptr` points to a valid `KMutex` held under `self.state` that stays live for as
        // long as `P::release_raw` requires. The raw lock is passed by pointer, and no reference to
        // the `KMutex` is formed (it is `repr(transparent)` over its raw lock, so the pointer is
        // cast), because a guard created by `from_raw` may be the only thing keeping the mutex
        // alive, in which case it can be destroyed by another thread as soon as the release is
        // published.
        unsafe {
            let me = self.get_unchecked_mut();
            if let Some(mutex_ptr) = me.mutex.take() {
                P::release_raw(mutex_ptr.cast::<M>(), &mut me.state);
            }
        }
    }

    /// Temporarily releases the lock before executing the given callable `f` and then
    /// re-acquires the lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        // SAFETY: `state` is pinned in place. Taking `me.mutex` before `P::release` and restoring
        // it only after `P::reacquire` ensures that if `f()` (or `P::reacquire`) unwinds, `Drop`
        // will not double-unlock and the guard remains empty. By `self.mutex`'s safety invariant,
        // `mutex_ptr` remains valid across `f()` and `P::reacquire`.
        unsafe {
            let me = self.get_unchecked_mut();
            let mutex_ptr =
                me.mutex.take().expect("cannot call_unlocked on a released KMutexGuard");
            P::release(&mutex_ptr.as_ref().mutex, &mut me.state);
            let result = f();
            P::reacquire(&mutex_ptr.as_ref().mutex, &mut me.state);
            me.mutex = Some(mutex_ptr);
            result
        }
    }

    /// Calls a closure while temporarily disabling lockdep tracking for the lock held by this
    /// guard.
    #[inline]
    pub fn call_untracked<R, F: FnOnce(&mut LockToken<'a, Class>) -> R>(
        self: Pin<&mut Self>,
        f: F,
    ) -> R {
        struct RetrackGuard<'g, M: RawLock, P: LockPolicy<M>> {
            lock: &'g M,
            state: &'g mut P::GuardState,
        }
        impl<M: RawLock, P: LockPolicy<M>> Drop for RetrackGuard<'_, M, P> {
            #[inline]
            fn drop(&mut self) {
                // SAFETY: `self.state` was untracked by `P::untrack` just before constructing
                // `RetrackGuard` while the calling thread continues to hold `self.lock`.
                unsafe {
                    P::retrack(self.lock, self.state);
                }
            }
        }

        // SAFETY: `me.state` is pinned in place and `me.mutex` is valid while held. `RetrackGuard`
        // borrows only `mutex_ref.mutex` and `me.state` (disjoint from `me.token`) and guarantees
        // `P::retrack` runs even if `f` unwinds before `KMutexGuard::drop` releases the lock.
        unsafe {
            let me = self.get_unchecked_mut();
            let mutex_ptr = me.mutex.expect("cannot call_untracked on a released KMutexGuard");
            let mutex_ref = mutex_ptr.as_ref();
            P::untrack(&mutex_ref.mutex, &mut me.state);
            let _retrack = RetrackGuard::<M, P> { lock: &mutex_ref.mutex, state: &mut me.state };
            f(&mut me.token)
        }
    }
}

struct KMutexGuardInit<'a, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>> {
    // Safety invariant: `mutex` satisfies the preconditions of `KMutexGuard::from_raw_with_args`.
    mutex: NonNull<KMutex<Class, M>>,
    args: P::AcquireArgs,
    _policy: PhantomData<fn() -> P>,
    _lifetime: PhantomData<&'a KMutex<Class, M>>,
}

// SAFETY: `KMutexGuard::init_slot` initializes all fields of `KMutexGuard` in place.
unsafe impl<'a, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>>
    PinInit<KMutexGuard<'a, Class, M, P>, core::convert::Infallible>
    for KMutexGuardInit<'a, Class, M, P>
{
    #[inline(always)]
    unsafe fn __pinned_init(
        self,
        slot: *mut KMutexGuard<'a, Class, M, P>,
    ) -> Result<(), core::convert::Infallible> {
        // SAFETY: Forwarded from `PinInit::__pinned_init` and `self.mutex`'s safety invariant.
        unsafe { KMutexGuard::init_slot(slot, self.mutex, self.args) }
    }
}

struct KMutexGuardTryInit<'a, Class: LockClass, M: RawLock, P: TryLockPolicy<M>> {
    mutex: &'a KMutex<Class, M>,
    args: P::AcquireArgs,
    _policy: PhantomData<fn() -> P>,
}

// SAFETY: `KMutexGuard::try_init_slot` initializes all fields of `KMutexGuard` on `Ok(())` and
// drops `state` in place before returning `Err(())` on failure.
unsafe impl<'a, Class: LockClass, M: RawLock, P: TryLockPolicy<M>>
    PinInit<KMutexGuard<'a, Class, M, P>, ()> for KMutexGuardTryInit<'a, Class, M, P>
{
    #[inline(always)]
    unsafe fn __pinned_init(self, slot: *mut KMutexGuard<'a, Class, M, P>) -> Result<(), ()> {
        // SAFETY: Forwarded from `PinInit::__pinned_init`.
        unsafe { KMutexGuard::try_init_slot(slot, self.mutex, self.args) }
    }
}

impl<'a, Class: LockClass, M: RawLock, P: AdoptableLockPolicy<M>> KMutexGuard<'a, Class, M, P> {
    /// Creates an empty (released) pinned `KMutexGuard` initialization block.
    #[inline]
    pub fn empty() -> impl PinInit<Self, core::convert::Infallible> {
        let init_fn = move |this: *mut Self| -> Result<(), core::convert::Infallible> {
            // SAFETY: `this` points to valid, aligned, uninitialized pinned storage for `Self`;
            // `LockToken::inactive()` is stored alongside `mutex = None`.
            unsafe {
                core::ptr::write(addr_of_mut!((*this).mutex), None);
                core::ptr::write(addr_of_mut!((*this).state), P::GuardState::default());
                core::ptr::write(addr_of_mut!((*this).token), LockToken::inactive());
                core::ptr::write(addr_of_mut!((*this)._pin), PhantomPinned);
                core::ptr::write(addr_of_mut!((*this)._phantom), PhantomData);
            }
            Ok(())
        };
        // SAFETY: `init_fn` initializes every field of `KMutexGuard` in place before returning
        // `Ok(())`.
        unsafe { pin_init_from_closure(init_fn) }
    }

    /// Initializes a new pinned `KMutexGuard` by moving any active acquisition out of `src`.
    ///
    /// # Safety
    ///
    /// If `src` holds a lock, its underlying `KMutex` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires, and for the duration of any borrows derived from
    /// `'a` or its [`LockToken`].
    #[inline]
    unsafe fn move_init_from(
        src: Pin<&mut KMutexGuard<'_, Class, M, P>>,
        empty_panic_msg: Option<&'static str>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        if let Some(msg) = empty_panic_msg {
            assert!(src.is_some(), "{msg}");
        }
        let init_fn = move |this: *mut Self| -> Result<(), core::convert::Infallible> {
            // SAFETY: `this` points to valid, aligned, uninitialized pinned storage for `Self`;
            // `src` is pinned and exclusively borrowed; if `src` holds a lock, its `KMutex`
            // satisfies `Self`'s lifetime and validity invariants per the caller.
            unsafe {
                let src = src.get_unchecked_mut();
                let state_addr = addr_of_mut!((*this).state);
                core::ptr::write(state_addr, P::GuardState::default());
                if let Some(mutex_ptr) = src.mutex.take() {
                    P::move_state(&mut src.state, &mut *state_addr);
                    core::ptr::write(addr_of_mut!((*this).mutex), Some(mutex_ptr));
                    core::ptr::write(addr_of_mut!((*this).token), LockToken::new());
                } else {
                    core::ptr::write(addr_of_mut!((*this).mutex), None);
                    core::ptr::write(addr_of_mut!((*this).token), LockToken::inactive());
                }
                core::ptr::write(addr_of_mut!((*this)._pin), PhantomPinned);
                core::ptr::write(addr_of_mut!((*this)._phantom), PhantomData);
            }
            Ok(())
        };
        // SAFETY: `init_fn` initializes every field of `KMutexGuard` in place before returning
        // `Ok(())`, relocating any active `GuardState` from `src` into the pinned `this` slot.
        unsafe { pin_init_from_closure(init_fn) }
    }

    /// Releases any lock held by `self`, then moves any active acquisition from `src` into `self`.
    ///
    /// # Safety
    ///
    /// If `src` holds a lock, its underlying `KMutex` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires, and for the duration of any borrows derived from
    /// `'a` or its [`LockToken`].
    #[inline]
    unsafe fn move_into_slot(
        mut self: Pin<&mut Self>,
        src: Pin<&mut KMutexGuard<'_, Class, M, P>>,
        empty_panic_msg: Option<&'static str>,
    ) {
        if let Some(msg) = empty_panic_msg {
            assert!(src.is_some(), "{msg}");
        }
        self.as_mut().release();
        // SAFETY: Both `self` and `src` are pinned in place and non-aliasing (`Pin<&mut _>`),
        // `self` is empty after `release()`, and if `src` holds an active lock its `KMutex`
        // satisfies `Self`'s lifetime and validity invariants per the caller.
        unsafe {
            let dst = self.get_unchecked_mut();
            let src = src.get_unchecked_mut();
            if let Some(mutex_ptr) = src.mutex.take() {
                P::move_state(&mut src.state, &mut dst.state);
                dst.mutex = Some(mutex_ptr);
                dst.token = LockToken::new();
            }
        }
    }

    /// Adopts the lock acquisition held by `adoptable` into a newly initialized pinned
    /// `KMutexGuard`, moving the lock state out of `adoptable` and leaving `adoptable` empty.
    ///
    /// # Panics
    ///
    /// Panics if `adoptable` is empty (`adoptable.is_none()`).
    #[inline]
    pub fn adopt(
        adoptable: Pin<&mut Adoptable<'a, Class, M, P>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `adoptable` has lifetime `'a`, so its underlying `KMutex` is valid for `'a`.
        unsafe { Self::adopt_unbounded(adoptable) }
    }

    /// Adopts the lock acquisition held by `adoptable` into a newly initialized pinned
    /// `KMutexGuard` with caller-chosen lifetime `'a`, leaving `adoptable` empty.
    ///
    /// # Safety
    ///
    /// The underlying `KMutex` held by `adoptable` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires (through its final release, including across any
    /// temporary unlock in [`call_unlocked`](Self::call_unlocked)) and for the duration of any
    /// borrows derived from `'a` or its [`LockToken`].
    ///
    /// # Panics
    ///
    /// Panics if `adoptable` is empty (`adoptable.is_none()`).
    #[inline]
    pub unsafe fn adopt_unbounded(
        adoptable: Pin<&mut Adoptable<'_, Class, M, P>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: Forwarded from the caller.
        unsafe {
            Self::move_init_from(
                adoptable.inner_pin(),
                Some("cannot adopt from an empty Adoptable"),
            )
        }
    }

    /// Releases any lock currently held by `self`, then adopts the lock acquisition held by
    /// `adoptable` into `self`, leaving `adoptable` empty.
    ///
    /// # Panics
    ///
    /// Panics if `adoptable` is empty (`adoptable.is_none()`).
    #[inline]
    pub fn adopt_from(self: Pin<&mut Self>, adoptable: Pin<&mut Adoptable<'a, Class, M, P>>) {
        // SAFETY: `adoptable` has lifetime `'a`, matching `self`.
        unsafe {
            self.move_into_slot(
                adoptable.inner_pin(),
                Some("cannot adopt_from an empty Adoptable"),
            );
        }
    }

    /// Releases any lock currently held by `self`, then moves the lock acquisition (if any) from
    /// `other` into `self`, leaving `other` empty.
    #[inline]
    pub fn move_from(self: Pin<&mut Self>, other: Pin<&mut Self>) {
        // SAFETY: `other` has lifetime `'a`, matching `self`.
        unsafe {
            self.move_into_slot(other, None);
        }
    }

    /// Moves the active lock acquisition out of `self` into a newly initialized pinned
    /// [`Adoptable`], leaving `self` released/empty.
    ///
    /// # Panics
    ///
    /// Panics if `self` has already been released.
    #[inline]
    pub fn take(
        self: Pin<&mut Self>,
    ) -> impl PinInit<Adoptable<'a, Class, M, P>, core::convert::Infallible> {
        Adoptable::from_guard(self)
    }

    /// Moves the active lock acquisition out of `self` into a newly initialized pinned
    /// [`Adoptable`] with caller-chosen lifetime `'b`, leaving `self` released/empty.
    ///
    /// # Safety
    ///
    /// The underlying `KMutex` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires (through its final release, including across any
    /// temporary unlock in [`Adoptable::call_unlocked`]) and for the duration of any borrows
    /// derived from `'b` or its [`LockToken`].
    ///
    /// # Panics
    ///
    /// Panics if `self` has already been released.
    #[inline]
    pub unsafe fn take_unbounded<'b>(
        self: Pin<&mut Self>,
    ) -> impl PinInit<Adoptable<'b, Class, M, P>, core::convert::Infallible>
    where
        Class: 'b,
        M: 'b,
    {
        // SAFETY: Forwarded from the caller.
        unsafe { Adoptable::from_guard_unbounded(self) }
    }
}

impl<Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>> core::fmt::Debug
    for KMutexGuard<'_, Class, M, P>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KMutexGuard")
            .field("class", &core::any::type_name::<Class>())
            .field("held", &self.is_some())
            .finish()
    }
}

// Not `#[pin_data(PinnedDrop)]`, whose generated `Drop` is not `#[inline]`.
impl<'a, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>> Drop
    for KMutexGuard<'a, Class, M, P>
{
    #[inline(always)]
    fn drop(&mut self) {
        // SAFETY: A guard is only created pinned, and `self` is not moved after it is dropped.
        unsafe { Pin::new_unchecked(self) }.release();
    }
}

/// A pinned, adoptable lock guard with the same layout and inline state as [`KMutexGuard`],
/// mirroring C++ `Guard::Adoptable`.
///
/// Unlike a movable by-value guard, `Adoptable` stays pinned in memory so that its inline
/// `P::GuardState` (including lockdep's `AcquiredLockEntry`) is never relocated by a bitwise Rust
/// move. Ownership of the lock acquisition is transferred between pinned slots via
/// [`LockPolicy::move_state`] (which invokes C++ `AcquiredLockEntry`'s move constructor when
/// lockdep is enabled, updating the thread's held-lock list in place):
///
/// - Into a [`KMutexGuard`] via [`KMutex::adopt`] or [`KMutexGuard::adopt`] /
///   [`KMutexGuard::adopt_from`].
/// - Out of a [`KMutexGuard`] via [`KMutexGuard::take`] or [`Adoptable::from_guard`] /
///   [`Adoptable::take_from`].
/// - Between [`Adoptable`] slots via [`Adoptable::from_adoptable`] or [`Adoptable::move_from`].
///
/// Requires [`AdoptableLockPolicy`] so policies with stack-LIFO CPU/interrupt state (such as
/// kernel spinlock `IrqSavePolicy`) cannot be relocated or released out of LIFO order.
///
/// If an `Adoptable` is dropped while still holding a lock, it releases the lock just like
/// [`KMutexGuard`].
#[repr(transparent)]
pub struct Adoptable<
    'a,
    Class: LockClass,
    M: RawLock,
    P: AdoptableLockPolicy<M> = <M as RawLock>::DefaultPolicy,
> {
    inner: KMutexGuard<'a, Class, M, P>,
}

impl<'a, Class: LockClass, M: RawLock, P: AdoptableLockPolicy<M>> Adoptable<'a, Class, M, P> {
    #[inline]
    fn wrap_init(
        init: impl PinInit<KMutexGuard<'a, Class, M, P>, core::convert::Infallible>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `Adoptable` is `#[repr(transparent)]` over `KMutexGuard`, so pin-initializing
        // `inner` in place initializes the entire `Adoptable` slot.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                init.__pinned_init(addr_of_mut!((*slot).inner))
            })
        }
    }

    #[inline]
    fn inner_pin(self: Pin<&mut Self>) -> Pin<&mut KMutexGuard<'a, Class, M, P>> {
        // SAFETY: `inner` is structurally pinned inside `Adoptable`.
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().inner) }
    }

    /// Creates an empty (unlocked) pinned `Adoptable` initialization block.
    #[inline]
    pub fn empty() -> impl PinInit<Self, core::convert::Infallible> {
        Self::wrap_init(KMutexGuard::empty())
    }

    /// Acquires `mutex` with the policy's default arguments into a newly initialized pinned
    /// `Adoptable`.
    #[inline]
    pub fn new(mutex: &'a KMutex<Class, M>) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: AdoptableLockPolicy<M, AcquireArgs = ()>,
    {
        Self::wrap_init(KMutexGuard::new(mutex))
    }

    /// Acquires `mutex` with `args` into a newly initialized pinned `Adoptable`.
    #[inline]
    pub fn new_with_args(
        mutex: &'a KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        Self::wrap_init(KMutexGuard::new_with_args(mutex, args))
    }

    /// Acquires `*mutex` and initializes a pinned `Adoptable` with caller-chosen lifetime `'a`.
    ///
    /// This is an escape hatch for storing a pinned `Adoptable` inside a struct alongside a raw
    /// pointer or owning reference (such as `LockedPtr`), or with `'static` lifetime when the
    /// caller manages the underlying object's lifetime manually.
    ///
    /// # Safety
    ///
    /// - `mutex` must be non-null, properly aligned, and point to an initialized
    ///   `KMutex<Class, M>` that stays valid:
    ///   1. while this acquisition is held, including for the whole of any
    ///      [`call_unlocked`](Self::call_unlocked), which borrows `*mutex` across its release and
    ///      reacquire;
    ///   2. for the duration of any borrows derived from `'a` or its [`LockToken`]; and
    ///   3. until the final [`release`](Self::release) or `Drop` of this acquisition returns.
    ///
    ///   The final release goes through [`LockPolicy::release_raw`] with no reference to `*mutex`
    ///   held across it, so the third requirement is replaced by whatever weaker requirement `P`
    ///   documents, directly or by delegation, for its `release_raw`.
    /// - All acquisition preconditions of `P` (such as thread blockability or disabled interrupts)
    ///   must be upheld.
    ///
    /// # Panics
    ///
    /// Panics if `mutex` is null.
    #[inline]
    pub unsafe fn from_raw(
        mutex: *const KMutex<Class, M>,
    ) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: AdoptableLockPolicy<M, AcquireArgs = ()>,
    {
        // SAFETY: Forwarded from the caller.
        Self::wrap_init(unsafe { KMutexGuard::from_raw(mutex) })
    }

    /// Acquires `*mutex` with `args` and initializes a pinned `Adoptable` with caller-chosen
    /// lifetime `'a`.
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`from_raw`](Self::from_raw).
    ///
    /// # Panics
    ///
    /// Panics if `mutex` is null.
    #[inline]
    pub unsafe fn from_raw_with_args(
        mutex: *const KMutex<Class, M>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: Forwarded from the caller.
        Self::wrap_init(unsafe { KMutexGuard::from_raw_with_args(mutex, args) })
    }

    /// Moves the active lock acquisition out of `guard` into a newly initialized pinned
    /// `Adoptable`, leaving `guard` released/empty.
    ///
    /// # Panics
    ///
    /// Panics if `guard` has already been released.
    #[inline]
    pub fn from_guard(
        guard: Pin<&mut KMutexGuard<'a, Class, M, P>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `guard` has lifetime `'a`, so the underlying mutex is valid for `'a`.
        unsafe { Self::from_guard_unbounded(guard) }
    }

    /// Moves the active lock acquisition out of `guard` into a newly initialized pinned
    /// `Adoptable` with caller-chosen lifetime `'a`, leaving `guard` released/empty.
    ///
    /// # Safety
    ///
    /// The underlying `KMutex` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires (through its final release, including across any
    /// temporary unlock in [`call_unlocked`](Self::call_unlocked)) and for the duration of any
    /// borrows derived from `'a` or its [`LockToken`].
    ///
    /// # Panics
    ///
    /// Panics if `guard` has already been released.
    #[inline]
    pub unsafe fn from_guard_unbounded(
        guard: Pin<&mut KMutexGuard<'_, Class, M, P>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: Forwarded from the caller.
        Self::wrap_init(unsafe {
            KMutexGuard::move_init_from(
                guard,
                Some("cannot create Adoptable from a released KMutexGuard"),
            )
        })
    }

    /// Moves the state of `other` (whether holding a lock or empty) into a newly initialized
    /// pinned `Adoptable`, leaving `other` empty.
    #[inline]
    pub fn from_adoptable(other: Pin<&mut Self>) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `other` has lifetime `'a`, so if it holds a lock the mutex is valid for `'a`.
        unsafe { Self::from_adoptable_unbounded(other) }
    }

    /// Moves the state of `other` (whether holding a lock or empty) into a newly initialized
    /// pinned `Adoptable` with caller-chosen lifetime `'a`, leaving `other` empty.
    ///
    /// # Safety
    ///
    /// If `other` holds a lock, the underlying `KMutex` must remain valid for this acquisition
    /// as [`from_raw`](Self::from_raw) requires (through its final release, including across
    /// any temporary unlock in [`call_unlocked`](Self::call_unlocked)) and for the duration of any
    /// borrows derived from `'a` or its [`LockToken`].
    #[inline]
    pub unsafe fn from_adoptable_unbounded(
        other: Pin<&mut Adoptable<'_, Class, M, P>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: Forwarded from the caller.
        Self::wrap_init(unsafe { KMutexGuard::move_init_from(other.inner_pin(), None) })
    }

    /// Releases any lock currently held by `self`, then moves the active lock acquisition from
    /// `guard` into `self`, leaving `guard` released/empty.
    ///
    /// # Panics
    ///
    /// Panics if `guard` has already been released.
    #[inline]
    pub fn take_from(self: Pin<&mut Self>, guard: Pin<&mut KMutexGuard<'a, Class, M, P>>) {
        // SAFETY: `guard` has lifetime `'a`.
        unsafe { self.take_from_unbounded(guard) }
    }

    /// Releases any lock currently held by `self`, then moves the active lock acquisition from
    /// `guard` into `self` with lifetime `'a`, leaving `guard` released/empty.
    ///
    /// # Safety
    ///
    /// The underlying `KMutex` must remain valid for this acquisition as
    /// [`from_raw`](Self::from_raw) requires (through its final release, including across any
    /// temporary unlock in [`call_unlocked`](Self::call_unlocked)) and for the duration of any
    /// borrows derived from `'a` or its [`LockToken`].
    ///
    /// # Panics
    ///
    /// Panics if `guard` has already been released.
    #[inline]
    pub unsafe fn take_from_unbounded(
        self: Pin<&mut Self>,
        guard: Pin<&mut KMutexGuard<'_, Class, M, P>>,
    ) {
        // SAFETY: Forwarded from the caller.
        unsafe {
            self.inner_pin().move_into_slot(guard, Some("cannot take_from a released KMutexGuard"));
        }
    }

    /// Releases any lock currently held by `self`, then moves the state of `other` (whether
    /// holding a lock or empty) into `self`, leaving `other` empty.
    #[inline]
    pub fn move_from(self: Pin<&mut Self>, other: Pin<&mut Self>) {
        // SAFETY: `other` has the same lifetime `'a` as `self`.
        unsafe { self.move_from_unbounded(other) }
    }

    /// Releases any lock currently held by `self`, then moves the state of `other` (whether
    /// holding a lock or empty) into `self` with lifetime `'a`, leaving `other` empty.
    ///
    /// # Safety
    ///
    /// If `other` holds a lock, the underlying `KMutex` must remain valid for this acquisition
    /// as [`from_raw`](Self::from_raw) requires (through its final release, including across
    /// any temporary unlock in [`call_unlocked`](Self::call_unlocked)) and for the duration of any
    /// borrows derived from `'a` or its [`LockToken`].
    #[inline]
    pub unsafe fn move_from_unbounded(
        self: Pin<&mut Self>,
        other: Pin<&mut Adoptable<'_, Class, M, P>>,
    ) {
        // SAFETY: Forwarded from the caller.
        unsafe {
            self.inner_pin().move_into_slot(other.inner_pin(), None);
        }
    }

    /// Returns `true` if this `Adoptable` currently holds a lock.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.inner.is_some()
    }

    /// Returns `true` if this `Adoptable` is empty (holds no lock).
    #[inline]
    pub fn is_none(&self) -> bool {
        self.inner.is_none()
    }

    /// Returns `true` if this `Adoptable` currently holds the given `target` mutex instance.
    #[inline]
    pub fn is_locked_by(&self, target: &KMutex<Class, M>) -> bool {
        self.inner.is_locked_by(target)
    }

    /// Returns the mutex reference held by this `Adoptable`, or `None` if empty.
    #[inline]
    pub fn mutex(&self) -> Option<&KMutex<Class, M>> {
        self.inner.mutex()
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    ///
    /// # Panics
    ///
    /// Panics if this `Adoptable` is empty.
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class> {
        assert!(self.is_some(), "cannot access token of an empty Adoptable");
        // SAFETY: `self.is_some()` was just asserted.
        unsafe { self.inner.token_unchecked() }
    }

    /// Returns a mutable reference to the lock proof `LockToken` inside this pinned projection.
    ///
    /// # Panics
    ///
    /// Panics if this `Adoptable` is empty.
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        assert!(self.is_some(), "cannot access token of an empty Adoptable");
        // SAFETY: `self.is_some()` was just asserted.
        unsafe { self.inner_pin().token_mut_unchecked() }
    }

    /// Returns a shared reference to the lock proof `LockToken` without checking in release builds
    /// whether this `Adoptable` holds a lock.
    ///
    /// # Safety
    ///
    /// This `Adoptable` must currently hold a lock (`self.is_some()`).
    #[inline]
    pub unsafe fn token_unchecked(&self) -> &LockToken<'a, Class> {
        debug_assert!(self.is_some(), "cannot access token of an empty Adoptable");
        // SAFETY: Caller guarantees `self.is_some()`.
        unsafe { self.inner.token_unchecked() }
    }

    /// Returns a mutable reference to the lock proof `LockToken` without checking in release
    /// builds whether this `Adoptable` holds a lock.
    ///
    /// # Safety
    ///
    /// This `Adoptable` must currently hold a lock (`self.is_some()`).
    #[inline]
    pub unsafe fn token_mut_unchecked(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        debug_assert!(self.is_some(), "cannot access token of an empty Adoptable");
        // SAFETY: Caller guarantees `self.is_some()`.
        unsafe { self.inner_pin().token_mut_unchecked() }
    }

    /// Explicitly releases the lock if held, leaving this `Adoptable` empty.
    ///
    /// If this `Adoptable` is already empty, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        self.inner_pin().release();
    }

    /// Temporarily releases the lock before executing the given callable `f` and then
    /// re-acquires the lock.
    ///
    /// # Panics
    ///
    /// Panics if this `Adoptable` is empty.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        assert!(self.is_some(), "cannot call_unlocked on an empty Adoptable");
        self.inner_pin().call_unlocked(f)
    }

    /// Calls a closure while temporarily disabling lockdep tracking for the lock held by this
    /// `Adoptable`.
    ///
    /// # Panics
    ///
    /// Panics if this `Adoptable` is empty.
    #[inline]
    pub fn call_untracked<R, F: FnOnce(&mut LockToken<'a, Class>) -> R>(
        self: Pin<&mut Self>,
        f: F,
    ) -> R {
        assert!(self.is_some(), "cannot call_untracked on an empty Adoptable");
        self.inner_pin().call_untracked(f)
    }
}

impl<Class: LockClass, M: RawLock, P: AdoptableLockPolicy<M>> core::fmt::Debug
    for Adoptable<'_, Class, M, P>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Adoptable")
            .field("class", &core::any::type_name::<Class>())
            .field("held", &self.is_some())
            .finish()
    }
}

/// Guards simultaneous acquisitions of multiple locks of the same class and type.
///
/// Locks are automatically sorted in-place by ascending memory address before acquisition to
/// preserve the intra-class ordering invariant and prevent AB/BA deadlocks, matching C++
/// `lockdep::GuardMultiple`.
///
/// Unlike `GuardMultiple`, which passes each lock's address to lockdep as its order, every lock
/// is currently acquired with lockdep order 0, so lockdep does not validate the address ordering
/// itself and, for `N > 1`, a tracked lock class must be `LOCK_FLAGS_MULTI_ACQUIRE` for the
/// second acquisition not to be reported as an invalid re-acquisition. This is not checked at
/// compile time because untracked classes (and `N <= 1`) are unaffected.
///
/// TODO: Pass the physical lock address as the lockdep order like C++ `GuardMultiple`, which
/// removes the `LOCK_FLAGS_MULTI_ACQUIRE` requirement and lets lockdep validate the ordering.
///
/// Nestable lock classes and externally ordered policies are rejected at compile time. The check
/// is against [`LockClass::FLAGS`], i.e. the member's flags as well as the raw lock type's, where
/// C++ `GuardMultiple` only inspects the lock type's flags.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct KMutexGuardMultiple<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock,
    P: ExclusiveLockPolicy<M> = <M as RawLock>::DefaultPolicy,
> {
    mutexes: [Option<&'a KMutex<Class, M>>; N],

    #[pin]
    states: [P::GuardState; N],

    tokens: [LockToken<'a, Class>; N],
}

impl<'a, const N: usize, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>>
    KMutexGuardMultiple<'a, N, Class, M, P>
{
    /// Sorts an array of lock references in-place by ascending memory address using insertion sort,
    /// matching C++ `GuardMultiple::InsertionSortPointers`.
    #[inline]
    fn insertion_sort_locks(locks: &mut [&'a KMutex<Class, M>; N]) {
        let mut i = 1;
        while i < N {
            let x = locks[i];
            let x_addr = x.mutex.as_mut_ptr() as usize;
            let mut j = i;
            while j > 0 && (locks[j - 1].mutex.as_mut_ptr() as usize) > x_addr {
                locks[j] = locks[j - 1];
                j -= 1;
            }
            locks[j] = x;
            i += 1;
        }
    }

    /// Creates a new stack-pinned multi-lock validation guard initialization block.
    pub fn new(locks: [&'a KMutex<Class, M>; N]) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: ExclusiveLockPolicy<M, AcquireArgs = ()>,
    {
        Self::new_with_args(locks, ())
    }

    /// Creates a new stack-pinned multi-lock validation guard initialization block with policy
    /// acquire arguments.
    pub fn new_with_args(
        mut locks: [&'a KMutex<Class, M>; N],
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        const {
            lockdep::validate_lock_flags(Class::FLAGS);
            // Prevent passing nestable locks to `KMutexGuardMultiple` to avoid inconsistency
            // due to mixing external ordering and address ordering.
            assert!(
                (Class::FLAGS & lockdep::LOCK_FLAGS_NESTABLE) == 0 && !P::EXTERNAL_ORDER,
                "Nestable lock classes and externally ordered policies cannot be used with \
                 KMutexGuardMultiple"
            );
        }
        Self::insertion_sort_locks(&mut locks);
        assert!(
            N <= 1 || core::mem::size_of::<M>() > 0,
            "KMutexGuardMultiple requires physical locks with non-zero size"
        );
        for i in 1..N {
            assert_ne!(
                locks[i - 1].mutex.as_mut_ptr(),
                locks[i].mutex.as_mut_ptr(),
                "KMutexGuardMultiple requires all locks to be distinct instances"
            );
        }
        // SAFETY: The closure initializes all fields of `KMutexGuardMultiple` in-place.
        unsafe {
            pin_init_from_closure(move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                let mutexes_ptr =
                    addr_of_mut!((*this).mutexes) as *mut Option<&'a KMutex<Class, M>>;
                let states_ptr = addr_of_mut!((*this).states) as *mut P::GuardState;

                for (i, lock) in locks.into_iter().enumerate() {
                    core::ptr::write(mutexes_ptr.add(i), Some(lock));
                    let state_addr = states_ptr.add(i);
                    core::ptr::write(state_addr, P::GuardState::default());
                    P::acquire(&lock.mutex, &mut *state_addr, args);
                }

                core::ptr::write(
                    addr_of_mut!((*this).tokens),
                    core::array::from_fn(|_| LockToken::new()),
                );

                Ok(())
            })
        }
    }

    /// Returns shared references to the lock proof tokens.
    #[inline]
    pub fn tokens(&self) -> &[LockToken<'a, Class>; N] {
        assert!(
            N == 0 || self.mutexes[0].is_some(),
            "cannot access tokens of a released KMutexGuardMultiple"
        );
        &self.tokens
    }

    /// Returns mutable references to the lock proof tokens.
    #[inline]
    pub fn tokens_mut(self: Pin<&mut Self>) -> &mut [LockToken<'a, Class>; N] {
        // SAFETY: Modifying the non-pinned `tokens` field does not violate pinning invariants.
        let me = unsafe { self.get_unchecked_mut() };
        assert!(
            N == 0 || me.mutexes[0].is_some(),
            "cannot access tokens of a released KMutexGuardMultiple"
        );
        &mut me.tokens
    }

    /// Explicitly releases all held locks in reverse order before the guard is dropped.
    ///
    /// If the locks have already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        // SAFETY: `states` are pinned on the stack and valid. Locks are released in reverse
        // acquisition order (matching C++ `GuardMultiple` destruction order).
        unsafe {
            let me = self.get_unchecked_mut();
            for i in (0..N).rev() {
                if let Some(mutex) = me.mutexes[i].take() {
                    P::release(&mutex.mutex, &mut me.states[i]);
                }
            }
        }
    }
}

#[pinned_drop]
impl<'a, const N: usize, Class: LockClass, M: RawLock, P: ExclusiveLockPolicy<M>> PinnedDrop
    for KMutexGuardMultiple<'a, N, Class, M, P>
{
    fn drop(self: Pin<&mut Self>) {
        self.release();
    }
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order, using the default policy.
#[inline]
pub fn guard_multiple<'a, const N: usize, Class: LockClass, M: RawLock>(
    locks: [&'a KMutex<Class, M>; N],
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible>
where
    <M as RawLock>::DefaultPolicy: ExclusiveLockPolicy<M, AcquireArgs = ()>,
{
    KMutexGuardMultiple::new(locks)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order with arguments, using the default policy.
#[inline]
pub fn guard_multiple_with<'a, const N: usize, Class: LockClass, M: RawLock>(
    locks: [&'a KMutex<Class, M>; N],
    args: <<M as RawLock>::DefaultPolicy as LockPolicy<M>>::AcquireArgs,
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M>, core::convert::Infallible> {
    KMutexGuardMultiple::new_with_args(locks, args)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order, using policy `P`.
#[inline]
pub fn guard_multiple_policy<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock,
    P: ExclusiveLockPolicy<M, AcquireArgs = ()>,
>(
    locks: [&'a KMutex<Class, M>; N],
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
    KMutexGuardMultiple::new(locks)
}

/// Acquires multiple `KMutex` instances of the same class and type in ascending memory address
/// order with policy `P` and arguments.
#[inline]
pub fn guard_multiple_policy_with<
    'a,
    const N: usize,
    Class: LockClass,
    M: RawLock,
    P: ExclusiveLockPolicy<M>,
>(
    locks: [&'a KMutex<Class, M>; N],
    args: P::AcquireArgs,
) -> impl PinInit<KMutexGuardMultiple<'a, N, Class, M, P>, core::convert::Infallible> {
    KMutexGuardMultiple::new_with_args(locks, args)
}

/// Type tag to indicate aliased lock acquisition.
pub struct AliasedLock;

/// Acquires an aliased lock on two `KMutex` instances that reference the same underlying lock.
///
/// Only `lock1` is physically acquired, but the resulting guard holds proof tokens for both
/// `Class1` and `Class2`.
#[inline]
pub fn aliased_lock<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2>, core::convert::Infallible>
where
    <M1 as RawLock>::DefaultPolicy: ExclusiveLockPolicy<M1, AcquireArgs = ()>,
{
    KMutexAliasedGuard::new(lock1, lock2)
}

/// Acquires an aliased lock with arguments on two `KMutex` instances that reference the same
/// underlying lock.
#[inline]
pub fn aliased_lock_with<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
    args: <<M1 as RawLock>::DefaultPolicy as LockPolicy<M1>>::AcquireArgs,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2>, core::convert::Infallible> {
    KMutexAliasedGuard::new_with_args(lock1, lock2, args)
}

/// Acquires an aliased lock with a specific policy on two `KMutex` instances that reference the
/// same underlying lock.
#[inline]
pub fn aliased_lock_policy<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock,
    M2: RawLock,
    P: ExclusiveLockPolicy<M1, AcquireArgs = ()>,
>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>, core::convert::Infallible> {
    KMutexAliasedGuard::new(lock1, lock2)
}

/// Acquires an aliased lock with a specific policy and arguments on two `KMutex` instances that
/// reference the same underlying lock.
#[inline]
pub fn aliased_lock_policy_with<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock,
    M2: RawLock,
    P: ExclusiveLockPolicy<M1>,
>(
    lock1: &'a KMutex<Class1, M1>,
    lock2: &'a KMutex<Class2, M2>,
    args: P::AcquireArgs,
) -> impl PinInit<KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>, core::convert::Infallible> {
    KMutexAliasedGuard::new_with_args(lock1, lock2, args)
}

/// A validation guard representing ownership of two aliased locks simultaneously.
///
/// Only the first lock is physically acquired, but proof tokens for both lock classes (`Class1` and
/// `Class2`) are provided.
#[pin_data]
pub struct KMutexAliasedGuard<
    'a,
    Class1: LockClass,
    Class2: LockClass,
    M1: RawLock,
    M2: RawLock = M1,
    P: ExclusiveLockPolicy<M1> = <M1 as RawLock>::DefaultPolicy,
> {
    #[pin]
    inner: KMutexGuard<'a, Class1, M1, P>,

    token2: LockToken<'a, Class2>,

    lock2: &'a KMutex<Class2, M2>,
}

impl<'a, Class1: LockClass, Class2: LockClass, M1: RawLock, M2: RawLock, P: ExclusiveLockPolicy<M1>>
    KMutexAliasedGuard<'a, Class1, Class2, M1, M2, P>
{
    /// Creates a new stack-pinned aliased validation guard initialization block.
    #[inline]
    pub fn new(
        lock1: &'a KMutex<Class1, M1>,
        lock2: &'a KMutex<Class2, M2>,
    ) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: ExclusiveLockPolicy<M1, AcquireArgs = ()>,
    {
        Self::new_with_args(lock1, lock2, ())
    }

    /// Creates a new stack-pinned aliased validation guard initialization block with policy acquire
    /// arguments.
    #[inline]
    pub fn new_with_args(
        lock1: &'a KMutex<Class1, M1>,
        lock2: &'a KMutex<Class2, M2>,
        args: P::AcquireArgs,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        const {
            // `Class1` is checked by the inner `KMutexGuard`. `Class2` aliases the same physical
            // lock and must mirror its flags, so it is held to the same requirements (stricter
            // than C++ `Guard(AssertOrderedAliasedLock, ...)`, which only asserts the physical
            // lock's class).
            lockdep::validate_lock_flags(Class2::FLAGS);
            assert!(
                !P::EXTERNAL_ORDER || (Class2::FLAGS & lockdep::LOCK_FLAGS_NESTABLE) != 0,
                "Externally ordered lock policies require a nestable lock class"
            );
        }
        assert!(
            core::mem::size_of::<M1>() > 0 && core::mem::size_of::<M2>() > 0,
            "AliasedLock requires physical locks with non-zero size"
        );
        assert_eq!(
            lock1.mutex.as_mut_ptr(),
            lock2.mutex.as_mut_ptr(),
            "AliasedLock requires lock1 and lock2 to point to the same physical lock"
        );
        pin_init!(Self {
            inner <- KMutexGuard::new_with_args(lock1, args),
            // SAFETY: `inner` holds the underlying mutex, which is aliased to represent `Class2`.
            token2: unsafe { LockToken::new() },
            lock2,
        })
    }

    #[inline]
    fn assert_alias_intact(inner: &KMutexGuard<'a, Class1, M1, P>, lock2: &KMutex<Class2, M2>) {
        let m1 = inner.mutex().expect("cannot access tokens of a released KMutexAliasedGuard");
        assert_eq!(
            m1.mutex.as_mut_ptr(),
            lock2.mutex.as_mut_ptr(),
            "KMutexAliasedGuard inner guard was retargeted to a different physical lock"
        );
    }

    /// Returns shared references to both lock proof tokens `(Class1, Class2)`.
    #[inline]
    pub fn tokens(&self) -> (&LockToken<'a, Class1>, &LockToken<'a, Class2>) {
        Self::assert_alias_intact(&self.inner, self.lock2);
        // SAFETY: `assert_alias_intact` verified `self.inner` holds the physical lock matching
        // `self.lock2`.
        (unsafe { self.inner.token_unchecked() }, &self.token2)
    }

    /// Returns simultaneous mutable references to both proof tokens `(Class1, Class2)`.
    ///
    /// # Safety
    ///
    /// Both returned tokens are backed by a single physical lock acquisition. The caller must
    /// ensure that the two mutable tokens are not used to obtain aliasing mutable references to
    /// the same `KCell` or `KOnceCell` (for example, by passing both tokens to `guard_<lock>_mut`
    /// on the same struct instance).
    #[inline]
    pub unsafe fn tokens_mut(
        self: Pin<&mut Self>,
    ) -> (&mut LockToken<'a, Class1>, &mut LockToken<'a, Class2>) {
        // SAFETY: `get_unchecked_mut` and `Pin::new_unchecked` project the pinned `inner` field.
        let (inner_pin, token2, lock2) = unsafe {
            let me = self.get_unchecked_mut();
            (Pin::new_unchecked(&mut me.inner), &mut me.token2, me.lock2)
        };
        Self::assert_alias_intact(&inner_pin, lock2);
        // SAFETY: `assert_alias_intact` verified `inner_pin` holds the physical lock matching
        // `lock2`.
        (unsafe { inner_pin.token_mut_unchecked() }, token2)
    }

    /// Returns a shared reference to the primary lock proof token (`Class1`).
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class1> {
        self.inner.token()
    }

    /// Returns a mutable reference to the primary lock proof token (`Class1`).
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class1> {
        self.inner_guard().token_mut()
    }

    /// Returns a pinned mutable reference to the primary inner `KMutexGuard`.
    #[inline]
    pub fn inner_guard(self: Pin<&mut Self>) -> Pin<&mut KMutexGuard<'a, Class1, M1, P>> {
        // SAFETY: `inner` is structurally pinned inside `Self`.
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().inner) }
    }

    /// Explicitly releases the underlying lock before the guard is dropped.
    ///
    /// If the lock has already been released, this is a no-op.
    #[inline]
    pub fn release(self: Pin<&mut Self>) {
        self.inner_guard().release();
    }

    /// Temporarily releases the lock before executing the given callable `f` and then
    /// re-acquires the lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        self.inner_guard().call_unlocked(f)
    }

    /// Calls a closure while temporarily disabling lockdep tracking for the lock held by this
    /// guard.
    #[inline]
    pub fn call_untracked<R, F: FnOnce(&mut LockToken<'a, Class1>) -> R>(
        self: Pin<&mut Self>,
        f: F,
    ) -> R {
        self.inner_guard().call_untracked(f)
    }
}

#[cfg(not(feature = "kernel"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KCell, RawMutex, guarded};
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
    use lockdep::LockClass;
    use pin_init::{pin_init, stack_pin_init};

    struct MyClass;
    impl LockClass for MyClass {
        const ID: *mut core::ffi::c_void = core::ptr::null_mut();
        const FLAGS: lockdep::LockFlags = lockdep::LOCK_FLAGS_NONE;
    }

    #[pin_init::pin_data]
    struct MyStruct {
        #[pin]
        mu: KMutex<MyClass, RawMutex>,
        data1: KCell<u32, MyClass>,
        data2: KCell<i32, MyClass>,
    }

    #[test]
    fn test_basic_token_access() {
        stack_pin_init!(let s = pin_init!(MyStruct {
            mu <- KMutex::init(),
            data1: KCell::new(10),
            data2: KCell::new(-5),
        }));

        lock!(let mut guard = s.mu.lock());

        // SAFETY: `guard` holds `s.mu`, which guards `s.data1` and `s.data2`.
        unsafe {
            assert_eq!(*s.data1.get(guard.token()), 10);
            assert_eq!(*s.data2.get(guard.token()), -5);
        }
        // SAFETY: `guard` holds `s.mu`, which guards `s.data1`.
        unsafe {
            let token_mut = guard.as_mut().token_mut();
            *s.data1.get_mut(token_mut) = 20;
            assert_eq!(*s.data1.get(guard.token()), 20);
        }
    }

    #[guarded]
    struct MyGuardedStruct {
        #[mutex]
        mu: KMutex<RawMutex>,
        #[guarded_by(mu)]
        data1: u32,
        #[guarded_by(mu)]
        data2: i32,
    }

    #[test]
    fn test_macro_guarded() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 100.into(),
            data2: (-50).into(),
        }));

        {
            lock!(let mut guard = s.lock_mu());

            // Safe individual field access
            assert_eq!(*guard.data1(), 100);
            assert_eq!(*guard.data2(), -50);

            *guard.as_mut().data1_mut() = 200;
            assert_eq!(*guard.data1(), 200);

            // Safe disjoint/split access
            let fields = guard.as_mut().fields_mut();
            *fields.data1 += 50;
            *fields.data2 += 50;
        }

        // Verify fields
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data1(), 250);
        assert_eq!(*guard.data2(), 0);
    }

    #[test]
    fn test_kmutex_init() {
        stack_pin_init!(let mu = KMutex::<MyClass, RawMutex>::init());
        lock!(mu.lock());
    }

    #[test]
    fn test_lockdep_tracked_as_ref() {
        stack_pin_init!(let mu = KMutex::<MyClass, RawMutex>::init());
        let tracked: &RawMutex = mu.raw_mutex();
        let storage: &crate::RawMutexStorage = tracked.as_ref();
        assert!(core::ptr::eq(storage, &**tracked));
    }

    #[test]
    fn test_kmutex_debug() {
        extern crate std;
        stack_pin_init!(let mu = KMutex::<MyClass, RawMutex>::init());
        let debug_str = std::format!("{:?}", mu);
        assert!(debug_str.contains("KMutex"));
    }

    #[guarded]
    struct MyMultiGuardedStruct {
        #[mutex]
        mu1: KMutex<RawMutex>,
        #[mutex]
        mu2: KMutex<RawMutex>,
        #[guarded_by(mu1)]
        data1: u32,
        #[guarded_by(mu2)]
        data2: i32,
    }

    #[test]
    fn test_macro_multi_guarded() {
        stack_pin_init!(let s = pin_init!(MyMultiGuardedStruct {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard1 = s.lock_mu1());
        lock!(let mut guard2 = s.lock_mu2());

        assert_eq!(*guard1.data1(), 10);
        assert_eq!(*guard2.data2(), 20);
        *guard1.as_mut().data1_mut() = 15;
        *guard2.as_mut().data2_mut() = 25;
        assert_eq!(*guard1.data1(), 15);
        assert_eq!(*guard2.data2(), 25);
    }

    #[guarded]
    struct MyDefaultGuardedStruct {
        #[mutex]
        mu: KMutex<RawMutex>,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_derive_default_guarded() {
        stack_pin_init!(let s = pin_init!(MyDefaultGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);
    }

    #[guarded]
    struct MyGenericLockGuardedStruct<L: RawLock> {
        #[mutex]
        mu: KMutex<L>,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_macro_generic_lock_guarded() {
        fn lock_generic<L: RawLock>(s: &MyGenericLockGuardedStruct<L>) -> u32
        where
            L::DefaultPolicy: ExclusiveLockPolicy<L>,
            <L::DefaultPolicy as LockPolicy<L>>::AcquireArgs: Default,
        {
            lock!(let guard = s.lock_mu_with(Default::default()));
            *guard.data()
        }

        stack_pin_init!(let s1 = pin_init!(MyGenericLockGuardedStruct::<RawMutex> {
            mu <- KMutex::init(),
            data: 100.into(),
        }));
        assert_eq!(lock_generic(&s1), 100);

        stack_pin_init!(let s2 = pin_init!(MyGenericLockGuardedStruct::<RawMutex> {
            mu <- KMutex::init(),
            data: 200.into(),
        }));
        assert_eq!(lock_generic(&s2), 200);
    }

    #[guarded]
    struct MyGenericGuardedStruct<T> {
        #[mutex]
        mu: KMutex<RawMutex>,
        #[guarded_by(mu)]
        data: T,
    }

    #[test]
    fn test_macro_generic_guarded() {
        stack_pin_init!(let s = pin_init!(MyGenericGuardedStruct::<u32> {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let mut guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);

        *guard.as_mut().data_mut() = 42;
        assert_eq!(*guard.data(), 42);

        let fields = guard.as_mut().fields_mut();
        *fields.data = 100;

        let fields_shared = guard.fields();
        assert_eq!(*fields_shared.data, 100);
    }

    #[guarded]
    struct MyExplicitParentGuardedStruct {
        #[mutex]
        mu: KMutex<RawMutex>,
        #[guarded_by(mu)]
        data: u32,
        pub label: &'static str,
    }

    impl MyExplicitParentGuardedStruct {
        pub fn has_label(&self) -> bool {
            !self.label.is_empty()
        }
    }

    impl<'a> MyExplicitParentGuardedStructMuGuard<'a> {
        pub fn process_with_context(mut self: Pin<&mut Self>) {
            let has_label = self.parent.has_label();
            let label = self.parent.label;
            if has_label && label == "apply_update" {
                let fields = self.as_mut().fields_mut();
                *fields.data = 100;
            }
        }
    }

    #[test]
    fn test_macro_guard_explicit_parent_access() {
        stack_pin_init!(let s = pin_init!(MyExplicitParentGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
            label: "apply_update",
        }));

        {
            lock!(let mut guard = s.lock_mu());
            guard.as_mut().process_with_context();
        }

        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 100);
    }

    #[test]
    fn test_call_unlocked() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        assert_eq!(*guard.data1(), 10);

        let unlocked_result = guard.as_mut().call_unlocked(|| {
            // During call_unlocked, the lock is temporarily released.
            42
        });
        assert_eq!(unlocked_result, 42);

        // After call_unlocked returns, the lock is held again and fields can be accessed/modified.
        *guard.as_mut().data1_mut() = 99;
        assert_eq!(*guard.data1(), 99);
    }

    #[guarded]
    struct AliasedTargetStruct {
        #[mutex(MyGuardedStructMuClass)]
        mu: KMutex<crate::PhantomMutex>,
        parent: *const MyGuardedStruct,
        #[guarded_by(mu)]
        value: u32,
    }

    impl AliasedTargetStruct {
        fn lock(&self) -> &KMutex<MyGuardedStructMuClass, RawMutex> {
            // SAFETY: `self.parent` points to a live `MyGuardedStruct` in the test scope.
            unsafe { &(*self.parent).mu }
        }
    }

    #[test]
    fn test_aliased_lock_basic() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 100.into(),
            data2: 200.into(),
        }));

        let s_ptr: *const MyGuardedStruct = &*s;
        stack_pin_init!(let target = pin_init!(AliasedTargetStruct {
            mu: KMutex::new(crate::PhantomMutex),
            parent: s_ptr,
            value: 300.into(),
        }));

        {
            lock!(let mut guard = aliased_lock(&s.mu, target.lock()));

            // Access fields of s and target using tokens() pair
            let (t1, t2) = guard.tokens();
            // SAFETY: `t1` and `t2` are backed by `s.mu`, which guards `s` and `target`.
            unsafe {
                assert_eq!(*s.guard_mu(t1).data1(), 100);
                assert_eq!(*s.guard_mu(t1).data2(), 200);
                assert_eq!(*target.guard_mu(t2).value(), 300);
            }

            // Disjoint simultaneous mutable access to both structs
            // SAFETY: `s` and `target` are distinct structs with disjoint `KCell` fields.
            unsafe {
                let (t1_mut, t2_mut) = guard.as_mut().tokens_mut();
                *s.guard_mu_mut(t1_mut).data1_mut() = 101;
                *target.guard_mu_mut(t2_mut).value_mut() = 301;
            }

            // Call unlocked on aliased guard
            let res = guard.as_mut().call_unlocked(|| 1234);
            assert_eq!(res, 1234);

            // Verify mutations persist
            let (t1, t2) = guard.tokens();
            // SAFETY: `t1` and `t2` are backed by `s.mu`, which guards `s` and `target`.
            unsafe {
                assert_eq!(*s.guard_mu(t1).data1(), 101);
                assert_eq!(*target.guard_mu(t2).value(), 301);
            }
        }

        // Verify state with regular lock
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data1(), 101);
    }

    #[test]
    fn test_aliased_lock_macro_method() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        let s_ptr: *const MyGuardedStruct = &*s;
        stack_pin_init!(let target = pin_init!(AliasedTargetStruct {
            mu: KMutex::new(crate::PhantomMutex),
            parent: s_ptr,
            value: 30.into(),
        }));

        {
            lock!(let mut guard = s.lock_mu_aliased(target.lock()));
            let (_t1, t2) = guard.tokens();
            // SAFETY: `target` is guarded by `s.mu` and `_t1_mut` is unused.
            unsafe {
                assert_eq!(*target.guard_mu(t2).value(), 30);
                let (_t1_mut, t2_mut) = guard.as_mut().tokens_mut();
                *target.guard_mu_mut(t2_mut).value_mut() = 35;
            }
        }

        lock!(let guard = s.lock_mu());
        // SAFETY: `guard` holds `s.mu`, which protects `target`.
        unsafe {
            assert_eq!(*target.guard_mu(guard.token()).value(), 35);
        }
    }

    #[guarded]
    struct FlaggedMutexStruct {
        #[mutex(flags = lockdep::LOCK_FLAGS_ACTIVE_LIST_DISABLED)]
        seek_lock: KMutex<RawMutex>,
        #[guarded_by(seek_lock)]
        seek: u64,
    }

    #[test]
    fn test_flagged_mutex_struct() {
        stack_pin_init!(let s = pin_init!(FlaggedMutexStruct {
            seek_lock <- KMutex::init(),
            seek: 0.into(),
        }));

        lock!(let mut guard = s.lock_seek_lock());
        assert_eq!(*guard.seek(), 0);
        *guard.as_mut().seek_mut() = 1024;
        assert_eq!(*guard.seek(), 1024);
    }

    #[guarded]
    struct NestableGuardedStruct {
        #[mutex(flags = lockdep::LOCK_FLAGS_NESTABLE)]
        mu: KMutex<RawMutex>,
        #[guarded_by(mu)]
        data: u32,
    }

    /// `OrderedLockPolicy` over the host `RawMutexPolicy`, through the generated
    /// `lock_<mu>_policy_with` / `lock_<mu>_aliased_policy_with` helpers and
    /// `KMutex::lock_policy_with`, including `call_unlocked` and `call_untracked`.
    #[test]
    fn test_ordered_lock_policy() {
        type Ordered = crate::OrderedLockPolicy<crate::raw_userspace_mutex::RawMutexPolicy>;

        stack_pin_init!(let a = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 1.into(),
        }));
        stack_pin_init!(let b = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 2.into(),
        }));

        {
            lock!(let mut ga = a.lock_mu_policy_with::<Ordered>(0));
            lock!(let mut gb = b.lock_mu_policy_with::<Ordered>(1));
            assert_eq!(*ga.data(), 1);
            assert_eq!(*gb.data(), 2);

            let sum = gb.as_mut().call_unlocked(|| *ga.data() + 10);
            *gb.as_mut().data_mut() = sum;

            let data = gb.as_mut().call_untracked(|token| {
                // SAFETY: `token` was acquired from `b.mu`.
                *unsafe { b.guard_mu(token) }.data()
            });
            assert_eq!(data, 11);
            *ga.as_mut().data_mut() = 3;
        }

        {
            lock!(let gb = b.mu.lock_policy_with::<Ordered>(0));
            lock!(let ga = a.lock_mu_aliased_policy_with::<_, _, Ordered>(&a.mu, 1));
            let (t1, _t2) = ga.tokens();
            // SAFETY: `t1` was acquired from `a.mu`.
            let guarded_a = unsafe { a.guard_mu(t1) };
            assert_eq!(*guarded_a.data(), 3);
            assert_eq!(*b.guard_mu_from_guard(&gb).data(), 11);
        }

        lock!(let guard = a.lock_mu());
        assert_eq!(*guard.data(), 3);
    }

    #[test]
    fn test_guard_release() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        *guard.as_mut().data1_mut() = 50;
        guard.as_mut().release();
        // Calling release again is a no-op.
        guard.as_mut().release();

        // Since `guard` was explicitly released, we can re-acquire the lock in the same scope.
        lock!(let guard2 = s.lock_mu());
        assert_eq!(*guard2.data1(), 50);
    }

    #[test]
    fn test_guard_multiple() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 3.into(),
            data2: 4.into(),
        }));

        // Acquire in [s1, s2] order.
        {
            lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s2]));
            let [t1, t2] = guards.as_mut().tokens_mut();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`, and `s1` and `s2` are
            // distinct instances.
            unsafe {
                *s1.guard_mu_mut(t1).data1_mut() = 10;
                *s2.guard_mu_mut(t2).data1_mut() = 30;
            }
        }

        // Acquire in reverse [s2, s1] order and release early.
        {
            lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s2, &*s1]));
            let [t1, t2] = guards.tokens();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`.
            unsafe {
                assert_eq!(*s1.guard_mu(t1).data1(), 10);
                assert_eq!(*s2.guard_mu(t2).data1(), 30);
            }
            guards.as_mut().release();

            // Verify both locks were released by re-acquiring them individually.
            lock!(let g1 = s1.lock_mu());
            lock!(let g2 = s2.lock_mu());
            assert_eq!(*g1.data1(), 10);
            assert_eq!(*g2.data1(), 30);
        }
    }

    #[test]
    #[should_panic(expected = "cannot access token of a released KMutexGuard")]
    fn test_released_guard_token_panics() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let mut guard = s.mu.lock());
        guard.as_mut().release();
        let _ = guard.token();
    }

    #[test]
    #[should_panic(expected = "cannot access token of a released KMutexGuard")]
    fn test_released_guard_field_access_panics() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let mut guard = s.lock_mu());
        guard.as_mut().release();
        let _ = guard.as_mut().data1_mut();
    }

    #[test]
    #[should_panic(expected = "cannot access tokens of a released KMutexGuardMultiple")]
    fn test_released_guard_multiple_tokens_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 3.into(),
            data2: 4.into(),
        }));

        lock!(let mut guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s2]));
        guards.as_mut().release();
        let _ = guards.tokens();
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_safe_guard_from_guard_and_token_guard_methods() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        {
            lock!(let mut guard = s.mu.lock());
            {
                let tg = s.guard_mu_from_guard(&guard);
                assert_eq!(*tg.data1(), 10);
                assert_eq!(*tg.data2(), 20);
                assert!(core::ptr::eq(tg.parent(), &*s));
            }
            {
                let mut tg_mut = s.guard_mu_mut_from_guard(guard.as_mut());
                assert!(core::ptr::eq(tg_mut.parent(), &*s));
                *tg_mut.reborrow().data1_mut() = 11;
                assert_eq!(*tg_mut.as_token_guard().data1(), 11);
            }
        }

        {
            lock!(let mut guard = s.lock_mu());
            assert!(core::ptr::eq(guard.parent(), &*s));
            assert_eq!(*guard.as_token_guard().data1(), 11);
            let mut tg_mut = guard.as_mut().as_token_guard_mut();
            *tg_mut.reborrow().data2_mut() = 22;
            assert_eq!(*tg_mut.as_token_guard().data2(), 22);
        }
    }

    #[test]
    #[should_panic(expected = "KMutexGuard does not belong to this struct's mutex")]
    fn test_guard_mu_from_guard_mismatched_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let guard2 = s2.mu.lock());
        let _ = s1.guard_mu_from_guard(&guard2);
    }

    #[test]
    #[should_panic(
        expected = "AliasedLock requires lock1 and lock2 to point to the same physical lock"
    )]
    fn test_aliased_lock_mismatched_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let _guard = aliased_lock(&s1.mu, &s2.mu));
    }

    #[test]
    #[should_panic(expected = "KMutexGuardMultiple requires all locks to be distinct instances")]
    fn test_guard_multiple_duplicate_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));

        lock!(let _guards = MyGuardedStruct::lock_mu_multiple([&*s1, &*s1]));
    }

    #[guarded]
    struct SharedLockGuardedStruct {
        #[mutex]
        mu1: KMutex<RawMutex>,
        #[mutex]
        mu2: KMutex<RawMutex>,
        #[mutex]
        mu3: KMutex<RawMutex>,
        #[guarded_by(mu1)]
        only1: u32,
        #[guarded_by(mu2)]
        only2: u32,
        #[guarded_by(mu1, mu2)]
        shared12: u32,
        #[guarded_by(mu1)]
        #[guarded_by(mu2)]
        #[pin]
        pinned_shared12: u32,
        #[guarded_by(mu1, mu2)]
        once_shared12: crate::KOnceCell<u32>,
        #[guarded_by(mu1, mu2, mu3)]
        shared123: u32,
    }

    impl SharedLockGuardedStruct {
        fn init() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                mu1 <- KMutex::init(),
                mu2 <- KMutex::init(),
                mu3 <- KMutex::init(),
                only1: 1.into(),
                only2: 2.into(),
                shared12: 12.into(),
                pinned_shared12: 120.into(),
                once_shared12: crate::KOnceCell::new(),
                shared123: 123.into(),
            })
        }
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_guarded_by_multiple_locks() {
        stack_pin_init!(let s = SharedLockGuardedStruct::init());

        // Reading `shared12`, `pinned_shared12`, `once_shared12`, and `shared123` is allowed when
        // holding only `mu1`.
        {
            lock!(let mut g1 = s.lock_mu1());
            assert_eq!(*g1.only1(), 1);
            assert_eq!(*g1.shared12(), 12);
            assert_eq!(*g1.pinned_shared12(), 120);
            assert_eq!(g1.once_shared12(), None);
            assert_eq!(*g1.shared123(), 123);

            let f1 = g1.fields();
            assert_eq!(*f1.only1, 1);
            assert_eq!(*f1.shared12, 12);
            assert_eq!(*f1.pinned_shared12, 120);
            assert_eq!(f1.once_shared12, None);
            assert_eq!(*f1.shared123, 123);

            // In `fields_mut()`, `only1` is mutable while multi-lock fields are read-only.
            let f1_mut = g1.as_mut().fields_mut();
            *f1_mut.only1 = 10;
            assert_eq!(*f1_mut.shared12, 12);
            assert_eq!(*f1_mut.pinned_shared12, 120);
            assert_eq!(f1_mut.once_shared12, None);
            assert_eq!(*f1_mut.shared123, 123);
        }

        // Reading is also allowed when holding only `mu2`.
        {
            lock!(let g2 = s.lock_mu2());
            assert_eq!(*g2.only2(), 2);
            assert_eq!(*g2.shared12(), 12);
            assert_eq!(*g2.pinned_shared12(), 120);
            assert_eq!(g2.once_shared12(), None);
            assert_eq!(*g2.shared123(), 123);
        }

        // Mutating `shared12`, `pinned_shared12`, and `once_shared12` when holding both `mu1` and
        // `mu2` (via either guard or token guards).
        {
            lock!(let mut g1 = s.lock_mu1());
            lock!(let mut g2 = s.lock_mu2());

            *g1.as_mut().shared12_mut(&mut g2) = 42;
            assert_eq!(*g1.shared12(), 42);
            assert_eq!(*g2.shared12(), 42);

            *g2.as_mut().shared12_mut(&mut g1) = 84;
            assert_eq!(*g1.shared12(), 84);

            *g1.as_mut().pinned_shared12_mut(&mut g2) = 240;
            assert_eq!(*g2.pinned_shared12(), 240);

            let mut cell = g2.as_mut().once_shared12_cell(&mut g1);
            let val = cell.get_or_init(|| 777);
            assert_eq!(*val, 777);
            assert_eq!(g1.once_shared12(), Some(&777));
            assert_eq!(g2.once_shared12(), Some(&777));

            if let Some(v) = g1.as_mut().once_shared12_mut(&mut g2) {
                *v = 888;
            }
            assert_eq!(g2.once_shared12(), Some(&888));

            // TokenGuardMut access for standard, pinned, and KOnceCell multi-lock fields.
            let mut tg1 = g1.as_mut().as_token_guard_mut();
            let mut tg2 = g2.as_mut().as_token_guard_mut();
            *tg1.reborrow().shared12_mut(&mut tg2) = 99;
            assert_eq!(*tg2.shared12(), 99);
            assert_eq!(*tg2.as_token_guard().shared12(), 99);
            *tg2.reborrow().shared12_mut(&mut tg1) = 100;
            assert_eq!(*tg1.shared12(), 100);
            assert_eq!(*tg1.as_token_guard().shared12(), 100);

            assert_eq!(*tg1.pinned_shared12(), 240);
            *tg1.reborrow().pinned_shared12_mut(&mut tg2) = 360;
            assert_eq!(*tg2.pinned_shared12(), 360);
            assert_eq!(*tg2.as_token_guard().pinned_shared12(), 360);

            assert_eq!(tg1.once_shared12(), Some(&888));
            assert_eq!(tg1.as_token_guard().once_shared12(), Some(&888));
            if let Some(v) = tg1.reborrow().once_shared12_mut(&mut tg2) {
                *v = 999;
            }
            assert_eq!(tg2.reborrow().once_shared12_cell(&mut tg1).get(), Some(&999));
            assert_eq!(tg1.fields().once_shared12, Some(&999));
            assert_eq!(tg1.reborrow().fields_mut().once_shared12, Some(&999));
            assert_eq!(g1.as_mut().fields_mut().once_shared12, Some(&999));

            // Mutating 3-lock field `shared123` requires all 3 locks.
            lock!(let mut g3 = s.lock_mu3());
            *g1.as_mut().shared123_mut(&mut g2, &mut g3) = 321;
            assert_eq!(*g3.shared123(), 321);
            *g3.as_mut().shared123_mut(&mut g1, &mut g2) = 654;
            assert_eq!(*g1.shared123(), 654);
        }
    }

    #[test]
    #[should_panic(expected = "Lock guard belongs to a different struct instance")]
    fn test_multi_guarded_wrong_instance_panics() {
        stack_pin_init!(let s1 = SharedLockGuardedStruct::init());
        stack_pin_init!(let s2 = SharedLockGuardedStruct::init());

        lock!(let mut g1 = s1.lock_mu1());
        lock!(let mut g2_wrong = s2.lock_mu2());

        // Passing `g2_wrong` (locked on `s2`) to `g1` (locked on `s1`) must panic.
        *g1.as_mut().shared12_mut(&mut g2_wrong) = 42;
    }

    #[test]
    #[should_panic(expected = "Lock guard belongs to a different struct instance")]
    fn test_multi_guarded_token_guard_wrong_instance_panics() {
        stack_pin_init!(let s1 = SharedLockGuardedStruct::init());
        stack_pin_init!(let s2 = SharedLockGuardedStruct::init());

        lock!(let mut g1 = s1.lock_mu1());
        lock!(let mut g2_wrong = s2.lock_mu2());

        let mut tg1 = g1.as_mut().as_token_guard_mut();
        let mut tg2_wrong = g2_wrong.as_mut().as_token_guard_mut();

        // Passing `tg2_wrong` (bound to `s2`) to `tg1` (bound to `s1`) must panic.
        *tg1.shared12_mut(&mut tg2_wrong) = 42;
    }

    #[ksync::guarded]
    struct GenericMultiLockStruct<'lt, T, const N: usize> {
        #[mutex]
        mu1: KMutex<RawMutex>,
        #[mutex]
        mu2: KMutex<RawMutex>,
        #[guarded_by(mu1)]
        slice_ref: &'lt [T; N],
        #[guarded_by(mu1, mu2)]
        shared: [T; N],
    }

    #[test]
    #[forbid(unsafe_code)]
    fn test_generic_multi_lock_struct() {
        static BACKING: [u32; 2] = [10, 20];
        stack_pin_init!(let s = pin_init!(GenericMultiLockStruct::<u32, 2> {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            slice_ref: (&BACKING).into(),
            shared: [1u32, 2u32].into(),
        }));

        lock!(let mut g1 = s.lock_mu1());
        assert_eq!(*g1.slice_ref(), &[10, 20]);
        assert_eq!(*g1.shared(), [1, 2]);
        assert_eq!(*g1.fields().shared, [1, 2]);
        assert_eq!(*g1.as_mut().fields_mut().shared, [1, 2]);

        lock!(let mut g2 = s.lock_mu2());
        assert_eq!(*g2.shared(), [1, 2]);
        *g1.as_mut().shared_mut(&mut g2) = [30, 40];
        assert_eq!(*g2.shared(), [30, 40]);
    }

    #[ksync::guarded]
    struct PhantomMultiLockStruct {
        #[mutex(MyGuardedStructMuClass)]
        phantom_mu: KMutex<crate::PhantomMutex>,
        #[mutex]
        local_mu: KMutex<RawMutex>,
        #[guarded_by(phantom_mu, local_mu)]
        shared: u32,
    }

    #[test]
    fn test_phantom_and_custom_class_multi_lock() {
        stack_pin_init!(let real_obj = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 0.into(),
            data2: 0.into(),
        }));
        stack_pin_init!(let s = pin_init!(PhantomMultiLockStruct {
            phantom_mu: KMutex::new(crate::PhantomMutex),
            local_mu <- KMutex::init(),
            shared: 50.into(),
        }));

        // SAFETY: `real_obj.mu` is the physical lock synchronizing `s.phantom_mu`.
        lock!(let mut pg = unsafe { s.lock_phantom_mu(&real_obj.mu) });
        assert_eq!(*pg.shared(), 50);

        lock!(let mut lg = s.lock_local_mu());
        assert_eq!(*lg.shared(), 50);

        *pg.as_mut().shared_mut(&mut lg) = 60;
        assert_eq!(*lg.shared(), 60);

        *lg.as_mut().shared_mut(&mut pg) = 70;
        assert_eq!(*pg.shared(), 70);
    }

    #[ksync::guarded]
    struct UntrackedGuardedStruct {
        #[mutex]
        mu1: KMutex<crate::RawMutexStorage>,
        #[mutex(untracked)]
        mu2: KMutex<crate::RawMutexStorage>,
        #[guarded_by(mu1)]
        val1: u32,
        #[guarded_by(mu2)]
        val2: u32,
    }

    #[test]
    fn test_untracked_guarded_and_as_mut_ptr() {
        stack_pin_init!(let tracked = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        assert!(core::ptr::eq(
            <crate::RawMutex as crate::RawLock>::as_mut_ptr(tracked.mu.raw_mutex()),
            <crate::RawMutexStorage as crate::RawLock>::as_mut_ptr(&**tracked.mu.raw_mutex()),
        ));

        stack_pin_init!(let untracked = pin_init!(UntrackedGuardedStruct {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            val1: 10.into(),
            val2: 20.into(),
        }));
        assert!(<UntrackedGuardedStructMu1Class as LockClass>::ID.is_null());
        assert!(<UntrackedGuardedStructMu2Class as LockClass>::ID.is_null());

        lock!(let mut g1 = untracked.lock_mu1());
        lock!(let mut g2 = untracked.lock_mu2());
        assert_eq!(*g1.val1(), 10);
        assert_eq!(*g2.val2(), 20);
        *g1.as_mut().val1_mut() = 11;
        *g2.as_mut().val2_mut() = 21;
        assert_eq!(*g1.val1(), 11);
        assert_eq!(*g2.val2(), 21);
    }

    #[pin_init::pin_data]
    struct MockIrqLock {
        held: core::sync::atomic::AtomicBool,
        restored_irq: core::sync::atomic::AtomicU32,
    }

    impl MockIrqLock {
        fn acquire_raw(&self) {
            assert!(!self.held.swap(true, core::sync::atomic::Ordering::Acquire));
        }

        fn acquire_irq_save_raw(&self, saved: u32) -> u32 {
            assert!(!self.held.swap(true, core::sync::atomic::Ordering::Acquire));
            saved
        }

        fn is_held(&self) -> bool {
            self.held.load(core::sync::atomic::Ordering::Relaxed)
        }
    }

    struct MockIrqLockPolicy;

    // SAFETY: Single-threaded test mock enforcing `held` state transitions.
    unsafe impl LockPolicy<MockIrqLock> for MockIrqLockPolicy {
        type AcquireArgs = ();
        type GuardState = ();

        unsafe fn acquire(lock: &MockIrqLock, _state: &mut (), _args: ()) {
            lock.acquire_raw();
        }

        unsafe fn reacquire(lock: &MockIrqLock, _state: &mut ()) {
            lock.acquire_raw();
        }

        unsafe fn release(lock: &MockIrqLock, _state: &mut ()) {
            assert!(lock.held.swap(false, core::sync::atomic::Ordering::Release));
        }
    }

    // SAFETY: `MockIrqLockPolicy` enforces exclusive mutual exclusion in single-threaded tests.
    unsafe impl ExclusiveLockPolicy<MockIrqLock> for MockIrqLockPolicy {}

    // SAFETY: `MockIrqLock` has non-zero size and a unique address per instance.
    unsafe impl RawLock for MockIrqLock {
        type DefaultPolicy = MockIrqLockPolicy;

        unsafe fn init(
            _class_id: *const core::ffi::c_void,
        ) -> impl PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                held: core::sync::atomic::AtomicBool::new(false),
                restored_irq: core::sync::atomic::AtomicU32::new(0),
            })
        }

        fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
            core::ptr::from_ref(self).cast_mut().cast()
        }
    }

    impl RawAssertHeldLock for MockIrqLock {
        fn assert_held(&self) {
            assert!(self.is_held());
        }
    }

    impl RawUntrackedLock for MockIrqLock {
        unsafe fn release(&self) {
            assert!(self.held.swap(false, core::sync::atomic::Ordering::Release));
        }
    }

    impl RawUntrackedIrqLock for MockIrqLock {
        type InterruptState = u32;

        unsafe fn release_irq_restore(&self, state: u32) {
            assert!(self.held.swap(false, core::sync::atomic::Ordering::Release));
            self.restored_irq.store(state, core::sync::atomic::Ordering::Relaxed);
        }
    }

    impl RawUntrackedStatefulLock for MockIrqLock {
        type State = u32;

        /// # Safety
        ///
        /// `self` must be held by the caller in this single-threaded test.
        unsafe fn release_with(&self, state: u32) {
            assert!(self.held.swap(false, core::sync::atomic::Ordering::Release));
            self.restored_irq.store(state, core::sync::atomic::Ordering::Relaxed);
        }
    }

    #[ksync::guarded]
    struct MockUntrackedIrqObj {
        #[mutex]
        mu: KMutex<MockIrqLock>,
        #[guarded_by(mu)]
        val: u32,
    }

    #[ksync::guarded]
    struct MockTrackedIrqObj {
        #[mutex]
        mu: KMutex<crate::LockdepTracked<MockIrqLock>>,
        #[guarded_by(mu)]
        val: u32,
    }

    #[test]
    fn test_assert_held_and_untracked_release() {
        stack_pin_init!(let obj = pin_init!(MockUntrackedIrqObj {
            mu <- KMutex::init(),
            val: 7.into(),
        }));

        obj.mu.raw_mutex().acquire_raw();
        {
            // SAFETY: `obj.mu` is held via `acquire_raw` and `token` is consumed by `release`.
            let token = unsafe { obj.mu.assert_held() };
            // SAFETY: `token` witnesses `obj.mu` on `obj`.
            assert_eq!(*unsafe { obj.guard_mu(&token) }.val(), 7);
            // SAFETY: `obj.mu` is held via `acquire_raw` and no guarded borrows remain live.
            unsafe { obj.mu.release(token) };
        }
        assert!(!obj.mu.raw_mutex().is_held());

        let saved = obj.mu.raw_mutex().acquire_irq_save_raw(42);
        {
            // SAFETY: `obj.mu` is held via `acquire_irq_save_raw` and `token` is consumed below.
            let mut token = unsafe { obj.mu.assert_held() };
            // SAFETY: `token` witnesses `obj.mu` on `obj`.
            *unsafe { obj.guard_mu_mut(&mut token) }.val_mut() = 99;
            // SAFETY: `obj.mu` is held with saved state `saved` and no guarded borrows remain live.
            unsafe { obj.mu.release_irq_restore(saved, token) };
        }
        assert!(!obj.mu.raw_mutex().is_held());
        assert_eq!(obj.mu.raw_mutex().restored_irq.load(core::sync::atomic::Ordering::Relaxed), 42);

        let saved = obj.mu.raw_mutex().acquire_irq_save_raw(77);
        {
            // SAFETY: `obj.mu` is held via `acquire_irq_save_raw` and `token` is consumed below.
            let mut token = unsafe { obj.mu.assert_held() };
            // SAFETY: `token` witnesses `obj.mu` on `obj`.
            *unsafe { obj.guard_mu_mut(&mut token) }.val_mut() = 55;
            // SAFETY: `obj.mu` is held with state `saved` and no guarded borrows remain live.
            unsafe { obj.mu.release_with(saved, token) };
        }
        assert!(!obj.mu.raw_mutex().is_held());
        assert_eq!(obj.mu.raw_mutex().restored_irq.load(core::sync::atomic::Ordering::Relaxed), 77);

        stack_pin_init!(let tracked = pin_init!(MockTrackedIrqObj {
            mu <- KMutex::init(),
            val: 123.into(),
        }));
        lock!(let guard = tracked.lock_mu());
        // SAFETY: `guard` holds `tracked.mu` and `token` is only used for shared access while
        // `guard` is live.
        let token = unsafe { tracked.mu.assert_held() };
        // SAFETY: `token` witnesses `tracked.mu` on `tracked`.
        assert_eq!(*unsafe { tracked.guard_mu(&token) }.val(), 123);
        assert_eq!(*guard.val(), 123);
    }

    #[test]
    fn test_try_lock_policy_macro() {
        type DefaultMutexPolicy = <crate::RawMutex as crate::RawLock>::DefaultPolicy;

        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 7.into(),
            data2: 8.into(),
        }));

        crate::try_lock!(let g = s.try_lock_mu_policy::<DefaultMutexPolicy>());
        let mut g = g.expect("uncontested try_lock_mu_policy should succeed");
        assert_eq!(*g.data1(), 7);
        *g.as_mut().data1_mut() = 9;
        assert_eq!(*g.data1(), 9);

        // A second non-blocking acquisition while `g` holds `s.mu` must fail with `Err(())`.
        crate::try_lock!(let g2 = s.try_lock_mu_policy::<DefaultMutexPolicy>());
        assert!(g2.is_err());

        g.release();

        // After releasing `g`, `try_lock_mu_policy` succeeds again.
        crate::try_lock!(let g3 = s.try_lock_mu_policy::<DefaultMutexPolicy>());
        let g3 = g3.expect("try_lock_mu_policy after release should succeed");
        assert_eq!(*g3.data1(), 9);
    }

    #[test]
    fn test_adoptable_and_guard_adoption() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 30.into(),
            data2: 40.into(),
        }));

        // Acquire directly into a pinned Adoptable, move between pinned Adoptable slots, and
        // adopt into a callee's stack-pinned KMutexGuard.
        fn callee_adopt(
            s: &MyGuardedStruct,
            adoptable: Pin<&mut Adoptable<'_, MyGuardedStructMuClass, RawMutex>>,
        ) {
            lock!(let mut guard = s.mu.adopt(adoptable));
            assert!(guard.is_some());
            assert!(!guard.is_none());
            assert!(guard.is_locked_by(&s.mu));
            assert!(core::ptr::eq(guard.mutex().unwrap(), &s.mu));
            let mut tg = s.guard_mu_mut_from_guard(guard.as_mut());
            assert_eq!(*tg.data1(), 10);
            *tg.reborrow().data1_mut() = 15;
        }

        {
            lock!(let mut a1 = s1.mu.lock_adoptable());
            assert!(a1.is_some());
            assert!(!a1.is_none());
            assert!(a1.is_locked_by(&s1.mu));
            assert!(!a1.is_locked_by(&s2.mu));
            assert!(core::ptr::eq(a1.mutex().unwrap(), &s1.mu));

            lock!(let mut a2 = Adoptable::empty());
            assert!(a2.is_none());
            assert!(!a2.is_some());
            a2.as_mut().move_from(a1.as_mut());
            assert!(a1.is_none());
            assert!(a2.is_locked_by(&s1.mu));

            lock!(let mut a3 = Adoptable::from_adoptable(a2.as_mut()));
            assert!(a2.is_none());
            assert!(a3.is_locked_by(&s1.mu));

            callee_adopt(&s1, a3.as_mut());
            assert!(a3.is_none());
        }

        // Verify the lock was released when `callee_adopt`'s guard dropped, and the mutation
        // persisted.
        {
            lock!(let mut guard = s1.mu.lock());
            assert_eq!(*s1.guard_mu_from_guard(&guard).data1(), 15);

            // Take from KMutexGuard into Adoptable, then adopt back into KMutexGuard.
            lock!(let mut adoptable = guard.as_mut().take());
            assert!(guard.is_none());
            assert!(adoptable.is_locked_by(&s1.mu));

            // Test call_unlocked and call_untracked on Adoptable.
            let unlocked_val = adoptable.as_mut().call_unlocked(|| 42);
            assert_eq!(unlocked_val, 42);
            adoptable.as_mut().call_untracked(|token| {
                // SAFETY: `token` witnesses exclusive access to `s1.mu` on `s1`.
                *unsafe { s1.guard_mu_mut(token) }.data1_mut() = 25;
            });

            lock!(let mut guard2 = KMutexGuard::adopt(adoptable.as_mut()));
            assert!(adoptable.is_none());
            assert!(guard2.is_locked_by(&s1.mu));
            assert_eq!(*s1.guard_mu_from_guard(&guard2).data1(), 25);

            // Move from `guard2` into `adoptable` via `take_from`, then back into `guard` via
            // `adopt_from`.
            adoptable.as_mut().take_from(guard2.as_mut());
            assert!(guard2.is_none());
            assert!(adoptable.is_locked_by(&s1.mu));

            guard.as_mut().adopt_from(adoptable.as_mut());
            assert!(adoptable.is_none());
            assert!(guard.is_locked_by(&s1.mu));
        }

        // Test `KMutexGuard::empty`, `KMutexGuard::from_raw`, `Adoptable::from_raw_with_args`,
        // and `OrderedLockPolicy` across moves.
        type Ordered = crate::OrderedLockPolicy<crate::raw_userspace_mutex::RawMutexPolicy>;
        stack_pin_init!(let na = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 1.into(),
        }));
        stack_pin_init!(let nb = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 2.into(),
        }));
        {
            lock!(let empty_g = KMutexGuard::<MyGuardedStructMuClass, RawMutex>::empty());
            assert!(empty_g.is_none());

            lock!(let raw_g = {
                // SAFETY: `s2.mu` is valid for the duration of this block.
                unsafe {
                    KMutexGuard::<MyGuardedStructMuClass, RawMutex>::from_raw(&raw const s2.mu)
                }
            });
            assert!(raw_g.is_locked_by(&s2.mu));

            lock!(let mut oa = na.mu.lock_adoptable_policy_with::<Ordered>(0));
            lock!(let mut ob = {
                // SAFETY: `nb.mu` is valid for the duration of this block.
                unsafe { Adoptable::<_, _, Ordered>::from_raw_with_args(&raw const nb.mu, 1) }
            });
            assert!(oa.is_locked_by(&na.mu));
            assert!(ob.is_locked_by(&nb.mu));

            // Move `ob` into `oa` (which releases `na.mu` first, then moves `nb.mu` at order 1
            // into `oa`).
            oa.as_mut().move_from(ob.as_mut());
            assert!(ob.is_none());
            assert!(oa.is_locked_by(&nb.mu));
            oa.as_mut().call_unlocked(|| {});
            // SAFETY: `oa` holds `nb.mu`.
            unsafe {
                *nb.guard_mu_mut(oa.as_mut().token_mut()).data_mut() = 99;
                assert_eq!(*nb.guard_mu(oa.token()).data(), 99);
            }
            oa.as_mut().release();
            assert!(oa.is_none());
        }
    }

    #[test]
    fn test_guard_move_from_and_adoptable_edge_cases() {
        extern crate std;
        type DefaultMutexPolicy = <crate::RawMutex as crate::RawLock>::DefaultPolicy;

        // Compile-time check that neither `KMutexGuard` nor `Adoptable` implements `Send` or
        // `Sync`.
        trait AmbiguousIfSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

        trait AmbiguousIfSync<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

        <KMutexGuard<'static, MyClass, RawMutex> as AmbiguousIfSend<_>>::check();
        <KMutexGuard<'static, MyClass, RawMutex> as AmbiguousIfSync<_>>::check();
        <Adoptable<'static, MyClass, RawMutex> as AmbiguousIfSend<_>>::check();
        <Adoptable<'static, MyClass, RawMutex> as AmbiguousIfSync<_>>::check();

        stack_pin_init!(let a = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 10.into(),
        }));
        stack_pin_init!(let b = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 20.into(),
        }));
        type Ordered = crate::OrderedLockPolicy<crate::raw_userspace_mutex::RawMutexPolicy>;

        // 1. `KMutexGuard::move_from`: active -> active releases destination's old lock, and
        // empty -> active releases destination and leaves it empty.
        {
            lock!(let mut ga = a.mu.lock_policy_with::<Ordered>(0));
            lock!(let mut gb = b.mu.lock_policy_with::<Ordered>(1));
            assert!(std::format!("{ga:?}").contains("KMutexGuard"));

            ga.as_mut().move_from(gb.as_mut());
            assert!(gb.is_none());
            assert!(ga.is_locked_by(&b.mu));
            assert!(std::format!("{gb:?}").contains("false"));

            // Verify `a.mu` was actually released when `ga` was overwritten.
            {
                crate::try_lock!(let try_a = a.try_lock_mu_policy::<DefaultMutexPolicy>());
                assert!(try_a.is_ok());
            }

            // Moving an empty guard (`gb`) into an active guard (`ga`) releases `b.mu` and leaves
            // `ga` empty.
            ga.as_mut().move_from(gb.as_mut());
            assert!(ga.is_none());
            {
                crate::try_lock!(let try_b = b.try_lock_mu_policy::<DefaultMutexPolicy>());
                assert!(try_b.is_ok());
            }
        }

        // 2. Empty-source `Adoptable::from_adoptable` and `Adoptable::move_from`, active-dest
        // `adopt_from` / `take_from`, `take_unbounded`, `Adoptable::from_raw`, and dropping a
        // non-empty `Adoptable` without calling `release()`.
        {
            lock!(let mut empty_a = Adoptable::<NestableGuardedStructMuClass, RawMutex>::empty());
            assert!(std::format!("{empty_a:?}").contains("Adoptable"));
            lock!(let mut moved_empty = Adoptable::from_adoptable(empty_a.as_mut()));
            assert!(moved_empty.is_none());

            lock!(let mut active_a = {
                // SAFETY: `a.mu` outlives this block.
                unsafe { Adoptable::from_raw(&raw const a.mu) }
            });
            assert!(active_a.is_locked_by(&a.mu));
            // Moving empty `moved_empty` into `active_a` releases `a.mu`.
            active_a.as_mut().move_from(moved_empty.as_mut());
            assert!(active_a.is_none());
            {
                crate::try_lock!(let try_a = a.try_lock_mu_policy::<DefaultMutexPolicy>());
                assert!(try_a.is_ok());
            }
        }

        {
            lock!(let mut ga = a.mu.lock_policy_with::<Ordered>(0));
            lock!(let mut ab = b.mu.lock_adoptable_policy_with::<Ordered>(1));
            // Active-destination `adopt_from` releases `a.mu` before adopting `b.mu`.
            ga.as_mut().adopt_from(ab.as_mut());
            assert!(ab.is_none());
            assert!(ga.is_locked_by(&b.mu));
            {
                crate::try_lock!(let try_a = a.try_lock_mu_policy::<DefaultMutexPolicy>());
                assert!(try_a.is_ok());
            }

            // `take_unbounded` moves `b.mu` into a `'static`-typed `Adoptable`, and active-dest
            // `take_from` on another held `Adoptable` releases the previous lock.
            stack_pin_init!(let taken_static: Adoptable<'static, _, _, Ordered> = {
                // SAFETY: `b.mu` outlives `taken_static` (which drops at the end of this block).
                unsafe { ga.as_mut().take_unbounded() }
            });
            assert!(ga.is_none());
            assert!(taken_static.is_locked_by(&b.mu));

            lock!(let mut ga2 = a.mu.lock_policy_with::<Ordered>(0));
            taken_static.as_mut().call_unlocked(|| {});
            // Overwrite `taken_static` (holding `b.mu`) via `take_from` from `ga2` (holding
            // `a.mu`), then let `taken_static` drop while still holding `a.mu`.
            // SAFETY: `a.mu` outlives `taken_static`.
            unsafe {
                taken_static.as_mut().take_from_unbounded(ga2.as_mut());
            }
            assert!(ga2.is_none());
            assert!(taken_static.is_locked_by(&a.mu));
            {
                crate::try_lock!(let try_b = b.try_lock_mu_policy::<DefaultMutexPolicy>());
                assert!(try_b.is_ok());
            }
        }
        // `taken_static` dropped here while non-empty; verify `a.mu` was released by `Drop`.
        crate::try_lock!(let try_a = a.try_lock_mu_policy::<DefaultMutexPolicy>());
        assert!(try_a.is_ok());
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn test_call_unlocked_and_call_untracked_unwind_safety() {
        extern crate std;

        #[pin_init::pin_data]
        struct TrackCheckLock {
            held: core::sync::atomic::AtomicBool,
            tracked: core::sync::atomic::AtomicBool,
        }

        struct TrackCheckPolicy;

        // SAFETY: Single-threaded test mock asserting strict acquire/untrack/retrack/release
        // pairing.
        unsafe impl LockPolicy<TrackCheckLock> for TrackCheckPolicy {
            type AcquireArgs = ();
            type GuardState = ();

            unsafe fn acquire(lock: &TrackCheckLock, _state: &mut (), _args: ()) {
                assert!(!lock.held.swap(true, core::sync::atomic::Ordering::Acquire));
                assert!(!lock.tracked.swap(true, core::sync::atomic::Ordering::Relaxed));
            }

            unsafe fn reacquire(lock: &TrackCheckLock, _state: &mut ()) {
                assert!(!lock.held.swap(true, core::sync::atomic::Ordering::Acquire));
                assert!(!lock.tracked.swap(true, core::sync::atomic::Ordering::Relaxed));
            }

            unsafe fn untrack(lock: &TrackCheckLock, _state: &mut ()) {
                assert!(lock.held.load(core::sync::atomic::Ordering::Relaxed));
                assert!(lock.tracked.swap(false, core::sync::atomic::Ordering::Relaxed));
            }

            unsafe fn retrack(lock: &TrackCheckLock, _state: &mut ()) {
                assert!(lock.held.load(core::sync::atomic::Ordering::Relaxed));
                assert!(!lock.tracked.swap(true, core::sync::atomic::Ordering::Relaxed));
            }

            unsafe fn release(lock: &TrackCheckLock, _state: &mut ()) {
                assert!(
                    lock.tracked.swap(false, core::sync::atomic::Ordering::Relaxed),
                    "release called while lock was untracked"
                );
                assert!(
                    lock.held.swap(false, core::sync::atomic::Ordering::Release),
                    "double release on TrackCheckLock"
                );
            }
        }

        // SAFETY: Single-threaded test mock enforcing exclusive ownership.
        unsafe impl ExclusiveLockPolicy<TrackCheckLock> for TrackCheckPolicy {}

        // SAFETY: Non-zero size and unique address per instance.
        unsafe impl RawLock for TrackCheckLock {
            type DefaultPolicy = TrackCheckPolicy;

            unsafe fn init(
                _class_id: *const core::ffi::c_void,
            ) -> impl PinInit<Self, core::convert::Infallible> {
                pin_init!(Self {
                    held: core::sync::atomic::AtomicBool::new(false),
                    tracked: core::sync::atomic::AtomicBool::new(false),
                })
            }

            fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
                core::ptr::from_ref(self).cast_mut().cast()
            }
        }

        stack_pin_init!(let mu = KMutex::<MyClass, TrackCheckLock>::init());

        // 1. Unwinding out of `call_unlocked` must not double-release on `Drop` or leave a caught
        // guard claiming to hold the lock.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock!(let mut g = mu.lock());
            g.as_mut().call_unlocked(|| panic!("boom inside call_unlocked"));
        }));
        assert!(res.is_err());
        assert!(!mu.raw_mutex().held.load(core::sync::atomic::Ordering::Relaxed));
        assert!(!mu.raw_mutex().tracked.load(core::sync::atomic::Ordering::Relaxed));

        {
            lock!(let mut g = mu.lock());
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                g.as_mut().call_unlocked(|| panic!("caught inside call_unlocked"));
            }));
            assert!(caught.is_err());
            assert!(g.is_none());
        }

        // 2. Unwinding out of `call_untracked` must run `retrack` before `Drop` runs `release`.
        let res_untracked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock!(let mut g = mu.lock());
            g.as_mut().call_untracked(|_tok| panic!("boom inside call_untracked"));
        }));
        assert!(res_untracked.is_err());
        assert!(!mu.raw_mutex().held.load(core::sync::atomic::Ordering::Relaxed));
        assert!(!mu.raw_mutex().tracked.load(core::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_guard_releases_through_release_raw() {
        #[pin_init::pin_data]
        struct CountingLock {
            held: AtomicBool,
            raw_releases: AtomicU32,
            ref_releases: AtomicU32,
        }

        impl CountingLock {
            /// Returns the number of releases through `release_raw` and through `release`.
            fn releases(&self) -> (u32, u32) {
                (
                    self.raw_releases.load(Ordering::Relaxed),
                    self.ref_releases.load(Ordering::Relaxed),
                )
            }
        }

        struct CountingPolicy;

        // SAFETY: Single-threaded test mock asserting strict acquire/release pairing.
        unsafe impl LockPolicy<CountingLock> for CountingPolicy {
            type AcquireArgs = ();
            type GuardState = ();

            unsafe fn acquire(lock: &CountingLock, _state: &mut (), _args: ()) {
                assert!(!lock.held.swap(true, Ordering::Acquire));
            }

            unsafe fn reacquire(lock: &CountingLock, _state: &mut ()) {
                assert!(!lock.held.swap(true, Ordering::Acquire));
            }

            unsafe fn release(lock: &CountingLock, _state: &mut ()) {
                let _ = lock.ref_releases.fetch_add(1, Ordering::Relaxed);
                assert!(lock.held.swap(false, Ordering::Release));
            }

            unsafe fn release_raw(lock: NonNull<CountingLock>, _state: &mut ()) {
                // SAFETY: The test keeps the lock alive for the whole call.
                let lock = unsafe { lock.as_ref() };
                let _ = lock.raw_releases.fetch_add(1, Ordering::Relaxed);
                assert!(lock.held.swap(false, Ordering::Release));
            }
        }

        // SAFETY: Single-threaded test mock enforcing exclusive ownership.
        unsafe impl ExclusiveLockPolicy<CountingLock> for CountingPolicy {}

        // SAFETY: `CountingPolicy` has `GuardState = ()` and no stack-LIFO state.
        unsafe impl crate::AdoptableLockPolicy<CountingLock> for CountingPolicy {}

        // SAFETY: Non-zero size and unique address per instance.
        unsafe impl RawLock for CountingLock {
            type DefaultPolicy = CountingPolicy;

            unsafe fn init(
                _class_id: *const core::ffi::c_void,
            ) -> impl PinInit<Self, core::convert::Infallible> {
                pin_init!(Self {
                    held: AtomicBool::new(false),
                    raw_releases: AtomicU32::new(0),
                    ref_releases: AtomicU32::new(0),
                })
            }

            fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
                core::ptr::from_ref(self).cast_mut().cast()
            }
        }

        stack_pin_init!(let mu = KMutex::<MyClass, CountingLock>::init());

        // Dropping a guard and releasing it explicitly both go through `release_raw`, so the
        // guard holds no reference to the lock across the release.
        {
            lock!(let _g = mu.lock());
        }
        assert_eq!(mu.raw_mutex().releases(), (1, 0));
        {
            lock!(let mut g = mu.lock());
            g.as_mut().release();
            assert_eq!(mu.raw_mutex().releases(), (2, 0));
        }
        assert_eq!(mu.raw_mutex().releases(), (2, 0));

        // A temporary unlock reacquires the same lock, so it may borrow it for the release.
        {
            lock!(let mut g = mu.lock());
            g.as_mut().call_unlocked(|| ());
        }
        assert_eq!(mu.raw_mutex().releases(), (3, 1));

        // An `Adoptable` releases the same way as a guard.
        {
            lock!(let mut a = mu.lock_adoptable());
            a.as_mut().release();
        }
        assert_eq!(mu.raw_mutex().releases(), (4, 1));

        // `LockdepTracked` forwards `release_raw` to the wrapped lock's policy.
        stack_pin_init!(
            let tracked = KMutex::<MyClass, crate::LockdepTracked<CountingLock>>::init()
        );
        {
            lock!(let _g = tracked.lock());
        }
        assert_eq!(tracked.raw_mutex().releases(), (1, 0));

        // So does `OrderedLockPolicy`, which needs a nestable lock class.
        struct NestableClass;
        impl LockClass for NestableClass {
            const ID: *mut core::ffi::c_void = core::ptr::null_mut();
            const FLAGS: lockdep::LockFlags = lockdep::LOCK_FLAGS_NESTABLE;
        }
        stack_pin_init!(
            let ordered = KMutex::<NestableClass, crate::LockdepTracked<CountingLock>>::init()
        );
        {
            lock!(
                let _g = ordered.lock_policy_with::<crate::OrderedLockPolicy<CountingPolicy>>(0)
            );
        }
        assert_eq!(ordered.raw_mutex().releases(), (1, 0));
    }

    /// A guard made by `from_raw` may be the only thing keeping its mutex alive, so the mutex can
    /// be destroyed as soon as the release is published. Model that with a policy whose
    /// `release_raw` makes the mutex's memory inaccessible before it returns: any access that the
    /// guard or `LockdepTracked` made to the mutex after forwarding the release would fault.
    #[test]
    fn test_from_raw_guard_does_not_access_mutex_after_release() {
        static REVOKED: AtomicU32 = AtomicU32::new(0);

        #[pin_init::pin_data]
        struct RevokingLock {
            held: AtomicBool,
            // The mapping containing this lock, which `RevokingPolicy::release_raw` protects.
            map: AtomicPtr<u8>,
            map_len: AtomicUsize,
        }

        struct RevokingPolicy;

        // SAFETY: Single-threaded test mock asserting strict acquire/release pairing.
        unsafe impl LockPolicy<RevokingLock> for RevokingPolicy {
            type AcquireArgs = ();
            type GuardState = ();

            unsafe fn acquire(lock: &RevokingLock, _state: &mut (), _args: ()) {
                assert!(!lock.held.swap(true, Ordering::Acquire));
            }

            unsafe fn reacquire(lock: &RevokingLock, _state: &mut ()) {
                assert!(!lock.held.swap(true, Ordering::Acquire));
            }

            unsafe fn release(lock: &RevokingLock, _state: &mut ()) {
                // The caller borrows the lock for the whole call, so it stays accessible.
                assert!(lock.held.swap(false, Ordering::Release));
            }

            unsafe fn release_raw(lock: NonNull<RevokingLock>, _state: &mut ()) {
                // SAFETY: The lock is valid until the release is published by the `swap`, where
                // this reference ends.
                let (map, map_len) = unsafe {
                    let lock = lock.as_ref();
                    let map = lock.map.load(Ordering::Relaxed);
                    let map_len = lock.map_len.load(Ordering::Relaxed);
                    assert!(lock.held.swap(false, Ordering::Release));
                    (map, map_len)
                };
                // The release is published. Make the mutex inaccessible, standing in for another
                // thread destroying it: from here on, any access to it faults.
                //
                // SAFETY: `map_mutex` created this mapping, and nothing may access the mutex
                // after its final release.
                assert_eq!(unsafe { libc::mprotect(map.cast(), map_len, libc::PROT_NONE) }, 0);
                let _ = REVOKED.fetch_add(1, Ordering::Relaxed);
            }
        }

        // SAFETY: Single-threaded test mock enforcing exclusive ownership.
        unsafe impl ExclusiveLockPolicy<RevokingLock> for RevokingPolicy {}

        // SAFETY: Non-zero size and unique address per instance.
        unsafe impl RawLock for RevokingLock {
            type DefaultPolicy = RevokingPolicy;

            unsafe fn init(
                _class_id: *const core::ffi::c_void,
            ) -> impl PinInit<Self, core::convert::Infallible> {
                pin_init!(Self {
                    held: AtomicBool::new(false),
                    map: AtomicPtr::new(core::ptr::null_mut()),
                    map_len: AtomicUsize::new(0),
                })
            }

            fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
                core::ptr::from_ref(self).cast_mut().cast()
            }
        }

        /// Initializes a mutex in a page of its own and records the mapping in the `RevokingLock`
        /// it wraps. The page is never unmapped, so its address stays inaccessible once
        /// `release_raw` has protected it.
        fn map_mutex<M: RawLock>(
            revoking_lock: fn(&M) -> &RevokingLock,
        ) -> *const KMutex<MyClass, M> {
            // SAFETY: `sysconf` has no preconditions.
            let page_size = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
            assert!(core::mem::size_of::<KMutex<MyClass, M>>() <= page_size);
            // SAFETY: Mapping new anonymous memory has no preconditions.
            let map = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    page_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(map, libc::MAP_FAILED);
            let mutex = map.cast::<KMutex<MyClass, M>>();
            // SAFETY: `mutex` is a valid, page-aligned, uninitialized slot that never moves.
            let Ok(()) = unsafe { KMutex::<MyClass, M>::init().__pinned_init(mutex) };
            // SAFETY: `*mutex` was just initialized, and this reference ends with this function.
            let lock = revoking_lock(unsafe { &*mutex }.raw_mutex());
            lock.map.store(map.cast(), Ordering::Relaxed);
            lock.map_len.store(page_size, Ordering::Relaxed);
            mutex
        }

        // Dropping the guard makes the mutex inaccessible from inside `release_raw`.
        let mutex = map_mutex::<RevokingLock>(|lock| lock);
        {
            lock!(let _g = {
                // SAFETY: `*mutex` is initialized and stays valid until its release is
                // published, which is all that `RevokingPolicy::release_raw` requires.
                unsafe { KMutexGuard::<MyClass, RevokingLock>::from_raw(mutex) }
            });
        }
        assert_eq!(REVOKED.load(Ordering::Relaxed), 1);

        // So does releasing it explicitly, after which dropping the guard does nothing.
        let mutex = map_mutex::<RevokingLock>(|lock| lock);
        {
            lock!(let mut g = {
                // SAFETY: As above.
                unsafe { KMutexGuard::<MyClass, RevokingLock>::from_raw(mutex) }
            });
            g.as_mut().release();
            assert_eq!(REVOKED.load(Ordering::Relaxed), 2);
        }
        assert_eq!(REVOKED.load(Ordering::Relaxed), 2);

        // `LockdepTracked` does not access the mutex after forwarding the release either.
        let mutex = map_mutex::<crate::LockdepTracked<RevokingLock>>(|tracked| tracked);
        {
            lock!(let _g = {
                // SAFETY: As above; `LockdepTracked` documents that it requires no more of the
                // mutex than the policy of the lock it wraps.
                unsafe {
                    KMutexGuard::<MyClass, crate::LockdepTracked<RevokingLock>>::from_raw(mutex)
                }
            });
        }
        assert_eq!(REVOKED.load(Ordering::Relaxed), 3);
    }

    #[test]
    #[should_panic(expected = "cannot adopt from an empty Adoptable")]
    fn test_adopt_empty_adoptable_panics() {
        lock!(let mut a = Adoptable::<MyClass, RawMutex>::empty());
        lock!(let _g = KMutexGuard::adopt(a.as_mut()));
    }

    #[test]
    #[should_panic(expected = "cannot create Adoptable from a released KMutexGuard")]
    fn test_take_released_guard_panics() {
        stack_pin_init!(let mu = KMutex::<MyClass, RawMutex>::init());
        lock!(let mut g = mu.lock());
        g.as_mut().release();
        lock!(let _a = g.as_mut().take());
    }

    #[test]
    #[should_panic(
        expected = "KMutexAliasedGuard inner guard was retargeted to a different physical lock"
    )]
    fn test_aliased_guard_retarget_panics_on_tokens() {
        stack_pin_init!(let a = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 1.into(),
        }));
        stack_pin_init!(let b = pin_init!(NestableGuardedStruct {
            mu <- KMutex::init(),
            data: 2.into(),
        }));
        type Ordered = crate::OrderedLockPolicy<crate::raw_userspace_mutex::RawMutexPolicy>;

        lock!(let mut aliased = a.lock_mu_aliased_policy_with::<_, _, Ordered>(&a.mu, 0));
        lock!(let mut other = b.mu.lock_policy_with::<Ordered>(1));
        aliased.as_mut().inner_guard().move_from(other.as_mut());
        let _ = aliased.tokens();
    }

    #[test]
    #[should_panic(expected = "cannot adopt an Adoptable that does not hold this KMutex")]
    fn test_adopt_wrong_mutex_panics() {
        stack_pin_init!(let s1 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 1.into(),
            data2: 2.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 3.into(),
            data2: 4.into(),
        }));

        lock!(let mut a1 = s1.mu.lock_adoptable());
        lock!(let _g2 = s2.mu.adopt(a1.as_mut()));
    }

    /// A `!Unpin` value with a method that requires a pinned receiver.
    #[pin_init::pin_data]
    struct NotUnpin {
        #[pin]
        _pin: core::marker::PhantomPinned,
        v: u32,
    }

    impl NotUnpin {
        fn new(v: u32) -> impl pin_init::PinInit<Self> {
            pin_init!(Self { _pin: core::marker::PhantomPinned, v })
        }

        fn bump(self: Pin<&mut Self>) {
            *self.project().v += 1;
        }
    }

    #[guarded]
    struct PinnedGuardedStruct {
        #[mutex]
        mu: KMutex<RawMutex>,
        #[pin]
        #[guarded_by(mu)]
        cell: NotUnpin,
        #[guarded_by(mu)]
        plain: u32,
    }

    /// The lock accessors of `#[pin]` guarded fields hand them out mutably only as `Pin<&mut T>`,
    /// so `!Unpin` values can be mutated through pin-aware APIs entirely in safe code.
    #[test]
    #[forbid(unsafe_code)]
    fn test_pinned_guarded_field_projects_as_pin() {
        // Autoref-specialization probe: the inherent `is_unpin` (returning `true`) only applies
        // when `T: Unpin`; otherwise the trait method (returning `false`) is selected.
        struct Probe<T>(core::marker::PhantomData<T>);
        trait NotUnpinProbe {
            fn is_unpin(&self) -> bool {
                false
            }
        }
        impl<T> NotUnpinProbe for Probe<T> {}
        impl<T: Unpin> Probe<T> {
            fn is_unpin(&self) -> bool {
                true
            }
        }
        assert!(!Probe::<NotUnpin>(core::marker::PhantomData).is_unpin());
        assert!(Probe::<u32>(core::marker::PhantomData).is_unpin());

        stack_pin_init!(let s = pin_init!(PinnedGuardedStruct {
            mu <- KMutex::init(),
            cell <- crate::kcell_init(NotUnpin::new(1)),
            plain: 10.into(),
        }));

        {
            lock!(let mut guard = s.lock_mu());
            let cell: Pin<&mut NotUnpin> = guard.as_mut().cell_mut();
            cell.bump();
            assert_eq!(guard.cell().v, 2);

            let mut fields = guard.as_mut().fields_mut();
            let _: &mut Pin<&mut NotUnpin> = &mut fields.cell;
            fields.cell.as_mut().bump();
            fields.cell.as_mut().bump();
            *fields.plain += 1;
        }

        lock!(let guard = s.lock_mu());
        assert_eq!(guard.cell().v, 4);
        assert_eq!(guard.fields().cell.v, 4);
        assert_eq!(*guard.plain(), 11);
    }
}
