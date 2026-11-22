// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// TODO(https://fxbug.dev/507423574): Remove these FFI entry points once all C++ callers of
// Backtrace are ported to Rust.

use super::{Backtrace, FrameType};
use libc::stdio::FILE;

/// FFI entry point for C++ `Backtrace::reset()`.
///
/// # Safety
///
/// `bt` must be a valid, aligned pointer to an initialized [`Backtrace`] with exclusive access for
/// the duration of this call (including a valid [`FrameType`] in `first_frame_type` and
/// `size <= Backtrace::MAX_SIZE`).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_backtrace_reset(bt: *mut Backtrace) {
    // SAFETY: The caller guarantees `bt` is a valid, aligned pointer to an initialized `Backtrace`
    // with exclusive access for the duration of this call.
    unsafe { (*bt).reset() };
}

/// FFI entry point for C++ `Backtrace::push_back(vaddr_t)`.
///
/// # Safety
///
/// `bt` must be a valid, aligned pointer to an initialized [`Backtrace`] with exclusive access for
/// the duration of this call (including a valid [`FrameType`] in `first_frame_type` and
/// `size <= Backtrace::MAX_SIZE`).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_backtrace_push_back(bt: *mut Backtrace, addr: usize) {
    // SAFETY: The caller guarantees `bt` is a valid, aligned pointer to an initialized `Backtrace`
    // with exclusive access for the duration of this call.
    unsafe { (*bt).push_back(addr) };
}

/// FFI entry point for C++ `Backtrace::set_first_frame_type(FrameType)`.
///
/// # Safety
///
/// `bt` must be a valid, aligned pointer to an initialized [`Backtrace`] with exclusive access for
/// the duration of this call (including a valid [`FrameType`] in `first_frame_type` and
/// `size <= Backtrace::MAX_SIZE`).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_backtrace_set_first_frame_type(bt: *mut Backtrace, ty: u32) {
    let ty = match ty {
        1 => FrameType::PreciseLocation,
        _ => FrameType::ReturnAddress,
    };
    // SAFETY: The caller guarantees `bt` is a valid, aligned pointer to an initialized `Backtrace`
    // with exclusive access for the duration of this call.
    unsafe { (*bt).set_first_frame_type(ty) };
}

/// FFI entry point for C++ `Backtrace::Print(FILE*)`.
///
/// # Safety
///
/// `bt` must be a valid, aligned pointer to an initialized [`Backtrace`] with shared read access
/// for the duration of this call (including a valid [`FrameType`] in `first_frame_type` and
/// `size <= Backtrace::MAX_SIZE`), and the caller must uphold [`Backtrace::print_to_file`]'s
/// safety contract for `file`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_backtrace_print(bt: *const Backtrace, file: *mut FILE) {
    // SAFETY: The caller guarantees `bt` is a valid, aligned pointer to an initialized `Backtrace`
    // with shared read access for the duration of this call, and upholds
    // `Backtrace::print_to_file`'s safety contract for `file`.
    unsafe { (*bt).print_to_file(file) };
}

/// FFI entry point for C++ `Backtrace::PrintWithoutVersion(FILE*)`.
///
/// # Safety
///
/// `bt` must be a valid, aligned pointer to an initialized [`Backtrace`] with shared read access
/// for the duration of this call (including a valid [`FrameType`] in `first_frame_type` and
/// `size <= Backtrace::MAX_SIZE`), and the caller must uphold
/// [`Backtrace::print_without_version`]'s safety contract for `file`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_backtrace_print_without_version(bt: *const Backtrace, file: *mut FILE) {
    // SAFETY: The caller guarantees `bt` is a valid, aligned pointer to an initialized `Backtrace`
    // with shared read access for the duration of this call, and upholds
    // `Backtrace::print_without_version`'s safety contract for `file`.
    unsafe { (*bt).print_without_version(file) };
}
