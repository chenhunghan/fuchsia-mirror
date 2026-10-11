// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::userabi_ffi::{HandoffEndElf, ZERO_FILL};
use crate::arch_rs::ops::{ZX_VM_FEATURE_CAN_MAP_XOM, arch_vm_features};
use crate::object::{HandleOwner, VmAddressRegionDispatcher};
use crate::vm::pmm::{ALLOC_FLAG_ANY, ALLOC_FLAG_CAN_WAIT};
use crate::vm::vm_object_paged::VmObjectPaged;
use core::str::from_utf8;
use debug::dprintf;
use page::SIZE as PAGE_SIZE;
use zx_status::Status;
use zx_types::{
    ZX_MAX_NAME_LEN, ZX_VM_CAN_MAP_EXECUTE, ZX_VM_CAN_MAP_READ, ZX_VM_CAN_MAP_SPECIFIC,
    ZX_VM_CAN_MAP_WRITE, ZX_VM_PERM_EXECUTE, ZX_VM_PERM_READ, ZX_VM_PERM_WRITE, ZX_VM_SPECIFIC,
    zx_vaddr_t, zx_vm_option_t,
};

pub struct MappedElf {
    pub vmar: HandleOwner,
    pub vaddr_start: zx_vaddr_t,
    pub entry: zx_vaddr_t,
    pub stack_size: Option<usize>,
}

// This creates a VMAR within the given parent VMAR, and maps the ELF file in.
// The new VMAR is returned, so it can be used to change protections for RELRO.
// The reference can just be dropped to ensure no more changes are possible.
pub fn map_handoff_elf(
    elf: HandoffEndElf,
    parent_vmar: &VmAddressRegionDispatcher,
) -> Result<MappedElf, Status> {
    let elf_vmo = elf.vmo.ok_or(Status::INVALID_ARGS)?;
    let mut vmo_name_buffer = [0u8; ZX_MAX_NAME_LEN];
    elf_vmo.get_name(&mut vmo_name_buffer);
    let mut vmo_name = from_utf8(&vmo_name_buffer).unwrap_or("");
    vmo_name = &vmo_name[..vmo_name.find('\0').unwrap_or(vmo_name.len())];

    let vmar_flags: zx_vm_option_t =
        ZX_VM_CAN_MAP_SPECIFIC | ZX_VM_CAN_MAP_READ | ZX_VM_CAN_MAP_WRITE | ZX_VM_CAN_MAP_EXECUTE;
    let (vmar_handle, vmar_rights) =
        parent_vmar.allocate(0, elf.vmar_size, vmar_flags).inspect_err(|_| {
            dprintf!(
                CRITICAL,
                "userboot: failed to allocate VMAR of {} bytes for {} ELF image\n",
                elf.vmar_size,
                vmo_name
            );
        })?;
    let vmar = vmar_handle.dispatcher().clone();
    let vmar_info = vmar.get_vmar_info();
    let parent_vmar_info = parent_vmar.get_vmar_info();

    let mapped_elf = MappedElf {
        vmar: HandleOwner::make(vmar_handle, vmar_rights).ok_or(Status::NO_MEMORY)?,
        vaddr_start: vmar_info.base,
        entry: vmar_info.base + elf.info.relative_entry_point,
        stack_size: elf.info.stack_size(),
    };

    // TODO(mcgrathr): emit symbolizer markup for these instead
    dprintf!(
        SPEW,
        "userboot: {:<31} @ [{:#x},{:#x})\n",
        "inside VMAR",
        parent_vmar_info.base,
        parent_vmar_info.base + parent_vmar_info.len
    );
    dprintf!(
        SPEW,
        "userboot: {:<31} @ [{:#x},{:#x})\n",
        vmo_name,
        mapped_elf.vaddr_start,
        mapped_elf.vaddr_start + vmar_info.len
    );

    // Mappings marked with ZERO_FILL need anonymous VMO pages rather than file
    // VMO pages.  Sum those and make a single VMO for all the pages needed.
    let mut bss_total: usize = 0;
    for mapping in &*elf.mappings {
        if mapping.paddr == ZERO_FILL {
            bss_total += mapping.size;
        }
    }
    let bss_vmo = if bss_total > 0 {
        debug_assert!(bss_total.is_multiple_of(PAGE_SIZE));
        let vmo = VmObjectPaged::create(ALLOC_FLAG_ANY | ALLOC_FLAG_CAN_WAIT, 0, bss_total as u64)
            .inspect_err(|status| {
                dprintf!(
                    CRITICAL,
                    "userboot: failed to allocate VMO of {} bss bytes for {} ELF image: {}\n",
                    bss_total,
                    vmo_name,
                    status.into_raw()
                );
            })?;
        Some(VmObjectPaged::into_vm_object(vmo))
    } else {
        None
    };

    let mut segment_name = "???";
    for mapping in &*elf.mappings {
        let mut map_flags: zx_vm_option_t = ZX_VM_SPECIFIC;
        if mapping.perms.readable() {
            map_flags |= ZX_VM_PERM_READ;
            segment_name = "rodata";
        }
        if mapping.perms.writable() {
            map_flags |= ZX_VM_PERM_WRITE;
            segment_name = "data";
        }
        if mapping.perms.executable() {
            assert!(!mapping.perms.writable());
            map_flags |= ZX_VM_PERM_EXECUTE;
            if (arch_vm_features() & ZX_VM_FEATURE_CAN_MAP_XOM) == 0 {
                map_flags |= ZX_VM_PERM_READ;
            }
            segment_name = "code";
        }
        let (vmo, vmo_offset) = if mapping.paddr == ZERO_FILL {
            // Map from bss_vmo instead of elf.vmo; consume its pages from the end.
            bss_total -= mapping.size;
            segment_name = "bss";
            (bss_vmo.as_ref().ok_or(Status::INTERNAL)?.clone(), bss_total as u64)
        } else {
            (elf_vmo.clone(), mapping.paddr as u64)
        };
        let map_base = vmar
            .map(mapping.vaddr, vmo, vmo_offset, mapping.size, map_flags)
            .inspect_err(|status| {
                dprintf!(
                    CRITICAL,
                    "userboot: {} ELF {} mapping {:#x} @ {:#x} size {:#x} failed {}\n",
                    vmo_name,
                    segment_name,
                    mapping.paddr,
                    mapped_elf.vaddr_start + mapping.vaddr,
                    mapping.size,
                    status.into_raw()
                );
            })?
            .base;
        debug_assert!(map_base == mapped_elf.vaddr_start + mapping.vaddr);
        dprintf!(
            SPEW,
            "userboot: {:<12} ELF {:<6} {:#7x} @ [{:#x},{:#x})\n",
            vmo_name,
            segment_name,
            mapping.paddr,
            map_base,
            map_base + mapping.size
        );
    }
    debug_assert!(bss_total == 0);

    Ok(mapped_elf)
}

pub const fn initial_stack_pointer(base: usize, size: usize) -> usize {
    // Standard machines mandate 16-byte stack alignment (`elfldltl::AbiTraits`).
    const STACK_ALIGNMENT: usize = 16;
    let sp = (base + size) & !(STACK_ALIGNMENT - 1);
    #[cfg(target_arch = "x86_64")]
    {
        // 8 bytes below 16-byte alignment to account for the return address slot pushed by `call`.
        sp - 8
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        sp
    }
}
