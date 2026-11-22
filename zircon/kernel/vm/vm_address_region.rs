// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHE_MASK, ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_INVALID,
    ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_RWX_MASK,
    ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED, ARCH_MMU_FLAG_UNCACHED_DEVICE,
    ARCH_MMU_FLAG_WRITE_COMBINING, ARCH_UNMAP_OPTION_ENLARGE, ARCH_UNMAP_OPTION_NONE, ArchMmuFlags,
};
use super::attribution::AttributionCounts;
use super::name::name_to_c_buf;
use super::pmm;
use super::vm::KERNEL_HEAP_VMAR;
use super::vm_address_region_enumerator::{
    MappingsOnly, VmAddressRegionEnumerator, VmarsAndMappings,
};
use super::vm_aspace::{
    KERNEL_ROOT_VMAR, VmAspace, VmAspaceLockClass, VmAspaceMutex, VmAspaceRegionLockClass,
    VmAspaceRegionMutex,
};
use super::vm_mapping::{Mergeable, VmMapping};
use super::vm_object::{EvictionHint, VmObject, VmObjectLockClass};
use super::vm_object_paged::VmObjectPaged;
use crate::arch_rs::curr_cpu_num;
use crate::counters::define_kcounter;
use crate::kernel::mp::{cpu_num_to_mask, get_online_mask};
use crate::kernel::types::VAddr;
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::user_copy::UserInOutPtr;
use crate::userabi::vdso::VDso;
use ::page as kernel_page;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};
use core::hint;
use core::mem::{ManuallyDrop, MaybeUninit, offset_of};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use debug::{ltracef, ltracef_level};
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr, UninitRecyclable};
use kalloc::{AllocError, Allocator, DefaultAllocator};
use kprint::kprint;
use ksync::{KCell, KMutexGuard, LockToken};
use pin_init::{PinInit, cast_pin_init, pin_data, pin_init, pinned_drop};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zx_status::Status;
use zx_types::{
    ZX_CACHE_POLICY_UNCACHED, ZX_CACHE_POLICY_UNCACHED_DEVICE, ZX_CACHE_POLICY_WRITE_COMBINING,
    ZX_MAX_NAME_LEN, zx_status_t,
};

const LOCAL_TRACE: u32 = 0;

// Number of attempted address range mapping, regardless of arguments.
define_kcounter!(VM_REGION_MAP_ALL_ATTEMPT, "vm.region.map.all.attempt", Sum);
// Number of successful address range mapping.
define_kcounter!(VM_REGION_MAP_ALL_SUCCESS, "vm.region.map.all.success", Sum);
// Number of attempted mapping at a specified address.
define_kcounter!(VM_REGION_MAP_SPECIFIC_ATTEMPT, "vm.region.map.specific.attempt", Sum);
// Number of successful mapping at a specified address.
define_kcounter!(VM_REGION_MAP_SPECIFIC_SUCCESS, "vm.region.map.specific.success", Sum);
// Number of attempted address range mapping with a requested upper limit.
define_kcounter!(VM_REGION_MAP_UPPER_BOUND_ATTEMPT, "vm.region.map.upper_bound.attempt", Sum);
// Number of successful address range mapping with a requested upper limit.
define_kcounter!(VM_REGION_MAP_UPPER_BOUND_SUCCESS, "vm.region.map.upper_bound.success", Sum);

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

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LifeCycleState {
    /// Initial state: not yet in the address space layout, so no `destroy` is needed.
    NotReady = 0,
    /// Usual state: information is representative of the address space layout. Must be destroyed
    /// before the last reference is dropped.
    Alive = 1,
    /// Object is invalid.
    Dead = 2,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOpType {
    Commit,
    Decommit,
    MapRange,
    Zero,
    DontNeed,
    AlwaysNeed,
    Prefetch,
}

/// Result of calling [`VmAddressRegion::create_vm_mapping`].
pub struct MapResult {
    /// The newly created mapping.
    pub mapping: RefPtr<VmMapping>,
    /// The virtual address of the mapping at creation time.
    pub base: usize,
}

/// The state shared by `VmAddressRegion` and `VmMapping`. Each stores it as its first field, at
/// offset 0, and C++ sees it as the opaque `VmAddressRegionOrMapping`, at the same address as the
/// `VmAddressRegion` or `VmMapping` that contains it.
///
/// Safety invariant: `is_mapping` is `true` exactly when this is the `common` field of a
/// `VmMapping`, and `false` exactly when it is the `common` field of a `VmAddressRegion`.
/// `RegionOrMappingCommon::recycle` and `VmAddressRegionOrMapping::from_raw` cast to the
/// containing type on it.
///
/// A `VmAddressRegion` or `VmMapping` is `NotReady` until it is added to the address space
/// layout, then `Alive` while it describes the virtual memory mappings of the address range it
/// represents in its parent `VmAspace`, and `Dead` once it is destroyed, when it has no meaning.
///
/// `ref_count` is atomic. `memory_priority` and `parent` are only accessed with the aspace lock
/// held, and `life_cycle` as its `# Safety` section states. The other fields are immutable after
/// construction.
#[repr(C)]
pub struct RegionOrMappingCommon {
    ref_count: RefCounted,
    canary: Canary<{ fbl::magic(b"VMRM") }>,
    is_mapping: bool,
    /// The current state of the region or mapping. If `LifeCycleState::Dead`, then all other
    /// fields are invalid.
    ///
    /// # Safety
    ///
    /// Read only with the aspace lock or the region lock held, or, for a `VmMapping`, with the
    /// object lock held. Written only with both the aspace lock and the region lock held, and for a
    /// `VmMapping` that has an `object` also with the object lock held, or before the region or
    /// mapping is shared.
    ///
    /// While this is `Alive`, a `Some` `parent` points to a live `VmAddressRegion` whose subregions
    /// contain this region or mapping, and a `VmMapping` has a `Some` `parent` and a `Some`
    /// `object`. Every child in the subregions of a `VmAddressRegion` is `Alive` and has that
    /// region as its `parent`. Once `Dead`, `parent` is `None` and a `VmMapping` has no `object`.
    /// Splitting, merging or replacing mappings may break this only while the region lock and the
    /// aspace lock stay held.
    life_cycle: UnsafeCell<LifeCycleState>,
    /// The priority of the region or mapping. This starts at `Default` and must be reset back to
    /// `Default` as part of the destroy path to ensure any propagation is undone correctly.
    memory_priority: KCell<MemoryPriority, VmAspaceLockClass>,
    /// The flags given at creation.
    flags: u32,
    /// The address and size within the containing address space.
    base: VAddr,
    size: usize,
    /// A pointer back to the address space this belongs to. The aspace's lock is used to serialize
    /// all modifications.
    aspace: RefPtr<VmAspace>,
    /// A pointer back to the parent region, or `None` if this is a root or has been destroyed.
    parent: KCell<Option<NonNull<VmAddressRegion>>, VmAspaceLockClass>,
}

::zr::static_assert!(cfg!(debug_assertions) == constants::kRefCountedAdoptionValidator);
zr::static_assert!(offset_of!(RegionOrMappingCommon, ref_count) == 0);
zr::static_assert!(
    align_of::<RegionOrMappingCommon>() >= align_of::<bindings::VmAddressRegionOrMapping>()
);

impl HasRefCount for RegionOrMappingCommon {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        &self.ref_count
    }
}

// SAFETY: `RegionOrMappingCommon` is not tied to the thread that created it.
unsafe impl Send for RegionOrMappingCommon {}
// SAFETY: Shared access from several threads is sound for each field: `ref_count` is atomic;
// `memory_priority` and `parent` are `KCell`s accessed only with a token for the aspace lock of
// `aspace`; `life_cycle` is accessed only as its `# Safety` section states; every other field is
// never mutated after construction.
unsafe impl Sync for RegionOrMappingCommon {}

impl RegionOrMappingCommon {
    /// # Safety
    ///
    /// The result is stored as the `common` field of a `VmMapping` if `is_mapping`, and of a
    /// `VmAddressRegion` otherwise.
    pub(super) unsafe fn new(
        base: usize,
        size: usize,
        flags: u32,
        aspace: &RefPtr<VmAspace>,
        parent: Option<NonNull<VmAddressRegion>>,
        is_mapping: bool,
    ) -> Self {
        Self {
            ref_count: RefCounted::new(),
            canary: Canary::new(),
            is_mapping,
            life_cycle: UnsafeCell::new(LifeCycleState::NotReady),
            memory_priority: KCell::new(MemoryPriority::Default),
            flags,
            base: VAddr(base),
            size,
            aspace: aspace.clone(),
            parent: KCell::new(parent),
        }
    }

    // Accessors.
    #[inline]
    pub(super) fn common_canary(&self) -> &Canary<{ fbl::magic(b"VMRM") }> {
        &self.canary
    }

    #[inline]
    pub fn base(&self) -> VAddr {
        self.base
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn flags(&self) -> u32 {
        self.flags
    }

    #[inline]
    pub fn aspace(&self) -> &RefPtr<VmAspace> {
        &self.aspace
    }

    /// Returns whether this is the first field of a `VmMapping` rather than of a
    /// `VmAddressRegion`.
    #[inline]
    pub fn is_mapping(&self) -> bool {
        self.is_mapping
    }

    /// Returns the aspace lock of `aspace()`.
    #[inline]
    pub fn lock(&self) -> &VmAspaceMutex {
        self.aspace.lock()
    }

    /// Returns the region lock of `aspace()`.
    #[inline]
    pub fn region_lock(&self) -> &VmAspaceRegionMutex {
        self.aspace.region_lock()
    }

    pub(super) fn lock_is_held(&self) -> bool {
        self.lock().raw_mutex().is_held()
    }

    pub(super) fn region_lock_is_held(&self) -> bool {
        self.region_lock().raw_mutex().is_held()
    }

    pub fn is_in_range(&self, base: VAddr, size: usize) -> bool {
        let offset = base.0.wrapping_sub(self.base().0);
        base.0 >= self.base().0 && offset < self.size() && self.size() - offset >= size
    }

    /// Returns true if the instance is alive and reporting information that
    /// reflects the address space layout.
    pub fn is_alive_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> bool {
        self.canary.assert();
        self.state_locked(token) == LifeCycleState::Alive
    }

    /// Check if the given `arch_mmu_flags` are allowed under this
    /// region's `flags()`
    pub(super) fn is_valid_mapping_flags(&self, arch_mmu_flags: ArchMmuFlags) -> bool {
        // Work out what flags we must support for these arch_mmu_flags
        let mut needed = 0;
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_READ) != 0 {
            needed |= flag::CAN_MAP_READ;
        }
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
            needed |= flag::CAN_MAP_WRITE;
        }
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
            needed |= flag::CAN_MAP_EXECUTE;
        }
        // Mask out the actual relevant mappings flags we have.
        let actual =
            self.flags() & (flag::CAN_MAP_READ | flag::CAN_MAP_WRITE | flag::CAN_MAP_EXECUTE);
        // Validate that every `needed` occurs in `actual`
        (needed & actual) == needed
    }

    #[inline]
    pub(super) fn state_locked(&self, _token: &LockToken<'_, VmAspaceLockClass>) -> LifeCycleState {
        // SAFETY: The aspace lock is held.
        unsafe { *self.life_cycle.get() }
    }

    #[inline]
    fn state_locked_region(
        &self,
        _token: &LockToken<'_, VmAspaceRegionLockClass>,
    ) -> LifeCycleState {
        // SAFETY: The region lock is held.
        unsafe { *self.life_cycle.get() }
    }

    /// Returns true if the instance is in the `Dead` state. Exclusive access stands in for the
    /// locks.
    #[inline]
    pub(super) fn is_dead_mut(&mut self) -> bool {
        *self.life_cycle.get_mut() == LifeCycleState::Dead
    }

    #[inline]
    pub(super) fn memory_priority_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
    ) -> MemoryPriority {
        // SAFETY: The aspace lock is held.
        unsafe { *self.memory_priority.get(token) }
    }

    #[inline]
    pub(super) fn set_memory_priority_field_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.memory_priority.get_mut(token) } = priority;
    }

    #[inline]
    pub(super) fn parent_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
    ) -> Option<NonNull<VmAddressRegion>> {
        // SAFETY: The aspace lock is held.
        unsafe { *self.parent.get(token) }
    }

    /// # Safety
    ///
    /// The parent of `self`, if any, is a live `VmAddressRegion` for `'a`.
    #[inline]
    pub(super) unsafe fn parent_ref_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> Option<&'a VmAddressRegion> {
        // SAFETY: The parent of `self`, if any, is a live `VmAddressRegion` for `'a`.
        self.parent_locked(token).map(|parent| unsafe { parent.as_ref() })
    }

    /// Transitions from `LifeCycleState::NotReady` to `LifeCycleState::Alive`.
    ///
    /// # Safety
    ///
    /// `self` is in `LifeCycleState::NotReady`, and either has no parent or is in the subregions
    /// of its parent, which is a live `VmAddressRegion`. For a `VmMapping`, its `object` is `Some`
    /// and the object lock is held.
    #[inline]
    unsafe fn set_alive_locked(
        &self,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) {
        // SAFETY: The region lock and the aspace lock are held, and for a `VmMapping` also the
        // object lock.
        unsafe { *self.life_cycle.get() = LifeCycleState::Alive };
    }

    /// Transitions to `LifeCycleState::Dead` and clears the parent.
    ///
    /// # Safety
    ///
    /// `self` is not in the subregions of its parent. For a `VmAddressRegion`, its subregions are
    /// empty. For a `VmMapping`, its `object` is `None` and the object lock of the object it had is
    /// held.
    #[inline]
    pub(super) unsafe fn mark_dead_locked(
        &self,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.parent.get_mut(aspace_token) } = None;
        // SAFETY: The region lock and the aspace lock are held, and for a `VmMapping` also the
        // object lock of the object it had.
        unsafe { *self.life_cycle.get() = LifeCycleState::Dead };
    }
}

impl Drop for RegionOrMappingCommon {
    fn drop(&mut self) {
        ltracef!("{:p}\n", self);

        assert!(*self.life_cycle.get_mut() != LifeCycleState::Alive);

        debug_assert!(*self.memory_priority.get_inner_mut() == MemoryPriority::Default);
    }
}

impl VmMapping {
    pub(super) fn is_alive_locked_object(&self, token: &LockToken<'_, VmObjectLockClass>) -> bool {
        (**self).canary.assert();
        self.state_locked_object(token) == LifeCycleState::Alive
    }

    /// Transitions from `LifeCycleState::NotReady` to `LifeCycleState::Alive`.
    ///
    /// # Safety
    ///
    /// `self` is in `LifeCycleState::NotReady` and in the subregions of its parent, which is a
    /// live `VmAddressRegion`, and its `object` is `Some`.
    #[inline]
    pub(super) unsafe fn set_alive_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: `self` is in `LifeCycleState::NotReady` and in the subregions of its parent,
        // which is a live `VmAddressRegion`, its `object` is `Some`, and the object lock is held.
        unsafe { RegionOrMappingCommon::set_alive_locked(self, region_token, aspace_token) };
    }

    /// For a `VmMapping` the state is only modified either with the `object` lock held, or if
    /// there is no `object`. Therefore it is safe to read the state if just the object lock is
    /// held.
    #[inline]
    pub(super) fn state_locked_object(
        &self,
        _token: &LockToken<'_, VmObjectLockClass>,
    ) -> LifeCycleState {
        // SAFETY: The object lock is held.
        unsafe { *self.life_cycle.get() }
    }

    #[inline]
    pub(super) fn as_region_or_mapping(&self) -> VmAddressRegionOrMapping<'_> {
        VmAddressRegionOrMapping::Mapping(self)
    }

    /// Destroys this mapping, as `VmAddressRegionOrMapping::destroy` describes.
    #[inline]
    pub fn destroy(&self) -> Result<(), Status> {
        self.as_region_or_mapping().destroy()
    }
}

