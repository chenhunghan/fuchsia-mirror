// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI declarations and C-ABI entry points for x86-64 thread management (`kernel/thread.h`,
//! `arch/x86/asm.S`, and `arch/src/api.rs`).
//!
//! All exported functions in this module use `extern "C"` ABI and accept type-erased `*mut c_void`
//! / `*const c_void` thread pointers to match the cross-architecture function pointer types in
//! [`crate::arch_rs::api`] (`assert_arch_signatures!`) and `kernel/thread.h`.

use core::ffi::c_void;

use super::{GeneralRegsSource, thread};
use crate::kernel::thread::Thread;

unsafe extern "C" {
    /// Performs the low-level x86-64 callee-saved register and stack pointer switch
    /// (`x86_64_context_switch` in `asm.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - CPL is 0, `%gs.base` is initialized to the calling CPU's `x86_percpu`, and interrupts are
    ///   disabled on the current CPU.
    /// - `oldsp` (and `old_unsafe_sp` when safe-stack is enabled) points to writable `usize`
    ///   storage in the outgoing thread's `ArchThread` (disjoint from `new_thread`).
    /// - `newsp` (and `new_unsafe_sp` when safe-stack is enabled) is a valid kernel stack pointer
    ///   pointing to an `X86ContextSwitchFrame` on `new_thread`'s inactive kernel stack.
    /// - `new_thread` points to a live (not freed), valid `Thread` that remains live and valid for
    ///   as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
    #[cfg(sanitize = "safestack")]
    pub fn x86_64_context_switch(
        oldsp: *mut usize,
        newsp: usize,
        new_thread: *mut Thread,
        old_unsafe_sp: *mut usize,
        new_unsafe_sp: usize,
    );

    /// Performs the low-level x86-64 callee-saved register and stack pointer switch
    /// (`x86_64_context_switch` in `asm.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - CPL is 0, `%gs.base` is initialized to the calling CPU's `x86_percpu`, and interrupts are
    ///   disabled on the current CPU.
    /// - `oldsp` points to writable `usize` storage in the outgoing thread's `ArchThread` (disjoint
    ///   from `new_thread`).
    /// - `newsp` is a valid kernel stack pointer pointing to an `X86ContextSwitchFrame` on
    ///   `new_thread`'s inactive kernel stack.
    /// - `new_thread` points to a live (not freed), valid `Thread` that remains live and valid for
    ///   as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
    #[cfg(not(sanitize = "safestack"))]
    pub fn x86_64_context_switch(oldsp: *mut usize, newsp: usize, new_thread: *mut Thread);
}

/// Initializes the architectural state of `t` (`arch_thread_initialize`).
///
/// # Safety
///
/// The caller must ensure that:
/// - [`super::registers::extended_register_init`] has completed on CPU 0.
/// - `t` points to a live (not freed), valid `Thread` that is not currently running, and the caller
///   holds exclusive access to `t` (or `t`'s lock).
/// - `t` has an allocated kernel stack whose top (`get_stack_top(t)`) is 16-byte aligned with at
///   least `size_of::<u64>() + size_of::<X86ContextSwitchFrame>()` (`64`) writable bytes below
///   `stack_top` (and a 16-byte-aligned `unsafe_sp` when safe-stack is enabled).
// SAFETY: Unique symbol name and C ABI signature matches `crate::arch_rs::api`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_thread_initialize(t: *mut c_void, entry_point: usize) {
    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0 and `t` is a valid,
    // non-running `Thread` with exclusive access and an aligned, writable kernel stack.
    unsafe { thread::thread_initialize(t.cast::<Thread>(), entry_point) };
}

/// Establishes `t` as the current thread on the calling CPU (`arch_thread_construct_first`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to the calling CPU's `x86_percpu`,
/// interrupts are disabled on the calling CPU, and `t` points to a live (not freed), valid `Thread`
/// dedicated to the calling CPU (not concurrently running on another CPU) that remains live and
/// valid for as long as it remains installed in `%gs:[PERCPU_CURRENT_THREAD_OFFSET]`.
// SAFETY: Unique symbol name and C ABI signature matches `kernel/thread.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_thread_construct_first(t: *mut c_void) {
    // SAFETY: Caller guarantees `%gs.base` is initialized, interrupts are disabled, and `t` is a
    // valid `Thread` dedicated to this CPU that remains live while installed as current thread.
    unsafe { thread::thread_construct_first(t.cast::<Thread>()) };
}

