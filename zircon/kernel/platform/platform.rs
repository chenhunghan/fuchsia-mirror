// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod crashlog;
pub mod debug;
pub mod efi;
pub mod efi_bootbyte;
pub mod efi_crashlog;
mod efi_ffi;
#[cfg(ktest)]
mod efi_test;
pub mod halt_helper;
pub mod halt_token;
pub mod mapped_crashlog;
pub mod power;
pub mod timer;

#[cfg(target_arch = "aarch64")]
pub use crate::generic_arm::platform_is_efi_expected;
#[cfg(target_arch = "riscv64")]
pub use crate::generic_riscv64::platform_is_efi_expected;
#[cfg(target_arch = "x86_64")]
pub use crate::platform_pc::platform_is_efi_expected;
pub use efi_bootbyte::efi_bootbyte_set_reason;
pub use halt_token::HaltToken;

/// Super early platform initialization, before almost everything.
///
/// # Safety
///
/// Must be called once on the boot CPU during single-threaded early boot while the physboot
/// handoff remains valid and before any secondary CPU starts.
#[inline]
pub unsafe fn platform_early_init() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: Caller upholds `platform_early_init`'s single-threaded early-boot contract.
    unsafe {
        crate::generic_arm::platform_early_init();
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: Caller upholds `platform_early_init`'s single-threaded early-boot contract.
    unsafe {
        crate::generic_riscv64::platform::platform_early_init();
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: Caller upholds `platform_early_init`'s single-threaded early-boot contract.
    unsafe {
        crate::platform_pc::platform::platform_early_init();
    }
}

/// Performs any platform setup required before virtual memory is enabled or the heap is set up.
#[inline]
pub fn platform_prevm_init() {
    #[cfg(target_arch = "aarch64")]
    crate::generic_arm::platform_prevm_init();
    #[cfg(target_arch = "riscv64")]
    crate::generic_riscv64::platform::platform_prevm_init();
    #[cfg(target_arch = "x86_64")]
    crate::platform_pc::platform::platform_prevm_init();
}

/// Later platform initialization, after threading and the heap are up.
///
/// # Safety
///
/// Must be called once on the boot CPU from an initialized `Thread` (`bootstrap2`), after
/// [`platform_early_init`] and topology initialization have run, while the physboot handoff is
/// still valid, and before any secondary CPU starts.
#[inline]
pub unsafe fn platform_init() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: Caller upholds `platform_init`'s boot-CPU initialization contract.
    unsafe {
        crate::generic_arm::platform_init();
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: Caller upholds `platform_init`'s boot-CPU initialization contract.
    unsafe {
        crate::generic_riscv64::platform::platform_init();
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: Caller upholds `platform_init`'s boot-CPU initialization contract.
    unsafe {
        crate::platform_pc::platform::platform_init();
    }
}
