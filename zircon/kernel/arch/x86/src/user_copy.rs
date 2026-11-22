// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::mem::{align_of, size_of};

use super::feature::x86_feature_has_smap;
use super::registers::X86_FLAGS_AC;
use super::thread::{X86_PFR_RUN_FAULT_HANDLER_BIT, thread_arch};
use super::user_copy_ffi::_x86_copy_to_or_from_user;
use super::vm::is_user_accessible_range;
use super::x86::x86_save_flags;
use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{FaultInfo, UserCopyCaptureFaultsError, blocking_disallowed};
use crate::kernel::thread::current_get;
use debug::ltracef;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

/// Mask applied to `page_fault_resume` when faults should be captured without running the fault
/// handler (`X86_USER_COPY_CAPTURE_FAULTS`).
const X86_USER_COPY_CAPTURE_FAULTS: u64 = !(1u64 << X86_PFR_RUN_FAULT_HANDLER_BIT);

/// Mask applied to `page_fault_resume` when faults should invoke the page fault handler first
/// (`X86_USER_COPY_DO_FAULTS`).
const X86_USER_COPY_DO_FAULTS: u64 = !0u64;

pub use x86_user_copy_bindings::X64CopyToFromUserRet;

zr::static_assert!(size_of::<X64CopyToFromUserRet>() == 16);
zr::static_assert!(align_of::<X64CopyToFromUserRet>() == 8);
zr::static_assert!(core::mem::offset_of!(X64CopyToFromUserRet, status) == 0);
zr::static_assert!(core::mem::offset_of!(X64CopyToFromUserRet, pf_flags) == 4);
zr::static_assert!(core::mem::offset_of!(X64CopyToFromUserRet, pf_va) == 8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyDirection {
    ToUser,
    FromUser,
}

#[inline]
fn ac_flag() -> bool {
    (x86_save_flags() & X86_FLAGS_AC) != 0
}

#[inline(always)]
fn can_access(base: *const c_void, len: usize) -> bool {
    ltracef!("can_access: base {:p}, len {}\n", base, len);

    // We don't care about whether pages are actually mapped or what their
    // permissions are, as long as they are in the user address space.  We
    // rely on a page fault occurring if an actual permissions error occurs.
    is_user_accessible_range(base.addr(), len)
}

/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized and points to the current CPU's
/// `x86_percpu` with a valid `current_thread`, and that the kernel-side buffer (`dst` for
/// `CopyDirection::FromUser`, `src` for `CopyDirection::ToUser`) is valid for writes or reads of
/// `len` initialized bytes without concurrent conflicting accesses. When
/// `FAULT_RETURN_MASK == X86_USER_COPY_DO_FAULTS`, blocking must be allowed with no spinlocks held.
#[inline(always)]
unsafe fn copy_to_from_user<const FAULT_RETURN_MASK: u64>(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    direction: CopyDirection,
) -> Result<(), UserCopyCaptureFaultsError> {
    // There are exactly two versions of this function which may be expanded.
    // Anything else would be an error which should be caught at compile time.
    const {
        assert!(
            (FAULT_RETURN_MASK == X86_USER_COPY_DO_FAULTS)
                || (FAULT_RETURN_MASK == X86_USER_COPY_CAPTURE_FAULTS),
            "arch_copy_(to|from)_user routines must either capture faults, or simply take the \
             fault but return no details.",
        );
    }

    // If we have the SMAP feature, then AC should only be set when running
    // _x86_copy_to_or_from_user. If we don't have the SMAP feature, then we don't care if AC is set
    // or not.
    debug_assert!(!x86_feature_has_smap() || !ac_flag());

    // Check to make sure that our user space address exists entirely within the
    // possible user space address range.  If not, then we are not going to make
    // _any_ attempt to copy the data at all.  If this direction of this copy is
    // ToUser, then our "user" address to test is the destination address.
    // Otherwise, it is the source address.
    //
    // In either case, if we are not going to even try, then there is no fault
    // address or flags to propagate, just a failed status code.
    if !can_access(if direction == CopyDirection::ToUser { dst.cast_const() } else { src }, len) {
        core::hint::cold_path();
        return Err(UserCopyCaptureFaultsError { status: Status::INVALID_ARGS, fault_info: None });
    }

    // Spectre V1 (CopyDirection::FromUser) - force resolution of can_access() before attempting to
    // copy from a user-controlled pointer to a kernel address.  A poisoned conditional branch
    // predictor can be used to force the kernel to read any kernel address (speculatively).
    //
    // Spectre V1.1 (CopyDirection::ToUser) -  force resolution of can_access() before attempting to
    // speculatively store a value into a user-controlled kernel address. This can be chained with
    // other attacks to overwrite data such as stack return addresses or function pointers in the
    // speculative window.
    // SAFETY: `lfence` is a serializing instruction with no operands; omit `nomem` so it acts as a
    // compiler memory barrier as well.
    unsafe {
        core::arch::asm!("lfence", options(nostack, preserves_flags));
    }

    let thread = current_get();
    // SAFETY:
    // - `thread_arch(thread)`: Caller guarantees `%gs.base` is initialized with a valid
    //   `current_thread`, so `thread` points to the current CPU's live `Thread`.
    // - `&raw mut (*arch).page_fault_resume`: `arch` points to `thread`'s embedded `ArchThread`;
    //   forming a raw pointer to `page_fault_resume` does not create an intermediate reference.
    // - `_x86_copy_to_or_from_user`: The user range was validated by `can_access` (serialized by
    //   `lfence`), the caller guarantees the kernel buffer is valid for reads/writes of `len`
    //   bytes, and `fault_return` points to the current thread's `page_fault_resume` field.
    //   The `const { assert!(...) }` block guarantees `FAULT_RETURN_MASK` is valid, and SMAP `AC`
    //   is clear on entry.
    let ret = unsafe {
        let arch = thread_arch(thread);
        _x86_copy_to_or_from_user(
            dst,
            src,
            len,
            &raw mut (*arch).page_fault_resume,
            FAULT_RETURN_MASK,
        )
    };
    debug_assert!(!x86_feature_has_smap() || !ac_flag());

    // In the DO_FAULTS version of this expansion, do not make any attempt to
    // propagate the fault address and flags.  We only propagate fault info in the
    // CAPTURE_FAULTS version, and then only if we actually take a fault, not if
    // we succeed.
    let Err(status) = Status::ok(ret.status) else {
        return Ok(());
    };
    core::hint::cold_path();
    if FAULT_RETURN_MASK == X86_USER_COPY_DO_FAULTS {
        Err(UserCopyCaptureFaultsError::from(status))
    } else {
        Err(UserCopyCaptureFaultsError {
            status,
            fault_info: Some(FaultInfo { pf_va: ret.pf_va, pf_flags: ret.pf_flags }),
        })
    }
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`
/// (`arch_copy_from_user`).
///
/// The user address range `[src, src + len)` is validated at runtime against user space bounds,
/// and page faults on user memory are handled or returned as an error status.
///
/// # Panics
///
/// Panics if any `lockdep`-tracked locks are held.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized and points to the current CPU's
/// `x86_percpu` with a valid `current_thread`, blocking is allowed, no spinlocks are held,
/// and `dst` is valid for writes of `len` bytes in kernel memory without concurrent
/// conflicting accesses.
#[inline]
pub unsafe fn arch_copy_from_user(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> Result<(), Status> {
    debug_assert!(!blocking_disallowed());
    debug_assert!(num_spinlocks_held() == 0);
    // The copy might trigger a page fault that needs to block on a user pager request, in which
    // case it is not permitted to be holding locks. To ensure this doesn't accidentally work if a
    // copy does not happen to trigger a pager, we insist that all copies do not hold locks. This
    // method is an empty inline function if lockdep is not enabled.
    lockdep::assert_no_locks_held();

    // The DO_FAULTS version of the copy routine will never return fault information, so it's
    // safe to discard the `fault_info` by mapping the error back to its `status`.
    // SAFETY: The caller guarantees `%gs.base` is initialized with a valid `current_thread`,
    // the kernel-side buffer `dst` is valid for writes of `len` initialized bytes without
    // concurrent conflicting accesses, and blocking is allowed with no spinlocks held.
    unsafe {
        copy_to_from_user::<X86_USER_COPY_DO_FAULTS>(dst, src, len, CopyDirection::FromUser)
            .map_err(|e| e.status)
    }
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`, capturing any page
/// faults (`arch_copy_from_user_capture_faults`).
///
/// The user address range `[src, src + len)` is validated at runtime against user space bounds,
/// and page faults on user memory are captured and returned.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized and points to the current CPU's
/// `x86_percpu` with a valid `current_thread`, and `dst` is valid for writes of `len` bytes
/// in kernel memory without concurrent conflicting accesses.
#[inline]
pub unsafe fn arch_copy_from_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: crate::arch_rs::CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    debug_assert!(
        crate::arch_rs::ops::num_spinlocks_held() == 0
            || context == crate::arch_rs::CopyContext::BlockingNotAllowed
    );
    // SAFETY: The caller guarantees `%gs.base` is initialized with a valid `current_thread`,
    // and the kernel-side buffer `dst` is valid for writes of `len` initialized bytes without
    // concurrent conflicting accesses.
    unsafe {
        copy_to_from_user::<X86_USER_COPY_CAPTURE_FAULTS>(dst, src, len, CopyDirection::FromUser)
    }
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst` (`arch_copy_to_user`).
///
/// The user address range `[dst, dst + len)` is validated at runtime against user space bounds,
/// and page faults on user memory are handled or returned as an error status.
///
/// # Panics
///
/// Panics if any `lockdep`-tracked locks are held.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized and points to the current CPU's
/// `x86_percpu` with a valid `current_thread`, blocking is allowed, no spinlocks are held,
/// and `src` is valid for reads of `len` initialized bytes in kernel memory
/// without concurrent conflicting mutations.
#[inline]
pub unsafe fn arch_copy_to_user(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> Result<(), Status> {
    debug_assert!(!blocking_disallowed());
    debug_assert!(num_spinlocks_held() == 0);
    // See comment above.
    lockdep::assert_no_locks_held();

    // SAFETY: The caller guarantees `%gs.base` is initialized with a valid `current_thread`,
    // the kernel-side buffer `src` is valid for reads of `len` initialized bytes without
    // concurrent conflicting accesses, and blocking is allowed with no spinlocks held.
    unsafe {
        copy_to_from_user::<X86_USER_COPY_DO_FAULTS>(dst, src, len, CopyDirection::ToUser)
            .map_err(|e| e.status)
    }
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`, capturing any page
/// faults (`arch_copy_to_user_capture_faults`).
///
/// The user address range `[dst, dst + len)` is validated at runtime against user space bounds,
/// and page faults on user memory are captured and returned.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized and points to the current CPU's
/// `x86_percpu` with a valid `current_thread`, and `src` is valid for reads of `len` initialized
/// bytes in kernel memory without concurrent conflicting mutations.
#[inline]
pub unsafe fn arch_copy_to_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    context: crate::arch_rs::CopyContext,
) -> Result<(), UserCopyCaptureFaultsError> {
    debug_assert!(
        crate::arch_rs::ops::num_spinlocks_held() == 0
            || context == crate::arch_rs::CopyContext::BlockingNotAllowed
    );
    // SAFETY: The caller guarantees `%gs.base` is initialized with a valid `current_thread`,
    // and the kernel-side buffer `src` is valid for reads of `len` initialized bytes without
    // concurrent conflicting accesses.
    unsafe {
        copy_to_from_user::<X86_USER_COPY_CAPTURE_FAULTS>(dst, src, len, CopyDirection::ToUser)
    }
}
