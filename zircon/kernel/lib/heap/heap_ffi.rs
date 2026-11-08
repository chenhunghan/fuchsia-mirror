// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel heap C FFI wrappers.

use core::ffi::c_void;

// TODO(https://fxbug.dev/565806419): Remove once all C/C++ callers are ported to Rust.
/// C FFI entry point for [`super::heap_wrapper::rust_heap_init`].
///
/// # Safety
///
/// The caller must ensure that this is called exactly once during kernel initialization.
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_heap_init() {
    // SAFETY: The caller upholds the preconditions of `rust_heap_init`.
    unsafe { super::heap_wrapper::rust_heap_init() }
}

/// C FFI entry point for [`super::heap_wrapper::malloc`].
///
/// # Safety
///
/// The caller must ensure that the heap has been initialized.
// Prevent inlining into C++ callers when console_enabled is active so that return_address(0)
// in get_caller() works.
#[cfg_attr(console_enabled, inline(never))]
// SAFETY: The symbol name is unique and matches the standard C library signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // SAFETY: The caller upholds the preconditions of `malloc`.
    unsafe { super::heap_wrapper::malloc(size) }
}

/// C FFI entry point for [`super::heap_wrapper::memalign`].
///
/// # Safety
///
/// The caller must ensure that the heap has been initialized and `alignment` is a power of two.
#[cfg_attr(console_enabled, inline(never))]
// SAFETY: The symbol name is unique and matches the standard C library signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    // SAFETY: The caller upholds the preconditions of `memalign`.
    unsafe { super::heap_wrapper::memalign(alignment, size) }
}

/// C FFI entry point for [`super::heap_wrapper::calloc`].
///
/// # Safety
///
/// The caller must ensure that the heap has been initialized.
#[cfg_attr(console_enabled, inline(never))]
// SAFETY: The symbol name is unique and matches the standard C library signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    // SAFETY: The caller upholds the preconditions of `calloc`.
    unsafe { super::heap_wrapper::calloc(count, size) }
}

/// C FFI entry point for [`super::heap_wrapper::free`].
///
/// # Safety
///
/// The caller must ensure that `ptr` was allocated by the heap and has not yet been freed.
#[cfg_attr(console_enabled, inline(never))]
// SAFETY: The symbol name is unique and matches the standard C library signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    // SAFETY: The caller upholds the preconditions of `free`.
    unsafe { super::heap_wrapper::free(ptr) }
}

/// C FFI entry point for [`super::heap_wrapper::sized_free`].
///
/// # Safety
///
/// The caller must ensure that `ptr` was allocated by the heap with size `s`
/// and has not yet been freed.
#[cfg_attr(console_enabled, inline(never))]
// SAFETY: The symbol name is unique and matches the standard C library signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sized_free(ptr: *mut c_void, s: usize) {
    // SAFETY: The caller upholds the preconditions of `sized_free`.
    unsafe { super::heap_wrapper::sized_free(ptr, s) }
}

// TODO(https://fxbug.dev/565806419): Remove once all C/C++ callers are ported to Rust.
/// C FFI entry point for [`super::heap_wrapper::profile_track_alloc`].
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(export_name = "rust_profile_track_alloc")]
pub extern "C" fn rust_profile_track_alloc(size: usize) -> u32 {
    super::heap_wrapper::profile_track_alloc(size)
}

// TODO(https://fxbug.dev/565806419): Remove once all C/C++ callers are ported to Rust.
/// C FFI entry point for [`super::heap_wrapper::profile_track_free`].
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(export_name = "rust_profile_track_free")]
pub extern "C" fn rust_profile_track_free(handle: u32, size: usize) {
    super::heap_wrapper::profile_track_free(handle, size)
}

// TODO(https://fxbug.dev/565806419): Remove once all C/C++ callers are ported to Rust.
/// C FFI entry point for [`super::heap_wrapper::rust_cmpct_set_fill_on_alloc_threshold`].
// SAFETY: The symbol name is unique and matches the C FFI declaration.
#[unsafe(export_name = "rust_cmpct_set_fill_on_alloc_threshold")]
pub extern "C" fn rust_cmpct_set_fill_on_alloc_threshold(size: usize) {
    super::heap_wrapper::rust_cmpct_set_fill_on_alloc_threshold(size)
}
