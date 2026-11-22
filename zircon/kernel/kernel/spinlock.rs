// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel spinlocks: the `SpinLockBase` family declared in `<kernel/spinlock.h>`, whose C++
//! methods are thin forwarders to the `rust_*` trampolines below, plus the `ksync` raw locks built
//! on them ([`TrackedSpinlock`] and [`TrackedMonitoredSpinlock`]) and their lock policies, which
//! are re-exported from `crate::ksync_kernel`.

// TODO(https://fxbug.dev/581987928): Remove `#![coverage(off)]` once compiler options can control
// atomic coverage counter updates.
#![coverage(off)]

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, offset_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, Ordering};
use ksync::{
    AdoptableLockPolicy, ConstRawLock, ExclusiveLockPolicy, LockPolicy, LockdepTracked,
    RawAssertHeldLock, RawLock, RawUntrackedIrqLock, RawUntrackedLock, SourceTag, TryLockPolicy,
    impl_lockdep_tracked_lock_policy,
};
use pin_init::{PinInit, pin_init_from_closure};

use crate::arch_rs::{
    InterruptSavedState, arch_interrupt_restore, arch_interrupt_save,
    arch_spin_lock_non_instrumented, arch_spin_lock_trace_instrumented, arch_spin_trylock,
    arch_spin_unlock, curr_cpu_num, ints_disabled,
};
use crate::kernel::spin_tracing::{
    EncodedLockId, IdStringRef, LockNameStorageOps, SCHEDULER_LOCK_SPIN_TRACING_ENABLED,
    SpinLockNameStorage,
};
use crate::kernel::types::cpu_num_t;
use crate::ktrace_rs::InternedString;
use crate::lockup_detector::{lockup_begin_raw_on_cpu, lockup_end};

/// Returns which CPU holds `lock`, or `INVALID_CPU` if it is not held.
#[inline(always)]
fn arch_spin_lock_holder_cpu(lock: &AtomicU32) -> cpu_num_t {
    // Relaxed: this predicate does not synchronize other memory operations; see
    // `arch_spin_lock_held`.
    lock.load(Ordering::Relaxed).wrapping_sub(1)
}

/// Returns true if the current CPU holds the lock. In practice, this value is
/// only useful if migration to another CPU is prevented, by disabling preemption
/// or interrupts, while the value is being used.
///
/// Note that this predicate does not synchronize other memory operations (i.e.
/// it has relaxed atomic semantics).
#[inline(always)]
fn arch_spin_lock_held(lock: &AtomicU32) -> bool {
    arch_spin_lock_holder_cpu(lock) == curr_cpu_num()
}

/// Returns true if unlocked (i.e. not held by any CPU). Primarily useful for
/// asserting that locks are not being accessed incorrectly in destructors and
/// cleanup paths.
///
/// Note that this predicate does not synchronize other memory operations (i.e.
/// it has relaxed atomic semantics).
#[inline(always)]
fn arch_spin_lock_unlocked(lock: &AtomicU32) -> bool {
    lock.load(Ordering::Relaxed) == 0
}

/// Options selecting a [`SpinLockBase`] variant, combined with `|` into its `OPTIONS` parameter.
pub struct SpinLockOptions;

impl SpinLockOptions {
    /// A plain spinlock.
    pub const NONE: u32 = 0;

    /// Enable integration with the lockup_detector to monitor spinlock critical sections.
    ///
    /// See //zircon/kernel/lib/lockup_detector/README.md.
    pub const MONITORED: u32 = 1 << 0;
}

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

