// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::interrupt_dispatcher::{InterruptDispatcher, InterruptDispatcherState};

unsafe extern "C" {
    pub(super) fn cpp_interrupt_dispatcher_create(
        flags: u32,
        options: u32,
    ) -> *mut InterruptDispatcher;
}

crate::object::dispatcher::impl_dispatcher_state_init!(
    InterruptDispatcher,
    InterruptDispatcherState,
    flags: u32,
    options: u32,
);

/// Tears down the variant payload and unlinks the embedded `WakeEvent` before `disp`'s state is
/// dropped in place.
///
/// # Safety
///
/// Must only be called once, from `InterruptDispatcher::~InterruptDispatcher()` (`ref_count == 0`)
/// prior to `rust_interrupt_dispatcher_state_destroy`, on a `disp` whose `kind` (and `wake_event`,
/// if `has_wake_event()`) was initialized by `InterruptDispatcher::create_raw`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_teardown(disp: &InterruptDispatcher) {
    // SAFETY: Forwarded from the caller (`InterruptDispatcher::~InterruptDispatcher()`).
    unsafe { disp.teardown() };
}

/// # Safety
///
/// `disp` must be a valid reference to an initialized `InterruptDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_on_zero_handles(disp: &InterruptDispatcher) {
    disp.on_zero_handles();
}

/// # Safety
///
/// `disp` must be a valid reference to an initialized `InterruptDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_is_virtual(disp: &InterruptDispatcher) -> bool {
    disp.state().is_virtual()
}
