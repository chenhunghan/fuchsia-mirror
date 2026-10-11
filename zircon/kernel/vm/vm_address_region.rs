// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::vm_aspace::VmAspace;
use super::vm_mapping::VmMapping;
use super::vm_object::VmObject;
use crate::kernel::types::VAddr;
use crate::user_copy::UserInOutPtr;
use core::ffi::CStr;
use core::ptr::NonNull;
use fbl::{HasRefCount, OpaqueRefCountedFacade, Recyclable, RefCounted, RefPtr};
use zr::ToMutPtr;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

use vm_address_region_bindings as bindings;

fn name_to_c_buf(name: &[u8]) -> [u8; ZX_MAX_NAME_LEN] {
    let mut buf = [0u8; ZX_MAX_NAME_LEN];
    let len = name.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&name[..len]);
    buf
}

pub mod flag {
    use vm_address_region_bindings as bindings;

    /// When randomly allocating subregions, reduce sprawl by placing allocations near each other.
    pub const COMPACT: u32 = bindings::VMAR_FLAG_COMPACT;
    /// Request that the new region be at the specified offset in its parent region.
    pub const SPECIFIC: u32 = bindings::VMAR_FLAG_SPECIFIC;
    /// Like `VMAR_FLAG_SPECIFIC`, but permits overwriting existing mappings.
    pub const SPECIFIC_OVERWRITE: u32 = bindings::VMAR_FLAG_SPECIFIC_OVERWRITE;
    /// Allow `VmMappings` to be created inside the new region with the `SPECIFIC` or
    /// `OFFSET_IS_UPPER_LIMIT` flag.
    pub const CAN_MAP_SPECIFIC: u32 = bindings::VMAR_FLAG_CAN_MAP_SPECIFIC;
    /// Allow `VmMappings` to be created inside the region with read permissions.
    pub const CAN_MAP_READ: u32 = bindings::VMAR_FLAG_CAN_MAP_READ;
    /// Allow `VmMappings` to be created inside the region with write permissions.
    pub const CAN_MAP_WRITE: u32 = bindings::VMAR_FLAG_CAN_MAP_WRITE;
    /// Allow `VmMappings` to be created inside the region with execute permissions.
    pub const CAN_MAP_EXECUTE: u32 = bindings::VMAR_FLAG_CAN_MAP_EXECUTE;
    /// Require that VMO backing the mapping is non-resizable.
    pub const REQUIRE_NON_RESIZABLE: u32 = bindings::VMAR_FLAG_REQUIRE_NON_RESIZABLE;
    /// Allow VMO backings that could result in faults.
    pub const ALLOW_FAULTS: u32 = bindings::VMAR_FLAG_ALLOW_FAULTS;
    /// Treat the offset as an upper limit when allocating a VMO or child VMAR.
    pub const OFFSET_IS_UPPER_LIMIT: u32 = bindings::VMAR_FLAG_OFFSET_IS_UPPER_LIMIT;
    /// Opt this VMAR out of certain debugging checks. This allows for kernel mappings that have a
    /// more dynamic management strategy, that the regular checks would otherwise spuriously trip
    /// on.
    pub const DEBUG_DYNAMIC_KERNEL_MAPPING: u32 = bindings::VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING;
    /// Memory accesses past the stream size rounded up to the page boundary will fault.
    pub const FAULT_BEYOND_STREAM_SIZE: u32 = bindings::VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE;

    /// Mask of read, write, and execute permission flags.
    pub const CAN_RWX_FLAGS: u32 = bindings::VMAR_CAN_RWX_FLAGS;
}

/// Memory priorities that can be applied to VMARs and mappings to propagate to VMOs and page
/// tables.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPriority {
    /// Default overcommit priority where reclamation is allowed.
    Default = 0,
    /// High priority prevents all reclamation.
    High = 1,
}

zr::static_assert!(
    core::mem::size_of::<MemoryPriority>()
        == core::mem::size_of::<bindings::VmAddressRegionOrMapping_MemoryPriority>()
);
zr::static_assert!(
    MemoryPriority::Default as u8
        == bindings::VmAddressRegionOrMapping_MemoryPriority::DEFAULT as u8
);
zr::static_assert!(
    MemoryPriority::High as u8 == bindings::VmAddressRegionOrMapping_MemoryPriority::HIGH as u8
);

