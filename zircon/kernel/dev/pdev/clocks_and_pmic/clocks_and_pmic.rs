// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Platform Device (pdev) Clocks and PMIC Interface.
//!
//! Provides platform-level abstraction and dispatch for platform clock and
//! power rail management during suspend and resume.

use core::sync::atomic::Ordering;
#[cfg(ktest)]
use unittest as _;
use zr::AtomicConstPtr;
use zx_status::Status;

// clocks_and_pmic interface
/// Platform device clocks and PMIC operations table.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct PdevClocksAndPmicOps {
    /// Prepare the platform-specific clocks and power rails for entering a
    /// suspended state.
    pub prepare_for_suspend: Option<extern "C" fn() -> Result<(), Status>>,
    /// Prepare the platform-specific clocks and power rails for operation
    /// immediately after exiting a suspended state.
    pub wakeup_from_suspend: Option<extern "C" fn() -> Result<(), Status>>,
}

zr::static_assert!(core::mem::size_of::<PdevClocksAndPmicOps>() == 16);
zr::static_assert!(core::mem::align_of::<PdevClocksAndPmicOps>() == 8);

extern "C" fn default_prepare_for_suspend() -> Result<(), Status> {
    Ok(())
}

extern "C" fn default_wakeup_from_suspend() -> Result<(), Status> {
    Ok(())
}

static DEFAULT_OPS: PdevClocksAndPmicOps = PdevClocksAndPmicOps {
    prepare_for_suspend: Some(default_prepare_for_suspend),
    wakeup_from_suspend: Some(default_wakeup_from_suspend),
};

static CLOCKS_AND_PMIC_OPS: AtomicConstPtr<PdevClocksAndPmicOps> =
    AtomicConstPtr::new(core::ptr::addr_of!(DEFAULT_OPS));

fn get_ops() -> &'static PdevClocksAndPmicOps {
    let ops_ptr = CLOCKS_AND_PMIC_OPS.load(Ordering::Acquire);
    // SAFETY: `CLOCKS_AND_PMIC_OPS` always holds either null, `&DEFAULT_OPS`, or a registered
    // `PdevClocksAndPmicOps` table with `'static` lifetime guaranteed by the caller of
    // `pdev_register_clocks_and_pmic`.
    unsafe { ops_ptr.as_ref() }.unwrap_or(&DEFAULT_OPS)
}

fn register_clocks_and_pmic(ops: *const PdevClocksAndPmicOps) {
    // Note that registration of this interface must happen before the system has
    // brought up the secondary CPUs, so before LK_INIT_LEVEL_PLATFORM.  We'd like
    // to assert! that here, but unfortunately, the per-cpu init level of the
    // system is not published in the per-cpu data, merely passed to registered
    // init hooks, so we'll have to settle for a comment instead.
    //
    // Additionally, once a non-default interface has been registered, it may not
    // be changed afterwards.  We can at least assert that.  Do so now.
    let target_ptr = if ops.is_null() { core::ptr::addr_of!(DEFAULT_OPS) } else { ops };
    assert!(
        CLOCKS_AND_PMIC_OPS
            .compare_exchange(
                core::ptr::addr_of!(DEFAULT_OPS),
                target_ptr,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok(),
        "clocks_and_pmic interface already registered"
    );
}

/// Registers the platform clocks and PMIC operations table with `'static` lifetime.
pub fn pdev_register_clocks_and_pmic(ops: &'static PdevClocksAndPmicOps) {
    register_clocks_and_pmic(core::ptr::from_ref(ops));
}

/// Prepare the platform-specific clocks and power rails for entering a
/// suspended state.  This operation is required to be both idempotent and
/// thread-safe.
pub fn clocks_and_pmic_prepare_for_suspend() -> Result<(), Status> {
    if let Some(prepare_for_suspend) = get_ops().prepare_for_suspend {
        return prepare_for_suspend();
    }

    Err(Status::NOT_SUPPORTED)
}

/// Prepare the platform-specific clocks and power rails for operation
/// immediately after exiting a suspended state. This operation is required to be
/// both idempotent and thread-safe.
pub fn clocks_and_pmic_wakeup_from_suspend() -> Result<(), Status> {
    if let Some(wakeup_from_suspend) = get_ops().wakeup_from_suspend {
        return wakeup_from_suspend();
    }

    Err(Status::NOT_SUPPORTED)
}

/// Swaps the current clocks and PMIC operations table with a new one for testing, returning the
/// previous table.
#[cfg(ktest)]
fn swap_ops_for_test(
    ops: Option<&'static PdevClocksAndPmicOps>,
) -> Option<&'static PdevClocksAndPmicOps> {
    let target_ptr = ops.map_or(core::ptr::null(), core::ptr::from_ref);
    let prev_ptr = CLOCKS_AND_PMIC_OPS.swap(target_ptr, Ordering::AcqRel);
    // SAFETY: `CLOCKS_AND_PMIC_OPS` always holds either null, `&DEFAULT_OPS`, or a `'static`
    // reference to a `PdevClocksAndPmicOps` table.
    unsafe { prev_ptr.as_ref() }
}

