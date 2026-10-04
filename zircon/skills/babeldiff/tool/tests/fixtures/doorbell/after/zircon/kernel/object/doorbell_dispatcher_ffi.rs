// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::doorbell_dispatcher::{
    BuzzerDoorbellDispatcher, ChimeDoorbellDispatcher, DoorbellDispatcher,
};
use crate::object::dispatcher::KernelHandle;
use zx_status::Status;
use zx_types::{zx_duration_t, zx_rights_t, zx_status_t, ZX_OK};

unsafe extern "C" {
    pub(crate) fn cpp_doorbell_dispatcher_log(doorbell: *const DoorbellDispatcher, kind: u64);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_doorbell_dispatcher_ring(doorbell: &DoorbellDispatcher, tone: u32) -> zx_status_t {
    Status::from_result(doorbell.ring(tone))
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_doorbell_dispatcher_ring_count(doorbell: &DoorbellDispatcher) -> u64 {
    doorbell.ring_count()
}

/// # Safety
///
/// `handle` and `rights` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_chime_doorbell_dispatcher_create(
    notes: u32,
    handle: *mut KernelHandle<DoorbellDispatcher>,
    rights: *mut zx_rights_t,
) -> zx_status_t {
    match ChimeDoorbellDispatcher::create(notes) {
        Ok((h, r)) => {
            // SAFETY: The caller guarantees both pointers are valid for writes.
            unsafe {
                *handle = h;
                *rights = r;
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `handle` and `rights` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_buzzer_doorbell_dispatcher_create(
    length: zx_duration_t,
    handle: *mut KernelHandle<DoorbellDispatcher>,
    rights: *mut zx_rights_t,
) -> zx_status_t {
    match BuzzerDoorbellDispatcher::create(length) {
        Ok((h, r)) => {
            unsafe {
                *handle = h;
                *rights = r;
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
