// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    HandleValue, InitialMutability, PagerDispatcher, PortDispatcher, ProcessDispatcher,
    VmObjectDispatcher,
};
use crate::user_copy::UserOutPtr;
use crate::vm::vm_object::SupplyOptions;
use crate::vm::vm_object_paged::VmObjectPaged;
use crate::vm::vm_page_list::VmPageSpliceList;
use pin_init::stack_pin_init;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_MAX_NAME_LEN, ZX_POL_NEW_PAGER, ZX_POL_NEW_VMO, ZX_RIGHT_ATTACH_VMO, ZX_RIGHT_MANAGE_VMO,
    ZX_RIGHT_READ, ZX_RIGHT_WRITE, ZX_VMO_TRAP_DIRTY,
};

// Split out the pager vmo creation flags between PageSource and VmObjectPaged flags.
fn split_syscall_flags(mut flags: u32) -> (u32, u32) {
    // Extract any option flags relevant to creation of the page source.
    let mut src_flags = 0;
    if (flags & ZX_VMO_TRAP_DIRTY) != 0 {
        src_flags |= ZX_VMO_TRAP_DIRTY;
    }

    // Mask out any source flags. The remaining flags are the vmo creation flags; vmo creation will
    // perform validation on them.
    flags &= !ZX_VMO_TRAP_DIRTY;
    let vmo_flags = flags;
    let source_flags = src_flags;
    (source_flags, vmo_flags)
}

// zx_status_t zx_pager_create
#[syscall]
pub fn sys_pager_create(options: u32, out: &mut HandleValue) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();

    up.enforce_basic_policy(ZX_POL_NEW_PAGER)?;

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let (handle, rights) = PagerDispatcher::create()?;

    let mut proc_name = [0u8; ZX_MAX_NAME_LEN];
    if up.get_name(&mut proc_name).is_ok() {
        // Stash the name of the creating process for debugging.
        handle.dispatcher().set_debug_name(&proc_name);
    }

    *out = up.make_and_add_handle(handle, rights)?;
    Ok(())
}

// zx_status_t zx_pager_create_vmo
#[syscall]
pub fn sys_pager_create_vmo(
    pager: HandleValue,
    options: u32,
    port: HandleValue,
    key: u64,
    size: u64,
    out: &mut HandleValue,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();

    up.enforce_basic_policy(ZX_POL_NEW_VMO)?;

    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_ATTACH_VMO,
    )?;

    let port_dispatcher = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        port,
        ZX_RIGHT_WRITE,
    )?;

    let (source_flags, vmo_flags) = split_syscall_flags(options);

    let src = pager_dispatcher.create_source(port_dispatcher, key, source_flags)?;

    let stats = VmObjectDispatcher::parse_create_syscall_flags(vmo_flags, size)?;

    let vmo = VmObjectPaged::create_external(src, stats.flags, stats.size)?;

    let (kernel_handle, rights) =
        VmObjectDispatcher::create(&vmo, size, InitialMutability::Mutable)?;

    // As part of VMO creation some initial zeroing might have happened, placing the VMO in a
    // modified state. However, as the zeroing as part of the initialization, we do not want to
    // consider it modified and so reset any modification information.
    vmo.reset_pager_vmo_stats();

    *out = up.make_and_add_handle(kernel_handle, rights)?;
    Ok(())
}

// zx_status_t zx_pager_detach_vmo
#[syscall]
pub fn sys_pager_detach_vmo(pager: HandleValue, vmo: HandleValue) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();
    // TODO: Consider rights on the pager dispatcher.
    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_ATTACH_VMO | ZX_RIGHT_MANAGE_VMO,
    )?;

    let vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        vmo,
        ZX_RIGHT_WRITE,
    )?;

    if vmo_dispatcher.pager_koid() != pager_dispatcher.get_koid() {
        return Err(Status::INVALID_ARGS);
    }

    vmo_dispatcher.vmo().detach_source();
    Ok(())
}

