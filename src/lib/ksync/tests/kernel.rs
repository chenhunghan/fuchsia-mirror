// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(
    dead_code,
    unused_parens,
    unused_results,
    unsafe_op_in_unsafe_fn,
    missing_unsafe_on_extern,
    unsafe_attr_outside_unsafe,
    non_local_definitions,
    mismatched_lifetime_syntaxes,
    clippy::all,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default
)]

#[cfg(ktest)]
#[unittest::suite]
/// Tests for Rust ksync bindings
mod ksync_tests {
    use crate::kernel::mutex::Mutex;
    use crate::kernel::spinlock::{MonitoredSpinLock, SpinLock};
    use crate::ksync_kernel::{
        CriticalMutexOrderedPolicy, CriticalMutexPolicy, IrqSavePolicy, MutexSpinMaxDurationPolicy,
        NoIrqSavePolicy, RawBrwLockNoPi, RawBrwLockPi, TrackedCriticalMutex,
        TrackedMonitoredSpinlock, TrackedMutex, TrackedSpinlock, TryAcquireResult,
        declare_singleton_critical_mutex, declare_singleton_monitored_spinlock,
        declare_singleton_mutex, declare_singleton_spinlock,
    };
    use kprint::kprintln;
    use pin_init::{pin_data, pin_init, stack_pin_init};
    use unittest::{assert_true, expect_eq, expect_true};

