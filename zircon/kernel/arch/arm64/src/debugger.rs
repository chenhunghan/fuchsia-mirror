// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Debugger access to the saved user registers, single step and hardware debug state of a thread.
//!
//! The thread accessors take `thread` as the `Thread*` that `<arch/debugger.h>` declares, and
//! require of their callers what it does: the thread is in an exception or is suspended, and stays
//! so.

use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};

use arm64_thread_bindings::{
    zx_arm64_thread_state_debug_regs__bindgen_ty_1, zx_arm64_thread_state_debug_regs_t,
    zx_arm64_thread_state_fp_regs_t, zx_arm64_thread_state_sve_regs_t,
    zx_arm64_thread_state_vector_regs_t,
};
use libarch::arm64::{MonitorDebugSystemControlRegister, SavedProgramStatusRegister};
use zx_status::Status;
use zx_types::{
    ZX_ARM64_FEATURE_ISA_SVE, zx_thread_state_general_regs_t, zx_thread_state_single_step_t,
};

use super::mp::arch_get_restricted_flag;
use super::registers::{
    Arm64DebugState, arm64_hw_breakpoint_count, arm64_hw_watchpoint_count,
    arm64_set_debug_state_for_thread, arm64_validate_debug_state,
};
use super::thread::locked_thread_arch;
use super::{
    ARM_NCZV_FLAGS, ARM_USER_RESTRICTED_VISIBLE_FLAGS, ARM64_USER_VISIBLE_FLAGS, GeneralRegsSource,
    Iframe, arm64_feature_test,
};
use crate::clt_tag;
use crate::kernel::thread::{Thread, ThreadLockGuard};

// The layouts that `zircon/kernel/tests/abi_type_validator.cc` checks for the C types.
zr::static_assert!(size_of::<zx_arm64_thread_state_vector_regs_t>() == 520);
zr::static_assert!(align_of::<zx_arm64_thread_state_vector_regs_t>() == 8);
zr::static_assert!(size_of::<zx_arm64_thread_state_sve_regs_t>() == 34);
zr::static_assert!(align_of::<zx_arm64_thread_state_sve_regs_t>() == 2);
zr::static_assert!(offset_of!(zx_arm64_thread_state_sve_regs_t, p) == 0);
zr::static_assert!(offset_of!(zx_arm64_thread_state_sve_regs_t, ffr) == 32);
zr::static_assert!(size_of::<zx_arm64_thread_state_debug_regs_t>() == 528);
zr::static_assert!(align_of::<zx_arm64_thread_state_debug_regs_t>() == 8);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, hw_bps) == 0);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs__bindgen_ty_1, dbgbcr) == 0);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs__bindgen_ty_1, dbgbvr) == 8);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, hw_wps) == 256);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, far) == 512);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, esr) == 520);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, hw_bps_count) == 524);
zr::static_assert!(offset_of!(zx_arm64_thread_state_debug_regs_t, hw_wps_count) == 525);
zr::static_assert!(size_of::<zx_arm64_thread_state_fp_regs_t>() == 4);
zr::static_assert!(align_of::<zx_arm64_thread_state_fp_regs_t>() == 4);

// `zx_arm64_thread_state_general_regs_t`'s layout in `<zircon/syscalls/debug.h>`: all `uint64_t`.
zr::static_assert!(size_of::<zx_thread_state_general_regs_t>() == 280);
zr::static_assert!(align_of::<zx_thread_state_general_regs_t>() == 8);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r) == 0);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, lr) == 240);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, sp) == 248);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, pc) == 256);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, cpsr) == 264);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, tpidr) == 272);

/// SS (="Single Step") is bit 0 in MDSCR_EL1.
const MDSCR_SS_MASK: u64 = MonitorDebugSystemControlRegister::SS_MASK;

/// Single Step for PSTATE, see ARMv8 Manual C5.2.18, enable Single step for Process
const SS_MASK_SPSR: u64 = SavedProgramStatusRegister::SS_MASK;

