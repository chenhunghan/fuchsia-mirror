// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::pager_dispatcher::{PagerDispatcher, PagerDispatcherState, PagerProxy};
use super::port_dispatcher::PortDispatcher;
use crate::vm::page_source::PageSource;
use core::mem::MaybeUninit;
use fbl::{DoublyLinkedListNode, RefPtr};
use zx_types::zx_status_t;

// C++ FFI declarations for PagerDispatcher and PagerProxy.
unsafe extern "C" {
    pub(crate) fn cpp_pager_dispatcher_create(
        handle_out: *mut MaybeUninit<KernelHandle<PagerDispatcher>>,
    ) -> zx_status_t;

    pub(crate) fn cpp_pager_proxy_free(proxy: *mut PagerProxy);
    pub(crate) fn cpp_pager_proxy_get_ref_counted(proxy: *mut PagerProxy) -> *mut ();
    pub(crate) fn cpp_pager_proxy_get_dll_node(
        proxy: &PagerProxy,
    ) -> *const DoublyLinkedListNode<PagerProxy>;
    pub(crate) fn cpp_pager_proxy_create(
        dispatcher: &PagerDispatcher,
        port: *mut PortDispatcher,
        key: u64,
        options: u32,
        out_proxy: &mut *mut PagerProxy,
    ) -> zx_status_t;
    pub(crate) fn cpp_pager_proxy_create_page_source(
        proxy: &PagerProxy,
        out_src: &mut *mut PageSource,
    ) -> zx_status_t;
    pub(crate) fn cpp_pager_proxy_set_page_source_unchecked(
        proxy: &PagerProxy,
        src: *mut PageSource,
    );
    pub(crate) fn cpp_pager_proxy_on_dispatcher_close(proxy: &PagerProxy);
}

// Rust FFI trampolines for C++ calling into Rust PagerDispatcher.

crate::object::dispatcher::impl_dispatcher_state_init!(PagerDispatcher, PagerDispatcherState);

/// Called when all handles to the dispatcher are closed.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_dispatcher_on_zero_handles(disp: &PagerDispatcher) {
    disp.on_zero_handles();
}

/// Drops and returns `disp`'s reference to `proxy` as a raw pointer.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_dispatcher_release_proxy(
    disp: &PagerDispatcher,
    proxy: &PagerProxy,
) -> *mut PagerProxy {
    match disp.release_proxy(proxy) {
        Some(proxy_ref) => RefPtr::into_raw(proxy_ref).cast_mut(),
        None => core::ptr::null_mut(),
    }
}

/// Copies the debug name of `disp` into `name` (up to `len` bytes, null-terminated).
///
/// # Safety
///
/// If `len > 0`, `name` must point to `len` bytes of writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_dispatcher_get_debug_name(
    disp: &PagerDispatcher,
    name: *mut core::ffi::c_char,
    len: usize,
) {
    if name.is_null() || len == 0 {
        return;
    }
    // SAFETY: Caller guarantees `name` is valid for writing `len` bytes.
    let out_slice = unsafe { core::slice::from_raw_parts_mut(name.cast::<u8>(), len) };
    disp.get_debug_name(out_slice);
}
