// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Cross-architecture debugger register state accessors.

// TODO(https://fxbug.dev/540178114): Drop `#[unsafe(no_mangle)]` and `extern "C"` on the
// `arm64::debugger` and `riscv64::debugger` register accessors in a follow-up CL now that all
// callers invoke them directly from Rust.

#[cfg(target_arch = "x86_64")]
use core::mem::MaybeUninit;

use crate::kernel::thread::Thread;
use zx_status::Status;

/// Reads the saved user-mode general registers for `thread` into `out`.
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call with its saved user register state (on x86, any non-null `suspended_general_regs`
/// frame) valid, initialized, and not mutated during the call, no chain lock transaction may be
/// active on the calling CPU, and `out` must point to aligned, writable, non-overlapping storage
/// for `zx_thread_state_general_regs_t` that is uniquely accessed during the call.
#[inline(always)]
pub unsafe fn arch_get_general_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_general_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_general_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions (`out`
        // is aligned, writable, uniquely accessed, and non-overlapping), and `get_general_regs`
        // never writes uninitialized bytes into `out`.
        unsafe {
            super::x86::debugger::get_general_regs(thread, &mut *out.cast::<MaybeUninit<_>>())
        }
    }
}

/// Writes `in_` into the saved user-mode general registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call with its saved user register state (on x86, any non-null `suspended_general_regs`
/// frame) valid, initialized, writable, and not otherwise accessed during the call, no chain lock
/// transaction may be active on the calling CPU, and `in_` must point to aligned, initialized,
/// non-overlapping storage for `zx_thread_state_general_regs_t` that is not mutated during the
/// call.
#[inline(always)]
pub unsafe fn arch_set_general_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_general_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_general_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::x86::debugger::set_general_regs(thread, &*in_.cast()) }
    }
}

/// Reads the saved floating-point registers for `thread` into `out`.
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// `thread` must point to a live, initialized `Thread` (created after early CPU 0 extended-register
/// initialization on x86-64) that stays suspended or in an exception for the duration of the call,
/// no chain lock transaction may be active on the calling CPU, and `out` must point to aligned,
/// writable, non-overlapping storage for `zx_thread_state_fp_regs_t` that is uniquely accessed
/// during the call.
#[inline(always)]
pub unsafe fn arch_get_fp_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::debugger::arch_get_fp_regs(thread, out.cast())
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_fp_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all early-init, thread-state, lock, and buffer validity
        // preconditions (`out` is aligned, writable, uniquely accessed, and non-overlapping), and
        // `get_fp_regs` never writes uninitialized bytes into `out`.
        unsafe { super::x86::debugger::get_fp_regs(thread, &mut *out.cast::<MaybeUninit<_>>()) }
    }
}

/// Writes `in_` into the saved floating-point registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live, initialized `Thread` (created after early CPU 0 extended-register
/// initialization on x86-64) that stays suspended or in an exception for the duration of the call,
/// no chain lock transaction may be active on the calling CPU, and `in_` must point to aligned,
/// initialized, non-overlapping storage for `zx_thread_state_fp_regs_t` that is not mutated during
/// the call.
#[inline(always)]
pub unsafe fn arch_set_fp_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        super::arm64::debugger::arch_set_fp_regs(thread, in_.cast())
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_fp_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all early-init, thread-state, lock, and buffer validity
        // preconditions.
        unsafe { super::x86::debugger::set_fp_regs(thread, &*in_.cast()) }
    }
}

/// Reads the saved vector registers for `thread` into `out`.
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// `thread` must point to a live, initialized `Thread` (created after early CPU 0 extended-register
/// initialization on x86-64) that stays suspended or in an exception for the duration of the call,
/// no chain lock transaction may be active on the calling CPU, and `out` must point to aligned,
/// writable, non-overlapping storage for `zx_thread_state_vector_regs_t` that is uniquely accessed
/// during the call.
#[inline(always)]
pub unsafe fn arch_get_vector_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_vector_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_vector_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all early-init, thread-state, lock, and buffer validity
        // preconditions (`out` is aligned, writable, uniquely accessed, and non-overlapping), and
        // `get_vector_regs` never writes uninitialized bytes into `out`.
        unsafe { super::x86::debugger::get_vector_regs(thread, &mut *out.cast::<MaybeUninit<_>>()) }
    }
}

/// Writes `in_` into the saved vector registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live, initialized `Thread` (created after early CPU 0 extended-register
/// initialization on x86-64) that stays suspended or in an exception for the duration of the call,
/// no chain lock transaction may be active on the calling CPU, and `in_` must point to aligned,
/// initialized, non-overlapping storage for `zx_thread_state_vector_regs_t` that is not mutated
/// during the call.
#[inline(always)]
pub unsafe fn arch_set_vector_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_vector_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_vector_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all early-init, thread-state, lock, and buffer validity
        // preconditions.
        unsafe { super::x86::debugger::set_vector_regs(thread, &*in_.cast()) }
    }
}