impl From<MemoryPriority> for bindings::VmAddressRegionOrMapping_MemoryPriority {
    fn from(priority: MemoryPriority) -> Self {
        match priority {
            MemoryPriority::Default => Self::DEFAULT,
            MemoryPriority::High => Self::HIGH,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmAddressRegionOpChildren {
    Yes = 0,
    No = 1,
}

zr::static_assert!(
    core::mem::size_of::<VmAddressRegionOpChildren>()
        == core::mem::size_of::<bindings::VmAddressRegionOpChildren>()
);
zr::static_assert!(
    VmAddressRegionOpChildren::Yes as u8 == bindings::VmAddressRegionOpChildren::Yes as u8
);
zr::static_assert!(
    VmAddressRegionOpChildren::No as u8 == bindings::VmAddressRegionOpChildren::No as u8
);

impl From<VmAddressRegionOpChildren> for bindings::VmAddressRegionOpChildren {
    fn from(op_children: VmAddressRegionOpChildren) -> Self {
        match op_children {
            VmAddressRegionOpChildren::Yes => Self::Yes,
            VmAddressRegionOpChildren::No => Self::No,
        }
    }
}
pub type RangeOpType = bindings::VmAddressRegion_RangeOpType;
pub type UnmapOptions = bindings::VmMapping_UnmapOptions;

/// Internal fully locked version of Destroy. Has controls to both skip the arch aspace unmapping
/// as well as removal from the parent subregions list. These controls facilitate the fine grained
/// control needed when splitting, merging and replacing mappings.
///
/// If unmap is `No` then this method is defined to never fail. `remove_region` does not impact
/// success or failure of the operation.
pub type DestroyUnmap = bindings::VmMapping_DestroyUnmap;
pub type DestroyRemoveFromParent = bindings::VmMapping_DestroyRemoveFromParent;

pub type Mergeable = bindings::VmMapping_Mergeable;

/// Fully locked version of Activate that can additionally control whether the region is installed
/// into the parent subregion list and vmo mapping list or not. This control exists to facilitate
/// the fine grained control needed for splitting, merging and replacing of mappings as when set to
/// `No` this method is defined as never failing.
pub type ActivateInsertRegions = bindings::VmMapping_ActivateInsertRegions;

/// Result of calling [`VmAddressRegion::create_vm_mapping`].
pub struct MapResult {
    /// The newly created mapping.
    pub mapping: RefPtr<VmMapping>,
    /// The virtual address of the mapping at creation time.
    pub base: usize,
}

/// A representation of a contiguous range of virtual address space
#[repr(C)]
pub struct VmAddressRegion {
    _facade: OpaqueRefCountedFacade,
}

impl HasRefCount for VmAddressRegion {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        let raw = unsafe { bindings::cpp_vm_address_region_get_ref_counted(self.as_ffi_ptr()) };
        // SAFETY: `raw` points to the `fbl::RefCounted` subobject of `self`.
        unsafe { &*raw.cast::<RefCounted>() }
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmAddressRegion {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` is the last reference to a live `VmAddressRegion`.
        unsafe { bindings::cpp_vm_address_region_free(ptr.as_ptr().cast()) }
    }
}

impl VmAddressRegion {
    fn as_ffi_ptr(&self) -> *mut bindings::VmAddressRegion {
        self.to_mut_ptr().cast()
    }

    /// Creates a subregion of this region.
    pub fn create_sub_vmar(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        name: &[u8],
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        let name_buf = name_to_c_buf(name);
        let mut status = 0;
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        let raw = unsafe {
            bindings::cpp_vm_address_region_create_sub_vmar(
                self.as_ffi_ptr(),
                offset,
                size,
                align_pow2,
                vmar_flags,
                name_buf.as_ptr().cast(),
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: `raw` is null or an owned reference to a live `VmAddressRegion`.
        unsafe { RefPtr::try_from_raw(raw.cast()).ok_or(Status::NO_MEMORY) }
    }

    /// Creates a [`VmMapping`] within this region.
    ///
    /// To avoid leaks, this should be paired with a call to [`VmMapping::destroy`] if desired;
    /// dropping `MapResult::mapping` will not destroy the mapping.
    #[allow(clippy::too_many_arguments)]
    pub fn create_vm_mapping(
        &self,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: &[u8],
    ) -> Result<MapResult, Status> {
        let name_buf = name_to_c_buf(name);
        let mut base = 0;
        let mut status = 0;
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        let raw = unsafe {
            bindings::cpp_vm_address_region_create_vm_mapping(
                self.as_ffi_ptr(),
                mapping_offset,
                size,
                align_pow2,
                vmar_flags,
                RefPtr::into_raw(vmo).cast(),
                vmo_offset,
                arch_mmu_flags,
                name_buf.as_ptr().cast(),
                &mut base,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: `raw` is null or an owned reference to a live `VmMapping`.
        let mapping = unsafe { RefPtr::try_from_raw(raw.cast()).ok_or(Status::NO_MEMORY)? };
        Ok(MapResult { mapping, base })
    }

    /// Destroys this region and recursively destroys child VMARs.
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe { bindings::cpp_vm_address_region_destroy(self.as_ffi_ptr()) })
    }

    /// Returns a reference to the address space this region belongs to.
    pub fn aspace(&self) -> &RefPtr<VmAspace> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { &*(bindings::cpp_vm_address_region_aspace(self.as_ffi_ptr()).cast()) }
    }

    /// Returns the base address of this region.
    pub fn base(&self) -> VAddr {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        VAddr(unsafe { bindings::cpp_vm_address_region_base(self.as_ffi_ptr()) })
    }

    /// Returns the size in bytes of this region.
    pub fn size(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { bindings::cpp_vm_address_region_size(self.as_ffi_ptr()) }
    }

    /// Returns the creation flags of this region.
    pub fn flags(&self) -> u32 {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { bindings::cpp_vm_address_region_flags(self.as_ffi_ptr()) }
    }

    /// Returns the name of this region.
    pub fn name(&self) -> &CStr {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { CStr::from_ptr(bindings::cpp_vm_address_region_name(self.as_ffi_ptr())) }
    }

    /// Returns true if this region has a parent region.
    pub fn has_parent(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { bindings::cpp_vm_address_region_has_parent(self.as_ffi_ptr()) }
    }

    /// Applies the given memory priority to this region and all subregions.
    pub fn set_memory_priority(&self, priority: MemoryPriority) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_set_memory_priority(self.as_ffi_ptr(), priority.into())
        })
    }

    /// Unmaps a subset of the region of memory in the containing address space.
    ///
    /// # Safety
    ///
    /// Caller must ensure the specified virtual address region to unmap is no longer used.
    pub unsafe fn unmap(
        &self,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_unmap(
                self.as_ffi_ptr(),
                base.0,
                size,
                op_children.into(),
            )
        })
    }

    /// Changes protections on a subset of the region of memory in the containing address space.
    pub fn protect(
        &self,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_protect(
                self.as_ffi_ptr(),
                base.0,
                size,
                new_arch_mmu_flags,
                op_children.into(),
            )
        })
    }

    /// Performs a VMO or mapping operation (`op`) across mappings in `[base, base + len)`.
    pub fn range_op(
        &self,
        op: RangeOpType,
        base: VAddr,
        len: usize,
        op_children: VmAddressRegionOpChildren,
        buffer: UserInOutPtr<u8>,
        buffer_size: usize,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_range_op(
                self.as_ffi_ptr(),
                op,
                base.0,
                len,
                op_children.into(),
                buffer.as_ptr().cast(),
                buffer_size,
            )
        })
    }

    /// Reserves a memory region within this VMAR without allocating physical pages.
    pub fn reserve_space(
        &self,
        name: &[u8],
        base: usize,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_reserve_space(
                self.as_ffi_ptr(),
                name_buf.as_ptr().cast(),
                base,
                size,
                arch_mmu_flags,
            )
        })
    }
}
