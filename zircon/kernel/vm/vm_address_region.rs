// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHE_MASK, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_WRITE, ArchMmuFlags,
};
use super::attribution::AttributionCounts;
use super::name::name_to_c_buf;
use super::vm_address_region_enumerator::{
    VmAddressRegionEnumerator, VmAddressRegionEnumeratorType,
};
use super::vm_aspace::{
    VmAspace, VmAspaceLockClass, VmAspaceMutex, VmAspaceRegionLockClass, VmAspaceRegionMutex,
};
use super::vm_mapping::VmMapping;
use super::vm_object::{EvictionHint, VmObject, VmObjectLockClass};
use crate::kernel::types::VAddr;
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::user_copy::UserInOutPtr;
use ::page as kernel_page;
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};
use core::hint;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use debug::ltracef;
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use kprint::kprint;
use ksync::{KCell, KMutexGuard, LockToken};
use pin_init::{PinInit, pin_data};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_status_t};

const LOCAL_TRACE: u32 = 0;

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

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifeCycleState {
    /// Initial state: not yet in the address space layout, so no `destroy` is needed.
    NotReady = 0,
    /// Usual state: information is representative of the address space layout. Must be destroyed
    /// before the last reference is dropped.
    Alive = 1,
    /// Object is invalid.
    Dead = 2,
}

zr::static_assert!(
    core::mem::size_of::<LifeCycleState>()
        == core::mem::size_of::<bindings::VmAddressRegionOrMapping_LifeCycleState>()
);
zr::static_assert!(
    LifeCycleState::NotReady as u8
        == bindings::VmAddressRegionOrMapping_LifeCycleState::NOT_READY as u8
);
zr::static_assert!(
    LifeCycleState::Alive as u8 == bindings::VmAddressRegionOrMapping_LifeCycleState::ALIVE as u8
);
zr::static_assert!(
    LifeCycleState::Dead as u8 == bindings::VmAddressRegionOrMapping_LifeCycleState::DEAD as u8
);

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

/// A `VmAddressRegion` represents a contiguous region of the virtual address
/// space.  It is partitioned by non-overlapping children of the following types:
/// 1) child `VmAddressRegion`
/// 2) child `VmMapping` (leafs that map `VmObject`s into the address space)
/// 3) gaps (logical, not actually objects).
///
/// `VmAddressRegionOrMapping` represents a tagged union of the two types.
///
/// A `VmAddressRegion`/`VmMapping` may be in one of two states: `Alive` or `Dead`.  If
/// it is `Alive`, then the `VmAddressRegion` is a description of the virtual memory
/// mappings of the address range it represents in its parent `VmAspace`.  If it is
/// `Dead`, then the `VmAddressRegion` is invalid and has no meaning.
///
/// All `VmAddressRegion` and `VmMapping` state is protected by the aspace lock.
#[repr(transparent)]
pub struct VmAddressRegionOrMapping {
    raw: Opaque<bindings::VmAddressRegionOrMapping>,
}

#[repr(C)]
pub struct VmAddressRegionOrMappingState {
    ref_count: RefCounted,
    canary: Canary<{ fbl::magic(b"VMRM") }>,
    is_mapping: bool,
    /// current state of the VMAR. If `LifeCycleState::Dead`, then all other
    /// fields are invalid.
    life_cycle: UnsafeCell<LifeCycleState>,
    /// Priority of the VMAR. This starts at `Default` and must be reset back to `Default` as part
    /// of the destroy path to ensure any propagation is undone correctly.
    memory_priority: KCell<MemoryPriority, VmAspaceLockClass>,
    /// flags from VMAR creation time
    flags: u32,
    /// address/size within the container address space
    base: VAddr,
    size: usize,
    /// pointer back to our member address space. The aspace's lock is used
    /// to serialize all modifications.
    aspace: RefPtr<VmAspace>,
    /// pointer back to our parent region (null if root or destroyed)
    parent: KCell<*mut VmAddressRegion, VmAspaceLockClass>,
}

::zr::static_assert!(cfg!(debug_assertions) == constants::kRefCountedAdoptionValidator);

::zr::static_assert_size_and_align!(
    VmAddressRegionOrMappingState,
    constants::kVmAddressRegionOrMappingStateSize,
    constants::kVmAddressRegionOrMappingStateAlign,
);

impl VmAddressRegionOrMappingState {
    fn new(
        base: usize,
        size: usize,
        flags: u32,
        aspace: &VmAspace,
        parent: *mut VmAddressRegion,
        is_mapping: bool,
        life_cycle: LifeCycleState,
    ) -> Self {
        Self {
            ref_count: RefCounted::new(),
            canary: Canary::new(),
            is_mapping,
            life_cycle: UnsafeCell::new(life_cycle),
            memory_priority: KCell::new(MemoryPriority::Default),
            flags,
            base: VAddr(base),
            size,
            aspace: RefPtr::from_ref(aspace),
            parent: KCell::new(parent),
        }
    }
}

