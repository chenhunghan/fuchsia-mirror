// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Debugger access to the saved user registers, single step and hardware debug state of an x86
//! thread.
//!
//! Note on locking: The below functions need to read and write the register state and make sure
//! that nothing happens with respect to scheduling that thread while this is happening. As a
//! result they use the thread's lock. In most cases this will not be necessary but there are
//! relatively few guarantees so we lock the scheduler. Since these functions are used mostly for
//! debugging, this shouldn't be too significant a performance penalty.

use core::ffi::c_void;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};

use zx_status::Status;
use zx_types::{zx_thread_state_general_regs_t, zx_thread_state_single_step_t};

use super::registers::{
    self, HW_DEBUG_REGISTERS_COUNT, X86_FLAGS_TF, X86_FLAGS_USER, X86_XSAVE_STATE_INDEX_AVX,
    X86_XSAVE_STATE_INDEX_SSE, X86_XSAVE_STATE_INDEX_X87, X86DebugState,
};
use super::thread::thread_arch;
use super::{GeneralRegsSource, Iframe, SyscallRegs, vm};
use crate::clt_tag;
use crate::kernel::thread::{Thread, ThreadLockGuard, thread_lock_guard};

pub use x86_registers_bindings::{
    zx_thread_state_debug_regs_t, zx_thread_state_fp_regs_t, zx_thread_state_vector_regs_t,
};

// `zx_x86_64_thread_state_general_regs_t` and `zx_thread_state_single_step_t` layouts in
// `<zircon/syscalls/debug.h>`.
zr::static_assert!(size_of::<zx_thread_state_general_regs_t>() == 160);
zr::static_assert!(align_of::<zx_thread_state_general_regs_t>() == 8);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rax) == 0);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rbx) == 8);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rcx) == 16);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rdx) == 24);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rsi) == 32);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rdi) == 40);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rbp) == 48);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rsp) == 56);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r8) == 64);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r9) == 72);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r10) == 80);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r11) == 88);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r12) == 96);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r13) == 104);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r14) == 112);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, r15) == 120);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rip) == 128);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, rflags) == 136);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, fs_base) == 144);
zr::static_assert!(offset_of!(zx_thread_state_general_regs_t, gs_base) == 152);
zr::static_assert!(size_of::<zx_thread_state_single_step_t>() == 4);
zr::static_assert!(align_of::<zx_thread_state_single_step_t>() == 4);
zr::static_assert!(size_of::<zx_thread_state_fp_regs_t>() == 160);
zr::static_assert!(align_of::<zx_thread_state_fp_regs_t>() == 16);
zr::static_assert!(size_of::<zx_thread_state_vector_regs_t>() == 2120);
zr::static_assert!(align_of::<zx_thread_state_vector_regs_t>() == 8);
zr::static_assert!(size_of::<zx_thread_state_debug_regs_t>() == 48);
zr::static_assert!(align_of::<zx_thread_state_debug_regs_t>() == 8);

macro_rules! copy_common_regs {
    ($out:expr, $in:expr) => {{
        let out = $out;
        let in_ = $in;
        (*out).rax = (*in_).rax;
        (*out).rbx = (*in_).rbx;
        (*out).rcx = (*in_).rcx;
        (*out).rdx = (*in_).rdx;
        (*out).rsi = (*in_).rsi;
        (*out).rdi = (*in_).rdi;
        (*out).rbp = (*in_).rbp;
        (*out).r8 = (*in_).r8;
        (*out).r9 = (*in_).r9;
        (*out).r10 = (*in_).r10;
        (*out).r11 = (*in_).r11;
        (*out).r12 = (*in_).r12;
        (*out).r13 = (*in_).r13;
        (*out).r14 = (*in_).r14;
        (*out).r15 = (*in_).r15;
    }};
}

/// Copies the general registers in `in_` into `out`. On return, every field of `*out` except
/// `fs_base` and `gs_base` is initialized, and no uninitialized bytes are written to `*out`.
///
/// # Safety
///
/// The caller must ensure that `out` is aligned, valid for writes of a
/// `zx_thread_state_general_regs_t`, uniquely accessed during the call, and non-overlapping with
/// `in_`.
#[inline]
unsafe fn fill_in_gregs_from_syscall(out: *mut zx_thread_state_general_regs_t, in_: &SyscallRegs) {
    // SAFETY: Caller guarantees `out` is aligned, writable, uniquely accessed, and non-overlapping
    // with `in_`.
    unsafe {
        copy_common_regs!(out, in_);
        (*out).rip = in_.rip;
        (*out).rsp = in_.rsp;
        (*out).rflags = in_.rflags;
    }
}

#[inline]
fn fill_in_syscall_from_gregs(out: &mut SyscallRegs, in_: &zx_thread_state_general_regs_t) {
    copy_common_regs!(&mut *out, in_);
    out.rip = in_.rip;
    out.rsp = in_.rsp;
    // Don't allow overriding privileged fields of rflags, and ignore writes
    // to reserved fields.
    out.rflags &= !X86_FLAGS_USER;
    out.rflags |= in_.rflags & X86_FLAGS_USER;
}

