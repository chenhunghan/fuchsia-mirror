// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::pager_dispatcher::{PagerDispatcher, PagerDispatcherState};
use super::pager_proxy::PagerProxy;
use super::port_dispatcher::PortDispatcher;
use crate::vm::page_source::PageProvider;
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

// C++ FFI declarations for PagerDispatcher and PagerProxy.
unsafe extern "C" {
    pub(crate) fn cpp_pager_dispatcher_create(
        handle_out: *mut MaybeUninit<KernelHandle<PagerDispatcher>>,
    ) -> zx_status_t;

    pub(crate) fn cpp_pager_proxy_free(proxy: *mut PagerProxy);
    pub(crate) fn cpp_pager_proxy_get_ref_counted(proxy: *mut PagerProxy) -> *mut ();
    pub(crate) fn cpp_pager_proxy_create(
        dispatcher: &PagerDispatcher,
        port: *mut PortDispatcher,
        key: u64,
        options: u32,
        out_proxy: &mut *mut PagerProxy,
    ) -> zx_status_t;
    pub(crate) fn cpp_pager_proxy_as_page_provider(proxy: *mut PagerProxy) -> *mut PageProvider;
}

// Rust FFI trampolines for C++ calling into Rust PagerDispatcher.

crate::object::dispatcher::impl_dispatcher_state_init!(PagerDispatcher, PagerDispatcherState);

/// Called when all handles to the dispatcher are closed.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_dispatcher_on_zero_handles(disp: &PagerDispatcher) {
    disp.on_zero_handles();
}
