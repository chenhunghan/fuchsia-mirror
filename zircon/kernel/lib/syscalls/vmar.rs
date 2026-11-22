// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::VAddr;
use crate::object::{
    ClockDispatcher, HandleValue, IoBufferDispatcher, ProcessDispatcher, VmAddressRegionDispatcher,
    VmObjectDispatcher,
};
use crate::user_copy::{UserInOutPtr, UserOutPtr};
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use core::mem::DropGuard;
use fbl::RefPtr;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_EXECUTE, ZX_RIGHT_MAP, ZX_RIGHT_OP_CHILDREN, ZX_RIGHT_READ, ZX_RIGHT_WRITE,
    ZX_VM_ALLOW_FAULTS, ZX_VM_CAN_MAP_EXECUTE, ZX_VM_CAN_MAP_READ, ZX_VM_CAN_MAP_WRITE,
    ZX_VM_FAULT_BEYOND_STREAM_SIZE, ZX_VM_FEATURE_CAN_MAP_XOM, ZX_VM_MAP_RANGE, ZX_VM_PERM_EXECUTE,
    ZX_VM_PERM_READ, ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED, ZX_VM_PERM_WRITE,
    ZX_VM_SPECIFIC_OVERWRITE, zx_rights_t, zx_vaddr_t, zx_vm_option_t,
};

#[syscall]
pub fn sys_vmar_allocate(
    parent_vmar_handle: HandleValue,
    options: zx_vm_option_t,
    offset: usize,
    size: usize,
    child_vmar: &mut HandleValue,
    child_addr: UserOutPtr<zx_vaddr_t>,
) -> Result<(), Status> {
    // Compute needed rights from requested mapping protections.
    let mut vmar_rights: zx_rights_t = 0;
    if (options & ZX_VM_CAN_MAP_READ) != 0 {
        vmar_rights |= ZX_RIGHT_READ;
    }
    if (options & ZX_VM_CAN_MAP_WRITE) != 0 {
        vmar_rights |= ZX_RIGHT_WRITE;
    }
    if (options & ZX_VM_CAN_MAP_EXECUTE) != 0 {
        vmar_rights |= ZX_RIGHT_EXECUTE;
    }

    // lookup the dispatcher from handle
    let vmar = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VmAddressRegionDispatcher>(
            up,
            parent_vmar_handle,
            vmar_rights,
        )
    })?;

    // Create the new VMAR
    let (handle, new_rights) = vmar.allocate(offset, size, options)?;

    // Setup a handler to destroy the new VMAR if the syscall is unsuccessful.
    let vmar_dispatcher = DropGuard::new(handle.dispatcher().clone(), |vmar_dispatcher| {
        let _ = vmar_dispatcher.destroy();
    });

    child_addr.copy_to_user(&vmar_dispatcher.vmar().base().0)?;

    // Create a handle and attach the dispatcher to it
    *child_vmar = handle.make_and_add_handle(new_rights)?;

    DropGuard::dismiss(vmar_dispatcher);
    Ok(())
}

#[syscall]
pub fn sys_vmar_destroy(handle: HandleValue) -> Result<(), Status> {
    // lookup the dispatcher from handle
    let vmar = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<VmAddressRegionDispatcher>(
            up,
            handle,
            ZX_RIGHT_OP_CHILDREN,
        )
    })?;

    vmar.destroy()
}

/// Source VMO passed to [`vmar_map_common`].
enum MapVmo<'a> {
    /// A VMO backed by a [`VmObjectDispatcher`], which can lazily allocate a
    /// `StreamSizeManager` when `ZX_VM_FAULT_BEYOND_STREAM_SIZE` is requested.
    Dispatcher(&'a VmObjectDispatcher),
    /// A raw [`VmObject`] from a non-`VmObjectDispatcher` source (such as an IOBuffer or clock).
    Object(RefPtr<VmObject>),
}