/// Copies the general registers in `in_` into `out`. On return, every field of `*out` except
/// `fs_base` and `gs_base` is initialized, and no uninitialized bytes are written to `*out`.
///
/// # Safety
///
/// The caller must ensure that `out` is aligned, valid for writes of a
/// `zx_thread_state_general_regs_t`, uniquely accessed during the call, and non-overlapping with
/// `in_`.
#[inline]
unsafe fn fill_in_gregs_from_iframe(out: *mut zx_thread_state_general_regs_t, in_: &Iframe) {
    // SAFETY: Caller guarantees `out` is aligned, writable, uniquely accessed, and non-overlapping
    // with `in_`.
    unsafe {
        copy_common_regs!(out, in_);
        (*out).rsp = in_.user_sp;
        (*out).rip = in_.ip;
        (*out).rflags = in_.flags;
    }
}

#[inline]
fn fill_in_iframe_from_gregs(out: &mut Iframe, in_: &zx_thread_state_general_regs_t) {
    copy_common_regs!(&mut *out, in_);
    out.user_sp = in_.rsp;
    out.ip = in_.rip;
    // Don't allow overriding privileged fields of rflags, and ignore writes
    // to reserved fields.
    out.flags &= !X86_FLAGS_USER;
    out.flags |= in_.rflags & X86_FLAGS_USER;
}

