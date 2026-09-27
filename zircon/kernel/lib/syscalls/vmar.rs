// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    ClockDispatcher, Dispatcher, HandleValue, IoBufferDispatcher, VmAddressRegionDispatcher,
};
use crate::user_copy::UserOutPtr;
use crate::vm::vm_object::VmObject;
use fbl::RefPtr;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_MAP, ZX_RIGHT_READ, ZX_VM_PERM_EXECUTE, ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED,
    ZX_VM_PERM_WRITE, zx_rights_t, zx_status_t, zx_vaddr_t, zx_vm_option_t,
};

unsafe extern "C" {
    fn cpp_vmar_map_common(
        options: zx_vm_option_t,
        vmar: *mut VmAddressRegionDispatcher,
        vmar_offset: u64,
        vmar_rights: zx_rights_t,
        vmo: *mut VmObject,
        vmo_offset: u64,
        vmo_rights: zx_rights_t,
        len: u64,
        mapped_addr: *mut zx_vaddr_t,
    ) -> zx_status_t;
}

#[syscall]
pub fn sys_vmar_map_iob(
    handle: HandleValue,
    options: zx_vm_option_t,
    vmar_offset: usize,
    ep: HandleValue,
    region_index: u32,
    region_offset: u64,
    region_length: usize,
    mapped_addr: UserOutPtr<zx_vaddr_t>,
) -> Result<(), Status> {
    let (vmar, vmar_rights) = Dispatcher::get_and_rights::<VmAddressRegionDispatcher>(handle)?;
    let (iob, iob_rights) = Dispatcher::get_and_rights::<IoBufferDispatcher>(ep)?;

    if region_index as usize >= iob.region_count() {
        return Err(Status::OUT_OF_RANGE);
    }

    let vmo = iob.create_mappable_vmo_for_region(region_index as usize)?;
    let region_rights = iob.get_map_rights(iob_rights, region_index as usize);

    // SAFETY: We transfer ownership of the `RefPtr` references into `cpp_vmar_map_common`
    // via `RefPtr::into_raw`, which `ImportFromRawPtr` reclaims on the C++ side.
    let status = unsafe {
        cpp_vmar_map_common(
            options,
            RefPtr::into_raw(vmar).cast_mut(),
            vmar_offset as u64,
            vmar_rights,
            RefPtr::into_raw(vmo).cast_mut(),
            region_offset,
            region_rights,
            region_length as u64,
            mapped_addr.as_ptr(),
        )
    };
    Status::ok(status)?;
    Ok(())
}

#[syscall]
pub fn sys_vmar_map_clock(
    handle: HandleValue,
    options: zx_vm_option_t,
    vmar_offset: usize,
    clock_handle: HandleValue,
    len: usize,
    mapped_addr: UserOutPtr<zx_vaddr_t>,
) -> Result<(), Status> {
    // Pretty much all of the options are allowed when attempting to map a clock's
    // VMO, but not all of them.  Check out the options requested by the user and
    // reject the call if any of the explicitly disallowed options are present in
    // the request.  Leave the rest of the option validation logic to the common
    // map routine.
    const DISALLOWED_OPTIONS: zx_vm_option_t =
        ZX_VM_PERM_WRITE | ZX_VM_PERM_EXECUTE | ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED;
    if (options & DISALLOWED_OPTIONS) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // The length of the requested mapping must be what we expect it to be, in
    // this case, the value reported by the ZX_INFO_CLOCK_MAPPED_SIZE topic.
    // Anything else is an error.
    if (len as u64) != ClockDispatcher::MAPPED_SIZE {
        return Err(Status::INVALID_ARGS);
    }

    // lookup the Clock dispatcher from handle
    let (clock, clock_rights) = Dispatcher::get_and_rights::<ClockDispatcher>(clock_handle)?;

    // If this is not a mappable clock, then there is no point in proceeding.
    if !clock.is_mappable() {
        return Err(Status::INVALID_ARGS);
    }

    // Grab a reference to the internal VMO which we can pass to the common map
    // routine.  It should be impossible to have successfully created a clock
    // whose options indicate that it is mappable, but which does not have a valid
    // underlying VMO.
    let clock_vmo: RefPtr<VmObject> = clock.vmo().cloned().unwrap();

    // lookup the VMAR dispatcher from handle
    let (vmar, vmar_rights) = Dispatcher::get_and_rights::<VmAddressRegionDispatcher>(handle)?;

    // In order to map a clock, users must have both the READ and MAP permissions.
    // Mask out all of the other permissions to act as the "effective" permissions
    // for the underlying VMO that this clock owns.  We will pass these effective
    // rights as the VMO rights to the common mapping function.
    const REQUIRED_CLOCK_RIGHTS: zx_rights_t = ZX_RIGHT_READ | ZX_RIGHT_MAP;
    let effective_vmo_rights = clock_rights & REQUIRED_CLOCK_RIGHTS;

    // Finally hand off the map operation to the common map routine.
    // SAFETY: We transfer ownership of the `RefPtr` references into `cpp_vmar_map_common`
    // via `RefPtr::into_raw`, which `ImportFromRawPtr` reclaims on the C++ side.
    let status = unsafe {
        cpp_vmar_map_common(
            options,
            RefPtr::into_raw(vmar).cast_mut(),
            vmar_offset as u64,
            vmar_rights,
            RefPtr::into_raw(clock_vmo).cast_mut(),
            0,
            effective_vmo_rights,
            len as u64,
            mapped_addr.as_ptr(),
        )
    };
    Status::ok(status)?;
    Ok(())
}
