// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_void};

unsafe extern "C" {
    /// Compares the first `n` bytes (each interpreted as `u8`) of the memory areas `s1` and `s2`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that even when `n == 0`, `s1` and `s2` are valid, non-null pointers
    /// (pointing within or one-past-the-end of a live allocated object, per C11 §7.24.1p2 /
    /// §7.1.4p1), and that `n <= isize::MAX as usize` and `s1` and `s2` are each valid for reads
    /// of `n` initialized bytes within a single allocated object (without wrapping the address
    /// space and not concurrently mutated for the duration of the call).
    pub fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;

    /// Copies `n` non-overlapping bytes from `src` to `dst`, returning `dst`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that even when `n == 0`, `dst` and `src` are valid, non-null
    /// pointers (pointing within or one-past-the-end of a live allocated object, per C11
    /// §7.24.1p2 / §7.1.4p1), `n <= isize::MAX as usize`, `src` is valid for reads of `n`
    /// initialized bytes within a single allocated object (without wrapping the address space and
    /// not concurrently mutated for the duration of the call), `dst` is valid for writes of `n`
    /// bytes within a single allocated object (without wrapping the address space and not
    /// concurrently accessed for the duration of the call), and the `[src, src + n)` and
    /// `[dst, dst + n)` regions do not overlap. On return, all `n` bytes in `[dst, dst + n)` are
    /// initialized with the bytes copied from `[src, src + n)`.
    pub fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;

    /// Fills `n` bytes starting at `s` with `c as u8`, returning `s`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that even when `n == 0`, `s` is a valid, non-null pointer (pointing
    /// within or one-past-the-end of a live allocated object, per C11 §7.24.1p2 / §7.1.4p1),
    /// `n <= isize::MAX as usize`, and `s` is valid for writes of `n` bytes within a single
    /// allocated object (without wrapping the address space and not concurrently accessed for the
    /// duration of the call). On return, all `n` bytes in `[s, s + n)` are initialized to
    /// `c as u8`.
    pub fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void;

    /// Returns the length in bytes of the NUL-terminated string `s`.
    ///
    /// # Safety
    ///
    /// `s` must be non-null and point to a valid NUL-terminated C string.
    pub fn strlen(s: *const c_char) -> usize;
}
