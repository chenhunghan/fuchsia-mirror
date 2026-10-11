// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{GuestDispatcher, HandleValue, ProcessDispatcher, VcpuDispatcher};
use crate::user_copy::{UserInPtr, UserOutPtr};
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_EXECUTE, ZX_RIGHT_MANAGE_THREAD, ZX_RIGHT_READ, ZX_RIGHT_SIGNAL, ZX_RIGHT_WRITE,
    ZX_VCPU_IO, ZX_VCPU_STATE, zx_port_packet_t, zx_vaddr_t, zx_vcpu_io_t, zx_vcpu_state_t,
};

#[syscall]
pub fn sys_vcpu_create(
    guest_handle: HandleValue,
    options: u32,
    entry: zx_vaddr_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let guest = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<GuestDispatcher>(
            up,
            guest_handle,
            ZX_RIGHT_MANAGE_THREAD,
        )
    })?;

    let (kernel_handle, rights) = VcpuDispatcher::create(guest, entry)?;
    *out = kernel_handle.make_and_add_handle(rights)?;
    Ok(())
}

#[syscall]
pub fn sys_vcpu_enter(
    handle: HandleValue,
    user_packet: UserOutPtr<zx_port_packet_t>,
) -> Result<(), Status> {
    let vcpu = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VcpuDispatcher>(up, handle, ZX_RIGHT_EXECUTE)
    })?;

    let mut packet = zx_port_packet_t::default();
    vcpu.enter(&mut packet)?;

    user_packet.copy_to_user(&packet)
}

#[syscall]
pub fn sys_vcpu_kick(handle: HandleValue) -> Result<(), Status> {
    let vcpu = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VcpuDispatcher>(up, handle, ZX_RIGHT_EXECUTE)
    })?;
    vcpu.kick();
    Ok(())
}

#[syscall]
pub fn sys_vcpu_interrupt(handle: HandleValue, vector: u32) -> Result<(), Status> {
    let vcpu = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VcpuDispatcher>(up, handle, ZX_RIGHT_SIGNAL)
    })?;
    vcpu.interrupt(vector)
}

#[syscall]
pub fn sys_vcpu_read_state(
    handle: HandleValue,
    kind: u32,
    user_buffer: UserOutPtr<u8>,
    buffer_size: usize,
) -> Result<(), Status> {
    let vcpu = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VcpuDispatcher>(up, handle, ZX_RIGHT_READ)
    })?;

    if kind != ZX_VCPU_STATE || buffer_size != core::mem::size_of::<zx_vcpu_state_t>() {
        return Err(Status::INVALID_ARGS);
    }

    let state = vcpu.read_state()?;

    user_buffer.reinterpret::<zx_vcpu_state_t>().copy_to_user(&state)
}

#[syscall]
pub fn sys_vcpu_write_state(
    handle: HandleValue,
    kind: u32,
    user_buffer: UserInPtr<u8>,
    buffer_size: usize,
) -> Result<(), Status> {
    let vcpu = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VcpuDispatcher>(up, handle, ZX_RIGHT_WRITE)
    })?;

    match kind {
        ZX_VCPU_STATE => {
            if buffer_size != core::mem::size_of::<zx_vcpu_state_t>() {
                return Err(Status::INVALID_ARGS);
            }
            let state = user_buffer.reinterpret::<zx_vcpu_state_t>().read()?;
            vcpu.write_state(&state)
        }
        ZX_VCPU_IO => {
            if buffer_size != core::mem::size_of::<zx_vcpu_io_t>() {
                return Err(Status::INVALID_ARGS);
            }
            let io_state = user_buffer.reinterpret::<zx_vcpu_io_t>().read()?;
            vcpu.write_io_state(&io_state)
        }
        _ => Err(Status::INVALID_ARGS),
    }
}