/// Whether an operation gets thread state or sets it.
enum RegAccess<'a> {
    Get(&'a mut MaybeUninit<zx_thread_state_vector_regs_t>),
    Set(&'a zx_thread_state_vector_regs_t),
}

/// Checks whether the `mxcsr` register has unsupported bits.
///
/// The processor specifies which flags of the `mxcsr` are supported via the
/// `mxcsr_mask` obtained with the `fxsave` instruction.
///
/// The manuals mention that it is possible for the mask to be 0, and specify
/// `0x0000ffbf` as the default value.
///
/// For details see:
/// - Intel 64 and IA-32 Architectures Software Developer's Manual, Volume 1: Basic Architecture,
///   Section 11.6.6: Guidelines for Writing to the MXCSR Register.
/// - AMD64 Architecture Programmer's Manual, Volume 2: System Programming, Section 11.5.9: MXCSR
///   State Management.
#[inline]
fn mxcsr_is_valid(mxcsr: u32, mut mxcsr_mask: u32) -> bool {
    if mxcsr_mask == 0x0 {
        mxcsr_mask = 0x0000_ffbf;
    }

    (mxcsr & !mxcsr_mask) == 0
}

/// Backend for [`get_vector_regs`] and [`set_vector_regs`]. This does a read or write of the
/// thread to or from the [`zx_thread_state_vector_regs_t`] structure in `access`.
///
/// When `access` is [`RegAccess::Get`], its `out` buffer is never overwritten with uninitialized
/// bytes and is fully initialized when `Ok(())` is returned.
///
/// # Safety
///
/// The caller must ensure that:
/// - [`registers::extended_register_init`] has completed on CPU 0.
/// - `thread` points to a live (not freed), valid `Thread` that stays suspended or stopped in an
///   exception for the duration of the call, whose `extended_register_buffer` holds a valid
///   extended register state image (see [`registers::get_xsave_legacy_area`]).
/// - `access` does not overlap `thread`'s state.
/// - No chain lock transaction is active on the calling CPU.
#[inline(always)]
unsafe fn get_set_vector_regs(
    thread: *mut Thread,
    mut access: RegAccess<'_>,
) -> Result<(), Status> {
    if let RegAccess::Get(out) = &mut access {
        // Not all parts will be filled in in all cases so zero out first.
        // SAFETY: `out` is a valid mutable `MaybeUninit<zx_thread_state_vector_regs_t>`, which is a
        // C-repr POD struct valid when all-zero.
        unsafe { out.as_mut_ptr().write_bytes(0, 1) };
    }

    // Whether to force the components to be marked present in the xsave area.
    let mark_present = matches!(access, RegAccess::Set(_));

    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("x86_get_set_vector_regs")) }
    );
    debug_assert!(guard.is_user_state_saved_locked());

    const NUM_SSE_REGS: usize = 16;

    // SAFETY: `thread` points to a live `Thread` allocation; `thread_arch` only offsets the
    // pointer.
    let arch = unsafe { thread_arch(thread) };

    // The low 128 bits of registers 0-15 come from the legacy area and are always present.
    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0 and keeps `thread`
    // suspended or stopped in an exception while `guard` holds its lock, giving exclusive access to
    // the 64-byte-aligned `(*arch).extended_register_buffer`, which holds a valid extended register
    // state image and is disjoint from `access`. `X86_XSAVE_STATE_INDEX_SSE` is `1`.
    let save = unsafe {
        &mut *registers::get_xsave_legacy_area(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
            X86_XSAVE_STATE_INDEX_SSE,
            mark_present,
        )
    };

    // https://fxbug.dev/42127734: Overwriting the reserved bits of the mxcsr register
    // causes a #GP Fault. We need to check against the mxcsr_mask to see if the
    // proposed mxcsr is valid.
    if let RegAccess::Set(in_) = access
        && !mxcsr_is_valid(in_.mxcsr, save.mxcsr_mask)
    {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }

    match &mut access {
        RegAccess::Get(out) => {
            // SAFETY: `out` was zero-initialized above before acquiring `guard`.
            let out = unsafe { out.assume_init_mut() };
            for (zmm, xmm) in out.zmm[..NUM_SSE_REGS].iter_mut().zip(save.xmm.iter()) {
                zmm.v[0] = xmm.low;
                zmm.v[1] = xmm.high;
            }
            // MXCSR (always present): 32-bit status word.
            out.mxcsr = save.mxcsr;
        }
        RegAccess::Set(in_) => {
            for (xmm, zmm) in save.xmm.iter_mut().zip(in_.zmm[..NUM_SSE_REGS].iter()) {
                xmm.low = zmm.v[0];
                xmm.high = zmm.v[1];
            }
            // MXCSR (always present): 32-bit status word.
            save.mxcsr = in_.mxcsr;
        }
    }

    // AVX grows the registers to 256 bits each. Optional.
    const YMM_HIGH_SIZE: usize = 16; // Additional bytes in each register.
    // SAFETY: `(*arch).extended_register_buffer` is 64-byte aligned and uniquely accessed under
    // `guard`, and `save` from the legacy block above is no longer live.
    let ymm_highbits = unsafe {
        registers::get_extended_register_state_component(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
            X86_XSAVE_STATE_INDEX_AVX,
            mark_present,
        )
    };
    if let Some(ymm_slice) = ymm_highbits {
        match access {
            RegAccess::Get(out) => {
                // SAFETY: `get_extended_register_state_component` returns an initialized slice
                // inside `(*arch).extended_register_buffer`, disjoint from `out`, which was
                // zero-initialized above.
                let (out, ymm_bytes) = unsafe { (out.assume_init_mut(), &*ymm_slice) };
                debug_assert_eq!(ymm_bytes.len(), YMM_HIGH_SIZE * NUM_SSE_REGS);
                let (ymm_words, _) = ymm_bytes.as_chunks::<8>();
                let (ymm_regs, _) = ymm_words.as_chunks::<2>();
                for (zmm, ymm_high) in
                    out.zmm[..NUM_SSE_REGS].iter_mut().zip(&ymm_regs[..NUM_SSE_REGS])
                {
                    zmm.v[2] = u64::from_ne_bytes(ymm_high[0]);
                    zmm.v[3] = u64::from_ne_bytes(ymm_high[1]);
                }
            }
            RegAccess::Set(in_) => {
                // SAFETY: `get_extended_register_state_component` returns a uniquely writable slice
                // inside `(*arch).extended_register_buffer`, disjoint from `in_`.
                let ymm_bytes = unsafe { &mut *ymm_slice };
                debug_assert_eq!(ymm_bytes.len(), YMM_HIGH_SIZE * NUM_SSE_REGS);
                let (ymm_words, _) = ymm_bytes.as_chunks_mut::<8>();
                let (ymm_regs, _) = ymm_words.as_chunks_mut::<2>();
                for (ymm_high, zmm) in
                    ymm_regs[..NUM_SSE_REGS].iter_mut().zip(&in_.zmm[..NUM_SSE_REGS])
                {
                    ymm_high[0] = zmm.v[2].to_ne_bytes();
                    ymm_high[1] = zmm.v[3].to_ne_bytes();
                }
            }
        }
    }

    Ok(())
}

