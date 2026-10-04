// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_void};
use core::fmt;
use core::mem::{align_of, size_of};

/// This represents the kernel `<stdio.h>` `FILE` type.
#[repr(C)]
pub struct FILE {
    write: Option<unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> c_int>,
    ptr: *mut c_void,
}

const _: () = {
    assert!(size_of::<FILE>() == 16);
    assert!(align_of::<FILE>() == 8);
};

impl fmt::Write for FILE {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let Some(write) = self.write else {
            return Err(fmt::Error);
        };
        if s.is_empty() {
            return Ok(());
        }
        let c_chars = s.as_ptr().cast::<c_char>();
        // SAFETY:
        // - `self.ptr` is the context uniquely paired with `write` by the C API.
        // - `s` is not empty, so `c_chars` points to a valid sequence of `s.len()` bytes.
        // - The `s` memory region remains valid and immutable for the duration of the FFI call.
        let ret = unsafe { write(self.ptr, c_chars, s.len()) };
        if ret < 0 { Err(fmt::Error) } else { Ok(()) }
    }
}