/// Validates common `zx_vmar_map*` rights and options and maps `vmo` into `vmar`.
#[allow(clippy::too_many_arguments)]
fn vmar_map_common(
    mut options: zx_vm_option_t,
    vmar: RefPtr<VmAddressRegionDispatcher>,
    vmar_offset: usize,
    vmar_rights: zx_rights_t,
    vmo: MapVmo<'_>,
    vmo_offset: u64,
    vmo_rights: zx_rights_t,
    len: usize,
    mapped_addr: UserOutPtr<zx_vaddr_t>,
) -> Result<(), Status> {
    // Test to see if we should even be able to map this.
    if (vmo_rights & ZX_RIGHT_MAP) == 0 {
        return Err(Status::ACCESS_DENIED);
    }

    if (options & ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED) != 0
        && (crate::arch_rs::ops::arch_vm_features() & ZX_VM_FEATURE_CAN_MAP_XOM) == 0
    {
        options |= ZX_VM_PERM_READ;
    }

    if !VmAddressRegionDispatcher::is_valid_mapping_protection(options) {
        return Err(Status::INVALID_ARGS);
    }

    let mut do_map_range = false;
    if (options & ZX_VM_MAP_RANGE) != 0 {
        do_map_range = true;
        options &= !ZX_VM_MAP_RANGE;
    }

    if do_map_range && (options & ZX_VM_SPECIFIC_OVERWRITE) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // Usermode is not allowed to specify these flags on mappings, though we may
    // set them below.
    if (options & (ZX_VM_CAN_MAP_READ | ZX_VM_CAN_MAP_WRITE | ZX_VM_CAN_MAP_EXECUTE)) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // Permissions allowed by both the VMO and the VMAR.
    let can_read = (vmo_rights & ZX_RIGHT_READ) != 0 && (vmar_rights & ZX_RIGHT_READ) != 0;
    let can_write = (vmo_rights & ZX_RIGHT_WRITE) != 0 && (vmar_rights & ZX_RIGHT_WRITE) != 0;
    let can_exec = (vmo_rights & ZX_RIGHT_EXECUTE) != 0 && (vmar_rights & ZX_RIGHT_EXECUTE) != 0;

    // Test to see if the requested mapping protections are allowed.
    if (options & ZX_VM_PERM_READ) != 0 && !can_read {
        return Err(Status::ACCESS_DENIED);
    }
    if (options & ZX_VM_PERM_WRITE) != 0 && !can_write {
        return Err(Status::ACCESS_DENIED);
    }
    if (options & ZX_VM_PERM_EXECUTE) != 0 && !can_exec {
        return Err(Status::ACCESS_DENIED);
    }

    // If a permission is allowed by both the VMO and the VMAR, add it to the
    // flags for the new mapping, so that the VMO's rights as of now can be used
    // to constrain future permission changes via protect().
    if can_read {
        options |= ZX_VM_CAN_MAP_READ;
    }
    if can_write {
        options |= ZX_VM_CAN_MAP_WRITE;
    }
    if can_exec {
        options |= ZX_VM_CAN_MAP_EXECUTE;
    }

    if (options & ZX_VM_FAULT_BEYOND_STREAM_SIZE) != 0 {
        // Allow faults flag must be used if creating a mapping that can fault.
        if (options & ZX_VM_ALLOW_FAULTS) == 0 {
            return Err(Status::INVALID_ARGS);
        }

        match &vmo {
            MapVmo::Dispatcher(disp) => {
                // Allocate SSM if creating a fault-beyond-stream-size mapping.
                disp.ensure_stream_size_manager()?;
            }
            MapVmo::Object(vmo) => {
                // If fault beyond stream size has been requested, verify that the underlying VMO
                // is stream compatible and has a user stream size. This might be false if the
                // mapping is being created from a non-VmObjectDispatcher source.
                if !vmo.is_stream_compatible() {
                    return Err(Status::INVALID_ARGS);
                }
                let Some(paged) = vmo.as_paged() else {
                    return Err(Status::INVALID_ARGS);
                };
                ksync::lock!(let guard = paged.lock());
                if paged.user_stream_size_locked(guard.token()).is_none() {
                    return Err(Status::INVALID_ARGS);
                }
            }
        }
    }

    let vmo = match vmo {
        MapVmo::Dispatcher(disp) => disp.vmo().clone(),
        MapVmo::Object(vmo) => vmo,
    };

    let map_result = vmar.map(vmar_offset, vmo, vmo_offset, len, options)?;

    // Setup a handler to destroy the new mapping if the syscall is unsuccessful.
    let mapping = DropGuard::new(map_result.mapping, |mapping| {
        let _ = mapping.destroy();
    });

    if do_map_range {
        // Mappings may have already been created due to memory priority, so need to ignore
        // existing. Ignoring existing mappings is safe here as we are always free to populate and
        // destroy page table mappings for user addresses.
        mapping.map_range(0, len, /*commit=*/ false, /*ignore_existing=*/ true)?;
    }

    mapped_addr.copy_to_user(&map_result.base)?;

    let mapping = DropGuard::dismiss(mapping);

    // This mapping will now always be used via the aspace so it is free to be merged into different
    // actual mapping objects.
    VmMapping::mark_mergeable(mapping);

    Ok(())
}

