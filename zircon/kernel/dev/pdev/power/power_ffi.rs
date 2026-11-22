// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for the platform device (pdev) power driver interface.

use zx_status::Status;

use super::{PdevPowerOps, pdev_register_power, pdev_swap_power_for_test, power_cpu_on};

/// Registers the platform power operations table from C-ABI.
///
/// # Safety
///
/// If non-null, `ops` must point to a valid `PdevPowerOps` table that remains valid
/// for the duration of the kernel's execution (`'static` lifetime).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pdev_register_power(ops: *const PdevPowerOps) {
    // SAFETY: Caller guarantees that if `ops` is non-null, it points to a valid `PdevPowerOps`
    // table with `'static` lifetime.
    let ops_ref = unsafe { ops.as_ref() };
    if let Some(ops_ref) = ops_ref {
        pdev_register_power(ops_ref);
    } else {
        // SAFETY: Passing null resets `POWER_OPS` to `DEFAULT_OPS`.
        unsafe {
            pdev_swap_power_for_test(core::ptr::null());
        }
    }
}

/// Powers on the CPU with the specified hardware ID.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_cpu_on(
    hw_cpu_id: u64,
    entry: u64,
    context: u64,
) -> Result<(), Status> {
    power_cpu_on(hw_cpu_id, entry, context)
}
