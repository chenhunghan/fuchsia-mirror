// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::efi_ffi::{cpp_phys_handoff_efi_memory_attributes, cpp_phys_handoff_memory};
use crate::kernel::thread;
use crate::kernel::types::PAddr;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHE_MASK, ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_EXECUTE,
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE, ArchMmuFlags,
};
use crate::vm::vm_address_region::flag as vmar_flag;
use crate::vm::vm_aspace::{Type, VmAspace};
use crate::vm::vm_object_physical::VmObjectPhysical;
use crate::vm::vmm;
use core::mem::{align_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use debug::dprintf;
use efi::{
    EFI_INVALID_PARAMETER, EFI_MEMORY_MAPPED_IO, EFI_MEMORY_RO, EFI_MEMORY_RUNTIME, EFI_MEMORY_XP,
    EFI_UNSUPPORTED, EfiGuid, EfiMemoryAttributesTableHeader, EfiMemoryDescriptor,
    EfiRuntimeServices, EfiStatus, EfiSystemTable,
};
use fbl::RefPtr;
use handoff::PhysHandoff;
use kprint::kprint;
use memalloc::{NormalizedRangeIterator, NormalizedRangeUncheckedIterator, Range};
use zerocopy::FromBytes;
use zx_status::Status;

/// EFI system table physical address.
///
/// Written at most once, at the end of a successful `init_efi_services` and before `EFI_ASPACE` is
/// published with `Ordering::Release`. Readers only use it after an `Ordering::Acquire` load of a
/// non-null `EFI_ASPACE`, so `Ordering::Relaxed` suffices for all accesses.
static EFI_SYSTEM_TABLE: AtomicU64 = AtomicU64::new(0);

/// Whether `EFI_SYSTEM_TABLE` has been set.
///
/// Written at most once, alongside `EFI_SYSTEM_TABLE` at the end of a successful
/// `init_efi_services`, so `Ordering::Relaxed` suffices for the same reason. A failed
/// initialization leaves it unset.
static EFI_SYSTEM_TABLE_SET: AtomicBool = AtomicBool::new(false);

/// Address space with EFI services mapped in 1:1.
///
/// Published at most once at the end of `init_efi_services`, after all 1:1 runtime mappings are in
/// place, with `Ordering::Release`. `try_activate_efi_services` loads it with `Ordering::Acquire`,
/// so a reader that sees a non-null aspace also sees the mappings and `EFI_SYSTEM_TABLE`.
static EFI_ASPACE: AtomicPtr<VmAspace> = AtomicPtr::new(ptr::null_mut());

fn efi_init_hook(_level: init::LkInitLevel) {
    // Attempt to initialize EFI.
    if let Some(efi_system_table) = Option::from(PhysHandoff::get().efi_system_table) {
        let status = init_efi_services(efi_system_table);
        if let Err(status) = status {
            dprintf!(INFO, "Unable to initialize EFI services: {}\n", status.into_raw());
        } else {
            kprint!("Init EFI OK\n");
        }
    } else {
        dprintf!(INFO, "No EFI available on system.\n");
    }
}

// Init EFI before INIT_LEVEL_PLATFORM in case the platform code wants to use the EFI crashlog.
init::lk_init_hook!(efi_init, efi_init_hook, init::LkInitLevel(init::LK_INIT_LEVEL_PLATFORM.0 - 1));

/// Helper function that maps the region with size `size` at `base` into the given aspace.
fn map_unaligned_region(
    aspace: &VmAspace,
    base: PAddr,
    size: usize,
    name: &str,
    mut arch_mmu_flags: ArchMmuFlags,
) -> Result<(), Status> {
    if size == 0 {
        return Err(Status::INVALID_ARGS);
    }
    let aligned_base = PAddr(page::round_down(base.0));
    let aligned_size = size
        .checked_add(base.0 - aligned_base.0)
        .and_then(|unrounded_size| unrounded_size.checked_next_multiple_of(page::SIZE))
        .ok_or(Status::INVALID_ARGS)?;
    let aligned_end = PAddr(aligned_base.0.checked_add(aligned_size).ok_or(Status::INVALID_ARGS)?);

    // Check that the given region, rounded out to whole pages, does not intersect with any RAM.
    {
        let mut reserved = true;
        let mut check_intersection_with_ram = |ram: &Range| -> bool {
            // We need only check for intersection with the first RAM range ending after the
            // beginning the region.
            if ram.end() <= aligned_base.0 as u64 {
                return true;
            }
            reserved = aligned_end.0 as u64 <= ram.addr();
            false
        };
        let mut count = 0usize;
        // SAFETY: `gPhysHandoff` stays valid until `EndHandoff` clears it, which is after the
        // `LK_INIT_LEVEL_PLATFORM - 1` hook runs, and `count` is a live local that the callee
        // fills in.
        let memory =
            unsafe { zr::slice_from_raw_parts(cpp_phys_handoff_memory(&mut count), count) };
        // SAFETY: `PhysHandoff::memory` is documented as a normalized accounting of RAM: maximally
        // contiguous ranges in sorted order (`phys/handoff/include/phys/handoff/handoff.h`).
        let ram = unsafe { NormalizedRangeUncheckedIterator::new(memory.iter().copied()) };
        for range in ram.filter_map_ram() {
            if !check_intersection_with_ram(&range) {
                break;
            }
        }
        if !reserved {
            kprint!(
                "ERROR: Attempted to map EFI region [0x{:x}, 0x{:x}) ({:s}), which is not a \
                 reserved region.\n",
                aligned_base.0,
                aligned_end.0,
                name
            );
            return Err(Status::INVALID_ARGS);
        }
    }

    let vmar = aspace.root_vmar().expect("uefi aspace has no root VMAR");
    let vmo = VmObjectPhysical::create(aligned_base, aligned_size)?;

    // SAFETY: `vmo` was just created and has no mappings or children yet.
    unsafe { vmo.set_mapping_cache_policy(arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK)? };
    arch_mmu_flags &= !ARCH_MMU_FLAG_CACHE_MASK;

    let mut vmar_flags: u32 = vmar_flag::SPECIFIC_OVERWRITE;
    if arch_mmu_flags & ARCH_MMU_FLAG_PERM_READ != 0 {
        vmar_flags |= vmar_flag::CAN_MAP_READ;
    }
    if arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE != 0 {
        vmar_flags |= vmar_flag::CAN_MAP_WRITE;
    }
    if arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE != 0 {
        vmar_flags |= vmar_flag::CAN_MAP_EXECUTE;
    }

    let mapping_result = vmar.create_vm_mapping(
        aligned_base.0,
        aligned_size,
        page::SHIFT as u8,
        vmar_flags,
        VmObjectPhysical::into_vm_object(vmo),
        0,
        arch_mmu_flags,
        name.as_bytes(),
    )?;

    mapping_result.mapping.map_range(0, aligned_size, true, false)?;

    Ok(())
}

/// Loops over a `&[u8]` that may or may not be a valid efi_memory_attributes_table.
/// Will return early if `callback` does not return `Ok(())`.
/// Returns `Status::INVALID_ARGS` if `table` is invalid.
pub(super) fn for_each_memory_attribute_entry_safe(
    table: &[u8],
    mut callback: impl FnMut(&EfiMemoryDescriptor) -> Result<(), Status>,
) -> Result<(), Status> {
    if table.len() < size_of::<EfiMemoryAttributesTableHeader>() {
        return Err(Status::INVALID_ARGS);
    }
    let header_bytes = &table[..size_of::<EfiMemoryAttributesTableHeader>()];
    let header = EfiMemoryAttributesTableHeader::read_from_bytes(header_bytes).unwrap();

    let mut entries = &table[size_of::<EfiMemoryAttributesTableHeader>()..];

    if (header.descriptor_size as usize) < size_of::<EfiMemoryDescriptor>() {
        dprintf!(
            CRITICAL,
            "EFI memory attributes header reports a descriptor size of 0x{:x}, which is smaller \
             than ours (0x{:x})\n",
            header.descriptor_size,
            size_of::<EfiMemoryDescriptor>()
        );
        return Err(Status::INVALID_ARGS);
    }

    let desc_size = header.descriptor_size as usize;
    for _ in 0..header.number_of_entries {
        if entries.len() < desc_size {
            return Err(Status::INVALID_ARGS);
        }
        let entry = &entries[..size_of::<EfiMemoryDescriptor>()];

        let desc = EfiMemoryDescriptor::read_from_bytes(entry).unwrap();

        callback(&desc)?;

        entries = &entries[desc_size..];
    }

    Ok(())
}

/// Initialize data structures for EFI services.
#[sanitize(address = "off")]
fn init_efi_services(efi_system_table: u64) -> Result<(), Status> {
    assert!(!EFI_SYSTEM_TABLE_SET.load(Ordering::Relaxed));
    if efi_system_table == 0
        || !(efi_system_table as usize).is_multiple_of(align_of::<EfiSystemTable>())
    {
        return Err(Status::INVALID_ARGS);
    }

    // gPhysHandoff currently points into physical pages that are part of the ZBI VMO.
    // This is safe for now, because we call the efi_init_hook at LK_INIT_LEVEL_PLATFORM, which is
    // before userboot runs.
    // There are plans to change this in the future, at which point we may need to revisit this.
    let mut size = 0usize;
    // SAFETY: `gPhysHandoff` stays valid until `EndHandoff` clears it, which is after the
    // `LK_INIT_LEVEL_PLATFORM - 1` hook runs, and `size` is a live local that the callee fills in.
    let efi_memory_attributes = unsafe { cpp_phys_handoff_efi_memory_attributes(&mut size) };
    if size == 0 {
        dprintf!(CRITICAL, "EFI did not provide memory table, cannot map runtime services.\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Map in the system table.
    let efi_memory_table = efi_memory_attributes;

    if efi_memory_table.is_null() {
        dprintf!(CRITICAL, "EFI did not provide memory table, cannot map runtime services.\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Create a new address space.
    let Some(efi_aspace) = VmAspace::create(Type::LowKernel, b"uefi") else {
        return Err(Status::NO_RESOURCES);
    };
    let aspace_for_cleanup = efi_aspace.clone();
    let mut cleanup = zr::defer(move || {
        let _ = aspace_for_cleanup.destroy();
    });

    // SAFETY: `efi_memory_attributes` is non-null and points to `size` bytes of hand-off memory
    // that stay mapped and unwritten for the duration of this call.
    let efi_memory_attributes = unsafe { zr::slice_from_raw_parts(efi_memory_attributes, size) };
    for_each_memory_attribute_entry_safe(efi_memory_attributes, |desc| {
        if desc.Attribute & EFI_MEMORY_RUNTIME == 0 {
            return Ok(());
        }

        // UEFI v2.9, section 4.6, "EFI_MEMORY_ATTRIBUTES_TABLE" says that only RUNTIME, RO and XP
        // are allowed to be set.
        //
        // We assume double-negatives apply sensibly: "not read-only" implies writable and "not
        // execute-protected" implies executable (except for MMIO, which is never executable).
        let mut arch_mmu_flags: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ;
        if (desc.Attribute & EFI_MEMORY_RO) == 0 {
            arch_mmu_flags |= ARCH_MMU_FLAG_PERM_WRITE;
        }
        if desc.Type == EFI_MEMORY_MAPPED_IO {
            arch_mmu_flags |= ARCH_MMU_FLAG_UNCACHED_DEVICE;
        } else {
            arch_mmu_flags |= ARCH_MMU_FLAG_CACHED;
            if (desc.Attribute & EFI_MEMORY_XP) == 0 {
                arch_mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
            }
        }

        let Some(region_size) = desc.NumberOfPages.checked_mul(page::SIZE as u64) else {
            dprintf!(
                CRITICAL,
                "EFI region base=0x{:x} has an invalid page count 0x{:x}\n",
                desc.PhysicalStart,
                desc.NumberOfPages
            );
            return Err(Status::INVALID_ARGS);
        };
        let result = map_unaligned_region(
            &efi_aspace,
            PAddr(desc.PhysicalStart as usize),
            region_size as usize,
            "efi_runtime",
            arch_mmu_flags,
        );
        if let Err(result) = result {
            dprintf!(
                CRITICAL,
                "Failed to map EFI region base=0x{:x} size=0x{:x}: {}\n",
                desc.PhysicalStart,
                region_size,
                result.into_raw()
            );
            return Err(result);
        }

        Ok(())
    })?;

    cleanup.cancel();
    EFI_SYSTEM_TABLE_SET.store(true, Ordering::Relaxed);
    EFI_SYSTEM_TABLE.store(efi_system_table, Ordering::Relaxed);
    EFI_ASPACE.store(RefPtr::into_raw(efi_aspace).cast_mut(), Ordering::Release);
    Ok(())
}

/// Activate EFI services.
///
/// Calling this function will attempt to activate the address space containing EFI services, and
/// return a scoped [`EfiServicesActivation`] guard providing access to runtime services. When the
/// guard is dropped, the previous address space will be restored.
///
/// Returns a guard for which [`EfiServicesActivation::valid`] is `false` if no EFI services are
/// available.
///
/// WARNING: Callers accessing EFI runtime services while this guard is active must be tagged with
/// `#[sanitize(address = "off")]` (the Rust equivalent of `NO_ASAN`) to avoid crashes when running
/// under KASAN.
///
/// # Safety
///
/// - Any [`EfiServicesActivation`] guards active on the current thread must be reset or dropped in
///   strict LIFO (stack) order without switching or destroying the thread's saved previous
///   `VmAspace` while the guard is active; otherwise, resetting or dropping an outer guard early
///   restores the previous address space while an inner guard still reports `valid() == true` and
///   would dereference unmapped 1:1 physical EFI pointers.
/// - Callers accessing EFI runtime services while the returned guard is active must disable KASAN
///   instrumentation (`#[sanitize(address = "off")]`).
#[sanitize(address = "off")]
pub unsafe fn try_activate_efi_services() -> EfiServicesActivation {
    // Ensure we have EFI services available and it has been initialised.
    let efi_aspace = EFI_ASPACE.load(Ordering::Acquire);
    if efi_aspace.is_null() {
        return EfiServicesActivation::default();
    }
    debug_assert!(EFI_SYSTEM_TABLE_SET.load(Ordering::Relaxed));

    // Switch into the address space where EFI services have been mapped.
    // SAFETY: The reference is immediately converted to a pointer and stored; the caller keeps
    // that aspace valid until the guard restores it.
    let old_aspace = unsafe { thread::current_active_aspace() }.map(NonNull::from);
    // SAFETY: `efi_aspace` is published once into `EFI_ASPACE` at the end of `init_efi_services`
    // (and loaded with `Acquire` above), is never destroyed, and the caller restores the previous
    // aspace through the guard.
    unsafe { vmm::set_active_aspace(Some(&*efi_aspace)) };

    // Return the services.
    let sys = ptr::with_exposed_provenance::<EfiSystemTable>(
        EFI_SYSTEM_TABLE.load(Ordering::Relaxed) as usize,
    );
    // SAFETY: `sys` is the firmware's system table, which `init_efi_services` mapped 1:1 into the
    // aspace that is now active, and this function is `#[sanitize(address = "off")]`.
    let runtime_services = unsafe { (*sys).RuntimeServices };
    let Some(services) = NonNull::new(runtime_services) else {
        // SAFETY: `old_aspace` was the thread's active aspace above, and the caller keeps it valid.
        unsafe { vmm::set_active_aspace(old_aspace.map(|p| p.as_ref())) };
        return EfiServicesActivation::default();
    };
    EfiServicesActivation { previous_aspace: Some(old_aspace), services: Some(services) }
}

/// Manages access to [`EfiRuntimeServices`] and restoration of the previous address space on drop.
#[derive(Default)]
pub struct EfiServicesActivation {
    /// `None` when inactive or already reset. Otherwise, the thread's active aspace when the guard
    /// was created, which is `Some(None)` for a thread with no active aspace.
    previous_aspace: Option<Option<NonNull<VmAspace>>>,
    services: Option<NonNull<EfiRuntimeServices>>,
}

impl EfiServicesActivation {
    /// Returns true if there is a valid EFI services pointer.
    #[sanitize(address = "off")]
    pub fn valid(&self) -> bool {
        self.services.is_some()
    }

    /// Destroy this object, and restore the previous address space.
    ///
    /// # Safety
    ///
    /// Any inner [`EfiServicesActivation`] guards on the current thread must already have been
    /// reset or dropped (strict LIFO order), and the caller must not dereference a pointer
    /// previously returned by [`Self::get`] after calling `reset`.
    #[sanitize(address = "off")]
    pub(super) unsafe fn reset(&mut self) {
        self.services = None;
        let Some(previous_aspace) = self.previous_aspace.take() else {
            return;
        };

        // Restore the previous address space.
        // SAFETY: `previous_aspace` was the thread's active aspace when this guard was created,
        // and the caller of `try_activate_efi_services` keeps it valid until the guard is reset.
        unsafe { vmm::set_active_aspace(previous_aspace.map(|p| p.as_ref())) };
    }

    /// Get pointer to EFI services.
    #[sanitize(address = "off")]
    pub(super) fn get(&self) -> Option<NonNull<EfiRuntimeServices>> {
        self.services
    }

    /// Sets the value of an EFI variable via the `SetVariable` runtime service.
    #[sanitize(address = "off", cfi = "off")]
    pub fn set_variable(
        &self,
        var_name: &[u16],
        vendor_guid: &EfiGuid,
        attributes: u32,
        data: &[u8],
    ) -> EfiStatus {
        if !self.valid() || var_name.last() != Some(&0) {
            return EFI_INVALID_PARAMETER;
        }
        let services = self.get().unwrap();
        // SAFETY: `services` is the firmware's runtime services table, mapped 1:1 in the aspace
        // this guard activated, and this function is `#[sanitize(address = "off")]`.
        let Some(set_variable) = (unsafe { (*services.as_ptr()).SetVariable }) else {
            return EFI_UNSUPPORTED;
        };
        let data_ptr = if data.is_empty() { ptr::null() } else { data.as_ptr().cast() };
        // SAFETY: `set_variable` is the firmware's `SetVariable`. `var_name` is NUL-terminated, and
        // `SetVariable` only reads it, `vendor_guid` and `data` while it runs.
        unsafe {
            set_variable(
                var_name.as_ptr().cast_mut(),
                ptr::from_ref(vendor_guid).cast_mut(),
                attributes,
                data.len(),
                data_ptr,
            )
        }
    }
}

impl Drop for EfiServicesActivation {
    #[sanitize(address = "off")]
    fn drop(&mut self) {
        // SAFETY: `try_activate_efi_services`'s caller upholds strict LIFO drop order, and `self`
        // is being destroyed so `set_variable` and `get` cannot be used afterward.
        unsafe { self.reset() };
    }
}