#[syscall]
pub fn sys_vmar_map(
    handle: HandleValue,
    options: zx_vm_option_t,
    vmar_offset: usize,
    vmo_handle: HandleValue,
    vmo_offset: u64,
    len: usize,
    mapped_addr: UserOutPtr<zx_vaddr_t>,
) -> Result<(), Status> {
    let ((vmar, vmar_rights), (vmo, vmo_rights)) =
        ProcessDispatcher::with_current(|up| -> Result<_, Status> {
            // lookup the VMAR dispatcher from handle
            let vmar = up
                .handle_table()
                .get_dispatcher_and_rights::<VmAddressRegionDispatcher>(up, handle)?;

            // lookup the VMO dispatcher from handle
            let vmo = up
                .handle_table()
                .get_dispatcher_and_rights::<VmObjectDispatcher>(up, vmo_handle)?;
            Ok((vmar, vmo))
        })?;

    vmar_map_common(
        options,
        vmar,
        vmar_offset,
        vmar_rights,
        MapVmo::Dispatcher(&vmo),
        vmo_offset,
        vmo_rights,
        len,
        mapped_addr,
    )
}

#[syscall]
pub fn sys_vmar_unmap(handle: HandleValue, addr: zx_vaddr_t, len: usize) -> Result<(), Status> {
    // lookup the dispatcher from handle
    let (vmar, vmar_rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_and_rights::<VmAddressRegionDispatcher>(up, handle)
    })?;

    vmar.unmap(VAddr(addr), len, VmAddressRegionDispatcher::op_children_from_rights(vmar_rights))
}

#[syscall]
pub fn sys_vmar_protect(
    handle: HandleValue,
    mut options: zx_vm_option_t,
    addr: zx_vaddr_t,
    len: usize,
) -> Result<(), Status> {
    if (options & ZX_VM_PERM_READ_IF_XOM_UNSUPPORTED) != 0
        && (crate::arch_rs::ops::arch_vm_features() & ZX_VM_FEATURE_CAN_MAP_XOM) == 0
    {
        options |= ZX_VM_PERM_READ;
    }

    let mut vmar_rights: zx_rights_t = 0;
    if (options & ZX_VM_PERM_READ) != 0 {
        vmar_rights |= ZX_RIGHT_READ;
    }
    if (options & ZX_VM_PERM_WRITE) != 0 {
        vmar_rights |= ZX_RIGHT_WRITE;
    }
    if (options & ZX_VM_PERM_EXECUTE) != 0 {
        vmar_rights |= ZX_RIGHT_EXECUTE;
    }

    // lookup the dispatcher from handle
    let (vmar, vmar_rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights_and_actual::<VmAddressRegionDispatcher>(
            up,
            handle,
            vmar_rights,
        )
    })?;

    if !VmAddressRegionDispatcher::is_valid_mapping_protection(options) {
        return Err(Status::INVALID_ARGS);
    }

    vmar.protect(
        VAddr(addr),
        len,
        options,
        VmAddressRegionDispatcher::op_children_from_rights(vmar_rights),
    )
}

