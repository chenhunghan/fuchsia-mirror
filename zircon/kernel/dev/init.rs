// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel driver handoff initialization stages.

#[cfg(target_arch = "aarch64")]
use handoff::ArchPhysHandoff;
use handoff::PhysHandoff;

use crate::platform_rs::debug::{
    uart_driver_handoff_early, uart_driver_handoff_late, uart_driver_handoff_post_vm,
};

/// Initializes the kernel's drivers at the early stage (immediately after early platform
/// initialization, before the VM and heap are set up), as a function of the configurations
/// described in the physboot handoff.
///
/// # Safety
///
/// Must be called once during early kernel boot with the valid physboot `handoff` reference.
pub unsafe fn driver_handoff_early(handoff: &PhysHandoff) {
    // SAFETY: `handoff` is the valid physboot handoff structure for the early handoff stage.
    unsafe {
        uart_driver_handoff_early(&handoff.uart);
        #[cfg(target_arch = "aarch64")]
        PlatformDriverHandoffEarly(&handoff.arch_handoff);
        #[cfg(target_arch = "riscv64")]
        crate::generic_riscv64::dev_init::platform_driver_handoff_early(&handoff.arch_handoff);
    }
}

/// Initializes the kernel's drivers at the post-VM stage (immediately after the main virtual
/// memory system is initialized, so drivers can use the VM to map physical memory), as a function
/// of the configurations described in the physboot handoff.
///
/// # Safety
///
/// Must be called once during post-VM kernel boot with the valid physboot `handoff` reference.
pub unsafe fn driver_handoff_post_vm(handoff: &PhysHandoff) {
    uart_driver_handoff_post_vm(&handoff.uart);
    // SAFETY: `handoff` is the valid physboot handoff structure for the post-VM handoff stage,
    // and the caller upholds the one-time post-VM boot contract.
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    unsafe {
        #[cfg(target_arch = "aarch64")]
        PlatformDriverHandoffPostVm(&handoff.arch_handoff);
        #[cfg(target_arch = "riscv64")]
        crate::generic_riscv64::dev_init::platform_driver_handoff_post_vm(&handoff.arch_handoff);
    }
}

/// Initializes the kernel's drivers at the late stage (immediately after main platform
/// initialization, once the VM, heap, threading, and general kernel facilities are available), as
/// a function of the configurations described in the physboot handoff.
///
/// # Safety
///
/// Must be called once during late kernel boot with the valid physboot `handoff` reference.
pub unsafe fn driver_handoff_late(handoff: &PhysHandoff) {
    // SAFETY: `handoff` is the valid physboot handoff structure for the late handoff stage,
    // and the caller upholds the one-time late boot contract.
    unsafe {
        uart_driver_handoff_late(&handoff.uart);
        #[cfg(target_arch = "aarch64")]
        PlatformDriverHandoffLate(&handoff.arch_handoff);
        #[cfg(target_arch = "riscv64")]
        crate::generic_riscv64::dev_init::platform_driver_handoff_late(&handoff.arch_handoff);
    }
}

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    /// Early platform-specific driver initialization routine.
    fn PlatformDriverHandoffEarly(arch_handoff: &ArchPhysHandoff);
    /// Post-VM platform-specific driver initialization routine.
    fn PlatformDriverHandoffPostVm(arch_handoff: &ArchPhysHandoff);
    /// Late platform-specific driver initialization routine.
    fn PlatformDriverHandoffLate(arch_handoff: &ArchPhysHandoff);
}