/// Reads the general registers of `thread`.
///
/// The caller is responsible for making sure the thread is in an exception or is suspended, and
/// stays so.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `out` is aligned
/// and valid for writes of a `zx_thread_state_general_regs_t`, nothing else accesses it during the
/// call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_general_regs(
    thread: *mut Thread,
    out: *mut zx_thread_state_general_regs_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_get_general_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    let (suspended_general_regs, tpidr_el0) = unsafe {
        let arch = locked_thread_arch(&thread_guard);
        ((*arch).suspended_general_regs, (*arch).tpidr_el0)
    };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }

    let in_ = suspended_general_regs.cast::<Iframe>();
    debug_assert!(!in_.is_null());
    // SAFETY: `in_` is the `iframe_t` that `arch_set_suspended_general_regs` recorded, which stays
    // live and unwritten while the thread's user state stays saved. mod.rs pins `Iframe`'s size and
    // every field offset to the values regs.h asserts for `iframe_t`.
    let in_ = unsafe { &*in_ };

    // SAFETY: The caller guarantees `out`. Each field is written through the raw pointer, so `out`
    // need not be initialized; the six fields cover the whole struct.
    unsafe {
        (*out).r = in_.r;
        (*out).lr = in_.lr;
        (*out).sp = in_.usp;
        (*out).pc = in_.elr;
        // Enable a view into user visible registers as well as those
        // settable by restricted mode.
        (*out).cpsr = in_.spsr & u64::from(ARM_USER_RESTRICTED_VISIBLE_FLAGS);
        (*out).tpidr = tpidr_el0;
    }

    Ok(())
}

/// Writes the general registers of `thread`.
///
/// The caller is responsible for making sure the thread is in an exception or is suspended, and
/// stays so.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `in_` is aligned
/// and valid for reads of an initialized `zx_thread_state_general_regs_t`, nothing writes it during
/// the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_general_regs(
    thread: *mut Thread,
    in_: *const zx_thread_state_general_regs_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_set_general_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    let arch = locked_thread_arch(&thread_guard);
    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    let suspended_general_regs = unsafe { (*arch).suspended_general_regs };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }

    let out = suspended_general_regs.cast::<Iframe>();
    debug_assert!(!out.is_null());
    // SAFETY: `out` is the `iframe_t` that `arch_set_suspended_general_regs` recorded, which stays
    // live, and is not otherwise accessed, while the thread's user state stays saved. mod.rs pins
    // `Iframe`'s size and every field offset to the values regs.h asserts for `iframe_t`.
    let out = unsafe { &mut *out };
    // SAFETY: The caller guarantees `in_`.
    let in_ = unsafe { &*in_ };

    out.r = in_.r;
    out.lr = in_.lr;
    out.usp = in_.sp;
    out.elr = in_.pc;
    if arch_get_restricted_flag() {
        // Preserve all flags outside of NCZV (the directly user visible set) when
        // in restricted mode.  This allows continuity in 32-bit or 64-bit without
        // allowing direct access to mode changes.
        out.spsr = (out.spsr & u64::from(!ARM_NCZV_FLAGS)) | (in_.cpsr & u64::from(ARM_NCZV_FLAGS));
    } else {
        // A normal mode thread should only allow user visible flags to be set.
        // However, this function may be called on a thread that has transitioned
        // from restricted mode, and as such, the mask below ensures that any saved
        // restricted-mode-only flags are not preserved.
        out.spsr = (out.spsr & u64::from(!ARM_USER_RESTRICTED_VISIBLE_FLAGS))
            | (in_.cpsr & u64::from(ARM64_USER_VISIBLE_FLAGS));
    }
    // SAFETY: As for the read of `suspended_general_regs` above.
    unsafe { (*arch).tpidr_el0 = in_.tpidr };

    Ok(())
}

