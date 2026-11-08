// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel spinlocks, mirroring the C++ `SpinLock` and `MonitoredSpinLock` declared in
//! `<kernel/spinlock.h>`, plus the `ksync` raw locks built on them ([`TrackedSpinlock`] and
//! [`TrackedMonitoredSpinlock`]) and their lock policies, which are re-exported from
//! `crate::ksync_kernel`.
//!
//! The spinlocks are implemented in Rust on top of the architecture spinlock primitives in
//! [`crate::arch_rs::spinlock`], which are shared with the C++ implementation, so a lock may be
//! acquired and released from either language.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::AtomicU32;
use ksync::{
    ConstRawLock, ExclusiveLockPolicy, LockPolicy, LockdepTracked, RawAssertHeldLock, RawLock,
    RawUntrackedIrqLock, RawUntrackedLock, SourceTag, impl_lockdep_tracked_lock_policy,
};
use pin_init::{PinInit, pin_init_from_closure};

use crate::arch_rs::spinlock::{
    arch_spin_lock_held, arch_spin_lock_holder_cpu, arch_spin_lock_non_instrumented,
    arch_spin_lock_trace_instrumented, arch_spin_trylock, arch_spin_unlock,
};
use crate::arch_rs::{
    InterruptSavedState, arch_interrupt_restore, arch_interrupt_save, ints_disabled,
};
use crate::kernel::spin_tracing::{
    EncodedLockId, IdStringRef, SCHEDULER_LOCK_SPIN_TRACING_ENABLED, SPINLOCK_NAME_TRACING_ENABLED,
    SpinLockNameStorage,
};
use crate::kernel::types::cpu_num_t;
use crate::lockup_detector::{lockup_begin, lockup_end};

/// Outcome of a non-blocking [`SpinLock::try_acquire`] or [`MonitoredSpinLock::try_acquire`]
/// attempt.
#[must_use = "if `Acquired`, the spinlock is now held and must be released"]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TryAcquireResult {
    /// The spinlock was acquired by the calling CPU and must be released.
    Acquired,
    /// The spinlock was already held and was not acquired.
    Failed,
}

impl TryAcquireResult {
    /// Returns `true` if the spinlock was acquired (and so must be released).
    #[inline]
    pub const fn is_acquired(self) -> bool {
        matches!(self, Self::Acquired)
    }
}

/// A kernel spinlock, layout-compatible with the C++ `SpinLockBase` template in
/// `<kernel/spinlock.h>`. Use the [`SpinLock`] and [`MonitoredSpinLock`] aliases.
///
/// `MONITORED` selects the C++ `SpinLockOptions::Monitored` option: a monitored spinlock's
/// critical sections are named, and monitored by the kernel lockup detector (see
/// `//zircon/kernel/lib/lockup_detector/README.md`), so it is acquired with the name of the
/// critical section.
///
/// Note: When spin lock tracing is disabled, the C++ `SpinLock` has size 4 and alignment 4,
/// whereas this type intentionally over-aligns to 8 bytes (`#[repr(C, align(8))]`, resulting in
/// size 8 and alignment 8) so that [`TrackedSpinlock`] and `#[repr(C)]` FFI structs embedding it
/// (such as `ControlRegion` and `TrapMap`, which pad `sizeof(LockType)` to 8 bytes in C++)
/// maintain uniform 8-byte alignment across all lockdep and spin lock tracing configurations.
///
/// Every field is interior-mutable, so a `&SpinLockBase` makes no assertion that its lock word is
/// unchanged once a release through it has unlocked the lock. However, the over-alignment padding
/// is not interior-mutable, so the lock must remain live until a release through it returns (see
/// [`Self::release`]).
#[repr(C, align(8))]
pub struct SpinLockBase<const MONITORED: bool> {
    /// Storage for the lock name and encoded lock ID used by spin tracing. Corresponds to the C++
    /// `spin_tracing::LockNameStorage` base class, and is zero-sized when
    /// `SPINLOCK_NAME_TRACING_ENABLED` is false. Only mutated during initialization, before the
    /// lock is shared.
    name_storage: UnsafeCell<SpinLockNameStorage>,

    /// The `value` of the C++ `arch_spin_lock_t`: the number of the CPU holding the lock plus
    /// one, so 0 means unheld.
    spinlock: AtomicU32,

    /// No moving allowed, as lockdep identifies a lock by its address. Wrapped in `PhantomData`
    /// so `SpinLockBase` and types embedding it stay FFI-safe (`improper_ctypes` rejects a bare
    /// `PhantomPinned`).
    _pin: PhantomData<PhantomPinned>,
}

/// A kernel spinlock, matching the C++ `SpinLock`.
pub type SpinLock = SpinLockBase<false>;

