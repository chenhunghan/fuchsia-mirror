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
    /// `x86_cpu_feature_init`.
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

    /// Initializes x86 CPU feature state on the boot CPU (`x86_cpu_feature_init` in `feature.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called once on CPU 0 during pre-VM boot.
    pub fn x86_cpu_feature_init();

    /// Performs late per-CPU x86 feature and mitigation setup (`x86_cpu_feature_late_init_percpu`
    /// in `feature.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called after platform and topology
    /// initialization.
    pub fn x86_cpu_feature_late_init_percpu();

    /// Prints x86 CPU feature and vulnerability information (`x86_feature_debug` in `feature.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that early boot CPUID initialization has completed.
    pub fn x86_feature_debug();

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

    /// Kernel secondary CPU entry point (`lk_secondary_cpu_entry` in `top/main.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that early secondary CPU initialization has completed on the calling
    /// CPU.
    pub fn lk_secondary_cpu_entry();
}

#[cfg(console_enabled)]
pub mod console {
    use super::super::Iframe;
    use super::super::arch::console::CpuContext;
    use crate::kernel::types::cpu_num_t;
    use zx_types::{zx_duration_mono_t, zx_status_t};

    unsafe extern "C" {
        /// Requests the execution context of `target_cpu`, waiting up to `timeout`.
        ///
        /// # Safety
        ///
        /// The caller must ensure that interrupts are disabled on the calling CPU, `target_cpu` is
        /// a valid CPU number, and `context` points to a valid, mutable `CpuContext`.
        #[link_name = "cpp_x86_request_context"]
        pub fn x86_request_context(
            target_cpu: cpu_num_t,
            timeout: zx_duration_mono_t,
            context: *mut CpuContext,
        ) -> zx_status_t;

        /// Prints `frame` to the kernel console.
        ///
        /// # Safety
        ///
        /// The caller must ensure that `frame` points to a valid `Iframe`.
        #[link_name = "cpp_x86_print_frame"]
        pub fn x86_print_frame(frame: *const Iframe);
    }
}

/// FFI entry point for [`super::arch::early_init`].
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during early boot before the
/// VM and heap are initialized.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_early_init() {
    // SAFETY: Caller guarantees single-CPU early boot context on CPU 0.
    unsafe { super::arch::early_init() };
}

/// FFI entry point for [`super::arch::prevm_init`].
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 during pre-VM boot.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_prevm_init() {
    // SAFETY: Caller guarantees single-CPU pre-VM boot context on CPU 0.
    unsafe { super::arch::prevm_init() };
}

/// FFI entry point for [`super::arch::init`].
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 while only CPU 0 is online
/// and after early feature, kernel address space, and VM initialization have completed.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_init() {
    // SAFETY: Caller guarantees single-CPU `arch_init` context on CPU 0.
    unsafe { super::arch::init() };
}

/// FFI entry point for [`super::arch::late_init_percpu`].
///
/// # Safety
///
/// The caller must ensure that this function is called on CPU 0 during late arch initialization
/// after topology and platform initialization have completed.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_late_init_percpu() {
    // SAFETY: Caller guarantees late arch initialization context on CPU 0.
    unsafe { super::arch::late_init_percpu() };
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