/// Reads whether single step is enabled for `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `out` is aligned
/// and valid for writes of a `zx_thread_state_single_step_t`, nothing else accesses it during the
/// call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_single_step(
    thread: *mut Thread,
    out: *mut zx_thread_state_single_step_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_get_single_step")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    let (suspended_general_regs, mdscr_el1) = unsafe {
        let arch = locked_thread_arch(&thread_guard);
        ((*arch).suspended_general_regs, (*arch).mdscr_el1)
    };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }
    let regs = suspended_general_regs.cast::<Iframe>();

    let mdscr_ss_enable = (mdscr_el1 & MDSCR_SS_MASK) != 0;
    // SAFETY: `regs` is the `iframe_t` that `arch_set_suspended_general_regs` recorded, which stays
    // live and unwritten while the thread's user state stays saved. mod.rs pins `Iframe`'s size and
    // every field offset to the values regs.h asserts for `iframe_t`.
    let spsr = unsafe { (*regs).spsr };
    let spsr_ss_enable = (spsr & SS_MASK_SPSR) != 0;

    // SAFETY: The caller guarantees `out`.
    unsafe { *out = u32::from(mdscr_ss_enable && spsr_ss_enable) };
    Ok(())
}

/// Enables or disables single step for `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `in_` is aligned
/// and valid for reads of an initialized `zx_thread_state_single_step_t`, nothing writes it during
/// the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_single_step(
    thread: *mut Thread,
    in_: *const zx_thread_state_single_step_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `in_`.
    let in_ = unsafe { &*in_ };
    if *in_ != 0 && *in_ != 1 {
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_set_single_step")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    let arch = locked_thread_arch(&thread_guard);
    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    let suspended_general_regs = unsafe { (*arch).suspended_general_regs };

    // Punt if registers aren't available. E.g.,
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    if suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }
    // SAFETY: The pointer is the `iframe_t` that `arch_set_suspended_general_regs` recorded, which
    // stays live, and is not otherwise accessed, while the thread's user state stays saved. mod.rs
    // pins `Iframe`'s size and every field offset to the values regs.h asserts for `iframe_t`.
    let regs = unsafe { &mut *suspended_general_regs.cast::<Iframe>() };
    // SAFETY: As for the read of `suspended_general_regs` above. The reference covers only
    // `mdscr_el1`.
    let mdscr_el1 = unsafe { &mut (*arch).mdscr_el1 };
    if *in_ != 0 {
        *mdscr_el1 |= MDSCR_SS_MASK;
        regs.spsr |= SS_MASK_SPSR;
    } else {
        *mdscr_el1 &= !MDSCR_SS_MASK;
        regs.spsr &= !SS_MASK_SPSR;
    }
    Ok(())
}

/// Fails with `NOT_SUPPORTED`: arm64 keeps all floating point state in the vector registers.
#[unsafe(no_mangle)]
pub extern "C" fn arch_get_fp_regs(
    _thread: *mut Thread,
    _out: *mut zx_arm64_thread_state_fp_regs_t,
) -> Result<(), Status> {
    // There are no ARM fp regs.
    Err(Status::NOT_SUPPORTED)
}

/// Fails with `NOT_SUPPORTED`: arm64 keeps all floating point state in the vector registers.
#[unsafe(no_mangle)]
pub extern "C" fn arch_set_fp_regs(
    _thread: *mut Thread,
    _in: *const zx_arm64_thread_state_fp_regs_t,
) -> Result<(), Status> {
    // There are no ARM fp regs.
    Err(Status::NOT_SUPPORTED)
}