/// A kernel spinlock that is integrated with the lockup detector, matching the C++
/// `MonitoredSpinLock`.
pub type MonitoredSpinLock = SpinLockBase<true>;

// SAFETY: `SpinLockBase` has no thread-local ownership invariants, so ownership of a
// `SpinLockBase` can be transferred across threads.
unsafe impl<const MONITORED: bool> Send for SpinLockBase<MONITORED> {}
// SAFETY: Shared accesses through `&SpinLockBase` operate on the `spinlock` atomic, or read
// `name_storage`, which is only mutated during initialization, before the lock is shared.
unsafe impl<const MONITORED: bool> Sync for SpinLockBase<MONITORED> {}

impl<const MONITORED: bool> SpinLockBase<MONITORED> {
    /// Creates an unlocked spinlock, matching a constant-initialized C++ `SpinLock`.
    ///
    /// When spin lock tracing is enabled, the lock ID is left as a placeholder (see
    /// `SpinLockNameStorage::const_init`) rather than being assigned a unique ID as by the C++
    /// default constructor. Initializing the lock through [`RawLock::init`] assigns the unique ID.
    pub const fn new() -> Self {
        Self {
            name_storage: UnsafeCell::new(
                SpinLockNameStorage::<SPINLOCK_NAME_TRACING_ENABLED>::const_init(
                    IdStringRef::INVALID_ID,
                ),
            ),
            spinlock: AtomicU32::new(0),
            _pin: PhantomData,
        }
    }

    /// Returns the encoded lock ID used to identify this lock in spin trace events.
    #[inline(always)]
    fn encoded_lock_id(&self) -> EncodedLockId {
        // SAFETY: `name_storage` is only mutated during initialization, before the lock is
        // shared, so there are no concurrent writes.
        unsafe { (*self.name_storage.get()).encoded_lock_id() }
    }

    /// Debug-asserts the preconditions of acquiring the spinlock.
    #[inline(always)]
    fn debug_assert_acquirable(&self) {
        debug_assert!(ints_disabled());
        debug_assert!(!arch_spin_lock_held(&self.spinlock));
    }

    /// Acquires the spinlock, without any lockup detector integration.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::acquire`].
    #[inline(always)]
    unsafe fn acquire_internal(&self) {
        if SCHEDULER_LOCK_SPIN_TRACING_ENABLED {
            // SAFETY: The caller has disabled interrupts and does not hold this spinlock.
            unsafe { arch_spin_lock_trace_instrumented(&self.spinlock, self.encoded_lock_id()) };
        } else {
            // SAFETY: The caller has disabled interrupts and does not hold this spinlock.
            unsafe { arch_spin_lock_non_instrumented(&self.spinlock) };
        }
    }

    /// Attempts to acquire the spinlock without waiting, and without any lockup detector
    /// integration.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::try_acquire`].
    #[inline(always)]
    unsafe fn try_acquire_internal(&self) -> TryAcquireResult {
        // SAFETY: The caller has disabled interrupts. `arch_spin_trylock` returns `true` when the
        // lock is not acquired.
        if unsafe { arch_spin_trylock(&self.spinlock) } {
            TryAcquireResult::Failed
        } else {
            TryAcquireResult::Acquired
        }
    }

    /// Release the spinlock.
    ///
    /// Interrupts must already be disabled.
    ///
    /// # Safety
    ///
    /// Interrupts must be disabled and this spinlock must currently be held by the calling CPU
    /// without outstanding references protected by this acquisition.
    ///
    /// The allocation backing `self` must remain live until this call returns. Once the lock word
    /// is released another CPU may acquire the lock immediately, so this must not be used to
    /// release a lock that guards the destruction of its own containing allocation.
    #[inline]
    pub unsafe fn release(&self) {
        // SAFETY: The caller guarantees that the calling CPU holds this live spinlock with
        // interrupts disabled. Nothing accesses `self` once the lock is released.
        unsafe { arch_spin_unlock(ptr::from_ref(&self.spinlock)) };
        if MONITORED {
            // SAFETY: The acquisition of this monitored spinlock entered a lockup detector
            // critical section on this CPU, which interrupts have remained disabled on since.
            unsafe { lockup_end() };
        }
    }

    /// Just like [`Self::release`], but also restores interrupt state.
    ///
    /// # Safety
    ///
    /// This spinlock must currently be held by the calling CPU without outstanding references
    /// protected by this acquisition, and `state` must be the [`InterruptSavedState`] returned by
    /// the corresponding interrupt-saving acquisition on this CPU.
    ///
    /// The allocation backing `self` must remain live until this call returns. The lock word is
    /// released before interrupts are restored, and once released another CPU may acquire the
    /// lock immediately, so this must not be used to release a lock that guards the destruction
    /// of its own containing allocation.
    #[inline]
    pub unsafe fn release_irq_restore(&self, state: InterruptSavedState) {
        // SAFETY: The caller upholds the `release` contract, and interrupts are disabled as the
        // lock was acquired by an interrupt-saving acquisition.
        unsafe { self.release() };
        arch_interrupt_restore(state);
    }

