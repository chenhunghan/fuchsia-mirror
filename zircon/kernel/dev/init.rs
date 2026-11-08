// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel driver handoff initialization stages, mirroring `<dev/init.h>`.

use handoff::{ArchPhysHandoff, PhysHandoff, uart_all_Driver};

/// These routines initialize the kernel's drivers, as a function of the
/// configurations described in the physboot hand-off, at two stages:
/// * "early" refers to immediately after early platform initialization, before
///   the VM and heap are set up;
/// * "post vm" refers to immediately after the main virtual memory system is
///   initialized, so drivers can use the VM to map physical memory.
/// * "late" refers to immediately after the main platform initialization, once
///   the VM, the heap, threading, and general kernel facilities are available.
///
/// # Safety
///
/// Must be called once during early kernel boot with the valid physboot `handoff` reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DriverHandoffEarly(handoff: &PhysHandoff) {
    // SAFETY: `handoff` is the valid physboot handoff structure for the early handoff stage.
    unsafe {
        UartDriverHandoffEarly(&handoff.uart);
        PlatformDriverHandoffEarly(&handoff.arch_handoff);
    }
}

/// Initializes the kernel's drivers at the "post vm" stage (immediately after the main virtual
/// memory system is initialized, so drivers can use the VM to map physical memory), as a function
/// of the configurations described in the physboot hand-off.
///
/// # Safety
///
/// Must be called once during post-VM kernel boot with the valid physboot `handoff` reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DriverHandoffPostVm(handoff: &PhysHandoff) {
    // SAFETY: `handoff` is the valid physboot handoff structure for the post-VM handoff stage.
    unsafe {
        UartDriverHandoffPostVm(&handoff.uart);
        PlatformDriverHandoffPostVm(&handoff.arch_handoff);
    }
}

/// Initializes the kernel's drivers at the "late" stage (immediately after the main platform
/// initialization, once the VM, the heap, threading, and general kernel facilities are available),
/// as a function of the configurations described in the physboot hand-off.
///
/// # Safety
///
/// Must be called once during late kernel boot with the valid physboot `handoff` reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DriverHandoffLate(handoff: &PhysHandoff) {
    // SAFETY: `handoff` is the valid physboot handoff structure for the late handoff stage.
    unsafe {
        UartDriverHandoffLate(&handoff.uart);
        PlatformDriverHandoffLate(&handoff.arch_handoff);
    }
}

unsafe extern "C" {
    /// Early UART initialization routine.
    fn UartDriverHandoffEarly(serial: &uart_all_Driver);
    /// Post-VM UART initialization routine.
    fn UartDriverHandoffPostVm(serial: &uart_all_Driver);
    /// Late UART initialization routine.
    fn UartDriverHandoffLate(serial: &uart_all_Driver);

    /// Early platform-specific driver initialization routine.
    fn PlatformDriverHandoffEarly(arch_handoff: &ArchPhysHandoff);
    /// Post-VM platform-specific driver initialization routine.
    fn PlatformDriverHandoffPostVm(arch_handoff: &ArchPhysHandoff);
    /// Late platform-specific driver initialization routine.
    fn PlatformDriverHandoffLate(arch_handoff: &ArchPhysHandoff);
}