/// Reads the general registers of `thread` into `out` (`arch_get_general_regs`).
///
/// The caller is responsible for making sure the thread is in an exception or is suspended, and
/// stays so. `out` is never overwritten with uninitialized bytes and is fully initialized when
/// `Ok(())` is returned.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` that stays
/// suspended or stopped in an exception for the duration of the call, any non-null
/// `suspended_general_regs` frame of `thread` matches its `general_regs_source` and is valid,
/// initialized, and not mutated during the call, `out` does not overlap `thread`'s state, and no
/// chain lock transaction is active on the calling CPU.
pub unsafe fn get_general_regs(
    thread: *mut Thread,
    out: &mut MaybeUninit<zx_thread_state_general_regs_t>,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_get_general_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    let out_ptr = out.as_mut_ptr();

    // SAFETY: Caller keeps `thread` suspended or stopped in an exception while `guard` holds its
    // lock, `general_regs_source` selects the active union arm of `suspended_general_regs`, and
    // `out_ptr` comes from `&mut MaybeUninit` disjoint from `thread`'s state. Every field of
    // `*out_ptr` is written via raw pointer projection.
    unsafe {
        let arch = thread_arch(thread);
        let gregs = (*arch).suspended_general_regs.gregs;

        // Punt if registers aren't available. E.g.,
        // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
        if gregs.is_null() {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        match (*arch).general_regs_source {
            GeneralRegsSource::Iframe => {
                fill_in_gregs_from_iframe(
                    out_ptr,
                    &*(*arch).suspended_general_regs.iframe.cast::<Iframe>(),
                );
            }
            GeneralRegsSource::Syscall => {
                fill_in_gregs_from_syscall(
                    out_ptr,
                    &*(*arch).suspended_general_regs.syscall.cast::<SyscallRegs>(),
                );
            }
            GeneralRegsSource::None => {
                panic!("unexpected GeneralRegsSource::None");
            }
        }

        (*out_ptr).fs_base = (*arch).fs_base as u64;
        (*out_ptr).gs_base = (*arch).gs_base as u64;
    }

    Ok(())
}

/// Writes `in_` into the general registers of `thread` (`arch_set_general_regs`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` that stays
/// suspended or stopped in an exception for the duration of the call, any non-null
/// `suspended_general_regs` frame of `thread` matches its `general_regs_source` and is valid,
/// initialized, writable, and not otherwise accessed during the call, `in_` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn set_general_regs(
    thread: *mut Thread,
    in_: &zx_thread_state_general_regs_t,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_set_general_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // SAFETY: Caller keeps `thread` suspended or stopped in an exception while `guard` holds its
    // lock, `in_` does not overlap `thread`'s state, and `general_regs_source` selects the active
    // union arm of `suspended_general_regs`.
    unsafe {
        let arch = thread_arch(thread);
        let gregs = (*arch).suspended_general_regs.gregs;

        // Punt if registers aren't available. E.g.,
        // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
        if gregs.is_null() {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        // If these addresses are not canonical, the kernel will GPF when it tries
        // to set them as the current values.
        if !vm::is_vaddr_canonical(in_.fs_base as usize) {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }
        if !vm::is_vaddr_canonical(in_.gs_base as usize) {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // https://fxbug.dev/42127735: Disallow setting RIP to a non-canonical address, to
        // prevent returning to such addresses using the SYSRET or IRETQ
        // instructions. See docs/concepts/kernel/sysret_problem.md.
        //
        // The code also restricts the RIP to userspace addresses. There is no use
        // case for setting the RIP to a kernel address.
        if !vm::is_valid_user_pc(in_.rip as usize) {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        match (*arch).general_regs_source {
            GeneralRegsSource::Iframe => {
                fill_in_iframe_from_gregs(
                    &mut *(*arch).suspended_general_regs.iframe.cast::<Iframe>(),
                    in_,
                );
            }
            GeneralRegsSource::Syscall => {
                fill_in_syscall_from_gregs(
                    &mut *(*arch).suspended_general_regs.syscall.cast::<SyscallRegs>(),
                    in_,
                );
            }
            GeneralRegsSource::None => {
                panic!("unexpected GeneralRegsSource::None");
            }
        }

        (*arch).fs_base = in_.fs_base as usize;
        (*arch).gs_base = in_.gs_base as usize;
    }

    Ok(())
}

/// Reads the single-step flag (`RFLAGS.TF`) of `thread` into `out` (`arch_get_single_step`).
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` that stays
/// suspended or stopped in an exception for the duration of the call, any non-null
/// `suspended_general_regs` frame of `thread` matches its `general_regs_source` and is valid,
/// initialized, and not mutated during the call, `out` does not overlap `thread`'s state, and no
/// chain lock transaction is active on the calling CPU.
pub unsafe fn get_single_step(
    thread: *mut Thread,
    out: &mut MaybeUninit<zx_thread_state_single_step_t>,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_get_single_step")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // SAFETY: Caller keeps `thread` suspended or stopped in an exception while `guard` holds its
    // lock, and `general_regs_source` selects the active union arm of `suspended_general_regs`.
    let flags = unsafe {
        let arch = thread_arch(thread);

        // Punt if registers aren't available. E.g.,
        // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
        if (*arch).suspended_general_regs.gregs.is_null() {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        match (*arch).general_regs_source {
            GeneralRegsSource::Iframe => {
                (*(*arch).suspended_general_regs.iframe.cast::<Iframe>()).flags
            }
            GeneralRegsSource::Syscall => {
                (*(*arch).suspended_general_regs.syscall.cast::<SyscallRegs>()).rflags
            }
            GeneralRegsSource::None => {
                panic!("unexpected GeneralRegsSource::None");
            }
        }
    };

    out.write(u32::from((flags & X86_FLAGS_TF) != 0));
    Ok(())
}

/// Sets the single-step flag (`RFLAGS.TF`) of `thread` from `in_` (`arch_set_single_step`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` that stays
/// suspended or stopped in an exception for the duration of the call, any non-null
/// `suspended_general_regs` frame of `thread` matches its `general_regs_source` and is valid,
/// initialized, writable, and not otherwise accessed during the call, `in_` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn set_single_step(
    thread: *mut Thread,
    in_: &zx_thread_state_single_step_t,
) -> Result<(), Status> {
    let single_step = *in_;
    if single_step != 0 && single_step != 1 {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }

    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_set_single_step")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // SAFETY: Caller keeps `thread` suspended or stopped in an exception while `guard` holds its
    // lock, and `general_regs_source` selects the active union arm of `suspended_general_regs`.
    let flags = unsafe {
        let arch = thread_arch(thread);

        // Punt if registers aren't available. E.g.,
        // TODO(https://fxbug.dev/42105394): Registers aren't available in synthetic exceptions.
        if (*arch).suspended_general_regs.gregs.is_null() {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        match (*arch).general_regs_source {
            GeneralRegsSource::Iframe => {
                &mut (*(*arch).suspended_general_regs.iframe.cast::<Iframe>()).flags
            }
            GeneralRegsSource::Syscall => {
                &mut (*(*arch).suspended_general_regs.syscall.cast::<SyscallRegs>()).rflags
            }
            GeneralRegsSource::None => {
                panic!("unexpected GeneralRegsSource::None");
            }
        }
    };

    if single_step != 0 {
        *flags |= X86_FLAGS_TF;
    } else {
        *flags &= !X86_FLAGS_TF;
    }
    Ok(())
}

/// Reads the x87 floating-point registers of `thread` into `out` (`arch_get_fp_regs`).
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0,
/// `thread` points to a live (not freed), valid `Thread` that stays suspended or stopped in an
/// exception for the duration of the call, whose `extended_register_buffer` holds a valid extended
/// register state image (see [`registers::get_xsave_legacy_area`]), `out` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn get_fp_regs(
    thread: *mut Thread,
    out: &mut MaybeUninit<zx_thread_state_fp_regs_t>,
) -> Result<(), Status> {
    // Don't leak any reserved fields.
    // SAFETY: `out` is a valid mutable `MaybeUninit<zx_thread_state_fp_regs_t>`, which is a C-repr
    // POD struct valid when all-zero.
    let out = unsafe {
        out.as_mut_ptr().write_bytes(0, 1);
        out.assume_init_mut()
    };

    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_get_fp_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0 and keeps `thread`
    // suspended or stopped in an exception while `guard` holds its lock, giving exclusive access to
    // the 64-byte-aligned `(*arch).extended_register_buffer`, which holds a valid extended register
    // state image and is disjoint from `out`. `X86_XSAVE_STATE_INDEX_X87` is `0`.
    let save = unsafe {
        let arch = thread_arch(thread);
        &*registers::get_xsave_legacy_area(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
            X86_XSAVE_STATE_INDEX_X87,
            false,
        )
    };

    out.fcw = save.fcw;
    out.fsw = save.fsw;
    out.ftw = save.ftw;
    out.fop = save.fop;
    out.fip = save.fip;
    out.fdp = save.fdp;
    for i in 0..out.st.len() {
        out.st[i].low = save.st[i].low;
        out.st[i].high = save.st[i].high;
    }

    Ok(())
}

/// Writes `in_` into the x87 floating-point registers of `thread` (`arch_set_fp_regs`).
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0,
/// `thread` points to a live (not freed), valid `Thread` that stays suspended or stopped in an
/// exception for the duration of the call, whose `extended_register_buffer` holds a valid extended
/// register state image (see [`registers::get_xsave_legacy_area`]), `in_` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn set_fp_regs(
    thread: *mut Thread,
    in_: &zx_thread_state_fp_regs_t,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_set_fp_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0 and keeps `thread`
    // suspended or stopped in an exception while `guard` holds its lock, giving exclusive access to
    // the 64-byte-aligned `(*arch).extended_register_buffer`, which holds a valid extended register
    // state image and is disjoint from `in_`. `X86_XSAVE_STATE_INDEX_X87` is `0`.
    let save = unsafe {
        let arch = thread_arch(thread);
        &mut *registers::get_xsave_legacy_area(
            (&raw mut (*arch).extended_register_buffer).cast::<u8>(),
            X86_XSAVE_STATE_INDEX_X87,
            true,
        )
    };

    save.fcw = in_.fcw;
    save.fsw = in_.fsw;
    save.ftw = in_.ftw;
    save.fop = in_.fop;
    save.fip = in_.fip;
    save.fdp = in_.fdp;
    for i in 0..save.st.len() {
        save.st[i].low = in_.st[i].low;
        save.st[i].high = in_.st[i].high;
    }

    Ok(())
}

/// Reads the SSE/AVX vector registers of `thread` into `out` (`arch_get_vector_regs`).
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0,
/// `thread` points to a live (not freed), valid `Thread` that stays suspended or stopped in an
/// exception for the duration of the call, whose `extended_register_buffer` holds a valid extended
/// register state image (see [`registers::get_xsave_legacy_area`]), `out` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn get_vector_regs(
    thread: *mut Thread,
    out: &mut MaybeUninit<zx_thread_state_vector_regs_t>,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `get_set_vector_regs` preconditions.
    unsafe { get_set_vector_regs(thread, RegAccess::Get(out)) }
}