    /// Returns `true` if held by the calling CPU.
    ///
    /// Interrupts must be disabled before calling this method, otherwise it could return `true`
    /// when it should return `false`.
    ///
    /// # Panics
    ///
    /// In builds with debug assertions enabled, panics if interrupts are not disabled on the
    /// calling CPU. Without debug assertions no check is made, and calling this with interrupts
    /// enabled (and so migration possible) can return a false positive.
    #[inline]
    pub fn is_held(&self) -> bool {
        debug_assert!(ints_disabled());
        arch_spin_lock_held(&self.spinlock)
    }

    /// Returns which CPU currently holds the spinlock, or `INVALID_CPU` if it is not held.
    ///
    /// Note that this does not synchronize other memory operations (i.e. it has relaxed atomic
    /// semantics).
    #[inline]
    pub fn holder_cpu(&self) -> cpu_num_t {
        arch_spin_lock_holder_cpu(&self.spinlock)
    }

    /// Asserts (in debug builds) that the spinlock is currently held by the calling CPU.
    ///
    /// # Panics
    ///
    /// In builds with debug assertions enabled, panics if interrupts are not disabled on the
    /// calling CPU or if this spinlock is not held by the calling CPU.
    #[inline]
    pub fn assert_held(&self) {
        debug_assert!(self.is_held());
    }
}

impl SpinLock {
    /// Acquire the spinlock.
    ///
    /// Interrupts must already be disabled.
    ///
    /// # Safety
    ///
    /// Interrupts must already be disabled on the calling CPU, the calling CPU must not already
    /// hold this spinlock, and this acquisition must be paired with a matching [`Self::release`]
    /// or [`Self::release_irq_restore`] on the same CPU before interrupts are re-enabled or the
    /// thread blocks or migrates.
    #[inline]
    pub unsafe fn acquire(&self) {
        self.debug_assert_acquirable();
        // SAFETY: Forwarded from the caller.
        unsafe { self.acquire_internal() };
    }

    /// Attempt to acquire the spinlock without waiting.
    ///
    /// Interrupts must already be disabled.
    ///
    /// Returns [`TryAcquireResult::Acquired`] when the lock is acquired, and
    /// [`TryAcquireResult::Failed`] when the lock is not acquired.
    ///
    /// `try_acquire` operations are not permitted to fail spuriously, even on architectures with
    /// weak memory ordering. If a `try_acquire` operation fails, it must be because the lock was
    /// actually observed to be held during the attempt.
    ///
    /// # Safety
    ///
    /// Interrupts must already be disabled on the calling CPU, and if this call acquires the lock
    /// (returns [`TryAcquireResult::Acquired`]), it must be paired with a matching
    /// [`Self::release`] or [`Self::release_irq_restore`] on the same CPU before interrupts are
    /// re-enabled or the thread blocks or migrates.
    #[inline]
    pub unsafe fn try_acquire(&self) -> TryAcquireResult {
        // SAFETY: Forwarded from the caller.
        unsafe { self.try_acquire_internal() }
    }

    /// Just like [`Self::acquire`], but saves interrupt state and disables interrupts.
    ///
    /// # Safety
    ///
    /// The calling CPU must not already hold this spinlock, and the returned
    /// [`InterruptSavedState`] must be passed to a matching [`Self::release_irq_restore`] on the
    /// same CPU before the thread blocks or migrates.
    #[inline]
    #[must_use = "the saved interrupt state must be passed to `release_irq_restore`"]
    pub unsafe fn acquire_irq_save(&self) -> InterruptSavedState {
        let state = arch_interrupt_save();
        // SAFETY: Interrupts were disabled above, and the remaining requirements are forwarded
        // from the caller.
        unsafe { self.acquire() };
        state
    }
}

impl MonitoredSpinLock {
    /// Acquire the spinlock, entering the lockup detector critical section named `name`.
    ///
    /// Interrupts must already be disabled.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::acquire`].
    #[inline]
    pub unsafe fn acquire(&self, name: SourceTag) {
        self.debug_assert_acquirable();
        // SAFETY: The caller has disabled interrupts, and pairs this acquisition with a release
        // on the same CPU, which leaves the critical section.
        unsafe { lockup_begin(name.as_cstr()) };
        // SAFETY: Forwarded from the caller.
        unsafe { self.acquire_internal() };
    }

