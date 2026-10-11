// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Architectural debugger register accessors and control for RISC-V 64.

use super::arch::RISCV64_CSR_VTYPE_VILL;
use super::feature;
use super::restricted::Iframe;
use super::thread::{GeneralRegsSource, locked_thread_arch, locked_thread_arch_mut};
use super::vector::riscv64_vlmax;
use crate::clt_tag;
use crate::kernel::thread::ThreadLockGuard;
use core::ffi::c_void;
use debug::ltracef;
use libarch::riscv64::VectorType;
use riscv64_thread_bindings::{
    zx_riscv64_thread_state_fp_regs_t, zx_riscv64_thread_state_vector_regs_t,
};
use zx_status::Status;
use zx_types::{zx_thread_state_general_regs_t, zx_thread_state_single_step_t};

const LOCAL_TRACE: u32 = 0;

const _: () = {
    assert!(core::mem::size_of::<zx_riscv64_thread_state_fp_regs_t>() == 528);
    assert!(core::mem::align_of::<zx_riscv64_thread_state_fp_regs_t>() == 16);
};

/// Opaque zero-sized debug registers representation for RISC-V 64.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct zx_thread_state_debug_regs_t {
    pub unused: u32,
}

/// Get the instruction pointer from the given general registers source.
///
/// # Safety
/// `gregs` must point to a live `Iframe` that outlives the call. riscv64 only
/// ever saves general registers into an iframe, so `source` must be
/// `GeneralRegsSource::Iframe`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_instruction_pointer(
    source: GeneralRegsSource,
    gregs: *const core::ffi::c_void,
) -> usize {
    debug_assert!(source == GeneralRegsSource::Iframe);
    // SAFETY: the caller guarantees `gregs` points at a live `Iframe`; the
    // assertion above pins down which representation that is.
    let iframe = unsafe { &*(gregs as *const Iframe) };
    iframe.regs.pc as usize
}

/// Set the return instruction pointer in the given general registers source.
///
/// # Safety
/// `gregs` must point to a live, uniquely borrowed `Iframe` that outlives the
/// call, and `source` must be `GeneralRegsSource::Iframe`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_return_instruction_pointer(
    source: GeneralRegsSource,
    gregs: *mut core::ffi::c_void,
    ip: usize,
) -> Result<(), Status> {
    debug_assert!(source == GeneralRegsSource::Iframe);
    // SAFETY: the caller guarantees `gregs` points at a live `Iframe` that
    // nothing else is holding a reference to for the duration of the call.
    let iframe = unsafe { &mut *(gregs as *mut Iframe) };
    iframe.regs.pc = ip as u64;
    Ok(())
}

/// Hardware breakpoint count (0 on current riscv64).
#[unsafe(no_mangle)]
pub extern "C" fn arch_get_hw_breakpoint_count() -> u8 {
    0
}

/// Hardware watchpoint count (0 on current riscv64).
#[unsafe(no_mangle)]
pub extern "C" fn arch_get_hw_watchpoint_count() -> u8 {
    0
}

/// Single step debugging is currently unsupported on riscv64.
///
/// # Safety
/// Nothing: the arguments are never dereferenced. The function is `unsafe` only
/// to match the signature the other architectures export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_single_step(
    _thread: *mut core::ffi::c_void,
    _out: *mut zx_thread_state_single_step_t,
) -> Status {
    Status::NOT_SUPPORTED
}

/// Single step debugging is currently unsupported on riscv64.
///
/// # Safety
/// Nothing: the arguments are never dereferenced. The function is `unsafe` only
/// to match the signature the other architectures export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_single_step(
    _thread: *mut core::ffi::c_void,
    _in: *const zx_thread_state_single_step_t,
) -> Status {
    Status::NOT_SUPPORTED
}

/// Debug registers are zero-sized on riscv64.
///
/// # Safety
/// Nothing: the arguments are never dereferenced. The function is `unsafe` only
/// to match the signature the other architectures export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_debug_regs(
    _thread: *mut core::ffi::c_void,
    _out: *mut zx_thread_state_debug_regs_t,
) -> Result<(), Status> {
    Ok(())
}

/// Debug registers are zero-sized on riscv64.
///
/// # Safety
/// Nothing: the arguments are never dereferenced. The function is `unsafe` only
/// to match the signature the other architectures export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_debug_regs(
    _thread: *mut core::ffi::c_void,
    _in: *const zx_thread_state_debug_regs_t,
) -> Result<(), Status> {
    Ok(())
}

/// Reads the general registers of `thread`.
///
/// The caller is responsible for making sure the thread is in an exception or is suspended, and
/// stays so.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `out` to a writable
/// `zx_thread_state_general_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_general_regs(
    thread: *mut c_void,
    out: *mut zx_thread_state_general_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} out {:p}\n", thread, out);

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_get_general_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: this function's caller keeps the thread suspended or in an exception.
    let arch = unsafe { locked_thread_arch(&thread_guard) };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if arch.suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }

    let in_ = arch.suspended_general_regs;
    debug_assert!(!in_.is_null());

    // SAFETY: `in_` is the suspended thread's saved iframe, and the caller guarantees `out`.
    unsafe { *out = (*in_).regs };

    Ok(())
}