/// A borrow of either a `VmAddressRegion` or a `VmMapping`, the two types that contain a
/// `RegionOrMappingCommon`.
#[derive(Clone, Copy)]
pub enum VmAddressRegionOrMapping<'a> {
    Region(&'a VmAddressRegion),
    Mapping(&'a VmMapping),
}

impl<'a> VmAddressRegionOrMapping<'a> {
    /// Returns the region or mapping whose `RegionOrMappingCommon` `ptr` points to.
    ///
    /// This does not check the canary. Callers should consider calling
    /// `common_canary().assert()` on the result.
    ///
    /// # Safety
    ///
    /// `ptr` is non-null, aligned, points to the `RegionOrMappingCommon` of a `VmAddressRegion` or
    /// `VmMapping` that is live for `'a`, and has the provenance of that whole object.
    #[inline]
    unsafe fn from_raw(ptr: *const RegionOrMappingCommon) -> Self {
        // SAFETY: `ptr` is non-null, aligned, and points to a `RegionOrMappingCommon` that is live
        // for `'a`.
        if unsafe { ptr.as_ref_unchecked() }.is_mapping() {
            // SAFETY: `ptr` points to the first field of a `VmMapping` that is live for `'a`, as
            // `is_mapping()` reports, and has the provenance of that mapping.
            Self::Mapping(unsafe { ptr.cast::<VmMapping>().as_ref_unchecked() })
        } else {
            // SAFETY: `ptr` points to the first field of a `VmAddressRegion` that is live for `'a`,
            // as `is_mapping()` reports, and has the provenance of that region.
            Self::Region(unsafe { ptr.cast::<VmAddressRegion>().as_ref_unchecked() })
        }
    }

    #[inline]
    pub(super) fn common(self) -> &'a RegionOrMappingCommon {
        match self {
            Self::Region(region) => region,
            Self::Mapping(mapping) => mapping,
        }
    }

    #[inline]
    fn as_ffi_ptr(self) -> *mut bindings::VmAddressRegionOrMapping {
        match self {
            Self::Region(region) => ptr::from_ref(region).cast_mut().cast(),
            Self::Mapping(mapping) => ptr::from_ref(mapping).cast_mut().cast(),
        }
    }

    /// Returns a new reference to the region or mapping that `self` borrows.
    #[inline]
    pub fn to_owned(self) -> OwnedVmAddressRegionOrMapping {
        match self {
            Self::Region(region) => RefPtr::from_ref(region).into(),
            Self::Mapping(mapping) => RefPtr::from_ref(mapping).into(),
        }
    }

    /// Returns an owner of the mapping, made from a shared reference to it.
    ///
    /// This checks the canary.
    #[inline]
    pub(super) fn as_vm_mapping(self) -> Option<RefPtr<VmMapping>> {
        self.common_canary().assert();
        match self {
            Self::Mapping(mapping) => Some(RefPtr::from_ref(mapping)),
            Self::Region(_) => None,
        }
    }

    /// Transition from `NotReady` to `Alive`, and add references to `self` to related
    /// structures.
    /// On error no state is changed and no references are added.
    ///
    /// # Safety
    ///
    /// `self` must be in `LifeCycleState::NotReady` with a live parent `VmAddressRegion`, no child
    /// of the parent has the same base as `self`, and `self` is kept alive by another owning
    /// reference for the duration of the call.
    unsafe fn activate(
        self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        match self {
            Self::Mapping(mapping) => {
                // SAFETY: `mapping` is in `LifeCycleState::NotReady` with a live parent, no child
                // of the parent has the same base as `mapping`, and `mapping` is kept alive by
                // another owning reference for the duration of the call.
                unsafe { VmMapping::activate(mapping, region_token, aspace_token) }
            }
            Self::Region(region) => {
                // SAFETY: `region` is in `LifeCycleState::NotReady` with a live parent, no child
                // of the parent has the same base as `region`, and `region` is kept alive by
                // another owning reference for the duration of the call.
                unsafe { VmAddressRegion::activate(region, region_token, aspace_token) }
            }
        }
    }

    /// Dump debug info.
    fn dump_locked(
        self,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        token: &LockToken<'_, VmAspaceLockClass>,
        depth: u32,
        verbose: bool,
    ) {
        match self {
            Self::Mapping(mapping) => {
                VmMapping::dump_locked(mapping, region_token, token, depth, verbose)
            }
            Self::Region(region) => {
                VmAddressRegion::dump_locked(region, region_token, token, depth, verbose)
            }
        }
    }

    /// Performs any actions necessary to apply a high memory priority over the given range. This
    /// method is always safe to call as it will internally check the memory priority status and
    /// skip if necessary, so the caller does not need to worry about races with a different memory
    /// priority being applied. As this may need to acquire the lock even to check the memory
    /// priority, if the caller knows they have not caused this to become high priority (i.e. they
    /// have called `set_memory_priority_locked` with `MemoryPriority::Default`), then calling this
    /// should be skipped for performance. Memory that needs to be committed for a high memory
    /// priority are user pager backed pages and any compressed or loaned pages. Anonymous pages and
    /// copy-on-write pages do not allocated / committed. This method has no return value as it is
    /// entirely best effort and no part of its operation is needed for correctness.
    fn commit_high_memory_priority(self) {
        match self {
            Self::Mapping(mapping) => VmMapping::commit_high_memory_priority(mapping),
            Self::Region(region) => VmAddressRegion::commit_high_memory_priority(region),
        }
    }

    /// If a VMO-mapping, unmap all pages and remove dependency on `VmObject` it has a ref to.
    /// Otherwise recursively destroy child VMARs and transition to the `Dead` state.
    ///
    /// Returns `Ok(())` on success, `Err(Status::BAD_STATE)` if already dead, and other
    /// values on error (typically unmap failure).
    fn destroy(self) -> Result<(), Status> {
        self.canary.assert();

        ksync::lock!(let mut region_guard = self.region_lock().lock());
        ksync::lock!(let mut guard = self.lock().lock());
        if self.state_locked(guard.token()) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        self.to_owned()
            .destroy_locked(region_guard.as_mut().token_mut(), guard.as_mut().token_mut())
    }
}

impl Deref for VmAddressRegionOrMapping<'_> {
    type Target = RegionOrMappingCommon;

    #[inline]
    fn deref(&self) -> &RegionOrMappingCommon {
        self.common()
    }
}

// SAFETY: `allocate` is not overridden. `RegionOrMappingCommon::new` requires its result to be
// stored as the `common` field, at offset 0, of a `VmMapping` if `is_mapping` and of a
// `VmAddressRegion` otherwise, and the `HasRefCount` of both returns this `ref_count`, so the last
// release here is the last release of that object. `recycle` reads `is_mapping` through `ptr` and
// then recycles the whole object with `free_exposed`, which needs only `ptr`'s address. As for
// `VmAddressRegion` and `VmMapping`, every such object is in storage `allocate_exposed` returned
// except `KERNEL_ROOT_VMAR` and `KERNEL_HEAP_VMAR`, whose last reference is never released: the
// references adopted for them are held in `ManuallyDrop` and never released.
unsafe impl Recyclable for RegionOrMappingCommon {
    // Cold and out of line, so the destructor is not inlined into every `RefPtr` drop.
    #[cold]
    #[inline(never)]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` points to the `RegionOrMappingCommon` of an initialized region or mapping.
        if unsafe { ptr.as_ref() }.is_mapping() {
            // SAFETY: `ptr` points to the first field, at offset 0, of an initialized `VmMapping`
            // in storage returned by `allocate_exposed` to which no reference remains, and is not
            // used afterwards.
            unsafe { VmMapping::recycle(ptr.cast()) }
        } else {
            // SAFETY: `ptr` points to the first field, at offset 0, of an initialized
            // `VmAddressRegion` in storage returned by `allocate_exposed` to which no reference
            // remains, and is not used afterwards.
            unsafe { VmAddressRegion::recycle(ptr.cast()) }
        }
    }
}

/// A reference-counted pointer to a region or a mapping.
#[derive(Clone)]
#[repr(transparent)]
pub struct OwnedVmAddressRegionOrMapping(
    // Safety invariant: points to the `common` field of a `VmAddressRegion` or `VmMapping`, with
    // the provenance of that whole object (never from `RefPtr::from_ref` on a
    // `&RegionOrMappingCommon`).
    RefPtr<RegionOrMappingCommon>,
);

impl From<RefPtr<VmAddressRegion>> for OwnedVmAddressRegionOrMapping {
    #[inline]
    fn from(region: RefPtr<VmAddressRegion>) -> Self {
        // SAFETY: The `RegionOrMappingCommon` at offset 0 of `region` has `is_mapping == false`, so
        // `RegionOrMappingCommon::recycle` recycles it as a `VmAddressRegion`, and `RefPtr::cast`
        // keeps the provenance of the whole region, as the safety invariant of `Self` requires.
        Self(unsafe { region.cast() })
    }
}

impl From<RefPtr<VmMapping>> for OwnedVmAddressRegionOrMapping {
    #[inline]
    fn from(mapping: RefPtr<VmMapping>) -> Self {
        // SAFETY: The `RegionOrMappingCommon` at offset 0 of `mapping` has `is_mapping == true`, so
        // `RegionOrMappingCommon::recycle` recycles it as a `VmMapping`, and `RefPtr::cast` keeps
        // the provenance of the whole mapping, as the safety invariant of `Self` requires.
        Self(unsafe { mapping.cast() })
    }
}

impl OwnedVmAddressRegionOrMapping {
    /// Returns a borrowed view of the region or mapping.
    ///
    /// This does not check the canary. Callers should consider calling
    /// `common_canary().assert()` first.
    #[inline]
    pub fn as_ref(&self) -> VmAddressRegionOrMapping<'_> {
        // SAFETY: `self.0` is non-null, aligned, points to the `RegionOrMappingCommon` of a
        // `VmAddressRegion` or `VmMapping` that is live while `self` is borrowed, and has the
        // provenance of that whole object.
        unsafe { VmAddressRegionOrMapping::from_raw(RefPtr::as_ptr(&self.0)) }
    }

    /// Returns the region as `Ok`, or the mapping as `Err`.
    ///
    /// This does not check the canary. Callers should consider calling
    /// `common_canary().assert()` first.
    #[inline]
    pub(super) fn into_region_or_mapping(
        self,
    ) -> Result<RefPtr<VmAddressRegion>, RefPtr<VmMapping>> {
        if self.is_mapping() {
            // SAFETY: `self.0` was cast from a `RefPtr<VmMapping>`, as `is_mapping()` reports.
            Err(unsafe { self.0.cast() })
        } else {
            // SAFETY: `self.0` was cast from a `RefPtr<VmAddressRegion>`, as `is_mapping()`
            // reports.
            Ok(unsafe { self.0.cast() })
        }
    }

    /// Returns the region, or `None` if this is a mapping.
    ///
    /// This checks the canary.
    #[inline]
    pub fn into_vm_address_region(self) -> Option<RefPtr<VmAddressRegion>> {
        self.common_canary().assert();
        self.into_region_or_mapping().ok()
    }

    /// Returns the mapping, or `None` if this is a region.
    ///
    /// This checks the canary.
    #[inline]
    pub fn into_vm_mapping(self) -> Option<RefPtr<VmMapping>> {
        self.common_canary().assert();
        self.into_region_or_mapping().err()
    }

    /// Leaks the reference `self` holds, and returns a pointer to the `RegionOrMappingCommon` of
    /// the region or mapping with the provenance of the whole object, by the safety invariant of
    /// `self.0`.
    #[inline]
    pub(super) fn into_raw(self) -> NonNull<RegionOrMappingCommon> {
        let ptr = RefPtr::into_raw(self.0);
        // SAFETY: `RefPtr::into_raw` returns a non-null pointer.
        unsafe { NonNull::new_unchecked(ptr.cast_mut()) }
    }

    #[inline]
    fn destroy_locked(
        self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        match self.into_region_or_mapping() {
            Err(mapping) => VmMapping::destroy_locked(mapping, region_token, aspace_token),
            Ok(region) => VmAddressRegion::destroy_locked(region, region_token, aspace_token),
        }
    }
}

impl Deref for OwnedVmAddressRegionOrMapping {
    type Target = RegionOrMappingCommon;

    #[inline]
    fn deref(&self) -> &RegionOrMappingCommon {
        &self.0
    }
}

/// A representation of a contiguous range of virtual address space. It is partitioned by
/// non-overlapping children of the following types:
/// 1) child `VmAddressRegion`
/// 2) child `VmMapping` (leaves that map `VmObject`s into the address space)
/// 3) gaps (logical, not actually objects).
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmAddressRegion {
    common: RegionOrMappingCommon,
    canary: ManuallyDrop<Canary<{ fbl::magic(b"VMAR") }>>,
    name: [u8; ZX_MAX_NAME_LEN],
    /// list of memory regions, indexed by base address.
    ///
    /// This can be read with either lock held, but requires both locks to write it.
    #[pin]
    subregions: ManuallyDrop<region_list::Subregions>,
}

zr::static_assert!(offset_of!(VmAddressRegion, common) == 0);

#[pinned_drop]
impl PinnedDrop for VmAddressRegion {
    #[inline]
    fn drop(self: Pin<&mut Self>) {
        // `canary` and `subregions` are `ManuallyDrop` and dropped here, before the compiler
        // drops `common`, so that `common` and its `RefPtr<VmAspace>` are dropped last, as C++
        // destroys a derived class before its base.
        let this = self.project();
        // SAFETY: `canary` was initialized by `new`, this is its only drop as `PinnedDrop::drop`
        // runs once, and it is not used after this.
        unsafe { ManuallyDrop::drop(this.canary) };
        // SAFETY: The result is only used to drop `subregions` in place, which does not move it.
        let subregions = unsafe { Pin::get_unchecked_mut(this.subregions) };
        // SAFETY: `subregions` was initialized by `new`, this is its only drop as
        // `PinnedDrop::drop` runs once, and it is not used after this.
        unsafe { ManuallyDrop::drop(subregions) };
    }
}

/// # Safety
///
/// Once initialized, the inner `T` must be dropped in place before its storage is deallocated or
/// reused.
pub(super) const unsafe fn pin_init_manually_drop<T, E>(
    init: impl PinInit<T, E>,
) -> impl PinInit<ManuallyDrop<T>, E> {
    // SAFETY: `ManuallyDrop<T>` is `repr(transparent)` over `T`.
    unsafe { cast_pin_init(init) }
}

/// Allocates a `kalloc::Box<MaybeUninit<T>>`, leaks it, and exposes the provenance of the leaked
/// pointer.
#[inline]
pub(super) fn allocate_exposed<T>() -> Result<NonNull<MaybeUninit<T>>, AllocError> {
    let ptr = kalloc::Box::into_raw(kalloc::Box::try_new_uninit()?);
    ptr.expose_provenance();
    // SAFETY: `Box::into_raw` returns a non-null pointer.
    Ok(unsafe { NonNull::new_unchecked(ptr) })
}

/// Drops and frees the `T` at `ptr` through the provenance `allocate_exposed` exposed for it.
///
/// # Safety
///
/// `ptr` points to an initialized `T` in storage that `allocate_exposed::<T>` returned. No
/// reference to the `T` remains, and no pointer to it is used afterwards.
#[inline]
pub(super) unsafe fn free_exposed<T>(ptr: NonNull<T>) {
    let ptr = NonNull::<T>::with_exposed_provenance(ptr.addr());
    // SAFETY: `ptr` has the address of the storage `allocate_exposed` returned and the provenance
    // it exposed, which permits reads and writes of the whole allocation. The storage is aligned
    // for `T` and holds an initialized `T` that no reference or other pointer is used to reach,
    // so it is dropped here exactly once, in place, before its storage is freed.
    unsafe { ptr.drop_in_place() };
    // SAFETY: `allocate_exposed` allocated the storage with `kalloc::Box::try_new_uninit`, which
    // allocates a `MaybeUninit<T>` with `DefaultAllocator` and `Layout::new::<T>()`, or, if `T` is
    // zero-sized, does not allocate, and `deallocate` with a zero-sized layout does nothing. `ptr`
    // has the address of that allocation and the provenance exposed for it, the `T` in it was
    // dropped above, and the storage is not used afterwards.
    unsafe { DefaultAllocator.deallocate(ptr.cast(), Layout::new::<T>()) };
}

// SAFETY: `allocate_uninit` allocates with `allocate_exposed`, `recycle_uninit` frees with
// `kalloc::Box` through the pointer `allocate_uninit` returned, and `recycle` frees with
// `free_exposed`, so through any pointer to the region, whatever its provenance. `new` requires
// every `VmAddressRegion` to be constructed in place by `RefPtr::pin_init` in storage that
// `allocate_uninit` returned, or in static storage whose last reference is never released. The
// statics `KERNEL_ROOT_VMAR` and `KERNEL_HEAP_VMAR` are the latter: the references adopted for
// them are held in `ManuallyDrop` and never released, so they are never recycled.
unsafe impl Recyclable for VmAddressRegion {
    // Cold and out of line, so the destructor is not inlined into every `RefPtr` drop.
    #[cold]
    #[inline(never)]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` points to an initialized `VmAddressRegion`, which is in storage that
        // `allocate_exposed` returned, has no references left and is not used afterwards.
        unsafe { free_exposed(ptr) }
    }
}

// SAFETY: As for `Recyclable` above.
unsafe impl UninitRecyclable for VmAddressRegion {
    fn allocate_uninit() -> Result<NonNull<MaybeUninit<Self>>, AllocError> {
        allocate_exposed()
    }

    unsafe fn recycle_uninit(ptr: NonNull<MaybeUninit<Self>>) {
        // SAFETY: `ptr` was allocated as a `kalloc::Box`, has no references left and is not used
        // afterwards.
        drop(unsafe { kalloc::Box::from_non_null(ptr) });
    }
}

impl HasRefCount for VmAddressRegion {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
    }
}

impl Deref for VmAddressRegion {
    type Target = RegionOrMappingCommon;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

// SAFETY: `VmAddressRegion` is not tied to the thread that created it.
unsafe impl Send for VmAddressRegion {}
// SAFETY: Mutable state of `VmAddressRegion` is only accessed with the aspace lock or the region
// lock held.
unsafe impl Sync for VmAddressRegion {}

impl VmAddressRegion {
    /// Returns a `VmAddressRegionOrMapping` that borrows this region.
    #[inline]
    pub fn as_region_or_mapping(&self) -> VmAddressRegionOrMapping<'_> {
        VmAddressRegionOrMapping::Region(self)
    }

    /// Destroys this region, as `VmAddressRegionOrMapping::destroy` describes.
    #[inline]
    pub fn destroy(&self) -> Result<(), Status> {
        self.as_region_or_mapping().destroy()
    }

    /// # Safety
    ///
    /// The caller must ensure that the region is initialized either by `RefPtr::pin_init`, in
    /// storage that `allocate_uninit` returned, or in static storage whose last reference is never
    /// released.
    unsafe fn new(
        base: usize,
        size: usize,
        vmar_flags: u32,
        aspace: &RefPtr<VmAspace>,
        parent: Option<&VmAddressRegion>,
        name: &[u8],
    ) -> impl PinInit<Self> {
        let parent = parent.map(NonNull::from);
        pin_init!(Self {
            // SAFETY: The result is stored as the `common` field of this `VmAddressRegion`, and
            // `is_mapping` is `false`.
            common: unsafe {
                RegionOrMappingCommon::new(
                    base,
                    size,
                    vmar_flags,
                    aspace,
                    parent,
                    false,
                )
            },
            canary: ManuallyDrop::new(Canary::new()),
            name: name_to_c_buf(name),
            // SAFETY: `PinnedDrop::drop` drops `subregions` in place, and no later initializer
            // fails.
            subregions <- unsafe { pin_init_manually_drop(region_list::Subregions::new()) },
        })
    }