    /// Attempt to acquire the spinlock without waiting, entering the lockup detector critical
    /// section named `name` if the lock is acquired.
    ///
    /// See [`SpinLock::try_acquire`].
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::try_acquire`].
    #[inline]
    pub unsafe fn try_acquire(&self, name: SourceTag) -> TryAcquireResult {
        // SAFETY: Forwarded from the caller.
        let result = unsafe { self.try_acquire_internal() };
        if result.is_acquired() {
            // SAFETY: The caller has disabled interrupts, and pairs this acquisition with a
            // release on the same CPU, which leaves the critical section.
            unsafe { lockup_begin(name.as_cstr()) };
        }
        result
    }

    /// Just like [`Self::acquire`], but saves interrupt state and disables interrupts.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::acquire_irq_save`].
    #[inline]
    #[must_use = "the saved interrupt state must be passed to `release_irq_restore`"]
    pub unsafe fn acquire_irq_save(&self, name: SourceTag) -> InterruptSavedState {
        let state = arch_interrupt_save();
        // SAFETY: Interrupts were disabled above, and the remaining requirements are forwarded
        // from the caller.
        unsafe { self.acquire(name) };
        state
    }
}

impl<const MONITORED: bool> AsRef<SpinLockBase<MONITORED>> for SpinLockBase<MONITORED> {
    #[inline]
    fn as_ref(&self) -> &SpinLockBase<MONITORED> {
        self
    }
}

impl<const MONITORED: bool> Default for SpinLockBase<MONITORED> {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock policy for acquiring a spinlock while saving and disabling interrupts, matching the C++
/// `IrqSave` option.
pub struct IrqSavePolicy;

// SAFETY: `SpinLock::acquire_irq_save` (or saving interrupt state and `SpinLock::acquire`) and
// `SpinLock::release_irq_restore` enforce mutual exclusion on `SpinLock`.
unsafe impl LockPolicy<SpinLock> for IrqSavePolicy {
    type AcquireArgs = ();
    type GuardState = InterruptSavedState;

    #[inline]
    unsafe fn pre_validate(_lock: &SpinLock, state: &mut Self::GuardState, _args: ()) {
        *state = arch_interrupt_save();
    }

    #[inline]
    unsafe fn pre_reacquire(_lock: &SpinLock, state: &mut Self::GuardState) {
        *state = arch_interrupt_save();
    }

    #[inline]
    unsafe fn acquire(lock: &SpinLock, state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        *state = unsafe { lock.acquire_irq_save() };
    }

    #[inline]
    unsafe fn post_validate_acquire(lock: &SpinLock, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Interrupts were disabled in `pre_validate`, and the caller upholds the
        // `LockPolicy::post_validate_acquire` contract.
        unsafe { lock.acquire() };
    }

    #[inline]
    unsafe fn reacquire(lock: &SpinLock, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { <Self as LockPolicy<SpinLock>>::acquire(lock, state, ()) };
    }

    #[inline]
    unsafe fn post_validate_reacquire(lock: &SpinLock, _state: &mut Self::GuardState) {
        // SAFETY: Interrupts were disabled in `pre_reacquire`, and the caller upholds the
        // `LockPolicy::post_validate_reacquire` contract.
        unsafe { lock.acquire() };
    }

    #[inline]
    unsafe fn release(lock: &SpinLock, state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees that `lock` is held by the current CPU with saved
        // interrupt state `*state`.
        unsafe { lock.release_irq_restore(*state) };
    }
}

// SAFETY: `IrqSavePolicy` enforces exclusive mutual exclusion on `SpinLock`.
unsafe impl ExclusiveLockPolicy<SpinLock> for IrqSavePolicy {}

/// Lock policy for acquiring a spinlock with interrupts already disabled, matching the C++
/// `NoIrqSave` option.
///
/// As with the C++ `NoIrqSave` option, users of this policy must have disabled interrupts before
/// acquiring the lock, and keep them disabled until it is released.
///
/// # Panics
///
/// Acquiring (or reacquiring) a lock with this policy panics if interrupts are not disabled on
/// the calling CPU.
pub struct NoIrqSavePolicy;

// SAFETY: `SpinLock::acquire` and `SpinLock::release` enforce mutual exclusion on `SpinLock` when
// interrupts are disabled, which `acquire` asserts.
unsafe impl LockPolicy<SpinLock> for NoIrqSavePolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &SpinLock, _state: &mut Self::GuardState, _args: ()) {
        assert!(ints_disabled());
        // SAFETY: `ints_disabled()` is asserted above, and the caller upholds the
        // `LockPolicy::acquire` contract.
        unsafe { lock.acquire() };
    }

    #[inline]
    unsafe fn reacquire(lock: &SpinLock, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { <Self as LockPolicy<SpinLock>>::acquire(lock, state, ()) };
    }

    #[inline]
    unsafe fn release(lock: &SpinLock, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees that `lock` is held by the current CPU, with interrupts
        // disabled.
        unsafe { lock.release() };
    }
}

