// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Dispatch of the physboot driver handoff to the platform's drivers.

use crate::arch_rs::riscv64::timer::{DcfgRiscvGenericTimerDriver, riscv_generic_timer_init_early};
use crate::plic::{DcfgRiscvPlicDriver, plic_init_early, plic_init_late, plic_init_post_vm};
use handoff::{
    ArchPhysHandoff, zbi_dcfg_riscv_generic_timer_driver_t, zbi_dcfg_riscv_plic_driver_t,
};

fn to_plic_config(cfg: zbi_dcfg_riscv_plic_driver_t) -> DcfgRiscvPlicDriver {
    DcfgRiscvPlicDriver {
        mmio_phys: cfg.mmio_phys,
        size_bytes: cfg.size_bytes,
        num_irqs: cfg.num_irqs,
    }
}

fn to_timer_config(cfg: zbi_dcfg_riscv_generic_timer_driver_t) -> DcfgRiscvGenericTimerDriver {
    DcfgRiscvGenericTimerDriver { freq_hz: cfg.freq_hz, reserved: cfg.reserved }
}

/// Early driver initialization from the handoff: the PLIC and the generic timer, each only if
/// physboot handed off a driver configuration for it.
///
/// # Safety
///
/// Must be called once, on the boot CPU with interrupts disabled, before secondary CPUs are
/// started, with the valid physboot `arch_handoff`.
pub unsafe fn platform_driver_handoff_early(arch_handoff: &ArchPhysHandoff) {
    if let Some(plic) = Option::from(arch_handoff.plic_driver).map(to_plic_config) {
        plic_init_early(&plic);
    }
    if let Some(timer) = Option::from(arch_handoff.generic_timer_driver).map(to_timer_config) {
        // SAFETY: The caller upholds the one-time, boot-CPU contract above.
        unsafe { riscv_generic_timer_init_early(&timer) };
    }
}

/// Post-VM driver initialization from the handoff: the PLIC, if configured.
///
/// # Safety
///
/// Must be called once during post-VM boot, on the boot CPU before secondary CPUs are started,
/// with the valid physboot `arch_handoff`.
pub unsafe fn platform_driver_handoff_post_vm(arch_handoff: &ArchPhysHandoff) {
    if let Some(plic) = Option::from(arch_handoff.plic_driver).map(to_plic_config) {
        plic_init_post_vm(&plic);
    }
}

/// Late driver initialization from the handoff: the PLIC, if configured.
///
/// # Safety
///
/// Must be called once during late boot, after post-VM handoff and resource-filter
/// initialization, with the valid physboot `arch_handoff`.
pub unsafe fn platform_driver_handoff_late(arch_handoff: &ArchPhysHandoff) {
    if let Some(plic) = Option::from(arch_handoff.plic_driver).map(to_plic_config) {
        plic_init_late(&plic);
    }
}