/// Writes `in_` into the SSE/AVX vector registers of `thread` (`arch_set_vector_regs`).
///
/// # Safety
///
/// The caller must ensure that [`registers::extended_register_init`] has completed on CPU 0,
/// `thread` points to a live (not freed), valid `Thread` that stays suspended or stopped in an
/// exception for the duration of the call, whose `extended_register_buffer` holds a valid extended
/// register state image (see [`registers::get_xsave_legacy_area`]), `in_` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn set_vector_regs(
    thread: *mut Thread,
    in_: &zx_thread_state_vector_regs_t,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `get_set_vector_regs` preconditions.
    unsafe { get_set_vector_regs(thread, RegAccess::Set(in_)) }
}

/// Reads the hardware debug registers of `thread` into `out` (`arch_get_debug_regs`).
///
/// `out` is never overwritten with uninitialized bytes and is fully initialized when `Ok(())` is
/// returned.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a live (not freed), valid `Thread` that stays
/// suspended or stopped in an exception for the duration of the call, `out` does not overlap
/// `thread`'s state, and no chain lock transaction is active on the calling CPU.
pub unsafe fn get_debug_regs(
    thread: *mut Thread,
    out: &mut MaybeUninit<zx_thread_state_debug_regs_t>,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_get_debug_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // The kernel updates this per-thread data every time a hw debug event occurs, meaning that
    // these values will be always up to date. If the thread is not using hw debug capabilities,
    // these will have the default zero values.
    // SAFETY: Caller keeps `thread` suspended or stopped in an exception while `guard` holds its
    // lock.
    let debug_state = unsafe { &(*thread_arch(thread)).debug_state };
    out.write(zx_thread_state_debug_regs_t {
        dr: [debug_state.dr[0], debug_state.dr[1], debug_state.dr[2], debug_state.dr[3]],
        dr6: debug_state.dr6,
        dr7: debug_state.dr7,
    });

    Ok(())
}