// SAFETY: `NoIrqSavePolicy` enforces exclusive mutual exclusion on `SpinLock`.
unsafe impl ExclusiveLockPolicy<SpinLock> for NoIrqSavePolicy {}

/// Saved interrupt state and source tag for a held [`TrackedMonitoredSpinlock`].
#[derive(Copy, Clone, Default)]
pub struct MonitoredSpinlockGuardState {
    pub interrupt_state: InterruptSavedState,
    pub tag: SourceTag,
}

// SAFETY: `MonitoredSpinLock::acquire_irq_save` (or saving interrupt state and
// `MonitoredSpinLock::acquire`) and `MonitoredSpinLock::release_irq_restore` enforce mutual
// exclusion on `MonitoredSpinLock`.
unsafe impl LockPolicy<MonitoredSpinLock> for IrqSavePolicy {
    type AcquireArgs = SourceTag;
    type GuardState = MonitoredSpinlockGuardState;

    #[inline]
    unsafe fn pre_validate(
        _lock: &MonitoredSpinLock,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        let interrupt_state = arch_interrupt_save();
        *state = MonitoredSpinlockGuardState { interrupt_state, tag };
    }

    #[inline]
    unsafe fn pre_reacquire(_lock: &MonitoredSpinLock, state: &mut Self::GuardState) {
        state.interrupt_state = arch_interrupt_save();
    }

    #[inline]
    unsafe fn acquire(
        lock: &MonitoredSpinLock,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: Caller upholds the `LockPolicy::acquire` contract.
        let interrupt_state = unsafe { lock.acquire_irq_save(tag) };
        *state = MonitoredSpinlockGuardState { interrupt_state, tag };
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &MonitoredSpinLock,
        _state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: Interrupts were disabled in `pre_validate`, and the caller upholds the
        // `LockPolicy::post_validate_acquire` contract.
        unsafe { lock.acquire(tag) };
    }

    #[inline]
    unsafe fn reacquire(lock: &MonitoredSpinLock, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        state.interrupt_state = unsafe { lock.acquire_irq_save(state.tag) };
    }

    #[inline]
    unsafe fn post_validate_reacquire(lock: &MonitoredSpinLock, state: &mut Self::GuardState) {
        // SAFETY: Interrupts were disabled in `pre_reacquire`, and the caller upholds the
        // `LockPolicy::post_validate_reacquire` contract.
        unsafe { lock.acquire(state.tag) };
    }

    #[inline]
    unsafe fn release(lock: &MonitoredSpinLock, state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees that `lock` is held by the current CPU with saved
        // interrupt state `state.interrupt_state`.
        unsafe { lock.release_irq_restore(state.interrupt_state) };
    }
}

// SAFETY: `IrqSavePolicy` enforces exclusive mutual exclusion on `MonitoredSpinLock`.
unsafe impl ExclusiveLockPolicy<MonitoredSpinLock> for IrqSavePolicy {}

// SAFETY: `MonitoredSpinLock::acquire` and `MonitoredSpinLock::release` enforce mutual exclusion
// on `MonitoredSpinLock` when interrupts are disabled, which `acquire` asserts.
unsafe impl LockPolicy<MonitoredSpinLock> for NoIrqSavePolicy {
    type AcquireArgs = SourceTag;
    type GuardState = SourceTag;

    #[inline]
    unsafe fn acquire(
        lock: &MonitoredSpinLock,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        assert!(ints_disabled());
        // SAFETY: `ints_disabled()` is asserted above, and the caller upholds the
        // `LockPolicy::acquire` contract.
        unsafe { lock.acquire(tag) };
        *state = tag;
    }

    #[inline]
    unsafe fn reacquire(lock: &MonitoredSpinLock, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe { <Self as LockPolicy<MonitoredSpinLock>>::acquire(lock, state, *state) };
    }

    #[inline]
    unsafe fn release(lock: &MonitoredSpinLock, _state: &mut Self::GuardState) {
        // SAFETY: The caller guarantees that `lock` is held by the current CPU, with interrupts
        // disabled.
        unsafe { lock.release() };
    }
}

// SAFETY: `NoIrqSavePolicy` enforces exclusive mutual exclusion on `MonitoredSpinLock`.
unsafe impl ExclusiveLockPolicy<MonitoredSpinLock> for NoIrqSavePolicy {}

