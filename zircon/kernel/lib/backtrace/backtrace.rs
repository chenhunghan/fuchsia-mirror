// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use libc::stdio::FILE;
use object_constants_rs as object_constants;

unsafe extern "C" {
    fn cpp_backtrace_print(bt: *const Backtrace);
    fn cpp_backtrace_print_without_version(bt: *const Backtrace, file: *mut FILE);
    fn cpp_backtrace_data(bt: *const Backtrace) -> *const usize;
    fn cpp_backtrace_size(bt: *const Backtrace) -> usize;
    fn cpp_backtrace_reset(bt: *mut Backtrace);
}

/// Contains an array of return address values representing a thread's backtrace.
///
/// This type is not thread-safe.
#[repr(C, align(8))]
pub struct Backtrace {
    _opaque: zr::OpaqueBytes<{ object_constants::kBacktraceSize }>,
}

zr::static_assert_size_and_align!(
    Backtrace,
    object_constants::kBacktraceSize,
    object_constants::kBacktraceAlign,
);

impl Backtrace {
    /// The maximum number of elements in a backtrace. The value should be large enough to capture
    /// the full call stack for most kernel crashes, but also small enough that it's reasonable to
    /// allocate a `Backtrace` on the stack.
    pub const MAX_SIZE: usize = 32;

    /// Creates an empty `Backtrace`.
    #[inline]
    pub const fn new() -> Self {
        Self { _opaque: zr::OpaqueBytes::new([0u8; object_constants::kBacktraceSize]) }
    }

    /// Returns the number of elements in this backtrace.
    #[inline]
    pub fn size(&self) -> usize {
        // SAFETY: `self` is a valid `Backtrace` reference matching the C++ `Backtrace` layout.
        unsafe { cpp_backtrace_size(self) }
    }

    /// Resets the size to 0.
    #[inline]
    pub fn reset(&mut self) {
        // SAFETY: `self` is a valid `Backtrace` reference matching the C++ `Backtrace` layout.
        unsafe { cpp_backtrace_reset(self) }
    }

    /// Returns the recorded frame addresses as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[usize] {
        let size = self.size().min(Self::MAX_SIZE);
        if size == 0 {
            return &[];
        }
        // SAFETY: `cpp_backtrace_data(self)` returns a non-null, properly aligned pointer to
        // the inline `vaddr_` array inside `self`. It is valid for reads of `size` elements
        // (including when `size == 0`) for the lifetime of `self`.
        unsafe {
            let data = cpp_backtrace_data(self);
            core::slice::from_raw_parts(data, size)
        }
    }

    /// Pretty-prints this backtrace to `stdout`.
    #[inline]
    pub fn print(&self) {
        // SAFETY: `self` is a valid initialized `Backtrace`.
        unsafe { cpp_backtrace_print(self) }
    }

    /// Pretty-prints this backtrace to `file` without the kernel version preamble.
    ///
    /// # Safety
    ///
    /// `file` must point to a valid `FILE` for the duration of the call.
    #[inline]
    pub unsafe fn print_without_version(&self, file: *mut FILE) {
        // SAFETY: `self` is a valid initialized `Backtrace`, and the caller guarantees `file`
        // points to a valid `FILE`.
        unsafe { cpp_backtrace_print_without_version(self, file) }
    }
}

impl Default for Backtrace {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
