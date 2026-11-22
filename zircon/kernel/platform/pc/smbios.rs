// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! SMBIOS support for PC platform.

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_READ};
use crate::vm::vm_aspace::VmAspace;
use core::ptr::NonNull;
use handoff::PhysHandoff;
use lazy_init::LazyInit;
use smbios::{
    BiosInformationStruct2_0, BiosInformationStruct2_4, EntryPoint, EntryPoint2_1,
    EntryPointVersion, Header, SMBIOS2_ANCHOR, SMBIOS3_ANCHOR, SpecVersion, StringTable,
    StructType, SystemInformationStruct2_0, SystemInformationStruct2_1, SystemInformationStruct2_4,
};
use zerocopy::FromBytes;
use zx_status::Status;

unsafe extern "C" {
    fn printf(format: *const core::ffi::c_char, ...) -> core::ffi::c_int;
}

/// Holds the global SMBIOS state.
#[derive(Debug, Default)]
struct SmbiosState {
    version: Option<EntryPointVersion>,
    ep2_1: Option<EntryPoint2_1>,
    struct_base: usize,
    struct_len: usize,
}

static SMBIOS_STATE: LazyInit<SmbiosState> = LazyInit::uninit();

/// Maximum entry point size to map when searching for the anchor.
const MAX_EP_SIZE: usize = 31;

/// Maps a physical memory range into the kernel address space.
///
/// Returns a tuple of `(virt_ptr, mapping_base_addr)`.
///
/// # Safety
///
/// `[paddr, paddr + len)` expanded to page boundaries is a physical range for which
/// `ARCH_MMU_FLAG_CACHED` is appropriate for all mappings, and creating a read-only kernel mapping
/// of it is sound.
unsafe fn map_range(paddr: u64, len: usize) -> Result<(NonNull<u8>, usize), Status> {
    if paddr == 0 || len == 0 {
        return Err(Status::INVALID_ARGS);
    }
    let paddr_usize = usize::try_from(paddr).map_err(|_| Status::INVALID_ARGS)?;
    let paddr_end = paddr_usize.checked_add(len).ok_or(Status::INVALID_ARGS)?;
    let paddr_base = page::round_down(paddr_usize);
    let paddr_top = page::round_up(paddr_end);
    let map_size = paddr_top.checked_sub(paddr_base).ok_or(Status::INVALID_ARGS)?;

    // SAFETY: The caller guarantees that cached is appropriate for all mappings of the physical
    // range, and that creating this mapping is sound.
    let base = unsafe {
        VmAspace::kernel_aspace().alloc_physical(
            b"smbios",
            map_size,
            None,
            page::SHIFT as u8,
            crate::kernel::types::PAddr(paddr_base),
            0,
            ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ,
        )
    }?;

    let offset = paddr_usize - paddr_base;
    let virt_addr = base + offset;
    let virt_ptr = core::ptr::with_exposed_provenance_mut::<u8>(virt_addr);
    let non_null = NonNull::new(virt_ptr).ok_or(Status::NO_MEMORY)?;
    Ok((non_null, base))
}

/// Searches for and maps the SMBIOS entry point table, returning the mapped pointer and version.
///
/// # Safety
///
/// Caller must ensure `gPhysHandoff` is valid (`EndHandoff()` has not been called).
unsafe fn find_entry_point() -> Result<(NonNull<u8>, EntryPointVersion), Status> {
    // SAFETY: Caller guarantees `gPhysHandoff` is valid.
    let smbios_phys =
        Option::from(unsafe { PhysHandoff::get() }.smbios_phys).ok_or(Status::NOT_FOUND)?;
    // SAFETY: Cached is appropriate for all mappings of the SMBIOS entry point, and creating a
    // read-only kernel mapping of it is sound.
    let (ep_virt_ptr, ep_mapping_base) = unsafe { map_range(smbios_phys, MAX_EP_SIZE) }?;

    // SAFETY: `ep_virt_ptr` points to at least `MAX_EP_SIZE` (31) valid mapped bytes.
    let ep_bytes = unsafe { core::slice::from_raw_parts(ep_virt_ptr.as_ptr(), MAX_EP_SIZE) };

    if ep_bytes.starts_with(SMBIOS2_ANCHOR) {
        Ok((ep_virt_ptr, EntryPointVersion::V2_1))
    } else if ep_bytes.starts_with(SMBIOS3_ANCHOR) {
        Ok((ep_virt_ptr, EntryPointVersion::V3_0))
    } else {
        // SAFETY: `ep_mapping_base` is in a mapping of the root VMAR whose whole VMO range is
        // pinned by a pin this code owns, and nothing uses that mapping any more.
        unsafe {
            let _ = VmAspace::kernel_aspace().free_region(ep_mapping_base);
        }
        Err(Status::NOT_FOUND)
    }
}