/// A spinlock whose holder must keep interrupts disabled, optionally integrated with the lockup
/// detector ([`SpinLockOptions::MONITORED`]).
///
/// Layout-compatible with the C++ `SpinLockBase<Options>`: its `spin_tracing::LockNameStorage`
/// base (empty unless scheduler lock spin tracing is enabled) followed by the lock word.
///
/// Both the C++ `SpinLockBase` (`alignas(8)`) and this type (`#[repr(C, align(8))]`) align to 8
/// bytes (resulting in size 8 when spin tracing is disabled, 16 when enabled, and alignment 8) so
/// that `SpinLockBase`, [`TrackedSpinlock`], and `#[repr(C)]` FFI structs embedding them maintain
/// uniform 8-byte alignment across all lockdep and spin tracing configurations.
///
/// Every field is interior-mutable (`UnsafeCell` or `AtomicU32`), so `SpinLockBase` has zero
/// `Freeze` bytes across all configurations. Callers whose lock guards the destruction of its own
/// containing allocation (so another CPU may immediately acquire and free it once unlocked) must
/// release through [`Self::release_raw`] (as C++ `SpinLockBase::Release` does) so no `&Self`
/// reference remains live across the unlock store.
#[repr(C, align(8))]
#[derive(Debug, Default)]
pub struct SpinLockBase<const OPTIONS: u32 = { SpinLockOptions::NONE }> {
    /// Storage for the lock name and encoded lock ID used by lock spin tracing. Corresponds to
    /// the C++ `spin_tracing::LockNameStorage` base class, and is zero-sized when
    /// `SPINLOCK_NAME_TRACING_ENABLED` is false. Wrapped in `UnsafeCell` so `SpinLockBase` has no
    /// `Freeze` bytes in any configuration.
    lock_name_storage: UnsafeCell<SpinLockNameStorage>,
    /// The lock word: the number of the holding CPU plus one, so that 0 means unheld.
    spinlock: AtomicU32,
    /// The C++ `SpinLockBase` cannot be copied or moved, and lockdep identifies a lock by its
    /// address; mirror that by making the lock `!Unpin`. Wrapped in `PhantomData` so
    /// `SpinLockBase` and types embedding it stay FFI-safe (`improper_ctypes` rejects a bare
    /// `PhantomPinned`).
    _pinned: PhantomData<PhantomPinned>,
}

// SAFETY: `SpinLockBase` has no thread-local ownership invariants and is `!Unpin`, so ownership
// of a pinned `SpinLockBase` can be transferred across threads.
unsafe impl<const OPTIONS: u32> Send for SpinLockBase<OPTIONS> {}

// SAFETY: Shared accesses through `&SpinLockBase` operate on the `spinlock` atomic or read
// `lock_name_storage`, which is only mutated through `&mut self` during initialization /
// lock-class setup before concurrent use.
unsafe impl<const OPTIONS: u32> Sync for SpinLockBase<OPTIONS> {}

/// A plain spinlock, matching the C++ `SpinLock`.
pub type SpinLock = SpinLockBase<{ SpinLockOptions::NONE }>;

/// `MonitoredSpinLock` is a `SpinLock` variant that's integrated with the lockup_detector.
///
/// Rust callers should prefer `KMonitoredSpinlock` / `declare_singleton_monitored_spinlock!` from
/// `crate::ksync_kernel`, acquiring a `ksync::Guard` with `ksync::source_tag!()`:
///
/// ```ignore
/// crate::ksync_kernel::declare_singleton_monitored_spinlock!(MyLock);
/// ...
/// {
///     ksync::lock!(let _guard = MyLock::get().lock_with(ksync::source_tag!()));
///     ...
/// }
/// ```
///
/// When used from C++ (`DECLARE_SINGLETON_SPINLOCK_WITH_TYPE` and `Guard<MonitoredSpinLock,
/// IrqSave>`), the last argument passed to `Guard`'s constructor should be a `const char*`
/// C-string with static lifetime (such as `SOURCE_TAG`) that describes the critical section
/// protected by the guard.
pub type MonitoredSpinLock = SpinLockBase<{ SpinLockOptions::MONITORED }>;

impl<const OPTIONS: u32> SpinLockBase<OPTIONS> {
    const IS_MONITORED: bool = (OPTIONS & SpinLockOptions::MONITORED) != SpinLockOptions::NONE;

