// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::sync::atomic::AtomicU32;

use super::{Iframe, UserEntryState};
use crate::kernel::thread::Thread;

unsafe extern "C" {
    /// Initializes early x86 MMU state on the boot CPU (`x86_mmu_early_init` in `mmu.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called once on CPU 0 during early boot.
    pub fn x86_mmu_early_init();

    /// Initializes pre-VM x86 MMU state on the boot CPU (`x86_mmu_prevm_init` in `mmu.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called once on CPU 0 after
    /// [`super::feature::cpu_feature_init`].
    pub fn x86_mmu_prevm_init();

    /// Initializes main x86 MMU state on the boot CPU (`x86_mmu_init` in `mmu.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called once on CPU 0 during `arch_init`.
    pub fn x86_mmu_init();

    /// Initializes per-CPU x86 MMU state (`x86_mmu_percpu_init` in `mmu.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called at CPL 0 during per-CPU bootstrap.
    pub fn x86_mmu_percpu_init();

    /// Enters user space by restoring `iframe` and executing `swapgs; iretq`
    /// (`x86_uspace_entry` in `uspace_entry.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that interrupts are disabled, `%gs.base` and the current CPU's TSS
    /// `rsp0` are initialized, and `iframe` points to a valid user-space [`Iframe`].
    #[cfg(sanitize = "safestack")]
    pub fn x86_uspace_entry(iframe: *const Iframe, unsafe_sp: usize) -> !;

    /// Enters user space by restoring `iframe` and executing `swapgs; iretq`
    /// (`x86_uspace_entry` in `uspace_entry.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that interrupts are disabled, `%gs.base` and the current CPU's TSS
    /// `rsp0` are initialized, and `iframe` points to a valid user-space [`Iframe`].
    #[cfg(not(sanitize = "safestack"))]
    pub fn x86_uspace_entry(iframe: *const Iframe) -> !;
}

/// FFI entry point for [`super::arch::prepare_uspace`].
///
/// # Safety
///
/// The caller must ensure that `out` points to valid, writable memory for an [`Iframe`] and this
/// function is called at CPL 0 on the thread about to enter user space, and `state.tp` is a
/// canonical virtual address.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_prepare_uspace(state: &UserEntryState, out: *mut Iframe) {
    // SAFETY: Caller guarantees `out` is a valid pointer, CPL 0 context holds, and
    // `state.tp` is canonical.
    unsafe { super::arch::prepare_uspace(state, out) };
}

/// FFI entry point for [`super::arch::enter_uspace`].
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled, `%gs.base` and the current CPU's TSS
/// `rsp0` are initialized for the current thread, and `iframe` points to a valid user-space
/// [`Iframe`] with a canonical user `ip`, `cs == USER_CODE_64_SELECTOR`,
/// `user_ss == USER_DATA_SELECTOR`,
/// `flags & X86_FLAGS_IOPL_MASK == 0`, and `flags & X86_FLAGS_IF != 0`.
/// Under `#[cfg(sanitize = "safestack")]`, the caller must also ensure that `%gs.base`'s
/// `current_thread` slot points to a valid running `Thread` with an initialized `KernelStack`.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_enter_uspace(iframe: *const Iframe) -> ! {
    // SAFETY: Caller guarantees `iframe` is valid and user-entry preconditions hold.
    unsafe { super::arch::enter_uspace(iframe) }
}

/// FFI entry point for [`super::arch::secondary_entry`], called from `start16.S`.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 on a newly started Application
/// Processor with `aps_still_booting` pointing to a valid `AtomicU32` bitmask and `thread`
/// pointing to this AP's live (not freed) bootstrap `Thread`.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
#[sanitize(safestack = "off")]
pub unsafe extern "C" fn x86_secondary_entry(
    aps_still_booting: *const AtomicU32,
    thread: *mut Thread,
) -> ! {
    // SAFETY: Caller guarantees AP bootstrap entry preconditions from `start16.S`.
    unsafe { super::arch::secondary_entry(aps_still_booting, thread) }
}
