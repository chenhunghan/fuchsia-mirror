// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

use super::registers;

/// Returns whether or not `xsave` is supported by the CPU (`x86_xsave_supported`).
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub extern "C" fn x86_xsave_supported() -> bool {
    registers::xsave_supported()
}

/// Initialize a state vector to a specific set of state bits. The passed in buffer must be
/// `X86_MAX_EXTENDED_REGISTER_SIZE` big and it must be 64-byte aligned. This function will
/// initialize it for use in save and restore.
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0, and
/// that `register_state` points to a 64-byte aligned writable buffer of at least
/// [`registers::X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) bytes. If `xstate_bv` sets any component
/// bits `>= 2`, the caller must ensure the corresponding extended area bytes in `register_state`
/// are already initialized/zeroed.
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_extended_register_init_state_from_bv(
    register_state: *mut c_void,
    xstate_bv: u64,
) {
    // SAFETY: Caller guarantees `extended_register_init()` has completed and `register_state` is a
    // valid 64-byte aligned buffer.
    unsafe {
        registers::extended_register_init_state_from_bv(register_state.cast::<u8>(), xstate_bv)
    };
}

/// Saves current extended register state to `register_state` (`x86_extended_register_save_state`).
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0, CPL
/// is 0, `CR0.EM == 0`, `CR0.TS == 0`, `CR4.OSXSAVE == 1` when XSAVE is enabled, and
/// `register_state` is either null or points to a 64-byte aligned writable buffer of at least
/// [`registers::X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) bytes.
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_extended_register_save_state(register_state: *mut c_void) {
    // SAFETY: Caller guarantees `extended_register_init()` has completed, CPL 0, `CR0.EM == 0`,
    // `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is null or a valid 64-byte aligned
    // buffer.
    unsafe { registers::extended_register_save_state(register_state.cast::<u8>()) };
}

/// Restores extended register state from `register_state` (`x86_extended_register_restore_state`).
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0, CPL
/// is 0, `CR0.EM == 0`, `CR0.TS == 0`, `CR4.OSXSAVE == 1` when XSAVE is enabled, and
/// `register_state` is either null or points to a 64-byte aligned readable buffer containing a
/// valid extended register state image.
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_extended_register_restore_state(register_state: *const c_void) {
    // SAFETY: Caller guarantees `extended_register_init()` has completed, CPL 0, `CR0.EM == 0`,
    // `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is null or a valid 64-byte aligned
    // buffer.
    unsafe { registers::extended_register_restore_state(register_state.cast::<u8>()) };
}

/// Reads extended control register `reg` (`x86_xgetbv`).
///
/// # Safety
///
/// The caller must ensure that `CR4.OSXSAVE` is enabled and `reg` is a valid XCR index.
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_xgetbv(reg: u32) -> u64 {
    // SAFETY: Caller guarantees `CR4.OSXSAVE` is enabled and `reg` is valid.
    unsafe { registers::xgetbv(reg) }
}

/// Writes `val` to extended control register `reg` (`x86_xsetbv`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.OSXSAVE` is enabled, and `val` is valid for `reg`.
// SAFETY: Unique symbol name and C ABI signature matches registers.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_xsetbv(reg: u32, val: u64) {
    // SAFETY: Caller guarantees CPL 0, `CR4.OSXSAVE` is enabled, and `val` is valid.
    unsafe { registers::xsetbv(reg, val) };
}