/// Reads the vector registers of `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `out` is aligned
/// and valid for writes of a `zx_arm64_thread_state_vector_regs_t`, nothing else accesses it during
/// the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_vector_regs(
    thread: *mut Thread,
    out: *mut zx_arm64_thread_state_vector_regs_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_get_vector_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    // The reference covers only `fpstate`.
    let in_ = unsafe { &(*locked_thread_arch(&thread_guard)).fpstate };
    // SAFETY: The caller guarantees `out`. Each field is written through the raw pointer, so `out`
    // need not be initialized; `fpcr`, `fpsr` and the 32 `v` entries cover the whole struct.
    unsafe {
        (*out).fpcr = in_.fpcr;
        (*out).fpsr = in_.fpsr;
        for i in 0..32 {
            (*out).v[i].low = in_.regs[i * 2];
            (*out).v[i].high = in_.regs[i * 2 + 1];
        }
    }

    Ok(())
}

/// Writes the vector registers of `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `in_` is aligned
/// and valid for reads of an initialized `zx_arm64_thread_state_vector_regs_t`, nothing writes it
/// during the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_vector_regs(
    thread: *mut Thread,
    in_: *const zx_arm64_thread_state_vector_regs_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_set_vector_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    // The reference covers only `fpstate`.
    let out = unsafe { &mut (*locked_thread_arch(&thread_guard)).fpstate };
    // SAFETY: The caller guarantees `in_`.
    let in_ = unsafe { &*in_ };
    out.fpcr = in_.fpcr;
    out.fpsr = in_.fpsr;
    for (regs, v) in out.regs.as_chunks_mut::<2>().0.iter_mut().zip(&in_.v) {
        regs[0] = v.low;
        regs[1] = v.high;
    }

    Ok(())
}

/// Whether the CPU has SVE, and so `thread` state has the SVE predicate registers and FFR.
#[unsafe(no_mangle)]
pub extern "C" fn arch_arm64_sve_supported() -> bool {
    arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE)
}

/// Reads the SVE predicate registers and FFR of `thread`. Fails with `NOT_SUPPORTED` when the CPU
/// has no SVE.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `out` is aligned
/// and valid for writes of a `zx_arm64_thread_state_sve_regs_t`, nothing else accesses it during
/// the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_arm64_sve(
    thread: *mut Thread,
    out: *mut zx_arm64_thread_state_sve_regs_t,
) -> Result<(), Status> {
    if !arch_arm64_sve_supported() {
        return Err(Status::NOT_SUPPORTED);
    }

    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_get_arm64_sve")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    // The reference covers only `fpstate`.
    let in_ = unsafe { &(*locked_thread_arch(&thread_guard)).fpstate };
    // Canonicalize FFR to its contiguous prefix of 1s from bit 0 so that even if a user thread
    // executed WRFFR with a non-monotonic predicate (producing an UNKNOWN architectural FFR
    // value), reading state and writing it back unchanged never fails with ZX_ERR_INVALID_ARGS.
    let ones = in_.ffr.trailing_ones();
    // SAFETY: The caller guarantees `out`. Each field is written through the raw pointer, so `out`
    // need not be initialized; `p` and `ffr` cover the whole struct.
    unsafe {
        (*out).p = in_.p_regs;
        (*out).ffr = ((1u32 << ones) - 1) as u16;
    }

    Ok(())
}

/// Writes the SVE predicate registers and FFR of `thread`. Fails with `NOT_SUPPORTED` when the CPU
/// has no SVE, and with `INVALID_ARGS` when `in_`'s FFR is not a contiguous run of 1s from bit 0.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `in_` is aligned
/// and valid for reads of an initialized `zx_arm64_thread_state_sve_regs_t`, nothing writes it
/// during the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_arm64_sve(
    thread: *mut Thread,
    in_: *const zx_arm64_thread_state_sve_regs_t,
) -> Result<(), Status> {
    if !arch_arm64_sve_supported() {
        return Err(Status::NOT_SUPPORTED);
    }

    // SAFETY: The caller guarantees `in_`.
    let in_ = unsafe { &*in_ };
    // Per the Arm ARM for WRFFR, the source predicate bits must be a monotonically
    // decreasing sequence of 1s starting from bit 0 (i.e. 2^k - 1 for 0 <= k <= 16).
    let ffr = u32::from(in_.ffr);
    if ((ffr + 1) & ffr) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_set_arm64_sve")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    // The reference covers only `fpstate`.
    let out = unsafe { &mut (*locked_thread_arch(&thread_guard)).fpstate };
    out.p_regs = in_.p;
    out.ffr = in_.ffr;

    Ok(())
}