// zx_status_t zx_pager_supply_pages
#[syscall]
pub fn sys_pager_supply_pages(
    pager: HandleValue,
    pager_vmo: HandleValue,
    offset: u64,
    size: u64,
    aux_vmo_handle: HandleValue,
    aux_offset: u64,
) -> Result<(), Status> {
    if !page::is_aligned(offset as usize)
        || !page::is_aligned(size as usize)
        || !page::is_aligned(aux_offset as usize)
    {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_MANAGE_VMO,
    )?;

    let pager_vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        pager_vmo,
        ZX_RIGHT_WRITE,
    )?;

    if pager_vmo_dispatcher.pager_koid() != pager_dispatcher.get_koid() {
        return Err(Status::INVALID_ARGS);
    }

    let aux_vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        aux_vmo_handle,
        ZX_RIGHT_READ | ZX_RIGHT_WRITE,
    )?;

    stack_pin_init!(let pages = VmPageSpliceList::new());
    aux_vmo_dispatcher.vmo().take_pages(aux_offset, size, pages.as_mut())?;

    pager_vmo_dispatcher.vmo().supply_pages(
        offset,
        size,
        pages.as_mut(),
        SupplyOptions::PagerSupply,
    )
}

// zx_status_t zx_pager_op_range
#[syscall]
pub fn sys_pager_op_range(
    pager: HandleValue,
    op: u32,
    pager_vmo: HandleValue,
    offset: u64,
    length: u64,
    data: u64,
) -> Result<(), Status> {
    if !page::is_aligned(offset as usize) || !page::is_aligned(length as usize) {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_MANAGE_VMO,
    )?;

    let pager_vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        pager_vmo,
        ZX_RIGHT_WRITE,
    )?;

    if pager_vmo_dispatcher.pager_koid() != pager_dispatcher.get_koid() {
        return Err(Status::INVALID_ARGS);
    }

    pager_dispatcher.range_op(op, pager_vmo_dispatcher.vmo(), offset, length, data)
}

// zx_status_t zx_pager_query_dirty_ranges
#[allow(clippy::too_many_arguments)]
#[syscall]
pub fn sys_pager_query_dirty_ranges(
    pager: HandleValue,
    pager_vmo: HandleValue,
    offset: u64,
    length: u64,
    buffer: UserOutPtr<u8>,
    buffer_size: usize,
    actual: UserOutPtr<usize>,
    avail: UserOutPtr<usize>,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();
    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_MANAGE_VMO,
    )?;

    let pager_vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        pager_vmo,
        ZX_RIGHT_READ,
    )?;

    if pager_vmo_dispatcher.pager_koid() != pager_dispatcher.get_koid() {
        return Err(Status::INVALID_ARGS);
    }

    pager_dispatcher.query_dirty_ranges(
        pager_vmo_dispatcher.vmo(),
        offset,
        length,
        buffer,
        buffer_size,
        actual,
        avail,
    )
}

// zx_status_t zx_pager_query_vmo_stats
#[syscall]
pub fn sys_pager_query_vmo_stats(
    pager: HandleValue,
    pager_vmo: HandleValue,
    options: u32,
    buffer: UserOutPtr<u8>,
    buffer_size: usize,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();
    let pager_dispatcher = up.handle_table().get_dispatcher_with_rights::<PagerDispatcher>(
        &up,
        pager,
        ZX_RIGHT_MANAGE_VMO,
    )?;

    let pager_vmo_dispatcher = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        pager_vmo,
        ZX_RIGHT_READ,
    )?;

    if pager_vmo_dispatcher.pager_koid() != pager_dispatcher.get_koid() {
        return Err(Status::INVALID_ARGS);
    }

    pager_dispatcher.query_pager_vmo_stats(pager_vmo_dispatcher.vmo(), options, buffer, buffer_size)
}

/// Kernel unit tests for pager syscalls.
#[cfg(ktest)]
#[unittest::suite(name = "pager_syscall_tests")]
mod tests {
    use super::{ZX_VMO_TRAP_DIRTY, split_syscall_flags};
    use zx_types::ZX_VMO_RESIZABLE;

    /// Tests splitting pager VMO creation flags into page source and VMO flags.
    #[test]
    fn test_split_syscall_flags() {
        let (src, vmo) = split_syscall_flags(0);
        unittest::expect_eq!(src, 0);
        unittest::expect_eq!(vmo, 0);

        let (src, vmo) = split_syscall_flags(ZX_VMO_TRAP_DIRTY | ZX_VMO_RESIZABLE);
        unittest::expect_eq!(src, ZX_VMO_TRAP_DIRTY);
        unittest::expect_eq!(vmo, ZX_VMO_RESIZABLE);
    }
}
