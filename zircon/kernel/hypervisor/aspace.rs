// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Guest physical address space management (`<hypervisor/aspace.h>`).

use core::mem::{align_of, size_of};
use core::ptr::with_exposed_provenance_mut;
use fbl::RefPtr;
use ksync::LockToken;
use range_check::in_range_max;
use zr::static_assert;
use zx_status::Status;
use zx_types::{zx_gpaddr_t, zx_vaddr_t};

use crate::kernel::types::{PAddr, VAddr};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE, ArchMmuFlags, ArchVmAspace,
};
use crate::vm::fault::flag as pf_flag;
use crate::vm::page_source::MultiPageRequest;
use crate::vm::pinned_vm_object::PinnedVmObject;
use crate::vm::vm_address_region::{VmAddressRegion, VmAddressRegionOpChildren, flag as vmar_flag};
use crate::vm::vm_aspace::{Type, VmAspace, VmAspaceLockClass};
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object_physical::VmObjectPhysical;

const INTERRUPT_MMU_FLAGS: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
const GUEST_MMU_FLAGS: ArchMmuFlags =
    ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

/// RAII object that holds a mapping of guest physical address space to the host
/// kernel virtual address space. Can be used to map a frequently accessed
/// portion of guest physical memory for faster access.
#[repr(C)]
#[derive(Default)]
pub struct GuestPtr {
    mapping: Option<RefPtr<VmMapping>>,
    pinned_vmo: PinnedVmObject,
    offset: zx_vaddr_t,
}

static_assert!(size_of::<GuestPtr>() == 40);
static_assert!(align_of::<GuestPtr>() == 8);

impl GuestPtr {
    /// Creates a new `GuestPtr` from an active host `mapping`, `pinned_vmo`, and intra-page
    /// `offset`.
    fn new(mapping: RefPtr<VmMapping>, pinned_vmo: PinnedVmObject, offset: zx_vaddr_t) -> Self {
        Self { mapping: Some(mapping), pinned_vmo, offset }
    }

    /// Destroys the host mapping and unpins the underlying VMO pages.
    pub fn reset(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let _ = mapping.destroy();
        }
        self.pinned_vmo = PinnedVmObject::default();
    }

    /// Returns a typed pointer to the mapped guest memory, or null if `size_of::<T>()` exceeds
    /// the remaining mapped size from `offset`.
    pub fn as_ptr<T>(&self) -> *mut T {
        let mapping = self.mapping.as_ref().expect("GuestPtr is not initialized");
        if self.offset + size_of::<T>() > mapping.size() {
            return core::ptr::null_mut();
        }
        with_exposed_provenance_mut(mapping.base().0 + self.offset)
    }
}