/// Validates and writes `in_` into the hardware debug registers of `thread`
/// (`arch_set_debug_regs`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `DR7.GD == 0`, `thread` points to a live (not freed),
/// valid `Thread` that stays suspended or stopped in an exception for the duration of the call,
/// `in_` does not overlap `thread`'s state, and no chain lock transaction is active on the calling
/// CPU.
pub unsafe fn set_debug_regs(
    thread: *mut Thread,
    in_: &zx_thread_state_debug_regs_t,
) -> Result<(), Status> {
    thread_lock_guard!(
        guard,
        // SAFETY: Caller guarantees `thread` is a live `Thread` past this scope, no chain lock
        // transaction is active, and we do not block.
        unsafe { ThreadLockGuard::target(thread, clt_tag!("arch_set_debug_regs")) }
    );

    debug_assert!(guard.is_user_state_saved_locked());

    // Replace the state of the thread with the given one. We now need to keep track of the debug
    // state of this register across context switches.
    let mut new_debug_state = X86DebugState {
        dr: [in_.dr[0], in_.dr[1], in_.dr[2], in_.dr[3]],
        dr6: in_.dr6,
        dr7: in_.dr7,
    };

    // Validate the new input. This will mask reserved bits to their stated values.
    if !registers::validate_debug_state(&mut new_debug_state) {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }

    // NOTE: This currently does a write-read round-trip to the CPU in order to ensure that
    //       `(*arch).debug_state` tracks the exact value as it is stored in the registers.
    // TODO(https://fxbug.dev/42108006): Ideally, we could do some querying at boot time about the
    // format that the CPU is storing reserved bits and we can create a mask we can apply to the
    // input values and avoid changing the state.

    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0` while `guard` holds `thread`'s lock with
    // interrupts disabled (preventing context switches during the hardware DR0..DR7 round-trip) and
    // keeps `thread` suspended or stopped in an exception.
    unsafe {
        let arch = thread_arch(thread);

        // Save the current debug state temporarily.
        let current_debug_state = registers::read_hw_debug_regs();

        // Write and then read from the CPU to have real values tracked by the thread data.
        // Mark the thread as now tracking the debug state.
        registers::write_hw_debug_regs(&new_debug_state);
        (*arch).debug_state = registers::read_hw_debug_regs();

        (*arch).track_debug_state = true;

        // Restore the original debug state. Should always work as the input was already validated.
        registers::write_hw_debug_regs(&current_debug_state);
    }

    Ok(())
}

/// Returns the instruction pointer from `gregs` according to `source`
/// (`arch_get_instruction_pointer`).
///
/// # Safety
///
/// The caller must ensure that when `source` is [`GeneralRegsSource::Iframe`] (respectively
/// [`GeneralRegsSource::Syscall`]), `gregs` is non-null, aligned for, and points to a valid,
/// initialized [`Iframe`] (respectively [`SyscallRegs`]) that is not mutated during the call.
#[inline(always)]
pub unsafe fn get_instruction_pointer(source: GeneralRegsSource, gregs: *const c_void) -> usize {
    match source {
        GeneralRegsSource::Iframe => {
            // SAFETY: Caller guarantees `gregs` points to a valid `Iframe`.
            unsafe { (*gregs.cast::<Iframe>()).ip as usize }
        }
        GeneralRegsSource::Syscall => {
            // SAFETY: Caller guarantees `gregs` points to a valid `SyscallRegs`.
            unsafe { (*gregs.cast::<SyscallRegs>()).rip as usize }
        }
        GeneralRegsSource::None => 0,
    }
}

/// Sets the return instruction pointer in `gregs` to `ip` (`arch_set_return_instruction_pointer`).
///
/// # Safety
///
/// The caller must ensure that when `source` is [`GeneralRegsSource::Iframe`] (respectively
/// [`GeneralRegsSource::Syscall`]), `gregs` is non-null, aligned for, and points to a valid,
/// uniquely writable [`Iframe`] (respectively [`SyscallRegs`]) for the duration of the call.
#[inline(always)]
pub unsafe fn set_return_instruction_pointer(
    source: GeneralRegsSource,
    gregs: *mut c_void,
    ip: usize,
) -> Result<(), Status> {
    if !vm::is_ip_canonical(ip) {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }
    match source {
        GeneralRegsSource::Iframe => {
            // SAFETY: Caller guarantees `gregs` points to a valid mutable `Iframe`.
            unsafe { (*gregs.cast::<Iframe>()).ip = ip as u64 };
        }
        GeneralRegsSource::Syscall => {
            // SAFETY: Caller guarantees `gregs` points to a valid mutable `SyscallRegs`.
            unsafe { (*gregs.cast::<SyscallRegs>()).rip = ip as u64 };
        }
        GeneralRegsSource::None => {}
    }
    Ok(())
}

/// Returns the number of hardware breakpoint registers on x86 (`arch_get_hw_breakpoint_count`).
///
/// NOTE: While x86 supports up to 4 hw breakpoints/watchpoints, there is a catch:
///       They are shared, so `(breakpoints + watchpoints) <= HW_DEBUG_REGISTERS_COUNT`.
#[inline(always)]
pub const fn get_hw_breakpoint_count() -> u8 {
    HW_DEBUG_REGISTERS_COUNT as u8
}

/// Returns the number of hardware watchpoint registers on x86 (`arch_get_hw_watchpoint_count`).
#[inline(always)]
pub const fn get_hw_watchpoint_count() -> u8 {
    HW_DEBUG_REGISTERS_COUNT as u8
}

/// Unit tests for x86 debugger register helpers.
#[cfg(ktest)]
#[unittest::suite(name = "x86_debugger_tests")]
mod tests {
    use super::registers::{X86_FLAGS_CF, X86_FLAGS_IF, X86_FLAGS_IOPL_MASK};
    use unittest::{expect_eq, expect_err, expect_false, expect_ok, expect_true};

    /// Tests `mxcsr_is_valid` against explicit and zero masks.
    #[test]
    fn test_mxcsr_is_valid() {
        expect_true!(mxcsr_is_valid(0x1f80, 0));
        expect_true!(mxcsr_is_valid(0x1f80, 0x0000_ffbf));
        expect_false!(mxcsr_is_valid(0x0000_0040, 0));
        expect_false!(mxcsr_is_valid(0x0001_0000, 0x0000_ffbf));
    }

    /// Tests general register copy helpers and `X86_FLAGS_USER` masking.
    #[test]
    fn test_gregs_iframe_and_syscall_conversion() {
        let mut iframe = Iframe {
            rax: 1,
            rbx: 2,
            rcx: 3,
            rdx: 4,
            rsi: 5,
            rdi: 6,
            rbp: 7,
            r8: 8,
            r9: 9,
            r10: 10,
            r11: 11,
            r12: 12,
            r13: 13,
            r14: 14,
            r15: 15,
            ip: 0x2000,
            user_sp: 0x3000,
            flags: X86_FLAGS_IF | X86_FLAGS_CF,
            ..Iframe::default()
        };
        let mut gregs = zx_thread_state_general_regs_t::default();
        // SAFETY: `gregs` is a valid, aligned local non-overlapping with `iframe`.
        unsafe { fill_in_gregs_from_iframe(&raw mut gregs, &iframe) };
        expect_eq!(gregs.rax, 1);
        expect_eq!(gregs.r15, 15);
        expect_eq!(gregs.rip, 0x2000);
        expect_eq!(gregs.rsp, 0x3000);
        expect_eq!(gregs.rflags, X86_FLAGS_IF | X86_FLAGS_CF);

        // Attempt to clear IF and set IOPL via user general regs; privileged bits must be
        // preserved from `iframe.flags` while user flags update.
        gregs.rflags = X86_FLAGS_IOPL_MASK | X86_FLAGS_TF;
        gregs.rip = 0x4000;
        gregs.rsp = 0x5000;
        fill_in_iframe_from_gregs(&mut iframe, &gregs);
        expect_eq!(iframe.ip, 0x4000);
        expect_eq!(iframe.user_sp, 0x5000);
        expect_eq!(iframe.flags, X86_FLAGS_IF | X86_FLAGS_TF);

        let mut syscall = SyscallRegs {
            rax: 10,
            r15: 25,
            rip: 0x6000,
            rsp: 0x7000,
            rflags: X86_FLAGS_IF,
            ..SyscallRegs::default()
        };
        // SAFETY: `gregs` is a valid, aligned local non-overlapping with `syscall`.
        unsafe { fill_in_gregs_from_syscall(&raw mut gregs, &syscall) };
        expect_eq!(gregs.rax, 10);
        expect_eq!(gregs.r15, 25);
        expect_eq!(gregs.rip, 0x6000);
        expect_eq!(gregs.rsp, 0x7000);

        gregs.rflags = X86_FLAGS_IOPL_MASK | X86_FLAGS_CF;
        fill_in_syscall_from_gregs(&mut syscall, &gregs);
        expect_eq!(syscall.rflags, X86_FLAGS_IF | X86_FLAGS_CF);
    }

    /// Tests `get_instruction_pointer` and `set_return_instruction_pointer` across all variants.
    #[test]
    fn test_instruction_pointer_accessors() {
        let mut iframe = Iframe { ip: 0x1234, ..Iframe::default() };
        let iframe_ptr = (&raw mut iframe).cast::<c_void>();
        // SAFETY: `iframe_ptr` points to a valid mutable `Iframe`.
        unsafe {
            expect_eq!(get_instruction_pointer(GeneralRegsSource::Iframe, iframe_ptr), 0x1234);
            expect_ok!(set_return_instruction_pointer(
                GeneralRegsSource::Iframe,
                iframe_ptr,
                0x5678,
            ));
            expect_eq!(get_instruction_pointer(GeneralRegsSource::Iframe, iframe_ptr), 0x5678);
            let non_canon_res = set_return_instruction_pointer(
                GeneralRegsSource::Iframe,
                iframe_ptr,
                0x0001_0000_0000_0000,
            );
            expect_err!(non_canon_res, Status::INVALID_ARGS);
            let kernel_ip_res = set_return_instruction_pointer(
                GeneralRegsSource::Iframe,
                iframe_ptr,
                0xffff_ff80_0000_0000,
            );
            expect_err!(kernel_ip_res, Status::INVALID_ARGS);
        }

        let mut syscall = SyscallRegs { rip: 0x2468, ..SyscallRegs::default() };
        let syscall_ptr = (&raw mut syscall).cast::<c_void>();
        // SAFETY: `syscall_ptr` points to a valid mutable `SyscallRegs`.
        unsafe {
            expect_eq!(get_instruction_pointer(GeneralRegsSource::Syscall, syscall_ptr), 0x2468);
            expect_ok!(set_return_instruction_pointer(
                GeneralRegsSource::Syscall,
                syscall_ptr,
                0x9abc,
            ));
            expect_eq!(get_instruction_pointer(GeneralRegsSource::Syscall, syscall_ptr), 0x9abc);
            let non_canon_res = set_return_instruction_pointer(
                GeneralRegsSource::Syscall,
                syscall_ptr,
                0x0001_0000_0000_0000,
            );
            expect_err!(non_canon_res, Status::INVALID_ARGS);
            let kernel_ip_res = set_return_instruction_pointer(
                GeneralRegsSource::Syscall,
                syscall_ptr,
                0xffff_ff80_0000_0000,
            );
            expect_err!(kernel_ip_res, Status::INVALID_ARGS);
        }

        // SAFETY: `GeneralRegsSource::None` does not dereference `gregs`.
        unsafe {
            let none_ip = get_instruction_pointer(GeneralRegsSource::None, core::ptr::null_mut());
            expect_eq!(none_ip, 0);
            expect_ok!(set_return_instruction_pointer(
                GeneralRegsSource::None,
                core::ptr::null_mut(),
                0x1000,
            ));
        }

        expect_eq!(get_hw_breakpoint_count(), 4);
        expect_eq!(get_hw_watchpoint_count(), 4);
    }
}
