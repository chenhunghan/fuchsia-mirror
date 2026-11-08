// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::event_pair_dispatcher::{EventPairDispatcher, EventPairDispatcherState};
use super::handle::KernelHandle;
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

unsafe extern "C" {
    pub(crate) fn cpp_event_pair_dispatcher_create(
        holder: *mut (),
        handle_out: *mut MaybeUninit<KernelHandle<EventPairDispatcher>>,
    ) -> zx_status_t;
}

crate::object::dispatcher::impl_peered_dispatcher_state_init!(
    EventPairDispatcher,
    EventPairDispatcherState,
);
