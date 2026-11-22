// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::guest::Guest;
use super::guest_dispatcher::{GuestDispatcher, GuestDispatcherState};
use core::mem::MaybeUninit;
use core::pin::Pin;
use fbl::UniquePtr;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    /// Creates a `GuestDispatcher` for `guest_raw`.
    ///
    /// On `ZX_OK`, takes ownership of `guest_raw` and initializes `*guest_handle_out`. Otherwise,
    /// ownership of `guest_raw` stays with the caller, and `*guest_handle_out` is uninitialized.
    pub(crate) fn cpp_guest_dispatcher_create(
        guest_raw: *mut Guest,
        guest_handle_out: *mut MaybeUninit<KernelHandle<GuestDispatcher>>,
    ) -> zx_status_t;
}

// Rust FFI trampolines for C++ calling into Rust GuestDispatcher

/// Initializes a `GuestDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `GuestDispatcherState`.
/// `guest_raw` must be a non-null pointer that is the sole owner of a `Guest`, which can be freed
/// by `UniquePtr<Guest>` and has not been moved since it was pinned (such as a pointer released
/// from `Pin<UniquePtr<Guest>>` by `GuestDispatcher::create`). Ownership of the guest passes to
/// the `GuestDispatcherState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_guest_dispatcher_state_init(
    state: *mut GuestDispatcherState,
    dispatcher: *const GuestDispatcher,
    guest_raw: *mut Guest,
) {
    // SAFETY: The caller guarantees that `guest_raw` is the sole owner of a pinned guest that
    // `UniquePtr` can free, and passes ownership of it here.
    let guest = unsafe { Pin::new_unchecked(UniquePtr::from_raw(guest_raw)) };
    let init = GuestDispatcherState::init(dispatcher, guest);
    // SAFETY: `state` points to uninitialized memory allocated for `GuestDispatcherState`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(init, state);
    }
}