/// Initializes the SMBIOS subsystem.
///
/// # Safety
///
/// Caller must ensure this is called at most once during single-CPU platform initialization before
/// `EndHandoff()`.
pub(super) unsafe fn pc_init_smbios() {
    // SAFETY: Caller guarantees serialized initialization prior to `EndHandoff()`.
    unsafe {
        SMBIOS_STATE.init(probe_smbios());
    }
}

/// # Safety
///
/// Caller must ensure `gPhysHandoff` is valid (`EndHandoff()` has not been called).
unsafe fn probe_smbios() -> SmbiosState {
    let mut state = SmbiosState::default();
    // SAFETY: Caller guarantees `gPhysHandoff` is valid.
    let (ep_ptr, version) = match unsafe { find_entry_point() } {
        Ok(res) => res,
        Err(_) => {
            // SAFETY: `printf` format string is a valid null-terminated C string.
            unsafe {
                printf(c"smbios: Failed to locate entry point\n".as_ptr());
            }
            return state;
        }
    };

    match version {
        EntryPointVersion::V2_1 => {
            // SAFETY: `ep_ptr` points to at least `MAX_EP_SIZE` (31) valid mapped bytes.
            let ep_bytes = unsafe { core::slice::from_raw_parts(ep_ptr.as_ptr(), MAX_EP_SIZE) };
            let Ok((ep, _)) = EntryPoint2_1::ref_from_prefix(ep_bytes) else {
                // SAFETY: `ep_ptr` rounded down to a page is in a mapping of the root VMAR whose
                // whole VMO range is pinned by a pin this code owns, and nothing uses that mapping
                // any more.
                unsafe {
                    let _ = VmAspace::kernel_aspace()
                        .free_region(page::round_down(ep_ptr.addr().get()));
                }
                return state;
            };

            if !ep.is_valid() {
                // SAFETY: `ep_ptr` rounded down to a page is in a mapping of the root VMAR whose
                // whole VMO range is pinned by a pin this code owns, and nothing uses that mapping
                // any more.
                unsafe {
                    let _ = VmAspace::kernel_aspace()
                        .free_region(page::round_down(ep_ptr.addr().get()));
                }
                return state;
            }

            let ep_copy = *ep;
            let struct_table_phys = ep.struct_table_phys.get() as u64;
            let struct_table_length = ep.struct_table_length.get() as usize;

            // SAFETY: `ep_ptr` rounded down to a page is in a mapping of the root VMAR whose whole
            // VMO range is pinned by a pin this code owns, and nothing uses that mapping any more.
            unsafe {
                let _ =
                    VmAspace::kernel_aspace().free_region(page::round_down(ep_ptr.addr().get()));
            }

            // SAFETY: Cached is appropriate for all mappings of the SMBIOS structure table, and
            // creating a read-only kernel mapping of it is sound.
            match unsafe { map_range(struct_table_phys, struct_table_length) } {
                Ok((struct_virt_ptr, _struct_mapping_base)) => {
                    state.version = Some(EntryPointVersion::V2_1);
                    state.ep2_1 = Some(ep_copy);
                    state.struct_base = struct_virt_ptr.as_ptr() as usize;
                    state.struct_len = struct_table_length;
                }
                Err(status) => {
                    // SAFETY: `printf` format string is a valid null-terminated C string.
                    unsafe {
                        printf(c"smbios: failed to map structs: %d\n".as_ptr(), status.into_raw());
                    }
                }
            }
        }
        EntryPointVersion::V3_0 => {
            // SAFETY: `ep_ptr` rounded down to a page is in a mapping of the root VMAR whose whole
            // VMO range is pinned by a pin this code owns, and nothing uses that mapping any more.
            unsafe {
                let _ =
                    VmAspace::kernel_aspace().free_region(page::round_down(ep_ptr.addr().get()));
            }
            // SAFETY: `printf` format string is a valid null-terminated C string.
            unsafe {
                printf(c"smbios: version 3 not yet implemented\n".as_ptr());
            }
        }
        _ => {
            // SAFETY: `ep_ptr` rounded down to a page is in a mapping of the root VMAR whose whole
            // VMO range is pinned by a pin this code owns, and nothing uses that mapping any more.
            unsafe {
                let _ =
                    VmAspace::kernel_aspace().free_region(page::round_down(ep_ptr.addr().get()));
            }
        }
    }

    state
}

