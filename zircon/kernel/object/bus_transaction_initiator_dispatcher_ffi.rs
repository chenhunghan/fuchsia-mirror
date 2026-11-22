// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::bus_transaction_initiator_dispatcher::{
    BusTransactionInitiatorDispatcher, BusTransactionInitiatorDispatcherState,
};
use super::dispatcher::impl_dispatcher_state_init;
use crate::iommu::Bti;
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_status_t};

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Allocates and initializes a new `BusTransactionInitiatorDispatcher` via C++
    /// `KernelHandle`, writing the resulting handle to `handle_out` on `ZX_OK`.
    ///
    /// On any status other than `ZX_OK`, no dispatcher has been constructed, `bti` has not been
    /// retained, and `handle_out` is left uninitialized.
    ///
    /// # Safety
    ///
    /// `bti` must be a valid reference for the duration of the call, and `handle_out` must
    /// point to valid, aligned, writable uninitialized storage.
    pub(crate) fn cpp_bus_transaction_initiator_dispatcher_create(
        bti: &Bti,
        handle_out: *mut MaybeUninit<KernelHandle<BusTransactionInitiatorDispatcher>>,
    ) -> zx_status_t;
}

impl_dispatcher_state_init!(
    BusTransactionInitiatorDispatcher,
    BusTransactionInitiatorDispatcherState,
    bti: &Bti,
);

/// Called when all handles to the dispatcher are closed.
#[unsafe(no_mangle)]
pub extern "C" fn rust_bus_transaction_initiator_dispatcher_on_zero_handles(
    disp: &BusTransactionInitiatorDispatcher,
) {
    disp.on_zero_handles();
}

/// Sets the debug name of the BTI dispatcher.
///
/// # Safety
///
/// If `len > 0`, `name` must point to at least `len` initialized bytes within a single allocated
/// object, and those bytes must remain valid for reads and not be mutated for the duration of this
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_bus_transaction_initiator_dispatcher_set_name(
    disp: &BusTransactionInitiatorDispatcher,
    name: *const core::ffi::c_char,
    len: usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees `name` is valid for `len` initialized bytes within a single
    // allocation and unmutated for the duration of this call when `len > 0`.
    let slice = unsafe { zr::slice_from_raw_parts(name.cast::<u8>(), len) };
    Status::result_into_raw(disp.set_name(slice))
}

/// Gets the debug name of the BTI dispatcher.
///
/// # Safety
///
/// `out_name` must be a valid, aligned, writable pointer to `ZX_MAX_NAME_LEN` bytes (which may be
/// uninitialized) with exclusive write access for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_bus_transaction_initiator_dispatcher_get_name(
    disp: &BusTransactionInitiatorDispatcher,
    out_name: *mut [u8; ZX_MAX_NAME_LEN],
) -> zx_status_t {
    let mut buf = [0u8; ZX_MAX_NAME_LEN];
    Status::result_into_raw(disp.get_name(&mut buf).map(|()| {
        // SAFETY: Caller guarantees `out_name` is valid, aligned, and writable for
        // `ZX_MAX_NAME_LEN` bytes for the duration of this call.
        unsafe { out_name.write(buf) };
    }))
}
