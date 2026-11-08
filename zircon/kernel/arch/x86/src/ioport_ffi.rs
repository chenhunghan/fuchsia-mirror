// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;
use pin_init::InPlaceWrite;

use super::ioport::{IoBitmap, clear_tss_io_bitmap, set_tss_io_bitmap};
use crate::vm::arch_vm_aspace::ArchVmAspace;

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Returns a pointer to the embedded [`IoBitmap`] within `aspace`
    /// (`X86ArchVmAspace::io_bitmap` in `mmu.cc`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `aspace` points to a live (not freed), valid `X86ArchVmAspace`.
    #[link_name = "cpp_x86_arch_aspace_io_bitmap"]
    pub fn x86_arch_aspace_io_bitmap(aspace: *const ArchVmAspace) -> *const IoBitmap;
}

/// FFI entry point to initialize an [`IoBitmap`] in place.
///
/// # Safety
///
/// The caller must ensure that `bitmap` points to valid, aligned, uninitialized storage for a
/// live (not freed) [`IoBitmap`] that remains pinned until dropped by [`rust_io_bitmap_destroy`].
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_bitmap_init(bitmap: *mut MaybeUninit<IoBitmap>) {
    // SAFETY: Caller guarantees `bitmap` points to valid uninitialized storage that remains pinned.
    let slot = unsafe { &mut *bitmap };
    let _ = slot.write_pin_init(IoBitmap::init());
}

/// FFI entry point to destroy an [`IoBitmap`] in place.
///
/// # Safety
///
/// The caller must ensure that `bitmap` points to a live (not freed), initialized [`IoBitmap`]
/// that will not be used again.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_bitmap_destroy(bitmap: *mut IoBitmap) {
    // SAFETY: Caller guarantees `bitmap` points to a valid initialized `IoBitmap`.
    unsafe { core::ptr::drop_in_place(bitmap) };
}

/// FFI entry point for [`set_tss_io_bitmap`].
///
/// # Safety
///
/// The caller must ensure that `bitmap` points to a live (not freed), valid [`IoBitmap`],
/// `%gs.base` is initialized for the current CPU, and interrupts are disabled.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_set_tss_io_bitmap(bitmap: &IoBitmap) {
    // SAFETY: Caller guarantees `bitmap` is valid, `%gs.base` is initialized, and interrupts are
    // disabled.
    unsafe { set_tss_io_bitmap(bitmap) };
}

/// FFI entry point for [`clear_tss_io_bitmap`].
///
/// # Safety
///
/// The caller must ensure that `bitmap` points to a live (not freed), valid [`IoBitmap`],
/// `%gs.base` is initialized for the current CPU, and interrupts are disabled.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_clear_tss_io_bitmap(bitmap: &IoBitmap) {
    // SAFETY: Caller guarantees `bitmap` is valid, `%gs.base` is initialized, and interrupts are
    // disabled.
    unsafe { clear_tss_io_bitmap(bitmap) };
}