/// Walks the known SMBIOS structures. `cb` will be called once for each structure found.
pub(super) fn walk_structs<F>(cb: F) -> Result<(), Status>
where
    F: FnMut(
        SpecVersion,
        &'static Header,
        &'static [u8],
        &StringTable<'static>,
    ) -> Result<(), Status>,
{
    let state = SMBIOS_STATE.get();

    match state.version {
        Some(EntryPointVersion::V2_1) => {
            let Some(ep2_1) = state.ep2_1.as_ref() else {
                return Err(Status::NOT_SUPPORTED);
            };
            let entry = EntryPoint::from(ep2_1);
            // SAFETY: `struct_base` points to the permanently mapped structure table of
            // `struct_len` bytes in the kernel address space (`'static`).
            unsafe { entry.walk_structs_raw(state.struct_base, cb) }
        }
        Some(EntryPointVersion::V3_0) | Some(EntryPointVersion::Unknown) | None => {
            Err(Status::NOT_SUPPORTED)
        }
    }
}

/// Callback for debug dumping SMBIOS structures.
fn debug_struct_walk(
    ver: SpecVersion,
    hdr: &'static Header,
    raw_bytes: &'static [u8],
    st: &StringTable<'static>,
) -> Result<(), Status> {
    let mut writer = debug::dprintf::KernelConsoleWriter;

    match hdr.r#type {
        StructType::BIOS_INFO => {
            if ver.includes_version(2, 4, 0)
                && let Ok((entry, _)) = BiosInformationStruct2_4::ref_from_prefix(raw_bytes)
            {
                let _ = entry.dump(&mut writer, st);
                return Ok(());
            } else if ver.includes_version(2, 0, 0)
                && let Ok((entry, _)) = BiosInformationStruct2_0::ref_from_prefix(raw_bytes)
            {
                let _ = entry.dump(&mut writer, st, raw_bytes);
                return Ok(());
            }
        }
        StructType::SYSTEM_INFO => {
            if ver.includes_version(2, 4, 0)
                && let Ok((entry, _)) = SystemInformationStruct2_4::ref_from_prefix(raw_bytes)
            {
                let _ = entry.dump(&mut writer, st);
                return Ok(());
            } else if ver.includes_version(2, 1, 0)
                && let Ok((entry, _)) = SystemInformationStruct2_1::ref_from_prefix(raw_bytes)
            {
                let _ = entry.dump(&mut writer, st);
                return Ok(());
            } else if ver.includes_version(2, 0, 0)
                && let Ok((entry, _)) = SystemInformationStruct2_0::ref_from_prefix(raw_bytes)
            {
                let _ = entry.dump(&mut writer, st);
                return Ok(());
            }
        }
        _ => {}
    }

    // SAFETY: `printf` format string is a valid null-terminated C string.
    unsafe {
        printf(
            c"smbios: found struct@%p: typ=%u len=%u st_len=%zu\n".as_ptr(),
            hdr as *const Header,
            hdr.r#type.0 as core::ffi::c_uint,
            hdr.length as core::ffi::c_uint,
            st.length(),
        );
    }
    let _ = st.dump(&mut writer);

    Ok(())
}

#[cfg(console_enabled)]
unsafe extern "C" fn cmd_smbios(argc: i32, argv: *const CmdArgs, _flags: u32) -> i32 {
    // SAFETY: The console framework guarantees `argv` is valid and contains `argc` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    let usage = || -> i32 {
        // SAFETY: `printf` format strings are valid null-terminated C strings.
        unsafe {
            printf(c"usage:\n".as_ptr());
            printf(c"%s dump\n".as_ptr(), args[0].arg_str);
        }
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        // SAFETY: `printf` format string is a valid null-terminated C string.
        unsafe {
            printf(c"not enough arguments\n".as_ptr());
        }
        return usage();
    }

    // SAFETY: The console framework guarantees `args[1].arg_str` is a valid null-terminated C
    // string.
    let cmd = unsafe { core::ffi::CStr::from_ptr(args[1].arg_str) };
    if cmd.to_bytes() == b"dump" {
        if let Err(status) = walk_structs(debug_struct_walk) {
            // SAFETY: `printf` format string is a valid null-terminated C string.
            unsafe {
                printf(c"smbios: failed to walk structs: %d\n".as_ptr(), status.into_raw());
            }
        }
        zx_status::sys::ZX_OK
    } else {
        // SAFETY: `printf` format string is a valid null-terminated C string.
        unsafe {
            printf(c"unknown command\n".as_ptr());
        }
        usage()
    }
}

#[cfg(console_enabled)]
static_command!(CMD_SMBIOS, c"smbios".as_ptr(), c"smbios".as_ptr(), cmd_smbios, CMD_AVAIL_ALWAYS);
