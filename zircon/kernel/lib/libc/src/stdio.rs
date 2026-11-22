// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{VaList, c_char, c_int, c_void};
use core::fmt;
use core::mem::{align_of, size_of};

mod string_file;

pub use string_file::{ScopedFile, StringFile};

/// The C++ `FILE::Callback` signature: `int(void* ptr, const char* str, size_t len)`.
pub type FileCallback = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> c_int;

/// This represents the kernel `<stdio.h>` `FILE` type.
#[repr(C)]
pub struct FILE {
    write: Option<FileCallback>,
    ptr: *mut c_void,
}

const _: () = {
    assert!(size_of::<FILE>() == 16);
    assert!(align_of::<FILE>() == 8);
};

impl FILE {
    /// Creates a `FILE` that sends everything written to it to `write`, passing `ptr` as the
    /// callback's first argument, like the C++ `FILE(Callback* write, void* ptr)` constructor.
    ///
    /// # Safety
    ///
    /// For the lifetime of the returned `FILE`, `write` (if present) must be sound to call with
    /// `ptr` and any `(data, len)` where `data` points to `len` readable bytes when `len > 0` (and
    /// may be null when `len == 0`).
    pub const unsafe fn new(write: Option<FileCallback>, ptr: *mut c_void) -> Self {
        Self { write, ptr }
    }

    /// Writes `bytes` via the callback of `*this` without forming a Rust reference to `*this`.
    ///
    /// Returns the callback's result, or -1 if `*this` has no callback.
    ///
    /// # Safety
    ///
    /// `this` must be non-null, aligned, and point to an initialized `FILE` whose `write` and
    /// `ptr` fields can be read without a data race, and whose `write` callback (if `Some`) is
    /// sound to call with `(ptr, bytes.as_ptr().cast::<c_char>(), bytes.len())`.
    pub unsafe fn write_raw(this: *const Self, bytes: &[u8]) -> c_int {
        // SAFETY: The caller guarantees `this` is non-null, aligned, and points to an initialized
        // `FILE` whose `write` field is valid to read.
        let Some(write) = (unsafe { (*this).write }) else {
            return -1;
        };
        // SAFETY: The caller guarantees `this` is non-null, aligned, and points to an initialized
        // `FILE` whose `ptr` field is valid to read.
        let ptr = unsafe { (*this).ptr };
        // SAFETY:
        // - `ptr` is the context uniquely paired with `write` by the C API.
        // - `bytes` points to a valid sequence of `bytes.len()` bytes that remains valid and
        //   immutable for the duration of the FFI call.
        unsafe { write(ptr, bytes.as_ptr().cast::<c_char>(), bytes.len()) }
    }

    /// Writes `bytes` via the callback, like the C++ `FILE::Write`. This is what `fprintf` calls
    /// to do output.
    ///
    /// Returns the callback's result, or -1 if this `FILE` has no callback.
    pub fn write(&self, bytes: &[u8]) -> c_int {
        // SAFETY: `self` is a valid, aligned, initialized `FILE` whose `write` and `ptr` fields
        // satisfy `Self::new`'s safety contract.
        unsafe { Self::write_raw(core::ptr::from_ref(self), bytes) }
    }
}

impl fmt::Write for FILE {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.write(s.as_bytes()) < 0 { Err(fmt::Error) } else { Ok(()) }
    }
}

/// Returns a pointer to the kernel's global `gStdout`, matching `<stdio.h>` `stdout`.
///
/// `gStdout` is statically initialized with a thread-safe write callback that is a safe no-op
/// (dropping output) until `call_constructors()` has completed (`ConstructorsCalled() == true`),
/// and is not mutated during normal kernel operation (note that `console_rust` unit tests
/// temporarily overwrite `*stdout()` assuming quiescent output).
#[inline]
pub fn stdout() -> *mut FILE {
    &raw mut gStdout
}

unsafe extern "C" {
    /// The kernel's `<stdio.h>` `gStdout`.
    #[allow(non_upper_case_globals)]
    static mut gStdout: FILE;

    /// Writes formatted output to `f`, matching `<stdio.h>` `fprintf`.
    ///
    /// # Safety
    ///
    /// `f` must point to a valid `FILE`. `fmt` must be non-null and point to a valid
    /// NUL-terminated C format string whose format specifiers match the variadic arguments.
    pub fn fprintf(f: *mut FILE, fmt: *const c_char, ...) -> c_int;

    /// Writes `VaList` formatted output to `f`, matching `<stdio.h>` `vfprintf`.
    ///
    /// # Safety
    ///
    /// `f` must point to a valid `FILE`. `fmt` must be non-null and point to a valid
    /// NUL-terminated C format string whose format specifiers match the arguments in `ap`.
    pub fn vfprintf(f: *mut FILE, fmt: *const c_char, ap: VaList<'_>) -> c_int;
}
