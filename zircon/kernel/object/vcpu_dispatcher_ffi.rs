// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::guest_dispatcher::GuestDispatcher;
use super::vcpu::Vcpu;
use super::vcpu_dispatcher::{VcpuDispatcher, VcpuDispatcherState};
use core::mem::MaybeUninit;
use core::pin::Pin;
use fbl::{RefPtr, UniquePtr};
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    pub fn cpp_vcpu_dispatcher_create(
        guest_dispatcher_raw: *mut GuestDispatcher,
        vcpu_raw: *mut Vcpu,
        handle_out: *mut MaybeUninit<KernelHandle<VcpuDispatcher>>,
    ) -> zx_status_t;
}

// Rust FFI trampolines for C++ calling into Rust VcpuDispatcher

/// Initializes a `VcpuDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `VcpuDispatcherState`.
/// `guest_dispatcher_raw` must be a valid, non-null pointer carrying an owned reference released
/// from `RefPtr<GuestDispatcher>` via `RefPtr::into_raw` by `VcpuDispatcher::create`. `vcpu_raw`
/// must be a valid, non-null owning pointer to a pinned `Vcpu` released from
/// `Pin<UniquePtr<Vcpu>>` by `VcpuDispatcher::create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vcpu_dispatcher_state_init(
    state: *mut VcpuDispatcherState,
    dispatcher: *const VcpuDispatcher,
    guest_dispatcher_raw: *mut GuestDispatcher,
    vcpu_raw: *mut Vcpu,
) {
    // SAFETY: `guest_dispatcher_raw` carries an owned reference released from `RefPtr::into_raw`
    // in `VcpuDispatcher::create`.
    let guest_dispatcher = unsafe { RefPtr::from_raw(guest_dispatcher_raw) };
    // SAFETY: `vcpu_raw` is a valid owning pointer to a pinned VCPU, transferred from
    // `VcpuDispatcher::create`.
    let vcpu = unsafe { Pin::new_unchecked(UniquePtr::from_raw(vcpu_raw)) };
    let init = VcpuDispatcherState::init(dispatcher, guest_dispatcher, vcpu);
    // SAFETY: `state` points to uninitialized memory allocated for `VcpuDispatcherState`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(init, state);
    }
}
