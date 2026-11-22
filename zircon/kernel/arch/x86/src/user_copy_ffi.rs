// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

use zx_status::Status;
use zx_types::zx_status_t;

use super::user_copy::{self, X64CopyToFromUserRet};
use crate::arch_rs::UserCopyCaptureFaultsError;

unsafe extern "C" {
    /// Assembly user-copy helper (`_x86_copy_to_or_from_user` in `user-copy.S`).
    ///
    /// It should not be called anywhere except in the x86 usercopy implementation. If
    /// `X86_USER_COPY_CAPTURE_FAULTS` is passed as `fault_return_mask` then the returned struct
    /// will have `pf_flags` and `pf_va` filled out on pagefault, otherwise they should be ignored.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the user address range has been validated to lie within user
    /// space, `dst` is valid for writes of `len` bytes when copying from user or `src` is valid
    /// for reads of `len` bytes when copying to user (without concurrent conflicting accesses), and
    /// `fault_return` points to the current thread's `ArchThread::page_fault_resume` field.
    pub fn _x86_copy_to_or_from_user(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> X64CopyToFromUserRet;
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`
/// (`arch_copy_from_user`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized with a valid `current_thread`, blocking
/// is allowed, no spinlocks or tracked locks are held, and `dst` points to at least `len` bytes of
/// writable kernel memory without concurrent conflicting accesses.
// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_copy_from_user(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees valid kernel `dst`, initialized `%gs.base` with a valid
    // `current_thread`, and blocking context.
    Status::result_into_raw(unsafe { user_copy::arch_copy_from_user(dst, src, len) })
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst` (`arch_copy_to_user`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized with a valid `current_thread`, blocking
/// is allowed, no spinlocks or tracked locks are held, and `src` points to at least `len` bytes of
/// initialized readable kernel memory without concurrent conflicting mutations.
// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_copy_to_user(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees valid kernel `src`, initialized `%gs.base` with a valid
    // `current_thread`, and blocking context.
    Status::result_into_raw(unsafe { user_copy::arch_copy_to_user(dst, src, len) })
}

/// Copies `len` bytes from user memory at `src` into kernel memory at `dst`, capturing any page
/// faults (`arch_copy_from_user_capture_faults`).
///
/// Returns `ZX_ERR_OUT_OF_RANGE` in `status` if `src`/`len` is outside user space so the C++
/// wrapper can distinguish range errors (no `FaultInfo`) from page faults (`Some(FaultInfo)`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized with a valid `current_thread` and `dst`
/// points to at least `len` bytes of writable kernel memory without concurrent conflicting
/// accesses.
#[inline(always)]
fn capture_faults_to_ffi(result: Result<(), UserCopyCaptureFaultsError>) -> X64CopyToFromUserRet {
    match result {
        Ok(()) => X64CopyToFromUserRet { status: zx_types::ZX_OK, pf_flags: 0, pf_va: 0 },
        Err(UserCopyCaptureFaultsError { status: _, fault_info: None }) => {
            X64CopyToFromUserRet { status: Status::OUT_OF_RANGE.into_raw(), pf_flags: 0, pf_va: 0 }
        }
        Err(UserCopyCaptureFaultsError { status, fault_info: Some(fault) }) => {
            X64CopyToFromUserRet {
                status: status.into_raw(),
                pf_flags: fault.pf_flags,
                pf_va: fault.pf_va,
            }
        }
    }
}

// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_copy_from_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> X64CopyToFromUserRet {
    // SAFETY: Caller guarantees valid kernel `dst` and initialized `%gs.base` with a valid
    // `current_thread`.
    capture_faults_to_ffi(unsafe {
        user_copy::arch_copy_from_user_capture_faults(
            dst,
            src,
            len,
            crate::arch_rs::CopyContext::BlockingNotAllowed,
        )
    })
}

/// Copies `len` bytes from kernel memory at `src` into user memory at `dst`, capturing any page
/// faults (`arch_copy_to_user_capture_faults`).
///
/// Returns `ZX_ERR_OUT_OF_RANGE` in `status` if `dst`/`len` is outside user space so the C++
/// wrapper can distinguish range errors (no `FaultInfo`) from page faults (`Some(FaultInfo)`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized with a valid `current_thread` and `src`
/// points to at least `len` bytes of initialized readable kernel memory without concurrent
/// conflicting mutations.
// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_copy_to_user_capture_faults(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
) -> X64CopyToFromUserRet {
    // SAFETY: Caller guarantees valid kernel `src` and initialized `%gs.base` with a valid
    // `current_thread`.
    capture_faults_to_ffi(unsafe {
        user_copy::arch_copy_to_user_capture_faults(
            dst,
            src,
            len,
            crate::arch_rs::CopyContext::BlockingNotAllowed,
        )
    })
}
