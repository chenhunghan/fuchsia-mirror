// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::fifo_dispatcher::FifoDispatcher;
use crate::user_copy::{UserInPtr, UserOutPtr};
use zx_types::{ZX_OK, zx_status_t};

/// # Safety
///
/// `actual` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_fifo_dispatcher_write_from_user(
    fifo: &FifoDispatcher,
    elem_size: usize,
    ptr: *const u8,
    count: usize,
    actual: *mut usize,
) -> zx_status_t {
    match fifo.write_from_user(elem_size, UserInPtr::new(ptr), count) {
        Ok(n) => {
            // SAFETY: The caller guarantees `actual` is valid for writes.
            unsafe { *actual = n };
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `actual` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_fifo_dispatcher_read_to_user(
    fifo: &FifoDispatcher,
    elem_size: usize,
    ptr: *mut u8,
    count: usize,
    actual: *mut usize,
) -> zx_status_t {
    match fifo.read_to_user(elem_size, UserOutPtr::new(ptr), count) {
        Ok(n) => {
            // SAFETY: The caller guarantees `actual` is valid for writes.
            unsafe { *actual = n };
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
