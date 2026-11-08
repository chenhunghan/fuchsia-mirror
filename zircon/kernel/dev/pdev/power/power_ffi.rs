// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for the platform device (pdev) power driver interface.

use zx_status::Status;

use super::{
    PdevPowerOps, PowerCpuState, PowerRebootFlags, pdev_register_power, pdev_swap_power_for_test,
    power_cpu_off, power_cpu_on, power_get_cpu_state, power_reboot, power_shutdown,
    power_system_resume,
};

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

/// Reboots the system using the registered power operations.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_reboot(flags: PowerRebootFlags) {
    power_reboot(flags);
}

/// Shuts down the system using the registered power operations.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_shutdown() {
    power_shutdown();
}

/// Powers off the calling CPU.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_cpu_off() -> Result<(), Status> {
    power_cpu_off()
}

/// Restores platform power state after returning from system suspend.
#[unsafe(no_mangle)]
pub extern "C" fn rust_power_system_resume() -> Result<(), Status> {
    power_system_resume()
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

/// Retrieves the power state of the CPU with the specified hardware ID.
///
/// # Safety
///
/// `out_state` must be either null (which returns `Err(Status::INVALID_ARGS)`) or a valid,
/// well-aligned, writable pointer to a `PowerCpuState` for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_power_get_cpu_state(
    hw_cpu_id: u64,
    out_state: *mut PowerCpuState,
) -> Result<(), Status> {
    if out_state.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    let state = power_get_cpu_state(hw_cpu_id)?;
    // SAFETY: `out_state` was verified non-null above and caller guarantees it points to valid,
    // aligned, writable memory for a `PowerCpuState`.
    unsafe {
        *out_state = state;
    }
    Ok(())
}