    #[inline]
    pub(crate) fn trace_new(&self) {
        ltracef!("{:p}\n", self);
        ltracef!("{:p} '{}'\n", self, zr::from_utf8_lossy(self.name().to_bytes()));
    }

    /// # Safety
    ///
    /// The caller must ensure that the region is initialized either by `RefPtr::pin_init`, in
    /// storage that `allocate_uninit` returned, or in static storage whose last reference is never
    /// released.
    pub(crate) unsafe fn new_root(
        aspace: &RefPtr<VmAspace>,
        base: usize,
        size: usize,
        vmar_flags: u32,
    ) -> impl PinInit<Self> {
        // SAFETY: The caller initializes the region by `RefPtr::pin_init` in storage that
        // `allocate_uninit` returned, or in static storage whose last reference is never released.
        unsafe {
            Self::new(
                base,
                size,
                vmar_flags | flag::CAN_RWX_FLAGS,
                // We add in CAN_RWX_FLAGS above, since an address space can't usefully
                // contain a process without all of these.
                aspace,
                None,
                b"root",
            )
        }
    }

    /// Constructor for use in creating the kernel aspace singleton.
    ///
    /// # Safety
    ///
    /// `kernel_aspace` is the kernel aspace, and this is its only root region. The region is
    /// initialized in static storage whose last reference is never released.
    pub(super) unsafe fn new_kernel_root(kernel_aspace: &RefPtr<VmAspace>) -> impl PinInit<Self> {
        // SAFETY: The caller initializes the region in static storage whose last reference is
        // never released.
        unsafe {
            Self::new_root(
                kernel_aspace,
                kernel_aspace.base(),
                kernel_aspace.size(),
                flag::CAN_MAP_SPECIFIC,
            )
        }
        .pin_chain(|this| {
            // Activate the kernel root aspace immediately
            *this.project().common.life_cycle.get_mut() = LifeCycleState::Alive;
            Ok(())
        })
    }

    /// # Safety
    ///
    /// The caller must ensure that the region is initialized either by `RefPtr::pin_init`, in
    /// storage that `allocate_uninit` returned, or in static storage whose last reference is never
    /// released.
    pub(crate) unsafe fn new_child(
        parent: &VmAddressRegion,
        base: usize,
        size: usize,
        vmar_flags: u32,
        name: &[u8],
    ) -> impl PinInit<Self> {
        // SAFETY: The caller initializes the region by `RefPtr::pin_init` in storage that
        // `allocate_uninit` returned, or in static storage whose last reference is never released.
        unsafe { Self::new(base, size, vmar_flags, parent.aspace(), Some(parent), name) }
    }

    /// Creates a root region. This will span the entire aspace.
    pub(super) fn create_root_locked(
        aspace: &RefPtr<VmAspace>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        vmar_flags: u32,
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        // SAFETY: The region is initialized by `RefPtr::pin_init`, in storage that
        // `allocate_uninit` returned.
        let init = unsafe { Self::new_root(aspace, aspace.base(), aspace.size(), vmar_flags) };
        let vmar = RefPtr::pin_init(init).map_err(|_| {
            hint::cold_path();
            Status::NO_MEMORY
        })?;
        vmar.trace_new();
        // SAFETY: `vmar` was just constructed in `LifeCycleState::NotReady` with no parent.
        unsafe { vmar.set_alive_locked(region_token, aspace_token) };
        Ok(vmar)
    }

    /// Helper to share code between `create_sub_vmar` and `create_vm_mapping`.
    ///
    /// # Safety
    ///
    /// `size` is page aligned. If `vmo` is `Some`, `vmo_offset` is page aligned and
    /// `vmo_offset + size as u64` does not overflow. If `vmar_flags` contains
    /// `flag::SPECIFIC_OVERWRITE`, nothing may access
    /// `[self.base() + offset, self.base() + offset + size)`, or the rest of any large page
    /// it partly covers, in a way that relies on it staying mapped, and no Rust reference into
    /// that memory may be live.
    #[expect(clippy::too_many_arguments)]
    unsafe fn create_sub_vmar_internal(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: Option<RefPtr<VmObject>>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: &[u8],
    ) -> Result<(OwnedVmAddressRegionOrMapping, VAddr), Status> {
        // SAFETY: `size` is page aligned. If `vmo` is `Some`, `vmo_offset` is page aligned and
        // `vmo_offset + size as u64` does not overflow. If `vmar_flags` contains
        // `flag::SPECIFIC_OVERWRITE`, nothing relies on
        // `[self.base() + offset, self.base() + offset + size)`, or on the rest of any large page
        // it partly covers, staying mapped, and no Rust reference into that memory is live.
        let result = unsafe {
            self.create_sub_vmar_inner(
                offset,
                size,
                align_pow2,
                vmar_flags,
                vmo,
                vmo_offset,
                arch_mmu_flags,
                name,
            )
        };

        let is_specific = vmar_flags & (flag::SPECIFIC | flag::SPECIFIC_OVERWRITE) != 0;
        let is_upper_bound = vmar_flags & flag::OFFSET_IS_UPPER_LIMIT != 0;

        VM_REGION_MAP_ALL_ATTEMPT.add(1);
        if is_specific {
            VM_REGION_MAP_SPECIFIC_ATTEMPT.add(1);
        } else if is_upper_bound {
            VM_REGION_MAP_UPPER_BOUND_ATTEMPT.add(1);
        }

        if result.is_ok() {
            VM_REGION_MAP_ALL_SUCCESS.add(1);
            if is_specific {
                VM_REGION_MAP_SPECIFIC_SUCCESS.add(1);
            } else if is_upper_bound {
                VM_REGION_MAP_UPPER_BOUND_SUCCESS.add(1);
            }
        } else {
            hint::cold_path();
        }
        result
    }

