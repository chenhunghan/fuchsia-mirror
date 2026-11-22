// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::bus_transaction_initiator_dispatcher::BusTransactionInitiatorDispatcher;
use super::dispatcher::impl_dispatcher_state_init;
use super::handle::KernelHandle;
use super::pinned_memory_token_dispatcher::{
    PinnedMemoryTokenDispatcher, PinnedMemoryTokenDispatcherState,
};
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_types::zx_status_t;

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Allocates and initializes a new `PinnedMemoryTokenDispatcher` via C++ `KernelHandle`,
    /// writing the resulting handle to `handle_out` on `ZX_OK`.
    ///
    /// # Safety
    ///
    /// `bti` must be a valid reference for the duration of the call, and `handle_out` must
    /// point to valid, aligned, writable uninitialized storage.
    pub(crate) fn cpp_pinned_memory_token_dispatcher_create(
        bti: &RefPtr<BusTransactionInitiatorDispatcher>,
        handle_out: *mut MaybeUninit<KernelHandle<PinnedMemoryTokenDispatcher>>,
    ) -> zx_status_t;
}

impl_dispatcher_state_init!(
    PinnedMemoryTokenDispatcher,
    PinnedMemoryTokenDispatcherState,
    bti: &RefPtr<BusTransactionInitiatorDispatcher>,
);

/// Called when all handles to the dispatcher are closed.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pinned_memory_token_dispatcher_on_zero_handles(
    disp: &PinnedMemoryTokenDispatcher,
) {
    disp.on_zero_handles();
}