impl Drop for GuestPtr {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Manages the guest physical address space (`VmAspace::Type::GuestPhysical`) for a hypervisor
/// guest.
#[repr(C)]
#[derive(Default)]
pub struct GuestPhysicalAspace {
    physical_aspace: Option<RefPtr<VmAspace>>,
}

static_assert!(size_of::<GuestPhysicalAspace>() == size_of::<usize>());
static_assert!(align_of::<GuestPhysicalAspace>() == align_of::<usize>());

impl GuestPhysicalAspace {
    /// Creates a new `GuestPhysicalAspace`.
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok(gpa)`, `gpa` exclusively owns a freshly initialized, undestroyed guest
    /// (`ARCH_ASPACE_FLAG_GUEST`) `ArchVmAspace` with no mappings installed and no concurrent
    /// operations on `gpa.arch_aspace()`.
    pub fn create() -> Result<Self, Status> {
        let physical_aspace =
            VmAspace::create(Type::GuestPhysical, b"guest_physical").ok_or(Status::NO_MEMORY)?;
        Ok(Self { physical_aspace: Some(physical_aspace) })
    }

    #[inline]
    fn physical_aspace(&self) -> &RefPtr<VmAspace> {
        self.physical_aspace.as_ref().expect("GuestPhysicalAspace is not initialized")
    }

    /// Returns the size of the guest physical address space in bytes.
    pub fn size(&self) -> usize {
        self.physical_aspace().size()
    }

    /// Returns a reference to the underlying `ArchVmAspace`.
    pub fn arch_aspace(&self) -> &ArchVmAspace {
        self.physical_aspace().arch_aspace()
    }

    /// Returns the root `VmAddressRegion` of the guest physical address space.
    pub fn root_vmar(&self) -> RefPtr<VmAddressRegion> {
        self.physical_aspace().root_vmar().expect("GuestPhysicalAspace has no root VMAR")
    }

    /// Returns true if `guest_paddr` is backed by a `VmMapping` in this guest physical address
    /// space.
    #[cfg(ktest)]
    fn is_mapped(&self, guest_paddr: zx_gpaddr_t) -> bool {
        ksync::lock!(let guard = self.physical_aspace().lock().lock());
        self.find_mapping(guard.token(), guest_paddr).is_some()
    }

    /// Maps the physical interrupt controller range `[host_paddr, host_paddr + len)` at
    /// `guest_paddr` in the guest physical address space.
    pub fn map_interrupt_controller(
        &self,
        guest_paddr: zx_gpaddr_t,
        host_paddr: PAddr,
        len: usize,
    ) -> Result<(), Status> {
        let vmo = VmObjectPhysical::create(host_paddr, len)?;
        // SAFETY: `vmo` was just created and has no mappings or children yet.
        unsafe {
            vmo.set_mapping_cache_policy(ARCH_MMU_FLAG_UNCACHED_DEVICE)?;
        }
        let vmo_size = vmo.size() as usize;

        // The root VMAR will maintain a reference to the VmMapping internally so
        // we don't need to maintain a long-lived reference to the mapping here.
        let mapping_result = self.root_vmar().create_vm_mapping_no_overwrite(
            guest_paddr,
            vmo_size,
            /* align_pow2*/ 0,
            vmar_flag::SPECIFIC,
            VmObjectPhysical::into_vm_object(vmo),
            /* vmo_offset */ 0,
            INTERRUPT_MMU_FLAGS,
        )?;

        // Write mapping to page table.
        if let Err(status) = mapping_result.mapping.map_range(0, vmo_size, true, false) {
            let _ = mapping_result.mapping.destroy();
            return Err(status);
        }

        Ok(())
    }

    /// Unmaps `[guest_paddr, guest_paddr + len)` from the deepest containing VMAR in the guest
    /// physical address space.
    pub fn unmap_range(&self, guest_paddr: zx_gpaddr_t, len: usize) -> Result<(), Status> {
        // Walk from the root down through any child VMARs for this range so we can do an unmap.
        let mut vmar = self.root_vmar();
        while let Some(next) = vmar.find_region(VAddr(guest_paddr)) {
            let Some(next_vmar) = next.into_vm_address_region() else {
                break;
            };
            vmar = next_vmar;
        }
        // SAFETY: Unmaps a guest physical address range rather than active host kernel memory.
        unsafe { vmar.unmap(VAddr(guest_paddr), len, VmAddressRegionOpChildren::No) }
    }

    /// Resolves a guest physical page fault at `guest_paddr`.
    pub fn page_fault(&self, guest_paddr: zx_gpaddr_t) -> Result<(), Status> {
        pin_init::stack_pin_init!(let page_request = MultiPageRequest::new());

        let guest_paddr = page::round_down(guest_paddr);

        loop {
            let status = {
                ksync::lock!(let mut guard = self.physical_aspace().lock().lock());
                let mapping =
                    self.find_mapping(guard.token(), guest_paddr).ok_or(Status::NOT_FOUND)?;

                // In order to avoid re-faulting if the guest changes how it accesses
                // guest physical memory, and to avoid the need for invalidation of the
                // guest physical address space on x86 (through the use of INVEPT), we
                // fault the page with the maximum allowable permissions of the mapping.
                let mmu_flags = mapping.arch_mmu_flags_locked(guard.token(), VAddr(guest_paddr));
                let mut pf_flags = pf_flag::GUEST | pf_flag::HW_FAULT;
                if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
                    pf_flags |= pf_flag::WRITE;
                }
                if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
                    pf_flags |= pf_flag::INSTRUCTION;
                }
                // SAFETY: `mapping` is alive and the object lock is not held. `guest_paddr` is
                // page-aligned and within `mapping`, the additional page count is less than
                // `usize::MAX`, and `page_request` does not hold an active request. `mapping` is in
                // the guest-physical aspace.
                let (fault_status, _) = unsafe {
                    mapping.page_fault_locked(
                        guard.as_mut().token_mut(),
                        VAddr(guest_paddr),
                        pf_flags,
                        0,
                        page_request.as_ref(),
                    )
                };
                fault_status
            };

            if status == Err(Status::SHOULD_WAIT) {
                page_request.as_ref().wait(true)?;
            } else {
                return status;
            }
        }
    }

    /// Creates a pinned host kernel mapping (`GuestPtr`) for `[guest_paddr, guest_paddr + len)`.
    ///
    /// Until the returned `GuestPtr` is reset or dropped, the range is mapped writable in the
    /// kernel address space and its pages are pinned, so a non-null pointer returned by
    /// `GuestPtr::as_ptr` is valid for reads and writes of the requested type. The memory is
    /// shared with the guest, so callers should access it through raw pointers rather than Rust
    /// references.
    pub fn create_guest_ptr(
        &self,
        guest_paddr: zx_gpaddr_t,
        len: usize,
    ) -> Result<GuestPtr, Status> {
        let begin = page::round_down(guest_paddr);
        let end = guest_paddr.wrapping_add(len).wrapping_add(page::MASK) & !page::MASK;
        let mapping_len = end.wrapping_sub(begin);
        if begin > end || !in_range_max(begin as u64, mapping_len as u64, self.size() as u64) {
            return Err(Status::INVALID_ARGS);
        }

        let (intra_mapping_offset, mapping_object_offset, vmo) = {
            ksync::lock!(let guard = self.physical_aspace().lock().lock());
            let guest_mapping = self.find_mapping(guard.token(), begin).ok_or(Status::NOT_FOUND)?;
            let intra_mapping_offset = (begin - guest_mapping.base().0) as u64;
            if !in_range_max(intra_mapping_offset, mapping_len as u64, guest_mapping.size() as u64)
            {
                // The address range is not contained within a single mapping.
                return Err(Status::OUT_OF_RANGE);
            }
            let mapping_object_offset = guest_mapping.object_offset();
            let vmo =
                guest_mapping.vmo_locked(guard.token()).expect("live mapping must have a VMO");
            (intra_mapping_offset, mapping_object_offset, vmo)
        };

        // Pin the range of the guest VMO to ensure the user cannot manipulate it to cause our
        // kernel mapping to become invalidate and generate faults.
        let pinned_vmo = PinnedVmObject::create(
            vmo.clone(),
            mapping_object_offset + intra_mapping_offset,
            mapping_len as u64,
            true,
        )?;

        let kernel_vmar =
            VmAspace::kernel_aspace().root_vmar().expect("kernel aspace has no root VMAR");
        let host_mapping_result = kernel_vmar.create_vm_mapping_no_overwrite(
            /* mapping_offset */ 0,
            mapping_len,
            /* align_pow2 */ 0,
            /* vmar_flags */ 0,
            vmo,
            mapping_object_offset + intra_mapping_offset,
            GUEST_MMU_FLAGS,
        )?;
        // Pre-populate the page tables so there's no need for kernel page faults.
        host_mapping_result.mapping.map_range(0, mapping_len, true, false)?;

        Ok(GuestPtr::new(host_mapping_result.mapping, pinned_vmo, guest_paddr - begin))
    }

    fn find_mapping(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        guest_paddr: zx_gpaddr_t,
    ) -> Option<RefPtr<VmMapping>> {
        self.physical_aspace()
            .root_vmar_locked(token)?
            .find_mapping_locked(token, VAddr(guest_paddr))
            .map(RefPtr::from_ref)
    }
}

impl Drop for GuestPhysicalAspace {
    fn drop(&mut self) {
        if let Some(physical_aspace) = self.physical_aspace.take() {
            // VmAspace maintains a circular reference with it's root VMAR. We need to
            // destroy the VmAspace in order to break that reference and allow the
            // VmAspace to be destructed.
            let _ = physical_aspace.destroy();
        }
    }
}

/// Unit tests for `GuestPhysicalAspace` and `GuestPtr`.
#[cfg(ktest)]
#[unittest::suite(name = "aspace_rust")]
mod tests {
    use super::super::hypervisor_unittest::{
        MMU_FLAGS, commit_vmo, create_mapping, create_vmo, hypervisor_supported,
    };
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use unittest::{
        assert_nonnull, expect_eq, expect_err, expect_false, expect_null, expect_ok, expect_true,
        unwrap_ok,
    };
    use zx_types::{
        ZX_CACHE_POLICY_UNCACHED, ZX_CACHE_POLICY_UNCACHED_DEVICE, ZX_CACHE_POLICY_WRITE_COMBINING,
    };

    fn create_gpas() -> Result<GuestPhysicalAspace, Status> {
        if !hypervisor_supported() {
            return Err(Status::NOT_SUPPORTED);
        }
        let gpa = GuestPhysicalAspace::create()?;
        #[cfg(target_arch = "aarch64")]
        // SAFETY:
        // - By `GuestPhysicalAspace::create`'s safety-usable invariant, `gpa` exclusively owns a
        //   freshly initialized, undestroyed guest (`ARCH_ASPACE_FLAG_GUEST`) `ArchVmAspace` with
        //   no mappings installed and no concurrent operations.
        // - By `hypervisor_supported`'s safety-usable invariant, the kernel is executing at EL2
        //   with `FEAT_VHE` enabled.
        // - `1` is non-zero and fits within the 8-bit hardware VMID width.
        // - By type encapsulation, no safe API can attach or enter a `Vcpu` on a standalone
        //   `GuestPhysicalAspace` (`Vcpu::create` requires `&Guest`, which allocates its own
        //   private `GuestPhysicalAspace` and VMID), so `gpa` is never entered with
        //   `HCR_EL2.VM == 1`.
        unsafe {
            gpa.arch_aspace().arch_set_asid(1);
        }
        Ok(gpa)
    }

    fn create_sub_vmar(
        vmar: &VmAddressRegion,
        offset: usize,
        size: usize,
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        vmar.create_sub_vmar(offset, size, 0, vmar.flags() | vmar_flag::SPECIFIC, b"vmar")
    }

    /// Verifies that `unmap_range` unmaps a mapped page in the guest physical address space.
    #[test]
    fn guest_physical_aspace_unmap_range() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");

        // Unmap page.
        expect_ok!(gpa.unmap_range(0, page::SIZE), "Failed to unmap page from GuestPhysicalAspace");

        // Verify `is_mapped` for unmapped address fails.
        expect_false!(gpa.is_mapped(0), "Expected address to be unmapped");
    }

    /// Verifies that `unmap_range` succeeds when the range is outside any existing mapping.
    #[test]
    fn guest_physical_aspace_unmap_range_outside_of_mapping() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");

        // Unmap page.
        expect_ok!(
            gpa.unmap_range(page::SIZE * 8, page::SIZE),
            "Failed to unmap page from GuestPhysicalAspace"
        );
    }

    /// Verifies that `unmap_range` unmaps ranges spanning multiple mappings.
    #[test]
    fn guest_physical_aspace_unmap_range_multiple_mappings() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");

        let vmo1 = unwrap_ok!(create_vmo(page::SIZE * 2), "Failed to create VMO");
        expect_ok!(
            create_mapping(&gpa.root_vmar(), vmo1, 0, MMU_FLAGS),
            "Failed to create mapping"
        );

        let vmo2 = unwrap_ok!(create_vmo(page::SIZE * 2), "Failed to create VMO");
        expect_ok!(
            create_mapping(&gpa.root_vmar(), vmo2, page::SIZE * 3, MMU_FLAGS),
            "Failed to create mapping"
        );

        // Unmap pages.
        expect_ok!(
            gpa.unmap_range(page::SIZE, page::SIZE * 3),
            "Failed to unmap multiple pages from GuestPhysicalAspace"
        );

        // Verify `is_mapped` for unmapped addresses fails.
        for addr in (page::SIZE..page::SIZE * 4).step_by(page::SIZE) {
            expect_false!(gpa.is_mapped(addr), "Expected address to be unmapped");
        }

        // Verify `is_mapped` for mapped addresses succeeds.
        expect_true!(gpa.is_mapped(0), "Expected address to be mapped");
        expect_true!(gpa.is_mapped(page::SIZE * 4), "Expected address to be mapped");
    }

    /// Verifies that `unmap_range` rejects ranges intersecting sub-VMARs.
    #[test]
    fn guest_physical_aspace_unmap_range_sub_region() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let root_vmar = gpa.root_vmar();
        // To test partial unmapping within sub-VMAR:
        // Sub-VMAR from [0, page::SIZE * 2).
        // Map within sub-VMAR from [page::SIZE, page::SIZE * 2).
        let sub_vmar1 =
            unwrap_ok!(create_sub_vmar(&root_vmar, 0, page::SIZE * 2), "Failed to create sub-VMAR");
        expect_true!(sub_vmar1.has_parent(), "Sub-VMAR does not have a parent");
        let vmo1 = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(
            create_mapping(&sub_vmar1, vmo1, page::SIZE, MMU_FLAGS),
            "Failed to create mapping"
        );
        // To test destroying of sub-VMAR:
        // Sub-VMAR from [page::SIZE * 2, page::SIZE * 3).
        // Map within sub-VMAR from [0, page::SIZE).
        let sub_vmar2 = unwrap_ok!(
            create_sub_vmar(&root_vmar, page::SIZE * 2, page::SIZE),
            "Failed to create sub-VMAR"
        );
        expect_true!(sub_vmar2.has_parent(), "Sub-VMAR does not have a parent");
        let vmo2 = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(create_mapping(&sub_vmar2, vmo2, 0, MMU_FLAGS), "Failed to create mapping");
        // To test partial unmapping within root-VMAR:
        // Map within root-VMAR from [page::SIZE * 3, page::SIZE * 5).
        let vmo3 = unwrap_ok!(create_vmo(page::SIZE * 2), "Failed to create VMO");
        expect_ok!(
            create_mapping(&root_vmar, vmo3, page::SIZE * 3, MMU_FLAGS),
            "Failed to create mapping"
        );

        // Cannot unmap spanning across two different sub-vmars.
        expect_err!(gpa.unmap_range(page::SIZE, page::SIZE * 3), Status::INVALID_ARGS);

        // Verify `is_mapped` for mapped addresses succeeds.
        expect_true!(gpa.is_mapped(page::SIZE * 4), "Expected address to be mapped");

        // Verify that sub-VMARs still have a parent.
        expect_true!(sub_vmar1.has_parent(), "Sub-VMAR does not have a parent");
        expect_true!(sub_vmar2.has_parent(), "Sub-VMAR does not have a parent");
    }

    /// Verifies that multiple mappings of a single VMO at different offsets work properly.
    #[test]
    fn guest_physical_address_space_single_vmo_multiple_mappings() {
        if !hypervisor_supported() {
            return true;
        }

        let _scanner_disable = AutoVmScannerDisable::new();

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE * 4), "Failed to create VMO");

        // Map a single page of this four page VMO at offset 0x1000 and offset 0x3000.
        expect_ok!(
            gpa.root_vmar()
                .create_vm_mapping_no_overwrite(
                    page::SIZE,
                    page::SIZE,
                    0,
                    vmar_flag::SPECIFIC,
                    VmObjectPaged::into_vm_object(vmo.clone()),
                    page::SIZE as u64,
                    MMU_FLAGS,
                )
                .map(|_| ()),
            "Failed to create first mapping"
        );
        expect_ok!(
            gpa.root_vmar()
                .create_vm_mapping_no_overwrite(
                    page::SIZE * 3,
                    page::SIZE,
                    0,
                    vmar_flag::SPECIFIC,
                    VmObjectPaged::into_vm_object(vmo.clone()),
                    (page::SIZE * 3) as u64,
                    MMU_FLAGS,
                )
                .map(|_| ()),
            "Failed to create second mapping"
        );

        expect_ok!(commit_vmo(&vmo), "Failed to commit VMO");

        // No mapping at 0x0 or 0x2000.
        expect_false!(gpa.is_mapped(0), "Expected address to be unmapped");
        expect_false!(gpa.is_mapped(page::SIZE * 2), "Expected address to be unmapped");

        // There is a mapping at 0x1000 and 0x3000.
        expect_true!(gpa.is_mapped(page::SIZE), "Expected address to be mapped");
        expect_true!(gpa.is_mapped(page::SIZE * 3), "Expected address to be mapped");
    }

    /// Verifies that `page_fault` faults in pages across mappings with different permission flags.
    #[test]
    fn guest_physical_aspace_page_fault() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(
            create_mapping(&gpa.root_vmar(), vmo.clone(), 0, MMU_FLAGS),
            "Failed to create mapping"
        );
        expect_ok!(
            create_mapping(&gpa.root_vmar(), vmo.clone(), page::SIZE, ARCH_MMU_FLAG_PERM_READ),
            "Failed to create mapping"
        );
        expect_ok!(
            create_mapping(
                &gpa.root_vmar(),
                vmo.clone(),
                page::SIZE * 2,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            ),
            "Failed to create mapping"
        );
        expect_ok!(
            create_mapping(
                &gpa.root_vmar(),
                vmo,
                page::SIZE * 3,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
            ),
            "Failed to create mapping"
        );

        // Fault in each page.
        for addr in (0..page::SIZE * 4).step_by(page::SIZE) {
            expect_ok!(gpa.page_fault(addr), "Failed to fault page");
        }
    }

    /// Verifies that `map_interrupt_controller` maps a physical page into the guest address space.
    #[test]
    fn guest_physical_aspace_map_interrupt_controller() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");

        // Allocate a page to use as the interrupt controller.
        let (vm_page, paddr) = unwrap_ok!(pmm::alloc_page(0), "Unable to allocate a page");

        // Map interrupt controller page in an arbitrary location.
        const GICV_ADDRESS: zx_gpaddr_t = 0x800001000;
        expect_ok!(
            gpa.map_interrupt_controller(GICV_ADDRESS, paddr, page::SIZE),
            "Failed to map interrupt controller page"
        );

        // Cleanup: must destroy `gpa` (which unmaps the physical page) before freeing `vm_page`.
        drop(gpa);
        // SAFETY: `vm_page` was allocated above and is no longer mapped.
        unsafe {
            pmm::free_page(vm_page);
        }
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_UNCACHED` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_uncached() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(
            unsafe { vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_UNCACHED as ArchMmuFlags) },
            "Failed to set cache policy"
        );

        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_UNCACHED_DEVICE` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_uncached_device() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(
            unsafe {
                vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_UNCACHED_DEVICE as ArchMmuFlags)
            },
            "Failed to set cache policy"
        );

        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_WRITE_COMBINING` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_write_combining() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(
            unsafe {
                vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_WRITE_COMBINING as ArchMmuFlags)
            },
            "Failed to set cache policy"
        );

        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");
    }

    /// Verifies `VmAddressRegion::protect` on a mapping in a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_protect() {
        if !hypervisor_supported() {
            return true;
        }

        // Setup.
        let vmo = unwrap_ok!(create_vmo(page::SIZE), "Failed to create VMO");

        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS), "Failed to create mapping");

        // SAFETY: Changes permissions on a guest physical address range rather than active host
        // kernel memory.
        expect_ok!(
            unsafe {
                gpa.root_vmar().protect(
                    VAddr(0),
                    page::SIZE,
                    ARCH_MMU_FLAG_PERM_WRITE,
                    VmAddressRegionOpChildren::Yes,
                )
            },
            "Failed to enable write access"
        );
    }

    /// Verifies `ArchVmAspace::query` across mappings with various permission flags.
    #[test]
    fn guest_physical_aspace_query() {
        if !hypervisor_supported() {
            return true;
        }

        // Disable the VM scanner to ensure pages stay committed into the VMO, and hence stay
        // mapped into the aspace before we query.
        let _scanner_disable = AutoVmScannerDisable::new();

        // This test is arch independent so be conservative with the permission and assume that
        // read is needed for any other permission.
        const MMU_FLAG_TESTS: [ArchMmuFlags; 4] = [
            ARCH_MMU_FLAG_PERM_READ,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE,
        ];

        let vmo = unwrap_ok!(create_vmo(page::SIZE));
        let gpa = unwrap_ok!(create_gpas());

        for flags in MMU_FLAG_TESTS {
            let mapping_result = unwrap_ok!(gpa.root_vmar().create_vm_mapping_no_overwrite(
                0,
                page::SIZE,
                0,
                vmar_flag::SPECIFIC,
                VmObjectPaged::into_vm_object(vmo.clone()),
                0,
                flags,
            ));
            expect_ok!(mapping_result.mapping.map_range(0, page::SIZE, true, false));

            let (_, query_flags) = unwrap_ok!(gpa.arch_aspace().query(0));
            expect_eq!(flags, query_flags);

            // Cleanup the mapping for next iteration.
            expect_ok!(mapping_result.mapping.destroy());
        }
    }

    /// Verifies that `create_guest_ptr` maps and pins a guest region accessible from the host.
    #[test]
    fn guest_physical_aspace_create_guest_ptr() {
        if !hypervisor_supported() {
            return true;
        }

        let gpa = unwrap_ok!(create_gpas(), "Failed to create GuestPhysicalAspace");
        let vmo = unwrap_ok!(create_vmo(page::SIZE * 2), "Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, page::SIZE, MMU_FLAGS));

        let mut guest_ptr = unwrap_ok!(
            gpa.create_guest_ptr(page::SIZE + 64, size_of::<u64>()),
            "Failed to create GuestPtr"
        );
        let ptr = guest_ptr.as_ptr::<u64>();
        assert_nonnull!(ptr);
        // SAFETY: `ptr` was verified non-null and points to `size_of::<u64>()` writable, pinned
        // bytes at intra-page offset `64` from a page-aligned host mapping (`page::SIZE + 64` is
        // 8-byte aligned for `u64`), and `vmo` is local to this test with no concurrent aliases
        // during this write and read.
        unsafe {
            *ptr = 0xdeadbeef_cafebabe;
            expect_eq!(0xdeadbeef_cafebabe, *ptr);
        }
        // Requesting a type larger than the mapped page must return null.
        expect_null!(guest_ptr.as_ptr::<[u8; page::SIZE]>());
        guest_ptr.reset();
    }
}