    /// # Safety
    ///
    /// `size` is page aligned. If `vmo` is `Some`, `vmo_offset` is page aligned and
    /// `vmo_offset + size as u64` does not overflow. If `vmar_flags` contains
    /// `flag::SPECIFIC_OVERWRITE`, nothing may access
    /// `[self.base() + offset, self.base() + offset + size)`, or the rest of any large page
    /// it partly covers, in a way that relies on it staying mapped, and no Rust reference into
    /// that memory may be live.
    #[expect(clippy::too_many_arguments)]
    unsafe fn create_sub_vmar_inner(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        mut vmar_flags: u32,
        vmo: Option<RefPtr<VmObject>>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: &[u8],
    ) -> Result<(OwnedVmAddressRegionOrMapping, VAddr), Status> {
        let vmar: OwnedVmAddressRegionOrMapping;
        let (new_base, memory_priority) = {
            ksync::lock!(let mut region_guard = self.region_lock().lock());
            if self.state_locked_region(region_guard.token()) != LifeCycleState::Alive {
                hint::cold_path();
                return Err(Status::BAD_STATE);
            }

            if size == 0 {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }

            // Check if there are any RWX privileges that the child would have that the
            // parent does not.
            if vmar_flags & !self.flags() & flag::CAN_RWX_FLAGS != 0 {
                hint::cold_path();
                return Err(Status::ACCESS_DENIED);
            }

            // Extract the creation only flags out of `vmar_flags`, leaving the remaining to be
            // stored permanently in the object.
            let is_specific_overwrite = vmar_flags & flag::SPECIFIC_OVERWRITE != 0;
            let is_specific = (vmar_flags & flag::SPECIFIC != 0) || is_specific_overwrite;
            let is_upper_bound = vmar_flags & flag::OFFSET_IS_UPPER_LIMIT != 0;
            vmar_flags &=
                !(flag::SPECIFIC_OVERWRITE | flag::SPECIFIC | flag::OFFSET_IS_UPPER_LIMIT);
            if is_specific && is_upper_bound {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }
            if !is_specific && !is_upper_bound && offset != 0 {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }
            if !kernel_page::is_page_rounded(offset) {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }

            // Check that we have the required privileges if we want a SPECIFIC or
            // UPPER_LIMIT mapping.
            if (is_specific || is_upper_bound) && self.flags() & flag::CAN_MAP_SPECIFIC == 0 {
                hint::cold_path();
                return Err(Status::ACCESS_DENIED);
            }

            if !is_upper_bound && (offset >= self.size() || size > self.size() - offset) {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }
            if is_upper_bound && (offset > self.size() || size > self.size() || size > offset) {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }

            let new_base = if is_specific {
                // This would not overflow because offset <= size() - 1,
                // base() + offset <= base() + size() - 1.
                let new_base = VAddr(self.base().0 + offset);
                if align_pow2 > 0 {
                    let align_mask = (1usize << align_pow2) - 1;
                    if (new_base.0 & align_mask) != 0 {
                        hint::cold_path();
                        return Err(Status::INVALID_ARGS);
                    }
                }
                if !region_list::is_range_available(self, region_guard.token(), new_base, size) {
                    if is_specific_overwrite {
                        ksync::lock!(let mut guard = self.lock().lock());
                        let vmo = vmo.expect("vmo must be set");
                        // SAFETY: `self` was verified `Alive` and `size != 0` was checked above,
                        // `size` is page aligned by caller precondition, and `new_base`
                        // (`self.base() + offset`) is page aligned because `self` is `Alive` and
                        // `offset` is page aligned and in bounds. As `vmo` is `Some`, the caller
                        // guarantees `vmo_offset` is page aligned and `vmo_offset + size as u64`
                        // does not overflow. With `flag::SPECIFIC_OVERWRITE` set, the caller
                        // guarantees nothing relies on `[new_base, new_base + size)`, or on the
                        // rest of any large page it partly covers, staying mapped, and no Rust
                        // reference into that memory is live.
                        let vmar = unsafe {
                            self.overwrite_vm_mapping_locked(
                                region_guard.as_mut().token_mut(),
                                guard.as_mut().token_mut(),
                                new_base,
                                size,
                                vmar_flags,
                                vmo,
                                vmo_offset,
                                arch_mmu_flags,
                            )
                        }?;
                        return Ok((vmar, new_base));
                    }
                    hint::cold_path();
                    return Err(Status::ALREADY_EXISTS);
                }
                new_base
            } else {
                // If we're not mapping to a specific place, search for an opening.
                let upper_bound = if is_upper_bound {
                    VAddr(self.base().0.wrapping_add(offset))
                } else {
                    VAddr(usize::MAX)
                };
                self.alloc_spot_locked_region(
                    region_guard.token(),
                    size,
                    align_pow2,
                    arch_mmu_flags,
                    upper_bound,
                )?
            };

            // Notice if this is an executable mapping from the vDSO VMO
            // before we lose the VMO reference by moving `vmo`.
            let is_vdso_code = vmo.as_ref().is_some_and(|vmo| {
                (arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 && VDso::vmo_is_vdso(vmo)
            });

            let created = if let Some(vmo) = vmo {
                // Check that VMOs that back kernel mappings start off with their pages pinned,
                // unless the dynamic flag has been set to opt out of this specific check.
                debug_assert!(
                    self.aspace().is_user()
                        || self.aspace().is_guest_physical()
                        || vmar_flags & flag::DEBUG_DYNAMIC_KERNEL_MAPPING != 0
                        || vmo.debug_is_range_pinned(vmo_offset, size as u64)
                );
                VmMapping::create(
                    self,
                    false,
                    new_base.0,
                    size,
                    vmar_flags,
                    vmo,
                    if is_upper_bound { 0 } else { vmo_offset },
                    arch_mmu_flags,
                    Mergeable::No,
                )
                .map(OwnedVmAddressRegionOrMapping::from)
            } else {
                // SAFETY: The region is initialized by `RefPtr::pin_init`, in storage that
                // `allocate_uninit` returned.
                let init =
                    unsafe { VmAddressRegion::new_child(self, new_base.0, size, vmar_flags, name) };
                RefPtr::pin_init(init)
                    .ok()
                    .inspect(|vmar| vmar.trace_new())
                    .map(OwnedVmAddressRegionOrMapping::from)
            };

            let Some(created) = created else {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            };
            vmar = created;

            ksync::lock!(let mut guard = self.lock().lock());

            if is_vdso_code {
                hint::cold_path();
                // For an executable mapping of the vDSO, allow only one per process
                // and only for the valid range of the image.
                if self.aspace().vdso_code_mapping_locked(guard.token()).is_some()
                    || !VDso::valid_code_mapping(vmo_offset, size)
                {
                    hint::cold_path();
                    return Err(Status::ACCESS_DENIED);
                }
                let VmAddressRegionOrMapping::Mapping(mapping) = vmar.as_ref() else {
                    panic!("vDSO code mapping is a mapping");
                };
                let mapping = RefPtr::from_ref(mapping);
                self.aspace().set_vdso_code_mapping_locked(guard.as_mut().token_mut(), mapping);
            }

            debug_assert!(vmar.lock_is_held());
            debug_assert!(vmar.region_lock_is_held());
            // SAFETY: `vmar` was just constructed in `LifeCycleState::NotReady` with live parent
            // `self`, and a local `RefPtr` holds an owning reference to it across the call.
            // `is_range_available` or `alloc_spot_locked_region` verified that no child of `self`
            // has base `new_base`, and `region_guard` has been held continuously since.
            unsafe {
                vmar.as_ref()
                    .activate(region_guard.as_mut().token_mut(), guard.as_mut().token_mut())
            }?;
            // Propagate any memory priority settings. This should only fail if not alive, but we
            // hold the lock and just made it alive, so that cannot happen.
            let priority = self.memory_priority_locked(guard.token());
            vmar.common_canary().assert();
            match vmar.as_ref() {
                VmAddressRegionOrMapping::Mapping(downcast_mapping) => {
                    downcast_mapping
                        .set_memory_priority_locked::<false>(guard.as_mut().token_mut(), priority);
                }
                VmAddressRegionOrMapping::Region(downcast_vmar) => {
                    let status = downcast_vmar
                        .set_memory_priority_locked(guard.as_mut().token_mut(), priority);
                    debug_assert!(status.is_ok(), "status: {}", Status::result_into_raw(status));
                }
            }
            (new_base, priority)
        };

        if memory_priority == MemoryPriority::High {
            hint::cold_path();
            vmar.as_ref().commit_high_memory_priority();
        }
        Ok((vmar, new_base))
    }

    /// Creates a subregion of this region
    pub fn create_sub_vmar(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        name: &[u8],
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        if !kernel_page::is_page_rounded(size) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // Check that only allowed flags have been set
        if vmar_flags
            & !(flag::SPECIFIC
                | flag::CAN_MAP_SPECIFIC
                | flag::COMPACT
                | flag::CAN_RWX_FLAGS
                | flag::OFFSET_IS_UPPER_LIMIT)
            != 0
        {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // SAFETY: `size` is page aligned, `vmo` is `None`, and `vmar_flags` does not contain
        // `flag::SPECIFIC_OVERWRITE`.
        let (res, _base) = unsafe {
            self.create_sub_vmar_internal(
                offset,
                size,
                align_pow2,
                vmar_flags,
                None,
                0,
                ARCH_MMU_FLAG_INVALID,
                name,
            )
        }?;
        Ok(res.into_region_or_mapping().ok().expect("res is a region"))
    }

    /// Creates a [`VmMapping`] within this region.
    ///
    /// To avoid leaks, this should be paired with a call to
    /// [`VmMapping::destroy`] if desired; dropping `MapResult::mapping` will not
    /// destroy the mapping.
    ///
    /// Panics if `vmar_flags` contains `flag::SPECIFIC_OVERWRITE`; use
    /// [`Self::create_vm_mapping`] to overwrite.
    #[expect(clippy::too_many_arguments)]
    #[inline]
    pub fn create_vm_mapping_no_overwrite(
        &self,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<MapResult, Status> {
        assert!(vmar_flags & flag::SPECIFIC_OVERWRITE == 0);
        // SAFETY: `vmar_flags` does not contain `flag::SPECIFIC_OVERWRITE`.
        unsafe {
            self.create_vm_mapping(
                mapping_offset,
                size,
                align_pow2,
                vmar_flags,
                vmo,
                vmo_offset,
                arch_mmu_flags,
            )
        }
    }

    /// Creates a [`VmMapping`] within this region.
    ///
    /// To avoid leaks, this should be paired with a call to
    /// [`VmMapping::destroy`] if desired; dropping `MapResult::mapping` will not
    /// destroy the mapping.
    ///
    /// # Safety
    ///
    /// If `vmar_flags` contains `flag::SPECIFIC_OVERWRITE`, nothing may access
    /// `[self.base() + mapping_offset, self.base() + mapping_offset + size)`, or the rest of any
    /// page (including any large page) it partly covers, in a way that relies on it staying mapped,
    /// and no Rust reference into that memory may be live.
    #[expect(clippy::too_many_arguments)]
    pub unsafe fn create_vm_mapping(
        &self,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        mut vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<MapResult, Status> {
        ltracef!("{:p} {:#x} {:#x} {:x}\n", self, mapping_offset, size, vmar_flags);

        // Check that only allowed flags have been set
        if vmar_flags
            & !(flag::SPECIFIC
                | flag::SPECIFIC_OVERWRITE
                | flag::CAN_RWX_FLAGS
                | flag::OFFSET_IS_UPPER_LIMIT
                | flag::DEBUG_DYNAMIC_KERNEL_MAPPING
                | flag::FAULT_BEYOND_STREAM_SIZE)
            != 0
        {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // Validate that arch_mmu_flags does not contain any prohibited flags
        if !self.is_valid_mapping_flags(arch_mmu_flags) {
            hint::cold_path();
            return Err(Status::ACCESS_DENIED);
        }

        if !kernel_page::is_page_rounded(vmo_offset as usize) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        let mapping_size = kernel_page::round_up(size);
        // Make sure that rounding up the page size did not overflow.
        if mapping_size < size {
            hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }
        // Make sure that a mapping of this size wouldn't overflow the vmo offset.
        let Some(vmo_end) = vmo_offset.checked_add(mapping_size as u64) else {
            hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        };

        // For physical VMOs, the mapping must be within the VMO.
        if !vmo.is_paged() {
            hint::cold_path();
            if vmo_end > vmo.size() {
                return Err(Status::OUT_OF_RANGE);
            }
        }

        // Can't create fault-beyond-stream-size mapping of physical or contiguous VMOs. There is
        // currently no use case for this as the stream size of these VMOs is always zero, so the
        // mapping would always fault. In this case, sys_vmar_map should have returned
        // Status::NOT_SUPPORTED.
        debug_assert!(
            vmar_flags & flag::FAULT_BEYOND_STREAM_SIZE == 0
                || (vmo.is_paged() && !vmo.is_contiguous())
        );

        // If we're mapping it with a specific permission, we should allow
        // future protect() calls on the mapping to keep that permission.
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_READ) != 0 {
            vmar_flags |= flag::CAN_MAP_READ;
        }
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
            vmar_flags |= flag::CAN_MAP_WRITE;
        }
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
            vmar_flags |= flag::CAN_MAP_EXECUTE;
        }

        // SAFETY: `mapping_size` and `vmo_offset` are page aligned, and
        // `vmo_offset + mapping_size as u64` does not overflow. If `vmar_flags` contains
        // `flag::SPECIFIC_OVERWRITE`, the caller guarantees that nothing relies on the range, or on
        // the rest of any page it partly covers, staying mapped, and that no Rust reference into
        // it is live; `mapping_size` only extends `size` to the end of its last page.
        let (res, base) = unsafe {
            self.create_sub_vmar_internal(
                mapping_offset,
                mapping_size,
                align_pow2,
                vmar_flags,
                Some(vmo),
                vmo_offset,
                arch_mmu_flags,
                // Only sub-VMARs are named.
                b"",
            )
        }?;
        let mapping = res.into_region_or_mapping().err().expect("res is a mapping");
        Ok(MapResult { mapping, base: base.0 })
    }

    /// Create a new `VmMapping` within this region, overwriting any existing
    /// mappings that are in the way.  If the range crosses a subregion, the call
    /// fails.
    ///
    /// # Safety
    ///
    /// `self` is in `LifeCycleState::Alive`, `size` is not zero, and `base` and `size` are page
    /// aligned. `vmo_offset` is page aligned and `vmo_offset + size as u64` does not overflow.
    /// Nothing may access `[base, base + size)`, or the rest of any large page it partly covers, in
    /// a way that relies on it staying mapped, and no Rust reference into that memory may be live.
    #[cold]
    #[inline(never)]
    #[expect(clippy::too_many_arguments)]
    unsafe fn overwrite_vm_mapping_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        base: VAddr,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<OwnedVmAddressRegionOrMapping, Status> {
        self.canary.assert();

        let Some(vmar) = VmMapping::create(
            self,
            false,
            base.0,
            size,
            vmar_flags,
            vmo,
            vmo_offset,
            arch_mmu_flags,
            Mergeable::No,
        ) else {
            hint::cold_path();
            return Err(Status::NO_MEMORY);
        };

        // SAFETY: `size` is not zero, `base` and `size` are page aligned, nothing relies on
        // `[base, base + size)`, or on the rest of any large page it partly covers, staying mapped,
        // and no Rust reference into that memory is live.
        unsafe {
            self.unmap_internal_locked(
                region_token,
                aspace_token,
                base,
                size,
                false, /* can_destroy_regions */
            )
        }?;

        debug_assert!(vmar.lock_is_held());
        debug_assert!(vmar.region_lock_is_held());
        // SAFETY: `vmar` was just constructed in `LifeCycleState::NotReady` with live parent
        // `self`, and a local `RefPtr` holds an owning reference to it across the call.
        // `unmap_internal_locked` just cleared `[base, base + size)` under the held locks, and
        // `size != 0`, so no child of `self` has base `base`.
        if let Err(status) = unsafe { vmar.activate(region_token, aspace_token) } {
            hint::cold_path();
            // Activation can fail if an allocation needed to happen. Nothing can be done to
            // rollback here so the only option is to propagate the `Status::NO_MEMORY` up. If this
            // is happening in response to a user request then they will most likely panic, as
            // users have no ability to resolve `Status::NO_MEMORY`.
            assert!(status == Status::NO_MEMORY);
            return Err(status);
        }

        // Propagate any memory priority settings. This should only fail if not alive, but we hold
        // the lock and just made it alive, so that cannot happen.
        let priority = self.memory_priority_locked(aspace_token);
        vmar.set_memory_priority_locked::<false>(aspace_token, priority);

        Ok(vmar.into())
    }

    /// Applies the given memory priority to this VMAR, which may or may not result in a change. Up
    /// to the derived type to know how to apply and update the memory priority.
    fn set_memory_priority_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) -> Result<(), Status> {
        if self.state_locked(token) != LifeCycleState::Alive {
            hint::cold_path();
            debug_assert!(self.memory_priority_locked(token) == MemoryPriority::Default);
            return Err(Status::BAD_STATE);
        }

        let set_region_priority =
            |region: &VmAddressRegion, token: &mut LockToken<'_, VmAspaceLockClass>| {
                debug_assert!(region.lock_is_held());
                if priority == region.memory_priority_locked(token) {
                    return;
                }
                region.set_memory_priority_field_locked(token, priority);
                // As a region we only need to inform the `VmAspace` of the change.
                region.aspace().change_high_priority_count_locked(
                    token,
                    if priority == MemoryPriority::High { 1 } else { -1 },
                );
            };

        // Do our own priority change.
        set_region_priority(self, token);

        // Enumerate all of the subregions below us.
        // SAFETY: The aspace lock is held and no region list is modified for the enumerator's
        // lifetime.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<VmarsAndMappings>::new(self, 0, usize::MAX, token)
        };
        // SAFETY: The aspace lock (`token`) remains held and `subregions_` is not modified in this
        // loop, so `next` stays live for the duration of each iteration.
        while let Some((next, _)) = unsafe { enumerator.next_unbound(token) } {
            match next {
                VmAddressRegionOrMapping::Mapping(map) => {
                    map.set_memory_priority_locked::<false>(token, priority);
                }
                VmAddressRegionOrMapping::Region(region) => set_region_priority(region, token),
            }
        }
        Ok(())
    }

    pub(super) fn destroy_locked(
        this: RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        this.canary.assert();
        ltracef!("{:p} '{}'\n", &**this, zr::from_utf8_lossy(this.name().to_bytes()));
        // The statics `KERNEL_ROOT_VMAR` and `KERNEL_HEAP_VMAR` must never be destroyed.
        assert!(!ptr::eq(&*this, KERNEL_ROOT_VMAR.as_ptr()));
        assert!(!ptr::eq(&*this, KERNEL_HEAP_VMAR.as_ptr()));
        if this.state_locked(aspace_token) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        // Remove any applied memory priority.
        let status = this.set_memory_priority_locked(aspace_token, MemoryPriority::Default);
        debug_assert!(status.is_ok());

        debug_assert!(this.lock_is_held());
        debug_assert!(this.region_lock_is_held());
        // The `cur` reference prevents regions from being destructed after dropping
        // the last reference to them when removing from their parent.
        let this_ptr: *const VmAddressRegion = &*this;
        let mut cur: Option<RefPtr<VmAddressRegion>> = Some(this);
        while let Some(current) = cur {
            // Iterate through children destroying mappings. If we find a
            // subregion, stop so we can traverse down.
            let mut child_region: Option<RefPtr<VmAddressRegion>> = None;
            while !region_list::is_empty(&current, aspace_token) && child_region.is_none() {
                // SAFETY: The list of `current` is not empty.
                let child = unsafe { region_list::front(&current, aspace_token) };
                match child {
                    VmAddressRegionOrMapping::Mapping(mapping) => {
                        debug_assert!(child.lock_is_held());
                        debug_assert!(child.region_lock_is_held());
                        // `destroy_locked` should remove this child from our list on success.
                        match VmMapping::destroy_locked(
                            RefPtr::from_ref(mapping),
                            region_token,
                            aspace_token,
                        ) {
                            Ok(()) => {}
                            Err(status) => {
                                // TODO(teisenbe): Do we want to handle this case differently?
                                return Err(status);
                            }
                        }
                    }
                    VmAddressRegionOrMapping::Region(region) => {
                        child.common_canary().assert();
                        child_region = Some(RefPtr::from_ref(region));
                    }
                }
            }

            if child_region.is_some() {
                // If we found a child region, traverse down the tree.
                cur = child_region;
            } else {
                // All children are destroyed, so now destroy the current node.
                let parent = current.parent_locked(aspace_token);
                // SAFETY: `parent_locked` is either `None` or points to a live parent
                // `VmAddressRegion`.
                let parent = parent.map(|parent| unsafe { parent.as_ref() });
                if let Some(parent) = parent {
                    debug_assert!(parent.lock_is_held());
                    debug_assert!(parent.region_lock_is_held());
                    // SAFETY: `current` is a child of `parent` and is kept alive by another owning
                    // reference for the duration of the call.
                    unsafe {
                        region_list::remove_region(parent, region_token, aspace_token, &current)
                    };
                }
                // SAFETY: `current` has no children left and is no longer in the subregions of
                // its parent.
                unsafe { current.mark_dead_locked(region_token, aspace_token) };

                // If we destroyed the original node, stop. Otherwise traverse
                // up the tree and keep destroying.
                cur =
                    if ptr::eq(&*current, this_ptr) { None } else { parent.map(RefPtr::from_ref) };
            }
        }
        Ok(())
    }

    /// Returns the name of this region.
    pub fn name(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.name).expect("name is null-terminated")
    }

    /// Returns true if this region has a parent region.
    pub fn has_parent(&self) -> bool {
        ksync::lock!(let guard = self.lock().lock());
        self.parent_locked(guard.token()).is_some()
    }

    /// Finds the child region that contains the given `addr`. If `addr` is in a gap,
    /// returns `None`. This is a non-recursive search.
    pub fn find_region(&self, addr: VAddr) -> Option<OwnedVmAddressRegionOrMapping> {
        ksync::lock!(let guard = self.lock().lock());
        self.find_region_locked(guard.token(), addr)
    }

    pub fn find_region_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        addr: VAddr,
    ) -> Option<OwnedVmAddressRegionOrMapping> {
        if self.state_locked(token) != LifeCycleState::Alive {
            hint::cold_path();
            return None;
        }
        region_list::find_region(self, token, addr).map(VmAddressRegionOrMapping::to_owned)
    }

    /// Recursively compute the amount of attributed memory within this region.
    pub fn get_attributed_memory(&self) -> AttributionCounts {
        self.canary.assert();

        ksync::lock!(let mut guard = self.lock().lock());
        if !self.is_alive_locked(guard.token()) {
            return AttributionCounts::default();
        }

        let mut page_counts = AttributionCounts::default();

        // Enumerate all of the subregions below us & count allocated pages.
        // SAFETY: The aspace lock is held, and no region list is modified, except while the
        // enumerator is paused.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<MappingsOnly>::new(self, 0, usize::MAX, guard.token())
        };
        while let Some((next, _)) = enumerator.next(guard.token()) {
            let map = RefPtr::from_ref(next);
            enumerator.pause(guard.token());
            page_counts += map.get_attributed_memory_locked(guard.as_mut());
            drop(map);
            // SAFETY: The aspace lock is held, and no region list is modified, until the
            // enumerator is paused again or dropped.
            unsafe { enumerator.resume(guard.token()) };
            if !self.is_alive_locked(guard.token()) {
                break;
            }
        }

        page_counts
    }

    /// Recursively traverses the regions for a given virtual address and returns a reference to a
    /// mapping if one is found. The returned reference is only valid as long as the aspace lock
    /// remains held.
    pub fn find_mapping_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmAspaceLockClass>,
        va: VAddr,
    ) -> Option<&'a VmMapping> {
        self.canary.assert();

        let mut vmar = self;
        debug_assert!(vmar.lock_is_held());
        loop {
            let next = region_list::find_region(vmar, token, va)?;
            next.common_canary().assert();
            vmar = match next {
                VmAddressRegionOrMapping::Mapping(mapping) => return Some(mapping),
                VmAddressRegionOrMapping::Region(region) => region,
            };
        }
    }

    /// If the allocation between the given children can be met this returns a virtual address of
    /// the base address of that allocation, otherwise a `None` is returned.
    #[expect(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn check_gap_locked_region(
        &self,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        prev: Option<VmAddressRegionOrMapping<'_>>,
        next: Option<VmAddressRegionOrMapping<'_>>,
        search_base: VAddr,
        align: usize,
        region_size: usize,
        min_gap: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Option<VAddr> {
        let search_base = search_base.0;

        // compute the starting address of the gap
        // first byte of a gap
        let mut gap_beg = if let Some(prev) = prev {
            prev.base().0.checked_add(prev.size())?.checked_add(min_gap)?
        } else {
            self.base().0
        };

        // compute the ending address of the gap
        // last byte of a gap
        let gap_end = if let Some(next) = next {
            if gap_beg == next.base().0 {
                hint::cold_path();
                return None; // no gap between regions
            }
            next.base().0.checked_sub(1)?.checked_sub(min_gap)?
        } else {
            if gap_beg.wrapping_sub(self.base().0) == self.size() {
                hint::cold_path();
                return None; // no gap at the end of address space.
            }
            self.base().0.checked_add(self.size() - 1)?
        };

        debug_assert!(gap_end > gap_beg);

        // trim it to the search range
        if gap_end <= search_base {
            hint::cold_path();
            return None;
        }
        if gap_beg < search_base {
            gap_beg = search_base;
        }

        debug_assert!(gap_end > gap_beg);

        ltracef_level!(
            2,
            "search base {:#x} gap_beg {:#x} end {:#x}\n",
            search_base,
            gap_beg,
            gap_end
        );

        let va = self.aspace().arch_aspace().pick_spot(
            gap_beg,
            gap_end,
            align,
            region_size,
            arch_mmu_flags,
        );

        if va < gap_beg {
            hint::cold_path();
            return None; // address wrapped around
        }

        if va >= gap_end || (gap_end - va).wrapping_add(1) < region_size {
            hint::cold_path();
            return None; // not enough room
        }

        Some(VAddr(va))
    }

    /// search for a spot to allocate for a region of a given size
    ///
    /// Perform allocations for VMARs. This allocator works by choosing uniformly at random from a
    /// set of positions that could satisfy the allocation. The set of positions are the 'left'
    /// most positions of the address space and are capped by the address entropy limit. The
    /// entropy limit is retrieved from the address space, and can vary based on whether the user
    /// has requested compact allocations or not.
    pub(crate) fn alloc_spot_locked_region(
        &self,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        size: usize,
        mut align_pow2: u8,
        arch_mmu_flags: ArchMmuFlags,
        upper_limit: VAddr,
    ) -> Result<VAddr, Status> {
        ltracef!(
            "size={} align_pow2={} arch_mmu_flags={:x} upper_limit={:x}\n",
            size,
            align_pow2,
            arch_mmu_flags,
            upper_limit.0
        );
        self.canary.assert();
        debug_assert!(size > 0 && kernel_page::is_page_rounded(size));

        ltracef_level!(
            2,
            "aspace {:p} size {:#x} align {} upper_limit {:#x}\n",
            self,
            size,
            align_pow2,
            upper_limit.0
        );

        align_pow2 = align_pow2.max(kernel_page::SHIFT as u8);
        let entropy = self.aspace().aslr_entropy_bits(self.flags() & flag::COMPACT != 0);
        let prng = self.aspace().is_aslr_enabled().then(|| self.aspace().aslr_prng());

        let alloc_spot = region_list::get_alloc_spot(
            self,
            region_token,
            align_pow2,
            entropy,
            size,
            self.base(),
            self.size(),
            prng,
            upper_limit,
        )?;

        // Sanity check that the allocation fits.
        let (alloc_last_byte, overflowed) = alloc_spot.0.overflowing_add(size - 1);
        assert!(!overflowed);
        let (before, after) =
            region_list::upper_bound_neighbours(self, region_token, VAddr(alloc_last_byte));

        let align = 1usize << align_pow2;
        Ok(self
            .check_gap_locked_region(
                region_token,
                before,
                after,
                alloc_spot,
                align,
                size,
                0,
                arch_mmu_flags,
            )
            .expect("Unexpected allocation failure"))
    }

    /// Transition from `NotReady` to `Alive`, and add references to `self` to related
    /// structures.
    /// On error no state is changed and no references are added.
    ///
    /// # Safety
    ///
    /// `self` must be in `LifeCycleState::NotReady` with a live parent `VmAddressRegion`, no child
    /// of the parent has the same base as `self`, and `self` is kept alive by another owning
    /// reference for the duration of the call.
    pub(super) unsafe fn activate(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        debug_assert!(self.state_locked(aspace_token) == LifeCycleState::NotReady);

        let parent_ptr = self.parent_locked(aspace_token).unwrap();
        // SAFETY: `parent_ptr` is valid for reads.
        let parent = unsafe { parent_ptr.as_ref() };
        debug_assert!(parent.lock_is_held());
        debug_assert!(parent.region_lock_is_held());

        // Validate we are a correct child of our parent.
        debug_assert!(parent.is_in_range(self.base(), self.size()));

        // Look for a region in the parent starting from our desired base. If any region is found,
        // make sure we do not intersect with it.
        if let Some(candidate) =
            region_list::include_or_higher(parent, aspace_token, self.base()).get()
        {
            debug_assert!(candidate.lock_is_held());
            if self.base().0.checked_add(self.size()).is_none_or(|end| candidate.base().0 < end) {
                #[cold]
                #[inline(never)]
                fn panic_overlapping_region(addr: usize) -> ! {
                    panic!("overlapping region at {:#x}", addr);
                }
                panic_overlapping_region(candidate.base().0);
            }
        }

        // SAFETY: `self` is kept alive by another owning reference for the duration of the call,
        // and no child of `parent` has the same base as `self`.
        unsafe {
            region_list::insert_region(
                parent,
                region_token,
                aspace_token,
                self.as_region_or_mapping(),
            )?
        };
        // SAFETY: `self` is in `LifeCycleState::NotReady` and was inserted into the subregions of
        // its live parent above.
        unsafe { self.set_alive_locked(region_token, aspace_token) };
        Ok(())
    }

    /// Dump debug info.
    pub fn dump_locked(
        &self,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        token: &LockToken<'_, VmAspaceLockClass>,
        depth: u32,
        verbose: bool,
    ) {
        self.canary.assert();
        debug_assert!(self.lock_is_held());
        debug_assert!(self.region_lock_is_held());
        for _ in 0..depth {
            kprint!("  ");
        }
        let subregions_size = region_list::size_slow(self, token);
        kprint!(
            "vmar {:p} [{:#x} {:#x}] sz {:#x} ref {} state {} '{:cs}' subregions {}\n",
            self as *const _,
            self.base().0,
            self.base().0 + (self.size() - 1),
            self.size(),
            self.ref_count().ref_count_debug(),
            self.state_locked(token) as i32,
            self.name().as_ptr(),
            subregions_size,
        );

        region_list::for_each(self, region_token, token, move |child| {
            debug_assert!(child.lock_is_held());
            debug_assert!(child.region_lock_is_held());
            child.dump_locked(region_token, token, depth + 1, verbose);
        });
    }

    fn commit_high_memory_priority(&self) {
        self.canary.assert();

        ksync::lock!(let mut guard = self.lock().lock());
        // Capture the validation that we need to do whenever the lock is acquired.
        let validate = |token: &LockToken<'_, VmAspaceLockClass>| -> bool {
            if self.state_locked(token) != LifeCycleState::Alive {
                hint::cold_path();
                return false;
            }

            if self.memory_priority_locked(token) != MemoryPriority::High {
                hint::cold_path();
                return false;
            }

            true
        };
        if !validate(guard.token()) {
            return;
        }

        // SAFETY: The aspace lock is held, and no region list is modified, except while the
        // enumerator is paused.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<MappingsOnly>::new(self, 0, usize::MAX, guard.token())
        };
        while let Some((region_or_mapping, _)) = enumerator.next(guard.token()) {
            // Presently we hold the lock, so we know that `region_or_mapping` is valid, but we want
            // to use this outside of the lock later on, and so we must upgrade it to a `RefPtr`.
            let mapping = RefPtr::from_ref(region_or_mapping);
            enumerator.pause(guard.token());
            guard.as_mut().call_unlocked(move || {
                mapping.commit_high_memory_priority();
                drop(mapping);
            });
            // Since the lock was dropped we must re-validate before doing anything else.
            if !validate(guard.token()) {
                return;
            }
            // SAFETY: The aspace lock is held, and no region list is modified, until the enumerator
            // is paused again or dropped.
            unsafe { enumerator.resume(guard.token()) };
        }
    }

    /// Applies the given memory priority to this region and all subregions.
    pub fn set_memory_priority(&self, priority: MemoryPriority) -> Result<(), Status> {
        self.canary.assert();
        let have_children = {
            ksync::lock!(let mut guard = self.lock().lock());
            if let Err(status) =
                self.set_memory_priority_locked(guard.as_mut().token_mut(), priority)
            {
                hint::cold_path();
                return Err(status);
            }
            !region_list::is_empty(self, guard.token())
        };
        // If a high memory priority was set, perform another pass through any mappings to commit
        // it, unless we know we didn't have any children at the point we set the priority to avoid
        // a needless lock acquisition and pass.
        if priority == MemoryPriority::High && have_children {
            self.commit_high_memory_priority();
        }
        Ok(())
    }

    /// Unmap a subset of the region of memory in the containing address space,
    /// returning it to this region to allocate. If a subregion is entirely in
    /// the range, and `op_children` is `Yes`, that subregion is destroyed. If a subregion is
    /// partially in the range, `unmap()` will fail.
    ///
    /// # Safety
    ///
    /// Nothing may access `[base, base + size)`, or the rest of any page (including any large
    /// page) it partly covers, in a way that relies on it staying mapped, and no Rust reference
    /// into that memory may be live.
    pub unsafe fn unmap(
        &self,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        self.canary.assert();

        let size = size.wrapping_add(kernel_page::MASK) & !kernel_page::MASK;
        if size == 0 || !kernel_page::is_page_rounded(base.0) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        ksync::lock!(let mut region_guard = self.region_lock().lock());
        ksync::lock!(let mut guard = self.lock().lock());
        if self.state_locked(guard.token()) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        // SAFETY: `size` is not zero, `base` and `size` are page aligned, nothing relies on
        // `[base, base + size)`, or on the rest of any large page it partly covers, staying
        // mapped, and no Rust reference into that memory is live.
        unsafe {
            self.unmap_internal_locked(
                region_guard.as_mut().token_mut(),
                guard.as_mut().token_mut(),
                base,
                size,
                op_children == VmAddressRegionOpChildren::Yes, /* can_destroy_regions */
            )
        }
    }

    /// Implementation for `unmap()` and `overwrite_vm_mapping_locked()` that does not acquire
    /// the region and aspace locks. If `can_destroy_regions` is true, then this may destroy
    /// VMARs that it completely covers.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, nothing may access
    /// `[base, base + size)`, or the rest of any large page it partly covers, in a way that relies
    /// on it staying mapped, and no Rust reference into that memory may be live.
    unsafe fn unmap_internal_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        base: VAddr,
        size: usize,
        can_destroy_regions: bool,
    ) -> Result<(), Status> {
        if !self.is_in_range(base, size) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        if region_list::is_empty(self, aspace_token) {
            return Ok(());
        }

        // Any unmap spanning the vDSO code mapping is verboten.
        if self.aspace().intersects_vdso_code_locked(aspace_token, base, size) {
            hint::cold_path();
            return Err(Status::ACCESS_DENIED);
        }

        debug_assert!(size > 0);
        let (end_addr_byte, overflowed) = base.0.overflowing_add(size - 1);
        assert!(!overflowed);

        // Check if we're partially spanning a subregion, or aren't allowed to
        // destroy regions and are spanning a region, and bail if we are.
        let mut itr = region_list::include_or_higher(self, aspace_token, base);
        while let Some(curr) = itr.get() {
            if curr.base().0 > end_addr_byte {
                break;
            }
            debug_assert!(curr.lock_is_held());
            debug_assert!(curr.size() > 0);
            let (itr_end_byte, overflowed) = curr.base().0.overflowing_add(curr.size() - 1);
            assert!(!overflowed);
            if !curr.is_mapping()
                && (!can_destroy_regions || curr.base().0 < base.0 || itr_end_byte > end_addr_byte)
            {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }
            itr.next();
        }

        // TODO(https://fxbug.dev/503042881): Every deletion invalidates our iterator, hence we have
        // to look up a new one every time.
        while let Some(curr_region) = region_list::include_or_higher(self, aspace_token, base).get()
        {
            if curr_region.base().0 > end_addr_byte {
                break;
            }

            debug_assert!(curr_region.lock_is_held());
            debug_assert!(curr_region.region_lock_is_held());

            assert!(curr_region.size() > 0);
            let (curr_end_byte, overflowed) =
                curr_region.base().0.overflowing_add(curr_region.size() - 1);
            assert!(!overflowed);
            let unmap_base = curr_region.base().0.max(base.0);
            let unmap_end_byte = curr_end_byte.min(end_addr_byte);
            assert!(unmap_base <= unmap_end_byte);
            let (unmap_size, overflowed) = (unmap_end_byte - unmap_base).overflowing_add(1);
            assert!(!overflowed);
            if unmap_base == curr_region.base().0 && unmap_size == curr_region.size() {
                if let Err(status) =
                    curr_region.to_owned().destroy_locked(region_token, aspace_token)
                {
                    hint::cold_path();
                    debug_assert!(status == Status::NO_MEMORY);
                    return Err(status);
                }
            } else {
                let mapping = curr_region.as_vm_mapping();
                // Already validated in the loop above that the only kind of region we can partially
                // span is a mapping.
                assert!(mapping.is_some());
                let mapping = mapping.unwrap();
                // Unmapping can fail if an allocation needed to happen. Nothing can be done to
                // rollback here so the only option is to propagate the `Status::NO_MEMORY` up. If
                // this is happening in response to a user request then they will most likely panic,
                // as users have no ability to resolve `Status::NO_MEMORY`.
                // SAFETY: `unmap_size` is not zero, `unmap_base` and `unmap_size` are page
                // aligned, `[unmap_base, unmap_base + unmap_size)` is a strict subrange of
                // `mapping`, and nothing relies on that range, or on the rest of any large page it
                // partly covers, staying mapped, and no Rust reference into that memory is live.
                if let Err(status) = unsafe {
                    VmMapping::unmap_locked(
                        &mapping,
                        region_token,
                        aspace_token,
                        unmap_base,
                        unmap_size,
                    )
                } {
                    hint::cold_path();
                    assert!(status == Status::NO_MEMORY);
                    return Err(status);
                }
            }
        }

        Ok(())
    }

    /// Change protections on a subset of the region of memory in the containing
    /// address space. If the requested range overlaps with a subregion and `op_children` is `No`,
    /// `protect()` will fail, otherwise the mapping permissions in the subregion may only be
    /// reduced.
    ///
    /// # Safety
    ///
    /// The virtual address range `[base, base + size)`, and the rest of any page it partly covers,
    /// may have its permissions changed. Nothing may access that memory, or the rest of any large
    /// page it partly covers, in a way that relies on it staying mapped, and no Rust reference
    /// into it may be live.
    pub unsafe fn protect(
        &self,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        self.canary.assert();

        let size = size.wrapping_add(kernel_page::MASK) & !kernel_page::MASK;
        if size == 0 || !kernel_page::is_page_rounded(base.0) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        ksync::lock!(let mut guard = self.lock().lock());
        let token = guard.as_mut().token_mut();
        if self.state_locked(token) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        if !self.is_in_range(base, size) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // Do not allow changing caching.
        if (new_arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != 0 {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // The last byte of the range.
        let (end_addr_byte, overflowed) = base.0.overflowing_add(size - 1);
        assert!(!overflowed);

        // Check part of the range is not mapped, or the new permissions are invalid for some
        // mapping in the range.
        {
            let token = &*token;
            // SAFETY: The aspace lock is held and no region list is modified for the enumerator's
            // lifetime.
            let mut enumerator = unsafe {
                VmAddressRegionEnumerator::<MappingsOnly>::new(self, base.0, end_addr_byte, token)
            };
            let mut expected = base.0;
            while let Some((mapping, _)) = enumerator.next(token) {
                if mapping.base().0 > expected {
                    hint::cold_path();
                    return Err(Status::NOT_FOUND);
                }
                let (end, overflowed) = mapping.base().0.overflowing_add(mapping.size());
                assert!(!overflowed);
                if !mapping.is_valid_mapping_flags(new_arch_mmu_flags) {
                    hint::cold_path();
                    return Err(Status::ACCESS_DENIED);
                }
                if self
                    .aspace()
                    .vdso_code_mapping_locked(token)
                    .is_some_and(|m| ptr::eq(m, mapping))
                {
                    hint::cold_path();
                    return Err(Status::ACCESS_DENIED);
                }
                if mapping.parent_locked(token) != Some(NonNull::from(self)) {
                    if op_children == VmAddressRegionOpChildren::No {
                        hint::cold_path();
                        return Err(Status::INVALID_ARGS);
                    }
                    // As this is a subregion we cannot increase its mapping flags, even if they
                    // might otherwise be permissible. A mapping might have multiple different
                    // protect regions so need to check all of them within the protection range.
                    // Already know that `expected` is within the mapping, calculate a length that
                    // is within the range of `mapping`.
                    let len = end_addr_byte.min(end - 1) - expected + 1;
                    mapping.enumerate_protection_ranges_locked(
                        token,
                        expected,
                        len,
                        move |_, _, flags| {
                            if (flags & new_arch_mmu_flags) != new_arch_mmu_flags {
                                hint::cold_path();
                                return Err(Status::ACCESS_DENIED);
                            }
                            Err(Status::NEXT)
                        },
                    )?;
                }
                expected = end;
            }
            if expected < end_addr_byte {
                hint::cold_path();
                return Err(Status::NOT_FOUND);
            }
        }

        // SAFETY: The aspace lock is held, and no region list is modified, except while the
        // enumerator is paused.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<MappingsOnly>::new(self, base.0, end_addr_byte, token)
        };
        // SAFETY: The aspace lock (`token`) remains held and `subregions_` is not modified in this
        // loop, so `mapping` stays live for the duration of each iteration.
        while let Some((mapping, _)) = unsafe { enumerator.next_unbound(token) } {
            // The last byte of the current region.
            let (curr_end_byte, overflowed) = mapping.base().0.overflowing_add(mapping.size() - 1);
            assert!(!overflowed);
            let protect_base = mapping.base().0.max(base.0);
            let protect_end_byte = curr_end_byte.min(end_addr_byte);
            let (protect_size, overflowed) = (protect_end_byte - protect_base).overflowing_add(1);
            assert!(!overflowed);

            // `protect_locked` mutates the mapping's protection ranges in place, and so we must
            // pause the enumerator to safely perform mutations. Note that even though we are
            // pausing the enumerator here, it is *NOT* okay to drop the lock between the pause and
            // resume. We need to mutate permissions on all the mappings in the requested range
            // atomically (except for failure due to `Status::NO_MEMORY`) and so the lock must be
            // held throughout.
            enumerator.pause(token);
            // SAFETY: `protect_size` is not zero, `protect_base` and `protect_size` are page
            // aligned, `[protect_base, protect_base + protect_size)` is within `mapping` and may
            // have its permissions changed, and `new_arch_mmu_flags` has no cache flags and is
            // valid for `mapping`. Nothing relies on that range, or on the rest of any large page
            // it partly covers, staying mapped, and no Rust reference into that memory is live.
            if let Err(status) = unsafe {
                mapping.protect_locked(token, protect_base, protect_size, new_arch_mmu_flags)
            } {
                hint::cold_path();
                // We can error out only due to failed allocations. Other error conditions should
                // have already been checked above.
                assert!(status == Status::NO_MEMORY);
                // TODO(teisenbe): Try to work out a way to guarantee success, or
                // provide a full unwind?
                return Err(status);
            }
            // SAFETY: The aspace lock is held, and no region list is modified, until the enumerator
            // is paused again or dropped.
            unsafe { enumerator.resume(token) };
        }

        Ok(())
    }

    /// Apply `op` to VMO mappings in the specified range of pages.
    pub fn range_op(
        &self,
        op: RangeOpType,
        base: VAddr,
        len: usize,
        op_children: VmAddressRegionOpChildren,
        buffer: UserInOutPtr<u8>,
        buffer_size: usize,
    ) -> Result<(), Status> {
        self.canary.assert();
        if !buffer.is_null() || buffer_size != 0 {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }
        let len = len.wrapping_add(kernel_page::MASK) & !kernel_page::MASK;
        if len == 0 || !kernel_page::is_page_rounded(base.0) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        if !self.is_in_range(base, len) {
            hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        let (last_addr, overflowed) = base.0.overflowing_add(len);
        assert!(!overflowed);

        ksync::lock!(let mut guard = self.lock().lock());
        // Capture the validation that we need to do whenever the lock is acquired.
        let validate = |token: &LockToken<'_, VmAspaceLockClass>| -> Result<(), Status> {
            if self.state_locked(token) != LifeCycleState::Alive {
                hint::cold_path();
                return Err(Status::BAD_STATE);
            }

            // Don't allow any operations on the vDSO code mapping.
            if self.aspace().intersects_vdso_code_locked(token, base, len) {
                hint::cold_path();
                return Err(Status::ACCESS_DENIED);
            }
            Ok(())
        };
        validate(guard.token())?;

        // SAFETY: The aspace lock is held, and no region list is modified, except while the
        // enumerator is paused.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<MappingsOnly>::new(self, base.0, last_addr, guard.token())
        };
        let mut expected = base.0;
        while let Some((region_or_mapping, _)) = enumerator.next(guard.token()) {
            // Presently we hold the lock, so we know that `region_or_mapping` is valid, but we want
            // to use this outside of the lock later on, and so we must upgrade it to a `RefPtr`.
            let mapping = RefPtr::from_ref(region_or_mapping);

            // It's possible the mapping's base is less than `expected` if the first mapping is not
            // precisely aligned to the start of our range. After that its base should always be
            // `expected`, and if it's greater then there is a gap and this is considered an error.
            if mapping.base().0 > expected {
                hint::cold_path();
                return Err(Status::BAD_STATE);
            }
            // We should only have been called if we were at least partially in range.
            debug_assert!(mapping.is_in_range(VAddr(expected), 1));
            let mapping_offset = expected - mapping.base().0;
            let vmo_offset = mapping.object_offset() + mapping_offset as u64;

            // Should only have been called for a non-zero range.
            debug_assert!(last_addr > expected);

            let total_remain = last_addr - expected;
            debug_assert!(mapping.size() > mapping_offset);
            let max_in_mapping = mapping.size() - mapping_offset;

            let size = total_remain.min(max_in_mapping);

            let vmo = mapping.vmo_locked(guard.token());

            enumerator.pause(guard.token());
            // The commit, decommit and prefetch ops check the maximal permissions of the mapping
            // and can be thought of as acting as if they perform a protect to add read or write
            // permissions. Since protect to add permissions through a parent VMAR is not valid we
            // similarly forbid this notional protect by not allowing these operations if acting
            // through a sub-VMAR, regardless of whether `op_children` is otherwise allowed.
            if (matches!(op, RangeOpType::Commit | RangeOpType::Decommit | RangeOpType::Prefetch)
                || op_children == VmAddressRegionOpChildren::No)
                && mapping.parent_locked(guard.token()) != Some(NonNull::from(self))
            {
                hint::cold_path();
                return Err(Status::INVALID_ARGS);
            }

            let result = guard.as_mut().call_unlocked(move || {
                let result = Self::range_op_on_mapping(
                    op,
                    &mapping,
                    vmo.as_deref().expect("mapping must have a VMO"),
                    mapping_offset,
                    vmo_offset,
                    size,
                );
                // Drop our references while the lock is released: the mapping may have been
                // destroyed and the VMO closed meanwhile, leaving these as the last references.
                drop(vmo);
                drop(mapping);
                result
            });
            // Since the lock was dropped we must re-validate before doing anything else.
            validate(guard.token())?;
            // SAFETY: The aspace lock is held, and no region list is modified, until the enumerator
            // is paused again or dropped.
            unsafe { enumerator.resume(guard.token()) };

            result?;
            expected += size;
        }

        // Check if there was a gap right at the end of the range.
        if expected < last_addr {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        Ok(())
    }

    /// Applies `op` to `size` bytes of `mapping` from `mapping_offset`, which maps `vmo` from
    /// `vmo_offset`. This is a separate function so that `range_op` can move its references to
    /// the mapping and VMO into the unlocked call and drop them there, not under the aspace lock.
    ///
    /// The aspace lock must not be held.
    fn range_op_on_mapping(
        op: RangeOpType,
        mapping: &VmMapping,
        vmo: &VmObject,
        mapping_offset: usize,
        vmo_offset: u64,
        size: usize,
    ) -> Result<(), Status> {
        debug_assert!(!mapping.lock_is_held());
        // For fault-beyond-stream-size mappings, ensure there are no gaps due to the stream
        // size being less than the end of the mapping. User synchronization is required
        // for the observable result to be defined, as the stream size is a user managed
        // property & not guaranteed atomic to the VMO.
        if mapping.flags() & flag::FAULT_BEYOND_STREAM_SIZE != 0 {
            let paged = vmo.as_paged().expect("vmo must be paged");
            {
                ksync::lock!(let vmo_guard = paged.lock_ref().lock());

                let stream_size = paged
                    .saturating_stream_size_locked(vmo_guard.token())
                    .expect("stream_size must be set");
                if vmo_offset > stream_size {
                    hint::cold_path();
                    return Err(Status::OUT_OF_RANGE);
                }
                if size as u64 > stream_size - vmo_offset {
                    hint::cold_path();
                    return Err(Status::OUT_OF_RANGE);
                }
            }
        }
        match op {
            RangeOpType::Commit => {
                if !mapping.is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_WRITE) {
                    hint::cold_path();
                    Err(Status::ACCESS_DENIED)
                } else {
                    vmo.commit_range(vmo_offset, size as u64)?;
                    mapping.map_range(
                        mapping_offset,
                        size,
                        /*commit=*/ false,
                        /*ignore_existing=*/ true,
                    )
                }
            }
            RangeOpType::Decommit => {
                // Decommit zeroes pages of the VMO, equivalent to writing to it.
                // Check that the mapping is currently writable, or could be made
                // writable.
                if !mapping.is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_WRITE) {
                    hint::cold_path();
                    Err(Status::ACCESS_DENIED)
                } else {
                    vmo.decommit_range(vmo_offset, size as u64)
                }
            }
            RangeOpType::MapRange => mapping.map_range(
                mapping_offset,
                size,
                /*commit=*/ false,
                /*ignore_existing=*/ true,
            ),
            RangeOpType::Zero => {
                if !mapping.is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_WRITE) {
                    hint::cold_path();
                    Err(Status::ACCESS_DENIED)
                } else {
                    vmo.zero_range(vmo_offset, size as u64)
                }
            }
            RangeOpType::AlwaysNeed => {
                vmo.hint_range(vmo_offset, size as u64, EvictionHint::AlwaysNeed)?;
                mapping.map_range(
                    mapping_offset,
                    size,
                    /*commit=*/ false,
                    /*ignore_existing=*/ true,
                )
            }
            RangeOpType::DontNeed => {
                vmo.hint_range(vmo_offset, size as u64, EvictionHint::DontNeed)
            }
            RangeOpType::Prefetch => {
                if !mapping.is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_READ) {
                    hint::cold_path();
                    Err(Status::ACCESS_DENIED)
                } else {
                    vmo.prefetch_range(vmo_offset, size as u64)?;
                    mapping.map_range(
                        mapping_offset,
                        size,
                        /*commit=*/ false,
                        /*ignore_existing=*/ true,
                    )
                }
            }
        }
    }

    /// Reserves a memory region within this VMAR. The region is already mapped in the page table
    /// with `arch_mmu_flags`. The VMAR creates a VmMapping for the region even though no physical
    /// pages need to be allocated for it.
    ///
    /// # Safety
    ///
    /// `self` is in the kernel aspace, only the current CPU is online, and `arch_mmu_flags` must
    /// be valid kernel MMU flags for `[base, base + size)`. Nothing may access
    /// `[base, base + size)` in a way that `arch_mmu_flags` does not permit; no Rust reference
    /// into `[base, base + size)` may be live if `arch_mmu_flags` grants no permissions, and no
    /// mutable Rust reference into it may be live if `arch_mmu_flags` is not writable. If
    /// splitting a partly covered large page may perform break-before-make, nothing may access
    /// the rest of that large page in a way that relies on it staying mapped, and no Rust
    /// reference into the rest of that large page may be live.
    pub unsafe fn reserve_space(
        &self,
        name: &CStr,
        base: VAddr,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        self.canary.assert();
        assert!(kernel_page::is_page_rounded(base.0) && kernel_page::is_page_rounded(size));
        if !self.is_in_range(base, size) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }
        let offset = base.0 - self.base().0;
        // We need a zero-length VMO to pass into create_vm_mapping so that a VmMapping would be
        // created. The VmMapping is already mapped to physical pages in start.S.
        // We would never call map_range on the VmMapping, thus the VMO would never actually
        // allocate any physical pages and we would never modify the PTE except for the permission
        // change below caused by protect.
        let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, 0)?;
        let name = name.to_bytes();
        let _ = vmo.set_name(name);

        // Set the cache policy on the VMO to match arch_mmu_flags to squelch a warning elsewhere
        // when the mapping is created.
        let cache_policy = match arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK {
            ARCH_MMU_FLAG_UNCACHED => Some(ZX_CACHE_POLICY_UNCACHED),
            ARCH_MMU_FLAG_UNCACHED_DEVICE => Some(ZX_CACHE_POLICY_UNCACHED_DEVICE),
            ARCH_MMU_FLAG_WRITE_COMBINING => Some(ZX_CACHE_POLICY_WRITE_COMBINING),
            ARCH_MMU_FLAG_CACHED => None, // nop
            _ => panic!("unhandled arch_mmu_flags {:#x}", arch_mmu_flags),
        };
        if let Some(cache_policy) = cache_policy {
            // SAFETY: `vmo` is a newly created zero-length, non-resizable `VmObjectPaged` that
            // never owns physical pages, and any mapping of `vmo` enforces `cache_policy`.
            let _ = unsafe { vmo.set_mapping_cache_policy(cache_policy as ArchMmuFlags) };
        }

        // allocate a region and put it in the aspace list.
        // Need to set flag::DEBUG_DYNAMIC_KERNEL_MAPPING since we are 'cheating' with this
        // fake zero-length VMO and so the checks that the pages in that VMO are pinned would
        // otherwise fail.
        let _r = self.create_vm_mapping_no_overwrite(
            offset,
            size,
            0,
            flag::SPECIFIC | flag::DEBUG_DYNAMIC_KERNEL_MAPPING,
            VmObjectPaged::into_vm_object(vmo),
            0,
            arch_mmu_flags,
        )?;
        // Directly invoke a protect on the hardware aspace to modify the protection of the existing
        // mappings. If the desired protection flags is "no permissions" then we need to use unmap
        // instead of protect since a mapping with no permissions is not valid on most
        // architectures.
        let arch_aspace = self.aspace().arch_aspace();
        if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) == 0 {
            // SAFETY: The kernel aspace is initialized and not unified, `[base, base + size)` is
            // within it, nothing relies on the range staying mapped, and no Rust reference into it
            // is live. If splitting a partly covered large page performs break-before-make, nothing
            // relies on the rest of that large page staying mapped and no Rust reference into it
            // is live.
            unsafe { arch_aspace.unmap(base.0, size / kernel_page::SIZE, ARCH_UNMAP_OPTION_NONE) }
        } else {
            // This method should only be called during early system init prior to the bringup of
            // other CPUs. In this case it is safe to allow the protect operations to temporarily
            // enlarge.
            if cfg!(debug_assertions) {
                let online = get_online_mask();
                let curr = curr_cpu_num();
                debug_assert!(
                    (online & !cpu_num_to_mask(curr)) == 0,
                    "Online mask {} has more than current cpu {}",
                    online,
                    curr
                );
            }
            // SAFETY: The kernel aspace is initialized and not unified, and `arch_mmu_flags` are
            // valid kernel MMU flags for `[base, base + size)`. Only the current CPU is online,
            // and if splitting a partly covered large page performs break-before-make, nothing
            // relies on the rest of that large page staying mapped and no Rust reference into it
            // is live.
            unsafe {
                arch_aspace.protect(
                    base.0,
                    size / kernel_page::SIZE,
                    arch_mmu_flags,
                    ARCH_UNMAP_OPTION_ENLARGE,
                )
            }
        }
    }

    /// Traverses this vmar (and any sub-vmars) starting at this node, in depth-first pre-order. See
    /// `VmEnumerator` for more details. If this vmar is not alive (in the `LifeCycleState` sense)
    /// or otherwise not enumerable this returns `Err(Status::BAD_STATE)`, otherwise the result of
    /// enumeration is returned.
    pub fn enumerate_children<E: VmEnumerator + ?Sized>(&self, ve: &mut E) -> Result<(), Status> {
        self.canary.assert();
        ksync::lock!(let mut guard = self.lock().lock());
        if self.state_locked(guard.token()) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        match ve.on_vm_address_region(self, 0, guard.as_mut()) {
            Status::NEXT => {}
            Status::STOP => return Ok(()),
            status => return Err(status),
        }
        // SAFETY: The aspace lock is held, and no region list is modified, except while the
        // enumerator is paused.
        let mut enumerator = unsafe {
            VmAddressRegionEnumerator::<VmarsAndMappings>::new(self, 0, usize::MAX, guard.token())
        };
        while let Some((region_or_mapping, depth)) = enumerator.next(guard.token()) {
            let region_or_mapping = region_or_mapping.to_owned();
            enumerator.pause(guard.token());
            let status = match region_or_mapping.as_ref() {
                VmAddressRegionOrMapping::Mapping(mapping) => {
                    ve.on_vm_mapping(mapping, self, depth, guard.as_mut())
                }
                VmAddressRegionOrMapping::Region(vmar) => {
                    ve.on_vm_address_region(vmar, depth, guard.as_mut())
                }
            };
            drop(region_or_mapping);
            match status {
                Status::NEXT => {}
                Status::STOP => return Ok(()),
                status => return Err(status),
            }
            // SAFETY: The aspace lock is held, and no region list is modified, until the enumerator
            // is paused again or dropped.
            unsafe { enumerator.resume(guard.token()) };
        }
        Ok(())
    }
}

