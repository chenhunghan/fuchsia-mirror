// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_uint, c_void};
use core::mem::{align_of, offset_of, size_of};

use zx_types::{zx_status_t, zx_vaddr_t};

#[repr(C)]
struct X64CopyToFromUserRet {
    status: zx_status_t,
    pf_flags: c_uint,
    pf_va: zx_vaddr_t,
}

zr::static_assert!(size_of::<X64CopyToFromUserRet>() == 16);
zr::static_assert!(align_of::<X64CopyToFromUserRet>() == 8);
zr::static_assert!(offset_of!(X64CopyToFromUserRet, status) == 0);
zr::static_assert!(offset_of!(X64CopyToFromUserRet, pf_flags) == 4);
zr::static_assert!(offset_of!(X64CopyToFromUserRet, pf_va) == 8);

type UserCopyFn = unsafe extern "C" fn(
    dst: *mut c_void,
    src: *const c_void,
    len: usize,
    fault_return: *mut u64,
    fault_return_mask: u64,
) -> X64CopyToFromUserRet;

unsafe extern "C" {
    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes, `src` must be valid for reads of `len`
    /// bytes, the two regions must not overlap, and `fault_return` must be a valid aligned pointer
    /// to a mutable `u64`.
    fn _x86_copy_to_or_from_user_movsb(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> X64CopyToFromUserRet;

    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes, `src` must be valid for reads of `len`
    /// bytes, the two regions must not overlap, and `fault_return` must be a valid aligned pointer
    /// to a mutable `u64`.
    fn _x86_copy_to_or_from_user_movsq(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> X64CopyToFromUserRet;
}

/// # Safety
///
/// `copy_fn` must copy `len` bytes from `src` to `dst`, write the temporary fault return address
/// to `*fault_return` and reset it to `0` before returning, and return `ZX_OK` in `status` when
/// `dst` and `src` are valid non-overlapping buffers of length `len` and `fault_return` is a valid
/// mutable `u64`.
unsafe fn test_user_copy(copy_fn: UserCopyFn) {
    for i in 1..40usize {
        let mut dst = vec![0u8; i];
        let src = vec![i as u8; i];

        let mut fault_return: u64 = 0;
        // SAFETY: `dst` and `src` are valid non-overlapping buffers of length `i`,
        // `fault_return` is a valid mutable `u64`, and the caller guarantees `copy_fn`'s contract.
        let result = unsafe {
            copy_fn(
                dst.as_mut_ptr().cast::<c_void>(),
                src.as_ptr().cast::<c_void>(),
                i,
                &mut fault_return,
                0,
            )
        };
        assert_eq!(zx_types::ZX_OK, result.status);
        for j in 0..i {
            assert_eq!(src[j], dst[j], "case ({i}, {j})");
        }

        // The fault return address should have been reset.
        assert_eq!(0u64, fault_return);
    }
}

#[test]
fn x86_copy_to_or_from_user_movsb() {
    // SAFETY: `_x86_copy_to_or_from_user_movsb` satisfies the `test_user_copy` contract.
    unsafe { test_user_copy(_x86_copy_to_or_from_user_movsb) };
}

#[test]
fn x86_copy_to_or_from_user_movsq() {
    // SAFETY: `_x86_copy_to_or_from_user_movsq` satisfies the `test_user_copy` contract.
    unsafe { test_user_copy(_x86_copy_to_or_from_user_movsq) };
}
