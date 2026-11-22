// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

use zx_status::Status;
use zx_types::zx_status_t;

use super::mp::{self, CpuidIo};
use crate::kernel::mp::{MpIpi, MpIpiTarget};
use crate::kernel::types::{cpu_mask_t, cpu_num_t};

unsafe extern "C" {
    #[link_name = "gBootCpuid0"]
    pub static G_BOOT_CPUID_0: CpuidIo;

    #[link_name = "gBootCpuidFeature"]
    pub static G_BOOT_CPUID_FEATURE: CpuidIo;

    /// Assembly SYSCALL entry point (`x86_syscall` in `syscall.S`).
    ///
    /// # Safety
    ///
    /// This is an assembly syscall entry point label and must never be called directly from Rust.
    #[link_name = "x86_syscall"]
    pub fn x86_syscall();

    /// Enables interrupts and executes `HLT` (`x86_enable_ints_and_hlt` in `ops.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that CPL is 0 and interrupts are disabled on entry.
    pub fn x86_enable_ints_and_hlt();
}

/// Reschedules tasks on the CPUs specified by `mask` (`arch_mp_reschedule`).
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_reschedule(mask: cpu_mask_t) {
    mp::mp_reschedule(mask);
}

/// Sends an inter-processor interrupt `ipi` to `target` CPUs (`arch_mp_send_ipi`).
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_send_ipi(target: MpIpiTarget, mask: cpu_mask_t, ipi: MpIpi) {
    mp::mp_send_ipi(target, mask, ipi);
}

/// Hotplugs secondary processor `cpu_id` (`arch_mp_cpu_hotplug`).
///
/// # Safety
///
/// The caller must ensure that SMP has been initialized via `x86_init_smp`.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_mp_cpu_hotplug(cpu_id: cpu_num_t) -> zx_status_t {
    // SAFETY: Caller guarantees SMP is initialized.
    Status::result_into_raw(unsafe { mp::mp_cpu_hotplug(cpu_id) })
}

/// Associates the high-level `percpu` pointer with `cpu_num`'s [`mp::X86Percpu`]
/// (`arch_setup_percpu`).
///
/// # Safety
///
/// The caller must ensure that `cpu_num < x86_num_cpus` and `percpu` points to a valid, live
/// `percpu` instance for the lifetime of the CPU.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_setup_percpu(cpu_num: cpu_num_t, percpu: *mut c_void) {
    // SAFETY: Caller guarantees `cpu_num` and `percpu` are valid and live.
    unsafe { mp::setup_percpu(cpu_num, percpu) };
}