/// Guard passed to [`VmEnumerator`] callbacks representing the held root `VmAspace` lock.
pub type VmAspaceGuard<'a> = KMutexGuard<'a, VmAspaceLockClass, TrackedCriticalMutex>;

/// Interface for walking a `VmAspace`-rooted `VmAddressRegion`/`VmMapping` tree.
/// Implement this trait and pass an instance to `VmAddressRegion::enumerate_children()`.
/// `VmAddressRegion::enumerate_children()` will call the `on_*` methods in depth-first pre-order.
/// `Status::NEXT` and `Status::STOP` can be used to control iteration, with any other
/// status becoming the return value of this method. The root `VmAspace`'s lock is held during the
/// traversal and passed in to the callbacks as `guard`. A callback is permitted to temporarily drop
/// the lock, using `call_unlocked`; `enumerate_children` holds a `RefPtr` across each callback so
/// the passed references remain valid, with the caveat that after dropping the lock they might
/// refer to a dead, aka unmapped, object.
pub trait VmEnumerator {
    /// `depth` will be 0 for the root `VmAddressRegion`.
    fn on_vm_address_region(
        &mut self,
        _vmar: &VmAddressRegion,
        _depth: u32,
        _guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        Status::NEXT
    }

    /// `vmar` is the parent of `map`.
    fn on_vm_mapping(
        &mut self,
        _map: &VmMapping,
        _vmar: &VmAddressRegion,
        _depth: u32,
        _guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        Status::NEXT
    }
}

