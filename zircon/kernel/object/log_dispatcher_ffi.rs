// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::log_dispatcher::{LogDispatcher, LogDispatcherState};
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    pub(crate) fn cpp_log_dispatcher_create(
        flags: u32,
        handle_out: *mut MaybeUninit<KernelHandle<LogDispatcher>>,
    ) -> zx_status_t;
}

// Trampoline callbacks from C++ into Rust LogDispatcherState

crate::object::dispatcher::impl_dispatcher_state_init!(
    LogDispatcher,
    LogDispatcherState,
    flags: u32
);