// C FFI exports

/// Registers the platform clocks and PMIC operations table from C-ABI.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevClocksAndPmicOps` table that remains valid for the
/// duration of the kernel's execution.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_register_clocks_and_pmic(ops: *const PdevClocksAndPmicOps) {
    register_clocks_and_pmic(ops);
}

/// Prepare the platform-specific clocks and power rails for entering a
/// suspended state (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_clocks_and_pmic_prepare_for_suspend() -> Result<(), Status> {
    clocks_and_pmic_prepare_for_suspend()
}

/// Prepare the platform-specific clocks and power rails for operation
/// immediately after exiting a suspended state (C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn rust_clocks_and_pmic_wakeup_from_suspend() -> Result<(), Status> {
    clocks_and_pmic_wakeup_from_suspend()
}

/// In-kernel unit tests for the platform clocks and PMIC subsystem.
#[cfg(ktest)]
#[unittest::suite(name = "pdev_clocks_and_pmic")]
mod tests {
    use super::{
        DEFAULT_OPS, PdevClocksAndPmicOps, Status, clocks_and_pmic_prepare_for_suspend,
        clocks_and_pmic_wakeup_from_suspend, get_ops, pdev_register_clocks_and_pmic,
        rust_clocks_and_pmic_prepare_for_suspend, rust_clocks_and_pmic_wakeup_from_suspend,
        rust_pdev_register_clocks_and_pmic, swap_ops_for_test,
    };
    use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
    use unittest::{assert_eq, assert_err, assert_ok, assert_true};

    static TEST_PREPARE_CALLED: AtomicU32 = AtomicU32::new(0);
    static TEST_WAKEUP_CALLED: AtomicU32 = AtomicU32::new(0);
    static TEST_PREPARE_STATUS: AtomicI32 = AtomicI32::new(0);
    static TEST_WAKEUP_STATUS: AtomicI32 = AtomicI32::new(0);

    extern "C" fn test_prepare_for_suspend() -> Result<(), Status> {
        TEST_PREPARE_CALLED.fetch_add(1, Ordering::Relaxed);
        Status::ok(TEST_PREPARE_STATUS.load(Ordering::Relaxed))
    }

    extern "C" fn test_wakeup_from_suspend() -> Result<(), Status> {
        TEST_WAKEUP_CALLED.fetch_add(1, Ordering::Relaxed);
        Status::ok(TEST_WAKEUP_STATUS.load(Ordering::Relaxed))
    }

    static TEST_OPS: PdevClocksAndPmicOps = PdevClocksAndPmicOps {
        prepare_for_suspend: Some(test_prepare_for_suspend),
        wakeup_from_suspend: Some(test_wakeup_from_suspend),
    };

    static EMPTY_OPS: PdevClocksAndPmicOps =
        PdevClocksAndPmicOps { prepare_for_suspend: None, wakeup_from_suspend: None };

    static PREPARE_ONLY_OPS: PdevClocksAndPmicOps = PdevClocksAndPmicOps {
        prepare_for_suspend: Some(test_prepare_for_suspend),
        wakeup_from_suspend: None,
    };

    static WAKEUP_ONLY_OPS: PdevClocksAndPmicOps = PdevClocksAndPmicOps {
        prepare_for_suspend: None,
        wakeup_from_suspend: Some(test_wakeup_from_suspend),
    };

    struct TestOpsGuard {
        previous: Option<&'static PdevClocksAndPmicOps>,
    }

    impl TestOpsGuard {
        fn new(ops: Option<&'static PdevClocksAndPmicOps>) -> Self {
            let previous = swap_ops_for_test(ops);
            Self { previous }
        }
    }

    impl Drop for TestOpsGuard {
        fn drop(&mut self) {
            swap_ops_for_test(self.previous);
        }
    }

    fn ops_ptr_eq(a: &'static PdevClocksAndPmicOps, b: &'static PdevClocksAndPmicOps) -> bool {
        core::ptr::eq(a, b)
    }

    /// Tests that the default operations table returns `Ok(())` for both prepare and wakeup.
    #[test]
    fn test_default_ops() {
        let _guard = TestOpsGuard::new(Some(&DEFAULT_OPS));

        assert_true!(ops_ptr_eq(get_ops(), &DEFAULT_OPS));
        assert_ok!(clocks_and_pmic_prepare_for_suspend());
        assert_ok!(clocks_and_pmic_wakeup_from_suspend());
        assert_ok!(rust_clocks_and_pmic_prepare_for_suspend());
        assert_ok!(rust_clocks_and_pmic_wakeup_from_suspend());
    }