/// Writes the general registers of `thread`.
///
/// The caller is responsible for making sure the thread is in an exception or is suspended, and
/// stays so.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `in_` to a readable
/// `zx_thread_state_general_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_general_regs(
    thread: *mut c_void,
    in_: *const zx_thread_state_general_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} in {:p}\n", thread, in_);

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_set_general_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: this function's caller keeps the thread suspended or in an exception.
    let arch = unsafe { locked_thread_arch(&thread_guard) };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if arch.suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }

    let out = arch.suspended_general_regs;
    debug_assert!(!out.is_null());

    // SAFETY: `out` is the suspended thread's saved iframe, and the caller guarantees `in_`.
    unsafe { (*out).regs = *in_ };

    Ok(())
}

/// Reads the floating point registers of `thread`.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `out` to a writable `zx_thread_state_fp_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_fp_regs(
    thread: *mut c_void,
    out: *mut zx_riscv64_thread_state_fp_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} out {:p}\n", thread, out);

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_get_fp_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: the caller guarantees `out`.
    let out = unsafe { &mut *out };
    *out = Default::default();

    // SAFETY: this function's caller keeps the thread suspended or in an exception.
    let in_ = unsafe { &locked_thread_arch(&thread_guard).fpu_state };
    for (q, f) in out.q.iter_mut().zip(in_.f.iter()) {
        q.low = *f;
        q.high = u64::MAX;
    }
    out.fcsr = in_.fcsr;

    Ok(())
}

/// Writes the floating point registers of `thread`.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `in_` to a readable `zx_thread_state_fp_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_fp_regs(
    thread: *mut c_void,
    in_: *const zx_riscv64_thread_state_fp_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} in {:p}\n", thread, in_);

    // SAFETY: the caller guarantees `in_`.
    let in_ = unsafe { &*in_ };

    // Check that the input is valid. The high bits must be all 1s.
    if in_.q.iter().any(|q| q.high != u64::MAX) {
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_set_fp_regs")) };
    ksync::lock!(let mut thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: this function's caller keeps the thread suspended or in an exception.
    let arch = unsafe { locked_thread_arch_mut(thread_guard.as_mut()) };
    let out = &mut arch.fpu_state;
    for (f, q) in out.f.iter_mut().zip(in_.q.iter()) {
        *f = q.low;
    }
    out.fcsr = in_.fcsr;

    // Mark the state as dirty in case it hadn't already been touched. This will
    // force the context switch routine to load it on next switch.
    arch.fpu_dirty = true;

    Ok(())
}

/// Reads the vector registers of `thread`.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `out` to a writable
/// `zx_thread_state_vector_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_vector_regs(
    thread: *mut c_void,
    out: *mut zx_riscv64_thread_state_vector_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} out {:p}\n", thread, out);

    if !feature::has_vector() {
        return Err(Status::NOT_SUPPORTED);
    }

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_get_vector_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());
    // SAFETY: this function's caller keeps the thread suspended or in an exception, and
    // guarantees `out`.
    unsafe { *out = locked_thread_arch(&thread_guard).vector_state };
    Ok(())
}

/// Writes the vector registers of `thread`.
///
/// # Safety
/// `thread` must point to a live `Thread`, and `in_` to a readable
/// `zx_thread_state_vector_regs_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_vector_regs(
    thread: *mut c_void,
    in_: *const zx_riscv64_thread_state_vector_regs_t,
) -> Result<(), Status> {
    ltracef!("thread {:p} in {:p}\n", thread, in_);

    if !feature::has_vector() {
        return Err(Status::NOT_SUPPORTED);
    }

    // SAFETY: the caller guarantees `in_`.
    let in_ = unsafe { &*in_ };

    // vcsr[63:3] is reserved-as-zero.
    let vcsr_rsvd_are_zero = (in_.vcsr >> 3) == 0;
    if !vcsr_rsvd_are_zero {
        return Err(Status::INVALID_ARGS);
    }

    let Some(vlmax) = riscv64_vlmax(in_.vtype) else {
        return Err(Status::INVALID_ARGS);
    };

    // Setting only VILL in vtype and setting vl as zero is the canonical 'reset'
    // state. Invalid values outside of that pair will be rejected.
    if in_.vtype != RISCV64_CSR_VTYPE_VILL || in_.vl != 0 {
        // vtype[63] (VILL) should be zero outside of the canonical reset state, and
        // vtype[62:8] is reserved-as-zero.
        let vtype_rsvd_are_zero = (in_.vtype >> 8) == 0;
        let vtype = VectorType::from(in_.vtype);
        let sew = vtype.vsew();
        let lmul = vtype.vlmul();
        if !vtype_rsvd_are_zero || sew >= 0b100 || lmul == 0b100 {
            // Reserved values.
            return Err(Status::INVALID_ARGS);
        }

        // vl may be any value up to VLMAX: vsetvli sets arbitrary lengths, and a thread
        // suspended in a vector loop keeps whatever it last set.
        if in_.vl > vlmax {
            return Err(Status::INVALID_ARGS);
        }
    }

    // VLMAX - 1 is the largest possible element index.
    if in_.vstart >= vlmax {
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: the caller guarantees `thread` is a live `Thread`. This CPU is in no chain lock
    // transaction, and the guard is a pinned local dropped before this function returns, with
    // nothing in between that blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread.cast(), clt_tag!("arch_set_vector_regs")) };
    ksync::lock!(let mut thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: this function's caller keeps the thread suspended or in an exception.
    let arch = unsafe { locked_thread_arch_mut(thread_guard.as_mut()) };
    arch.vector_state = *in_;

    // Mark the state as dirty in case it hadn't already been touched. This will
    // force the context switch routine to load it on next switch.
    arch.vector_dirty = true;

    Ok(())
}