/// Saves `thread`'s user extended register and segment selector state (`arch_save_user_state`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.EM == 0`, `CR0.TS == 0`,
/// [`super::registers::extended_register_init`] has completed on CPU 0, `CR4.OSXSAVE == 1` when
/// XSAVE is enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is
/// initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in `IA32_KERNEL_GS_BASE`),
/// interrupts are disabled, and `thread` points to the current live (not freed) `Thread` with
/// exclusive access to its saved user state.
// SAFETY: Unique symbol name and C ABI signature matches `crate::arch_rs::api`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_save_user_state(thread: *mut c_void) {
    // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `extended_register_init` has
    // completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is enabled, `CR4.FSGSBASE == 1` when
    // `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized, interrupts are disabled, and
    // `thread` is the current thread with exclusive access to its saved user state.
    unsafe { thread::save_user_state(thread.cast::<Thread>()) };
}

/// Restores `thread`'s user segment selector, debug, and extended register state
/// (`arch_restore_user_state`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
/// [`super::registers::extended_register_init`] has completed on CPU 0, `CR4.OSXSAVE == 1` when
/// XSAVE is enabled, `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is
/// initialized to the calling CPU's `x86_percpu` (with user `%gs.base` in `IA32_KERNEL_GS_BASE`),
/// interrupts are disabled, and `thread` points to the current live (not freed) `Thread` with
/// shared access to its valid saved user state (`fs_base` and `gs_base` are canonical addresses,
/// `debug_state` was validated by `validate_debug_state` if `track_debug_state` is true, and
/// `extended_register_buffer` contains a valid extended register state image created by
/// [`super::registers::extended_register_init_state`],
/// [`super::registers::extended_register_init_state_from_bv`], or
/// [`super::registers::extended_register_save_state`]).
// SAFETY: Unique symbol name and C ABI signature matches `crate::arch_rs::api`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_restore_user_state(thread: *mut c_void) {
    // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `DR7.GD == 0`,
    // `extended_register_init` has completed on CPU 0, `CR4.OSXSAVE == 1` when XSAVE is enabled,
    // `CR4.FSGSBASE == 1` when `X86_FEATURE_FSGSBASE` is reported, `%gs.base` is initialized,
    // interrupts are disabled, and `thread` is the current thread with valid saved user state
    // (including a 64-byte-aligned `extended_register_buffer` containing a valid extended register
    // state image).
    unsafe { thread::restore_user_state(thread.cast::<Thread>().cast_const()) };
}

/// Records `gregs` with `source` as the suspended general registers pointer for `thread`
/// (`arch_set_suspended_general_regs`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` with exclusive
/// access to its `suspended_general_regs` and `general_regs_source` fields (e.g., holding
/// `thread`'s lock), and `gregs` is a non-null pointer to a live `Iframe` or `SyscallRegs`
/// matching `source` (`GeneralRegsSource::Iframe` or `GeneralRegsSource::Syscall`) that remains
/// valid and live until cleared by [`arch_reset_suspended_general_regs`].
// SAFETY: Unique symbol name and C ABI signature matches `crate::arch_rs::api`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_suspended_general_regs(
    thread: *mut c_void,
    source: GeneralRegsSource,
    gregs: *mut c_void,
) {
    // SAFETY: Caller guarantees `thread` is valid with exclusive access to its suspended general
    // register fields and `gregs` is valid and live until reset.
    unsafe { thread::set_suspended_general_regs(thread.cast::<Thread>(), source, gregs) };
}

/// Clears `thread`'s suspended general registers pointer (`arch_reset_suspended_general_regs`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` with exclusive
/// access to its `suspended_general_regs` and `general_regs_source` fields (e.g., holding
/// `thread`'s lock).
// SAFETY: Unique symbol name and C ABI signature matches `crate::arch_rs::api`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_reset_suspended_general_regs(thread: *mut c_void) {
    // SAFETY: Caller guarantees `thread` is valid with exclusive access to its suspended general
    // register fields.
    unsafe { thread::reset_suspended_general_regs(thread.cast::<Thread>()) };
}