    /// Tests that null ops fallback to default ops and missing callbacks return Status::NOT_SUPPORTED.
    #[test]
    fn test_null_ops_and_missing_callbacks() {
        {
            let _guard = TestOpsGuard::new(None);
            assert_true!(ops_ptr_eq(get_ops(), &DEFAULT_OPS));
            assert_ok!(clocks_and_pmic_prepare_for_suspend());
            assert_ok!(clocks_and_pmic_wakeup_from_suspend());
        }

        {
            let _guard = TestOpsGuard::new(Some(&EMPTY_OPS));
            assert_true!(ops_ptr_eq(get_ops(), &EMPTY_OPS));
            assert_err!(clocks_and_pmic_prepare_for_suspend(), Status::NOT_SUPPORTED);
            assert_err!(clocks_and_pmic_wakeup_from_suspend(), Status::NOT_SUPPORTED);
        }

        TEST_PREPARE_STATUS.store(0, Ordering::Relaxed);
        TEST_WAKEUP_STATUS.store(0, Ordering::Relaxed);

        {
            let _guard = TestOpsGuard::new(Some(&PREPARE_ONLY_OPS));
            assert_ok!(clocks_and_pmic_prepare_for_suspend());
            assert_err!(clocks_and_pmic_wakeup_from_suspend(), Status::NOT_SUPPORTED);
        }

        {
            let _guard = TestOpsGuard::new(Some(&WAKEUP_ONLY_OPS));
            assert_err!(clocks_and_pmic_prepare_for_suspend(), Status::NOT_SUPPORTED);
            assert_ok!(clocks_and_pmic_wakeup_from_suspend());
        }
    }

    /// Tests registering and dispatching a custom operations table.
    #[test]
    fn test_register_and_dispatch_ops() {
        let _guard = TestOpsGuard::new(Some(&DEFAULT_OPS));

        TEST_PREPARE_CALLED.store(0, Ordering::Relaxed);
        TEST_WAKEUP_CALLED.store(0, Ordering::Relaxed);
        TEST_PREPARE_STATUS.store(0, Ordering::Relaxed);
        TEST_WAKEUP_STATUS.store(0, Ordering::Relaxed);

        pdev_register_clocks_and_pmic(&TEST_OPS);
        assert_true!(ops_ptr_eq(get_ops(), &TEST_OPS));

        assert_ok!(clocks_and_pmic_prepare_for_suspend());
        assert_eq!(TEST_PREPARE_CALLED.load(Ordering::Relaxed), 1);

        assert_ok!(clocks_and_pmic_wakeup_from_suspend());
        assert_eq!(TEST_WAKEUP_CALLED.load(Ordering::Relaxed), 1);

        TEST_PREPARE_STATUS.store(Status::BAD_STATE.into_raw(), Ordering::Relaxed);
        TEST_WAKEUP_STATUS.store(Status::INTERNAL.into_raw(), Ordering::Relaxed);

        assert_err!(clocks_and_pmic_prepare_for_suspend(), Status::BAD_STATE);
        assert_eq!(TEST_PREPARE_CALLED.load(Ordering::Relaxed), 2);

        assert_err!(clocks_and_pmic_wakeup_from_suspend(), Status::INTERNAL);
        assert_eq!(TEST_WAKEUP_CALLED.load(Ordering::Relaxed), 2);
    }

    /// Tests that `TestOpsGuard` restores the previously registered ops table on drop.
    #[test]
    fn test_swap_restores_previous_ops() {
        let before = get_ops();
        {
            let _guard = TestOpsGuard::new(Some(&TEST_OPS));
            assert_true!(ops_ptr_eq(get_ops(), &TEST_OPS));
        }
        assert_true!(ops_ptr_eq(get_ops(), before));
    }

    /// Tests registering operations table via the C FFI function.
    #[test]
    fn test_rust_pdev_register_clocks_and_pmic() {
        let _guard = TestOpsGuard::new(Some(&DEFAULT_OPS));

        TEST_PREPARE_CALLED.store(0, Ordering::Relaxed);
        TEST_WAKEUP_CALLED.store(0, Ordering::Relaxed);
        TEST_PREPARE_STATUS.store(0, Ordering::Relaxed);
        TEST_WAKEUP_STATUS.store(0, Ordering::Relaxed);

        // Passing null should be accepted and map to DEFAULT_OPS.
        // SAFETY: A null pointer is valid and maps to DEFAULT_OPS.
        unsafe { rust_pdev_register_clocks_and_pmic(core::ptr::null()) };
        assert_true!(ops_ptr_eq(get_ops(), &DEFAULT_OPS));

        // Now registering custom ops via raw pointer.
        // SAFETY: &TEST_OPS is a valid static pointer.
        unsafe { rust_pdev_register_clocks_and_pmic(&TEST_OPS) };
        assert_true!(ops_ptr_eq(get_ops(), &TEST_OPS));

        assert_ok!(rust_clocks_and_pmic_prepare_for_suspend());
        assert_eq!(TEST_PREPARE_CALLED.load(Ordering::Relaxed), 1);

        assert_ok!(rust_clocks_and_pmic_wakeup_from_suspend());
        assert_eq!(TEST_WAKEUP_CALLED.load(Ordering::Relaxed), 1);
    }
}