#[syscall]
pub fn sys_vmar_op_range(
    handle: HandleValue,
    op: u32,
    addr: zx_vaddr_t,
    len: usize,
    buffer: UserInOutPtr<u8>,
    buffer_size: usize,
) -> Result<(), Status> {
    let (vmar, vmar_rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_and_rights::<VmAddressRegionDispatcher>(up, handle)
    })?;

    vmar.range_op(op, VAddr(addr), len, vmar_rights, buffer, buffer_size)
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
    let ((vmar, vmar_rights), (iob, iob_rights)) =
        ProcessDispatcher::with_current(|up| -> Result<_, Status> {
            let vmar = up
                .handle_table()
                .get_dispatcher_and_rights::<VmAddressRegionDispatcher>(up, handle)?;
            let iob = up.handle_table().get_dispatcher_and_rights::<IoBufferDispatcher>(up, ep)?;
            Ok((vmar, iob))
        })?;

    if region_index as usize >= iob.region_count() {
        return Err(Status::OUT_OF_RANGE);
    }

    let vmo = iob.create_mappable_vmo_for_region(region_index as usize)?;
    let region_rights = iob.get_map_rights(iob_rights, region_index as usize);

    vmar_map_common(
        options,
        vmar,
        vmar_offset,
        vmar_rights,
        MapVmo::Object(vmo),
        region_offset,
        region_rights,
        region_length,
        mapped_addr,
    )
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
    let (clock, clock_rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_and_rights::<ClockDispatcher>(up, clock_handle)
    })?;

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
    let (vmar, vmar_rights) = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_and_rights::<VmAddressRegionDispatcher>(up, handle)
    })?;

    // In order to map a clock, users must have both the READ and MAP permissions.
    // Mask out all of the other permissions to act as the "effective" permissions
    // for the underlying VMO that this clock owns.  We will pass these effective
    // rights as the VMO rights to the common mapping function.
    const REQUIRED_CLOCK_RIGHTS: zx_rights_t = ZX_RIGHT_READ | ZX_RIGHT_MAP;
    let effective_vmo_rights = clock_rights & REQUIRED_CLOCK_RIGHTS;

    // Finally hand off the map operation to the common map routine.
    vmar_map_common(
        options,
        vmar,
        vmar_offset,
        vmar_rights,
        MapVmo::Object(clock_vmo),
        0,
        effective_vmo_rights,
        len,
        mapped_addr,
    )
}

/// Kernel unit tests for VMAR syscalls.
#[cfg(ktest)]
#[unittest::suite(name = "vmar_syscall_tests")]
mod tests {
    use super::{
        MapVmo, RefPtr, Status, VAddr, VmAddressRegionDispatcher, VmObject, VmObjectDispatcher,
        vmar_map_common,
    };
    use crate::object::InitialMutability;
    use crate::user_memory::UserMemory;
    use crate::vm::arch_vm_aspace::ARCH_MMU_FLAG_PERM_USER;
    use crate::vm::pmm;
    use crate::vm::vm_aspace::{Type, VmAspace};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm::vm_object_physical::VmObjectPhysical;
    use core::mem::size_of;
    use unittest::{
        expect_eq, expect_err, expect_false, expect_ne, expect_ok, expect_true, unwrap_ok,
        unwrap_some,
    };
    use zx_types::{
        ZX_RIGHT_EXECUTE, ZX_RIGHT_MAP, ZX_RIGHT_READ, ZX_RIGHT_WRITE, ZX_VM_ALLOW_FAULTS,
        ZX_VM_CAN_MAP_EXECUTE, ZX_VM_CAN_MAP_READ, ZX_VM_CAN_MAP_WRITE,
        ZX_VM_FAULT_BEYOND_STREAM_SIZE, ZX_VM_MAP_RANGE, ZX_VM_PERM_EXECUTE, ZX_VM_PERM_READ,
        ZX_VM_PERM_WRITE, ZX_VM_SPECIFIC_OVERWRITE, zx_vaddr_t,
    };