/// Reads the hardware breakpoints and watchpoints of `thread`, the CPU's count of each, and the ESR
/// and FAR of its last debug exception.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `out` is aligned
/// and valid for writes of a `zx_arm64_thread_state_debug_regs_t`, nothing else accesses it during
/// the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_debug_regs(
    thread: *mut Thread,
    out: *mut zx_arm64_thread_state_debug_regs_t,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `out`. Its fields are integers and arrays of integers or of
    // structs of integers, for which all zero bytes are a valid value.
    unsafe { out.write_bytes(0, 1) };
    // SAFETY: As above.
    let out = unsafe { &mut *out };
    out.hw_bps_count = arm64_hw_breakpoint_count();
    out.hw_wps_count = arm64_hw_watchpoint_count();

    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_get_debug_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    // The kernel ensures that this state is being kept up to date, so we can safely copy the
    // information over.
    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    // The reference covers only `debug_state`.
    let debug_state = unsafe { &(*locked_thread_arch(&thread_guard)).debug_state };

    // HW breakpoints.
    let hw_bps_count = usize::from(out.hw_bps_count);
    for (hw_bp, state) in out.hw_bps.iter_mut().zip(&debug_state.hw_bps).take(hw_bps_count) {
        hw_bp.dbgbcr = state.dbgbcr;
        hw_bp.dbgbvr = state.dbgbvr;
    }

    // Watchpoints.
    let hw_wps_count = usize::from(out.hw_wps_count);
    for (hw_wp, state) in out.hw_wps.iter_mut().zip(&debug_state.hw_wps).take(hw_wps_count) {
        hw_wp.dbgwcr = state.dbgwcr;
        hw_wp.dbgwvr = state.dbgwvr;
    }

    out.esr = debug_state.esr;
    out.far = debug_state.far;

    Ok(())
}