    /// Creates an unheld spinlock.
    #[inline]
    pub fn new() -> Self {
        Self {
            lock_name_storage: UnsafeCell::new(<SpinLockNameStorage>::new()),
            spinlock: AtomicU32::new(0),
            _pinned: PhantomData,
        }
    }

    /// Creates an unheld spinlock in a `const` context, for statics. When lock spin tracing is
    /// enabled its lock ID is zero until [`LockNameStorageOps::ensure_id`] assigns one.
    #[inline]
    pub const fn const_init() -> Self {
        Self {
            lock_name_storage: UnsafeCell::new(<SpinLockNameStorage>::const_init(
                IdStringRef::INVALID_ID,
            )),
            spinlock: AtomicU32::new(0),
            _pinned: PhantomData,
        }
    }

    /// Creates an unheld spinlock whose lock class is named `lock_name_string_ref` in spin
    /// tracing.
    #[inline]
    pub fn with_lock_name(lock_name_string_ref: &'static InternedString) -> Self {
        Self {
            lock_name_storage: UnsafeCell::new(<SpinLockNameStorage>::with_class_name(
                lock_name_string_ref,
            )),
            spinlock: AtomicU32::new(0),
            _pinned: PhantomData,
        }
    }

    /// Debug-asserts the preconditions of acquiring the spinlock on `cpu` (`curr_cpu_num()`).
    #[inline(always)]
    fn debug_assert_acquirable_on_cpu(&self, cpu: cpu_num_t) {
        debug_assert!(ints_disabled());
        debug_assert!(arch_spin_lock_holder_cpu(&self.spinlock) != cpu);
    }

    /// Acquires the spinlock on `cpu`, without any lockup detector integration.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::acquire`], with `cpu == curr_cpu_num()`.
    #[inline(always)]
    unsafe fn acquire_internal_on_cpu(&self, cpu: cpu_num_t) {
        if !SCHEDULER_LOCK_SPIN_TRACING_ENABLED {
            // SAFETY: The caller has disabled interrupts, `cpu == curr_cpu_num()`, and this CPU
            // does not hold this spinlock.
            unsafe { arch_spin_lock_non_instrumented(&self.spinlock, cpu) };
        } else {
            // SAFETY: The caller has disabled interrupts, `cpu == curr_cpu_num()`, and this CPU
            // does not hold this spinlock.
            unsafe {
                arch_spin_lock_trace_instrumented(&self.spinlock, cpu, self.encoded_lock_id())
            };
        }
    }

    /// Attempts to acquire the spinlock on `cpu` without waiting, and without any lockup detector
    /// integration.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::try_acquire`], with `cpu == curr_cpu_num()`.
    #[inline(always)]
    unsafe fn try_acquire_internal_on_cpu(&self, cpu: cpu_num_t) -> TryAcquireResult {
        // SAFETY: The caller has disabled interrupts and `cpu == curr_cpu_num()`.
        // `arch_spin_trylock` returns `true` when the lock is not acquired.
        if unsafe { arch_spin_trylock(&self.spinlock, cpu) } {
            TryAcquireResult::Failed
        } else {
            TryAcquireResult::Acquired
        }
    }