// SAFETY: `SpinLockBase` is a physical kernel spinlock with a unique storage address.
unsafe impl<const MONITORED: bool> RawLock for SpinLockBase<MONITORED>
where
    IrqSavePolicy: ExclusiveLockPolicy<Self>,
{
    type DefaultPolicy = IrqSavePolicy;

    /// Initializes the spinlock in place, assigning it a unique lock ID and the lock class name of
    /// `class_id` for spin tracing (when enabled), as the C++ `SpinLock` constructor and
    /// `SetLockClassId` do.
    ///
    /// # Safety
    ///
    /// `class_id` must be null or a valid static `LockClassId` that outlives the initialized lock.
    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure initializes the provided slot in place before returning `Ok(())`.
        // Once written, the slot is not yet shared, so the closure has the only access to its
        // `name_storage`. The caller guarantees that `class_id` is null or a valid static
        // `LockClassId`, as `set_lock_class_id` requires.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                slot.write(Self::new());
                let name_storage = &mut *UnsafeCell::raw_get(&raw const (*slot).name_storage);
                name_storage.ensure_id();
                name_storage.set_lock_class_id(class_id);
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast::<c_void>()
    }
}

impl<const MONITORED: bool> ConstRawLock for SpinLockBase<MONITORED>
where
    IrqSavePolicy: ExclusiveLockPolicy<Self>,
{
    const INIT: Self = Self::new();
}

impl RawAssertHeldLock for SpinLock {
    #[inline]
    fn assert_held(&self) {
        Self::assert_held(self);
    }
}

impl RawUntrackedLock for SpinLock {
    #[inline]
    unsafe fn release(&self) {
        // SAFETY: Caller upholds the `SpinLock::release` contract.
        unsafe { Self::release(self) };
    }
}

impl RawUntrackedIrqLock for SpinLock {
    type InterruptState = InterruptSavedState;

    #[inline]
    unsafe fn release_irq_restore(&self, state: Self::InterruptState) {
        // SAFETY: Caller upholds the `SpinLock::release_irq_restore` contract.
        unsafe { Self::release_irq_restore(self, state) };
    }
}

impl_lockdep_tracked_lock_policy!(IrqSavePolicy);
impl_lockdep_tracked_lock_policy!(NoIrqSavePolicy);

/// Lockdep-tracked kernel spinlock matching the Zircon C++ `Lock<SpinLock>` layout.
pub type TrackedSpinlock =
    LockdepTracked<SpinLock, IrqSavePolicy, { lockdep::LOCK_FLAGS_IRQ_SAFE }>;

/// Lockdep-tracked kernel monitored spinlock matching the Zircon C++ `Lock<MonitoredSpinLock>`
/// layout.
pub type TrackedMonitoredSpinlock =
    LockdepTracked<MonitoredSpinLock, IrqSavePolicy, { lockdep::LOCK_FLAGS_IRQ_SAFE }>;

// Must match `kExpectedSpinlockSize` and `kExpectedSpinlockAlign` in
// `zircon/kernel/lib/ksync/bindings.cc`, other than for the over-alignment of `SpinLockBase`.
const _: () = {
    #[cfg(with_lock_dep)]
    const BASE_SIZE: usize = 8;
    #[cfg(not(with_lock_dep))]
    const BASE_SIZE: usize = 0;

    // `SpinLockBase` is `#[repr(C, align(8))]`, so the 4-byte non-tracing storage is padded to 8
    // bytes.
    const EXPECTED_STORAGE_SIZE: usize = if SCHEDULER_LOCK_SPIN_TRACING_ENABLED { 16 } else { 8 };
    const EXPECTED_SPINLOCK_SIZE: usize = BASE_SIZE + EXPECTED_STORAGE_SIZE;

    assert!(size_of::<SpinLock>() == EXPECTED_STORAGE_SIZE);
    assert!(align_of::<SpinLock>() == 8);
    assert!(size_of::<MonitoredSpinLock>() == EXPECTED_STORAGE_SIZE);
    assert!(align_of::<MonitoredSpinLock>() == 8);
    assert!(size_of::<TrackedSpinlock>() == EXPECTED_SPINLOCK_SIZE);
    assert!(align_of::<TrackedSpinlock>() == 8);
    assert!(size_of::<TrackedMonitoredSpinlock>() == EXPECTED_SPINLOCK_SIZE);
    assert!(align_of::<TrackedMonitoredSpinlock>() == 8);

    // As for the C++ `SpinLockBase`, the `spin_tracing::LockNameStorage` base class is at offset
    // zero, followed by the `arch_spin_lock_t`.
    assert!(core::mem::offset_of!(SpinLock, name_storage) == 0);
    assert!(
        core::mem::offset_of!(SpinLock, spinlock) == size_of::<UnsafeCell<SpinLockNameStorage>>()
    );
};