/// Validates and installs the hardware breakpoints and watchpoints in `in_` as `thread`'s debug
/// state.
///
/// # Safety
///
/// `thread` points to a live `Thread` that is in an exception or suspended, so its user state is
/// saved and it makes none of its lock-free `arch_thread` writes, and stays so. `in_` is aligned
/// and valid for reads of an initialized `zx_arm64_thread_state_debug_regs_t`, nothing writes it
/// during the call, and it does not overlap `thread`'s state. The calling CPU is in no chain lock
/// transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_debug_regs(
    thread: *mut Thread,
    in_: *const zx_arm64_thread_state_debug_regs_t,
) -> Result<(), Status> {
    // SAFETY: The fields are integers and arrays of structs of integers, for which all zero bytes
    // are a valid value.
    let mut state: Arm64DebugState = unsafe { core::mem::zeroed() };
    // SAFETY: The caller guarantees `in_`.
    let in_ = unsafe { &*in_ };

    // We copy over the state from the input.
    let bp_count = usize::from(arm64_hw_breakpoint_count());
    for (hw_bp, in_bp) in state.hw_bps.iter_mut().zip(&in_.hw_bps).take(bp_count) {
        hw_bp.dbgbcr = in_bp.dbgbcr;
        hw_bp.dbgbvr = in_bp.dbgbvr;
    }

    let wp_count = usize::from(arm64_hw_watchpoint_count());
    for (hw_wp, in_wp) in state.hw_wps.iter_mut().zip(&in_.hw_wps).take(wp_count) {
        hw_wp.dbgwcr = in_wp.dbgwcr;
        hw_wp.dbgwvr = in_wp.dbgwvr;
    }

    let Some((active_breakpoints, active_watchpoints)) = arm64_validate_debug_state(&mut state)
    else {
        return Err(Status::INVALID_ARGS);
    };

    // SAFETY: The caller guarantees `thread` and that this CPU is in no chain lock transaction. The
    // guard is a pinned local dropped before this function returns, with nothing in between that
    // blocks.
    let lock = unsafe { ThreadLockGuard::lock(thread, clt_tag!("arch_set_debug_regs")) };
    ksync::lock!(let thread_guard = lock);

    debug_assert!(thread_guard.is_user_state_saved_locked());

    let arch = locked_thread_arch(&thread_guard);

    // If the suspended registers are not there, we cannot save the MDSCR values for this thread,
    // meaning that the debug HW state will be cleared almost immediately.
    // This should always be there.
    // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
    // SAFETY: The caller keeps the thread's user state saved, as the `debug_assert!` above checks,
    // so it makes no lock-free `arch_thread` write, and `thread_guard` keeps out the locked ones.
    let suspended_general_regs = unsafe { (*arch).suspended_general_regs };
    if suspended_general_regs.is_null() {
        return Err(Status::NOT_SUPPORTED);
    }

    let hw_debug_needed = (active_breakpoints > 0) || (active_watchpoints > 0);

    // SAFETY: As for the read of `suspended_general_regs` above. Each reference covers one field.
    unsafe {
        arm64_set_debug_state_for_thread(&mut (*arch).mdscr_el1, hw_debug_needed);
        state.esr = (*arch).debug_state.esr;
        state.far = (*arch).debug_state.far;

        (*arch).track_debug_state = true;
        (*arch).debug_state = state;
    }

    Ok(())
}

/// Returns the instruction pointer saved in `gregs`.
///
/// # Safety
///
/// `source` is `GeneralRegsSource::Iframe`, the only representation arm64 saves, and `gregs`
/// points to a live `iframe_t` that nothing writes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_get_instruction_pointer(
    source: GeneralRegsSource,
    gregs: *mut c_void,
) -> usize {
    debug_assert!(source == GeneralRegsSource::Iframe, "invalid source {}\n", source as u32);
    // SAFETY: The caller guarantees that `gregs` points at a live `iframe_t` that nothing writes.
    // mod.rs pins `Iframe`'s size and every field offset to the values regs.h asserts for
    // `iframe_t`.
    let elr = unsafe { (*gregs.cast::<Iframe>()).elr };
    elr as usize
}

/// Sets the instruction pointer that a return to user mode through `gregs` resumes at.
///
/// # Safety
///
/// `source` is `GeneralRegsSource::Iframe`, the only representation arm64 saves, and `gregs`
/// points to a live `iframe_t` that nothing else accesses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_return_instruction_pointer(
    source: GeneralRegsSource,
    gregs: *mut c_void,
    ip: usize,
) -> Result<(), Status> {
    debug_assert!(source == GeneralRegsSource::Iframe, "invalid source {}\n", source as u32);
    // SAFETY: The caller guarantees that `gregs` points at a live `iframe_t` that nothing else
    // accesses. mod.rs pins `Iframe`'s size and every field offset to the values regs.h asserts for
    // `iframe_t`.
    unsafe { (*gregs.cast::<Iframe>()).elr = ip as u64 };
    Ok(())
}

/// Returns the number of hardware breakpoints this CPU has.
#[unsafe(no_mangle)]
pub extern "C" fn arch_get_hw_breakpoint_count() -> u8 {
    arm64_hw_breakpoint_count()
}

/// Returns the number of hardware watchpoints this CPU has.
#[unsafe(no_mangle)]
pub extern "C" fn arch_get_hw_watchpoint_count() -> u8 {
    arm64_hw_watchpoint_count()
}
