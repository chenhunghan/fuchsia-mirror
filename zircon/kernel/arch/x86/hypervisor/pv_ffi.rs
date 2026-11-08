// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_status::Status;
use zx_types::zx_vaddr_t;

use super::pv::{
    PvClockState, pv_clock_populate_offset, pv_clock_reset_clock, pv_clock_stop_clock,
    pv_clock_update_boot_time, pv_clock_update_system_time,
};
use crate::hypervisor::aspace::GuestPhysicalAspace;

/// FFI entry point for `pv_clock_update_boot_time`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pv_clock_update_boot_time(
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    pv_clock_update_boot_time(gpa, guest_paddr)
}

/// FFI entry point for `pv_clock_reset_clock`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pv_clock_reset_clock(
    pv_clock: &mut PvClockState,
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    pv_clock_reset_clock(pv_clock, gpa, guest_paddr)
}

/// FFI entry point for `pv_clock_stop_clock`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pv_clock_stop_clock(pv_clock: &mut PvClockState) {
    pv_clock_stop_clock(pv_clock)
}

/// FFI entry point for `pv_clock_update_system_time`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pv_clock_update_system_time(pv_clock: &mut PvClockState) {
    pv_clock_update_system_time(pv_clock)
}

/// FFI entry point for `pv_clock_populate_offset`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pv_clock_populate_offset(
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    pv_clock_populate_offset(gpa, guest_paddr)
}
