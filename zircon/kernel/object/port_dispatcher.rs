// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::dispatcher::impl_dispatcher_facade;
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{ZX_OBJ_TYPE_PORT, zx_rights_t, zx_status_t};

unsafe extern "C" {
    fn cpp_port_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<PortDispatcher>>,
        rights_out: *mut MaybeUninit<zx_rights_t>,
    ) -> zx_status_t;
}

impl_dispatcher_facade!(
    /// Facade for C++ `PortDispatcher`.
    pub struct PortDispatcher,
    ZX_OBJ_TYPE_PORT
);

impl PortDispatcher {
    /// Creates a new `PortDispatcher` with `options`.
    pub fn create(options: u32) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `cpp_port_dispatcher_create` initializes both out parameters on `ZX_OK`.
        unsafe {
            KernelHandle::create_with_rights(|h, r| cpp_port_dispatcher_create(options, h, r))
        }
    }
}
