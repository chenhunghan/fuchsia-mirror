// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;

unsafe extern "C" {
    fn cpp_persistent_dlog_write(ptr: *const c_char, len: usize);
    fn cpp_persistent_dlog_set_location(vaddr: *mut u8, len: usize);
}

/// Sets where the persistent debuglog lives, assuming that we have one.  This
/// needs to happen early in boot, usually during ZBI header processing, before
/// we start up the secondary CPUs.  The persistent debuglog keeps using
/// `buffer` for the life of the kernel.
pub fn persistent_dlog_set_location(buffer: &'static mut [u8]) {
    // SAFETY: `buffer` is a live, uniquely borrowed mapping that stays valid
    // forever, which is what the persistent debuglog needs of it.
    unsafe { cpp_persistent_dlog_set_location(buffer.as_mut_ptr(), buffer.len()) }
}

/// Writes `bytes` to the persistent debuglog, if enabled.
pub fn persistent_dlog_write(bytes: &[u8]) {
    // SAFETY: `bytes.as_ptr()` points to `bytes.len()` initialized bytes in memory.
    unsafe { cpp_persistent_dlog_write(bytes.as_ptr().cast::<c_char>(), bytes.len()) }
}
