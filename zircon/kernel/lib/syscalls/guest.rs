// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    Dispatcher, GuestDispatcher, HandleValue, PortDispatcher, ProcessDispatcher,
    validate_ranged_resource,
};
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_RIGHT_WRITE, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_HYPERVISOR_BASE,
    zx_vaddr_t,
};

#[syscall]
pub fn sys_guest_create(
    resource: HandleValue,
    options: u32,
    guest_handle: &mut HandleValue,
    vmar_handle: &mut HandleValue,
) -> Result<(), Status> {
    validate_ranged_resource(resource, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_HYPERVISOR_BASE, 1)?;

    let (new_guest_handle, guest_rights, new_vmar_handle, vmar_rights) =
        GuestDispatcher::create(options)?;

    let (g_handle, v_handle) = ProcessDispatcher::with_current(|up| -> Result<_, Status> {
        let g = up.make_and_add_handle(new_guest_handle, guest_rights)?;
        let v = up.make_and_add_handle(new_vmar_handle, vmar_rights)?;
        Ok((g, v))
    })?;

    *guest_handle = g_handle;
    *vmar_handle = v_handle;
    Ok(())
}

#[syscall]
pub fn sys_guest_set_trap(
    handle: HandleValue,
    kind: u32,
    addr: zx_vaddr_t,
    size: usize,
    port_handle: HandleValue,
    key: u64,
) -> Result<(), Status> {
    let guest = Dispatcher::get_with_rights::<GuestDispatcher>(handle, ZX_RIGHT_WRITE)?;

    let port = if port_handle.raw_value() != ZX_HANDLE_INVALID {
        Some(Dispatcher::get_with_rights::<PortDispatcher>(port_handle, ZX_RIGHT_WRITE)?)
    } else {
        None
    };

    guest.set_trap(kind, addr, size, port, key)
}
