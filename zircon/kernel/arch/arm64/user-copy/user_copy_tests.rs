// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_int, c_uint, c_void};
use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};

use zx_types::{zx_status_t, zx_vaddr_t};

/// Typically we would not use structs as function return values, but in this case it enables us to
/// very efficiently use the 2 registers for return values to encode the optional flags and va
/// page fault values.
#[repr(C)]
struct Arm64UserCopyRet {
    status: zx_status_t,
    pf_flags: c_uint,
    pf_va: zx_vaddr_t,
}

zr::static_assert!(size_of::<Arm64UserCopyRet>() == 16);
zr::static_assert!(align_of::<Arm64UserCopyRet>() == 8);
zr::static_assert!(offset_of!(Arm64UserCopyRet, status) == 0);
zr::static_assert!(offset_of!(Arm64UserCopyRet, pf_flags) == 4);
zr::static_assert!(offset_of!(Arm64UserCopyRet, pf_va) == 8);

#[cfg(not(any(
    feature = "_arm64_user_copy",
    feature = "_arm64_user_copy_from_user",
    feature = "_arm64_user_copy_to_user",
)))]
compile_error!("One of _arm64_user_copy* features must be enabled");

unsafe extern "C" {
    /// # Safety
    ///
    /// `seed` must be a valid, aligned pointer to a mutable `c_uint`.
    fn rand_r(seed: *mut c_uint) -> c_int;

    /// This is the same as memcpy, except that it takes the additional argument of
    /// `&current_thread()->arch.data_fault_resume`, where it temporarily stores the fault recovery
    /// PC for bad page faults to user addresses during the call, and a `fault_return_mask`. If
    /// `ARM64_USER_COPY_CAPTURE_FAULTS` is passed as `fault_return_mask` then the returned struct
    /// will have `pf_flags` and `pf_va` filled out on pagefault, otherwise they should be ignored.
    /// `arch_copy_from_user` and `arch_copy_to_user` should be the only callers of this.
    ///
    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes, `src` must be valid for reads of `len`
    /// bytes, the two regions must not overlap, and `fault_return` must be a valid aligned pointer
    /// to a mutable `u64`.
    #[cfg(feature = "_arm64_user_copy")]
    fn _arm64_user_copy(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> Arm64UserCopyRet;

    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes, `src` must be valid for reads of `len`
    /// bytes, the two regions must not overlap, and `fault_return` must be a valid aligned pointer
    /// to a mutable `u64`.
    #[cfg(feature = "_arm64_user_copy_from_user")]
    fn _arm64_user_copy_from_user(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> Arm64UserCopyRet;

    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes, `src` must be valid for reads of `len`
    /// bytes, the two regions must not overlap, and `fault_return` must be a valid aligned pointer
    /// to a mutable `u64`.
    #[cfg(feature = "_arm64_user_copy_to_user")]
    fn _arm64_user_copy_to_user(
        dst: *mut c_void,
        src: *const c_void,
        len: usize,
        fault_return: *mut u64,
        fault_return_mask: u64,
    ) -> Arm64UserCopyRet;
}

#[cfg(feature = "_arm64_user_copy")]
use _arm64_user_copy as arm64_usercopy_fn;
#[cfg(feature = "_arm64_user_copy_from_user")]
use _arm64_user_copy_from_user as arm64_usercopy_fn;
#[cfg(feature = "_arm64_user_copy_to_user")]
use _arm64_user_copy_to_user as arm64_usercopy_fn;

struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize, align: usize) -> Self {
        assert!(size > 0);
        let layout = Layout::from_size_align(size, align).expect("valid layout");
        // SAFETY: `size > 0` is asserted above, and `layout` is valid.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        Self { ptr, layout }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `self.ptr` points to `self.layout.size()` initialized bytes exclusively owned by
        // `self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was allocated with `alloc_zeroed(self.layout)`.
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

/// The goal of this test is to verify the behavior of each unrolled loop of the usercopy impl,
/// with and without alignment. Later we can swap the `user_copy` variant, and verify it behaves as
/// expected.
#[track_caller]
fn do_and_verify_copy(copy_size: usize, src_offset: usize, dst_offset: usize) {
    const CANARY: [u8; 16] =
        [0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xA, 0xB, 0xC, 0xD, 0xE, 0xF];
    const ALIGNMENT_FILL: u8 = 0xAA;
    const ALIGNMENT: usize = 16;

    let get_size = |offset: usize| offset + copy_size + 2 * CANARY.len();

    let mut src_buffer = AlignedBuf::new(get_size(src_offset), ALIGNMENT);
    let mut dst_buffer = AlignedBuf::new(get_size(dst_offset), ALIGNMENT);

    let src = src_buffer.as_mut_slice();
    let dst = dst_buffer.as_mut_slice();
    // Randomize the contents of src and destination.
    let mut seed: c_uint = 0;
    let mut fill_random = |buffer: &mut [u8], copy_size: usize, offset: usize| {
        for i in 0..buffer.len() {
            // Front canary.
            if i < CANARY.len() {
                buffer[i] = CANARY[i];
            } else if i < CANARY.len() + offset {
                // Padding bytes with a fixed pattern.
                buffer[i] = ALIGNMENT_FILL;
            } else if i < CANARY.len() + offset + copy_size {
                // Actual region to copy.
                // SAFETY: `seed` is a valid aligned mutable `c_uint`.
                let r = unsafe { rand_r(&mut seed) };
                buffer[i] = (r % c_int::from(u8::MAX)) as u8;
            } else {
                // Back Canary.
                buffer[i] = CANARY[i - (CANARY.len() + copy_size + offset)];
            }
        }
    };

    fill_random(src, copy_size, src_offset);
    fill_random(dst, copy_size, dst_offset);

    let mut fault_return: u64 = 0;
    // SAFETY: `dst` and `src` have length `offset + copy_size + 2 * CANARY.len()`, so the regions
    // starting at `CANARY.len() + dst_offset` and `CANARY.len() + src_offset` are valid and
    // non-overlapping for `copy_size` bytes. `fault_return` is a valid mutable `u64`.
    let Arm64UserCopyRet { status, pf_flags: _fault_flags, pf_va: _fault_addr } = unsafe {
        arm64_usercopy_fn(
            dst[CANARY.len() + dst_offset..].as_mut_ptr().cast::<c_void>(),
            src[CANARY.len() + src_offset..].as_ptr().cast::<c_void>(),
            copy_size,
            &mut fault_return,
            0,
        )
    };

    assert_eq!(
        status,
        zx_types::ZX_OK,
        "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
    );
    assert_eq!(
        fault_return, 0,
        "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
    );

    // Verify contents.
    for i in 0..dst.len() {
        if i < CANARY.len() {
            assert_eq!(
                dst[i], CANARY[i],
                "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
            );
        } else if i < CANARY.len() + dst_offset {
            assert_eq!(
                dst[i], ALIGNMENT_FILL,
                "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
            );
        } else if i < CANARY.len() + copy_size + dst_offset {
            let region_offset = i - (CANARY.len() + dst_offset);
            assert_eq!(
                dst[i],
                src[CANARY.len() + src_offset + region_offset],
                "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
            );
        } else {
            assert_eq!(
                dst[i],
                CANARY[i - (CANARY.len() + copy_size + dst_offset)],
                "Args: copy_size: {copy_size} src_offset: {src_offset} dst_offset: {dst_offset}",
            );
        }
    }
}

// Each test case represents an internal unrolled loop branch. The goal is to verify individual
// branches for correctness.

#[test]
fn bytes_16() {
    const MAX_COPY_SIZE: usize = 16;
    const ALIGN_BOUNDARY: usize = 16;

    for copy_size in 0..=MAX_COPY_SIZE {
        for src_offset in 0..ALIGN_BOUNDARY {
            for dst_offset in 0..ALIGN_BOUNDARY {
                do_and_verify_copy(copy_size, src_offset, dst_offset);
            }
        }
    }
}

#[test]
fn bytes_17_to_96() {
    const MAX_COPY_SIZE: usize = 96;
    const MIN_COPY_SIZE: usize = 17;
    const ALIGN_BOUNDARY: usize = 16;

    for copy_size in MIN_COPY_SIZE..=MAX_COPY_SIZE {
        for src_offset in 0..ALIGN_BOUNDARY {
            for dst_offset in 0..ALIGN_BOUNDARY {
                do_and_verify_copy(copy_size, src_offset, dst_offset);
            }
        }
    }
}

#[test]
fn long_copy() {
    const MAX_COPY_SIZE: usize = 257;
    const MIN_COPY_SIZE: usize = 97;
    const ALIGN_BOUNDARY: usize = 16;

    for copy_size in MIN_COPY_SIZE..=MAX_COPY_SIZE {
        for src_offset in 0..ALIGN_BOUNDARY {
            for dst_offset in 0..ALIGN_BOUNDARY {
                do_and_verify_copy(copy_size, src_offset, dst_offset);
            }
        }
    }
}