    #[ksync::guarded]
    #[fbl::ref_counted]
    #[derive(fbl::Recyclable)]
    #[pin_data]
    #[repr(C)]
    struct GuardedMutexObj {
        #[mutex]
        mu: ksync::KMutex<TrackedMutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedSpinlockObj {
        #[mutex]
        mu: ksync::KMutex<TrackedSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedSpinlockObj2 {
        #[mutex]
        mu: ksync::KMutex<TrackedSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedMonitoredSpinlockObj {
        #[mutex]
        mu: ksync::KMutex<TrackedMonitoredSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedCriticalMutexObj {
        #[mutex]
        mu: ksync::KMutex<TrackedCriticalMutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedNestableCriticalMutexObj {
        #[mutex(flags = ksync::LOCK_FLAGS_NESTABLE | lockdep::LOCK_FLAGS_RE_ACQUIRE_FATAL)]
        mu: ksync::KMutex<TrackedCriticalMutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedNonFatalNestableCriticalMutexObj {
        #[mutex(flags = ksync::LOCK_FLAGS_NESTABLE)]
        mu: ksync::KMutex<TrackedCriticalMutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedUntrackedMutexObj {
        #[mutex]
        mu: ksync::KMutex<Mutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedUntrackedSpinlockObj {
        #[mutex]
        mu: ksync::KMutex<SpinLock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedUntrackedMonitoredSpinlockObj {
        #[mutex]
        mu: ksync::KMutex<MonitoredSpinLock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedBrwLockObj {
        #[brwlock]
        lock: ksync::BrwLockPi<RawBrwLockPi>,
        #[guarded_by(lock)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedBrwLockNoPiObj {
        #[brwlock]
        lock: ksync::BrwLockPi<RawBrwLockNoPi>,
        #[guarded_by(lock)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedBrwLockMultiAcquireObj {
        #[brwlock(flags = lockdep::LOCK_FLAGS_MULTI_ACQUIRE)]
        lock: ksync::BrwLockPi<RawBrwLockPi>,
        #[guarded_by(lock)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedOnceObj {
        #[mutex]
        mu: ksync::KMutex<TrackedMutex>,
        #[guarded_by(mu)]
        once_val: ksync::KOnceCell<u32>,
    }

    #[ksync::guarded]
    struct GuardedPhantomObj {
        #[mutex(GuardedMutexObjMuClass)]
        mu: ksync::KMutex<ksync::PhantomMutex>,
        #[guarded_by(mu)]
        target: fbl::RefPtr<GuardedMutexObj>,
    }

    #[ksync::guarded]
    #[fbl::ref_counted]
    #[derive(fbl::Recyclable)]
    #[pin_data]
    #[repr(C)]
    struct GuardedGenericObj<T> {
        #[mutex]
        mu: ksync::KMutex<TrackedMutex>,
        #[guarded_by(mu)]
        value: T,
    }

    #[ksync::guarded]
    struct GuardedGenericPhantomObj<T> {
        #[mutex(GuardedGenericObjMuClass<T>)]
        mu: ksync::KMutex<ksync::PhantomMutex>,
        #[guarded_by(mu)]
        target: fbl::RefPtr<GuardedGenericObj<T>>,
    }

    unsafe extern "C" {
        fn cpp_verify_mutex_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_critical_mutex_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_spinlock_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_monitored_spinlock_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_brwlock_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_brwlock_no_pi_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;

        fn cpp_get_test_singleton_mutex_size() -> usize;
        fn cpp_get_test_singleton_mutex_raw_lock_ptr() -> *const core::ffi::c_void;
        fn cpp_get_test_singleton_mutex_raw_lock_size() -> usize;

        fn cpp_get_test_singleton_critical_mutex_size() -> usize;
        fn cpp_get_test_singleton_critical_mutex_raw_lock_ptr() -> *const core::ffi::c_void;
        fn cpp_get_test_singleton_critical_mutex_raw_lock_size() -> usize;

        fn cpp_ksync_test_lock_validation_enabled() -> bool;
        fn cpp_ksync_test_lockdep_last_result() -> u8;
    }

    // LINT.IfChange
    const LOCKDEP_RESULT_SUCCESS: u8 = 0;
    const LOCKDEP_RESULT_INVALID_NESTING: u8 = 3;
    // LINT.ThenChange(//zircon/kernel/tests/ksync_tests.cc)

    /// Simulates losing the order across `call_unlocked` by reacquiring at order 0.
    #[derive(Debug, Default, Clone, Copy)]
    struct LostOrderPolicy;

    // SAFETY: Every operation delegates to `CriticalMutexOrderedPolicy`, which provides mutual
    // exclusion; only the lockdep order passed on reacquire differs.
    unsafe impl ksync::LockPolicy<TrackedCriticalMutex> for LostOrderPolicy {
        type AcquireArgs = usize;
        type GuardState =
            <CriticalMutexOrderedPolicy as ksync::LockPolicy<TrackedCriticalMutex>>::GuardState;
        const EXTERNAL_ORDER: bool = true;

        unsafe fn acquire(lock: &TrackedCriticalMutex, state: &mut Self::GuardState, order: usize) {
            // SAFETY: The caller upholds `LockPolicy::acquire`'s contract, which is identical for
            // `CriticalMutexOrderedPolicy`.
            unsafe {
                <CriticalMutexOrderedPolicy as ksync::LockPolicy<_>>::acquire(lock, state, order);
            }
        }

        unsafe fn reacquire(lock: &TrackedCriticalMutex, state: &mut Self::GuardState) {
            // Reset `state` after the preceding `release` so that it satisfies `acquire`'s
            // precondition of a freshly `Default`-initialized `GuardState` on the same pinned
            // slot. Deliberately reacquiring at order 0 instead of `state.order` is what this
            // policy tests; it only affects lockdep validation, not mutual exclusion.
            *state = Self::GuardState::default();
            // SAFETY: `state` is freshly `Default`-initialized and remains pinned on the stack
            // until the matching `release` call per `LockPolicy::reacquire`'s contract.
            unsafe {
                <CriticalMutexOrderedPolicy as ksync::LockPolicy<_>>::acquire(lock, state, 0);
            }
        }

        unsafe fn release(lock: &TrackedCriticalMutex, state: &mut Self::GuardState) {
            // SAFETY: The caller upholds `LockPolicy::release`'s contract; `state` came from this
            // policy's `acquire`/`reacquire`, which produce `CriticalMutexOrderedPolicy` state.
            unsafe {
                <CriticalMutexOrderedPolicy as ksync::LockPolicy<_>>::release(lock, state);
            }
        }
    }

    // SAFETY: `LostOrderPolicy` delegates lock acquisition and release to
    // `CriticalMutexOrderedPolicy`, which guarantees exclusive mutual exclusion.
    unsafe impl ksync::ExclusiveLockPolicy<TrackedCriticalMutex> for LostOrderPolicy {}

    /// test Rust KMutex ID
    #[test]
    fn mutex_id() {
        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        })
        .unwrap();
        // SAFETY: `obj.mu` is an initialized `KMutex`.
        unsafe {
            assert_true!(cpp_verify_mutex_id(
                (&raw const obj.mu).cast::<core::ffi::c_void>(),
                <GuardedMutexObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KCriticalMutex ID
    #[test]
    fn critical_mutex_id() {
        stack_pin_init!(let obj = pin_init!(GuardedCriticalMutexObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        // SAFETY: `obj.mu` is an initialized `KMutex<_, TrackedCriticalMutex>`.
        unsafe {
            assert_true!(cpp_verify_critical_mutex_id(
                (&raw const obj.mu).cast::<core::ffi::c_void>(),
                <GuardedCriticalMutexObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test ordered TrackedCriticalMutex acquisition and call_unlocked reacquire
    #[test]
    fn critical_mutex_ordered() {
        stack_pin_init!(let a = pin_init!(GuardedNestableCriticalMutexObj {
            mu <- ksync::KMutex::init(),
            value: 1.into(),
        }));
        stack_pin_init!(let b = pin_init!(GuardedNestableCriticalMutexObj {
            mu <- ksync::KMutex::init(),
            value: 2.into(),
        }));

        // Acquire `a` (order 0) then `b` (order 1), and test `call_unlocked` on `b` while holding
        // `a`.
        {
            ksync::lock!(
                let mut ga = a.lock_mu_policy_with::<CriticalMutexOrderedPolicy>(0)
            );
            ksync::lock!(
                let mut gb = b.lock_mu_policy_with::<CriticalMutexOrderedPolicy>(1)
            );
            let sum = gb.as_mut().call_unlocked(|| *ga.value() + 10);
            *gb.as_mut().value_mut() = sum;
            *ga.as_mut().value_mut() = 3;
        }

        // Acquire in reverse instance order (`b` at order 0, then `a` at order 1).
        {
            ksync::lock!(
                let gb = b.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(0)
            );
            ksync::lock!(
                let ga = a.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(1)
            );
            expect_true!(*b.guard_mu_from_guard(&gb).value() == 11);
            expect_true!(*a.guard_mu_from_guard(&ga).value() == 3);
        }

        // Exercise `lock_<mu>_aliased_policy_with` with `CriticalMutexOrderedPolicy`.
        {
            type Ordered = CriticalMutexOrderedPolicy;
            ksync::lock!(let ga = a.lock_mu_aliased_policy_with::<_, _, Ordered>(&a.mu, 0));
            ksync::lock!(let gb = b.lock_mu_policy_with::<Ordered>(1));
            let (t1, _t2) = ga.tokens();
            // SAFETY: `t1` was acquired from `a.mu`.
            let guarded_a = unsafe { a.guard_mu(t1) };
            expect_true!(*guarded_a.value() == 3);
            expect_true!(*gb.value() == 11);
        }
    }

    /// test that ordered reacquire in call_unlocked and retrack in call_untracked pass lockdep
    #[test]
    fn critical_mutex_ordered_call_unlocked() {
        // SAFETY: No preconditions.
        if unsafe { cpp_ksync_test_lock_validation_enabled() } {
            let last = || {
                // SAFETY: Runs on a valid kernel thread; reads only the current thread's
                // `ThreadLockState`.
                unsafe { cpp_ksync_test_lockdep_last_result() }
            };

            stack_pin_init!(let a = pin_init!(GuardedNonFatalNestableCriticalMutexObj {
                mu <- ksync::KMutex::init(),
                value: 0.into(),
            }));
            stack_pin_init!(let b = pin_init!(GuardedNonFatalNestableCriticalMutexObj {
                mu <- ksync::KMutex::init(),
                value: 7.into(),
            }));

            {
                ksync::lock!(
                    let ga = a.lock_mu_policy_with::<CriticalMutexOrderedPolicy>(0)
                );
                ksync::lock!(
                    let mut gb = b.lock_mu_policy_with::<CriticalMutexOrderedPolicy>(1)
                );
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
                gb.as_mut().call_unlocked(|| ());
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
                // The lockdep entry (and its order) survives `call_untracked`, so retracking `b`
                // at order 1 while holding `a` at order 0 validates successfully.
                let value = gb.as_mut().call_untracked(|token| {
                    // SAFETY: `token` was acquired from `b.mu`.
                    *unsafe { b.guard_mu(token) }.value()
                });
                expect_true!(value == 7);
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
                expect_true!(*ga.value() == 0);
            }
        }
    }

    /// test that lockdep detects invalid nesting and a lost order on ordered reacquire
    #[test]
    fn critical_mutex_ordered_lost_order_detected() {
        // SAFETY: No preconditions.
        if unsafe { cpp_ksync_test_lock_validation_enabled() } {
            struct LockdepExceptBlockGuard;
            impl Drop for LockdepExceptBlockGuard {
                fn drop(&mut self) {
                    kprintln!("[===KSYNC LOCKDEP EXCEPT BLOCK END===]");
                }
            }
            kprintln!("[===KSYNC LOCKDEP EXCEPT BLOCK START===]");
            let _except_block = LockdepExceptBlockGuard;

            let last = || {
                // SAFETY: Runs on a valid kernel thread; reads only the current thread's
                // `ThreadLockState`.
                unsafe { cpp_ksync_test_lockdep_last_result() }
            };

            stack_pin_init!(let a = pin_init!(GuardedNonFatalNestableCriticalMutexObj {
                mu <- ksync::KMutex::init(),
                value: 0.into(),
            }));
            stack_pin_init!(let b = pin_init!(GuardedNonFatalNestableCriticalMutexObj {
                mu <- ksync::KMutex::init(),
                value: 0.into(),
            }));

            // Equal orders: the second acquisition is not strictly above the first.
            {
                ksync::lock!(
                    let _ga = a.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(1)
                );
                ksync::lock!(
                    let _gb = b.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(1)
                );
                expect_true!(last() == LOCKDEP_RESULT_INVALID_NESTING);
            }

            // Descending orders (`a` at 1, then `b` at 0), as in the C++ lockdep tests.
            {
                ksync::lock!(
                    let _ga = a.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(1)
                );
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
                ksync::lock!(
                    let _gb = b.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(0)
                );
                expect_true!(last() == LOCKDEP_RESULT_INVALID_NESTING);
            }

            // Reacquiring at order 0 instead of the saved order 1 inside `call_unlocked`.
            {
                ksync::lock!(
                    let _ga = a.mu.lock_policy_with::<CriticalMutexOrderedPolicy>(0)
                );
                ksync::lock!(let mut gb = b.mu.lock_policy_with::<LostOrderPolicy>(1));
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
                gb.as_mut().call_unlocked(|| ());
                expect_true!(last() == LOCKDEP_RESULT_INVALID_NESTING);
            }
        }
    }

    /// test Rust KSpinlock ID
    #[test]
    fn spinlock_id() {
        stack_pin_init!(let obj = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        // SAFETY: `obj.mu` is an initialized `KMutex<_, TrackedSpinlock>`.
        unsafe {
            assert_true!(cpp_verify_spinlock_id(
                (&raw const obj.mu).cast::<core::ffi::c_void>(),
                <GuardedSpinlockObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KMonitoredSpinlock ID and acquire/release with source_tag!
    #[test]
    fn monitored_spinlock_id_and_ops() {
        stack_pin_init!(let obj = pin_init!(GuardedMonitoredSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));
        // SAFETY: `obj.mu` is an initialized `KMutex<_, TrackedMonitoredSpinlock>`.
        unsafe {
            assert_true!(cpp_verify_monitored_spinlock_id(
                (&raw const obj.mu).cast::<core::ffi::c_void>(),
                <GuardedMonitoredSpinlockObjMuClass as ksync::LockClass>::ID,
            ));
        }

        // Acquire with default IrqSavePolicy and source_tag!
        {
            ksync::lock!(let mut guard = obj.lock_mu(ksync::source_tag!()));
            expect_eq!(*guard.value(), 10);
            *guard.as_mut().value_mut() = 20;
            expect_eq!(*guard.value(), 20);
        }

        // Verify persistence and acquire with explicit policy
        {
            ksync::lock!(
                let guard = obj.lock_mu_policy::<IrqSavePolicy>(ksync::source_tag!())
            );
            expect_eq!(*guard.value(), 20);
        }
    }

    /// test Rust BrwLockPi and RawBrwLockNoPi IDs
    #[test]
    fn brwlock_id() {
        stack_pin_init!(let obj = pin_init!(GuardedBrwLockObj {
            lock <- ksync::BrwLockPi::init(),
            value: 0.into(),
        }));
        // SAFETY: `obj.lock` is an initialized `BrwLockPi`.
        unsafe {
            assert_true!(cpp_verify_brwlock_id(
                (&raw const obj.lock).cast::<core::ffi::c_void>(),
                <GuardedBrwLockObjLockClass as ksync::LockClass>::ID,
            ));
        }

        stack_pin_init!(let no_pi_obj = pin_init!(GuardedBrwLockNoPiObj {
            lock <- ksync::BrwLockPi::init(),
            value: 0.into(),
        }));
        // SAFETY: `no_pi_obj.lock` is an initialized `BrwLockPi<_, RawBrwLockNoPi>`.
        unsafe {
            assert_true!(cpp_verify_brwlock_no_pi_id(
                (&raw const no_pi_obj.lock).cast::<core::ffi::c_void>(),
                <GuardedBrwLockNoPiObjLockClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KSpinlock
    #[test]
    fn spinlock() {
        stack_pin_init!(let obj = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 100.into(),
        }));
        stack_pin_init!(let obj2 = pin_init!(GuardedSpinlockObj2 {
            mu <- ksync::KMutex::init(),
            value: 100.into(),
        }));

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_eq!(*guard.value(), 100);
            *guard.as_mut().value_mut() = 101;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_eq!(*guard.value(), 101);
        }
        {
            ksync::lock!(let guard = obj.mu.lock());
            ksync::lock!(let guard2 = obj2.mu.lock_policy::<NoIrqSavePolicy>());
            // Expect a no irq save policy guard to be strictly smaller than one that had to
            // remember the irq state.
            expect_true!(core::mem::size_of_val(&*guard2) < core::mem::size_of_val(&*guard));
        }
    }

    /// test Rust KMutex
    #[test]
    fn mutex() {
        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 42.into(),
        })
        .unwrap();

        expect_true!(core::ptr::eq(
            <TrackedMutex as ksync::RawLock>::as_mut_ptr(obj.mu.raw_mutex()),
            <Mutex as ksync::RawLock>::as_mut_ptr(&**obj.mu.raw_mutex()),
        ));

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_eq!(*guard.value(), 42);
            *guard.as_mut().value_mut() = 43;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_eq!(*guard.value(), 43);
        }
    }

    /// test untracked Mutex directly with both standard and critical policies
    #[test]
    fn untracked_mutex() {
        stack_pin_init!(let obj = pin_init!(GuardedUntrackedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));

        expect_true!(!obj.mu.raw_mutex().is_contested());

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_eq!(*guard.value(), 10);
            *guard.as_mut().value_mut() = 20;
            let res = guard.as_mut().call_unlocked(|| 99);
            expect_eq!(res, 99);
        }

        {
            ksync::lock!(let mut guard = obj.lock_mu_policy::<CriticalMutexPolicy>());
            expect_eq!(*guard.value(), 20);
            *guard.as_mut().value_mut() = 30;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_eq!(*guard.value(), 30);
        }
    }

    /// test MutexSpinMaxDurationPolicy acquire, call_unlocked, and call_untracked
    #[test]
    fn mutex_spin_max_duration() {
        // 1us monotonic duration, in nanoseconds.
        const SPIN_MAX_DURATION: i64 = 1_000;

        // SAFETY: No preconditions.
        let lock_validation_enabled = unsafe { cpp_ksync_test_lock_validation_enabled() };
        let last = || {
            // SAFETY: Runs on a valid kernel thread; reads only the current thread's
            // `ThreadLockState`.
            unsafe { cpp_ksync_test_lockdep_last_result() }
        };

        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 1.into(),
        })
        .unwrap();

        {
            ksync::lock!(
                let mut guard =
                    obj.lock_mu_policy_with::<MutexSpinMaxDurationPolicy>(
                        SPIN_MAX_DURATION
                    )
            );
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            expect_true!(obj.mu.raw_mutex().is_held());
            expect_true!(*guard.value() == 1);
            let held = guard.as_mut().call_unlocked(|| obj.mu.raw_mutex().is_held());
            expect_true!(!held);
            expect_true!(obj.mu.raw_mutex().is_held());
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            let (held_in_untracked, untracked_val) = guard.as_mut().call_untracked(|token| {
                let held = obj.mu.raw_mutex().is_held();
                // SAFETY: `token` was acquired from `obj.mu`.
                *unsafe { obj.guard_mu_mut(token) }.value_mut() = 2;
                // SAFETY: `token` was acquired from `obj.mu`.
                (held, *unsafe { obj.guard_mu(token) }.value())
            });
            expect_true!(held_in_untracked);
            expect_true!(untracked_val == 2);
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
        }
        expect_true!(!obj.mu.raw_mutex().is_held());

        // Exercise direct `KMutex::lock_policy_with` with boundary duration `0`.
        {
            ksync::lock!(
                let mut guard =
                    obj.mu.lock_policy_with::<MutexSpinMaxDurationPolicy>(0)
            );
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            expect_true!(obj.mu.raw_mutex().is_held());
            expect_true!(*obj.guard_mu_from_guard(&guard).value() == 2);
            let held = guard.as_mut().call_unlocked(|| obj.mu.raw_mutex().is_held());
            expect_true!(!held);
            expect_true!(obj.mu.raw_mutex().is_held());
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            *obj.guard_mu_mut_from_guard(guard.as_mut()).value_mut() = 3;
        }
        expect_true!(!obj.mu.raw_mutex().is_held());

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_true!(*guard.value() == 3);
        }

        // Exercise `GuardedFlaggedMutexObj` (`#[mutex(flags = ...)]`) as well as a
        // `LockdepTracked` mutex with non-default `const LOCK_FLAGS` (verifying `const LOCK_FLAGS`
        // in the 3-arg `impl_lockdep_tracked_lock_policy!` arm).
        stack_pin_init!(let flagged = pin_init!(GuardedFlaggedMutexObj {
            seek_lock <- ksync::KMutex::init(),
            seek: 100.into(),
        }));
        {
            ksync::lock!(
                let mut guard =
                    flagged.lock_seek_lock_policy_with::<MutexSpinMaxDurationPolicy>(
                        SPIN_MAX_DURATION
                    )
            );
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            expect_true!(flagged.seek_lock.raw_mutex().is_held());
            expect_true!(*guard.seek() == 100);
            *guard.as_mut().seek_mut() = 200;
        }
        expect_true!(!flagged.seek_lock.raw_mutex().is_held());

        type FlaggedTrackedMutex =
            ksync::LockdepTracked<Mutex, Mutex, { lockdep::LOCK_FLAGS_ACTIVE_LIST_DISABLED }>;
        stack_pin_init!(
            let const_flagged_mu =
                ksync::KMutex::<GuardedFlaggedMutexObjSeekLockClass, FlaggedTrackedMutex>::init()
        );
        {
            ksync::lock!(
                let _guard = const_flagged_mu
                    .lock_policy_with::<MutexSpinMaxDurationPolicy>(SPIN_MAX_DURATION)
            );
            if lock_validation_enabled {
                expect_true!(last() == LOCKDEP_RESULT_SUCCESS);
            }
            expect_true!(const_flagged_mu.raw_mutex().is_held());
        }
        expect_true!(!const_flagged_mu.raw_mutex().is_held());

        stack_pin_init!(let untracked = pin_init!(GuardedUntrackedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));

        {
            ksync::lock!(
                let mut guard =
                    untracked.lock_mu_policy_with::<MutexSpinMaxDurationPolicy>(
                        SPIN_MAX_DURATION
                    )
            );
            expect_true!(untracked.mu.raw_mutex().is_held());
            expect_true!(*guard.value() == 10);
            let held = guard.as_mut().call_unlocked(|| untracked.mu.raw_mutex().is_held());
            expect_true!(!held);
            expect_true!(untracked.mu.raw_mutex().is_held());
            let (held_in_untracked, untracked_val) = guard.as_mut().call_untracked(|token| {
                let held = untracked.mu.raw_mutex().is_held();
                // SAFETY: `token` was acquired from `untracked.mu`.
                *unsafe { untracked.guard_mu_mut(token) }.value_mut() = 20;
                // SAFETY: `token` was acquired from `untracked.mu`.
                (held, *unsafe { untracked.guard_mu(token) }.value())
            });
            expect_true!(held_in_untracked);
            expect_true!(untracked_val == 20);
        }
        expect_true!(!untracked.mu.raw_mutex().is_held());

        {
            ksync::lock!(let guard = untracked.lock_mu());
            expect_true!(*guard.value() == 20);
        }
    }

    /// test untracked SpinLock and MonitoredSpinLock directly
    #[test]
    fn untracked_spinlock() {
        stack_pin_init!(let obj = pin_init!(GuardedUntrackedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));
        stack_pin_init!(let mobj = pin_init!(GuardedUntrackedMonitoredSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 50.into(),
        }));

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_eq!(*guard.value(), 10);
            *guard.as_mut().value_mut() = 20;

            ksync::lock!(
                let mut mguard = mobj.lock_mu_policy::<NoIrqSavePolicy>(ksync::source_tag!())
            );
            expect_eq!(*mguard.value(), 50);
            *mguard.as_mut().value_mut() = 60;
        }

        {
            ksync::lock!(let mut mguard = mobj.lock_mu(ksync::source_tag!()));
            expect_eq!(*mguard.value(), 60);
            let res = mguard.as_mut().call_unlocked(|| 77);
            expect_eq!(res, 77);
            *mguard.as_mut().value_mut() = 70;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_eq!(*guard.value(), 20);
            ksync::lock!(let mguard = mobj.lock_mu(ksync::source_tag!()));
            expect_eq!(*mguard.value(), 70);
        }
    }

    /// test raw SpinLock methods and KMutex<_, SpinLock> assert/release helpers
    #[test]
    fn spinlock_raw_methods() {
        stack_pin_init!(let outer = SpinLock::new());
        stack_pin_init!(let inner = SpinLock::new());
        stack_pin_init!(let obj = pin_init!(GuardedUntrackedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));

        // SAFETY: `outer` is not held by this CPU, and it is released below with the returned
        // state on this CPU without blocking.
        let outer_state = unsafe { outer.acquire_irq_save() };
        expect_true!(outer.is_held());
        outer.assert_held();

        // Holding `outer` keeps interrupts disabled, so the non-IRQ-saving operations on `inner`
        // and `obj.mu` below are valid.
        expect_true!(!inner.is_held());
        // SAFETY: Interrupts are disabled, `inner` is not held, and it is released below.
        unsafe { inner.acquire() };
        expect_true!(inner.is_held());
        inner.assert_held();
        // SAFETY: Interrupts are disabled. `inner` is already held, so this must fail and not
        // acquire the lock.
        let result = unsafe { inner.try_acquire() };
        expect_eq!(result, TryAcquireResult::Failed);
        expect_true!(!result.is_acquired());
        // SAFETY: Interrupts are disabled and `inner` is held by this CPU.
        unsafe { inner.release() };
        expect_true!(!inner.is_held());

        // SAFETY: Interrupts are disabled, and if acquired `inner` is released below.
        let result = unsafe { inner.try_acquire() };
        expect_eq!(result, TryAcquireResult::Acquired);
        expect_true!(inner.is_held());
        if result.is_acquired() {
            // SAFETY: Interrupts are disabled and `inner` was acquired by this CPU above.
            unsafe { inner.release() };
        }
        expect_true!(!inner.is_held());

        // SAFETY: Interrupts are disabled, `obj.mu` is not held, and it is released below via
        // `KMutex::release`.
        unsafe { obj.mu.raw_mutex().acquire() };
        {
            // SAFETY: `obj.mu` is held by this CPU with interrupts disabled, and `token` is
            // consumed by `release` below with no outstanding borrows.
            let token = unsafe { obj.mu.assert_held() };
            // SAFETY: `token` witnesses `obj.mu`, which guards `obj.value`.
            let value = *unsafe { obj.guard_mu(&token) }.value();
            expect_eq!(value, 10);
            // SAFETY: `obj.mu` is held by this CPU via `acquire` with interrupts disabled, `token`
            // witnesses that acquisition, and no guarded borrows remain live.
            unsafe { obj.mu.release(token) };
        }
        expect_true!(!obj.mu.raw_mutex().is_held());

        // SAFETY: `outer` is held by this CPU and `outer_state` came from its acquisition.
        unsafe { outer.release_irq_restore(outer_state) };

        // SAFETY: `obj.mu` is not held by this CPU, and it is released below via
        // `KMutex::release_irq_restore` with the returned state.
        let state = unsafe { obj.mu.raw_mutex().acquire_irq_save() };
        {
            // SAFETY: `obj.mu` is held by this CPU with interrupts disabled, and `token` is
            // consumed by `release_irq_restore` below with no outstanding borrows.
            let mut token = unsafe { obj.mu.assert_held() };
            // SAFETY: `token` witnesses `obj.mu`, which guards `obj.value`.
            *unsafe { obj.guard_mu_mut(&mut token) }.value_mut() = 20;
            // SAFETY: `token` witnesses `obj.mu`, which guards `obj.value`.
            let value = *unsafe { obj.guard_mu(&token) }.value();
            expect_eq!(value, 20);
            // SAFETY: `obj.mu` is held by this CPU via `acquire_irq_save`, which returned `state`,
            // `token` witnesses that acquisition, and no guarded borrows remain live.
            unsafe { obj.mu.release_irq_restore(state, token) };
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_eq!(*guard.value(), 20);
        }
    }

    /// test KSpinlock::assert_held on a lockdep-tracked spinlock
    #[test]
    fn spinlock_assert_held() {
        stack_pin_init!(let obj = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 30.into(),
        }));

        ksync::lock!(let guard = obj.lock_mu());
        // SAFETY: `guard` holds `obj.mu` with interrupts disabled, `token` does not outlive
        // `guard`, and both are only used for shared access.
        let token = unsafe { obj.mu.assert_held() };
        // SAFETY: `token` witnesses `obj.mu`, which guards `obj.value`.
        let value = *unsafe { obj.guard_mu(&token) }.value();
        expect_eq!(value, 30);
        expect_eq!(*guard.value(), 30);
    }

    /// test PhantomMutex and #[mutex(LockClass)]
    #[test]
    fn phantom_lock() {
        // This test demonstrates how an object (`phantom_obj`) can use a zero-sized `PhantomMutex`
        // to participate in `#[guarded]` type checking while physical mutual exclusion is
        // provided by an external physical lock (`real_obj.mu`).

        // Create an object with a real, physical `KMutex`, wrapped in an `fbl::RefPtr`.
        let real_obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 1.into(),
        })
        .unwrap();

        // Create an object protected by a zero-sized `PhantomMutex` (no physical lock storage).
        // Its synchronization is provided externally by `real_obj.mu`.
        let target = real_obj.clone();
        stack_pin_init!(let phantom_obj = pin_init!(GuardedPhantomObj {
            mu: ksync::KMutex::new(ksync::PhantomMutex),
            target: target.into(),
        }));

        {
            // Acquire a guard for `phantom_obj` by passing the physical `real_obj.mu` to `lock_mu`.
            // SAFETY: `real_obj.mu` is the physical lock synchronizing `phantom_obj` and `target`.
            ksync::lock!(let mut guard = unsafe { phantom_obj.lock_mu(&real_obj.mu) });

            // Read the reference to `real_obj` from `phantom_obj.target`.
            let target = guard.target().clone();

            // Using the proof token from `guard`, read `target.value`.
            // SAFETY: `guard` holds `real_obj.mu`, which protects `target`.
            unsafe {
                expect_eq!(*target.guard_mu(guard.token()).value(), 1);

                // Modify `target.value` using the mutable token from `guard`.
                *target.guard_mu_mut(guard.as_mut().token_mut()).value_mut() = 200;
            }
        }

        {
            // Lock `real_obj` directly using `GuardedMutexObj`'s own guard to verify
            // that we read back the same value (200) that was written via `phantom_obj`.
            ksync::lock!(let guard = real_obj.lock_mu());
            expect_eq!(*guard.value(), 200);
        }
    }

    /// test PhantomMutex with generic LockClass
    #[test]
    fn generic_phantom_mutex() {
        let real_obj = fbl::pin_make_ref_counted!(GuardedGenericObj {
            mu <- ksync::KMutex::init(),
            value: 42.into(),
        })
        .unwrap();

        let real_obj_clone = real_obj.clone();
        stack_pin_init!(let phantom_obj = pin_init!(GuardedGenericPhantomObj {
            mu: ksync::KMutex::new(ksync::PhantomMutex),
            target: ksync::KCell::new(real_obj_clone),
        }));

        {
            // SAFETY: `real_obj.mu` is the physical lock synchronizing `phantom_obj` and `target`.
            ksync::lock!(let mut guard = unsafe { phantom_obj.lock_mu(&real_obj.mu) });
            let target = guard.target().clone();
            // SAFETY: `guard` holds `real_obj.mu`, which protects `target`.
            unsafe {
                expect_eq!(*target.guard_mu(guard.token()).value(), 42);
                *target.guard_mu_mut(guard.as_mut().token_mut()).value_mut() = 99;
            }
        }

        {
            ksync::lock!(let guard = real_obj.lock_mu());
            expect_eq!(*guard.value(), 99);
        }
    }

    /// test Rust BrwLockPi with RawBrwLockPi and RawBrwLockNoPi
    #[test]
    fn brwlock() {
        stack_pin_init!(let obj = pin_init!(GuardedBrwLockObj {
            lock <- ksync::BrwLockPi::init(),
            value: 10.into(),
        }));

        {
            ksync::lock!(let mut guard = obj.read_lock());
            expect_eq!(*guard.value(), 10);
            let res = guard.as_mut().call_unlocked(|| 55);
            expect_eq!(res, 55);
            expect_eq!(*guard.value(), 10);
        }

        {
            ksync::lock!(let mut guard = obj.write_lock());
            expect_eq!(*guard.value(), 10);
            let res = guard.as_mut().call_unlocked(|| 66);
            expect_eq!(res, 66);
            *guard.as_mut().value_mut() = 20;
        }

        {
            ksync::lock!(let guard = obj.read_lock());
            expect_eq!(*guard.value(), 20);
        }

        stack_pin_init!(let no_pi_obj = pin_init!(GuardedBrwLockNoPiObj {
            lock <- ksync::BrwLockPi::init(),
            value: 30.into(),
        }));

        {
            ksync::lock!(let mut guard = no_pi_obj.read_lock());
            expect_eq!(*guard.value(), 30);
            let res = guard.as_mut().call_unlocked(|| 77);
            expect_eq!(res, 77);
            expect_eq!(*guard.value(), 30);
        }

        {
            ksync::lock!(let mut guard = no_pi_obj.write_lock());
            expect_eq!(*guard.value(), 30);
            let res = guard.as_mut().call_unlocked(|| 88);
            expect_eq!(res, 88);
            *guard.as_mut().value_mut() = 40;
        }

        {
            ksync::lock!(let guard = no_pi_obj.read_lock());
            expect_eq!(*guard.value(), 40);
        }
    }

    /// test call_unlocked on KMutex and macro-generated guards
    #[test]
    fn call_unlocked() {
        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        })
        .unwrap();

        ksync::lock!(let mut guard = obj.lock_mu());
        expect_eq!(*guard.value(), 10);

        let res = guard.as_mut().call_unlocked(|| 42);
        expect_eq!(res, 42);

        *guard.as_mut().value_mut() = 25;
        expect_eq!(*guard.value(), 25);
    }

    /// test AliasedLock in kernel
    #[test]
    fn aliased_lock() {
        let real_obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        })
        .unwrap();

        let target = real_obj.clone();
        stack_pin_init!(let phantom_obj = pin_init!(GuardedPhantomObj {
            mu: ksync::KMutex::new(ksync::PhantomMutex),
            target: target.into(),
        }));

        {
            ksync::lock!(let mut guard = ksync::aliased_lock(&real_obj.mu, &real_obj.mu));

            let (t1, t2) = guard.tokens();
            // SAFETY: `real_obj` and `phantom_obj` are both protected by `real_obj.mu`, and
            // `_t2_mut` is unused.
            unsafe {
                expect_eq!(*real_obj.guard_mu(t1).value(), 10);
                expect_true!(core::ptr::eq(&**phantom_obj.guard_mu(t2).target(), &*real_obj));

                let (t1_mut, _t2_mut) = guard.as_mut().tokens_mut();
                *real_obj.guard_mu_mut(t1_mut).value_mut() = 55;
            }

            let res = guard.as_mut().call_unlocked(|| 99);
            expect_eq!(res, 99);

            let (t1, _t2) = guard.tokens();
            // SAFETY: `t1` is backed by `real_obj.mu`.
            unsafe {
                expect_eq!(*real_obj.guard_mu(t1).value(), 55);
            }
        }

        ksync::lock!(let guard = real_obj.lock_mu());
        expect_eq!(*guard.value(), 55);
    }

    #[ksync::guarded]
    struct GuardedFlaggedMutexObj {
        #[mutex(flags = lockdep::LOCK_FLAGS_ACTIVE_LIST_DISABLED)]
        seek_lock: ksync::KMutex<TrackedMutex>,
        #[guarded_by(seek_lock)]
        seek: u64,
    }

    /// test Mutex with custom flags (e.g. LOCK_FLAGS_ACTIVE_LIST_DISABLED)
    #[test]
    fn flagged_mutex() {
        stack_pin_init!(let obj = pin_init!(GuardedFlaggedMutexObj {
            seek_lock <- ksync::KMutex::init(),
            seek: 0.into(),
        }));

        ksync::lock!(let mut guard = obj.lock_seek_lock());
        expect_eq!(*guard.seek(), 0);
        *guard.as_mut().seek_mut() = 4096;
        expect_eq!(*guard.seek(), 4096);
    }

    declare_singleton_mutex!(TestKernelSingletonMutex);
    declare_singleton_critical_mutex!(TestKernelSingletonCriticalMutex);
    declare_singleton_spinlock!(TestKernelSingletonSpinlock);

    /// test Rust declare_singleton_mutex and declare_singleton_critical_mutex
    #[test]
    fn singleton_mutex() {
        let lock1 = TestKernelSingletonMutex::Get();
        let lock2 = TestKernelSingletonMutex::get();
        expect_true!(core::ptr::eq(lock1, lock2));

        // SAFETY: All singleton locks and C++ test helper pointers/sizes are valid static objects.
        // The singleton locks are not acquired or released while their raw storage is compared.
        unsafe {
            expect_true!(cpp_verify_mutex_id(
                (&raw const *lock1).cast::<core::ffi::c_void>(),
                <TestKernelSingletonMutex as ksync::LockClass>::ID,
            ));
            expect_true!(cpp_verify_critical_mutex_id(
                (&raw const *TestKernelSingletonCriticalMutex::Get()).cast::<core::ffi::c_void>(),
                <TestKernelSingletonCriticalMutex as ksync::LockClass>::ID,
            ));
            expect_true!(cpp_verify_spinlock_id(
                (&raw const *TestKernelSingletonSpinlock::Get()).cast::<core::ffi::c_void>(),
                <TestKernelSingletonSpinlock as ksync::LockClass>::ID,
            ));

            // Compare total sizes of Rust-declared singletons vs C++-declared singletons.
            let rust_mutex_size = core::mem::size_of_val(lock1);
            let cpp_mutex_size = cpp_get_test_singleton_mutex_size();
            expect_eq!(rust_mutex_size, cpp_mutex_size);

            let rust_critical_size =
                core::mem::size_of_val(TestKernelSingletonCriticalMutex::Get());
            let cpp_critical_size = cpp_get_test_singleton_critical_mutex_size();
            expect_eq!(rust_critical_size, cpp_critical_size);

            // Compare raw mutex storage byte-by-byte between Rust and C++.
            let cpp_raw_ptr = cpp_get_test_singleton_mutex_raw_lock_ptr();
            let cpp_raw_size = cpp_get_test_singleton_mutex_raw_lock_size();
            let cpp_raw_bytes = core::slice::from_raw_parts(cpp_raw_ptr as *const u8, cpp_raw_size);

            let rust_raw_bytes = lock1.raw_mutex().raw_storage_slice();
            expect_true!(rust_raw_bytes == cpp_raw_bytes);

            // Compare CriticalMutex raw storage byte-by-byte between Rust and C++.
            let cpp_critical_raw_ptr = cpp_get_test_singleton_critical_mutex_raw_lock_ptr();
            let cpp_critical_raw_size = cpp_get_test_singleton_critical_mutex_raw_lock_size();
            let cpp_critical_raw_bytes = core::slice::from_raw_parts(
                cpp_critical_raw_ptr as *const u8,
                cpp_critical_raw_size,
            );

            let rust_critical_raw_bytes =
                TestKernelSingletonCriticalMutex::Get().raw_mutex().raw_storage_slice();
            expect_true!(rust_critical_raw_bytes == cpp_critical_raw_bytes);
        }

        {
            ksync::lock!(let _guard = TestKernelSingletonMutex::Get().lock());
        }

        {
            ksync::lock!(TestKernelSingletonMutex::lock());
        }

        {
            ksync::lock!(let _guard = TestKernelSingletonCriticalMutex::Get().lock());
        }

        // Verify that a singleton spinlock registers LOCK_FLAGS_IRQ_SAFE so it can be acquired
        // while holding another IRQ-safe spinlock without triggering a lockdep violation.
        stack_pin_init!(let outer = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        {
            ksync::lock!(let _outer_guard = outer.lock_mu());
            ksync::lock!(let _inner_guard = TestKernelSingletonSpinlock::lock());
        }
    }

    declare_singleton_monitored_spinlock!(TestKernelCpuAlignedSingletonSpinlock, cpu_align);

    declare_singleton_monitored_spinlock!(
        TestKernelCpuAlignExclusiveSingletonSpinlock,
        cpu_align_exclusive,
    );

    /// test that `cpu_align` and `cpu_align_exclusive` place a singleton lock on its own cache line
    #[test]
    fn singleton_aligned() {
        let lock = TestKernelCpuAlignedSingletonSpinlock::get();
        expect_true!(core::ptr::from_ref(lock).addr().is_multiple_of(64));
        {
            ksync::lock!(TestKernelCpuAlignedSingletonSpinlock::lock(ksync::source_tag!()));
        }

        let lock = TestKernelCpuAlignExclusiveSingletonSpinlock::get();
        expect_true!(core::ptr::from_ref(lock).addr().is_multiple_of(64));
        {
            ksync::lock!(TestKernelCpuAlignExclusiveSingletonSpinlock::lock(ksync::source_tag!()));
        }

        // `kernel.ld` places `.data.cpu_align_exclusive` at the start of `.data`, before the lock
        // class registrations.
        unsafe extern "C" {
            static __data_start: u8;
            static __start_rust_lock_classes: u8;
        }
        let addr = core::ptr::from_ref(lock).addr();
        expect_true!(addr >= (&raw const __data_start).addr());
        expect_true!(addr < (&raw const __start_rust_lock_classes).addr());
    }

    /// test KOnceCell lazy initialization and access
    #[test]
    fn konce_cell_test() {
        static TEST_CELL: ksync::KOnceCell<u64, TestKernelSingletonMutex> = ksync::KOnceCell::new();
        ksync::lock!(let mut guard = TestKernelSingletonMutex::lock());
        // SAFETY: `guard` holds the singleton lock `TestKernelSingletonMutex` guarding `TEST_CELL`.
        unsafe {
            let token = guard.token();
            expect_true!(!TEST_CELL.is_initialized(token));
            expect_true!(TEST_CELL.get(token).is_none());

            let token_mut = guard.as_mut().token_mut();
            let val = TEST_CELL.get_or_init(|| 12345u64, token_mut);
            expect_eq!(*val, 12345);

            let token = guard.token();
            expect_true!(TEST_CELL.is_initialized(token));
            expect_true!(TEST_CELL.get(token) == Some(&12345));

            let unchk = TEST_CELL.get_unchecked();
            expect_true!(unchk == Some(&12345));
        }
    }

    /// test KOnceCell integration with #[ksync::guarded]
    #[test]
    fn guarded_konce_cell_test() {
        stack_pin_init!(let obj = pin_init!(GuardedOnceObj {
            mu <- ksync::KMutex::init(),
            once_val: ksync::KOnceCell::new(),
        }));

        // Before init: read accessor returns None
        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_true!(guard.once_val().is_none());
        }

        // Initialize through mutable guard
        {
            ksync::lock!(let mut guard = obj.lock_mu());
            let mut once_cell = guard.as_mut().once_val_cell();
            let val = once_cell.get_or_init(|| 42);
            expect_eq!(*val, 42);

            // Read after init
            expect_true!(guard.once_val() == Some(&42));
            expect_true!(guard.as_mut().once_val_mut() == Some(&mut 42));

            // Attempting to set fails because it's already initialized
            expect_true!(guard.as_mut().once_val_cell().set(99) == Err(99));

            // Test proxy cell access
            expect_true!(guard.as_mut().once_val_cell().get() == Some(&42));

            // Disjoint fields access
            let fields = guard.fields();
            expect_true!(fields.once_val == Some(&42));

            let fields_mut = guard.as_mut().fields_mut();
            expect_true!(fields_mut.once_val == Some(&mut 42));
        }
    }

    #[ksync::guarded]
    struct GuardedMultiSpinlockObj {
        #[mutex(flags = lockdep::LOCK_FLAGS_MULTI_ACQUIRE)]
        mu: ksync::KMutex<TrackedSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    /// test explicit release() and KMutexGuardMultiple in kernel
    #[test]
    fn guard_multiple_and_release_test() {
        stack_pin_init!(let s1 = pin_init!(GuardedMultiSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));
        stack_pin_init!(let s2 = pin_init!(GuardedMultiSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 20.into(),
        }));

        // Test single guard early release
        {
            ksync::lock!(let mut g = s1.lock_mu());
            *g.as_mut().value_mut() = 11;
            g.as_mut().release();

            ksync::lock!(let g2 = s1.lock_mu());
            expect_eq!(*g2.value(), 11);
        }

        // Test multi-lock in [s1, s2] and [s2, s1] order with early release
        {
            ksync::lock!(
                let mut guards = GuardedMultiSpinlockObj::lock_mu_multiple([&*s1, &*s2])
            );
            let [t1, t2] = guards.as_mut().tokens_mut();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`, and `s1` and `s2` are
            // distinct instances.
            unsafe {
                *s1.guard_mu_mut(t1).value_mut() = 100;
                *s2.guard_mu_mut(t2).value_mut() = 200;
            }
        }

        {
            ksync::lock!(
                let mut guards = GuardedMultiSpinlockObj::lock_mu_multiple([&*s2, &*s1])
            );
            let [t1, t2] = guards.tokens();
            // SAFETY: Both `s1.mu` and `s2.mu` are held by `guards`.
            unsafe {
                expect_eq!(*s1.guard_mu(t1).value(), 100);
                expect_eq!(*s2.guard_mu(t2).value(), 200);
            }
            guards.as_mut().release();

            ksync::lock!(let g1 = s1.lock_mu());
            ksync::lock!(let g2 = s2.lock_mu());
            expect_eq!(*g1.value(), 100);
            expect_eq!(*g2.value(), 200);
        }
    }

    #[ksync::guarded]
    struct GuardedMultiLockObj {
        #[mutex]
        mu1: ksync::KMutex<TrackedMutex>,
        #[mutex]
        mu2: ksync::KMutex<TrackedMutex>,
        #[guarded_by(mu1)]
        val1: u32,
        #[guarded_by(mu2)]
        val2: u32,
        #[guarded_by(mu1, mu2)]
        shared: u32,
    }

    /// test #[guarded_by(mu1, mu2)] in kernel
    #[test]
    fn guarded_multi_lock_test() {
        stack_pin_init!(let obj = pin_init!(GuardedMultiLockObj {
            mu1 <- ksync::KMutex::init(),
            mu2 <- ksync::KMutex::init(),
            val1: 1.into(),
            val2: 2.into(),
            shared: 10.into(),
        }));

        {
            ksync::lock!(let g1 = obj.lock_mu1());
            expect_eq!(*g1.val1(), 1);
            expect_eq!(*g1.shared(), 10);
        }

        {
            ksync::lock!(let g2 = obj.lock_mu2());
            expect_eq!(*g2.val2(), 2);
            expect_eq!(*g2.shared(), 10);
        }

        {
            ksync::lock!(let mut g1 = obj.lock_mu1());
            ksync::lock!(let mut g2 = obj.lock_mu2());
            *g1.as_mut().shared_mut(&mut g2) = 20;
            expect_eq!(*g1.shared(), 20);
            expect_eq!(*g2.shared(), 20);

            *g2.as_mut().shared_mut(&mut g1) = 30;
            expect_eq!(*g1.shared(), 30);
        }
    }

    /// test brwlock with custom flags attribute (e.g. LOCK_FLAGS_MULTI_ACQUIRE)
    #[test]
    fn guarded_brwlock_multi_acquire_test() {
        stack_pin_init!(let obj = pin_init!(GuardedBrwLockMultiAcquireObj {
            lock <- ksync::BrwLockPi::init(),
            value: 42.into(),
        }));

        ksync::lock!(let guard = obj.read_lock());
        expect_eq!(*guard.value(), 42);
    }

    #[ksync::guarded]
    struct GuardedGenericLockObj<L: ksync::RawLock> {
        #[mutex]
        mu: ksync::KMutex<L>,
        #[guarded_by(mu)]
        value: u32,
    }

    /// test #[ksync::guarded] with generic lock types
    #[test]
    fn generic_lock_guarded_test() {
        stack_pin_init!(let mutex_obj = pin_init!(GuardedGenericLockObj::<TrackedMutex> {
            mu <- ksync::KMutex::init(),
            value: 10.into(),
        }));
        stack_pin_init!(let crit_obj = pin_init!(GuardedGenericLockObj::<TrackedCriticalMutex> {
            mu <- ksync::KMutex::init(),
            value: 20.into(),
        }));
        stack_pin_init!(let spin_obj = pin_init!(GuardedGenericLockObj::<TrackedSpinlock> {
            mu <- ksync::KMutex::init(),
            value: 30.into(),
        }));
        stack_pin_init!(
            let mon_obj = pin_init!(GuardedGenericLockObj::<TrackedMonitoredSpinlock> {
                mu <- ksync::KMutex::init(),
                value: 40.into(),
            })
        );

        // SAFETY: All four objects have initialized `KMutex` instances of the corresponding raw
        // lock types.
        unsafe {
            assert_true!(cpp_verify_mutex_id(
                core::ptr::from_ref(&mutex_obj.mu).cast::<core::ffi::c_void>(),
                <GuardedGenericLockObjMuClass<TrackedMutex> as ksync::LockClass>::ID,
            ));
            assert_true!(cpp_verify_critical_mutex_id(
                core::ptr::from_ref(&crit_obj.mu).cast::<core::ffi::c_void>(),
                <GuardedGenericLockObjMuClass<TrackedCriticalMutex> as ksync::LockClass>::ID,
            ));
            assert_true!(cpp_verify_spinlock_id(
                core::ptr::from_ref(&spin_obj.mu).cast::<core::ffi::c_void>(),
                <GuardedGenericLockObjMuClass<TrackedSpinlock> as ksync::LockClass>::ID,
            ));
            assert_true!(cpp_verify_monitored_spinlock_id(
                core::ptr::from_ref(&mon_obj.mu).cast::<core::ffi::c_void>(),
                <GuardedGenericLockObjMuClass<TrackedMonitoredSpinlock> as ksync::LockClass>::ID,
            ));
        }

        // Verify that IRQ-safe and non-IRQ-safe instantiations select distinct LockClass IDs
        // when lockdep metadata/validation is enabled.
        if !<GuardedGenericLockObjMuClass<TrackedMutex> as ksync::LockClass>::ID.is_null() {
            expect_true!(
                <GuardedGenericLockObjMuClass<TrackedMutex> as ksync::LockClass>::ID
                    != <GuardedGenericLockObjMuClass<TrackedSpinlock> as ksync::LockClass>::ID
            );
        } else {
            expect_true!(
                <GuardedGenericLockObjMuClass<TrackedSpinlock> as ksync::LockClass>::ID.is_null()
            );
        }

        {
            ksync::lock!(let mut g = mutex_obj.lock_mu());
            expect_eq!(*g.value(), 10);
            *g.as_mut().value_mut() = 11;
            expect_eq!(*g.value(), 11);
        }

        {
            ksync::lock!(let mut g = crit_obj.lock_mu());
            expect_eq!(*g.value(), 20);
            *g.as_mut().value_mut() = 21;
            expect_eq!(*g.value(), 21);
        }

        // Verify that GuardedGenericLockObj<TrackedSpinlock> registers LOCK_FLAGS_IRQ_SAFE so it
        // can be acquired while holding another IRQ-safe spinlock without triggering a lockdep
        // violation.
        stack_pin_init!(let outer = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        {
            ksync::lock!(let _outer_guard = outer.lock_mu());
            ksync::lock!(let mut g = spin_obj.lock_mu());
            expect_eq!(*g.value(), 30);
            *g.as_mut().value_mut() = 31;
            expect_eq!(*g.value(), 31);
        }

        {
            ksync::lock!(let mut g = mon_obj.lock_mu_with(ksync::source_tag!()));
            expect_eq!(*g.value(), 40);
            *g.as_mut().value_mut() = 41;
            expect_eq!(*g.value(), 41);
        }
    }
}