    /// Returns whether `paged` currently has an attached `StreamSizeManager`.
    fn has_user_stream_size(paged: &VmObjectPaged) -> bool {
        ksync::lock!(let guard = paged.lock());
        paged.user_stream_size_locked(guard.token()).is_some()
    }

    /// Verifies SSM allocation is deferred until rights and mapping options are validated.
    #[test]
    fn test_map_ssm_deferred_until_validated() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"vmar-syscall-ssm"));
        let mut destroy_aspace = zr::defer(|| {
            aspace.destroy().expect("failed to destroy test VmAspace");
        });
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let (vmar_handle, vmar_rights) =
            unwrap_ok!(VmAddressRegionDispatcher::create(root_vmar, ARCH_MMU_FLAG_PERM_USER));
        let vmar = vmar_handle.dispatcher();

        // Bounded paged VMO (`stream_size == vmo_size`) defers creating a `StreamSizeManager`.
        let paged = unwrap_ok!(VmObjectPaged::create(0, 0, page::SIZE as u64));
        let (vmo_handle, vmo_rights) = unwrap_ok!(VmObjectDispatcher::create(
            &paged,
            page::SIZE as u64,
            InitialMutability::Mutable,
        ));
        let disp = vmo_handle.dispatcher();
        expect_false!(has_user_stream_size(&paged));

        let out_mem = unwrap_some!(UserMemory::create(size_of::<zx_vaddr_t>()));
        expect_ok!(out_mem.commit_and_map(0..page::SIZE));
        let out_addr = out_mem.user_out::<zx_vaddr_t>();
        const VALID_OPTS: u32 =
            ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS;
        // (options, vmar_rights_clear, vmar_rights_set, vmo_rights_clear, expected_status)
        const REJECTED_CASES: &[(u32, u32, u32, u32, Status)] = &[
            // Missing ZX_RIGHT_MAP on VMO.
            (VALID_OPTS, 0, 0, ZX_RIGHT_MAP, Status::ACCESS_DENIED),
            // Invalid protection combination (write without read).
            (
                ZX_VM_PERM_WRITE | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS,
                0,
                0,
                0,
                Status::INVALID_ARGS,
            ),
            // Incompatible ZX_VM_MAP_RANGE | ZX_VM_SPECIFIC_OVERWRITE.
            (
                VALID_OPTS | ZX_VM_MAP_RANGE | ZX_VM_SPECIFIC_OVERWRITE,
                0,
                0,
                0,
                Status::INVALID_ARGS,
            ),
            // Disallowed user-specified ZX_VM_CAN_MAP_* flags.
            (VALID_OPTS | ZX_VM_CAN_MAP_READ, 0, 0, 0, Status::INVALID_ARGS),
            (VALID_OPTS | ZX_VM_CAN_MAP_WRITE, 0, 0, 0, Status::INVALID_ARGS),
            (VALID_OPTS | ZX_VM_CAN_MAP_EXECUTE, 0, 0, 0, Status::INVALID_ARGS),
            // Missing ZX_RIGHT_READ on VMO or VMAR.
            (VALID_OPTS, 0, 0, ZX_RIGHT_READ, Status::ACCESS_DENIED),
            (VALID_OPTS, ZX_RIGHT_READ, 0, 0, Status::ACCESS_DENIED),
            // Missing ZX_RIGHT_WRITE on VMO or VMAR.
            (VALID_OPTS | ZX_VM_PERM_WRITE, 0, 0, ZX_RIGHT_WRITE, Status::ACCESS_DENIED),
            (VALID_OPTS | ZX_VM_PERM_WRITE, ZX_RIGHT_WRITE, 0, 0, Status::ACCESS_DENIED),
            // Missing ZX_RIGHT_EXECUTE on VMO.
            (
                VALID_OPTS | ZX_VM_PERM_EXECUTE,
                0,
                ZX_RIGHT_EXECUTE,
                ZX_RIGHT_EXECUTE,
                Status::ACCESS_DENIED,
            ),
            // Missing ZX_VM_ALLOW_FAULTS when ZX_VM_FAULT_BEYOND_STREAM_SIZE is set.
            (ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE, 0, 0, 0, Status::INVALID_ARGS),
        ];

        for &(opts, vmar_clear, vmar_set, vmo_clear, expected_err) in REJECTED_CASES {
            expect_err!(
                vmar_map_common(
                    opts,
                    vmar.clone(),
                    0,
                    (vmar_rights & !vmar_clear) | vmar_set,
                    MapVmo::Dispatcher(disp),
                    0,
                    vmo_rights & !vmo_clear,
                    page::SIZE,
                    out_addr,
                ),
                expected_err
            );
            expect_false!(has_user_stream_size(&paged));
        }
        expect_eq!(unwrap_ok!(out_mem.get::<zx_vaddr_t>(0)), 0);

        destroy_aspace.cancel();
        drop(destroy_aspace);
        expect_ok!(aspace.destroy());
    }

    /// Verifies SSM is allocated and attached once rights and mapping options are valid.
    #[test]
    fn test_map_ssm_allocated_when_valid() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"vmar-syscall-ok"));
        let mut destroy_aspace = zr::defer(|| {
            aspace.destroy().expect("failed to destroy test VmAspace");
        });
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let (vmar_handle, vmar_rights) =
            unwrap_ok!(VmAddressRegionDispatcher::create(root_vmar, ARCH_MMU_FLAG_PERM_USER));
        let vmar = vmar_handle.dispatcher();

        let paged = unwrap_ok!(VmObjectPaged::create(0, 0, page::SIZE as u64));
        let (vmo_handle, vmo_rights) = unwrap_ok!(VmObjectDispatcher::create(
            &paged,
            page::SIZE as u64,
            InitialMutability::Mutable,
        ));
        expect_false!(has_user_stream_size(&paged));

        let out_mem = unwrap_some!(UserMemory::create(size_of::<zx_vaddr_t>()));
        expect_ok!(out_mem.commit_and_map(0..page::SIZE));
        expect_ok!(vmar_map_common(
            ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS,
            vmar.clone(),
            0,
            vmar_rights,
            MapVmo::Dispatcher(vmo_handle.dispatcher()),
            0,
            vmo_rights,
            page::SIZE,
            out_mem.user_out::<zx_vaddr_t>(),
        ));
        expect_true!(has_user_stream_size(&paged));

        let mapped = unwrap_ok!(out_mem.get::<zx_vaddr_t>(0));
        expect_ne!(mapped, 0);
        expect_ok!(vmar.unmap(
            VAddr(mapped),
            page::SIZE,
            VmAddressRegionDispatcher::op_children_from_rights(vmar_rights),
        ));
        destroy_aspace.cancel();
        drop(destroy_aspace);
        expect_ok!(aspace.destroy());
    }

    /// Default valid mapping options for `ZX_VM_FAULT_BEYOND_STREAM_SIZE` test cases.
    const NON_STREAM_VALID_OPTS: u32 =
        ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS;

    /// `(options, vmo_rights_clear, expected_status)` table for non-stream-compatible VMOs.
    const NON_STREAM_CASES: &[(u32, u32, Status)] = &[
        // Rights errors must take precedence over Status::NOT_SUPPORTED.
        (NON_STREAM_VALID_OPTS, ZX_RIGHT_MAP, Status::ACCESS_DENIED),
        (NON_STREAM_VALID_OPTS, ZX_RIGHT_READ, Status::ACCESS_DENIED),
        (NON_STREAM_VALID_OPTS | ZX_VM_PERM_WRITE, ZX_RIGHT_WRITE, Status::ACCESS_DENIED),
        // Option validation errors must take precedence over Status::NOT_SUPPORTED.
        (
            ZX_VM_PERM_WRITE | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS,
            0,
            Status::INVALID_ARGS,
        ),
        (
            NON_STREAM_VALID_OPTS | ZX_VM_MAP_RANGE | ZX_VM_SPECIFIC_OVERWRITE,
            0,
            Status::INVALID_ARGS,
        ),
        (NON_STREAM_VALID_OPTS | ZX_VM_CAN_MAP_READ, 0, Status::INVALID_ARGS),
        (ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE, 0, Status::INVALID_ARGS),
        // With valid rights and options, non-stream-compatible VMOs return NOT_SUPPORTED.
        (NON_STREAM_VALID_OPTS, 0, Status::NOT_SUPPORTED),
    ];

    /// Verifies error precedence on contiguous paged VMO dispatchers.
    #[test]
    fn test_map_contiguous_vmo_error_precedence() {
        let aspace = unwrap_some!(VmAspace::create(Type::User, b"vmar-syscall-cnt"));
        let mut destroy_aspace = zr::defer(|| {
            aspace.destroy().expect("failed to destroy test VmAspace");
        });
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let (vmar_handle, vmar_rights) =
            unwrap_ok!(VmAddressRegionDispatcher::create(root_vmar, ARCH_MMU_FLAG_PERM_USER));
        let vmar = vmar_handle.dispatcher();

        let contig_vmo = unwrap_ok!(VmObjectPaged::create_contiguous(0, page::SIZE as u64, 0));
        let (contig_handle, contig_rights) = unwrap_ok!(VmObjectDispatcher::create(
            &contig_vmo,
            page::SIZE as u64,
            InitialMutability::Mutable,
        ));
        let out_mem = unwrap_some!(UserMemory::create(size_of::<zx_vaddr_t>()));
        expect_ok!(out_mem.commit_and_map(0..page::SIZE));
        let out_addr = out_mem.user_out::<zx_vaddr_t>();
        for &(opts, vmo_clear, expected_err) in NON_STREAM_CASES {
            expect_err!(
                vmar_map_common(
                    opts,
                    vmar.clone(),
                    0,
                    vmar_rights,
                    MapVmo::Dispatcher(contig_handle.dispatcher()),
                    0,
                    contig_rights & !vmo_clear,
                    page::SIZE,
                    out_addr,
                ),
                expected_err
            );
        }
        expect_eq!(unwrap_ok!(out_mem.get::<zx_vaddr_t>(0)), 0);

        destroy_aspace.cancel();
        drop(destroy_aspace);
        expect_ok!(aspace.destroy());
    }

    /// Verifies error precedence on physical VMO dispatchers.
    #[test]
    fn test_map_physical_vmo_error_precedence() {
        let (vm_page, pa) = unwrap_ok!(pmm::alloc_page(0));
        // MUST remain declared before `aspace`, `destroy_aspace`, `phys_vmo`, and `phys_handle` so
        // `vm_page` is freed only after `aspace` is destroyed and all `phys_vmo` references drop.
        let _free_page = zr::defer(|| {
            // SAFETY: `vm_page` was allocated via `pmm::alloc_page` above, is never loaned or
            // placed in a page queue, and `_free_page` runs only after `aspace` is destroyed and
            // all references to `phys_vmo` have been dropped.
            unsafe { pmm::free_page(vm_page) };
        });

        let aspace = unwrap_some!(VmAspace::create(Type::User, b"vmar-syscall-phy"));
        let mut destroy_aspace = zr::defer(|| {
            aspace.destroy().expect("failed to destroy test VmAspace");
        });
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let (vmar_handle, vmar_rights) =
            unwrap_ok!(VmAddressRegionDispatcher::create(root_vmar, ARCH_MMU_FLAG_PERM_USER));
        let vmar = vmar_handle.dispatcher();

        let phys_vmo = unwrap_ok!(VmObjectPhysical::create(pa, page::SIZE));
        let (phys_handle, phys_rights) = unwrap_ok!(VmObjectDispatcher::create(
            &phys_vmo,
            page::SIZE as u64,
            InitialMutability::Mutable,
        ));
        let out_mem = unwrap_some!(UserMemory::create(size_of::<zx_vaddr_t>()));
        expect_ok!(out_mem.commit_and_map(0..page::SIZE));
        let out_addr = out_mem.user_out::<zx_vaddr_t>();
        for &(opts, vmo_clear, expected_err) in NON_STREAM_CASES {
            expect_err!(
                vmar_map_common(
                    opts,
                    vmar.clone(),
                    0,
                    vmar_rights,
                    MapVmo::Dispatcher(phys_handle.dispatcher()),
                    0,
                    phys_rights & !vmo_clear,
                    page::SIZE,
                    out_addr,
                ),
                expected_err
            );
        }
        expect_eq!(unwrap_ok!(out_mem.get::<zx_vaddr_t>(0)), 0);

        destroy_aspace.cancel();
        drop(destroy_aspace);
        expect_ok!(aspace.destroy());
    }

    /// Verifies `MapVmo::Object` callers reject `ZX_VM_FAULT_BEYOND_STREAM_SIZE` without an SSM.
    #[test]
    fn test_map_object_vmo_without_ssm() {
        let (vm_page, pa) = unwrap_ok!(pmm::alloc_page(0));
        // MUST remain declared before `aspace`, `destroy_aspace`, `phys_vmo`, and `raw_vmos` so
        // `vm_page` is freed only after `aspace` is destroyed and all `phys_vmo` references drop.
        let _free_page = zr::defer(|| {
            // SAFETY: `vm_page` was allocated via `pmm::alloc_page` above, is never loaned or
            // placed in a page queue, and `_free_page` runs only after `aspace` is destroyed and
            // all references to `phys_vmo` have been dropped.
            unsafe { pmm::free_page(vm_page) };
        });

        let aspace = unwrap_some!(VmAspace::create(Type::User, b"vmar-syscall-obj"));
        let mut destroy_aspace = zr::defer(|| {
            aspace.destroy().expect("failed to destroy test VmAspace");
        });
        let root_vmar = unwrap_some!(aspace.root_vmar());
        let (vmar_handle, vmar_rights) =
            unwrap_ok!(VmAddressRegionDispatcher::create(root_vmar, ARCH_MMU_FLAG_PERM_USER));
        let vmar = vmar_handle.dispatcher();

        let paged_no_ssm = unwrap_ok!(VmObjectPaged::create(0, 0, page::SIZE as u64));
        let contig_vmo = unwrap_ok!(VmObjectPaged::create_contiguous(0, page::SIZE as u64, 0));
        let phys_vmo = unwrap_ok!(VmObjectPhysical::create(pa, page::SIZE));

        let out_mem = unwrap_some!(UserMemory::create(size_of::<zx_vaddr_t>()));
        expect_ok!(out_mem.commit_and_map(0..page::SIZE));
        let out_addr = out_mem.user_out::<zx_vaddr_t>();
        let valid_opts = ZX_VM_PERM_READ | ZX_VM_FAULT_BEYOND_STREAM_SIZE | ZX_VM_ALLOW_FAULTS;
        let raw_vmos: [RefPtr<VmObject>; 3] = [
            VmObjectPaged::into_vm_object(paged_no_ssm.clone()),
            VmObjectPaged::into_vm_object(contig_vmo),
            VmObjectPhysical::into_vm_object(phys_vmo),
        ];
        for raw_vmo in raw_vmos {
            // Rights check runs before non-dispatcher stream-size validation.
            expect_err!(
                vmar_map_common(
                    valid_opts,
                    vmar.clone(),
                    0,
                    vmar_rights,
                    MapVmo::Object(raw_vmo.clone()),
                    0,
                    ZX_RIGHT_READ,
                    page::SIZE,
                    out_addr,
                ),
                Status::ACCESS_DENIED
            );
            // Non-dispatcher VMOs without a StreamSizeManager must return INVALID_ARGS.
            expect_err!(
                vmar_map_common(
                    valid_opts,
                    vmar.clone(),
                    0,
                    vmar_rights,
                    MapVmo::Object(raw_vmo),
                    0,
                    ZX_RIGHT_MAP | ZX_RIGHT_READ,
                    page::SIZE,
                    out_addr,
                ),
                Status::INVALID_ARGS
            );
        }
        expect_eq!(unwrap_ok!(out_mem.get::<zx_vaddr_t>(0)), 0);
        expect_false!(has_user_stream_size(&paged_no_ssm));

        destroy_aspace.cancel();
        drop(destroy_aspace);
        expect_ok!(aspace.destroy());
    }
}