impl HasRefCount for VmAddressRegionOrMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        &self.state().ref_count
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmAddressRegionOrMapping {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` is the last reference to a region or mapping allocated with `kalloc::Box`.
        unsafe { Self::free(ptr) }
    }
}

// SAFETY: `VmAddressRegionOrMapping` is not tied to the thread that created it.
unsafe impl Send for VmAddressRegionOrMapping {}
// SAFETY: Mutable state of `VmAddressRegionOrMapping` is only accessed with the aspace lock or the
// region lock held.
unsafe impl Sync for VmAddressRegionOrMapping {}

impl VmAddressRegionOrMapping {
    #[inline]
    fn state(&self) -> &VmAddressRegionOrMappingState {
        // SAFETY: `self` points to a live `VmAddressRegionOrMapping`.
        unsafe { &*bindings::cpp_vm_address_region_or_mapping_state(self.raw.get()).cast() }
    }

    // Accessors.
    #[inline]
    pub fn base(&self) -> VAddr {
        self.state().base
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.state().size
    }

    #[inline]
    pub fn flags(&self) -> u32 {
        self.state().flags
    }

    #[inline]
    pub fn aspace(&self) -> &RefPtr<VmAspace> {
        &self.state().aspace
    }

    /// Subtype information and safe down-casting.
    #[inline]
    pub fn is_mapping(&self) -> bool {
        self.state().is_mapping
    }

    fn downcast(&self) -> RegionOrMapping<'_> {
        if self.is_mapping() {
            // SAFETY: `self` is a `VmMapping`, as `is_mapping()` reports.
            let mapping =
                unsafe { bindings::cpp_vm_address_region_or_mapping_as_vm_mapping(self.raw.get()) };
            // SAFETY: `mapping` points to the live `VmMapping` that is `self`.
            RegionOrMapping::Mapping(unsafe { &*mapping.cast::<VmMapping>() })
        } else {
            // SAFETY: `self` is a `VmAddressRegion`, as `is_mapping()` reports.
            let region = unsafe {
                bindings::cpp_vm_address_region_or_mapping_as_vm_address_region(self.raw.get())
            };
            // SAFETY: `region` points to the live `VmAddressRegion` that is `self`.
            RegionOrMapping::Region(unsafe { &*region.cast::<VmAddressRegion>() })
        }
    }

    fn into_downcast(this: RefPtr<Self>) -> OwnedRegionOrMapping {
        let is_mapping = this.is_mapping();
        let raw = RefPtr::into_raw(this).cast_mut().cast::<bindings::VmAddressRegionOrMapping>();
        if is_mapping {
            // SAFETY: `raw` points to a live `VmMapping`.
            let mapping = unsafe { bindings::cpp_vm_address_region_or_mapping_as_vm_mapping(raw) };
            // SAFETY: `mapping` is the object `raw` points to, its reference is owned by the
            // result, and it was allocated as a `VmMapping`.
            OwnedRegionOrMapping::Mapping(unsafe { RefPtr::from_raw(mapping.cast()) })
        } else {
            // SAFETY: `raw` points to a live `VmAddressRegion`.
            let region =
                unsafe { bindings::cpp_vm_address_region_or_mapping_as_vm_address_region(raw) };
            // SAFETY: `region` is the object `raw` points to, its reference is owned by the
            // result, and it was allocated as a `VmAddressRegion`.
            OwnedRegionOrMapping::Region(unsafe { RefPtr::from_raw(region.cast()) })
        }
    }

    #[inline]
    pub fn as_vm_address_region(&self) -> Option<RefPtr<VmAddressRegion>> {
        self.as_vm_address_region_ptr().map(RefPtr::from_ref)
    }

    pub fn as_vm_address_region_ptr(&self) -> Option<&VmAddressRegion> {
        self.state().canary.assert();
        match self.downcast() {
            RegionOrMapping::Region(region) => Some(region),
            RegionOrMapping::Mapping(_) => None,
        }
    }

    #[inline]
    pub fn as_vm_mapping(&self) -> Option<RefPtr<VmMapping>> {
        self.as_vm_mapping_ptr().map(RefPtr::from_ref)
    }

    pub fn as_vm_mapping_ptr(&self) -> Option<&VmMapping> {
        self.state().canary.assert();
        match self.downcast() {
            RegionOrMapping::Mapping(mapping) => Some(mapping),
            RegionOrMapping::Region(_) => None,
        }
    }

    /// Expose our backing lock for annotation purposes.
    #[inline]
    pub fn lock(&self) -> &VmAspaceMutex {
        self.state().aspace.lock()
    }

    /// Expose our backing lock for annotation purposes.
    #[inline]
    pub fn region_lock(&self) -> &VmAspaceRegionMutex {
        self.state().aspace.region_lock()
    }

    pub(super) fn lock_is_held(&self) -> bool {
        // SAFETY: `self.raw.get()` is a valid `VmAddressRegionOrMapping` pointer.
        unsafe { bindings::cpp_vm_address_region_or_mapping_lock_is_held(self.raw.get()) }
    }

    pub(super) fn region_lock_is_held(&self) -> bool {
        // SAFETY: `self.raw.get()` is a valid `VmAddressRegionOrMapping` pointer.
        unsafe { bindings::cpp_vm_address_region_or_mapping_region_lock_is_held(self.raw.get()) }
    }

    pub fn is_in_range(&self, base: VAddr, size: usize) -> bool {
        let offset = base.0.wrapping_sub(self.base().0);
        base.0 >= self.base().0 && offset < self.size() && self.size() - offset >= size
    }

    /// Returns true if the instance is alive and reporting information that
    /// reflects the address space layout. `aspace().lock()` must be held.
    pub fn is_alive_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> bool {
        self.state().canary.assert();
        self.state_locked(token) == LifeCycleState::Alive
    }

    /// Check if the given `arch_mmu_flags` are allowed under this
    /// region's `flags()`
    pub fn is_valid_mapping_flags(&self, arch_mmu_flags: ArchMmuFlags) -> bool {
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

    fn destroy_locked(
        this: RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        match Self::into_downcast(this) {
            OwnedRegionOrMapping::Mapping(mapping) => {
                VmMapping::destroy_locked(mapping, region_token, aspace_token)
            }
            OwnedRegionOrMapping::Region(region) => {
                VmAddressRegion::destroy_locked(region, region_token, aspace_token)
            }
        }
    }

    /// Transition from `NotReady` to `Alive`, and add references to `self` to related
    /// structures.
    /// On error no state is changed and no references are added.
    ///
    /// # Safety
    ///
    /// No child of the parent has the same base as `self`, and `self` is kept alive by another
    /// owning reference for the duration of the call.
    unsafe fn activate(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        match self.downcast() {
            RegionOrMapping::Mapping(mapping) => {
                // SAFETY: No child of the parent has the same base as `mapping`, and `mapping` is
                // kept alive by another owning reference for the duration of the call.
                unsafe { VmMapping::activate(mapping, region_token, aspace_token) }
            }
            RegionOrMapping::Region(region) => {
                VmAddressRegion::activate(region, region_token, aspace_token)
            }
        }
    }

    /// Dump debug info.
    fn dump_locked(
        &self,
        region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        token: &LockToken<'_, VmAspaceLockClass>,
        depth: u32,
        verbose: bool,
    ) {
        match self.downcast() {
            RegionOrMapping::Mapping(mapping) => {
                VmMapping::dump_locked(mapping, region_token, token, depth, verbose)
            }
            RegionOrMapping::Region(region) => {
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
    fn commit_high_memory_priority(&self) {
        match self.downcast() {
            RegionOrMapping::Mapping(mapping) => VmMapping::commit_high_memory_priority(mapping),
            RegionOrMapping::Region(region) => VmAddressRegion::commit_high_memory_priority(region),
        }
    }

    /// If a VMO-mapping, unmap all pages and remove dependency on `VmObject` it has a ref to.
    /// Otherwise recursively destroy child VMARs and transition to the `Dead` state.
    ///
    /// Returns `Ok(())` on success, `Err(Status::BAD_STATE)` if already dead, and other
    /// values on error (typically unmap failure).
    pub fn destroy(&self) -> Result<(), Status> {
        self.state().canary.assert();

        ksync::lock!(let mut region_guard = self.region_lock().lock());
        ksync::lock!(let mut guard = self.lock().lock());
        if self.state_locked(guard.token()) != LifeCycleState::Alive {
            return Err(Status::BAD_STATE);
        }

        Self::destroy_locked(
            RefPtr::from_ref(self),
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
        )
    }

    /// destructor, should only be invoked from `fbl_recycle`
    fn destruct(&self, token: &LockToken<'_, VmAspaceLockClass>) {
        ltracef!("{:p}\n", self);

        assert!(self.state_locked(token) != LifeCycleState::Alive);

        debug_assert!(self.memory_priority_locked(token) == MemoryPriority::Default);
    }

    #[inline]
    pub(super) fn state_locked(&self, _token: &LockToken<'_, VmAspaceLockClass>) -> LifeCycleState {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().life_cycle.get() }
    }

    #[inline]
    fn state_locked_region(
        &self,
        _token: &LockToken<'_, VmAspaceRegionLockClass>,
    ) -> LifeCycleState {
        // SAFETY: The region lock is held.
        unsafe { *self.state().life_cycle.get() }
    }

    #[inline]
    pub(super) fn set_state_locked(
        &self,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
        state: LifeCycleState,
    ) {
        // SAFETY: Both guarding locks are held.
        unsafe {
            *self.state().life_cycle.get() = state;
        }
    }

    #[inline]
    pub(super) fn memory_priority_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
    ) -> MemoryPriority {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().memory_priority.get(token) }
    }

    #[inline]
    pub(super) fn set_memory_priority_field_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().memory_priority.get_mut(token) } = priority;
    }

    pub fn parent_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> *mut VmAddressRegion {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().parent.get(token) }
    }

    #[inline]
    pub(super) fn set_parent_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        parent: *mut VmAddressRegion,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().parent.get_mut(token) } = parent;
    }

    /// # Safety
    ///
    /// `ptr` must point to a live region or mapping allocated with `kalloc::Box` to which no
    /// reference remains.
    #[inline]
    unsafe fn free(ptr: NonNull<Self>) {
        let raw = ptr.as_ptr().cast::<bindings::VmAddressRegionOrMapping>();
        // SAFETY: `ptr` points to a live region or mapping.
        if unsafe { ptr.as_ref() }.is_mapping() {
            // SAFETY: `raw` points to a live `VmMapping`.
            let mapping = unsafe { bindings::cpp_vm_address_region_or_mapping_as_vm_mapping(raw) };
            // SAFETY: `mapping` is non-null and points to a live `VmMapping` allocated with
            // `kalloc::Box` to which no reference remains.
            unsafe { VmMapping::recycle(NonNull::new_unchecked(mapping.cast())) }
        } else {
            // SAFETY: `raw` points to a live `VmAddressRegion`.
            let region =
                unsafe { bindings::cpp_vm_address_region_or_mapping_as_vm_address_region(raw) };
            // SAFETY: `region` is non-null and points to a live `VmAddressRegion` allocated with
            // `kalloc::Box` to which no reference remains.
            unsafe { VmAddressRegion::recycle(NonNull::new_unchecked(region.cast())) }
        }
    }
}

impl VmMapping {
    pub(super) fn is_alive_locked_object(&self, token: &LockToken<'_, VmObjectLockClass>) -> bool {
        (**self).state().canary.assert();
        self.state_locked_object(token) == LifeCycleState::Alive
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
        unsafe { *(**self).state().life_cycle.get() }
    }
}

enum RegionOrMapping<'a> {
    Region(&'a VmAddressRegion),
    Mapping(&'a VmMapping),
}

enum OwnedRegionOrMapping {
    Region(RefPtr<VmAddressRegion>),
    Mapping(RefPtr<VmMapping>),
}

/// A representation of a contiguous range of virtual address space
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct VmAddressRegion {
    #[pin]
    raw: Opaque<bindings::VmAddressRegion>,
}

unsafe_pinned_drop_ffi!(VmAddressRegion, bindings::cpp_vm_address_region_destruct);

#[repr(C)]
pub struct VmAddressRegionState {
    canary: Canary<{ fbl::magic(b"VMAR") }>,
    name: [u8; ZX_MAX_NAME_LEN],
}

::zr::static_assert_size_and_align!(
    VmAddressRegionState,
    constants::kVmAddressRegionStateSize,
    constants::kVmAddressRegionStateAlign,
);

impl VmAddressRegionState {
    fn new(name: &CStr) -> Self {
        Self { canary: Canary::new(), name: name_to_c_buf(name.to_bytes()) }
    }
}

impl HasRefCount for VmAddressRegion {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
    }
}

impl Deref for VmAddressRegion {
    type Target = VmAddressRegionOrMapping;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `VmAddressRegionOrMapping` is the first base of `VmAddressRegion`.
        unsafe { &*core::ptr::from_ref(self).cast() }
    }
}

// SAFETY: `VmAddressRegion` is not tied to the thread that created it.
unsafe impl Send for VmAddressRegion {}
// SAFETY: Mutable state of `VmAddressRegion` is only accessed with the aspace lock held.
unsafe impl Sync for VmAddressRegion {}

impl VmAddressRegion {
    fn new_root(
        aspace: &VmAspace,
        base: usize,
        size: usize,
        vmar_flags: u32,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_address_region_construct_root,
            aspace.as_ffi_ptr().cast(),
            base,
            size,
            vmar_flags
        )
    }

    /// Constructor for use in creating the kernel aspace singleton.
    ///
    /// # Safety
    ///
    /// `kernel_aspace` is the kernel aspace, and this is its only root region.
    pub(super) unsafe fn new_kernel_root(kernel_aspace: &VmAspace) -> impl PinInit<Self> {
        let kernel_aspace = kernel_aspace.as_ffi_ptr().cast();
        pin_init_ffi!(bindings::cpp_vm_address_region_construct_kernel_root, kernel_aspace)
    }

    pub(super) fn new_child(
        parent: &VmAddressRegion,
        base: usize,
        size: usize,
        vmar_flags: u32,
        name: &CStr,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_address_region_construct_child,
            parent.as_ffi_ptr(),
            base,
            size,
            vmar_flags,
            name.as_ptr(),
        )
    }

    pub(super) fn as_ffi_ptr(&self) -> *mut bindings::VmAddressRegion {
        self.raw.get()
    }

    fn state(&self) -> &VmAddressRegionState {
        // SAFETY: `self` points to a live `VmAddressRegion`, whose state is initialized.
        unsafe { &*bindings::cpp_vm_address_region_state(self.as_ffi_ptr()).cast() }
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
    /// To avoid leaks, this should be paired with a call to
    /// [`VmAddressRegionOrMapping::destroy`] if desired; dropping `MapResult::mapping` will not
    /// destroy the mapping.
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
        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::VmarsAndMappings,
            self,
            0,
            usize::MAX,
            token,
        ));
        debug_assert!(self.lock_is_held());
        while let Some((next, _)) = enumerator.as_mut().next(token) {
            let next = ptr::from_ref(next);
            // SAFETY: The aspace lock (`token`) remains held and `subregions_` is not modified in
            // this loop, so `next` stays live for the duration of the iteration.
            let next = unsafe { &*next };
            if let Some(map) = next.as_vm_mapping_ptr() {
                debug_assert!(map.lock_is_held());
                map.set_memory_priority_locked::<false>(token, priority);
            } else {
                set_region_priority(next.as_vm_address_region_ptr().unwrap(), token);
            }
        }
        Ok(())
    }

    fn destroy_locked(
        this: RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        this.state().canary.assert();
        ltracef!("{:p} '{}'\n", &**this, zr::from_utf8_lossy(this.name().to_bytes()));

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
                match child.downcast() {
                    RegionOrMapping::Mapping(mapping) => {
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
                    RegionOrMapping::Region(region) => {
                        child.state().canary.assert();
                        child_region = Some(RefPtr::from_ref(region));
                    }
                }
            }

            if child_region.is_some() {
                // If we found a child region, traverse down the tree.
                cur = child_region;
            } else {
                // All children are destroyed, so now destroy the current node.
                // SAFETY: `parent_locked` is either null or points to a live parent
                // `VmAddressRegion`.
                let parent = unsafe { current.parent_locked(aspace_token).as_ref() };
                if let Some(parent) = parent {
                    debug_assert!(parent.lock_is_held());
                    debug_assert!(parent.region_lock_is_held());
                    // SAFETY: `current` is a child of `parent` and is kept alive by another owning
                    // reference for the duration of the call.
                    unsafe {
                        region_list::remove_region(parent, region_token, aspace_token, &current)
                    };
                }
                current.set_state_locked(region_token, aspace_token, LifeCycleState::Dead);
                current.set_parent_locked(aspace_token, ptr::null_mut());

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
        CStr::from_bytes_until_nul(&self.state().name).expect("name is null-terminated")
    }

    /// Returns true if this region has a parent region.
    pub fn has_parent(&self) -> bool {
        ksync::lock!(let guard = self.lock().lock());
        !self.parent_locked(guard.token()).is_null()
    }

    /// Finds the child region that contains the given `addr`. If `addr` is in a gap,
    /// returns `None`. This is a non-recursive search.
    pub fn find_region(&self, addr: VAddr) -> Option<RefPtr<VmAddressRegionOrMapping>> {
        ksync::lock!(let guard = self.lock().lock());
        self.find_region_locked(guard.token(), addr)
    }

    pub fn find_region_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        addr: VAddr,
    ) -> Option<RefPtr<VmAddressRegionOrMapping>> {
        if self.state_locked(token) != LifeCycleState::Alive {
            return None;
        }
        region_list::find_region(self, token, addr).map(RefPtr::from_ref)
    }

    /// Recursively compute the amount of attributed memory within this region.
    pub fn get_attributed_memory(&self) -> AttributionCounts {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.lock().lock());
        if !self.is_alive_locked(guard.token()) {
            return AttributionCounts::default();
        }

        let mut page_counts = AttributionCounts::default();

        // Enumerate all of the subregions below us & count allocated pages.
        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::MappingsOnly,
            self,
            0,
            usize::MAX,
            guard.token(),
        ));
        debug_assert!(self.lock_is_held());
        while let Some((next, _)) = enumerator.as_mut().next(guard.token()) {
            if let Some(map) = next.as_vm_mapping() {
                debug_assert!(map.lock_is_held());
                enumerator.as_mut().pause(guard.token());
                page_counts += map.get_attributed_memory_locked(guard.as_mut());
                drop(map);
                enumerator.as_mut().resume(guard.token());
                if !self.is_alive_locked(guard.token()) {
                    break;
                }
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
        self.state().canary.assert();

        let mut vmar = self;
        debug_assert!(vmar.lock_is_held());
        loop {
            let next = region_list::find_region(vmar, token, va)?;
            if let Some(mapping) = next.as_vm_mapping_ptr() {
                return Some(mapping);
            }
            vmar = next.as_vm_address_region_ptr().unwrap();
        }
    }

    /// Transition from `NotReady` to `Alive`, and add references to `self` to related
    /// structures.
    /// On error no state is changed and no references are added.
    fn activate(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        debug_assert!(self.state_locked(aspace_token) == LifeCycleState::NotReady);

        let parent_ptr = self.parent_locked(aspace_token);
        // SAFETY: `parent_ptr` is valid for reads.
        let parent = unsafe { parent_ptr.as_ref() }.unwrap();
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
            assert!(candidate.base().0 >= self.base().0.wrapping_add(self.size()));
        }

        // SAFETY: No child of `parent` has the same base as `self`.
        unsafe { region_list::insert_region(parent, region_token, aspace_token, self)? };
        self.set_state_locked(region_token, aspace_token, LifeCycleState::Alive);
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
        self.state().canary.assert();
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
        self.state().canary.assert();

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

        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::MappingsOnly,
            self,
            0,
            usize::MAX,
            guard.token(),
        ));
        debug_assert!(self.lock_is_held());
        while let Some((region_or_mapping, _)) = enumerator.as_mut().next(guard.token()) {
            // Presently we hold the lock, so we know that `region_or_mapping` is valid, but we want
            // to use this outside of the lock later on, and so we must upgrade it to a `RefPtr`.
            let Some(mapping) = region_or_mapping.as_vm_mapping() else {
                continue;
            };
            enumerator.as_mut().pause(guard.token());
            guard.as_mut().call_unlocked(move || {
                mapping.commit_high_memory_priority();
                drop(mapping);
            });
            // Since the lock was dropped we must re-validate before doing anything else.
            if !validate(guard.token()) {
                return;
            }
            enumerator.as_mut().resume(guard.token());
        }
    }

    /// Applies the given memory priority to this region and all subregions.
    pub fn set_memory_priority(&self, priority: MemoryPriority) -> Result<(), Status> {
        self.state().canary.assert();
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
    /// Caller must ensure the specified virtual address region to unmap is no longer used.
    pub unsafe fn unmap(
        &self,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        self.state().canary.assert();

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

        // SAFETY: `size` is not zero, `base` and `size` are page aligned, and the virtual
        // address range `[base, base + size)` is no longer used.
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

    /// Implementation for `unmap()` and the C++ `OverwriteVmMappingLocked()` that does not acquire
    /// the region and aspace locks. If `can_destroy_regions` is true, then this may destroy
    /// VMARs that it completely covers.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, and the virtual address range
    /// `[base, base + size)` is no longer used.
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
                if let Err(status) = VmAddressRegionOrMapping::destroy_locked(
                    RefPtr::from_ref(curr_region),
                    region_token,
                    aspace_token,
                ) {
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
                // aligned, and `[unmap_base, unmap_base + unmap_size)` is a strict subrange of
                // `mapping` that may be unmapped.
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
    /// The virtual address range `[base, base + size)` may have its permissions changed and be
    /// unmapped.
    pub unsafe fn protect(
        &self,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        self.state().canary.assert();

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
            pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
                VmAddressRegionEnumeratorType::MappingsOnly,
                self,
                base.0,
                end_addr_byte,
                token,
            ));
            let mut expected = base.0;
            while let Some((entry, _)) = enumerator.as_mut().next(token) {
                let mapping = entry.as_vm_mapping_ptr();
                debug_assert!(mapping.is_some());
                let mapping = mapping.unwrap();
                debug_assert!(mapping.lock_is_held());
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
                if ptr::eq(self.aspace().vdso_code_mapping_locked(token), mapping) {
                    hint::cold_path();
                    return Err(Status::ACCESS_DENIED);
                }
                if !ptr::eq(mapping.parent_locked(token), self) {
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

        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::MappingsOnly,
            self,
            base.0,
            end_addr_byte,
            token,
        ));
        while let Some((entry, _)) = enumerator.as_mut().next(token) {
            let mapping = entry.as_vm_mapping_ptr();
            debug_assert!(mapping.is_some());
            let mapping = ptr::from_ref(mapping.unwrap());
            // SAFETY: The aspace lock (`token`) remains held and `subregions_` is not modified in
            // this loop, so `mapping` stays live for the duration of the iteration.
            let mapping = unsafe { &*mapping };

            // The last byte of the current region.
            debug_assert!(mapping.lock_is_held());
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
            enumerator.as_mut().pause(token);
            // SAFETY: `protect_size` is not zero, `protect_base` and `protect_size` are page
            // aligned, `[protect_base, protect_base + protect_size)` is within `mapping` and may
            // have its permissions changed and be unmapped, and `new_arch_mmu_flags` has no cache
            // flags and is valid for `mapping`.
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
            enumerator.as_mut().resume(token);
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
        self.state().canary.assert();
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

        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::MappingsOnly,
            self,
            base.0,
            last_addr,
            guard.token(),
        ));
        let mut expected = base.0;
        while let Some((region_or_mapping, _)) = enumerator.as_mut().next(guard.token()) {
            // Presently we hold the lock, so we know that `region_or_mapping` is valid, but we want
            // to use this outside of the lock later on, and so we must upgrade it to a `RefPtr`.
            let mapping = region_or_mapping.as_vm_mapping().expect("mapping must be set");

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

            enumerator.as_mut().pause(guard.token());
            // The commit, decommit and prefetch ops check the maximal permissions of the mapping
            // and can be thought of as acting as if they perform a protect to add read or write
            // permissions. Since protect to add permissions through a parent VMAR is not valid we
            // similarly forbid this notional protect by not allowing these operations if acting
            // through a sub-VMAR, regardless of whether `op_children` is otherwise allowed.
            if (matches!(op, RangeOpType::Commit | RangeOpType::Decommit | RangeOpType::Prefetch)
                || op_children == VmAddressRegionOpChildren::No)
                && !ptr::eq(mapping.parent_locked(guard.token()), self)
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
            enumerator.as_mut().resume(guard.token());

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

    /// Traverses this vmar (and any sub-vmars) starting at this node, in depth-first pre-order. See
    /// `VmEnumerator` for more details. If this vmar is not alive (in the `LifeCycleState` sense)
    /// or otherwise not enumerable this returns `Err(Status::BAD_STATE)`, otherwise the result of
    /// enumeration is returned.
    pub fn enumerate_children<E: VmEnumerator + ?Sized>(&self, ve: &mut E) -> Result<(), Status> {
        self.state().canary.assert();
        let self_ref = RefPtr::from_ref(self);
        ksync::lock!(let mut guard = self.lock().lock());
        if self.state_locked(guard.token()) != LifeCycleState::Alive {
            return Err(Status::BAD_STATE);
        }
        match ve.on_vm_address_region(&self_ref, 0, guard.as_mut()) {
            Status::NEXT => {}
            Status::STOP => return Ok(()),
            status => return Err(status),
        }
        pin_init::stack_pin_init!(let enumerator = VmAddressRegionEnumerator::new(
            VmAddressRegionEnumeratorType::VmarsAndMappings,
            &self_ref,
            0,
            usize::MAX,
            guard.token(),
        ));
        while let Some((region_or_mapping, depth)) = enumerator.as_mut().next(guard.token()) {
            enumerator.as_mut().pause(guard.token());
            let status = if let Some(mapping) = region_or_mapping.as_vm_mapping() {
                ve.on_vm_mapping(&mapping, &self_ref, depth, guard.as_mut())
            } else {
                let vmar = region_or_mapping
                    .as_vm_address_region()
                    .expect("region_or_mapping must be a VmAddressRegion if not a VmMapping");
                ve.on_vm_address_region(&vmar, depth, guard.as_mut())
            };
            match status {
                Status::NEXT => {}
                Status::STOP => return Ok(()),
                status => return Err(status),
            }
            enumerator.as_mut().resume(guard.token());
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
pub(super) mod region_list {
    use super::super::vm_aspace::{VmAspaceLockClass, VmAspaceRegionLockClass};
    use super::{VmAddressRegion, VmAddressRegionOrMapping, bindings};
    use crate::kernel::types::VAddr;
    use core::ffi::c_void;
    use core::marker::PhantomData;
    use ksync::LockToken;
    use zr::Opaque;
    use zx_status::Status;

    /// A cursor into a VMAR's region list, valid while the aspace lock is held for `'a`.
    pub(in crate::vm) struct Cursor<'a> {
        raw: Opaque<bindings::VmAddressRegionSubregionsCursor>,
        _vmar: PhantomData<&'a VmAddressRegion>,
    }

    impl<'a> Cursor<'a> {
        /// # Safety
        ///
        /// `init` initializes the cursor it is passed to a cursor into the region list of a VMAR,
        /// and the VMAR is live and its region list is not modified for `'a`.
        #[inline]
        unsafe fn new(init: impl FnOnce(*mut bindings::VmAddressRegionSubregionsCursor)) -> Self {
            let raw = Opaque::uninit();
            init(raw.get());
            Self { raw, _vmar: PhantomData }
        }

        #[inline]
        fn is_valid(&self) -> bool {
            // SAFETY: `self.raw` is a cursor into a live region list, and is not stale.
            unsafe { bindings::cpp_vm_address_region_subregions_cursor_is_valid(self.raw.get()) }
        }

        #[inline]
        pub(in crate::vm) fn get(&self) -> Option<&'a VmAddressRegionOrMapping> {
            if !self.is_valid() {
                return None;
            }
            // SAFETY: `self.raw` is a valid cursor into a live region list, and is not stale.
            let raw =
                unsafe { bindings::cpp_vm_address_region_subregions_cursor_get(self.raw.get()) };
            // SAFETY: `raw` points to a child of the VMAR that is live for `'a`.
            Some(unsafe { &*raw.cast::<VmAddressRegionOrMapping>() })
        }

        /// Advance to the next region in address order.
        #[inline]
        pub(in crate::vm) fn next(&mut self) {
            // SAFETY: `self.raw` is a cursor into a live region list, and is not stale.
            unsafe { bindings::cpp_vm_address_region_subregions_cursor_next(self.raw.get()) }
        }
    }

    /// Find the region that covers `addr`, returns `None` if not found.
    #[inline]
    pub(in crate::vm) fn find_region<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        addr: VAddr,
    ) -> Option<&'a VmAddressRegionOrMapping> {
        // SAFETY: `vmar` is live and the aspace lock is held.
        let raw = unsafe {
            bindings::cpp_vm_address_region_subregions_find_region(vmar.as_ffi_ptr(), addr.0)
        };
        // SAFETY: `raw` is null or points to a child of `vmar` that is live for `'a`.
        unsafe { raw.cast::<VmAddressRegionOrMapping>().as_ref() }
    }

    /// Find the region that contains `base`, or if that doesn't exist, the first region that
    /// contains an address greater than `base`.
    #[inline]
    pub(in crate::vm) fn include_or_higher<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        base: VAddr,
    ) -> Cursor<'a> {
        // SAFETY: `vmar` is live, the aspace lock is held for `'a`, `out` is valid for writes, and
        // the closure initializes `out` to a cursor into the region list of `vmar`.
        unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_include_or_higher(
                    vmar.as_ffi_ptr(),
                    base.0,
                    out.cast(),
                )
            })
        }
    }

    /// Insert `region` to the region list. On failure the region list is unmodified.
    ///
    /// # Safety
    ///
    /// No child of `vmar` has the same base as `region`.
    #[inline]
    pub(in crate::vm) unsafe fn insert_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        region: &VmAddressRegionOrMapping,
    ) -> Result<(), Status> {
        // SAFETY: `vmar` and `region` are live, no child of `vmar` has the same base as `region`,
        // and the region and aspace locks are held.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_subregions_insert_region(
                vmar.as_ffi_ptr(),
                region.raw.get(),
            )
        })
    }

    /// Remove `region` from the list.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar` and is kept alive by another owning reference for the
    /// duration of the call.
    #[inline]
    pub(in crate::vm) unsafe fn remove_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        region: &VmAddressRegionOrMapping,
    ) {
        // SAFETY: `vmar` is live, `region` is a child of `vmar` that is kept alive by another
        // owning reference for the duration of the call, and the region and aspace locks are
        // held.
        unsafe {
            bindings::cpp_vm_address_region_subregions_remove_region(
                vmar.as_ffi_ptr(),
                region.raw.get(),
            )
        }
    }

    /// Replaces the target region with a new region at the same address. Unlike insertion this
    /// cannot fail.
    ///
    /// # Safety
    ///
    /// `prev` is a child of `vmar` and is kept alive by another owning reference for the duration
    /// of the call, and `region` has the same base as `prev` and does not overlap with any other
    /// child of `vmar`.
    #[inline]
    pub(in crate::vm) unsafe fn replace_region(
        vmar: &VmAddressRegion,
        _region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        prev: &VmAddressRegionOrMapping,
        region: &VmAddressRegionOrMapping,
    ) {
        // SAFETY: `vmar` and `region` are live, `prev` is a child of `vmar` that is kept alive by
        // another owning reference for the duration of the call, `region` has the same base as
        // `prev` and does not overlap with any other child of `vmar`, and the region and aspace
        // locks are held.
        unsafe {
            bindings::cpp_vm_address_region_subregions_replace_region(
                vmar.as_ffi_ptr(),
                prev.raw.get(),
                region.raw.get(),
            )
        }
    }

    /// Request the region to the left of the given region.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar`.
    #[inline]
    pub(in crate::vm) unsafe fn left_of<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        region: &VmAddressRegionOrMapping,
    ) -> Option<&'a VmAddressRegionOrMapping> {
        // SAFETY: `vmar` is live and its region list is not modified for `'a`, `region` is a child
        // of `vmar`, the aspace lock is held for `'a`, `out` is valid for writes, and the closure
        // initializes `out` to a cursor into the region list of `vmar`.
        let cursor = unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_left_of(
                    vmar.as_ffi_ptr(),
                    region.raw.get(),
                    out.cast(),
                )
            })
        };
        cursor.get()
    }

    /// Request the region to the right of the given region.
    ///
    /// # Safety
    ///
    /// `region` is a child of `vmar`.
    #[inline]
    pub(in crate::vm) unsafe fn right_of<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
        region: &VmAddressRegionOrMapping,
    ) -> Option<&'a VmAddressRegionOrMapping> {
        // SAFETY: `vmar` is live and its region list is not modified for `'a`, `region` is a child
        // of `vmar`, the aspace lock is held for `'a`, `out` is valid for writes, and the closure
        // initializes `out` to a cursor into the region list of `vmar`.
        let cursor = unsafe {
            Cursor::new(|out| {
                bindings::cpp_vm_address_region_subregions_right_of(
                    vmar.as_ffi_ptr(),
                    region.raw.get(),
                    out.cast(),
                )
            })
        };
        cursor.get()
    }

    /// Returns whether the region list is empty.
    #[inline]
    pub(in crate::vm) fn is_empty(
        vmar: &VmAddressRegion,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> bool {
        // SAFETY: `vmar` is live and the aspace lock is held.
        unsafe { bindings::cpp_vm_address_region_subregions_is_empty(vmar.as_ffi_ptr()) }
    }

    /// Returns the first element of the list.
    ///
    /// # Safety
    ///
    /// The list is not empty.
    #[inline]
    pub(in crate::vm) unsafe fn front<'a>(
        vmar: &'a VmAddressRegion,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> &'a VmAddressRegionOrMapping {
        // SAFETY: `vmar` is live, its list is not empty and the aspace lock is held.
        unsafe {
            &*bindings::cpp_vm_address_region_subregions_front(vmar.as_ffi_ptr())
                .cast::<VmAddressRegionOrMapping>()
        }
    }

    #[inline]
    pub(in crate::vm) fn size_slow(
        vmar: &VmAddressRegion,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> usize {
        // SAFETY: `vmar` is live and the aspace lock is held.
        unsafe { bindings::cpp_vm_address_region_subregions_size_slow(vmar.as_ffi_ptr()) }
    }

    #[inline]
    pub(in crate::vm) fn for_each<F: FnMut(&VmAddressRegionOrMapping)>(
        vmar: &VmAddressRegion,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        _token: &LockToken<'_, VmAspaceLockClass>,
        mut f: F,
    ) {
        /// # Safety
        ///
        /// `child` points to a live `VmAddressRegionOrMapping` and `f` points to an `F`.
        #[inline]
        unsafe extern "C" fn call<F: FnMut(&VmAddressRegionOrMapping)>(
            child: *mut bindings::VmAddressRegionOrMapping,
            f: *mut c_void,
        ) {
            // SAFETY: `f` points to a valid `F` and is exclusively referenced for the call, and
            // `child` points to a live `VmAddressRegionOrMapping`.
            unsafe { (*f.cast::<F>())(&*child.cast::<VmAddressRegionOrMapping>()) }
        }

        // SAFETY: `vmar` is live, the region and aspace locks are held, and `f` outlives the call.
        unsafe {
            bindings::cpp_vm_address_region_subregions_for_each(
                vmar.as_ffi_ptr(),
                Some(call::<F>),
                (&raw mut f).cast(),
            )
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_state_init(
    storage: &mut MaybeUninit<VmAddressRegionOrMappingState>,
    base: usize,
    size: usize,
    flags: u32,
    aspace: &VmAspace,
    parent: *mut VmAddressRegion,
    is_mapping: bool,
) {
    storage.write(VmAddressRegionOrMappingState::new(
        base,
        size,
        flags,
        aspace,
        parent,
        is_mapping,
        LifeCycleState::NotReady,
    ));
}

/// # Safety
///
/// `this` points to a `VmAddressRegionOrMapping` whose state is initialized, `this` is not accessed
/// concurrently, and the state is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_destruct(
    this: &VmAddressRegionOrMapping,
) {
    // SAFETY: `this` is not accessed concurrently.
    let token = unsafe { LockToken::new() };
    this.destruct(&token);
    // SAFETY: The state is initialized and is not used afterwards.
    unsafe {
        ptr::drop_in_place(
            bindings::cpp_vm_address_region_or_mapping_state(this.raw.get())
                .cast::<VmAddressRegionOrMappingState>(),
        )
    };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_base(this: &VmAddressRegionOrMapping) -> usize {
    this.base().0
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_size(this: &VmAddressRegionOrMapping) -> usize {
    this.size()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_flags(this: &VmAddressRegionOrMapping) -> u32 {
    this.flags()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_aspace(
    this: &VmAddressRegionOrMapping,
) -> &RefPtr<VmAspace> {
    this.aspace()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_is_mapping(
    this: &VmAddressRegionOrMapping,
) -> bool {
    this.is_mapping()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_ref_counted(
    this: &VmAddressRegionOrMapping,
) -> *const RefCounted {
    this.ref_count()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_assert_canary(this: &VmAddressRegionOrMapping) {
    this.state().canary.assert();
}

/// # Safety
///
/// The aspace lock is held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_state_locked(
    this: &VmAddressRegionOrMapping,
) -> LifeCycleState {
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let token = unsafe { LockToken::new() };
    this.state_locked(&token)
}

/// # Safety
///
/// The region lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_state_locked_region(
    this: &VmAddressRegionOrMapping,
) -> LifeCycleState {
    // SAFETY: The region lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.state_locked_region(&token)
}

/// # Safety
///
/// The region lock and the aspace lock are held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_set_state_locked(
    this: &VmAddressRegionOrMapping,
    state: LifeCycleState,
) {
    // SAFETY: The region lock is held by the caller, or no other reference to `this` exists.
    let region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let aspace_token = unsafe { LockToken::new() };
    this.set_state_locked(&region_token, &aspace_token, state);
}

/// # Safety
///
/// The aspace lock is held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_memory_priority_locked(
    this: &VmAddressRegionOrMapping,
) -> MemoryPriority {
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let token = unsafe { LockToken::new() };
    this.memory_priority_locked(&token)
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_parent_locked(
    this: &VmAddressRegionOrMapping,
) -> *mut VmAddressRegion {
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.parent_locked(&token)
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_set_parent_locked(
    this: &VmAddressRegionOrMapping,
    parent: *mut VmAddressRegion,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_parent_locked(&mut token, parent);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_is_in_range(
    this: &VmAddressRegionOrMapping,
    base: usize,
    size: usize,
) -> bool {
    this.is_in_range(VAddr(base), size)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_is_valid_mapping_flags(
    this: &VmAddressRegionOrMapping,
    arch_mmu_flags: ArchMmuFlags,
) -> bool {
    this.is_valid_mapping_flags(arch_mmu_flags)
}

/// # Safety
///
/// `name` points to a null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_state_init(
    storage: &mut MaybeUninit<VmAddressRegionState>,
    name: *const c_char,
) {
    // SAFETY: `name` points to a null-terminated string.
    let name = unsafe { CStr::from_ptr(name) };
    storage.write(VmAddressRegionState::new(name));
}

/// # Safety
///
/// `storage` is initialized and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_destruct(
    storage: &mut MaybeUninit<VmAddressRegionState>,
) {
    // SAFETY: `storage` is initialized and valid for drop.
    unsafe { storage.assume_init_drop() }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_name(this: &VmAddressRegion) -> *const c_char {
    this.state().name.as_ptr().cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_assert_canary(this: &VmAddressRegion) {
    this.state().canary.assert();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_has_parent(this: &VmAddressRegion) -> bool {
    this.has_parent()
}

/// # Safety
///
/// The specified virtual address range to unmap is no longer used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_unmap(
    this: &VmAddressRegion,
    base: usize,
    size: usize,
    op_children: VmAddressRegionOpChildren,
) -> zx_status_t {
    // SAFETY: The specified virtual address range to unmap is no longer used.
    Status::result_into_raw(unsafe { this.unmap(VAddr(base), size, op_children) })
}

/// # Safety
///
/// The region lock and the aspace lock are held and the object lock is not held. `size` is not
/// zero, `base` and `size` are page aligned, and the virtual address range
/// `[base, base + size)` is no longer used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_unmap_internal_locked(
    this: &VmAddressRegion,
    base: usize,
    size: usize,
    can_destroy_regions: bool,
) -> zx_status_t {
    // SAFETY: The region lock is held.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: `size` is not zero, `base` and `size` are page aligned, and the virtual address
    // range `[base, base + size)` is no longer used.
    Status::result_into_raw(unsafe {
        this.unmap_internal_locked(
            &mut region_token,
            &mut aspace_token,
            VAddr(base),
            size,
            can_destroy_regions,
        )
    })
}

/// # Safety
///
/// The virtual address range `[base, base + size)` may have its permissions changed and be
/// unmapped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_protect(
    this: &VmAddressRegion,
    base: usize,
    size: usize,
    new_arch_mmu_flags: ArchMmuFlags,
    op_children: VmAddressRegionOpChildren,
) -> zx_status_t {
    // SAFETY: The virtual address range `[base, base + size)` may have its permissions changed and
    // be unmapped.
    Status::result_into_raw(unsafe {
        this.protect(VAddr(base), size, new_arch_mmu_flags, op_children)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_find_region(
    this: &VmAddressRegion,
    addr: usize,
) -> *mut bindings::VmAddressRegionOrMapping {
    match this.find_region(VAddr(addr)) {
        Some(region) => RefPtr::into_raw(region).cast_mut().cast(),
        None => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_get_attributed_memory(
    this: &VmAddressRegion,
    out_counts: &mut MaybeUninit<AttributionCounts>,
) {
    out_counts.write(this.get_attributed_memory());
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_find_mapping_locked(
    this: &VmAddressRegion,
    va: usize,
) -> *mut bindings::VmMapping {
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    match this.find_mapping_locked(&token, VAddr(va)) {
        Some(mapping) => core::ptr::from_ref(mapping).cast_mut().cast(),
        None => core::ptr::null_mut(),
    }
}

/// # Safety
///
/// `this` points to a live `VmAddressRegionOrMapping` and the caller holds a reference to it for
/// the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_destroy(
    this: *const VmAddressRegionOrMapping,
) -> zx_status_t {
    // SAFETY: `this` points to a live `VmAddressRegionOrMapping`.
    Status::result_into_raw(unsafe { &*this }.destroy())
}

/// # Safety
///
/// `this` points to a live `VmAddressRegionOrMapping`. The region lock and the aspace lock are held
/// and the object lock is not held, and the children of the parent are not being iterated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_destroy_locked(
    this: *const VmAddressRegionOrMapping,
) -> zx_status_t {
    // SAFETY: `this` points to a live `VmAddressRegionOrMapping`.
    let this = RefPtr::from_ref(unsafe { &*this });
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    Status::result_into_raw(VmAddressRegionOrMapping::destroy_locked(
        this,
        &mut region_token,
        &mut aspace_token,
    ))
}

/// # Safety
///
/// The region lock and the aspace lock are held and the object lock is not held, the children of
/// the parent are not being iterated, no child of the parent has the same base as `this`, and
/// `this` is kept alive by another owning reference for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_activate(
    this: &VmAddressRegionOrMapping,
) -> zx_status_t {
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: No child of the parent has the same base as `this`, and `this` is kept alive by
    // another owning reference for the duration of the call.
    Status::result_into_raw(unsafe { this.activate(&mut region_token, &mut aspace_token) })
}

/// # Safety
///
/// The region lock and the aspace lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_dump_locked(
    this: &VmAddressRegionOrMapping,
    depth: u32,
    verbose: bool,
) {
    // SAFETY: The region lock is held by the caller.
    let region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.dump_locked(&region_token, &token, depth, verbose);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_commit_high_memory_priority(
    this: &VmAddressRegionOrMapping,
) {
    this.commit_high_memory_priority();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_set_memory_priority(
    this: &VmAddressRegion,
    priority: MemoryPriority,
) -> zx_status_t {
    Status::result_into_raw(this.set_memory_priority(priority))
}

/// # Safety
///
/// The aspace lock is held and the object lock is not held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_set_memory_priority_locked(
    this: &VmAddressRegion,
    priority: MemoryPriority,
) -> zx_status_t {
    // SAFETY: The aspace lock is held.
    let mut token = unsafe { LockToken::new() };
    Status::result_into_raw(this.set_memory_priority_locked(&mut token, priority))
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_create_root(
    aspace: &VmAspace,
    base: usize,
    size: usize,
    vmar_flags: u32,
) -> Option<RefPtr<VmAddressRegion>> {
    RefPtr::pin_init(VmAddressRegion::new_root(aspace, base, size, vmar_flags)).ok()
}

/// # Safety
///
/// `name` points to a null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_create_child(
    parent: &VmAddressRegion,
    base: usize,
    size: usize,
    vmar_flags: u32,
    name: *const c_char,
) -> Option<RefPtr<VmAddressRegion>> {
    // SAFETY: `name` points to a null-terminated string.
    let name = unsafe { CStr::from_ptr(name) };
    RefPtr::pin_init(VmAddressRegion::new_child(parent, base, size, vmar_flags, name)).ok()
}

/// # Safety
///
/// `this` is the last reference to a region or mapping allocated with `kalloc::Box`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_recycle(
    this: NonNull<VmAddressRegionOrMapping>,
) {
    // SAFETY: `this` is the last reference to a region or mapping allocated with `kalloc::Box`.
    unsafe { VmAddressRegionOrMapping::recycle(this) }
}

/// # Safety
///
/// `this` is the last reference to a region allocated with `kalloc::Box`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_recycle(this: NonNull<VmAddressRegion>) {
    // SAFETY: `this` is the last reference to a region allocated with `kalloc::Box`.
    unsafe { VmAddressRegion::recycle(this) }
}
