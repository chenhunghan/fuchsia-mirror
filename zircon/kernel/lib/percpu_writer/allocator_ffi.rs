// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `percpu_writer::KernelAspaceAllocator`.

use super::KernelAspaceAllocator;
use core::ptr::{self, NonNull};

/// Allocates and maps a pinned region of `size` bytes in the kernel address space.
#[unsafe(no_mangle)]
pub extern "C" fn rust_percpu_writer_kernel_aspace_allocator_allocate(size: u32) -> *mut u8 {
    KernelAspaceAllocator::allocate(size).map_or(ptr::null_mut(), NonNull::as_ptr)
}

/// Frees a kernel address space region previously allocated by
/// [`rust_percpu_writer_kernel_aspace_allocator_allocate`].
///
/// # Safety
///
/// If non-null, `ptr` must be a pointer returned by
/// [`rust_percpu_writer_kernel_aspace_allocator_allocate`] that has not yet been freed and is no
/// longer in use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_percpu_writer_kernel_aspace_allocator_free(ptr: *mut u8) {
    // SAFETY: The caller guarantees `ptr` is null or a valid allocation from
    // `rust_percpu_writer_kernel_aspace_allocator_allocate` that is no longer in use.
    unsafe { KernelAspaceAllocator::free(NonNull::new(ptr)) };
}