/// A list of regions ordered by virtual address.
///
/// TODO(https://fxbug.dev/503042881): The RegionList API is quite object reference focused, instead
/// of iterator focused, and this leads to a lot of theoretically redundant 'find' operations on the
/// btree. These APIs, and the VMAR logic using them, should be re-designed to be more iterator
/// based.
pub(crate) mod region_list {
    use super::super::vm_aspace::{VmAspaceLockClass, VmAspaceRegionLockClass};
    use super::{
        RegionOrMappingCommon, VmAddressRegion, VmAddressRegionOrMapping, bindings, constants,
    };
    use crate::crypto::Prng;
    use crate::kernel::align::is_rounded;
    use crate::kernel::types::VAddr;
    use core::ffi::c_void;
    use core::marker::PhantomData;
    use core::mem::{MaybeUninit, align_of, size_of};
    use core::{hint, ptr};
    use ksync::LockToken;
    use pin_init::{PinInit, pin_data};
    use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
    use zx_status::Status;
    use zx_types::zx_status_t;

    #[pin_data(PinnedDrop)]
    #[repr(transparent)]
    pub(super) struct Subregions {
        #[pin]
        raw: Opaque<bindings::VmAddressRegionSubregions>,
    }

    zr::static_assert!(size_of::<Subregions>() == constants::kVmAddressRegionSubregionsSize);
    zr::static_assert!(align_of::<Subregions>() == constants::kVmAddressRegionSubregionsAlign);

    unsafe_pinned_drop_ffi!(Subregions, bindings::cpp_vm_address_region_subregions_destroy);

    impl Subregions {
        #[inline]
        pub(super) fn new() -> impl PinInit<Self> {
            pin_init_ffi!(bindings::cpp_vm_address_region_subregions_construct)
        }
    }