/// Reads the debug registers for `thread` into `out`.
///
/// `out` is never overwritten with uninitialized bytes (and is fully initialized when `Ok(())` is
/// returned on `x86_64` and `aarch64`).
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_thread_state_debug_regs_t` that is
/// uniquely accessed during the call.
#[inline(always)]
pub unsafe fn arch_get_debug_regs(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_debug_regs(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_get_debug_regs(thread.cast(), out.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions (`out`
        // is aligned, writable, uniquely accessed, and non-overlapping), and `get_debug_regs` never
        // writes uninitialized bytes into `out`.
        unsafe { super::x86::debugger::get_debug_regs(thread, &mut *out.cast::<MaybeUninit<_>>()) }
    }
}

/// Writes `in_` into the debug registers for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU (with CPL 0 and
/// `DR7.GD == 0` on x86-64), and `in_` must point to aligned, initialized, non-overlapping storage
/// for `zx_thread_state_debug_regs_t` that is not mutated during the call.
#[inline(always)]
pub unsafe fn arch_set_debug_regs(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_debug_regs(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::riscv64::debugger::arch_set_debug_regs(thread.cast(), in_.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds CPL 0, `DR7.GD == 0`, thread-state, lock, and buffer validity
        // preconditions.
        unsafe { super::x86::debugger::set_debug_regs(thread, &*in_.cast()) }
    }
}

/// Reads the single-step state for `thread` into `out`.
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call with its saved user register state (on x86, any non-null `suspended_general_regs`
/// frame) valid, initialized, and not mutated during the call, no chain lock transaction may be
/// active on the calling CPU, and `out` must point to aligned, writable, non-overlapping storage
/// for `zx_thread_state_single_step_t` that is uniquely accessed during the call.
#[inline(always)]
pub unsafe fn arch_get_single_step(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_get_single_step(thread, out.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(
            unsafe { super::riscv64::debugger::arch_get_single_step(thread.cast(), out.cast()) }
                .into_raw(),
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions (`out`
        // is aligned, writable, uniquely accessed, and non-overlapping), and `get_single_step`
        // never writes uninitialized bytes into `out`.
        unsafe { super::x86::debugger::get_single_step(thread, &mut *out.cast::<MaybeUninit<_>>()) }
    }
}

/// Writes `in_` into the single-step state for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call with its saved user register state (on x86, any non-null `suspended_general_regs`
/// frame) valid, initialized, writable, and not otherwise accessed during the call, no chain lock
/// transaction may be active on the calling CPU, and `in_` must point to aligned, initialized,
/// non-overlapping storage for `zx_thread_state_single_step_t` that is not mutated during the call.
#[inline(always)]
pub unsafe fn arch_set_single_step(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::arm64::debugger::arch_set_single_step(thread, in_.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        Status::ok(
            unsafe { super::riscv64::debugger::arch_set_single_step(thread.cast(), in_.cast()) }
                .into_raw(),
        )
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
        unsafe { super::x86::debugger::set_single_step(thread, &*in_.cast()) }
    }
}

/// Returns the instruction pointer from `gregs` according to `source`
/// (`arch_get_instruction_pointer`).
///
/// # Safety
///
/// `source` must be a valid non-[`None`](super::GeneralRegsSource::None)
/// [`super::GeneralRegsSource`] supported by the target architecture (`Iframe`, or `Syscall` on
/// `x86_64`), and `gregs` must be non-null, properly aligned, and point to a valid, initialized
/// general register frame matching `source` that is not mutated for the duration of the call.
#[inline(always)]
pub unsafe fn get_instruction_pointer(
    source: super::GeneralRegsSource,
    gregs: *const core::ffi::c_void,
) -> usize {
    // SAFETY: Caller guarantees `source` and `gregs` are valid and match for reads.
    unsafe { super::arch_get_instruction_pointer(source, gregs) }
}

/// Sets the return instruction pointer in `gregs` to `ip` (`arch_set_return_instruction_pointer`).
///
/// # Safety
///
/// `source` must be a valid non-[`None`](super::GeneralRegsSource::None)
/// [`super::GeneralRegsSource`] supported by the target architecture (`Iframe`, or `Syscall` on
/// `x86_64`), and `gregs` must be non-null, properly aligned, and point to a valid, initialized
/// general register frame matching `source` with exclusive mutable access (no live references or
/// concurrent accesses) for the duration of the call.
#[inline(always)]
pub unsafe fn set_return_instruction_pointer(
    source: super::GeneralRegsSource,
    gregs: *mut core::ffi::c_void,
    ip: usize,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees `source` and `gregs` are valid and uniquely writable.
    unsafe { super::arch_set_return_instruction_pointer(source, gregs, ip) }
}

/// Returns whether ARM64 SVE register state is supported on this system.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn arch_arm64_sve_supported() -> bool {
    super::arm64::debugger::arch_arm64_sve_supported()
}

/// Reads the ARM64 SVE predicate registers and FFR for `thread` into `out`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `out` must point
/// to aligned, writable, non-overlapping storage for `zx_arm64_thread_state_sve_regs_t`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn arch_get_arm64_sve(thread: *mut Thread, out: *mut u8) -> Result<(), Status> {
    // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
    unsafe { super::arm64::debugger::arch_get_arm64_sve(thread, out.cast()) }
}

/// Writes `in_` into the ARM64 SVE predicate registers and FFR for `thread`.
///
/// # Safety
///
/// `thread` must point to a live `Thread` that stays suspended or in an exception for the duration
/// of the call, no chain lock transaction may be active on the calling CPU, and `in_` must point
/// to aligned, initialized, non-overlapping storage for `zx_arm64_thread_state_sve_regs_t`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn arch_set_arm64_sve(thread: *mut Thread, in_: *const u8) -> Result<(), Status> {
    // SAFETY: Caller upholds all thread-state, lock, and buffer validity preconditions.
    unsafe { super::arm64::debugger::arch_set_arm64_sve(thread, in_.cast()) }
}