/// SpinLock tests.
#[cfg(ktest)]
#[unittest::suite(name = "spinlock")]
mod tests {
    use super::{MonitoredSpinLock, SpinLock, TryAcquireResult};
    use crate::arch_rs::{InterruptDisableGuard, InterruptSavedState};
    use crate::lockup_detector::current_critical_section_depth_and_name;
    use core::pin::Pin;
    use core::ptr;
    use ksync::{KMutex, SourceTag};
    use pin_init::{PinInit, pin_init, stack_pin_init};
    use unittest::{expect_eq, expect_false, expect_true};

    /// Verifies acquiring and releasing a spinlock while saving and restoring interrupt state.
    #[test]
    fn spinlock_lock_unlock() {
        stack_pin_init!(let spinlock = SpinLock::new());

        // SAFETY: `spinlock` is not held on the current CPU and is released immediately below.
        let mut state = unsafe { spinlock.acquire_irq_save() };
        // SAFETY: `spinlock` is held on the current CPU with saved interrupt state `state`.
        unsafe {
            spinlock.release_irq_restore(state);
        }

        // SAFETY: `spinlock` is not held on the current CPU and is released immediately below.
        state = unsafe { spinlock.acquire_irq_save() };
        // SAFETY: `spinlock` is held on the current CPU with saved interrupt state `state`.
        unsafe {
            spinlock.release_irq_restore(state);
        }
    }

    /// Verifies non-blocking spinlock acquisition with `try_acquire`.
    #[test]
    fn spinlock_trylock() {
        stack_pin_init!(let spinlock = SpinLock::new());
        let _interrupt_guard = InterruptDisableGuard::new();

        // SAFETY: Interrupts are disabled by `_interrupt_guard`, and the acquired lock is
        // released below before `_interrupt_guard` is dropped.
        expect_eq!(TryAcquireResult::Acquired, unsafe { spinlock.try_acquire() }, "TryAcquire");
        // SAFETY: Interrupts are disabled by `_interrupt_guard`; `try_acquire` observes the lock
        // is already held and returns `Failed` without re-acquiring.
        expect_eq!(
            TryAcquireResult::Failed,
            unsafe { spinlock.try_acquire() },
            "TryAcquire second time"
        );
        // SAFETY: Interrupts are disabled and `spinlock` is held on the current CPU.
        unsafe {
            spinlock.release();
        }

        // SAFETY: Interrupts are disabled by `_interrupt_guard`, and the acquired lock is
        // released below before `_interrupt_guard` is dropped.
        expect_eq!(TryAcquireResult::Acquired, unsafe { spinlock.try_acquire() }, "TryAcquire");
        // SAFETY: Interrupts are disabled and `spinlock` is held on the current CPU.
        unsafe {
            spinlock.release();
        }
    }

    /// Verifies `is_held` before, during, and after holding a spinlock.
    #[test]
    fn spinlock_is_held() {
        stack_pin_init!(let spinlock = SpinLock::new());
        let _interrupt_guard = InterruptDisableGuard::new();

        expect_false!(spinlock.is_held(), "Lock not held");
        // SAFETY: Interrupts are disabled by `_interrupt_guard` and `spinlock` is not held.
        unsafe {
            spinlock.acquire();
        }
        expect_true!(spinlock.is_held(), "Lock held");
        // SAFETY: Interrupts are disabled and `spinlock` is held on the current CPU.
        unsafe {
            spinlock.release();
        }
        expect_false!(spinlock.is_held(), "Lock not held");
    }

    /// Verifies `assert_held` succeeds when the spinlock is held.
    #[test]
    fn spinlock_assert_held() {
        stack_pin_init!(let spinlock = SpinLock::new());

        // SAFETY: `spinlock` is not held on the current CPU and is released below.
        let state = unsafe { spinlock.acquire_irq_save() };
        spinlock.assert_held(); // Lock is held: this should be a no-op.
        // SAFETY: `spinlock` is held on the current CPU with saved interrupt state `state`.
        unsafe {
            spinlock.release_irq_restore(state);
        }
    }

    /// A struct with a guarded value.
    #[ksync::guarded]
    struct ObjectWithLock {
        #[mutex]
        lock: KMutex<SpinLock>,
        #[guarded_by(lock)]
        val: i32,
        state: InterruptSavedState,
    }