    /// A cursor into a VMAR's region list, valid while the aspace lock or the region lock is held
    /// and the region list is not modified for `'a`.
    #[derive(Clone)]
    #[repr(C)]
    pub(crate) struct Cursor<'a> {
        /// # Safety
        ///
        /// `raw_0` and `raw_1` hold the bytes of a cursor into the region list of a live VMAR, and
        /// the aspace lock or the region lock is held and the region list is not modified for
        /// `'a`.
        raw_0: MaybeUninit<*mut c_void>,
        raw_1: MaybeUninit<usize>,
        _vmar: PhantomData<&'a VmAddressRegion>,
    }

    zr::static_assert!(
        size_of::<bindings::VmAddressRegionSubregionsCursor>()
            == constants::kVmAddressRegionSubregionsCursorSize
    );
    zr::static_assert!(
        align_of::<bindings::VmAddressRegionSubregionsCursor>()
            == constants::kVmAddressRegionSubregionsCursorAlign
    );
    zr::static_assert!(
        size_of::<Cursor<'_>>() == size_of::<bindings::VmAddressRegionSubregionsCursor>()
    );
    zr::static_assert!(
        align_of::<Cursor<'_>>() == align_of::<bindings::VmAddressRegionSubregionsCursor>()
    );

    impl<'a> Cursor<'a> {
        /// # Safety
        ///
        /// `init` initializes the cursor it is passed to a cursor into the region list of a VMAR,
        /// and the VMAR is live and its region list is not modified for `'a`.
        #[inline]
        unsafe fn new(init: impl FnOnce(*mut bindings::VmAddressRegionSubregionsCursor)) -> Self {
            let mut this = Self {
                raw_0: MaybeUninit::uninit(),
                raw_1: MaybeUninit::uninit(),
                _vmar: PhantomData,
            };
            init(this.as_ffi_mut_ptr());
            this
        }

        #[inline]
        fn as_ffi_ptr(&self) -> *const bindings::VmAddressRegionSubregionsCursor {
            ptr::from_ref(self).cast()
        }

        #[inline]
        fn as_ffi_mut_ptr(&mut self) -> *mut bindings::VmAddressRegionSubregionsCursor {
            ptr::from_mut(self).cast()
        }

        #[inline]
        pub(crate) fn is_valid(&self) -> bool {
            // SAFETY: `self` is a cursor into a live region list, and is not stale.
            unsafe { bindings::cpp_vm_address_region_subregions_cursor_is_valid(self.as_ffi_ptr()) }
        }

        /// Returns the region or mapping at the cursor, or `None` if the cursor is not valid.
        ///
        /// This does not check the canary. Callers should consider calling
        /// `common_canary().assert()` on the result.
        #[inline]
        pub(crate) fn get(&self) -> Option<VmAddressRegionOrMapping<'a>> {
            if !self.is_valid() {
                return None;
            }
            // SAFETY: `self` is valid.
            Some(unsafe { self.get_unchecked() })
        }

        /// # Safety
        ///
        /// `self` is valid.
        #[inline]
        unsafe fn get_unchecked(&self) -> VmAddressRegionOrMapping<'a> {
            // SAFETY: `self` is a valid cursor into a live region list, and is not stale.
            let raw =
                unsafe { bindings::cpp_vm_address_region_subregions_cursor_get(self.as_ffi_ptr()) };
            // SAFETY: `raw` is non-null, aligned, points to the `RegionOrMappingCommon` of a child
            // of the VMAR that is live for `'a`, and has the provenance of that child.
            unsafe { VmAddressRegionOrMapping::from_raw(raw.cast::<RegionOrMappingCommon>()) }
        }

        /// Advance to the next region in address order.
        #[inline]
        pub(in crate::vm) fn next(&mut self) {
            // SAFETY: `self` is a cursor into a live region list, and is not stale.
            unsafe { bindings::cpp_vm_address_region_subregions_cursor_next(self.as_ffi_mut_ptr()) }
        }

        /// # Safety
        ///
        /// For `'b`, `self` and the references `get` returns are used only while the aspace lock
        /// of the VMAR `self` points into is held and its region list is live and not modified
        /// since `self` was made.
        #[inline]
        pub(in crate::vm) unsafe fn unbind<'b>(self) -> Cursor<'b> {
            Cursor { raw_0: self.raw_0, raw_1: self.raw_1, _vmar: PhantomData }
        }

        /// # Safety
        ///
        /// `self` is not before the first item.
        #[inline]
        unsafe fn prev(&mut self) {
            // SAFETY: `self` is a cursor into a live region list, is not stale, and is not before
            // the first item.
            unsafe { bindings::cpp_vm_address_region_subregions_cursor_prev(self.as_ffi_mut_ptr()) }
        }
    }

    impl PartialEq for Cursor<'_> {
        #[inline]
        fn eq(&self, other: &Self) -> bool {
            // SAFETY: `self` and `other` are cursors into live region lists.
            unsafe {
                bindings::cpp_vm_address_region_subregions_cursor_eq(
                    self.as_ffi_ptr(),
                    other.as_ffi_ptr(),
                )
            }
        }
    }

    #[repr(transparent)]
    struct ObserverState {
        raw: Opaque<bindings::VmAddressRegionObserverState>,
    }

    impl ObserverState {
        #[inline]
        fn min_addr(&self) -> usize {
            // SAFETY: `self.raw` is a live observer state.
            unsafe { bindings::cpp_vm_address_region_observer_state_min_addr(self.raw.get()) }
        }

        #[inline]
        fn max_gap(&self) -> Option<usize> {
            let mut max_gap = MaybeUninit::<usize>::uninit();
            // SAFETY: `self.raw` is a live observer state and `max_gap` is valid for writes.
            let has_gap = unsafe {
                bindings::cpp_vm_address_region_observer_state_max_gap(
                    self.raw.get(),
                    max_gap.as_mut_ptr().cast(),
                )
            };
            // SAFETY: The shim initializes `max_gap` when it returns true.
            has_gap.then(|| unsafe { max_gap.assume_init() })
        }

        #[inline]
        fn max_addr(&self) -> usize {
            // SAFETY: `self.raw` is a live observer state.
            unsafe { bindings::cpp_vm_address_region_observer_state_max_addr(self.raw.get()) }
        }
    }

    #[inline]
    fn subregions(vmar: &VmAddressRegion) -> *mut bindings::VmAddressRegionSubregions {
        vmar.subregions.raw.get()
    }

    /// # Safety
    ///
    /// The aspace lock or the region lock is held, and the region list of `vmar` is not modified,
    /// for `'a`.
    #[inline]
    unsafe fn begin_raw<'a>(vmar: &'a VmAddressRegion) -> Cursor<'a> {
        // SAFETY: `vmar` is live and its region list is not modified for `'a`, `out` is valid for
        // writes, and the closure initializes `out` to a cursor into the region list of `vmar`.
        unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_begin(subregions(vmar), out.cast())
            })
        }
    }

    /// # Safety
    ///
    /// The aspace lock or the region lock is held, and the region list of `vmar` is not modified,
    /// for `'a`.
    #[inline]
    unsafe fn upper_bound_raw<'a>(vmar: &'a VmAddressRegion, key: VAddr) -> Cursor<'a> {
        // SAFETY: `vmar` is live and its region list is not modified for `'a`, `out` is valid for
        // writes, and the closure initializes `out` to a cursor into the region list of `vmar`.
        unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_upper_bound(
                    subregions(vmar),
                    key.0,
                    out.cast(),
                )
            })
        }
    }

    /// # Safety
    ///
    /// The aspace lock or the region lock is held, and the region list of `vmar` is not modified,
    /// for `'a`.
    #[inline]
    unsafe fn find_raw<'a>(vmar: &'a VmAddressRegion, key: VAddr) -> Cursor<'a> {
        // SAFETY: `vmar` is live and its region list is not modified for `'a`, `out` is valid for
        // writes, and the closure initializes `out` to a cursor into the region list of `vmar`.
        unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_find(subregions(vmar), key.0, out.cast())
            })
        }
    }

    /// # Safety
    ///
    /// The aspace lock or the region lock is held.
    #[inline]
    unsafe fn is_empty_raw(vmar: &VmAddressRegion) -> bool {
        // SAFETY: `vmar` is live and the aspace lock or the region lock is held.
        unsafe { bindings::cpp_vm_address_region_subregions_is_empty(subregions(vmar)) }
    }

    /// Find the region that covers `addr`, returns `None` if not found.
    #[inline]
    pub(crate) fn find_region<'a>(
        vmar: &'a VmAddressRegion,
        token: &'a LockToken<'_, VmAspaceLockClass>,
        addr: VAddr,
    ) -> Option<VmAddressRegionOrMapping<'a>> {
        // Find the first region with a base greater than `addr`. If a region
        // exists for `addr`, it will be immediately before it.
        let mut itr = upper_bound_locked(vmar, token, addr);
        // SAFETY: An upper bound cursor is not before the first item.
        unsafe { itr.prev() };
        let region = itr.get()?;
        // Subregion size should never be zero unless during unmapping which should never overlap
        // with this operation.
        debug_assert!(region.size() > 0);
        let (region_end, overflowed) = region.base().0.overflowing_add(region.size() - 1);
        assert!(!overflowed);
        if region.base().0 > addr.0 || addr.0 > region_end {
            return None;
        }

        Some(region)
    }

    /// Find the region that contains `base`, or if that doesn't exist, the first region that
    /// contains an address greater than `base`.
    pub(crate) fn include_or_higher<'a>(
        vmar: &'a VmAddressRegion,
        token: &'a LockToken<'_, VmAspaceLockClass>,
        base: VAddr,
    ) -> Cursor<'a> {
        // Find the first region with a base greater than `base`. If a region
        // exists for `base`, it will be immediately before it.
        let mut itr = upper_bound_locked(vmar, token, base);
        // SAFETY: An upper bound cursor is not before the first item.
        unsafe { itr.prev() };
        match itr.get() {
            // SAFETY: The aspace lock is held and the region list is not modified for `'a`.
            None => itr = unsafe { begin_raw(vmar) },
            Some(region) => {
                if base.0 >= region.base().0 && base.0 - region.base().0 >= region.size() {
                    // If `base` isn't in this region, ignore it.
                    itr.next();
                }
            }
        }
        itr
    }

    /// Insert `region` to the region list. On failure the region list is unmodified.
    ///
    /// # Safety
    ///
    /// `region` is kept alive by another owning reference for the duration of the call, and no
    /// child of `vmar` has the same base as `region`.
    #[inline]
    pub(crate) unsafe fn insert_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        region: VmAddressRegionOrMapping<'_>,
    ) -> Result<(), Status> {
        let base = region.base();
        // SAFETY: `vmar` is live, `region` is kept alive by another owning reference for the
        // duration of the call, no child of `vmar` has base `base`, the region and aspace locks are
        // held, `out` is valid for writes, and the closure initializes `out` to a cursor into the
        // region list of `vmar`, which is not modified while the cursor is used.
        let itr = unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_insert(
                    subregions(vmar),
                    base.0,
                    region.as_ffi_ptr(),
                    out.cast(),
                )
            })
        };
        if itr.is_valid() {
            Ok(())
        } else {
            hint::cold_path();
            Err(Status::NO_MEMORY)
        }
    }

    /// Remove `region` from the list.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar` and is kept alive by another owning reference for the
    /// duration of the call.
    pub(crate) unsafe fn remove_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        region: &RegionOrMappingCommon,
    ) {
        // SAFETY: The region and aspace locks are held, and the region list is modified only by the
        // last use of `it`.
        let it = unsafe { find_raw(vmar, region.base()) };
        assert!(it.is_valid());
        // SAFETY: `vmar` is live, `it` is a valid cursor into its region list, the item it points
        // to is kept alive by another owning reference for the duration of the call, and the
        // region and aspace locks are held.
        unsafe {
            bindings::cpp_vm_address_region_subregions_erase(subregions(vmar), it.as_ffi_ptr())
        }
    }

    /// Replaces the target region with a new region at the same address. Unlike insertion this
    /// cannot fail.
    ///
    /// # Safety
    ///
    /// `prev` is a child of `vmar`, `prev` and `region` are kept alive by other owning references
    /// for the duration of the call, and `region` does not overlap with any other child of `vmar`.
    pub(in crate::vm) unsafe fn replace_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        prev: &RegionOrMappingCommon,
        region: VmAddressRegionOrMapping<'_>,
    ) {
        assert!(prev.base() == region.base());
        // SAFETY: The region and aspace locks are held, and the region list is modified only by the
        // last use of `it`.
        let it = unsafe { find_raw(vmar, prev.base()) };
        assert!(it.is_valid());
        // SAFETY: `vmar` is live, `it` is a valid cursor into the region list of `vmar`, the item
        // it points to and `region` are kept alive by other owning references for the duration of
        // the call, `region` has the same base as that item and does not overlap with any other
        // child of `vmar`, and the region and aspace locks are held.
        unsafe {
            bindings::cpp_vm_address_region_subregions_update(
                subregions(vmar),
                it.as_ffi_ptr(),
                region.as_ffi_ptr(),
            )
        }
    }

    /// Request the region to the left of the given region.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar`.
    pub(in crate::vm) unsafe fn left_of<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        region: &RegionOrMappingCommon,
    ) -> Option<VmAddressRegionOrMapping<'a>> {
        // SAFETY: The aspace lock is held and the region list is not modified for `'a`.
        let mut it = unsafe { find_raw(vmar, region.base()) };
        debug_assert!(it.is_valid());
        // SAFETY: `it` points to `region`, so it is not before the first item.
        unsafe { it.prev() };
        it.get()
    }

    /// Request the region to the right of the given region.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar`.
    pub(in crate::vm) unsafe fn right_of<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        region: &RegionOrMappingCommon,
    ) -> Option<VmAddressRegionOrMapping<'a>> {
        // SAFETY: The aspace lock is held and the region list is not modified for `'a`.
        let mut it = unsafe { find_raw(vmar, region.base()) };
        debug_assert!(it.is_valid());
        it.next();
        it.get()
    }

    /// Returns whether the region list is empty.
    #[inline]
    pub(in crate::vm) fn is_empty(
        vmar: &VmAddressRegion,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> bool {
        // SAFETY: The aspace lock is held.
        unsafe { is_empty_raw(vmar) }
    }

    /// Returns the first element of the list.
    ///
    /// # Safety
    ///
    /// The list is not empty.
    #[inline]
    pub(super) unsafe fn front<'a>(
        vmar: &'a VmAddressRegion,
        token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> VmAddressRegionOrMapping<'a> {
        debug_assert!(!is_empty(vmar, token));
        // SAFETY: The aspace lock is held and the region list is not modified for `'a`.
        let it = unsafe { begin_raw(vmar) };
        // SAFETY: The list is not empty, so `it` is valid.
        unsafe { it.get_unchecked() }
    }

    #[inline]
    pub(super) fn size_slow(
        vmar: &VmAddressRegion,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> usize {
        // SAFETY: `vmar` is live and the aspace lock is held.
        unsafe {
            bindings::cpp_vm_address_region_subregions_calculate_utilization_slow(subregions(vmar))
        }
    }

    /// Check whether it would be valid to create a child in the range [base, base+size).
    pub(crate) fn is_range_available(
        vmar: &VmAddressRegion,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        base: VAddr,
        size: usize,
    ) -> bool {
        debug_assert!(size > 0);

        // Find the first region with base > `base`. Since the region list has no
        // overlapping elements, we just need to check this one and the prior
        // child.

        let mut prev = upper_bound(vmar, region_token, base);
        let next = prev.clone();
        // SAFETY: An upper bound cursor is not before the first item.
        unsafe { prev.prev() };

        if let Some(p) = prev.get() {
            let (prev_last_byte, overflowed) = p.base().0.overflowing_add(p.size() - 1);
            if overflowed {
                return false;
            }
            if prev_last_byte >= base.0 {
                return false;
            }
        }

        if let Some(n) = next.get() {
            let (last_byte, overflowed) = base.0.overflowing_add(size - 1);
            if overflowed {
                return false;
            }
            if n.base().0 <= last_byte {
                return false;
            }
        }
        true
    }

    #[inline]
    pub(crate) fn upper_bound_locked<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        addr: VAddr,
    ) -> Cursor<'a> {
        // SAFETY: The aspace lock is held and the region list is not modified for `'a`.
        unsafe { upper_bound_raw(vmar, addr) }
    }

    /// Returns a cursor to the first region with base strictly greater than `addr`, or an end
    /// cursor if no such region exists.
    #[inline]
    fn upper_bound<'a>(
        vmar: &'a VmAddressRegion,
        _region_token: &'a LockToken<'_, VmAspaceRegionLockClass>,
        addr: VAddr,
    ) -> Cursor<'a> {
        // SAFETY: The region lock is held and the region list is not modified for `'a`.
        unsafe { upper_bound_raw(vmar, addr) }
    }

    /// Returns a cursor to the first region in the list, or an end cursor if the list is empty.
    #[inline]
    fn begin<'a>(
        vmar: &'a VmAddressRegion,
        _region_token: &'a LockToken<'_, VmAspaceRegionLockClass>,
    ) -> Cursor<'a> {
        // SAFETY: The region lock is held and the region list is not modified for `'a`.
        unsafe { begin_raw(vmar) }
    }

    /// Returns the regions immediately preceding and following the `upper_bound` of `addr` (i.e.,
    /// the last region with `base <= addr` and the first region with `base > addr`), if present.
    #[inline]
    pub(crate) fn upper_bound_neighbours<'a>(
        vmar: &'a VmAddressRegion,
        region_token: &'a LockToken<'_, VmAspaceRegionLockClass>,
        addr: VAddr,
    ) -> (Option<VmAddressRegionOrMapping<'a>>, Option<VmAddressRegionOrMapping<'a>>) {
        let after_iter = upper_bound(vmar, region_token, addr);
        let mut before_iter = after_iter.clone();
        // SAFETY: An upper bound cursor is not before the first item.
        unsafe { before_iter.prev() };
        (before_iter.get(), after_iter.get())
    }

    #[inline]
    pub(super) fn for_each<F: FnMut(VmAddressRegionOrMapping<'_>)>(
        vmar: &VmAddressRegion,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        _token: &LockToken<'_, VmAspaceLockClass>,
        mut f: F,
    ) {
        let mut itr = begin(vmar, region_token);
        while let Some(child) = itr.get() {
            f(child);
            itr.next();
        }
    }

    /// Walks the region list of `vmar`, calling `search.subtree` for each intermediate btree node
    /// and `search.leaf` for each leaf node, until either returns an error.
    #[inline]
    fn walk<'a>(
        vmar: &'a VmAddressRegion,
        _region_token: &'a LockToken<'_, VmAspaceRegionLockClass>,
        search: &mut SpotSearch,
    ) -> Result<(), Status> {
        /// # Safety
        ///
        /// `ctx` is the pointer `walk` passes the C++ walk, which is derived from the `search`
        /// borrowed for the call, and no other reference to `search` is used while the result is.
        #[inline(always)]
        unsafe fn search_from_ctx<'b>(ctx: *mut c_void) -> &'b mut SpotSearch {
            // SAFETY: By the caller's obligation `ctx` is derived from a `&mut SpotSearch` that is
            // borrowed for the whole C++ walk, which outlives the callback using the result, so it
            // is non-null, aligned and points to a live `SpotSearch`, and nothing else accesses
            // that `SpotSearch` while the result is used.
            unsafe { &mut *ctx.cast::<SpotSearch>() }
        }

        /// # Safety
        ///
        /// `ctx` is the pointer `walk` passes the C++ walk.
        #[inline(always)]
        unsafe extern "C" fn call_subtree(state: &ObserverState, ctx: *mut c_void) -> zx_status_t {
            // SAFETY: `ctx` is the pointer `walk` passes the C++ walk, and the walk does not use
            // `search` while it runs a callback.
            let search = unsafe { search_from_ctx(ctx) };
            Status::result_into_raw(search.subtree(state))
        }

        /// # Safety
        ///
        /// `ctx` is the pointer `walk` passes the C++ walk.
        #[inline(always)]
        unsafe extern "C" fn call_leaf<'a>(
            state: &ObserverState,
            first: &Cursor<'a>,
            last: &Cursor<'a>,
            ctx: *mut c_void,
        ) -> zx_status_t {
            // SAFETY: `ctx` is the pointer `walk` passes the C++ walk, and the walk does not use
            // `search` while it runs a callback.
            let search = unsafe { search_from_ctx(ctx) };
            Status::result_into_raw(search.leaf(state, first.clone(), last.clone()))
        }

        // SAFETY: The declaration matches the C++ one but for taking references, which have the
        // ABI of pointers, to `ObserverState`, which is `#[repr(transparent)]` over the C++ type,
        // and `Cursor`, which is layout-compatible with the C++ type.
        unsafe extern "C" {
            fn cpp_vm_address_region_subregions_walk<'a>(
                subregions: *const bindings::VmAddressRegionSubregions,
                subtree: Option<unsafe extern "C" fn(&ObserverState, *mut c_void) -> zx_status_t>,
                leaf: Option<
                    unsafe extern "C" fn(
                        &ObserverState,
                        &Cursor<'a>,
                        &Cursor<'a>,
                        *mut c_void,
                    ) -> zx_status_t,
                >,
                ctx: *mut c_void,
            ) -> zx_status_t;
        }

        // SAFETY: `vmar` is live and its region list is not modified for `'a`, and the region lock
        // is held. The walk passes the callbacks a live observer state, valid cursors into the
        // region list of `vmar`, and `ctx` unchanged, which is the pointer `walk` passes it, as
        // `call_subtree` and `call_leaf` require. `ctx` is derived from `search`, which is
        // borrowed for the call and used only through `ctx` during it.
        Status::ok(unsafe {
            cpp_vm_address_region_subregions_walk(
                subregions(vmar),
                Some(call_subtree),
                Some(call_leaf),
                ptr::from_mut(search).cast(),
            )
        })
    }

    pub(crate) struct FindSpotAtIndexFailed {
        pub(crate) candidate_spot_count: usize,
    }

    struct AlignedRange {
        base: usize,
        size: usize,
    }

    struct SpotSearch {
        target_index: usize,
        candidate_spot_count: usize,
        next_gap_start: usize,
        next_gap_overflow: bool,
        spot: usize,
        align_pow2: u8,
        size: usize,
        upper_limit: usize,
    }

    impl SpotSearch {
        /// Returns the number of addresses that satisfy the size and alignment in the given range,
        /// accounting for ranges that overlap the upper limit.
        #[inline(always)]
        fn spots_in_range(&self, aligned_base: usize, aligned_size: usize) -> usize {
            debug_assert!(aligned_base < self.upper_limit);

            let range_limit = aligned_base.saturating_add(aligned_size);
            let clamped_range_size = if range_limit < self.upper_limit {
                aligned_size
            } else {
                aligned_size - (range_limit - self.upper_limit)
            };

            if clamped_range_size >= self.size {
                let diff = clamped_range_size - self.size;
                return diff.wrapping_shr(u32::from(self.align_pow2)) + 1;
            }
            0
        }

        /// Returns the given range with the base aligned and the size adjusted to maintain the same
        /// end address. If the aligned base address is greater than the end address, the returned
        /// size is zero.
        #[inline(always)]
        fn align_range(&self, range_base: usize, range_size: usize) -> AlignedRange {
            let align = 1usize << self.align_pow2;
            // Rounding up wraps to 0, as C++ `ROUNDUP` does, when `range_base` is within
            // `align - 1` of the top of the address space. `base_delta` is then
            // `2^64 - range_base`, which is at least `range_size` because the range does not wrap,
            // so `aligned_size` is 0 and the range holds no spot of the nonzero size searched for.
            let aligned_base = range_base.wrapping_add(align - 1) & !(align - 1);
            let base_delta = aligned_base.wrapping_sub(range_base);
            let aligned_size = range_size.saturating_sub(base_delta);
            AlignedRange { base: aligned_base, size: aligned_size }
        }

        /// Helper to process a gap and count our spots. Returns true if a spot was found.
        #[inline(always)]
        fn record_gap(&mut self, gap_base: usize, gap_size: usize) -> bool {
            let aligned_gap = self.align_range(gap_base, gap_size);
            if aligned_gap.base >= self.upper_limit {
                return false;
            }
            let spot_count = self.spots_in_range(aligned_gap.base, aligned_gap.size);
            self.candidate_spot_count += spot_count;
            if self.target_index < spot_count {
                self.spot = aligned_gap.base + (self.target_index << self.align_pow2);
                return true;
            }
            self.target_index -= spot_count;
            false
        }

        /// Called by the walk for handling an intermediate btree node.
        #[inline(always)]
        fn subtree(&mut self, state: &ObserverState) -> Result<(), Status> {
            let min_addr = state.min_addr();
            if self.next_gap_start < min_addr
                && self.record_gap(self.next_gap_start, min_addr - self.next_gap_start)
            {
                return Err(Status::STOP);
            }
            if min_addr >= self.upper_limit {
                return Err(Status::OUT_OF_RANGE);
            }
            if state.max_gap().is_some_and(|max_gap| max_gap < self.size) {
                // max_addr is inclusive, but next_gap_start is exclusive, so we add 1. Should this
                // overflow then that means we have reached the end of the possible address space
                // and will be handled when we check for trailing gaps at the end.
                (self.next_gap_start, self.next_gap_overflow) = state.max_addr().overflowing_add(1);
                // This subtree has no gaps that would fit, so skip the entire subtree with
                // `Status::NEXT`.
                return Err(Status::NEXT);
            }
            // Set next_gap_start to the start of the subtree as we are about to descend into it
            // and we previously processed any gap up to min_addr.
            self.next_gap_start = min_addr;
            Ok(())
        }

        /// Called by the walk for handling leaf btree nodes.
        #[inline(always)]
        fn leaf<'a>(
            &mut self,
            state: &ObserverState,
            mut first: Cursor<'a>,
            mut last: Cursor<'a>,
        ) -> Result<(), Status> {
            let min_addr = state.min_addr();
            if self.next_gap_start < min_addr
                && self.record_gap(self.next_gap_start, min_addr - self.next_gap_start)
            {
                return Err(Status::STOP);
            }
            if min_addr >= self.upper_limit {
                return Err(Status::OUT_OF_RANGE);
            }
            // No matter what happens, we will have processed till max_addr + 1. See `subtree` for
            // an explanation of +1 and overflow.
            (self.next_gap_start, self.next_gap_overflow) = state.max_addr().overflowing_add(1);
            if state.max_gap().is_some_and(|max_gap| max_gap < self.size) {
                // No gaps in this leaf node that would fit, can skip the iteration and go to the
                // next node.
                return Err(Status::NEXT);
            }
            let mut prev = first.clone();
            // The provided iterators are inclusive, so increment last to simplify our loop.
            last.next();
            first.next();
            while first != last {
                // SAFETY: `prev` and `first` are in the inclusive range the walk provided.
                let (prev_region, first_region) =
                    unsafe { (prev.get_unchecked(), first.get_unchecked()) };
                let gap_start = prev_region.base().0.wrapping_add(prev_region.size());
                let gap_end = first_region.base().0;
                if gap_start < gap_end && self.record_gap(gap_start, gap_end - gap_start) {
                    // Location found, can cease walking.
                    return Err(Status::STOP);
                }
                prev = first.clone();
                first.next();
            }
            // Continue to the next node.
            Err(Status::NEXT)
        }
    }

    /// Returns the base address of an available spot in the address range that satisfies the given
    /// entropy, alignment, size, and upper limit requirements. If no spot is found that satisfies
    /// the given entropy (i.e. `target_index`), the number of candidate spots encountered is
    /// returned.
    ///
    /// See vm/vm_address_region_observer.h for an explanation of the augmented state used by
    /// this method to perform efficient tree traversal.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn find_spot_at_index(
        vmar: &VmAddressRegion,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        target_index: usize,
        align_pow2: u8,
        size: usize,
        parent_base: VAddr,
        parent_size: usize,
        upper_limit: VAddr,
    ) -> Result<VAddr, FindSpotAtIndexFailed> {
        assert!(u32::from(align_pow2) < usize::BITS);
        let mut search = SpotSearch {
            target_index,
            // Track the number of candidate spots encountered.
            candidate_spot_count: 0,
            // As we iterate through regions we remember the end of the last allocated region as
            // the start of a potential gap.
            next_gap_start: parent_base.0,
            // Because an allocation can end at the very top of the 64-bit address space the
            // calculation of the logical start of the next gap (end + 1) could overflow. To assist
            // with debug validation we track this overflow explicitly.
            next_gap_overflow: false,
            // When we do find something record it here. Use `usize::MAX`, which can never be a
            // valid base address, to track the difference between the walk terminating early with
            // success, or completing with success (and hence not yet having a spot).
            spot: usize::MAX,
            align_pow2,
            size,
            upper_limit: upper_limit.0,
        };

        // Walk the btree examining the augmented state to optimally skip irrelevant subtrees.
        let status = walk(vmar, region_token, &mut search);
        if status.is_err_and(|status| status != Status::OUT_OF_RANGE) {
            return Err(FindSpotAtIndexFailed {
                candidate_spot_count: search.candidate_spot_count,
            });
        }
        // Check if we already found a spot or if we need to consider any trailing gap.
        if search.spot != usize::MAX {
            return Ok(VAddr(search.spot));
        }
        if search.next_gap_overflow {
            hint::cold_path();
            // The next gap should only overflow if the parent region is the end of the 64-bit
            // address space. There should also, therefore, not actually be any gap.
            let (parent_top, overflowed) = parent_base.0.overflowing_add(parent_size);
            assert!(overflowed);
            assert!(parent_top == search.next_gap_start);
        } else {
            // Any potential remaining gap has not wrapped, but the parent could still be at the end
            // of address space, so operate on its max_byte and not its top to avoid overflow.
            let parent_max_byte = parent_base.0 + (parent_size - 1);
            if search.next_gap_start <= parent_max_byte {
                let remaining_size = parent_max_byte - search.next_gap_start + 1;
                if search.record_gap(search.next_gap_start, remaining_size) {
                    return Ok(VAddr(search.spot));
                }
            }
        }
        Err(FindSpotAtIndexFailed { candidate_spot_count: search.candidate_spot_count })
    }

    /// Get the allocation spot that is free and large enough for the aligned size.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn get_alloc_spot(
        vmar: &VmAddressRegion,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        align_pow2: u8,
        entropy: u8,
        size: usize,
        parent_base: VAddr,
        parent_size: usize,
        prng: Option<&Prng>,
        upper_limit: VAddr,
    ) -> Result<VAddr, Status> {
        // Ensure our candidate calculation shift will not overflow.
        debug_assert!(u32::from(entropy) < usize::BITS);

        // The number of addresses to consider based on the configured entropy.
        let max_candidate_spaces = 1usize << entropy;

        // We first pick an index in [0, max_candidate_spaces) and hope to find a spot there. If
        // the number of available spots is less than the selected index, the attempt fails,
        // returning the actual number of candidate spots found, and we try again in this smaller
        // range.
        //
        // This is mathematically equivalent to randomly picking a spot within
        // [0, candidate_spot_count) when selected_index < candidate_spot_count.
        //
        // Prove as following:
        // Define M = candidate_spot_count
        // Define N = max_candidate_spaces (M < N, otherwise we can randomly allocate any spot from
        // [0, max_candidate_spaces), thus allocate a specific slot has (1 / N) probability).
        // Define slot X0 where X0 belongs to [1, M].
        // Define event A: randomly pick a slot X in [1, N], N = X0.
        // Define event B: randomly pick a slot X in [1, N], N belongs to [1, M].
        // Define event C: randomly pick a slot X in [1, N], N = X0 when N belongs to [1, M].
        // P(C) = P(A | B)
        // Since when A happens, B definitely happens, so P(AB) = P(A)
        // P(C) = P(A) / P(B) = (1 / N) / (M / N) = (1 / M)
        // which is equal to the probability of picking a specific spot in [0, M).
        let mut selected_index = if let Some(prng) = prng {
            prng.rand_int(max_candidate_spaces as u64) as usize
        } else {
            0
        };

        let mut allocation_result = find_spot_at_index(
            vmar,
            region_token,
            selected_index,
            align_pow2,
            size,
            parent_base,
            parent_size,
            upper_limit,
        );
        if let Err(FindSpotAtIndexFailed { candidate_spot_count }) = allocation_result {
            if candidate_spot_count == 0 {
                hint::cold_path();
                return Err(Status::NO_RESOURCES);
            }

            // If the number of available spaces is smaller than the selected index, pick again
            // from the available range.
            debug_assert!(candidate_spot_count < max_candidate_spaces);
            let prng = prng.expect("index 0 failed with candidate spots");
            selected_index = prng.rand_int(candidate_spot_count as u64) as usize;
            allocation_result = find_spot_at_index(
                vmar,
                region_token,
                selected_index,
                align_pow2,
                size,
                parent_base,
                parent_size,
                upper_limit,
            );
        }

        let Ok(alloc_spot) = allocation_result else {
            panic!("no spot at the selected index");
        };
        if !is_rounded(alloc_spot.0, 1usize << align_pow2) {
            #[cold]
            #[inline(never)]
            fn panic_unaligned_spot(size: usize, align_pow2: u8, alloc_spot: usize) -> ! {
                panic!("size={size} align_pow2={align_pow2} alloc_spot={alloc_spot:x}");
            }
            panic_unaligned_spot(size, align_pow2, alloc_spot.0);
        }
        Ok(alloc_spot)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_base(this: &RegionOrMappingCommon) -> usize {
    this.base().0
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_size(this: &RegionOrMappingCommon) -> usize {
    this.size()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_destroy(this: &VmAddressRegion) -> Result<(), Status> {
    this.destroy()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_base(this: &VmAddressRegion) -> usize {
    this.base().0
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_size(this: &VmAddressRegion) -> usize {
    this.size()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_flags(this: &VmAddressRegion) -> u32 {
    this.flags()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_aspace(this: &VmAddressRegion) -> &RefPtr<VmAspace> {
    this.aspace()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_has_parent(this: &VmAddressRegion) -> bool {
    this.has_parent()
}

/// # Safety
///
/// The virtual address range `[base, base + size)`, and the rest of any page it partly covers, may
/// have its permissions changed. Nothing may access that memory, or the rest of any large page it
/// partly covers, in a way that relies on it staying mapped, and no Rust reference into it may be
/// live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_protect(
    this: &VmAddressRegion,
    base: usize,
    size: usize,
    new_arch_mmu_flags: ArchMmuFlags,
    op_children: VmAddressRegionOpChildren,
) -> zx_status_t {
    // SAFETY: The virtual address range `[base, base + size)`, and the rest of any page it partly
    // covers, may have its permissions changed. Nothing relies on that memory, or on the rest of
    // any large page it partly covers, staying mapped, and no Rust reference into it is live.
    Status::result_into_raw(unsafe {
        this.protect(VAddr(base), size, new_arch_mmu_flags, op_children)
    })
}

/// # Safety
///
/// The caller must ensure that `name` is either null or points to a valid NUL-terminated C string
/// within a single allocation that is not mutated for the duration of the call, and that `this`
/// and `out` are valid, aligned, non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_create_sub_vmar(
    this: &VmAddressRegion,
    offset: usize,
    size: usize,
    align_pow2: u8,
    vmar_flags: u32,
    name: *const c_char,
    out: &mut MaybeUninit<RefPtr<VmAddressRegion>>,
) -> Result<(), Status> {
    let name = if name.is_null() {
        c""
    } else {
        // SAFETY: `name` is non-null, and the caller guarantees it points to a valid
        // NUL-terminated string that is not mutated during this call.
        unsafe { CStr::from_ptr(name) }
    };
    out.write(this.create_sub_vmar(offset, size, align_pow2, vmar_flags, name.to_bytes())?);
    Ok(())
}

/// # Safety
///
/// The caller must ensure that if `vmar_flags` contains [`flag::SPECIFIC_OVERWRITE`]
/// (`VMAR_FLAG_SPECIFIC_OVERWRITE`), nothing accesses
/// `[this.base() + mapping_offset, this.base() + mapping_offset + size)`, or the rest of any page
/// (including any large page) it partly covers, in a way that relies on it staying mapped, and no
/// Rust reference into that memory is live. `this`, `out_base`, and `out_mapping` must be valid,
/// aligned, non-null pointers, and `vmo` must be a non-null owned reference to a live `VmObject`
/// (e.g. via `fbl::ExportToRawPtr`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_create_vm_mapping(
    this: &VmAddressRegion,
    mapping_offset: usize,
    size: usize,
    align_pow2: u8,
    vmar_flags: u32,
    vmo: RefPtr<VmObject>,
    vmo_offset: u64,
    arch_mmu_flags: ArchMmuFlags,
    out_base: &mut MaybeUninit<usize>,
    out_mapping: &mut MaybeUninit<RefPtr<VmMapping>>,
) -> Result<(), Status> {
    // SAFETY: If `vmar_flags` contains `flag::SPECIFIC_OVERWRITE`, nothing relies on the range, or
    // on the rest of any page it partly covers, staying mapped, and no Rust reference into it is
    // live.
    let result = unsafe {
        this.create_vm_mapping(
            mapping_offset,
            size,
            align_pow2,
            vmar_flags,
            vmo,
            vmo_offset,
            arch_mmu_flags,
        )
    }?;
    out_base.write(result.base);
    out_mapping.write(result.mapping);
    Ok(())
}

/// # Safety
///
/// `this` points to the `RegionOrMappingCommon` of an initialized region or mapping in storage
/// returned by `allocate_exposed` to which no reference remains, and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_recycle(
    this: NonNull<RegionOrMappingCommon>,
) {
    // SAFETY: `this` points to the `RegionOrMappingCommon` of an initialized region or mapping in
    // storage returned by `allocate_exposed` to which no reference remains, and is not used
    // afterwards.
    unsafe { RegionOrMappingCommon::recycle(this) }
}

/// # Safety
///
/// `this` points to an initialized region in storage returned by `allocate_exposed` to which no
/// reference remains, and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_recycle(this: NonNull<VmAddressRegion>) {
    // SAFETY: `this` points to an initialized region in storage returned by `allocate_exposed` to
    // which no reference remains, and is not used afterwards.
    unsafe { VmAddressRegion::recycle(this) }
}