    /// Releases the spinlock through a raw pointer without keeping a `&Self` reference live across
    /// the unlock store or `lockup_end()`.
    ///
    /// Prefer this over [`Self::release`] when another CPU may immediately acquire the lock and
    /// destroy or free the containing object before this function returns (such as C++
    /// `SpinLockBase::Release` / `MpUnplugEvent`).
    ///
    /// # Safety
    ///
    /// `this` must point to a live, properly aligned `SpinLockBase<OPTIONS>` currently held by the
    /// calling CPU with interrupts disabled, and no borrows protected by this acquisition may
    /// remain live.
    #[inline]
    pub unsafe fn release_raw(this: *const Self) {
        // SAFETY: The caller guarantees `this` is live and held by the calling CPU until the
        // unlock store inside `arch_spin_unlock`, with interrupts disabled.
        unsafe { arch_spin_unlock(&raw const (*this).spinlock) };
        if Self::IS_MONITORED {
            // SAFETY: Balances the `lockup_begin_raw_on_cpu` of the acquisition being released, on
            // the same CPU with interrupts disabled.
            unsafe { lockup_end() };
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
    /// The allocation backing `self` must remain live until this call returns; use
    /// [`Self::release_raw`] instead when releasing a lock that guards the destruction of its own
    /// containing allocation.
    #[inline]
    pub unsafe fn release(&self) {
        // SAFETY: Forwarded from the caller.
        unsafe { Self::release_raw(ptr::from_ref(self)) };
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
        // SAFETY: The caller holds the lock with interrupts disabled (since `state` was saved) and
        // keeps `self` live until this call returns.
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

    /// Returns `true` if unlocked (i.e. not held by any CPU). Primarily useful for asserting that
    /// locks are not being accessed incorrectly in destructors and cleanup paths.
    ///
    /// Note that this predicate does not synchronize other memory operations (i.e. it has relaxed
    /// atomic semantics).
    #[inline]
    pub fn is_unlocked(&self) -> bool {
        arch_spin_lock_unlocked(&self.spinlock)
    }

    /// Returns which CPU currently holds the spinlock, or `INVALID_CPU` if it is not held.
    ///
    /// Note that this predicate does not synchronize other memory operations (i.e. it has relaxed
    /// atomic semantics).
    #[inline]
    pub fn holder_cpu(&self) -> cpu_num_t {
        arch_spin_lock_holder_cpu(&self.spinlock)
    }

    /// Asserts (in builds with debug assertions enabled) that the spinlock is held by the calling
    /// CPU.
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

macro_rules! impl_unmonitored_spinlock {
    ($ty:ty) => {
        impl $ty {
            /// Acquire the spinlock.
            ///
            /// Interrupts must already be disabled.
            ///
            /// # Safety
            ///
            /// Interrupts must already be disabled on the calling CPU, the calling CPU must not
            /// already hold this spinlock, and this acquisition must be paired with a matching
            /// [`Self::release`] or [`Self::release_irq_restore`] on the same CPU before
            /// interrupts are re-enabled or the thread blocks or migrates.
            #[inline]
            pub unsafe fn acquire(&self) {
                let cpu = curr_cpu_num();
                self.debug_assert_acquirable_on_cpu(cpu);
                // SAFETY: `cpu == curr_cpu_num()` (interrupts are disabled), and the remaining
                // requirements are forwarded from the caller.
                unsafe { self.acquire_internal_on_cpu(cpu) };
            }

            /// Attempt to acquire the spinlock without waiting.
            ///
            /// Interrupts must already be disabled.
            ///
            /// Returns [`TryAcquireResult::Acquired`] when the lock is acquired, and
            /// [`TryAcquireResult::Failed`] when the lock is not acquired.
            ///
            /// `try_acquire` operations are not permitted to fail spuriously, even on
            /// architectures with weak memory ordering. If a `try_acquire` operation fails, it
            /// must be because the lock was actually observed to be held during the attempt.
            ///
            /// # Safety
            ///
            /// Interrupts must already be disabled on the calling CPU, and if this call acquires
            /// the lock (returns [`TryAcquireResult::Acquired`]), it must be paired with a
            /// matching [`Self::release`] or [`Self::release_irq_restore`] on the same CPU before
            /// interrupts are re-enabled or the thread blocks or migrates.
            #[inline]
            pub unsafe fn try_acquire(&self) -> TryAcquireResult {
                let cpu = curr_cpu_num();
                // SAFETY: `cpu == curr_cpu_num()` (interrupts are disabled), and the remaining
                // requirements are forwarded from the caller.
                unsafe { self.try_acquire_internal_on_cpu(cpu) }
            }

            /// Just like [`Self::acquire`], but saves interrupt state and disables interrupts.
            ///
            /// # Safety
            ///
            /// The calling CPU must not already hold this spinlock, and the returned
            /// [`InterruptSavedState`] must be passed to a matching [`Self::release_irq_restore`]
            /// on the same CPU before the thread blocks or migrates.
            #[inline]
            #[must_use = "the saved interrupt state must be passed to `release_irq_restore`"]
            pub unsafe fn acquire_irq_save(&self) -> InterruptSavedState {
                let state = arch_interrupt_save();
                // SAFETY: Interrupts were disabled above, and the remaining requirements are
                // forwarded from the caller.
                unsafe { self.acquire() };
                state
            }
        }
    };
}

impl_unmonitored_spinlock!(SpinLock);

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
        // SAFETY: `name` wraps a static NUL-terminated C string, and the caller upholds the rest
        // of `acquire_raw`'s contract.
        unsafe { self.acquire_raw(NonNull::from(name.as_cstr()).cast()) };
    }

    /// [`Self::acquire`] for the C++ trampoline, which only has a raw pointer to `name`.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::acquire`]; additionally, `name` must point to a NUL-terminated C string
    /// with static lifetime.
    #[inline]
    unsafe fn acquire_raw(&self, name: NonNull<c_char>) {
        let cpu = curr_cpu_num();
        self.debug_assert_acquirable_on_cpu(cpu);
        // SAFETY: `cpu == curr_cpu_num()` (interrupts are disabled), the caller guarantees `name`
        // is a static NUL-terminated C string, and the matching `release` balances this with
        // `lockup_end`.
        unsafe { lockup_begin_raw_on_cpu(cpu, name) };
        // SAFETY: `cpu == curr_cpu_num()`, and the remaining requirements are forwarded from the
        // caller.
        unsafe { self.acquire_internal_on_cpu(cpu) };
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
        // SAFETY: `name` wraps a static NUL-terminated C string, and the caller upholds the rest
        // of `try_acquire_raw`'s contract.
        unsafe { self.try_acquire_raw(NonNull::from(name.as_cstr()).cast()) }
    }

    /// [`Self::try_acquire`] for the C++ trampoline, which only has a raw pointer to `name`.
    ///
    /// # Safety
    ///
    /// As for [`SpinLock::try_acquire`]; additionally, `name` must point to a NUL-terminated C
    /// string with static lifetime.
    #[inline]
    unsafe fn try_acquire_raw(&self, name: NonNull<c_char>) -> TryAcquireResult {
        let cpu = curr_cpu_num();
        // SAFETY: `cpu == curr_cpu_num()` (interrupts are disabled), and the remaining
        // requirements are forwarded from the caller.
        let result = unsafe { self.try_acquire_internal_on_cpu(cpu) };
        if result.is_acquired() {
            // SAFETY: `cpu == curr_cpu_num()` (interrupts are disabled), the caller guarantees
            // `name` is a static NUL-terminated C string, and the lock was acquired so the
            // matching `release` balances this with `lockup_end`.
            unsafe { lockup_begin_raw_on_cpu(cpu, name) };
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

impl<const OPTIONS: u32> AsRef<SpinLockBase<OPTIONS>> for SpinLockBase<OPTIONS> {
    #[inline]
    fn as_ref(&self) -> &SpinLockBase<OPTIONS> {
        self
    }
}

impl<const OPTIONS: u32> LockNameStorageOps for SpinLockBase<OPTIONS> {
    /// # Safety
    ///
    /// The caller must ensure that `lcid` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline]
    unsafe fn set_lock_class_id(&mut self, lcid: *const c_void) {
        // SAFETY: `SpinLockNameStorage::set_lock_class_id` has the identical safety contract on
        // `lcid` as `LockNameStorageOps::set_lock_class_id`, which the caller guarantees.
        unsafe { self.lock_name_storage.get_mut().set_lock_class_id(lcid) };
    }

    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline]
    unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self) {
        // SAFETY: `SpinLockNameStorage::set_lock_class` has the identical safety contract on
        // `C::ID` as `LockNameStorageOps::set_lock_class`, which the caller guarantees.
        unsafe { self.lock_name_storage.get_mut().set_lock_class::<C>() };
    }

    #[inline(always)]
    fn encoded_lock_id(&self) -> EncodedLockId {
        // SAFETY: `lock_name_storage` is only mutated through `&mut self` during initialization /
        // lock-class setup before concurrent use.
        unsafe { (*self.lock_name_storage.get()).encoded_lock_id() }
    }

    #[inline]
    fn class_name_ref(&self) -> IdStringRef {
        // SAFETY: `lock_name_storage` is only mutated through `&mut self` during initialization /
        // lock-class setup before concurrent use.
        unsafe { (*self.lock_name_storage.get()).class_name_ref() }
    }

    #[inline]
    fn ensure_id(&mut self) {
        self.lock_name_storage.get_mut().ensure_id();
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

// SAFETY: `NoIrqSavePolicy` requires interrupts to remain disabled externally across the entire
// critical section and neither saves nor restores CPU interrupt state on release, so relocating
// its `GuardState` (`()`) and releasing multiple `NoIrqSavePolicy` spinlocks out of LIFO order
// preserves interrupt-disabled invariants on the holding CPU.
unsafe impl AdoptableLockPolicy<SpinLock> for NoIrqSavePolicy {}

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

// SAFETY: `NoIrqSavePolicy` requires interrupts to remain disabled externally across the entire
// critical section and neither saves nor restores CPU interrupt state on release, and its
// `GuardState` (`SourceTag`) is relocated by value via `core::mem::take`.
unsafe impl AdoptableLockPolicy<MonitoredSpinLock> for NoIrqSavePolicy {}

// SAFETY: `MonitoredSpinLock::try_acquire` and `MonitoredSpinLock::release` establish exclusive
// mutual exclusion on `MonitoredSpinLock` when `try_acquire` returns `Acquired` and interrupts are
// disabled, which `try_acquire` asserts.
unsafe impl TryLockPolicy<MonitoredSpinLock> for NoIrqSavePolicy {
    #[inline]
    unsafe fn try_acquire(
        lock: &MonitoredSpinLock,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) -> bool {
        assert!(ints_disabled());
        // SAFETY: `ints_disabled()` is asserted above, and the caller upholds the
        // `TryLockPolicy::try_acquire` contract.
        let acquired = unsafe { lock.try_acquire(tag) }.is_acquired();
        if acquired {
            *state = tag;
        }
        acquired
    }
}

// SAFETY: `SpinLockBase` is a physical kernel spinlock with a unique storage address.
unsafe impl<const OPTIONS: u32> RawLock for SpinLockBase<OPTIONS>
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
        // Once written, the slot is not yet shared, so the closure has exclusive access to it.
        // The caller guarantees that `class_id` is null or a valid static `LockClassId`, as
        // `set_lock_class_id` requires.
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                slot.write(Self::new());
                (*slot).set_lock_class_id(class_id);
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast::<c_void>()
    }
}

impl<const OPTIONS: u32> ConstRawLock for SpinLockBase<OPTIONS>
where
    IrqSavePolicy: ExclusiveLockPolicy<Self>,
{
    const INIT: Self = Self::const_init();
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

// The C++ `SpinLockBase<Options>` in <kernel/spinlock.h> has the same layout, which it asserts
// with the same constants.
const EXPECTED_SPINLOCK_SIZE: usize = if SCHEDULER_LOCK_SPIN_TRACING_ENABLED { 16 } else { 8 };
const EXPECTED_SPINLOCK_ALIGN: usize = 8;
const EXPECTED_SPINLOCK_WORD_OFFSET: usize =
    if SCHEDULER_LOCK_SPIN_TRACING_ENABLED { 8 } else { 0 };

zr::static_assert!(size_of::<AtomicU32>() == size_of::<cpu_num_t>());
zr::static_assert!(align_of::<AtomicU32>() == align_of::<cpu_num_t>());
zr::static_assert!(size_of::<SpinLock>() == EXPECTED_SPINLOCK_SIZE);
zr::static_assert!(align_of::<SpinLock>() == EXPECTED_SPINLOCK_ALIGN);
zr::static_assert!(offset_of!(SpinLock, lock_name_storage) == 0);
zr::static_assert!(offset_of!(SpinLock, spinlock) == EXPECTED_SPINLOCK_WORD_OFFSET);
zr::static_assert!(size_of::<MonitoredSpinLock>() == EXPECTED_SPINLOCK_SIZE);
zr::static_assert!(align_of::<MonitoredSpinLock>() == EXPECTED_SPINLOCK_ALIGN);
zr::static_assert!(offset_of!(MonitoredSpinLock, spinlock) == EXPECTED_SPINLOCK_WORD_OFFSET);

// Must match `kExpectedSpinlockSize` and `kExpectedSpinlockAlign` in
// `zircon/kernel/lib/ksync/bindings.cc`.
const EXPECTED_TRACKED_SPINLOCK_BASE_SIZE: usize = if cfg!(with_lock_dep) { 8 } else { 0 };
const EXPECTED_TRACKED_SPINLOCK_SIZE: usize =
    EXPECTED_TRACKED_SPINLOCK_BASE_SIZE + EXPECTED_SPINLOCK_SIZE;

zr::static_assert!(size_of::<TrackedSpinlock>() == EXPECTED_TRACKED_SPINLOCK_SIZE);
zr::static_assert!(align_of::<TrackedSpinlock>() == EXPECTED_SPINLOCK_ALIGN);
zr::static_assert!(size_of::<TrackedMonitoredSpinlock>() == EXPECTED_TRACKED_SPINLOCK_SIZE);
zr::static_assert!(align_of::<TrackedMonitoredSpinlock>() == EXPECTED_SPINLOCK_ALIGN);

// FFI trampolines for the C++ `SpinLockBase<Options>` methods in <kernel/spinlock.h>, which
// carry the thread safety annotations and forward to these. Each requires what the corresponding
// method requires of its callers: in particular, interrupts are disabled on the calling CPU and
// the lock is held or not held as that method expects.

/// FFI trampoline for [`SpinLock::acquire`].
///
/// # Safety
///
/// As for [`SpinLock::acquire`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_spinlock_acquire(lock: &SpinLock) {
    // SAFETY: The caller upholds `acquire`'s contract.
    unsafe { lock.acquire() };
}

/// FFI trampoline for [`MonitoredSpinLock::acquire`].
///
/// # Safety
///
/// As for [`MonitoredSpinLock::acquire`]; additionally, `name` must point to a NUL-terminated C
/// string with static lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_monitored_spinlock_acquire(
    lock: &MonitoredSpinLock,
    name: NonNull<c_char>,
) {
    // SAFETY: The caller upholds `acquire_raw`'s contract.
    unsafe { lock.acquire_raw(name) };
}

/// FFI trampoline for [`MonitoredSpinLock::try_acquire`]. Returns false when the lock is
/// acquired, and true when the lock is not acquired.
///
/// # Safety
///
/// As for [`MonitoredSpinLock::try_acquire`]; additionally, `name` must point to a NUL-terminated
/// C string with static lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_monitored_spinlock_try_acquire(
    lock: &MonitoredSpinLock,
    name: NonNull<c_char>,
) -> bool {
    // SAFETY: The caller upholds `try_acquire_raw`'s contract.
    !unsafe { lock.try_acquire_raw(name) }.is_acquired()
}

/// FFI trampoline for [`SpinLockBase::release_raw`] on a [`SpinLock`].
///
/// # Safety
///
/// As for [`SpinLockBase::release_raw`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_spinlock_release(lock: *const SpinLock) {
    // SAFETY: The caller upholds `release_raw`'s contract.
    unsafe { SpinLock::release_raw(lock) };
}

/// FFI trampoline for [`SpinLockBase::release_raw`] on a [`MonitoredSpinLock`].
///
/// # Safety
///
/// As for [`SpinLockBase::release_raw`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_monitored_spinlock_release(lock: *const MonitoredSpinLock) {
    // SAFETY: The caller upholds `release_raw`'s contract.
    unsafe { MonitoredSpinLock::release_raw(lock) };
}

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