    impl ObjectWithLock {
        fn new() -> impl PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                lock <- KMutex::init(),
                val: 0.into(),
                state: InterruptSavedState::default(),
            })
        }

        /// # Safety
        ///
        /// The calling CPU must not already hold `self.lock`, and the acquisition must be paired
        /// with a matching `release_irq_restore` using `self.state` on the same CPU.
        unsafe fn take_lock(self: Pin<&mut Self>) {
            let this = self.project();
            // SAFETY: The caller guarantees `this.lock` is not held by the calling CPU and will
            // be released with `*this.state` on the same CPU.
            *this.state = unsafe { this.lock.raw_mutex().acquire_irq_save() };
        }
    }

    /// Verifies that `assert_held` grants compile-time access to `ksync`-guarded state.
    #[test]
    fn spinlock_assert_held_compile_test() {
        stack_pin_init!(let object = ObjectWithLock::new());

        // This shouldn't compile with ksync's compile-time lock checking.
        #[cfg(enable_errors)]
        {
            object.val = 3;
        }

        // We take the lock, but Rust's type system can't see it.
        // SAFETY: `object.lock` is not held and is released via `release_irq_restore` below.
        unsafe {
            object.as_mut().take_lock();
        }

        // Without the assertion, Rust will object to setting "val".
        #[cfg(not(enable_errors))]
        // SAFETY: `take_lock()` acquired `object.lock` with interrupts disabled on the current CPU
        // above, `token` does not outlive the lock hold, and no other borrows of guarded fields on
        // `object` exist.
        let mut token = unsafe { object.lock.assert_held() };
        // SAFETY: `token` was obtained from `object.lock` on this `object` instance.
        *unsafe { object.guard_lock_mut(&mut token) }.val_mut() = 3;

        // Without the assertion, Rust will object to releasing the lock.
        // SAFETY: `object.lock` is held on the current CPU with saved state `object.state` and no
        // borrows of data guarded by this acquisition remain live.
        unsafe {
            object.lock.release_irq_restore(object.state, token);
        }
    }

    /// Verifies that a monitored spinlock enters and leaves a named lockup critical section.
    #[test]
    fn monitored_spinlock_lockup_critical_section() {
        stack_pin_init!(let spinlock = MonitoredSpinLock::new());
        let _interrupt_guard = InterruptDisableGuard::new();
        const TAG: SourceTag = SourceTag::from_cstr(c"monitored_spinlock_lockup_critical_section");

        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled by `_interrupt_guard`, `spinlock` is not held, and it is
        // released below.
        let state = unsafe { spinlock.acquire_irq_save(TAG) };
        expect_true!(spinlock.is_held(), "Lock held");
        expect_true!(
            current_critical_section_depth_and_name() == (1, TAG.as_ptr()),
            "critical section depth and name"
        );
        // SAFETY: `spinlock` is held on the current CPU with saved interrupt state `state`.
        unsafe { spinlock.release_irq_restore(state) };
        expect_false!(spinlock.is_held(), "Lock not held");
        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled by `_interrupt_guard`, `spinlock` is not held, and it is
        // released below.
        unsafe { spinlock.acquire(TAG) };
        expect_true!(
            current_critical_section_depth_and_name() == (1, TAG.as_ptr()),
            "critical section depth and name"
        );
        // SAFETY: Interrupts are disabled and `spinlock` is held on the current CPU.
        unsafe { spinlock.release() };
        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );
    }

    /// Verifies that a monitored `try_acquire` only enters a critical section on success.
    #[test]
    fn monitored_spinlock_trylock() {
        stack_pin_init!(let spinlock = MonitoredSpinLock::new());
        let _interrupt_guard = InterruptDisableGuard::new();
        const TAG: SourceTag = SourceTag::from_cstr(c"monitored_spinlock_trylock");
        const OTHER_TAG: SourceTag = SourceTag::from_cstr(c"monitored_spinlock_trylock-other");

        // SAFETY: Interrupts are disabled by `_interrupt_guard`, and the acquired lock is
        // released below before `_interrupt_guard` is dropped.
        expect_eq!(TryAcquireResult::Acquired, unsafe { spinlock.try_acquire(TAG) }, "TryAcquire");
        expect_true!(
            current_critical_section_depth_and_name() == (1, TAG.as_ptr()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled by `_interrupt_guard`; `try_acquire` observes the lock
        // is already held and returns `Failed` without re-acquiring.
        expect_eq!(
            TryAcquireResult::Failed,
            unsafe { spinlock.try_acquire(OTHER_TAG) },
            "TryAcquire second time"
        );
        // A failed attempt leaves the critical section state unchanged.
        expect_true!(
            current_critical_section_depth_and_name() == (1, TAG.as_ptr()),
            "critical section depth and name"
        );

        // SAFETY: Interrupts are disabled and `spinlock` is held on the current CPU.
        unsafe { spinlock.release() };
        expect_true!(
            current_critical_section_depth_and_name() == (0, ptr::null()),
            "critical section depth and name"
        );
    }
}
