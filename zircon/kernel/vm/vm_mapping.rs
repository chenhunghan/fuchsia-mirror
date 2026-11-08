// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHE_MASK, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_RWX_MASK, ARCH_MMU_FLAG_PERM_USER, ARCH_MMU_FLAG_PERM_WRITE,
    ARCH_UNMAP_OPTION_ENLARGE, ARCH_UNMAP_OPTION_HARVEST, ARCH_UNMAP_OPTION_NONE, ArchMmuFlags,
    ArchVmAspace, ExistingEntryAction,
};
use super::attribution::AttributionCounts;
use super::fault::{self, flag as pf_flag};
use super::page_source::MultiPageRequest;
use super::stream_size_manager::StreamSizeManager;
use super::vm::{vm_get_zero_page_paddr, vm_ktrace_duration};
use super::vm_address_region::{
    LifeCycleState, MemoryPriority, VmAddressRegion, VmAddressRegionOrMapping, VmAspaceGuard, flag,
    region_list,
};
use super::vm_aspace::{VmAspace, VmAspaceLockClass, VmAspaceRegionLockClass};
use super::vm_cow_pages::DeferredOps;
use super::vm_object::{Resizability, SnapshotType, VmObject, VmObjectLockClass};
use super::vm_object_paged::VmObjectPaged;
use super::vm_object_physical::VmObjectPhysical;
use crate::counters::define_kcounter;
use crate::kernel::types::{PAddr, VAddr};
use crate::ksync_kernel::CriticalMutexOrderedPolicy;
use crate::ktrace_rs::ArgValue;
use ::page as kernel_page;
use bitflags::bitflags;
use core::cell::{Cell, UnsafeCell};
use core::cmp::{max, min};
use core::hint;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{ltracef, tracef};
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use ksync::{KCell, LockToken};
use libarch::store_memory_barrier;
use pin_init::{PinInit, pin_data, stack_pin_init, stack_try_pin_init};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::zx_status_t;

define_kcounter!(VM_MAPPING_ATTRIBUTION_QUERIES, "vm.attributed_memory.mapping.queries", Sum);
define_kcounter!(VM_MAPPINGS_STATE_DEAD, "vm.aspace.mapping.state.dead", Sum);
define_kcounter!(VM_MAPPINGS_PROTECT_NO_WRITE, "vm.aspace.mapping.protect_without_write", Sum);
define_kcounter!(VM_MAPPINGS_MERGED, "vm.aspace.mapping.merged_neighbors", Sum);

const LOCAL_TRACE: u32 = 0;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    No = 0,
    Yes = 1,
}

zr::static_assert!(
    core::mem::size_of::<Mergeable>() == core::mem::size_of::<bindings::VmMapping_Mergeable>()
);
zr::static_assert!(Mergeable::No as u8 == bindings::VmMapping_Mergeable::NO as u8);
zr::static_assert!(Mergeable::Yes as u8 == bindings::VmMapping_Mergeable::YES as u8);

/// The MMU flags of a protection region and where it ends.
#[derive(Debug, Clone, Copy)]
pub struct FlagsRange {
    /// The MMU flags of the region.
    pub mmu_flags: ArchMmuFlags,
    /// The end of the region.
    pub region_top: u64,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UnmapOptions: u8 {
        const ONLY_HAS_ZERO_PAGES = 1 << 0;
        const HARVEST = 1 << 1;
    }
}

zr::static_assert!(UnmapOptions::empty().bits() == bindings::VmMapping_UnmapOptions::kNone as u8);
zr::static_assert!(
    UnmapOptions::ONLY_HAS_ZERO_PAGES.bits()
        == bindings::VmMapping_UnmapOptions::OnlyHasZeroPages as u8
);
zr::static_assert!(UnmapOptions::HARVEST.bits() == bindings::VmMapping_UnmapOptions::Harvest as u8);

/// Whether `VmMapping::destroy_locked_object` unmaps the mapping from the arch aspace.
#[derive(Copy, Clone, Eq, PartialEq)]
enum DestroyUnmap {
    No,
    Yes,
}

/// Whether `VmMapping::destroy_locked_object` removes the mapping from its parent's subregions.
#[derive(Copy, Clone, Eq, PartialEq)]
enum DestroyRemoveFromParent {
    No,
    Yes,
}

/// Whether `VmMapping::activate_locked` inserts the mapping into its parent's subregions and the
/// VMO's mapping list.
#[derive(Copy, Clone, Eq, PartialEq)]
enum ActivateInsertRegions {
    No,
    Yes,
}

/// A representation of the mapping of a VMO into the address space
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct VmMapping {
    #[pin]
    raw: Opaque<bindings::VmMapping>,
}

unsafe_pinned_drop_ffi!(VmMapping, bindings::cpp_vm_mapping_destruct);

/// A B-Tree that records any transitions in protection. Each entry consists of a `usize` key and
/// an `ArchMmuFlags` value. A node (addr, flags) means that the range starting at `addr` has the
/// protections `flags`, up until the address of the next node in the tree, or `base() + size()` if
/// it is the last node.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
struct ProtectionRanges {
    #[pin]
    raw: Opaque<bindings::VmMappingProtectionRanges>,
}

unsafe_pinned_drop_ffi!(ProtectionRanges, bindings::cpp_vm_mapping_protection_ranges_destroy);

impl ProtectionRanges {
    fn as_ffi_ptr(&self) -> *mut bindings::VmMappingProtectionRanges {
        self.raw.get()
    }

    #[inline]
    fn new() -> impl PinInit<Self> {
        pin_init_ffi!(bindings::cpp_vm_mapping_protection_ranges_construct)
    }

    /// # Safety
    ///
    /// `raw` points to a live `ProtectionRanges` that outlives `'a` and is not mutated during `'a`.
    #[inline]
    unsafe fn from_raw_ref<'a>(raw: *const bindings::VmMappingProtectionRanges) -> &'a Self {
        // SAFETY: `raw` points to a live `ProtectionRanges` that outlives `'a`.
        unsafe { &*raw.cast::<Self>() }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        // SAFETY: `self` is live and not concurrently mutated.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_is_empty(self.as_ffi_ptr()) }
    }

    #[inline]
    fn begin(&self) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and not mutated while the returned cursor is live, `out` is valid
        // for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_begin(self.as_ffi_ptr(), out.cast())
            })
        }
    }

    /// Returns a cursor to the first item whose key is strictly greater than `key`, or an end
    /// cursor (for which `get()` returns `None`) if no such item.
    #[inline]
    fn upper_bound(&self, key: usize) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and not mutated while the returned cursor is live, `out` is valid
        // for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_upper_bound(
                    self.as_ffi_ptr(),
                    key,
                    out.cast(),
                )
            })
        }
    }
}

struct ProtectionRangesMut<'a> {
    ranges: NonNull<bindings::VmMappingProtectionRanges>,
    _ranges: PhantomData<&'a mut ProtectionRanges>,
}

impl Deref for ProtectionRangesMut<'_> {
    type Target = ProtectionRanges;

    #[inline]
    fn deref(&self) -> &ProtectionRanges {
        // SAFETY: `self.ranges` points to a live `ProtectionRanges` that outlives `self` and is
        // not mutated while `self` is borrowed.
        unsafe { ProtectionRanges::from_raw_ref(self.ranges.as_ptr()) }
    }
}

impl<'a> ProtectionRangesMut<'a> {
    /// # Safety
    ///
    /// `ranges` points to a live `ProtectionRanges` that outlives `'a` and that nothing else
    /// accesses during `'a`.
    #[inline]
    unsafe fn from_raw(ranges: *mut bindings::VmMappingProtectionRanges) -> Self {
        // SAFETY: `ranges` is not null.
        Self { ranges: unsafe { NonNull::new_unchecked(ranges) }, _ranges: PhantomData }
    }

    #[inline]
    fn new(ranges: Pin<&'a mut ProtectionRanges>) -> Self {
        // SAFETY: `ranges` is live for `'a` and exclusively borrowed.
        unsafe { Self::from_raw(ranges.as_ffi_ptr()) }
    }

    #[inline]
    fn as_ffi_ptr(&self) -> *mut bindings::VmMappingProtectionRanges {
        self.ranges.as_ptr()
    }

    #[inline]
    fn begin_mut(&mut self) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_begin(ranges, out.cast())
            })
        }
    }

    #[inline]
    fn end_mut(&mut self) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_end(ranges, out.cast())
            })
        }
    }

    /// Returns a cursor to the first item whose key is greater than or equal to `key`, or
    /// `end_mut()` if no such item.
    #[inline]
    fn lower_bound_mut(&mut self, key: usize) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_lower_bound(ranges, key, out.cast())
            })
        }
    }

    /// Returns a cursor to the item whose key is exactly `key`, or `end_mut()` if no such item.
    #[inline]
    fn find_mut(&mut self, key: usize) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_find(ranges, key, out.cast())
            })
        }
    }

    /// Inserts the provided value using the specified key. It is an error to insert a duplicate
    /// key and attempting to do so will either cause a runtime assertion failure or data structure
    /// corruption.
    ///
    /// If internal nodes needed to, but could not be, allocated then an end cursor (for which
    /// `get()` returns `None`) is returned, otherwise a cursor to the new item is returned. In all
    /// cases if an end cursor is returned the tree is left in an unmodified state.
    ///
    /// After an insertion all cursors, except the one returned, are stale and must not be used.
    ///
    /// # Safety
    ///
    /// `key` is not already present in `self`.
    #[inline]
    #[must_use]
    unsafe fn insert(&mut self, key: usize, flags: ArchMmuFlags) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and exclusively accessed, `key` is not already present in it,
        // `out` is valid for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_insert(
                    self.as_ffi_ptr(),
                    key,
                    flags,
                    out.cast(),
                )
            })
        }
    }

    /// Clears all content from the tree, returning it to the empty state. All cursors are
    /// invalidated.
    #[inline]
    fn clear(&mut self) {
        // SAFETY: `self` is live and exclusively accessed.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_clear(self.as_ffi_ptr()) }
    }

    /// Clears `self` and moves all entries from `other` into `self`, leaving `other` empty.
    /// All cursors into either tree are invalidated.
    #[inline]
    fn move_from(&mut self, other: &mut ProtectionRangesMut<'_>) {
        // SAFETY: `self` and `other` are live, distinct and exclusively accessed.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_move_assign(
                self.as_ffi_ptr(),
                other.as_ffi_ptr(),
            )
        }
    }
}

struct ProtectionRangesCursor<'a> {
    raw: Opaque<bindings::VmMappingProtectionRangesCursor>,
    _ranges: PhantomData<&'a ProtectionRanges>,
}

zr::static_assert!(
    size_of::<bindings::VmMappingProtectionRangesCursor>()
        == constants::kVmMappingProtectionRangesCursorSize
);
zr::static_assert!(
    align_of::<bindings::VmMappingProtectionRangesCursor>()
        == constants::kVmMappingProtectionRangesCursorAlign
);

impl Clone for ProtectionRangesCursor<'_> {
    #[inline]
    fn clone(&self) -> Self {
        let raw = Opaque::uninit();
        // SAFETY: `self.raw` is initialized and valid for reads, `raw` is valid for writes, they
        // do not overlap, and the cursor type is trivially copyable.
        unsafe { ptr::copy_nonoverlapping(self.raw.get(), raw.get(), 1) };
        Self { raw, _ranges: PhantomData }
    }
}

impl<'a> ProtectionRangesCursor<'a> {
    /// # Safety
    ///
    /// `init` initializes the cursor it is passed to a cursor into ranges that outlive `'a` and
    /// are not mutated during `'a`.
    #[inline]
    unsafe fn new(init: impl FnOnce(*mut bindings::VmMappingProtectionRangesCursor)) -> Self {
        let raw = Opaque::uninit();
        init(raw.get());
        Self { raw, _ranges: PhantomData }
    }

    #[inline]
    fn get(&self) -> Option<(usize, ArchMmuFlags)> {
        if !self.is_valid() {
            return None;
        }
        let mut key = 0;
        let mut flags = 0;
        // SAFETY: `self.raw` is a valid cursor into live ranges that are not concurrently
        // mutated, and is not stale, and `key` and `flags` are valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_cursor_get(
                self.raw.get(),
                &mut key,
                &mut flags,
            )
        };
        Some((key, flags))
    }

    /// Returns whether the cursor is one that can be dereferenced, i.e. is not an end
    /// cursor or `begin()` of an empty tree. If the cursor is stale due to `insert` or `erase`
    /// having been called then the return value is undefined, and if iterator validation is
    /// enabled this method counts as an access and will trigger an error.
    #[inline]
    fn is_valid(&self) -> bool {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_is_valid(self.raw.get()) }
    }

    #[inline]
    fn next(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_next(self.raw.get()) }
    }

    /// # Safety
    ///
    /// `self` is not before the first item.
    #[inline]
    unsafe fn prev(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, is
        // not stale, and is not before the first item.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_prev(self.raw.get()) }
    }
}

struct ProtectionRangesCursorMut<'a> {
    ranges: NonNull<bindings::VmMappingProtectionRanges>,
    raw: Opaque<bindings::VmMappingProtectionRangesCursor>,
    _ranges: PhantomData<&'a mut ProtectionRanges>,
}

impl<'a> ProtectionRangesCursorMut<'a> {
    /// # Safety
    ///
    /// `init` initializes the cursor it is passed to a cursor into `ranges`.
    #[inline]
    unsafe fn new(
        ranges: &'a mut ProtectionRangesMut<'_>,
        init: impl FnOnce(*mut bindings::VmMappingProtectionRangesCursor),
    ) -> Self {
        let raw = Opaque::uninit();
        init(raw.get());
        Self { ranges: ranges.ranges, raw, _ranges: PhantomData }
    }

    #[inline]
    fn get(&self) -> Option<(usize, ArchMmuFlags)> {
        if !self.is_valid() {
            return None;
        }
        let mut key = 0;
        let mut flags = 0;
        // SAFETY: `self.raw` is a valid cursor into live ranges that are not concurrently
        // mutated, and is not stale, and `key` and `flags` are valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_cursor_get(
                self.raw.get(),
                &mut key,
                &mut flags,
            )
        };
        Some((key, flags))
    }

    #[inline]
    fn is_valid(&self) -> bool {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_is_valid(self.raw.get()) }
    }

    /// # Safety
    ///
    /// `self` is not before the first item.
    #[inline]
    unsafe fn prev(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, is
        // not stale, and is not before the first item.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_prev(self.raw.get()) }
    }

    /// Like `ProtectionRangesMut::insert`, but uses `self` as a hint of an item to insert near,
    /// and moves `self` to the cursor that `insert` would return. `self` can be to an invalid
    /// location (i.e. `end_mut()`).
    ///
    /// # Safety
    ///
    /// `key` is not already present in the ranges.
    #[inline]
    unsafe fn insert_hint(&mut self, key: usize, flags: ArchMmuFlags) {
        let raw: Opaque<bindings::VmMappingProtectionRangesCursor> = Opaque::uninit();
        // SAFETY: `self.ranges` is live and exclusively accessed, `self.raw` is a cursor into it
        // that is not stale, `key` is not already present in it, and `raw` is valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_insert_hint(
                self.ranges.as_ptr(),
                self.raw.get(),
                key,
                flags,
                raw.get().cast(),
            )
        };
        self.raw = raw;
    }

    /// Updates the flags of the item referenced by `self`. C++ `BTree::update` asserts that
    /// `self.is_valid()` is true. As this does not change the structure of the tree this does not
    /// invalidate any cursors.
    #[inline]
    fn update(&mut self, flags: ArchMmuFlags) {
        // SAFETY: `self.ranges` is live and exclusively accessed, and `self.raw` is a cursor into
        // it that is not stale.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_update(
                self.ranges.as_ptr(),
                self.raw.get(),
                flags,
            )
        }
    }

    /// Removes the item at `self`, and moves `self` to the item that followed it.
    ///
    /// After an erase all cursors, except `self`, are stale and must not be used.
    ///
    /// # Safety
    ///
    /// `self.is_valid()` is true.
    #[inline]
    unsafe fn erase(&mut self) {
        let raw: Opaque<bindings::VmMappingProtectionRangesCursor> = Opaque::uninit();
        // SAFETY: `self.ranges` is live and exclusively accessed, `self.raw` is a valid cursor
        // into it that is not stale, and `raw` is valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_erase(
                self.ranges.as_ptr(),
                self.raw.get(),
                raw.get().cast(),
            )
        };
        self.raw = raw;
    }
}

#[repr(C)]
pub struct VmMappingState {
    canary: Canary<{ fbl::magic(b"VMAP") }>,
    /// Whether this mapping may be merged with other adjacent mappings. A mergeable mapping is just
    /// a region that can be represented by any `VmMapping` object, not specifically this one.
    mergeable: KCell<Mergeable, VmAspaceLockClass>,
    /// TODO(https://fxbug.dev/42106188): Tracks whether this mapping has been transitioned into a
    /// private clone to allow for writes to safely be done without modifying a VMO that the mapping
    /// does not have permission to.
    private_clone: bool,
    /// Tracks whether the `object` has been reset (i.e. is `None`) or not. `object` is always
    /// `Some` at construction and only ever performs a single transition, which is to `None`, prior
    /// to destruction. The reset of `object`, and hence the modification of this value, happens
    /// under both the aspace lock and the `object` lock. See `page_fault` for usage of this.
    object_reset: AtomicBool,
    /// The protection flags of this mapping are tracked as a series of contiguous
    /// virtual address ranges. Since the base address of the mapping is known (`base`),
    /// we only need to store the flags for the first range, and then the start address
    /// and flags for any subsequent ranges where the protections change.
    ///
    /// `first_region_arch_mmu_flags` holds the MMU flags for the interval starting
    /// exactly at `base`. If the protection ranges are empty, these flags apply to the
    /// entire mapping: `[base, base + size)`.
    ///
    /// This can be read with either lock held, but requires both locks to write it.
    first_region_arch_mmu_flags: UnsafeCell<ArchMmuFlags>,
    /// pointer and region of the object we are mapping.
    /// The `object` cannot be immutable, as it gets reset, but logically it is a constant value
    /// from construction until it gets reset, and then it stays `None` until the mapping is
    /// destructed.
    object: UnsafeCell<Option<RefPtr<VmObject>>>,
    object_offset: u64,
    /// Pointer to a `CurrentlyFaulting` object if the mapping is presently handling a page fault.
    /// This is protected specifically by the `object` lock so that `aspace_unmap_locked_object` can
    /// inspect it.
    currently_faulting: KCell<Option<NonNull<CurrentlyFaulting<'static>>>, VmObjectLockClass>,
}

::zr::static_assert_size_and_align!(
    VmMappingState,
    constants::kVmMappingStateSize,
    constants::kVmMappingStateAlign,
);

impl VmMappingState {
    fn new(
        object: Option<RefPtr<VmObject>>,
        object_offset: u64,
        first_region_arch_mmu_flags: ArchMmuFlags,
        private_clone: bool,
        mergeable: Mergeable,
    ) -> Self {
        Self {
            canary: Canary::new(),
            mergeable: KCell::new(mergeable),
            private_clone,
            object_reset: AtomicBool::new(false),
            first_region_arch_mmu_flags: UnsafeCell::new(first_region_arch_mmu_flags),
            object: UnsafeCell::new(object),
            object_offset,
            currently_faulting: KCell::new(None),
        }
    }
}

impl HasRefCount for VmMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
    }
}

impl Deref for VmMapping {
    type Target = VmAddressRegionOrMapping;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `VmAddressRegionOrMapping` is the first base of `VmMapping`.
        unsafe { &*core::ptr::from_ref(self).cast() }
    }
}

// SAFETY: `VmMapping` is not tied to the thread that created it.
unsafe impl Send for VmMapping {}
// SAFETY: Mutable state of `VmMapping` is only accessed with the aspace lock or the object lock
// held, or atomically.
unsafe impl Sync for VmMapping {}

zr::static_assert!(
    VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES as u64
        == constants::kVmMappingPageFaultMaxOptimisticPages
);

impl VmMapping {
    /// The maximum number of pages that a page fault can optimistically extend the fault to
    /// include. This is defined and exposed here for the purposes of unittests.
    pub const PAGE_FAULT_MAX_OPTIMISTIC_PAGES: usize = 16;

    /// private constructor, use `VmAddressRegion::create...()` instead
    #[expect(clippy::too_many_arguments)]
    fn new(
        parent: &VmAddressRegion,
        private_clone: bool,
        base: usize,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        mergeable: Mergeable,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_mapping_construct,
            parent.as_ffi_ptr(),
            private_clone,
            base,
            size,
            vmar_flags,
            RefPtr::into_raw(vmo).cast_mut().cast(),
            vmo_offset,
            arch_mmu_flags,
            mergeable == Mergeable::Yes,
        )
    }

    /// private constructor, use `VmAddressRegion::create...()` instead
    #[expect(clippy::too_many_arguments)]
    fn new_with_ranges(
        parent: &VmAddressRegion,
        private_clone: bool,
        base: usize,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        first_mmu_flags: ArchMmuFlags,
        ranges: Pin<&mut ProtectionRanges>,
        mergeable: Mergeable,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_mapping_construct_with_ranges,
            parent.as_ffi_ptr(),
            private_clone,
            base,
            size,
            vmar_flags,
            RefPtr::into_raw(vmo).cast_mut().cast(),
            vmo_offset,
            first_mmu_flags,
            ranges.as_ffi_ptr(),
            mergeable == Mergeable::Yes,
        )
    }

    fn as_ffi_ptr(&self) -> *mut bindings::VmMapping {
        self.raw.get()
    }

    fn state(&self) -> &VmMappingState {
        // SAFETY: `self` points to a live `VmMapping`.
        unsafe { &*bindings::cpp_vm_mapping_state(self.as_ffi_ptr()).cast() }
    }

    /// Marks this mapping as being a candidate for merging, and will immediately attempt to merge
    /// with any neighboring mappings. Making a mapping mergeable essentially indicates that you
    /// will no longer use this specific `VmMapping` instance to refer to the referenced region, and
    /// will access the region via the parent vmar in the future, and so the region merely needs to
    /// remain valid through some `VmMapping`.
    /// For this the function requires you to hand in your last remaining `RefPtr` to the mapping.
    pub fn mark_mergeable(mapping: RefPtr<VmMapping>) {
        ksync::lock!(let mut region_guard = mapping.region_lock().lock());
        ksync::lock!(let mut guard = mapping.lock().lock());
        // Now that we have the lock check this mapping is still alive and we haven't raced with
        // some kind of destruction.
        if mapping.state_locked(guard.token()) != LifeCycleState::Alive {
            hint::cold_path();
            return;
        }
        // Skip marking any vdso segments mergeable. Although there is currently only one vdso
        // segment and so it would never actually get merged, marking it mergeable is technically
        // incorrect.
        if ptr::eq(
            mapping.aspace().vdso_code_mapping_locked(guard.token()),
            RefPtr::as_ptr(&mapping),
        ) {
            hint::cold_path();
            return;
        }
        mapping.set_mergeable_locked(guard.as_mut().token_mut(), Mergeable::Yes);
        Self::try_merge_neighbors_locked(
            &mapping,
            region_guard.as_mut().token_mut(),
            guard.as_mut().token_mut(),
        );
    }

    /// Returns the offset into the underlying VMO for this mapping.
    pub fn object_offset(&self) -> u64 {
        self.state().object_offset
    }

    fn first_region_arch_mmu_flags_locked(
        &self,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().first_region_arch_mmu_flags.get() }
    }

    fn first_region_arch_mmu_flags_locked_object(
        &self,
        _object_token: &LockToken<'_, VmObjectLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: The object lock is held.
        unsafe { *self.state().first_region_arch_mmu_flags.get() }
    }

    fn set_first_region_arch_mmu_flags_locked(
        &self,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
        flags: ArchMmuFlags,
    ) {
        // SAFETY: Both guarding locks are held.
        unsafe {
            *self.state().first_region_arch_mmu_flags.get() = flags;
        }
    }

    fn mergeable_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> Mergeable {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().mergeable.get(token) }
    }

    fn set_mergeable_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        mergeable: Mergeable,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().mergeable.get_mut(token) } = mergeable;
    }

    #[inline]
    fn rest_protection_ranges_locked<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> &'a ProtectionRanges {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and is not mutated during `'a`.
        unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                self.as_ffi_ptr(),
            ))
        }
    }

    #[inline]
    fn rest_protection_ranges_locked_object<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmObjectLockClass>,
    ) -> &'a ProtectionRanges {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and is not mutated during `'a`.
        unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                self.as_ffi_ptr(),
            ))
        }
    }

    #[inline]
    fn rest_protection_ranges_mut_locked<'a>(
        &'a self,
        _aspace_token: &'a mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &'a mut LockToken<'_, VmObjectLockClass>,
    ) -> ProtectionRangesMut<'a> {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and that nothing else accesses during `'a`.
        unsafe {
            ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                self.as_ffi_ptr(),
            ))
        }
    }

    /// # Safety
    ///
    /// `other` is not `self`.
    #[inline]
    unsafe fn rest_protection_ranges_mut_and_other_locked<'a>(
        &'a self,
        other: &'a VmMapping,
        _aspace_token: &'a mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &'a mut LockToken<'_, VmObjectLockClass>,
    ) -> (ProtectionRangesMut<'a>, &'a ProtectionRanges) {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and that nothing else accesses during `'a`.
        let ranges = unsafe {
            ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                self.as_ffi_ptr(),
            ))
        };
        // SAFETY: `other` is live, and the returned pointer points to a live `ProtectionRanges`
        // that outlives `'a` and is not mutated during `'a`.
        let other_ranges = unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                other.as_ffi_ptr(),
            ))
        };
        (ranges, other_ranges)
    }

    /// Returns mutable handles to the protection ranges of both `self` and `other` under a single
    /// borrow of the lock tokens.
    ///
    /// # Safety
    ///
    /// `other` is not `self`.
    #[inline]
    unsafe fn rest_protection_ranges_pair_mut_locked<'a>(
        &'a self,
        other: &'a VmMapping,
        _aspace_token: &'a mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &'a mut LockToken<'_, VmObjectLockClass>,
    ) -> (ProtectionRangesMut<'a>, ProtectionRangesMut<'a>) {
        // SAFETY: `self` and `other` are live and distinct, and the returned pointers point to live
        // `ProtectionRanges` that outlive `'a` and that nothing else accesses during `'a`.
        unsafe {
            (
                ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                    self.as_ffi_ptr(),
                )),
                ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                    other.as_ffi_ptr(),
                )),
            )
        }
    }

    fn object_locked<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> &'a Option<RefPtr<VmObject>> {
        // SAFETY: The aspace lock is held.
        unsafe { &*self.state().object.get() }
    }

    #[inline]
    fn object_lock_is_held(&self, token: &LockToken<'_, VmAspaceLockClass>) -> bool {
        self.object_locked(token).as_ref().unwrap().lock_ref().raw_mutex().is_held()
    }

    fn reset_object_locked(
        &self,
        _aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: Both the aspace lock and the object lock are held.
        unsafe { *self.state().object.get() = None };
        self.state().object_reset.store(true, Ordering::Relaxed);
    }

    fn object_reset(&self) -> bool {
        self.state().object_reset.load(Ordering::Relaxed)
    }

    fn currently_faulting<'s>(
        &'s self,
        token: &LockToken<'_, VmObjectLockClass>,
    ) -> Option<NonNull<CurrentlyFaulting<'s>>> {
        // SAFETY: The object lock is held.
        let faulting = *unsafe { self.state().currently_faulting.get(token) };
        faulting.map(NonNull::cast::<CurrentlyFaulting<'s>>)
    }

    /// # Safety
    ///
    /// `faulting` is `None`, or points to a `CurrentlyFaulting` for this mapping that stays live
    /// and is not moved until `currently_faulting` is next set. Until then, the object lock stays
    /// held, no `&mut` to it exists, and it is not written except through its `Cell` fields.
    unsafe fn set_currently_faulting(
        &self,
        token: &mut LockToken<'_, VmObjectLockClass>,
        faulting: Option<NonNull<CurrentlyFaulting<'_>>>,
    ) {
        // SAFETY: The object lock is held.
        *unsafe { self.state().currently_faulting.get_mut(token) } =
            faulting.map(NonNull::cast::<CurrentlyFaulting<'static>>);
    }

    /// Intended to be used from `VmEnumerator` callbacks where the `aspace` lock will be held.
    pub fn vmo_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> Option<RefPtr<VmObject>> {
        self.object_locked(token).clone()
    }

    /// Returns the underlying VMO backing this mapping.
    pub fn vmo(&self) -> Option<RefPtr<VmObject>> {
        ksync::lock!(let guard = self.lock().lock());
        self.vmo_locked(guard.token())
    }

    /// Returns the memory attributed to this mapping. `guard` is temporarily dropped while the VMO
    /// is queried.
    ///
    /// `guard` must be the guard of this mapping's aspace lock.
    pub(super) fn get_attributed_memory_locked(
        &self,
        guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> AttributionCounts {
        self.state().canary.assert();

        if !self.is_alive_locked(guard.token()) {
            return AttributionCounts::default();
        }

        VM_MAPPING_ATTRIBUTION_QUERIES.add(1);

        let vmo = self.object_locked(guard.token()).clone().expect("object must be set");
        let object_offset = self.object_offset();
        let size = self.size() as u64;
        guard.call_unlocked(move || {
            let counts = vmo.get_attributed_memory_in_range(object_offset, size);
            drop(vmo);
            counts
        })
    }

    pub fn get_attributed_memory(&self) -> AttributionCounts {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.lock().lock());
        self.get_attributed_memory_locked(guard.as_mut())
    }

    /// Convenience wrapper for `vmo().decommit_range()` with the necessary
    /// offset modification and locking.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<(), Status> {
        self.state().canary.assert();
        ltracef!(
            "{:p} [{:#x}+{:#x}], offset {:#x}, len {:#x}\n",
            self as *const _,
            self.base().0,
            self.size(),
            offset,
            len
        );

        ksync::lock!(let guard = self.lock().lock());
        let token = guard.token();
        if self.state_locked(token) != LifeCycleState::Alive {
            return Err(Status::BAD_STATE);
        }
        let end = offset.checked_add(len).ok_or(Status::OUT_OF_RANGE)?;
        if end > self.size() {
            return Err(Status::OUT_OF_RANGE);
        }
        // `VmObject::decommit_range` will typically call back into our instance's
        // `VmMapping::aspace_unmap_locked_object`.
        let object = self.object_locked(token).as_ref().unwrap();
        object.decommit_range(self.object_offset() + offset as u64, len as u64)
    }

    /// Takes a range relative to the vmo and converts it into a virtual address range
    /// relative to `aspace()`. Returns `Some((base, virtual_len))` if a non zero sized intersection
    /// was found, `None` otherwise.
    fn object_range_to_vaddr_range(
        &self,
        _token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) -> Option<(VAddr, u64)> {
        debug_assert!(kernel_page::is_aligned(offset as usize));
        debug_assert!(kernel_page::is_aligned(len as usize));

        // Zero sized ranges are considered to have no overlap.
        if len == 0 {
            return None;
        }

        // compute the intersection of the passed in vmo range and our mapping
        let (offset_new, virtual_len) = range_check::get_intersect_offset_len(
            self.object_offset(),
            self.size() as u64,
            offset,
            len,
        )?;

        debug_assert!(virtual_len > 0 && virtual_len <= usize::MAX as u64);
        debug_assert!(offset_new >= self.object_offset());

        ltracef!("intersection offset {:#x}, len {:#x}\n", offset_new, virtual_len);

        // make sure the `base` + `offset` is within our address space
        // should be, according to the range stored in `base()` + `size()`
        let (base, overflowed) =
            self.base().0.overflowing_add((offset_new - self.object_offset()) as usize);
        assert!(!overflowed);

        // make sure we're only operating within our window
        assert!(base >= self.base().0);
        assert!((base + virtual_len as usize - 1) <= (self.base().0 + self.size() - 1));

        Some((VAddr(base), virtual_len))
    }

    /// Unmap any pages that map the passed in vmo range from the arch aspace.
    /// May not intersect with this range.
    /// If `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options` then the caller is asserting that
    /// it knows that any mappings in the region will only be for the shared zero page.
    ///
    /// # Safety
    ///
    /// This mapping is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    /// pages that map `[offset, offset + len)` of the VMO in this mapping may be unmapped. If
    /// `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only
    /// map the shared zero page.
    pub(super) unsafe fn aspace_unmap_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
        options: UnmapOptions,
    ) {
        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // The object pointer itself is not accessed in this method, and we do not hold the correct
        // lock for it, but we know the object lock is held and so therefore the pointer is valid
        // and will not be modified. Therefore it's correct to read it here for the purposes of an
        // assert, but cannot be expressed nicely with regular annotations.
        // SAFETY: The object lock is held.
        debug_assert!(unsafe { (*self.state().object.get()).is_some() });

        // In the case of unmapping known instances of the zero page check if this range intersects
        // with an in progress fault. If it does we can skip the unmap with the knowledge that the
        // mapping will be updated later. This is safe since the zero page is, by definition, only
        // mapped read only, and is never modified so delaying the update of the mapping cannot
        // cause either any users to see incorrect data, or users to be able to modify an old
        // mapping.
        if options.contains(UnmapOptions::ONLY_HAS_ZERO_PAGES)
            && let Some(currently_faulting) = self.currently_faulting(token)
        {
            // SAFETY: `currently_faulting` points to a live `CurrentlyFaulting`, to which no `&mut`
            // exists and which is not written except through its `Cell` fields while this
            // reference is live.
            let currently_faulting = unsafe { currently_faulting.as_ref() };
            if currently_faulting.unmap_range(offset, len) {
                return;
            }
        }

        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        // See if there's an intersect.
        let Some((base, new_len)) = self.object_range_to_vaddr_range(token, offset, len) else {
            return;
        };

        // If this is a kernel mapping then we should not be removing mappings out of the arch
        // aspace, unless this mapping has explicitly opted out of this check.
        let aspace = self.aspace();
        debug_assert!(
            aspace.is_user()
                || aspace.is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0
        );

        let mut aspace_op = aspace.enlarge_arch_unmap();
        if options.contains(UnmapOptions::HARVEST) {
            aspace_op |= ARCH_UNMAP_OPTION_HARVEST;
        }

        // SAFETY: `[base, base + new_len)` is part of this mapping and may be unmapped.
        let status = unsafe {
            aspace.arch_aspace().unmap(base.0, new_len as usize / kernel_page::SIZE, aspace_op)
        };
        assert!(status.is_ok());
    }

    /// Removes any writeable mappings for the passed in vmo range from the arch aspace.
    /// May fall back to unmapping pages from the arch aspace if necessary.
    ///
    /// # Safety
    ///
    /// This mapping is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    /// pages that map `[offset, offset + len)` of the VMO in this mapping may have write permission
    /// removed, and may be unmapped.
    pub(super) unsafe fn aspace_remove_write_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) {
        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // The object pointer itself is not accessed in this method, and we do not hold the correct
        // lock for it, but we know the object lock is held and so therefore the pointer is valid
        // and will not be modified. Therefore it's correct to read it here for the purposes of an
        // assert, but cannot be expressed nicely with regular annotations.
        // SAFETY: The object lock is held.
        debug_assert!(unsafe { (*self.state().object.get()).is_some() });

        // If this doesn't support writing then nothing to be done, as we know we have no write
        // mappings.
        if (self.flags() & flag::CAN_MAP_WRITE) == 0 {
            return;
        }

        // See if there's an intersect.
        let Some((base, new_len)) = self.object_range_to_vaddr_range(token, offset, len) else {
            return;
        };

        // If this is a kernel mapping then we should not modify mappings in the arch aspace,
        // unless this mapping has explicitly opted out of this check.
        if cfg!(debug_assertions)
            && !(self.aspace().is_user()
                || self.aspace().is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0)
        {
            debug_assert_aspace_remove_write_failed(self, offset, len);
        }

        let status = self.enumerate_protection_ranges_locked_object(
            token,
            base.0,
            new_len as usize,
            |region_base, region_len, mut mmu_flags| {
                // If this range doesn't currently support being writable then we can skip.
                if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0 {
                    return Err(Status::NEXT);
                }

                // Build new mmu flags without writing.
                mmu_flags &= !ARCH_MMU_FLAG_PERM_WRITE;

                // SAFETY: `region_base` and `region_len` are page aligned, and
                // `[region_base, region_base + region_len)` is part of this mapping, its
                // permissions may be changed to `mmu_flags`, and it may be unmapped.
                let result = unsafe {
                    Self::protect_or_unmap(self.aspace(), region_base, region_len, mmu_flags)
                };
                if result.is_ok() {
                    return Err(Status::NEXT);
                }
                result
            },
        );
        assert!(status.is_ok());
    }

    /// Checks if this is a kernel mapping within the given VMO range, which would be an error to be
    /// unpinning.
    pub(super) fn aspace_debug_unpin_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) {
        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // See if there's an intersect.
        if self.object_range_to_vaddr_range(token, offset, len).is_none() {
            return;
        }

        // This unpin is not allowed for kernel mappings, unless the mapping has specifically opted
        // out of this debug check due to it performing its own dynamic management.
        debug_assert!(
            self.aspace().is_user()
                || self.aspace().is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0
        );
    }

    /// Returns the minimum of the requested map length, the size of the VMO or, if
    /// `FAULT_BEYOND_STREAM_SIZE` is set, the page containing the stream size. `map_range` can be
    /// trimmed to these lengths as it should not be considered an error to call `map_range` past
    /// the VMO size in a resizable VMO or past the page containing the stream size in a
    /// `FAULT_BEYOND_STREAM_SIZE` VMO.
    fn trimmed_object_range_locked(
        &self,
        aspace_token: &LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) -> u64 {
        let vmo_offset = self.object_offset() + offset;
        let object = self.object_locked(aspace_token).as_ref().unwrap();
        let vmo_size = object.size_locked(object_token);
        if vmo_offset >= vmo_size {
            return 0;
        }

        let mut trim_len = vmo_size - vmo_offset;

        if (self.flags() & flag::FAULT_BEYOND_STREAM_SIZE) != 0 {
            let paged = object.as_paged();
            debug_assert!(paged.is_some());
            // SAFETY: `paged` is `Some`.
            let paged = unsafe { paged.unwrap_unchecked() };
            debug_assert!(paged.lock_is_held());
            let stream_size_res = paged.saturating_stream_size_locked(object_token);
            // Creating a fault-beyond-stream-size mapping should have allocated a SSM.
            debug_assert!(stream_size_res.is_some());
            let stream_size = stream_size_res.unwrap();
            debug_assert!(stream_size <= vmo_size);
            if vmo_offset >= stream_size {
                return 0;
            }
            trim_len = stream_size - vmo_offset;
        }

        core::cmp::min(trim_len, len)
    }

    /// Enumerates any different protection ranges that exist inside this mapping. The virtual range
    /// specified by `base` and `size` must be within this mapping's base and size. The provided
    /// callback is called in virtual address order for each protection type. `Status::NEXT` and
    /// `Status::STOP` can be used to control iteration, with any other status becoming the return
    /// value of this method.
    pub(super) fn enumerate_protection_ranges_locked<F>(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        self.enumerate_protection_ranges(
            || self.is_alive_locked(token),
            || self.rest_protection_ranges_locked(token),
            || self.first_region_arch_mmu_flags_locked(token),
            base,
            size,
            func,
        )
    }

    fn enumerate_protection_ranges_locked_object<F>(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        base: usize,
        size: usize,
        func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        self.enumerate_protection_ranges(
            || self.is_alive_locked_object(token),
            || self.rest_protection_ranges_locked_object(token),
            || self.first_region_arch_mmu_flags_locked_object(token),
            base,
            size,
            func,
        )
    }

    #[inline(always)]
    fn enumerate_protection_ranges<'r, F>(
        &self,
        is_alive: impl FnOnce() -> bool,
        rest_protection_ranges: impl FnOnce() -> &'r ProtectionRanges,
        first_region_arch_mmu_flags: impl FnOnce() -> ArchMmuFlags,
        base: usize,
        size: usize,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        debug_assert!(self.is_in_range(VAddr(base), size));
        // If the mapping is no longer alive, then return early since there's nothing to enumerate.
        if !is_alive() {
            hint::cold_path();
            return Ok(());
        }

        let end = base.wrapping_add(size);

        // Find the first transition strictly after `base`.
        let mut it = rest_protection_ranges().upper_bound(base);

        // `it` now represents the end point of the first range, so look backwards to determine the
        // start flags.
        let mut prev = it.clone();
        // SAFETY: `prev` comes from `upper_bound`, so it is not before the first item.
        unsafe { prev.prev() };
        let mut flags = match prev.get() {
            Some((_, flags)) => flags,
            None => first_region_arch_mmu_flags(),
        };
        let mut range_start = base;

        loop {
            let current = it.get();
            // The current range ends at either `end` or the next transition point, whichever is
            // earlier.
            let range_end = match current {
                Some((key, _)) => min(key, end),
                None => end,
            };
            debug_assert!(range_start < range_end);

            let result = func(range_start, range_end - range_start, flags);
            if result != Err(Status::NEXT) {
                if result == Err(Status::STOP) {
                    return Ok(());
                }
                return result;
            }

            // If we've reached the end of the requested range, or there are no more transitions,
            // stop. Placing this check here allows the range generation above to also generate the
            // 'trailing' range for the last transition point without needing an extra invocation of
            // `func`, which helps inlining.
            let Some((key, next_flags)) = current else {
                break;
            };
            if key >= end {
                break;
            }

            // Move to the next transition point.
            range_start = range_end;
            flags = next_flags;
            it.next();
        }
        Ok(())
    }

    #[inline]
    fn flags_range_at_addr_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        va: usize,
    ) -> FlagsRange {
        self.flags_range_at_addr(
            self.rest_protection_ranges_locked(token),
            || self.first_region_arch_mmu_flags_locked(token),
            va,
        )
    }

    #[inline]
    fn flags_range_at_addr_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        va: usize,
    ) -> FlagsRange {
        self.flags_range_at_addr(
            self.rest_protection_ranges_locked_object(token),
            || self.first_region_arch_mmu_flags_locked_object(token),
            va,
        )
    }

    #[inline(always)]
    fn flags_range_at_addr(
        &self,
        rest_protection_ranges: &ProtectionRanges,
        first_region_arch_mmu_flags: impl Fn() -> ArchMmuFlags,
        va: usize,
    ) -> FlagsRange {
        if rest_protection_ranges.is_empty() {
            return FlagsRange {
                mmu_flags: first_region_arch_mmu_flags(),
                region_top: self.base().0.wrapping_add(self.size()) as u64,
            };
        }
        core::hint::cold_path();
        // Find the first transition strictly after `va`.
        let mut it = rest_protection_ranges.upper_bound(va);
        // `it` represents the end of the range that includes `va`
        let top = match it.get() {
            Some((key, _)) => key,
            None => self.base().0.wrapping_add(self.size()),
        };
        // Now go backwards to find the start of the range that includes `va`, which tells us the
        // flags.
        // SAFETY: `it` comes from `upper_bound`, so it is not before the first item.
        unsafe { it.prev() };
        let flags = match it.get() {
            Some((_, flags)) => flags,
            None => first_region_arch_mmu_flags(),
        };
        FlagsRange { mmu_flags: flags, region_top: top as u64 }
    }

    /// Helper for protect and unmap.
    ///
    /// # Safety
    ///
    /// `base` and `size` are page aligned, and `[base, base + size)` is part of a mapping in
    /// `aspace` whose permissions may be changed to `new_arch_mmu_flags`, and which may be
    /// unmapped.
    unsafe fn protect_or_unmap(
        aspace: &VmAspace,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        debug_assert!(kernel_page::is_aligned(base) && kernel_page::is_aligned(size));
        // This can never be used to set a WRITE permission since it does not ask the underlying VMO
        // to perform the copy-on-write step. The underlying VMO might also support dirty tracking,
        // which requires write permission faults in order to track pages as dirty when written.
        assert!((new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0);
        // If not removing all permissions do the protect, otherwise skip straight to unmapping the
        // entire region.
        if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) != 0 {
            // SAFETY: The permissions of `[base, base + size)` may be changed to
            // `new_arch_mmu_flags`.
            let status = unsafe {
                aspace.arch_aspace().protect(
                    base,
                    size / kernel_page::SIZE,
                    new_arch_mmu_flags,
                    if aspace.can_enlarge_arch_unmap() {
                        ARCH_UNMAP_OPTION_ENLARGE
                    } else {
                        ARCH_UNMAP_OPTION_NONE
                    },
                )
            };
            // If the unmap failed and we are allowed to unmap extra portions of the aspace then
            // fall through and unmap, otherwise return with whatever the status is.
            if status.is_ok() {
                return status;
            }
            hint::cold_path();
            if !aspace.can_enlarge_arch_unmap() {
                return status;
            }
        }

        // SAFETY: `[base, base + size)` may be unmapped.
        unsafe {
            aspace.arch_aspace().unmap(base, size / kernel_page::SIZE, aspace.enlarge_arch_unmap())
        }
    }

    /// Copies protection ranges for the given sub-range into the returned first flags and
    /// `out_ranges`. The sub-range `[base, base + size)` must be within
    /// `[self.base(), self.base() + self.size())`.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not alive.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `[base, base + size)` is within
    /// `[self.base(), self.base() + self.size())`, and `out_ranges` is empty.
    #[inline(never)]
    unsafe fn copy_protection_ranges_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        out_ranges: Pin<&mut ProtectionRanges>,
    ) -> Result<ArchMmuFlags, Status> {
        debug_assert!(size != 0 && self.is_in_range(VAddr(base), size));
        let mut out_ranges = ProtectionRangesMut::new(out_ranges);
        let mut first_flags = None;
        let out_first_flags = &mut first_flags;
        self.enumerate_protection_ranges_locked(token, base, size, move |b, _s, flags| {
            if b == base {
                *out_first_flags = Some(flags);
            } else {
                // SAFETY: `b` is not present in `out_ranges`.
                if !unsafe { out_ranges.insert(b, flags) }.is_valid() {
                    hint::cold_path();
                    return Err(Status::NO_MEMORY);
                }
            }
            Err(Status::NEXT)
        })?;
        Ok(first_flags.expect("first_flags must be set"))
    }

    /// Merges a copy of the protection ranges of `right` into this mapping.
    /// Assumes `right` is immediately to the right of this mapping.
    ///
    /// # Safety
    ///
    /// `right` is not `self`, and `right.base()` and the keys of the protection ranges of `right`
    /// are distinct from each other and absent from the protection ranges of `self`.
    unsafe fn merge_protection_ranges_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        right: &VmMapping,
    ) -> Result<(), Status> {
        debug_assert!(right.base().0 == self.base().0 + self.size());
        let first_flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        let right_first_flags = right.first_region_arch_mmu_flags_locked(aspace_token);
        // SAFETY: `right` is not `self`.
        let (mut rest_protection_ranges, right_rest_protection_ranges) = unsafe {
            self.rest_protection_ranges_mut_and_other_locked(right, aspace_token, object_token)
        };
        // Lookup the flags for the end of the range.
        let mut it = rest_protection_ranges.end_mut();
        // SAFETY: `it` is not before the first item.
        unsafe { it.prev() };
        let last_flags = match it.get() {
            Some((_, flags)) => flags,
            None => first_flags,
        };
        if last_flags != right_first_flags {
            // SAFETY: `right.base()` is not present in `rest_protection_ranges`.
            unsafe { it.insert_hint(right.base().0, right_first_flags) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }
        let mut right_it = right_rest_protection_ranges.begin();
        while let Some((key, value)) = right_it.get() {
            // SAFETY: `key` is not present in `rest_protection_ranges`.
            unsafe { it.insert_hint(key, value) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
            right_it.next();
        }
        Ok(())
    }

    /// Removes any protection range transitions within the range `[base, end)`.
    fn clear_protection_range_transitions_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        base: usize,
        end: usize,
    ) {
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut it = rest_protection_ranges.lower_bound_mut(base);
        while it.get().is_some_and(|(key, _)| key < end) {
            // SAFETY: `it` is valid.
            unsafe { it.erase() };
        }
    }

    /// Removes all protection range transitions at or below `split_addr`, returning the flags
    /// that are active at `split_addr`.
    fn remove_after_split_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        split_addr: usize,
    ) -> ArchMmuFlags {
        let mut flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut it = rest_protection_ranges.begin_mut();
        while let Some((key, value)) = it.get() {
            if key > split_addr {
                break;
            }
            flags = value;
            // SAFETY: `it` is valid.
            unsafe { it.erase() };
        }
        flags
    }

    /// Change access permissions for this mapping.  It is an error to specify a
    /// caching mode in the flags.  This will persist the caching mode the
    /// mapping was created with.  If a subrange of the mapping is specified, the
    /// mapping may be split.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within
    /// this mapping and may have its permissions changed and be unmapped, and `new_arch_mmu_flags`
    /// has no cache flags and is valid for this mapping.
    pub(super) unsafe fn protect_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // Assert a few things that should already have been checked by the caller.
        debug_assert!(size != 0 && kernel_page::is_aligned(base) && kernel_page::is_aligned(size));
        debug_assert!((new_arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) == 0);
        debug_assert!(self.is_valid_mapping_flags(new_arch_mmu_flags));

        let object = self
            .object_locked(aspace_token)
            .as_ref()
            .map(RefPtr::as_ptr)
            .expect("object must be set");
        // SAFETY: `self` holds a reference to `object` that is only dropped with the aspace lock
        // held, and `aspace_token` holds the aspace lock until this function returns.
        let object = unsafe { object.as_ref_unchecked() };
        // grab the lock for the vmo
        ksync::lock!(let mut guard = object.lock_ref().lock());
        let object_token = guard.as_mut().token_mut();

        // Persist our current caching mode. Every protect region will have the same caching mode so
        // we can acquire this from any region.
        let new_arch_mmu_flags = new_arch_mmu_flags
            | (self.first_region_arch_mmu_flags_locked(aspace_token) & ARCH_MMU_FLAG_CACHE_MASK);

        // This will get called by `enumerate_protection_ranges_locked` below for every existing
        // unique protection range that gets changed and allows us to fine tune the protect action
        // based on the previous flags.
        let protect_callback = move |base: usize, size: usize, old_arch_mmu_flags: ArchMmuFlags| {
            // Perform an early return if the new and old flags are the same, as there's nothing to
            // be done.
            if new_arch_mmu_flags == old_arch_mmu_flags {
                return Err(Status::NEXT);
            }

            let mut flags = new_arch_mmu_flags;
            // Check if the new flags have the write permission. This is problematic as we cannot
            // just change any existing hardware mappings to have the write permission, as any
            // individual mapping may be the result of a read fault and still need to have a
            // copy-on-write step performed. This could also map a dirty tracked VMO which requires
            // write permission faults to track pages as dirty when written.
            if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
                // Whatever happens, we're not going to be protecting the arch aspace to have write
                // mappings, so this has to be a user aspace so that we can lazily take write faults
                // in the future.
                assert!(self.aspace().is_user() || self.aspace().is_guest_physical());
                flags &= !ARCH_MMU_FLAG_PERM_WRITE;
                VM_MAPPINGS_PROTECT_NO_WRITE.add(1);
                // If the new flags without write permission are the same as the old flags, then
                // skip the protect step since it will be a no-op.
                if flags == old_arch_mmu_flags {
                    return Err(Status::NEXT);
                }
            }

            // SAFETY: `base` and `size` are page aligned, and `[base, base + size)` is part of a
            // mapping in `self.aspace()` whose permissions may be changed to `flags`, and which
            // may be unmapped.
            let status = unsafe { Self::protect_or_unmap(self.aspace(), base, size, flags) };
            // If the protect failed then we do not have sufficient information left to rollback in
            // order to return an error, nor can we claim success, so require the protect to have
            // succeeded to continue.
            assert!(status.is_ok());
            Err(Status::NEXT)
        };

        // To handle the protection change, we need to ensure that the start and end of the
        // requested range are represented as boundary nodes in the protection ranges B-tree if
        // they are not already. This ensures that any change in protections for the sub-range does
        // not unintentionally leak into adjacent sub-ranges.

        // Efficiently calculate the flags before, at, and after the requested range using
        // minimal BTree searches.
        let mut flags_before = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut flags_at_base = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut flags_end = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut base_exists = false;

        let mut it_base = rest_protection_ranges.upper_bound(base);
        // SAFETY: `it_base` is not before the first item.
        unsafe { it_base.prev() };
        if let Some((addr, flags)) = it_base.get() {
            if addr == base {
                base_exists = true;
                flags_at_base = flags;
                // SAFETY: `it_base` is not before the first item.
                unsafe { it_base.prev() };
                if let Some((_prev_addr, prev_flags)) = it_base.get() {
                    flags_before = prev_flags;
                }
            } else {
                flags_at_base = flags;
                flags_before = flags_at_base;
            }
        }

        let mut end_exists = false;

        let mut it_end = rest_protection_ranges.upper_bound(base + size);
        // SAFETY: `it_end` is not before the first item.
        unsafe { it_end.prev() };
        if let Some((addr, flags)) = it_end.get() {
            if addr == base + size {
                end_exists = true;
                flags_end = flags;
            } else {
                flags_end = flags;
            }
        }

        // We only need a boundary node at `base` if it's not the start of the mapping and the
        // preceding flags are different from the new ones.
        let need_base = base > self.base().0 && flags_before != new_arch_mmu_flags;
        let insert_base = need_base && !base_exists;

        // Similarly, we need a boundary node at `base + size` if it's not the end of the mapping
        // and the new flags for the range are different from the existing flags that follow it.
        let need_end = base + size < self.base().0 + self.size() && new_arch_mmu_flags != flags_end;
        let insert_end = need_end && !end_exists;

        // Perform any required insertions first. By inserting the *old* flags initially, we make
        // the insertion a structural change only (a no-op for effective permissions). This allows
        // us to handle OOM errors before we begin any non-reversible mutations.
        if insert_base {
            // SAFETY: `base` is not present.
            let it = unsafe { rest_protection_ranges.insert(base, flags_at_base) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }
        if insert_end {
            // SAFETY: `base + size` is not present.
            let it = unsafe { rest_protection_ranges.insert(base + size, flags_end) };
            if !it.is_valid() {
                hint::cold_path();
                if insert_base {
                    // SAFETY: `base` is present in `rest_protection_ranges`.
                    unsafe { rest_protection_ranges.find_mut(base).erase() };
                }
                return Err(Status::NO_MEMORY);
            }
        }

        // Now that all structural nodes are guaranteed to exist, we can safely perform the
        // architectural protection changes. Calling this after successful insertions ensures
        // that we don't have to rollback MMU changes if an allocation fails.
        let _ = self.enumerate_protection_ranges_locked(aspace_token, base, size, protect_callback);

        // Update the protection flags for the start of the range.
        if base == self.base().0 {
            self.set_first_region_arch_mmu_flags_locked(
                aspace_token,
                object_token,
                new_arch_mmu_flags,
            );
        } else if need_base {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            rest_protection_ranges.find_mut(base).update(new_arch_mmu_flags);
        } else {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            // If the boundary node is no longer needed (e.g., flags now match), remove it.
            let mut it = rest_protection_ranges.find_mut(base);
            if it.is_valid() {
                // SAFETY: `it` is valid.
                unsafe { it.erase() };
            }
        }

        // If a boundary node at `base + size` is not needed, remove it to keep the tree minimal.
        if !need_end {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            let mut it = rest_protection_ranges.find_mut(base + size);
            if it.is_valid() {
                // SAFETY: `it` is valid.
                unsafe { it.erase() };
            }
        }

        // Finally, erase any internal boundary nodes that were entirely within the protected
        // range, as the entire range `[base, base + size)` now has uniform protections.
        self.clear_protection_range_transitions_locked(
            aspace_token,
            object_token,
            base + 1,
            base + size,
        );

        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    fn create_with_ranges(
        parent: &VmAddressRegion,
        private_clone: bool,
        base: usize,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        first_mmu_flags: ArchMmuFlags,
        ranges: Pin<&mut ProtectionRanges>,
        mergeable: Mergeable,
    ) -> Option<RefPtr<VmMapping>> {
        let init = Self::new_with_ranges(
            parent,
            private_clone,
            base,
            size,
            vmar_flags,
            vmo,
            vmo_offset,
            first_mmu_flags,
            ranges,
            mergeable,
        );
        RefPtr::pin_init(init).ok()
    }

    /// Unmap a subset of the region of memory in the containing address space,
    /// returning it to the parent region to allocate. If a subrange of the mapping is
    /// specified, the mapping may be split.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, and `[base, base + size)` is a
    /// strict subrange of `[this.base(), this.base() + this.size())` that may be unmapped.
    pub(super) unsafe fn unmap_locked(
        this: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
    ) -> Result<(), Status> {
        // `this` keeps us alive in case we drop the last reference when removing from our parent.
        this.state().canary.assert();
        debug_assert!(size != 0 && kernel_page::is_aligned(size) && kernel_page::is_aligned(base));
        debug_assert!(base >= this.base().0 && base - this.base().0 < this.size());
        debug_assert!(this.size() - (base - this.base().0) >= size);
        debug_assert!(!this.parent_locked(aspace_token).is_null());

        if this.state_locked(aspace_token) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        // SAFETY: The parent points to a live `VmAddressRegion`.
        let parent = unsafe { &*this.parent_locked(aspace_token) };
        debug_assert!(parent.lock_is_held());
        debug_assert!(parent.region_lock_is_held());

        // Should never be unmapping everything, otherwise should destroy.
        debug_assert!(base != this.base().0 || size != this.size());

        ltracef!("{:p}\n", &**this);

        // First create any new mapping. One or two might be required depending on whether unmapping
        // from an end or the middle.
        let mut left = None;
        let mut right = None;
        if this.base().0 != base {
            // Insert empty protection ranges for now as we will transform the existing
            // protection ranges tree into the target.
            let vmo = this.object_locked(aspace_token).clone().expect("object must be set");
            stack_pin_init!(let left_prot_ranges = ProtectionRanges::new());
            left = Self::create_with_ranges(
                parent,
                this.state().private_clone,
                this.base().0,
                base - this.base().0,
                this.flags(),
                vmo,
                this.object_offset(),
                0,
                left_prot_ranges,
                Mergeable::Yes,
            );
            if left.is_none() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }
        if base + size != this.base().0 + this.size() {
            // If we also have a left mapping then we need to build a copy of the subset of the
            // protection ranges for this right mapping. We do this first here as this might need to
            // allocate and can fail.
            let mut right_first_mmu_flags: ArchMmuFlags = 0;
            stack_pin_init!(let right_prot_ranges = ProtectionRanges::new());
            let offset = base + size - this.base().0;
            if left.is_some() {
                // SAFETY: `this.size() - offset` is not zero,
                // `[base + size, this.base().0 + this.size())` is within `this`, and
                // `right_prot_ranges` is empty.
                right_first_mmu_flags = unsafe {
                    this.copy_protection_ranges_locked(
                        aspace_token,
                        base + size,
                        this.size() - offset,
                        right_prot_ranges.as_mut(),
                    )
                }?;
            }
            let vmo = this.object_locked(aspace_token).clone().expect("object must be set");
            right = Self::create_with_ranges(
                parent,
                this.state().private_clone,
                this.base().0 + offset,
                this.size() - offset,
                this.flags(),
                vmo,
                this.object_offset() + offset as u64,
                right_first_mmu_flags,
                right_prot_ranges,
                Mergeable::Yes,
            );
            if right.is_none() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }

        // Grab the lock for the vmo. This is acquired here so that it is held continuously over
        // both the architectural unmap and removing the current mapping from the VMO.
        let object = this
            .object_locked(aspace_token)
            .as_ref()
            .map(RefPtr::as_ptr)
            .expect("object must be set");
        // SAFETY: `object` points to a live `VmObject` that is kept alive for the duration of this
        // function by `this` (until `destroy_locked_object`) and by the cloned `RefPtr<VmObject>`
        // in at least one of `left` or `right`.
        let object = unsafe { object.as_ref_unchecked() };
        ksync::lock!(let mut guard = object.lock_ref().lock());
        let object_token = guard.as_mut().token_mut();

        // With the object allocations done the last action that could fail is insertion into the
        // subregions list and object mapping list. Notionally we want to remove the old mapping,
        // then install our new mapping(s), but if our installation fails then we cannot necessarily
        // rollback and install the old mapping. To work around this we instead first install any
        // right mapping, which could fail and then if there is a left mapping replace the current
        // mapping, which cannot fail. This approach has the result of temporarily causing the
        // subregions list to have an overlapping entry and for the parent to have unactivated
        // mappings, however we hold both the main and subregion lock over the entire operation,
        // and so this is never visible. Similarly we hold the object lock over the entire
        // manipulation of mappings, and so the mappings being added before being in the alive
        // state is never visible.
        if let Some(right) = &right {
            // SAFETY: No child of `parent` has the same base as `right`.
            unsafe { region_list::insert_region(parent, region_token, aspace_token, right)? };
            if let Err(status) = object.add_mapping_locked(object_token, right) {
                hint::cold_path();
                // SAFETY: `right` is a child of `parent` and is kept alive by another reference.
                unsafe { region_list::remove_region(parent, region_token, aspace_token, right) };
                return Err(status);
            }
        }
        if let Some(left) = &left {
            if let Err(status) = object.add_mapping_locked(object_token, left) {
                hint::cold_path();
                if let Some(right) = &right {
                    object.remove_mapping_locked(object_token, right);
                    // SAFETY: `right` is a child of `parent` and is kept alive by another
                    // reference.
                    unsafe {
                        region_list::remove_region(parent, region_token, aspace_token, right)
                    };
                }
                return Err(status);
            }
            // Replace can never fail as it does not need to allocate.
            // SAFETY: `this` is a child of `parent` and is kept alive by another owning reference,
            // and `left` has the same base as `this` and does not overlap with any other child of
            // `parent`.
            unsafe { region_list::replace_region(parent, region_token, aspace_token, this, left) };
        } else {
            // SAFETY: `this` is a child of `parent` and is kept alive by another owning reference.
            unsafe { region_list::remove_region(parent, region_token, aspace_token, this) };
        }

        // SAFETY: `[base, base + size)` may be unmapped.
        let status = unsafe {
            this.aspace().arch_aspace().unmap(
                base,
                size / kernel_page::SIZE,
                this.aspace().enlarge_arch_unmap(),
            )
        };
        assert!(status.is_ok());

        let old_priority = this.memory_priority_locked(aspace_token);
        let set_priority =
            |mapping: &VmMapping,
             aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
             object_token: &LockToken<'_, VmObjectLockClass>| {
                if old_priority == MemoryPriority::High {
                    hint::cold_path();
                    mapping.set_memory_priority_high_already_positive_locked_object::<true>(
                        aspace_token,
                        object_token,
                    );
                } else {
                    debug_assert!(old_priority == MemoryPriority::Default);
                    mapping.set_memory_priority_default_locked_object::<true>(
                        aspace_token,
                        object_token,
                    );
                }
            };

        // Split the protection ranges from this mapping into the new mapping(s). This has to be
        // done after the mapping construction as this step is destructive and hard to rollback.
        //
        // Need to set memory priorities before we call `destroy_locked_object`. If we have
        // `MemoryPriority::High`, then we need to pass that on to left and right before the object
        // and the aspace suffer any dynamic reclamation.
        if let Some(right) = &right {
            debug_assert!(right.lock_is_held());
            debug_assert!(right.object_lock_is_held(aspace_token));
            // If there was a left mapping we already populated new protection ranges above when we
            // created the right mapping, for the other case we need to erase the unneeded parts of
            // our protection ranges and move it over.
            if left.is_none() {
                let flags = this.remove_after_split_locked(aspace_token, object_token, base + size);
                right.set_first_region_arch_mmu_flags_locked(aspace_token, object_token, flags);
                // SAFETY: `right` is not `this`.
                let (mut to, mut from) = unsafe {
                    right.rest_protection_ranges_pair_mut_locked(this, aspace_token, object_token)
                };
                to.move_from(&mut from);
            }
            set_priority(right, aspace_token, object_token);
        }
        if let Some(left) = &left {
            debug_assert!(left.lock_is_held());
            debug_assert!(left.object_lock_is_held(aspace_token));
            // Erase the top part of the protection ranges and then move it to the left mapping.
            this.clear_protection_range_transitions_locked(
                aspace_token,
                object_token,
                base,
                this.base().0 + this.size(),
            );
            left.set_first_region_arch_mmu_flags_locked(
                aspace_token,
                object_token,
                this.first_region_arch_mmu_flags_locked(aspace_token),
            );
            // SAFETY: `left` is not `this`.
            let (mut to, mut from) = unsafe {
                left.rest_protection_ranges_pair_mut_locked(this, aspace_token, object_token)
            };
            to.move_from(&mut from);
            set_priority(left, aspace_token, object_token);
        }

        // Now finish destroying this mapping. We already updated the subregion list in the
        // parent.
        let status = Self::destroy_locked_object(
            this,
            region_token,
            aspace_token,
            object_token,
            DestroyUnmap::No,
            DestroyRemoveFromParent::No,
        );
        assert!(status.is_ok());

        // Install the new mappings.
        let mut finish_mapping = |mapping: &Option<RefPtr<VmMapping>>| {
            if let Some(mapping) = mapping {
                debug_assert!(mapping.lock_is_held());
                debug_assert!(mapping.object_lock_is_held(aspace_token));
                debug_assert!(mapping.region_lock_is_held());
                // We already updated the parent region list and object mapping list.
                mapping.activate_no_insert_locked(region_token, aspace_token, object_token);
            }
        };
        finish_mapping(&left);
        finish_mapping(&right);
        Ok(())
    }

    /// Attempts to merge this and the given mapping into a new one. This only succeeds if the
    /// candidate is placed just after `this`, both in the aspace and the vmo. See implementation
    /// for the full requirements for merging to succeed. The candidate and this must be held as a
    /// `RefPtr` by the caller so that this function does not trigger any `VmMapping` destructor by
    /// dropping the last reference when removing from the parent vmar. If merging is successful
    /// the newly created and installed mapping is returned, otherwise `None` is returned.
    ///
    /// # Safety
    ///
    /// `this` and `right_candidate` must have the same parent.
    unsafe fn try_merge_right_neighbor_locked(
        this: &RefPtr<Self>,
        right_candidate: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Option<RefPtr<VmMapping>> {
        // The caller's `RefPtr` keeps us alive in case we drop the last one when erasing from our
        // parent.
        debug_assert!(right_candidate.lock_is_held());
        debug_assert!(right_candidate.region_lock_is_held());

        // This code is tolerant of many 'miss calls' if mappings aren't mergeable or are not
        // neighbors etc, but the caller should not be attempting to merge if these mappings are
        // not actually from the same vmar parent. Doing so indicates something structurally wrong
        // with the hierarchy.
        debug_assert!(
            this.parent_locked(aspace_token) == right_candidate.parent_locked(aspace_token)
        );

        // Should not be able to have the same parent yet have gotten a different memory priority.
        debug_assert!(
            this.memory_priority_locked(aspace_token)
                == right_candidate.memory_priority_locked(aspace_token)
        );

        // These tests are intended to be ordered such that we fail as fast as possible. As such
        // testing for mergeability, which we commonly expect to succeed and not fail, is done last.

        // Need to refer to the same object.
        if this.object_locked(aspace_token).as_ref().map_or(ptr::null(), RefPtr::as_ptr)
            != right_candidate
                .object_locked(aspace_token)
                .as_ref()
                .map_or(ptr::null(), RefPtr::as_ptr)
        {
            return None;
        }
        debug_assert!(this.state().private_clone == right_candidate.state().private_clone);
        // Aspace and VMO ranges need to be contiguous. Validate that the right candidate is
        // actually to the right in addition to checking that base+size lines up for single
        // scenario where `base() + size()` can overflow and becomes zero.
        if this.base().0.wrapping_add(this.size()) != right_candidate.base().0
            || right_candidate.base().0 < this.base().0
        {
            return None;
        }
        if this.object_offset().wrapping_add(this.size() as u64) != right_candidate.object_offset()
        {
            return None;
        }
        // All flags need to be consistent.
        if this.flags() != right_candidate.flags() {
            return None;
        }
        // Although we can combine the rest protection ranges of the two mappings, we require
        // that they be of the same cacheability, as this is an assumption that mapping has a single
        // cacheability type. Since all protection regions have the same cacheability we can check
        // any arbitrary one in each of the mappings. Note that this check is technically
        // redundant, since a VMO can only have one kind of cacheability and we already know this
        // is the same VMO, but some extra paranoia here does not hurt.
        if (this.first_region_arch_mmu_flags_locked(aspace_token) & ARCH_MMU_FLAG_CACHE_MASK)
            != (right_candidate.first_region_arch_mmu_flags_locked(aspace_token)
                & ARCH_MMU_FLAG_CACHE_MASK)
        {
            hint::cold_path();
            return None;
        }

        // Only merge live mappings.
        if this.state_locked(aspace_token) != LifeCycleState::Alive
            || right_candidate.state_locked(aspace_token) != LifeCycleState::Alive
        {
            hint::cold_path();
            return None;
        }
        // Both need to be mergeable.
        if this.mergeable_locked(aspace_token) == Mergeable::No
            || right_candidate.mergeable_locked(aspace_token) == Mergeable::No
        {
            return None;
        }

        // SAFETY: The parent points to a live `VmAddressRegion`.
        let parent = unsafe { &*this.parent_locked(aspace_token) };
        // SAFETY: An `Alive` mapping always has a backing `object`.
        let vmo = unsafe { this.object_locked(aspace_token).clone().unwrap_unchecked() };
        let object = RefPtr::as_ptr(&vmo);
        stack_pin_init!(let prot_ranges = ProtectionRanges::new());
        let Some(new_mapping) = Self::create_with_ranges(
            parent,
            this.state().private_clone,
            this.base().0,
            this.size() + right_candidate.size(),
            this.flags(),
            vmo,
            this.object_offset(),
            0,
            prot_ranges,
            Mergeable::Yes,
        ) else {
            hint::cold_path();
            return None;
        };
        debug_assert!(new_mapping.lock_is_held());

        let old_priority = this.memory_priority_locked(aspace_token);
        {
            // SAFETY: `object` points to a live `VmObject` that is kept alive across both
            // `destroy_locked_object` calls by the cloned `RefPtr<VmObject>` in `new_mapping`.
            let object = unsafe { object.as_ref_unchecked() };
            // Although it was safe to read `size()` without holding the object lock, we need to
            // acquire it to perform changes.
            ksync::lock!(let mut guard = object.lock_ref().lock());
            let object_token = guard.as_mut().token_mut();

            debug_assert!(new_mapping.object_lock_is_held(aspace_token));
            if object.add_mapping_locked(object_token, &new_mapping).is_err() {
                hint::cold_path();
                return None;
            }

            // Attempt to copy all the protection ranges first as this might fail due to an
            // allocation. If it fails we can still abort with a fairly minor roll-back
            // procedure.
            // SAFETY: `right_candidate` is not `this`, and `right_candidate.base()` and the
            // keys of the protection ranges of `right_candidate` are distinct from each other
            // and absent from the protection ranges of `this`.
            if unsafe {
                this.merge_protection_ranges_locked(aspace_token, object_token, right_candidate)
            }
            .is_err()
            {
                hint::cold_path();
                object.remove_mapping_locked(object_token, &new_mapping);
                // On failure roll back any protection ranges that might have been inserted.
                // Erase cannot fail.
                this.clear_protection_range_transitions_locked(
                    aspace_token,
                    object_token,
                    this.base().0.wrapping_add(this.size()),
                    this.base().0.wrapping_add(this.size()).wrapping_add(right_candidate.size()),
                );
                return None;
            }

            debug_assert!(this.region_lock_is_held());

            new_mapping.set_first_region_arch_mmu_flags_locked(
                aspace_token,
                object_token,
                this.first_region_arch_mmu_flags_locked(aspace_token),
            );
            // SAFETY: `new_mapping` is not `this`.
            let (mut to, mut from) = unsafe {
                new_mapping.rest_protection_ranges_pair_mut_locked(this, aspace_token, object_token)
            };
            to.move_from(&mut from);

            debug_assert!(right_candidate.region_lock_is_held());
            // First destroy the right hand mapping, and remove it from the parent.
            let status = Self::destroy_locked_object(
                right_candidate,
                region_token,
                aspace_token,
                object_token,
                DestroyUnmap::No,
                DestroyRemoveFromParent::Yes,
            );
            assert!(status.is_ok());
            debug_assert!(parent.lock_is_held());
            debug_assert!(parent.region_lock_is_held());
            // To avoid `activate_locked` from failing we use `replace_region` to swap the left
            // mapping for the new mapping in the subregions list. This temporarily results in
            // the subregions list having overlapping mappings and an unactivated mapping, but
            // as we hold both the main lock and subregion lock over the entire operation this
            // state cannot be observed.
            // SAFETY: `this` is a child of `parent` and is kept alive by another owning reference,
            // and `new_mapping` has the same base as `this` and does not overlap with any other
            // child of `parent`.
            unsafe {
                region_list::replace_region(parent, region_token, aspace_token, this, &new_mapping)
            };
            let status = Self::destroy_locked_object(
                this,
                region_token,
                aspace_token,
                object_token,
                DestroyUnmap::No,
                DestroyRemoveFromParent::No,
            );
            assert!(status.is_ok());
            debug_assert!(new_mapping.region_lock_is_held());
            new_mapping.activate_no_insert_locked(region_token, aspace_token, object_token);
        }

        new_mapping.set_memory_priority_locked::<false>(aspace_token, old_priority);

        VM_MAPPINGS_MERGED.add(1);
        Some(new_mapping)
    }

    /// Attempts to merge this mapping with any neighbors. It is the responsibility of the caller to
    /// ensure a `RefPtr` to this is being held, as on return `this` may be in the dead state and
    /// have removed itself from the hierarchy, dropping a `RefPtr`.
    #[inline]
    fn try_merge_neighbors_locked(
        this: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) {
        this.state().canary.assert();

        // Check that this mapping is mergeable and is currently in the correct lifecycle state.
        if this.mergeable_locked(aspace_token) == Mergeable::No
            || this.state_locked(aspace_token) != LifeCycleState::Alive
        {
            hint::cold_path();
            return;
        }
        // As a `VmMapping` if we are alive we by definition have a parent.
        debug_assert!(!this.parent_locked(aspace_token).is_null());

        // We expect there to be a `RefPtr` to us held beyond the one for the wavl tree ensuring
        // that we cannot trigger our own destructor should we remove ourselves from the hierarchy.
        debug_assert!(this.ref_count().ref_count_debug() > 1);

        // SAFETY: The parent points to a live `VmAddressRegion`.
        let parent = unsafe { &*this.parent_locked(aspace_token) };
        debug_assert!(parent.lock_is_held());
        debug_assert!(parent.region_lock_is_held());

        // Find our two merge candidates.
        // SAFETY: `this` is a child of `parent`.
        let left = unsafe { region_list::left_of(parent, aspace_token, this) }
            .and_then(VmAddressRegionOrMapping::as_vm_mapping);
        // SAFETY: `this` is a child of `parent`.
        let mut right = unsafe { region_list::right_of(parent, aspace_token, this) }
            .and_then(VmAddressRegionOrMapping::as_vm_mapping);

        // Attempt to merge with each candidate. Any successful merge will produce a new mapping and
        // invalidate this.
        if let Some(candidate) = right {
            // SAFETY: `this` and `candidate` are both children of `parent`.
            right = unsafe {
                Self::try_merge_right_neighbor_locked(this, &candidate, region_token, aspace_token)
            };
        }
        if let Some(left) = left {
            // We either merge the left with our result of the right merge, or if that was not
            // successful with `this`.
            debug_assert!(left.lock_is_held());
            debug_assert!(left.region_lock_is_held());
            // SAFETY: `left` and `right.as_ref().unwrap_or(this)` are both children of `parent`.
            unsafe {
                Self::try_merge_right_neighbor_locked(
                    &left,
                    right.as_ref().unwrap_or(this),
                    region_token,
                    aspace_token,
                )
            };
        }
    }

    #[inline(never)]
    pub(super) fn dump_locked(
        &self,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        token: &LockToken<'_, VmAspaceLockClass>,
        depth: u32,
        verbose: bool,
    ) {
        self.state().canary.assert();
        for _ in 0..depth {
            kprint::kprint!("  ");
        }
        let object = self.object_locked(token).as_ref().unwrap();
        let mut vmo_name = [0u8; 32];
        object.get_name(&mut vmo_name);

        kprint::kprint!(
            "map {:p} [{:#x} {:#x}] sz {:#x} state {} mergeable {:s}\n",
            self as *const _,
            self.base().0,
            self.base().0 + self.size() - 1,
            self.size(),
            self.state_locked(token) as i32,
            if self.mergeable_locked(token) == Mergeable::Yes { "true" } else { "false" }
        );

        let _ = self.enumerate_protection_ranges_locked(
            token,
            self.base().0,
            self.size(),
            |base, len, mmu_flags| {
                for _ in 0..(depth + 1) {
                    kprint::kprint!("  ");
                }
                kprint::kprint!(" [{:#x} {:#x}] mmufl {:#x}\n", base, base + len - 1, mmu_flags);
                Err(Status::NEXT)
            },
        );

        for _ in 0..(depth + 1) {
            kprint::kprint!("  ");
        }
        let counts =
            object.get_attributed_memory_in_range(self.object_offset(), self.size() as u64);
        kprint::kprint!(
            "vmo {:p}/k{} off {:#x} bytes ({}/{}) ref {} '{:cs}'\n",
            object.as_raw(),
            object.user_id(),
            self.object_offset(),
            counts.uncompressed_bytes,
            counts.compressed_bytes,
            self.ref_count().ref_count_debug(),
            vmo_name.as_ptr()
        );
        if verbose {
            object.dump(depth + 1, false);
        }
    }

    /// # Safety
    ///
    /// No child of the parent has the same base as `self`, and `self` is kept alive by another
    /// owning reference for the duration of the call.
    pub(super) unsafe fn activate(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        let object: *const VmObject =
            self.object_locked(aspace_token).as_deref().expect("object must be set");
        // SAFETY: `object` is kept alive by the object reference of `self`, which `activate_locked`
        // does not reset.
        let object = unsafe { &*object };
        ksync::lock!(let mut guard = object.lock_ref().lock());
        // SAFETY: No child of the parent has the same base as `self`, and `self` is kept alive by
        // another owning reference for the duration of the call.
        unsafe {
            self.activate_locked(
                region_token,
                aspace_token,
                guard.as_mut().token_mut(),
                ActivateInsertRegions::Yes,
            )
        }
    }

    /// Fully locked version of `activate` that can additionally control whether the region is
    /// installed into the parent subregion list and vmo mapping list or not. This control exists
    /// to facilitate the fine grained control needed for splitting, merging and replacing of
    /// mappings as when set to `No` this method is defined as never failing.
    ///
    /// # Safety
    ///
    /// If `insert_region` is `Yes`, no child of the parent has the same base as `self`, and `self`
    /// is kept alive by another owning reference for the duration of the call.
    unsafe fn activate_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        insert_region: ActivateInsertRegions,
    ) -> Result<(), Status> {
        debug_assert!(self.state_locked(aspace_token) == LifeCycleState::NotReady);
        let parent_ptr = self.parent_locked(aspace_token);
        debug_assert!(!parent_ptr.is_null());

        // SAFETY: `parent_ptr` is valid for reads.
        let parent = unsafe { &*parent_ptr };
        debug_assert!(parent.lock_is_held());
        debug_assert!(parent.region_lock_is_held());
        // If inserting into the regions attempt that first before modifying any state, as it can
        // fail.
        if insert_region == ActivateInsertRegions::Yes {
            // SAFETY: No child of `parent` has the same base as `self`.
            unsafe { region_list::insert_region(parent, region_token, aspace_token, self)? };

            let object = self.object_locked(aspace_token).as_ref().unwrap();
            if let Err(status) = object.add_mapping_locked(object_token, self) {
                // SAFETY: `self` is a child of `parent` and is kept alive by another owning
                // reference for the duration of the call.
                unsafe { region_list::remove_region(parent, region_token, aspace_token, self) };
                return Err(status);
            }
        }

        self.set_state_locked(region_token, aspace_token, LifeCycleState::Alive);

        // Now that we have added a mapping to the VMO it's cache policy becomes fixed, and we can
        // read it and augment our `arch_mmu_flags`.
        let object = self.object_locked(aspace_token).as_ref().unwrap();
        let cache_policy = object.get_mapping_cache_policy_locked(object_token);
        let mut arch_mmu_flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        if (arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != cache_policy {
            // Warn in the event that we somehow receive a VMO that has a cache
            // policy set while also holding cache policy flags within the arch
            // flags. The only path that should be able to achieve this is if
            // something in the kernel maps into their aspace incorrectly.
            if (arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != 0 {
                tracef!(
                    "warning: mapping has conflicting cache policies: \
                     vmo {:#02x} arch_mmu_flags {:#02x}.\n",
                    cache_policy,
                    arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK
                );
                // Clear the existing cache policy and use the new one.
                arch_mmu_flags &= !ARCH_MMU_FLAG_CACHE_MASK;
            }
            // If we are changing the cache policy then this can only happen if this is a new
            // mapping region and not a new mapping occurring as a result of an unmap split. In the
            // case of a new mapping region we know there cannot yet be any protection ranges.
            debug_assert!(self.rest_protection_ranges_locked(aspace_token).is_empty());
            arch_mmu_flags |= cache_policy;
            self.set_first_region_arch_mmu_flags_locked(aspace_token, object_token, arch_mmu_flags);
        }
        Ok(())
    }

    /// Wrapper for `activate_locked` that does not insert into the parent region, and
    /// therefore cannot fail.
    #[inline]
    fn activate_no_insert_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: `insert_region` is `No`.
        let status = unsafe {
            self.activate_locked(
                region_token,
                aspace_token,
                object_token,
                ActivateInsertRegions::No,
            )
        };
        assert!(status.is_ok());
    }

    pub(super) fn destroy_locked(
        this: RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        this.state().canary.assert();
        // Keep a `RefPtr` to the `object` so we know our lock remains valid.
        let object = this.object_locked(aspace_token).clone().expect("object must be set");
        ksync::lock!(let mut guard = object.lock_ref().lock());
        Self::destroy_locked_object(
            &this,
            region_token,
            aspace_token,
            guard.as_mut().token_mut(),
            DestroyUnmap::Yes,
            DestroyRemoveFromParent::Yes,
        )
    }

    /// Internal fully locked version of `destroy`. Has controls to both skip the arch aspace
    /// unmapping as well as removal from the parent subregions list. These controls facilitate
    /// the fine grained control needed when splitting, merging and replacing mappings.
    /// If `unmap` is `No` then this method is defined to never fail. `remove_region` does not
    /// impact success or failure of the operation.
    fn destroy_locked_object(
        this: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        unmap: DestroyUnmap,
        remove_region: DestroyRemoveFromParent,
    ) -> Result<(), Status> {
        // The caller's `RefPtr` keeps us alive after we drop our last other reference in this
        // method (e.g. when removing it from the parent's subregions below).
        let this: &Self = this;

        // If this is the `last_fault` then clear it before removing from the VMAR tree.
        // Even if this destroy fails, it's always safe to clear `last_fault`, so we preference
        // doing it upfront for clarity.
        if core::ptr::eq(this.aspace().last_fault_locked(aspace_token), this) {
            this.aspace().clear_last_fault_locked(aspace_token);
        }

        // The vDSO code mapping can never be unmapped, not even
        // by VMAR destruction (except for process exit, of course).
        // TODO(mcgrathr): Turn this into a policy-driven process-fatal case
        // at some point.  teisenbe@ wants to eventually make `zx_vmar_destroy`
        // never fail.
        if core::ptr::eq(this.aspace().vdso_code_mapping_locked(aspace_token), this) {
            return Err(Status::ACCESS_DENIED);
        }

        if unmap == DestroyUnmap::Yes {
            // SAFETY: The virtual range of this mapping is no longer in use.
            unsafe {
                this.aspace().arch_aspace().unmap(
                    this.base().0,
                    this.size() / kernel_page::SIZE,
                    this.aspace().enlarge_arch_unmap(),
                )
            }?;
        }

        // Remove any priority.
        this.set_memory_priority_default_locked_object::<false>(aspace_token, object_token);

        this.rest_protection_ranges_mut_locked(aspace_token, object_token).clear();
        let object = this.object_locked(aspace_token).as_ref().expect("object must be set");
        object.remove_mapping_locked(object_token, this);

        // Detach the region from the parent.
        let parent_ptr = this.parent_locked(aspace_token);
        if !parent_ptr.is_null() && remove_region == DestroyRemoveFromParent::Yes {
            // SAFETY: `parent_ptr` is valid for reads.
            let parent = unsafe { &*parent_ptr };
            debug_assert!(parent.lock_is_held());
            debug_assert!(parent.region_lock_is_held());
            // SAFETY: `this` is a child of `parent` and is kept alive by another owning
            // reference for the duration of the call.
            unsafe { region_list::remove_region(parent, region_token, aspace_token, this) };
        }

        // detach from any object we have mapped. Note that we are holding the `aspace.lock()`
        // so we will not race with other threads calling `vmo()`
        this.reset_object_locked(aspace_token, object_token);

        // mark ourself as dead
        this.set_parent_locked(aspace_token, core::ptr::null_mut());
        this.set_state_locked(region_token, aspace_token, LifeCycleState::Dead);
        VM_MAPPINGS_STATE_DEAD.add(1);
        Ok(())
    }

    fn destruct(&self) {
        self.state().canary.assert();
        ltracef!(
            "{:p} aspace {:p} base {:#x} size {:#x}\n",
            self as *const _,
            &**self.aspace(),
            self.base().0,
            self.size()
        );
        VM_MAPPINGS_STATE_DEAD.add(-1);
    }

    /// If `MemoryPriority::High`, then disable dynamic reclamation within this region. If
    /// `MemoryPriority::Default`, move towards allowing reclamation.
    ///
    /// When called with `SPLIT_ON_UNMAP = true`, we set the memory
    /// priority assuming the object has not been `activate`'d yet.
    #[inline(never)]
    pub(super) fn set_memory_priority_locked<const SPLIT_ON_UNMAP: bool>(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(token) == LifeCycleState::NotReady
                    && self.object_locked(token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(token) == LifeCycleState::Alive);
        }
        let to_high = priority == MemoryPriority::High;
        let delta: i64 = if to_high { 1 } else { -1 };
        if priority == self.memory_priority_locked(token) {
            return;
        }
        self.set_memory_priority_field_locked(token, priority);
        self.aspace().change_high_priority_count_locked(token, delta);
        let object = self.object_locked(token).as_deref().unwrap();
        if let Some(paged) = object.as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(delta));
            if priority == MemoryPriority::High {
                pc.as_mut().prepare_may_not_already_be_high_priority();
            }
            ksync::lock!(let guard = ksync::aliased_lock(object.lock_ref(), pc.lock()));
            let (_, cow_token) = guard.tokens();
            pc.as_mut().change_high_priority_count_locked(cow_token);
        }
    }

    /// Move towards allowing dynamic reclamation in the region. You may call this method with the
    /// `object` lock, in contrast to `set_memory_priority_locked`.
    ///
    /// When called with `set_memory_priority_default_locked_object::<true>`, we set the memory
    /// priority assuming the object has not been `activate`'d yet.
    fn set_memory_priority_default_locked_object<const SPLIT_ON_UNMAP: bool>(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(aspace_token) == LifeCycleState::NotReady
                    && self.object_locked(aspace_token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(aspace_token) == LifeCycleState::Alive);
        }
        if self.memory_priority_locked(aspace_token) == MemoryPriority::Default {
            return;
        }
        self.set_memory_priority_field_locked(aspace_token, MemoryPriority::Default);
        self.aspace().change_high_priority_count_locked(aspace_token, -1);
        if let Some(paged) = self.object_locked(aspace_token).as_deref().unwrap().as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(-1));
            debug_assert!(paged.cow_pages().lock_is_held()); // we have the object lock
            pc.as_mut().change_high_priority_count_locked(object_token);
        }
    }

    /// Marks the address region as high priority. Only call this method if the mapped
    /// `VmObject` is already high priority (for example if you have already called
    /// `set_memory_priority_locked` on a child).
    ///
    /// You may call this method with the object lock, in contrast to
    /// `set_memory_priority_locked`.
    ///
    /// When called with `set_memory_priority_high_already_positive_locked_object::<true>`, we set
    /// the memory priority assuming the object has not been `activate`'d yet.
    fn set_memory_priority_high_already_positive_locked_object<const SPLIT_ON_UNMAP: bool>(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(aspace_token) == LifeCycleState::NotReady
                    && self.object_locked(aspace_token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(aspace_token) == LifeCycleState::Alive);
        }
        if self.memory_priority_locked(aspace_token) == MemoryPriority::High {
            return;
        }
        self.set_memory_priority_field_locked(aspace_token, MemoryPriority::High);
        self.aspace().change_high_priority_count_locked(aspace_token, 1);
        if let Some(paged) = self.object_locked(aspace_token).as_deref().unwrap().as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(1));
            debug_assert!(paged.cow_pages().lock_is_held()); // we have the object lock
            pc.as_mut().prepare_is_already_high_priority_locked(object_token);
            pc.as_mut().change_high_priority_count_locked(object_token);
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
    pub(super) fn commit_high_memory_priority(&self) {
        let vmo;
        let offset;
        let len;
        {
            ksync::lock!(let guard = self.lock().lock());
            if self.state_locked(guard.token()) != LifeCycleState::Alive
                || self.memory_priority_locked(guard.token()) != MemoryPriority::High
            {
                return;
            }
            vmo = self.vmo_locked(guard.token());
            offset = self.object_offset();
            len = self.size() as u64;
        }
        debug_assert!(vmo.is_some());
        let vmo = vmo.unwrap();
        vmo.commit_high_priority_pages(offset, len);
        // Ignore the return result of `map_range` as this is just best effort.
        let _ = self.map_range(offset as usize, len as usize, false, true);
    }

    /// Map in pages from the underlying vm object, optionally committing pages as it goes.
    /// `ignore_existing` controls whether existing hardware mappings in the specified range should
    /// be ignored or treated as an error. `ignore_existing` should only be set to true for user
    /// mappings where populating mappings may already be racy with multiple threads, and where we
    /// are already tolerant of mappings being arbitrarily created and destroyed.
    pub fn map_range(
        &self,
        offset: usize,
        len: usize,
        commit: bool,
        ignore_existing: bool,
    ) -> Result<(), Status> {
        ksync::lock!(let aspace_guard = self.lock().lock());
        // SAFETY: `aspace_guard` is never released.
        let token = unsafe { aspace_guard.token_unchecked() };
        self.state().canary.assert();

        let len = kernel_page::round_up(len);
        if len == 0 {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        if self.state_locked(token) != LifeCycleState::Alive {
            hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        ltracef!(
            "region {:p}, offset {:#x}, size {:#x}, commit {}\n",
            self,
            offset,
            len,
            commit as i32
        );

        let object = self.object_locked(token);
        debug_assert!(object.is_some());
        let range_base = self.base().0.wrapping_add(offset);
        if !kernel_page::is_page_rounded(offset) || !self.is_in_range(VAddr(range_base), len) {
            hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }
        // SAFETY: An `Alive` mapping always has a backing `object`.
        let object = unsafe { object.as_ref().unwrap_unchecked() };

        // If this is a kernel mapping then validate that all pages being mapped are currently
        // pinned, ensuring that they cannot be taken away for any reason, unless the mapping has
        // specifically opted out of this debug check due to it performing its own dynamic
        // management.
        debug_assert!(
            self.aspace().is_user()
                || self.aspace().is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0
                || object.debug_is_range_pinned(self.object_offset() + offset as u64, len as u64)
        );

        // Cache whether the object is dirty tracked, we need to know this when computing mmu flags
        // later.
        let dirty_tracked = object.is_dirty_tracked();

        let existing_entry_action =
            if ignore_existing { ExistingEntryAction::Skip } else { ExistingEntryAction::Error };

        // The region to map could have multiple different current arch mmu flags, so we need to
        // iterate over them to ensure we install mappings with the correct permissions.
        self.enumerate_protection_ranges_locked(
            token,
            range_base,
            len,
            move |base, len, mut mmu_flags| {
                // Remove the write permission if this maps a vmo that supports dirty tracking, in
                // order to trigger write permission faults when writes occur, enabling us to track
                // when pages are dirtied.
                if dirty_tracked {
                    mmu_flags &= !ARCH_MMU_FLAG_PERM_WRITE;
                }

                // If there are no access permissions on this region then mapping has no effect, so
                // skip.
                if (mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) == 0 {
                    return Err(Status::NEXT);
                }

                let map_offset = (base - self.base().0) as u64;
                let vmo_offset = self.object_offset() + map_offset;
                if let Some(paged) = object.as_paged() {
                    // In the scenario where we are committing, and calling `require_owned_page`, we
                    // are supposed to pass in a `LazyPageRequest`. Technically we could get away
                    // with not passing in a `PageRequest` since:
                    //  * Only internal kernel VMOs will have the 'commit' flag passed in for their
                    //    mappings
                    //  * Only pager backed VMOs or VMOs that support delayed memory allocations
                    //    need to fill out a `PageRequest`
                    //  * Internal kernel VMOs are never pager backed or have the delayed memory
                    //    allocation flag set.
                    // However, should these assumptions ever get violated it's better to catch this
                    // gracefully than have `require_owned_page` error/crash internally, and it
                    // costs nothing to create and pass in.
                    stack_pin_init!(let page_request = MultiPageRequest::new());

                    // grab the lock for the vmo
                    stack_pin_init!(let deferred = paged.make_deferred_ops());
                    #[rustfmt::skip]
                    ksync::lock!(let mut object_guard = object
                        .lock_ref()
                        .lock_policy_with::<CriticalMutexOrderedPolicy>(
                            paged.lock_order() as usize,
                        ));
                    // SAFETY: `object_guard` is never released.
                    let object_token = unsafe { object_guard.as_mut().token_mut_unchecked() };

                    // Trim our range to the current VMO size. Our mapping might exceed the VMO in
                    // the case where the VMO is resizable, and this should not be considered an
                    // error.
                    let len = self.trimmed_object_range_locked(
                        token,
                        object_token,
                        map_offset,
                        len as u64,
                    ) as usize;
                    if len == 0 {
                        return Err(Status::STOP);
                    }

                    // SAFETY: The object lock is held until `coalescer` is dropped.
                    let mut coalescer = unsafe {
                        VmMappingCoalescer::<16>::new(
                            self,
                            object_token,
                            base,
                            mmu_flags,
                            existing_entry_action,
                        )
                    };

                    let writing = (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0;
                    // SAFETY: `object_token` is for the lock of `paged`. `vmo_offset` and `len`
                    // are page-aligned, and `len` is non-zero.
                    let cursor = unsafe {
                        paged.get_lookup_cursor_locked(object_token, vmo_offset, len as u64)
                    };
                    stack_try_pin_init!(let cursor = cursor);
                    let mut cursor = cursor?;
                    // Do not consider pages touched when mapping in, if they are actually touched
                    // they will get an accessed bit set in the hardware.
                    cursor.as_mut().disable_mark_accessed();
                    let mut off = 0;
                    while off < len {
                        let page = if commit {
                            // SAFETY: `deferred` was made for the VMO that `cursor` was made for,
                            // the range of `cursor` has a page left, and `page_request` does not
                            // hold an active request.
                            let result = unsafe {
                                cursor.as_mut().require_owned_page(
                                    writing,
                                    1,
                                    deferred.as_mut(),
                                    page_request.as_mut(),
                                )
                            };
                            match result {
                                Ok(result) => Some(result.page),
                                Err(status) => {
                                    hint::cold_path();
                                    // As per the comment above `page_request` definition, there
                                    // should never be commit + pager backed VMO and so we should
                                    // never end up with a `PageRequest` needing to be waited on.
                                    assert!(status != Status::SHOULD_WAIT);
                                    // fail when we can't commit every requested page
                                    coalescer.drop_outstanding();
                                    return Err(status);
                                }
                            }
                        } else {
                            // Not committing so get a page if one exists. This increments the
                            // cursor, returning `None` if no page.
                            // SAFETY: The range of `cursor` has a page left.
                            let page = unsafe { cursor.as_mut().maybe_page(writing) };
                            // This page was not present and if we are in a run of absent pages we
                            // would like to efficiently skip them, instead of querying each virtual
                            // address individually. Due to the assumptions of the cursor, we cannot
                            // call `skip_missing_pages` if we had just requested the last page in
                            // the range of the cursor.
                            if page.is_none() && off + kernel_page::SIZE < len {
                                // Increment `off` for the any pages we skip and let the original
                                // page from `maybe_page` get incremented on the way around the loop
                                // before the range gets checked.
                                // SAFETY: The range of `cursor` has a page left.
                                off += unsafe { cursor.as_mut().skip_missing_pages() } as usize
                                    * kernel_page::SIZE;
                            }
                            page
                        };
                        if let Some(page) = page {
                            // SAFETY: `page` was returned by `cursor` while holding the VMO lock,
                            // so its state is valid to inspect.
                            let paddr = unsafe { page.paddr() };
                            // SAFETY: `paddr` is valid to map at `base + off` until `coalescer` is
                            // dropped.
                            unsafe { coalescer.append(base + off, paddr) }?;
                        }
                        off += kernel_page::SIZE;
                    }
                    coalescer.flush()?;
                    Err(Status::NEXT)
                } else if let Some(phys) = object.as_physical() {
                    hint::cold_path();
                    // grab the lock for the vmo
                    ksync::lock!(let object_guard = object.lock_ref().lock());
                    // SAFETY: `object_guard` is never released.
                    let object_token = unsafe { object_guard.token_unchecked() };
                    // Physical VMOs are never resizable, so do not need to worry about trimming the
                    // range.
                    debug_assert!(!phys.is_resizable());
                    // SAFETY: The object lock is held until `coalescer` is dropped.
                    let mut coalescer = unsafe {
                        VmMappingCoalescer::<16>::new(
                            self,
                            object_token,
                            base,
                            mmu_flags,
                            existing_entry_action,
                        )
                    };

                    // Physical VMOs are always allocated and contiguous, just need to get the
                    // paddr.
                    let phys_base = phys
                        .lookup_contiguous_locked(object_token, vmo_offset, len as u64)
                        .expect("physical VMO range must be valid");

                    let mut offset = 0;
                    while offset < len {
                        // SAFETY: `phys_base + offset` is valid to map at `base + offset` until
                        // `coalescer` is dropped.
                        unsafe { coalescer.append(base + offset, PAddr(phys_base.0 + offset)) }?;
                        offset += kernel_page::SIZE;
                    }
                    coalescer.flush()?;
                    Err(Status::NEXT)
                } else {
                    panic!("VmObject should be paged or physical");
                }
            },
        )
    }

    /// Unlocked convenience wrapper of `unmap_locked` for testing.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, and `[base, base + size)` is a
    /// strict subrange of `[self.base(), self.base() + self.size())` that may be unmapped.
    pub unsafe fn debug_unmap(&self, base: usize, size: usize) -> Result<(), Status> {
        ksync::lock!(let mut region_guard = self.region_lock().lock());
        ksync::lock!(let mut guard = self.lock().lock());
        // Take a ref to ourselves in case we drop the last one when removing from our parent.
        let this = RefPtr::from_ref(self);
        // SAFETY: `size` is not zero, `base` and `size` are page aligned, and `[base, base + size)`
        // is a strict subrange of `[self.base(), self.base() + self.size())` that may be unmapped.
        unsafe {
            Self::unmap_locked(
                &this,
                region_guard.as_mut().token_mut(),
                guard.as_mut().token_mut(),
                base,
                size,
            )
        }
    }

    /// Unlocked convenience wrapper of `protect_locked` for testing.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within
    /// this mapping and may have its permissions changed and be unmapped, and `new_arch_mmu_flags`
    /// has no cache flags and is valid for this mapping.
    pub unsafe fn debug_protect(
        &self,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.lock().lock());
        // SAFETY: `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is
        // within this mapping and may have its permissions changed and be unmapped, and
        // `new_arch_mmu_flags` has no cache flags and is valid for this mapping.
        unsafe { self.protect_locked(guard.as_mut().token_mut(), base, size, new_arch_mmu_flags) }
    }

    // Accessors for VMO-mapping state.
    // These can be read under either lock (both locks being held for writing).
    /// Returns the architecture MMU flags and the top of the protection region containing `va`.
    pub fn arch_mmu_flags_range_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        va: usize,
    ) -> FlagsRange {
        self.flags_range_at_addr_locked(token, va)
    }

    /// Returns the MMU flags for `offset` while holding the aspace lock.
    pub fn arch_mmu_flags_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        offset: VAddr,
    ) -> ArchMmuFlags {
        self.flags_range_at_addr_locked(token, offset.0).mmu_flags
    }

    /// Page fault in an address within the mapping. The requested address must be page-aligned.
    ///
    /// If `additional_pages` is non-zero, then up to that many additional pages may be resolved
    /// using the same `pf_flags`. It is not an error for the `additional_pages` to span beyond the
    /// mapping or underlying VMO, although the range will get truncated internally. As such only
    /// the page containing `va` is required to be resolved, and this method may return `Ok` if any
    /// number, including zero, of the additional pages are resolved.
    ///
    /// `object` is required to be this mapping's object, with the requirement that if the aspace
    /// lock is not held over the call to this function, then the caller is required to ensure that
    /// `object` will remain alive for the duration of the call.
    ///
    /// As the `additional_pages` are resolved with the same `pf_flags` they may trigger
    /// copy-on-write or other allocations in the underlying VMO.
    ///
    /// If this returns `Status::SHOULD_WAIT`, then the caller should wait on `page_request`
    /// and try again. In addition to a status this returns how many pages got mapped in.
    ///
    /// This may return `Status::UNAVAILABLE` if the aspace lock is not held and means that the
    /// mapping was destroyed before the page fault could be handled.
    ///
    /// If `Ok` is returned then the number of pages mapped in is guaranteed to be >0.
    ///
    /// If `additional_pages` was non-zero, then the maximum number of pages that will be mapped is
    /// `additional_pages + 1`. Otherwise the maximum number of pages that will be mapped is
    /// `Self::PAGE_FAULT_MAX_OPTIMISTIC_PAGES`.
    ///
    /// # Safety
    ///
    /// `object` is this mapping's object, or was until the mapping's object was reset, and the
    /// object lock is not held. `va` is page-aligned and within this mapping, `additional_pages` is
    /// less than `usize::MAX`, and `page_request` does not hold an active request.
    #[inline(never)]
    pub unsafe fn page_fault(
        &self,
        va: VAddr,
        pf_flags: u32,
        additional_pages: usize,
        object: &VmObject,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        vm_ktrace_duration!(
            2,
            "VmMapping::PageFault",
            "user_id" => object.user_id(),
            "va" => ArgValue::Pointer(va.0),
        );
        self.state().canary.assert();

        debug_assert!(kernel_page::is_aligned(va.0));

        if let Some(paged) = object.as_paged() {
            // SAFETY: `paged` is this mapping's object, or was until the mapping's object was
            // reset, and the object lock is not held. `va` is page-aligned and within this
            // mapping, `additional_pages` is less than `usize::MAX`, and `page_request` does not
            // hold an active request.
            return unsafe {
                self.page_fault_paged(va, pf_flags, additional_pages, paged, page_request)
            };
        }
        hint::cold_path();
        let phys = object.as_physical();
        assert!(phys.is_some());
        let phys = phys.unwrap();
        // SAFETY: `phys` is this mapping's object, or was until the mapping's object was
        // reset, and the object lock is not held. `va` is page-aligned and within this
        // mapping, `additional_pages` is less than `usize::MAX`, and `page_request` does not
        // hold an active request.
        unsafe { self.page_fault_physical(va, pf_flags, additional_pages, phys, page_request) }
    }

    /// # Safety
    ///
    /// `paged` is this mapping's object, or was until the mapping's object was reset, and the
    /// object lock is not held. `va` is page-aligned and within this mapping, `additional_pages` is
    /// less than `usize::MAX`, and `page_request` does not hold an active request.
    #[inline(never)]
    unsafe fn page_fault_paged(
        &self,
        va: VAddr,
        pf_flags: u32,
        additional_pages: usize,
        paged: &VmObjectPaged,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        stack_pin_init!(let deferred = paged.make_deferred_ops());
        ksync::lock!(let mut guard = paged
            .lock_ref()
            .lock_policy_with::<CriticalMutexOrderedPolicy>(paged.lock_order() as usize));
        // SAFETY: `guard` is never released.
        let object_token = unsafe { guard.as_mut().token_mut_unchecked() };
        if self.object_reset() {
            hint::cold_path();
            return (Err(Status::UNAVAILABLE), 0);
        }
        // The caller was obliged to pass us our object in as `paged`, whose lock we now hold. Since
        // we know that our object can only be one of two values, `Some(paged)` or `None`, then if
        // the reset flag is false, i.e. our object is still `Some(paged)`, then we know that:
        //  * Our read of the reset flag did not race, since it is written under the object lock,
        //    which we presently hold
        //  * our object == `Some(paged)` since it's not `None`
        //  * our object cannot transition to `None` since we hold its lock.
        //
        // SAFETY: `paged` is this mapping's object, and this mapping has no other
        // `CurrentlyFaulting`. `va` is page-aligned and within this mapping, `additional_pages` is
        // less than `usize::MAX`, `deferred` was made for `paged`, and `page_request` does not
        // hold an active request.
        unsafe {
            self.page_fault_locked_object(
                object_token,
                va.0,
                pf_flags,
                additional_pages,
                paged,
                Some(deferred.as_mut()),
                page_request,
            )
        }
    }

    /// # Safety
    ///
    /// `phys` is this mapping's object, or was until the mapping's object was reset, and the object
    /// lock is not held. `va` is page-aligned and within this mapping, `additional_pages` is less
    /// than `usize::MAX`, and `page_request` does not hold an active request.
    #[cold]
    #[inline(never)]
    unsafe fn page_fault_physical(
        &self,
        va: VAddr,
        pf_flags: u32,
        additional_pages: usize,
        phys: &VmObjectPhysical,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        ksync::lock!(let mut guard = phys.lock_ref().lock());
        // SAFETY: `guard` is never released.
        let object_token = unsafe { guard.as_mut().token_mut_unchecked() };
        if self.object_reset() {
            hint::cold_path();
            return (Err(Status::UNAVAILABLE), 0);
        }
        // See comment in `page_fault_paged` for explanation.
        //
        // SAFETY: `phys` is this mapping's object, and this mapping has no other
        // `CurrentlyFaulting`. `va` is page-aligned and within this mapping, `additional_pages` is
        // less than `usize::MAX`, and `phys` is not paged.
        unsafe {
            self.page_fault_locked_object(
                object_token,
                va.0,
                pf_flags,
                additional_pages,
                phys,
                None,
                page_request,
            )
        }
    }

    /// Convenience wrapper around `page_fault` that can be called with the aspace lock held and
    /// will never return `Status::UNAVAILABLE`.
    ///
    /// # Safety
    ///
    /// This mapping is alive and the object lock is not held. `va` is page-aligned and within this
    /// mapping, `additional_pages` is less than `usize::MAX`, and `page_request` does not hold an
    /// active request.
    pub unsafe fn page_fault_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        va: VAddr,
        pf_flags: u32,
        additional_pages: usize,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        // As the aspace lock is held we can safely just use the direct raw value of the object,
        // knowing that it cannot be destructed, and call the regular `page_fault` method. This is
        // explicitly safe to call with the aspace lock held.
        // SAFETY: An `Alive` mapping always has a backing `object`.
        let object = unsafe { self.object_locked(token).as_deref().unwrap_unchecked() };
        // SAFETY: `object` is this mapping's object and the object lock is not held. `va` is
        // page-aligned and within this mapping, `additional_pages` is less than `usize::MAX`, and
        // `page_request` does not hold an active request.
        unsafe { self.page_fault(va, pf_flags, additional_pages, object, page_request) }
    }

    /// Internal helper for performing a page fault after the object lock is acquired.
    ///
    /// Additionally the object is passed in as the downcast specific type in `object` to allow the
    /// helper to assume that the object lock is held in all paths, without needing to do its own
    /// lock assertions, after runtime casting, throughout.
    ///
    /// # Safety
    ///
    /// `object` is this mapping's object and this mapping has no other `CurrentlyFaulting`. `va` is
    /// page-aligned and within this mapping, and `additional_pages` is less than `usize::MAX`. If
    /// `object` is paged, `deferred` is `Some` and was made for it, and `page_request` does not
    /// hold an active request.
    #[inline(always)]
    #[expect(clippy::too_many_arguments)]
    unsafe fn page_fault_locked_object<T: PageFaultObject>(
        &self,
        token: &mut LockToken<'_, VmObjectLockClass>,
        va: usize,
        pf_flags: u32,
        additional_pages: usize,
        object: &T,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
        mut page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        // Fault batch size when num_pages > 1.
        const BATCH_PAGES: usize = 16;
        const COALESCER_SIZE: usize = if VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES > BATCH_PAGES {
            VmMapping::PAGE_FAULT_MAX_OPTIMISTIC_PAGES
        } else {
            BATCH_PAGES
        };

        let vmo_offset = (va.wrapping_sub(self.base().0) as u64).wrapping_add(self.object_offset());

        ltracef!(
            "{:p} va {:#x} vmo_offset {:#x}, pf_flags {:#x} ({})\n",
            self,
            va,
            vmo_offset,
            pf_flags,
            zr::from_utf8_lossy(&fault::flags_to_string(pf_flags))
        );

        // Need to look up the mmu flags for this virtual address, as well as how large a region
        // those flags are for so we can cap the extra mappings we create.
        let range = self.flags_range_at_addr_locked_object(token, va);

        // Build the mmu flags we need to have based on the page fault. This strategy of building
        // the flags and then comparing all at once allows the compiler to provide much better code
        // gen.
        let mut needed_mmu_flags: ArchMmuFlags = 0;
        if (pf_flags & pf_flag::USER) != 0 {
            needed_mmu_flags |= ARCH_MMU_FLAG_PERM_USER;
        }
        let write = (pf_flags & pf_flag::WRITE) != 0;
        if write {
            needed_mmu_flags |= ARCH_MMU_FLAG_PERM_WRITE;
        } else {
            needed_mmu_flags |= ARCH_MMU_FLAG_PERM_READ;
        }
        if (pf_flags & pf_flag::INSTRUCTION) != 0 {
            needed_mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
        }
        // Check that all the needed flags are present.
        if (range.mmu_flags & needed_mmu_flags) != needed_mmu_flags {
            hint::cold_path();
            if (pf_flags & pf_flag::USER) != 0 && (range.mmu_flags & ARCH_MMU_FLAG_PERM_USER) == 0 {
                // user page fault on non user mapped region
                ltracef!("permission failure: user fault on non user region\n");
            }
            if (pf_flags & pf_flag::WRITE) != 0 && (range.mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0
            {
                // write to a non-writable region
                ltracef!("permission failure: write fault on non-writable region\n");
            }
            if (pf_flags & pf_flag::WRITE) == 0 && (range.mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0
            {
                // read to a non-readable region
                ltracef!("permission failure: read fault on non-readable region\n");
            }
            if (pf_flags & pf_flag::INSTRUCTION) != 0
                && (range.mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) == 0
            {
                // instruction fetch from a no execute region
                ltracef!("permission failure: execute fault on no execute region\n");
            }
            return (Err(Status::ACCESS_DENIED), 0);
        }

        // Calculate the number of pages from `va` until the end of the protection range.
        let num_protection_range_pages =
            range.region_top.wrapping_sub(va as u64) as usize / kernel_page::SIZE;

        let mut vmo_size = object.size_locked(token);
        if let Some(paged) = object.as_paged_object() {
            // If fault-beyond-stream-size is set, throw exception on memory accesses past the page
            // containing the user defined stream size.
            if (self.flags() & flag::FAULT_BEYOND_STREAM_SIZE) != 0 {
                hint::cold_path();
                if let Some(size) = paged.saturating_stream_size_locked(token) {
                    vmo_size = size;
                }
            }
        }

        if vmo_offset >= vmo_size {
            hint::cold_path();
            return (Err(Status::OUT_OF_RANGE), 0);
        }

        // Calculate the maximum number of pages we can legally look at, i.e. are valid, in the vmo
        // taking into account the protection range, which is implicitly taking into account the
        // mapping size.
        let num_vmo_pages = ((vmo_size - vmo_offset) / kernel_page::SIZE as u64) as usize;
        let num_valid_pages = min(num_protection_range_pages, num_vmo_pages);

        // Number of requested pages, trimmed to protection range & VMO.
        let num_required_pages = min(num_valid_pages, additional_pages.wrapping_add(1));
        debug_assert!(num_required_pages > 0);
        // Helper to calculate the remaining pt pages if we need them.
        let calc_pt_pages = |va: usize| {
            let next_pt_base = ArchVmAspace::next_user_page_table_offset(va);
            next_pt_base.wrapping_sub(va) / kernel_page::SIZE
        };
        // Number of pages we're aiming to fault. If a range > 1 page is supplied, it is assumed the
        // user knows the appropriate range, so opportunistic pages will not be added.
        let num_fault_pages = if additional_pages == 0 {
            min(min(Self::PAGE_FAULT_MAX_OPTIMISTIC_PAGES, num_valid_pages), calc_pt_pages(va))
        } else {
            num_required_pages
        };

        // Opportunistic pages are not considered in `currently_faulting` optimization, as it is not
        // guaranteed the mappings will be updated.
        let currently_faulting_len = (num_required_pages * kernel_page::SIZE) as u64;
        let fault = |currently_faulting: &CurrentlyFaulting<'_>,
                     token: &mut LockToken<'_, VmObjectLockClass>| {
            // SAFETY: The object lock is held until `coalescer` is dropped.
            let mut coalescer = unsafe {
                VmMappingCoalescer::<COALESCER_SIZE>::new(
                    self,
                    token,
                    va,
                    range.mmu_flags,
                    ExistingEntryAction::Upgrade,
                )
            };

            if let Some(paged) = object.as_paged_object() {
                let Some(mut deferred) = deferred else {
                    unreachable!("paged faults take DeferredOps");
                };
                // fault in or grab existing pages.
                let cursor_size = (num_fault_pages * kernel_page::SIZE) as u64;
                // SAFETY: `token` is for the lock of `paged`. `vmo_offset` and `cursor_size` are
                // page-aligned, and `cursor_size` is non-zero.
                let cursor =
                    unsafe { paged.get_lookup_cursor_locked(token, vmo_offset, cursor_size) };
                stack_try_pin_init!(let cursor = cursor);
                let mut cursor = match cursor {
                    Ok(cursor) => cursor,
                    Err(status) => {
                        hint::cold_path();
                        return (Err(status), coalescer.total_mapped() as u32);
                    }
                };
                // Do not consider pages touched when mapping in, if they are actually touched they
                // will get an accessed bit set in the hardware.
                cursor.as_mut().disable_mark_accessed();

                // Fault requested pages.
                let mut offset = 0;
                while offset < num_required_pages * kernel_page::SIZE {
                    let mut curr_mmu_flags = range.mmu_flags;

                    let num_curr_pages = num_required_pages - (offset / kernel_page::SIZE);
                    // SAFETY: `deferred` was made for the VMO that `cursor` was made for,
                    // `num_curr_pages` is non-zero and at most the number of pages left in the
                    // range of `cursor`, and `page_request` does not hold an active request.
                    let result = unsafe {
                        cursor.as_mut().require_page(
                            write,
                            num_curr_pages as u64,
                            deferred.as_mut(),
                            page_request.as_mut(),
                        )
                    };
                    let result = match result {
                        Ok(result) => result,
                        Err(status) => {
                            hint::cold_path();
                            let _ = coalescer.flush();
                            return (Err(status), coalescer.total_mapped() as u32);
                        }
                    };

                    debug_assert!(!write || result.writable);

                    // We looked up in order to write. Mark as modified. Only need to do this once.
                    if write && offset == 0 {
                        // SAFETY: `mark_modified_locked` does not change the page lists, parent
                        // links, parent limits, or sizes of the VMO hierarchy of `cursor`, or
                        // acquire ancestor locks in that hierarchy.
                        paged.mark_modified_locked(unsafe { cursor.as_mut().token_mut() });
                    }

                    // If we read faulted, and lookup didn't say that this is always writable, then
                    // we map or modify the page without any write permissions. This ensures we will
                    // fault again if a write is attempted so we can potentially replace this page
                    // with a copy or a new one, or update the page's dirty state.
                    if !write && !result.writable {
                        // we read faulted, so only map with read permissions
                        curr_mmu_flags &= !ARCH_MMU_FLAG_PERM_WRITE;
                    }

                    // SAFETY: The state of `result.page` is valid to inspect.
                    let paddr = unsafe { result.page.paddr() };
                    // SAFETY: `paddr` is valid to map at `va + offset` until `coalescer` is
                    // dropped.
                    if let Err(status) = unsafe {
                        coalescer.append_or_adjust_mapping(va + offset, paddr, curr_mmu_flags)
                    } {
                        hint::cold_path();
                        // Flush any existing pages in the coalescer.
                        let _ = coalescer.flush();
                        return (Err(status), coalescer.total_mapped() as u32);
                    }
                    offset += kernel_page::SIZE;
                }

                // Fault opportunistic pages. If a range is supplied, it is assumed the user knows
                // the appropriate range, so opportunistic pages will not be faulted.
                if additional_pages == 0 {
                    debug_assert!(num_fault_pages > 0);
                    // Check how much space the coalescer has for faulting additional pages.
                    let mut extra_pages =
                        coalescer.extra_page_capacity_from(va.wrapping_add(kernel_page::SIZE));
                    extra_pages = min(extra_pages, num_fault_pages - 1);

                    // Acquire any additional pages, but only if they already exist as the user has
                    // not attempted to use these pages yet.
                    if extra_pages > 0 {
                        let writable = (coalescer.mmu_flags() & ARCH_MMU_FLAG_PERM_WRITE) != 0;
                        let slots = coalescer.next_page_slots();
                        // SAFETY: `extra_pages` is at most the length of `slots`.
                        let slots = unsafe { slots.get_unchecked_mut(..extra_pages) };
                        // SAFETY: `slots` is non-empty, and its length is at most the number of
                        // pages left in the range of `cursor`.
                        let num_extra_pages =
                            unsafe { cursor.as_mut().if_exist_pages(writable, slots) }.len();
                        // SAFETY: `num_extra_pages` is at most the number of slots after the
                        // current count, and those slots are initialized with physical addresses
                        // that are valid to map at the virtual addresses that follow the current
                        // run until `coalescer` is dropped.
                        unsafe { coalescer.increment_count(num_extra_pages) };
                    }
                }
            }
            if let Some(phys) = object.as_physical_object() {
                // Already validated the size, and since physical VMOs are always allocated, and not
                // resizable, we know we can always retrieve the maximum number of pages without
                // failure.
                let phys_len = num_fault_pages * kernel_page::SIZE;
                let result = phys.lookup_contiguous_locked(token, vmo_offset, phys_len as u64);

                assert!(result.is_ok());
                let phys_base = result.unwrap();

                // Extrapolate the pages from the base address.
                let mut offset = 0;
                while offset < phys_len {
                    // SAFETY: `phys_base + offset` is valid to map at `va + offset` until
                    // `coalescer` is dropped.
                    if let Err(status) =
                        unsafe { coalescer.append(va + offset, PAddr(phys_base.0 + offset)) }
                    {
                        hint::cold_path();
                        return (Err(status), coalescer.total_mapped() as u32);
                    }
                    offset += kernel_page::SIZE;
                }
            }
            let status = coalescer.flush();
            if status.is_ok() {
                // Mapping has been successfully updated by us. Inform the faulting helper so that
                // it knows not to unmap the range instead.
                currently_faulting.mapping_updated();
            }
            (status, coalescer.total_mapped() as u32)
        };
        // SAFETY: `vmo_offset` and `currently_faulting_len` are page aligned, the range is in the
        // range of the VMO that this mapping maps, this mapping has no other `CurrentlyFaulting`,
        // and the pages that map it may be unmapped until this returns.
        unsafe { CurrentlyFaulting::scoped(self, token, vmo_offset, currently_faulting_len, fault) }
    }

    /// TODO(https://fxbug.dev/42106188): Informs the mapping that a write is going to be performed
    /// to the backing VMO, even if the VMO is not writable. This gives the mapping an opportunity
    /// to create a private clone of the VMO if necessary and use that to back a new mapping
    /// instead, providing a way to 'safely' perform the write. On success a `RefPtr` is returned
    /// either to the current mapping, or to a new mapping if one was created. If a new mapping was
    /// created then this mapping is no longer valid.
    pub fn force_writable(this: RefPtr<Self>) -> Result<RefPtr<VmMapping>, Status> {
        this.state().canary.assert();
        // The caller's `RefPtr` keeps us alive in case we drop the last one when removing from our
        // parent.
        let res = {
            ksync::lock!(let mut region_guard = this.region_lock().lock());
            ksync::lock!(let mut guard = this.lock().lock());
            let region_token = region_guard.as_mut().token_mut();
            let aspace_token = guard.as_mut().token_mut();
            (|| {
                if this.state_locked(aspace_token) != LifeCycleState::Alive {
                    hint::cold_path();
                    return Err(Status::BAD_STATE);
                }
                debug_assert!(this.object_locked(aspace_token).is_some());
                debug_assert!(!this.parent_locked(aspace_token).is_null());

                // Never allow writes to the vdso.
                if ptr::eq(
                    this.aspace().vdso_code_mapping_locked(aspace_token),
                    RefPtr::as_ptr(&this),
                ) {
                    hint::cold_path();
                    return Err(Status::ACCESS_DENIED);
                }
                // If we have already re-directed to a private clone then there is no need to do so
                // again.
                if this.state().private_clone {
                    return Ok(None);
                }
                // If the mapping is already possible to write to (even if disabled by current
                // protections), then writing is already safe.
                if this.is_valid_mapping_flags(ARCH_MMU_FLAG_PERM_WRITE) {
                    return Ok(None);
                }
                // A physical VMO cannot be cloned and so we cannot make this safe, just allow the
                // write.
                let object = this.object_locked(aspace_token).as_deref().unwrap();
                if !object.is_paged() {
                    return Ok(None);
                }

                // Create a clone of our VMO that covers the size of our mapping.
                let writable;
                {
                    let clone = object.create_clone(
                        Resizability::NonResizable,
                        SnapshotType::OnWrite,
                        this.object_offset(),
                        this.size() as u64,
                        true,
                    )?;
                    if this.flags() & flag::FAULT_BEYOND_STREAM_SIZE != 0 {
                        let paged = object.as_paged();
                        debug_assert!(paged.is_some());
                        let paged = paged.unwrap();
                        let original_stream_size;
                        {
                            ksync::lock!(let object_guard = object.lock_ref().lock());
                            let stream_size = paged.user_stream_size_locked(object_guard.token());
                            debug_assert!(stream_size.is_some());
                            original_stream_size = stream_size.unwrap();
                        }
                        let stream_size_over_offset =
                            max(original_stream_size, this.object_offset()) - this.object_offset();
                        let stream_size_limited_by_map_size =
                            min(stream_size_over_offset, this.size() as u64);
                        let result = StreamSizeManager::create(stream_size_limited_by_map_size)?;
                        clone.set_user_stream_size(Some(result));
                    }
                    // TODO(https://fxbug.dev/503042881) Support a more efficient deep copy.
                    stack_pin_init!(let protection_ranges = ProtectionRanges::new());
                    let mut new_ranges = ProtectionRangesMut::new(protection_ranges.as_mut());
                    let mut cursor = this.rest_protection_ranges_locked(aspace_token).begin();
                    while let Some((key, value)) = cursor.get() {
                        // SAFETY: `key` is not present in `new_ranges`.
                        let result = unsafe { new_ranges.insert(key, value) };
                        if !result.is_valid() {
                            hint::cold_path();
                            return Err(Status::NO_MEMORY);
                        }
                        cursor.next();
                    }

                    // SAFETY: The parent points to a live `VmAddressRegion`.
                    let parent = unsafe { &*this.parent_locked(aspace_token) };
                    // We created the clone starting at our object offset in the old object, so that
                    // makes the equivalent start offset be 0 in the clone.
                    writable = Self::create_with_ranges(
                        parent,
                        true,
                        this.base().0,
                        this.size(),
                        this.flags(),
                        clone,
                        0,
                        this.first_region_arch_mmu_flags_locked(aspace_token),
                        protection_ranges,
                        this.mergeable_locked(aspace_token),
                    )
                    .ok_or(Status::NO_MEMORY)?;
                    // First transfer any memory priority from the current mapping to the new
                    // mapping.
                    debug_assert!(writable.lock_is_held());
                    debug_assert!(writable.region_lock_is_held());
                    // Use `SPLIT_ON_UNMAP = true` because `writable` hasn't been activated yet.
                    let priority = this.memory_priority_locked(aspace_token);
                    writable.set_memory_priority_locked::<true>(aspace_token, priority);

                    let writable_object = writable
                        .object_locked(aspace_token)
                        .as_ref()
                        .map(RefPtr::as_ptr)
                        .expect("object must be set");
                    // SAFETY: `writable_object` points to a live `VmObject`.
                    let writable_object = unsafe { &*writable_object };
                    ksync::lock!(let mut object_guard = writable_object.lock_ref().lock());
                    let object_token = object_guard.as_mut().token_mut();
                    if let Err(status) = writable_object.add_mapping_locked(object_token, &writable)
                    {
                        hint::cold_path();
                        writable.set_memory_priority_default_locked_object::<true>(
                            aspace_token,
                            object_token,
                        );
                        return Err(status);
                    }

                    debug_assert!(parent.lock_is_held());
                    debug_assert!(parent.region_lock_is_held());
                    // SAFETY: `this` is a child of `parent` and is kept alive by another owning
                    // reference, and `writable` has the same base as `this` and does not overlap
                    // with any other child of `parent`.
                    unsafe {
                        region_list::replace_region(
                            parent,
                            region_token,
                            aspace_token,
                            &this,
                            &writable,
                        )
                    };
                    writable.activate_no_insert_locked(region_token, aspace_token, object_token);
                }
                // Now acquire the original object lock and destroy ourself.
                {
                    // Keep a `RefPtr` to the object so we know our lock remains valid.
                    let object =
                        this.object_locked(aspace_token).clone().expect("object must be set");
                    ksync::lock!(let mut object_guard = object.lock_ref().lock());
                    let status = Self::destroy_locked_object(
                        &this,
                        region_token,
                        aspace_token,
                        object_guard.as_mut().token_mut(),
                        DestroyUnmap::Yes,
                        DestroyRemoveFromParent::No,
                    );
                    assert!(status.is_ok());
                }
                Ok(Some(writable))
            })()
        };
        match res {
            Ok(writable) => Ok(writable.unwrap_or(this)),
            Err(status) => {
                hint::cold_path();
                Err(status)
            }
        }
    }
}

#[cold]
#[inline(never)]
fn debug_assert_aspace_remove_write_failed(mapping: &VmMapping, offset: u64, len: u64) -> ! {
    panic!(
        "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}",
        mapping,
        mapping.object_offset(),
        mapping.size(),
        offset,
        len
    );
}

/// Ensure the `object` type is exactly the form we expect so that whether it is paged is
/// calculated correctly.
///
/// # Safety
///
/// `as_paged_object` and `as_physical_object` return `None` or `self`.
unsafe trait PageFaultObject {
    fn size_locked(&self, token: &LockToken<'_, VmObjectLockClass>) -> u64;
    fn as_paged_object(&self) -> Option<&VmObjectPaged>;
    fn as_physical_object(&self) -> Option<&VmObjectPhysical>;
}

// SAFETY: `as_paged_object` returns `self` and `as_physical_object` returns `None`.
unsafe impl PageFaultObject for VmObjectPaged {
    #[inline(always)]
    fn size_locked(&self, token: &LockToken<'_, VmObjectLockClass>) -> u64 {
        VmObjectPaged::size_locked(self, token)
    }
    #[inline(always)]
    fn as_paged_object(&self) -> Option<&VmObjectPaged> {
        Some(self)
    }
    #[inline(always)]
    fn as_physical_object(&self) -> Option<&VmObjectPhysical> {
        None
    }
}

// SAFETY: `as_paged_object` returns `None` and `as_physical_object` returns `self`.
unsafe impl PageFaultObject for VmObjectPhysical {
    #[inline(always)]
    fn size_locked(&self, token: &LockToken<'_, VmObjectLockClass>) -> u64 {
        VmObjectPhysical::size_locked(self, token)
    }
    #[inline(always)]
    fn as_paged_object(&self) -> Option<&VmObjectPaged> {
        None
    }
    #[inline(always)]
    fn as_physical_object(&self) -> Option<&VmObjectPhysical> {
        Some(self)
    }
}

/// Helper struct for managing the logic of skipping certain unmap operations for in-progress
/// faults.
///
/// This is expected to be stack allocated under the object lock and the object lock must not be
/// dropped over its lifetime.
///
/// Creating this object creates a contract where the caller will either update the mapping for this
/// location and call `mapping_updated`, or this object will automatically unmap the location if
/// necessary.
struct CurrentlyFaulting<'a> {
    /// Reference back to the original mapping.
    mapping: &'a VmMapping,
    /// The offset, in object space, of the page fault.
    object_offset: u64,
    len: u64,
    state: Cell<CurrentlyFaultingState>,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentlyFaultingState {
    NoUnmapNeeded,
    UnmapSkipped,
    Completed,
}

impl<'a> CurrentlyFaulting<'a> {
    /// # Safety
    ///
    /// `object_offset` and `len` are page aligned, `[object_offset, object_offset + len)` is in the
    /// range of the VMO that `mapping` maps, `mapping` has no other `CurrentlyFaulting`, and the
    /// pages that map the range in `mapping` may be unmapped until this returns.
    unsafe fn scoped<'t, R>(
        mapping: &'a VmMapping,
        token: &mut LockToken<'t, VmObjectLockClass>,
        object_offset: u64,
        len: u64,
        f: impl FnOnce(&CurrentlyFaulting<'a>, &mut LockToken<'t, VmObjectLockClass>) -> R,
    ) -> R {
        let currently_faulting = Self {
            mapping,
            object_offset,
            len,
            state: Cell::new(CurrentlyFaultingState::NoUnmapNeeded),
        };
        debug_assert!(mapping.currently_faulting(token).is_none());
        // SAFETY: `currently_faulting` is a `CurrentlyFaulting` for `mapping` that stays live and
        // is not moved until `currently_faulting` is reset to `None` below. Until then, the object
        // lock stays held, no `&mut` to `currently_faulting` exists, and it is not written except
        // through its `Cell` fields.
        unsafe { mapping.set_currently_faulting(token, Some(NonNull::from(&currently_faulting))) };
        let result = f(&currently_faulting, token);
        // If the caller did not call `mapping_updated`, and an unmap was skipped, then we must
        // unmap the range ourselves. We only do the unmap here if a prior unmap was skipped to
        // avoid needless unmaps due to transient errors such as needing to wait on a page request.
        if currently_faulting.state.get() == CurrentlyFaultingState::UnmapSkipped {
            // SAFETY: `object_offset` and `len` are page aligned,
            // `[object_offset, object_offset + len)` is in the range of the VMO that `mapping`
            // maps, and the pages that map that range in `mapping` may be unmapped until now.
            unsafe { currently_faulting.unmap_skipped(token) };
        }
        // SAFETY: `faulting` is `None`.
        unsafe { mapping.set_currently_faulting(token, None) };
        result
    }

    /// Called to say that the given range needs to be unmapped. This returns true if updating the
    /// range will be handled by the faulting thread and that the unmap can therefore be skipped.
    /// Returns false if the caller should unmap themselves.
    fn unmap_range(&self, object_offset: u64, len: u64) -> bool {
        debug_assert!(self.state.get() != CurrentlyFaultingState::Completed);
        if range_check::intersects_offset_len(object_offset, len, self.object_offset, self.len) {
            self.state.set(CurrentlyFaultingState::UnmapSkipped);
            return true;
        }
        false
    }

    /// Called to indicate that the mapping for the fault location has been updated successfully.
    /// This acts to cancel the unmap that would otherwise happen when this object goes out of
    /// scope.
    fn mapping_updated(&self) {
        self.state.set(CurrentlyFaultingState::Completed);
    }

    /// # Safety
    ///
    /// `self.object_offset` and `self.len` are page aligned,
    /// `[self.object_offset, self.object_offset + self.len)` is in the range of the VMO that
    /// `self.mapping` maps, and the pages that map that range in `self.mapping` may be unmapped.
    #[cold]
    #[inline(never)]
    unsafe fn unmap_skipped(&self, token: &LockToken<'_, VmObjectLockClass>) {
        let (base, new_len) =
            self.mapping.object_range_to_vaddr_range(token, self.object_offset, self.len).unwrap();
        assert!(new_len == self.len);
        let aspace = self.mapping.aspace();
        // SAFETY: `[base, base + new_len)` maps the range of the VMO given to `scoped`, whose pages
        // may be unmapped.
        let status = unsafe {
            aspace.arch_aspace().unmap(
                base.0,
                new_len as usize / kernel_page::SIZE,
                aspace.enlarge_arch_unmap(),
            )
        };
        assert!(status.is_ok());
    }
}

/// Helper struct for batch-installing mappings into the arch aspace. The mapping's object lock
/// must be held over the entirety of the lifetime of this object, without ever being released.
struct VmMappingCoalescer<'a, const N: usize> {
    mapping: &'a VmMapping,
    base: usize,
    // Safety invariant: `phys[..count]` is initialized, and each `phys[i]` in it is valid to map at
    // `base + i * kernel_page::SIZE` until `self` is dropped.
    phys: [MaybeUninit<PAddr>; N],
    // Safety invariant: `count <= N`.
    count: usize,
    total_mapped: usize,
    mmu_flags: ArchMmuFlags,
    existing_entry_action: ExistingEntryAction,
}

impl<'a, const N: usize> VmMappingCoalescer<'a, N> {
    /// # Safety
    ///
    /// The object lock of `mapping` is held until the returned value is dropped.
    #[inline(always)]
    unsafe fn new(
        mapping: &'a VmMapping,
        _object_token: &LockToken<'_, VmObjectLockClass>,
        base: usize,
        mmu_flags: ArchMmuFlags,
        existing_entry_action: ExistingEntryAction,
    ) -> Self {
        // Mapping is only valid if there is at least some access in the flags.
        debug_assert!((mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) != 0);
        Self {
            mapping,
            base,
            phys: [const { MaybeUninit::uninit() }; N],
            count: 0,
            total_mapped: 0,
            mmu_flags,
            existing_entry_action,
        }
    }

    /// Add a page to the mapping run.
    ///
    /// # Safety
    ///
    /// `paddr` is valid to map at `vaddr` until `self` is dropped.
    #[inline(always)]
    unsafe fn append(&mut self, vaddr: usize, paddr: PAddr) -> Result<(), Status> {
        // If this isn't the expected vaddr, flush the run we have first.
        let count = if self.can_append(vaddr) {
            self.count
        } else {
            self.flush()?;
            self.base = vaddr;
            0
        };
        self.phys[count].write(paddr);
        self.count = count + 1;
        Ok(())
    }

    /// # Safety
    ///
    /// `paddr` is valid to map at `vaddr` until `self` is dropped.
    #[inline(always)]
    unsafe fn append_or_adjust_mapping(
        &mut self,
        vaddr: usize,
        paddr: PAddr,
        mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // If this isn't the expected vaddr or mmu_flags have changed, flush the run we have first.
        let count = if self.can_append(vaddr) && mmu_flags == self.mmu_flags {
            self.count
        } else {
            self.flush()?;
            self.base = vaddr;
            self.mmu_flags = mmu_flags;
            0
        };

        self.phys[count].write(paddr);
        self.count = count + 1;
        Ok(())
    }

    /// How much space remains in the `phys` array, starting from `vaddr`, that can be used to
    /// opportunistically map additional pages.
    fn extra_page_capacity_from(&self, vaddr: usize) -> usize {
        // vaddr must be appendable & the coalescer can't be empty.
        if self.can_append(vaddr) && self.count != 0 { N - self.count } else { 0 }
    }

    // Functions for the user to manually populate the remaining `phys` slots and advance the page
    // count.

    /// Returns the remaining slots in `self.phys` after the current count
    /// (`&mut self.phys[self.count..]`).
    fn next_page_slots(&mut self) -> &mut [MaybeUninit<PAddr>] {
        &mut self.phys[self.count..]
    }

    fn mmu_flags(&self) -> ArchMmuFlags {
        self.mmu_flags
    }

    /// # Safety
    ///
    /// `i` is at most `N` minus the current count, and the `i` slots after the current count are
    /// initialized with physical addresses that are valid to map at the virtual addresses that
    /// follow the current run until `self` is dropped.
    unsafe fn increment_count(&mut self, i: usize) {
        self.count += i;
    }

    /// Submit any outstanding mappings to the MMU.
    fn flush(&mut self) -> Result<(), Status> {
        if self.count == 0 {
            return Ok(());
        }

        vm_ktrace_duration!(
            2,
            "map_page",
            "va" => ArgValue::Pointer(self.base),
            "count" => self.count as u64,
            "mmu_flags" => self.mmu_flags as u32,
        );

        // Assert that we're not accidentally mapping the zero page writable. Unless called from a
        // kernel aspace, as the zero page can be mapped writable from the kernel aspace.
        debug_assert!(
            (self.mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0
                || self.phys[..self.count].iter().all(|p| {
                    // SAFETY: The first `count` entries of `phys` are initialized.
                    let p = unsafe { p.assume_init() };
                    p != vm_get_zero_page_paddr()
                })
                || !self.mapping.aspace().is_user()
        );

        // The caller likely overwrote the previous contents of the new pages before supplying them
        // to this `VmMappingCoalescer`. It's important that these writes are observed before the
        // page table entries corresponding to these pages are written out. Otherwise, there is a
        // risk of leaking the previous content.
        //
        // This site needs special synchronization; other sites guarantee this order by at least
        // releasing and acquiring the object lock between manipulating the new pages and modifying
        // mappings. See https://fxrev.dev/517176302.
        store_memory_barrier();

        // SAFETY: `phys` points to an array of at least `count` initialized physical addresses, and
        // the virtual and physical ranges are valid to map.
        let ret = unsafe {
            self.mapping.aspace().arch_aspace().map(
                self.base,
                self.phys.as_mut_ptr().cast::<PAddr>(),
                self.count,
                self.mmu_flags,
                self.existing_entry_action,
            )
        };
        if let Err(status) = ret {
            hint::cold_path();
            trace_map_error(status, self.count, self.base);
        }
        self.base = self.base.wrapping_add(self.count * kernel_page::SIZE);
        self.total_mapped += self.count;
        self.count = 0;
        ret
    }

    fn total_mapped(&self) -> usize {
        self.total_mapped
    }

    /// Drop the current outstanding mappings without sending them to the MMU.
    fn drop_outstanding(&mut self) {
        self.count = 0;
    }

    /// `vaddr` can be appended if it's the next free slot and the coalescer isn't full.
    #[inline(always)]
    fn can_append(&self, vaddr: usize) -> bool {
        self.count < N && vaddr == self.base.wrapping_add(self.count * kernel_page::SIZE)
    }
}

/// Prints the error of a failed `map` in `VmMappingCoalescer::flush`, out of line so that its
/// format arguments do not take stack space in `flush`, which is on the page fault path.
#[cold]
#[inline(never)]
fn trace_map_error(status: Status, count: usize, base: usize) {
    tracef!("error {} mapping {} pages starting at va {:#x}\n", status.into_raw(), count, base);
}

impl<const N: usize> Drop for VmMappingCoalescer<'_, N> {
    fn drop(&mut self) {
        // Make sure no outstanding mappings.
        debug_assert!(self.count == 0);
    }
}

/// # Safety
///
/// `vmo` is null or an owned reference to a live `VmObject`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_state_init(
    storage: &mut MaybeUninit<VmMappingState>,
    vmo: *mut VmObject,
    vmo_offset: u64,
    first_mmu_flags: ArchMmuFlags,
    private_clone: bool,
    mergeable: Mergeable,
) {
    // SAFETY: `vmo` is null or an owned reference to a live `VmObject`.
    let object = unsafe { RefPtr::try_from_raw(vmo) };
    storage.write(VmMappingState::new(
        object,
        vmo_offset,
        first_mmu_flags,
        private_clone,
        mergeable,
    ));
}

/// # Safety
///
/// `this` is being destroyed and its state is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_destruct(this: &VmMapping) {
    this.destruct();
    // SAFETY: The state is initialized and is not used afterwards.
    unsafe {
        core::ptr::drop_in_place(
            bindings::cpp_vm_mapping_state(this.as_ffi_ptr()).cast::<VmMappingState>(),
        )
    };
}

zr::static_assert!(
    core::mem::size_of::<Option<RefPtr<VmObject>>>() == core::mem::size_of::<*const VmObject>()
);

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_object(this: &VmMapping) -> *const Option<RefPtr<VmObject>> {
    this.state().object.get()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_object_offset(this: &VmMapping) -> u64 {
    this.object_offset()
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_arch_mmu_flags_locked(
    this: &VmMapping,
    offset: usize,
) -> ArchMmuFlags {
    // SAFETY: The aspace lock is held.
    let token = unsafe { LockToken::new() };
    this.arch_mmu_flags_locked(&token, VAddr(offset))
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_vmo(this: &VmMapping) -> *mut bindings::VmObject {
    match this.vmo() {
        Some(vmo) => RefPtr::into_raw(vmo).cast_mut().cast(),
        None => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_decommit_range(
    this: &VmMapping,
    offset: usize,
    len: usize,
) -> zx_status_t {
    Status::result_into_raw(this.decommit_range(offset, len))
}

/// # Safety
///
/// The aspace lock is held and the object lock is not held.
#[inline(never)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_locked(
    this: &VmMapping,
    priority: MemoryPriority,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_memory_priority_locked::<false>(&mut token, priority);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_map_range(
    this: &VmMapping,
    offset: usize,
    len: usize,
    commit: bool,
    ignore_existing: bool,
) -> zx_status_t {
    Status::result_into_raw(this.map_range(offset, len, commit, ignore_existing))
}

/// # Safety
///
/// `object` is the mapping's object, or was until the mapping's object was reset, stays alive for
/// the duration of the call, and its lock is not held. `va` is page-aligned and within the
/// mapping, `additional_pages` is less than `usize::MAX`, and `page_request` does not hold an
/// active request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_page_fault(
    this: &VmMapping,
    va: usize,
    pf_flags: u32,
    additional_pages: usize,
    object: &VmObject,
    page_request: Pin<&mut MultiPageRequest>,
    out_mapped: &mut MaybeUninit<u32>,
) -> zx_status_t {
    // SAFETY: `object` is the mapping's object, or was until the mapping's object was reset, and
    // the object lock is not held. `va` is page-aligned and within the mapping,
    // `additional_pages` is less than `usize::MAX`, and `page_request` does not hold an active
    // request.
    let (status, mapped) =
        unsafe { this.page_fault(VAddr(va), pf_flags, additional_pages, object, page_request) };
    out_mapped.write(mapped);
    Status::result_into_raw(status)
}

/// # Safety
///
/// The aspace lock is held and the object lock is not held. The mapping is alive, `va` is
/// page-aligned and within it, `additional_pages` is less than `usize::MAX`, and `page_request`
/// does not hold an active request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_page_fault_locked(
    this: &VmMapping,
    va: usize,
    pf_flags: u32,
    additional_pages: usize,
    page_request: Pin<&mut MultiPageRequest>,
    out_mapped: &mut MaybeUninit<u32>,
) -> zx_status_t {
    // SAFETY: The aspace lock is held.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: The mapping is alive and the object lock is not held. `va` is page-aligned and
    // within it, `additional_pages` is less than `usize::MAX`, and `page_request` does not hold
    // an active request.
    let (status, mapped) = unsafe {
        this.page_fault_locked(&mut token, VAddr(va), pf_flags, additional_pages, page_request)
    };
    out_mapped.write(mapped);
    Status::result_into_raw(status)
}

/// # Safety
///
/// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within `this`
/// and may have its permissions changed and be unmapped, and `new_arch_mmu_flags` has no cache
/// flags and is valid for `this`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_debug_protect(
    this: &VmMapping,
    base: usize,
    size: usize,
    new_arch_mmu_flags: ArchMmuFlags,
) -> zx_status_t {
    // SAFETY: `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is
    // within `this` and may have its permissions changed and be unmapped, and
    // `new_arch_mmu_flags` has no cache flags and is valid for `this`.
    Status::result_into_raw(unsafe { this.debug_protect(base, size, new_arch_mmu_flags) })
}

/// # Safety
///
/// The object lock is held, `this` is in its VMO's mapping list, `offset` and `len` are page
/// aligned, and the pages that map `[offset, offset + len)` of the VMO in `this` may be unmapped.
/// If `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only map
/// the shared zero page.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_unmap_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
    options: UnmapOptions,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    // SAFETY: `this` is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    // pages that map `[offset, offset + len)` of the VMO in `this` may be unmapped. If
    // `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only map
    // the shared zero page.
    unsafe { this.aspace_unmap_locked_object(&token, offset, len, options) };
}

/// # Safety
///
/// The object lock is held, `this` is in its VMO's mapping list, `offset` and `len` are page
/// aligned, and the pages that map `[offset, offset + len)` of the VMO in `this` may have write
/// permission removed, and may be unmapped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_remove_write_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    // SAFETY: `this` is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    // pages that map `[offset, offset + len)` of the VMO in `this` may have write permission
    // removed, and may be unmapped.
    unsafe { this.aspace_remove_write_locked_object(&token, offset, len) };
}

/// # Safety
///
/// The object lock is held, and `this` is in its VMO's mapping list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_debug_unpin_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.aspace_debug_unpin_locked_object(&token, offset, len);
}

/// # Safety
///
/// `this` points to a live `VmMapping`. The region lock, the aspace lock, and the object lock are
/// not held, and the children of the parent are not being iterated. `size` is not zero, `base` and
/// `size` are page aligned, and `[base, base + size)` is a strict subrange of
/// `[this.base(), this.base() + this.size())` that may be unmapped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_debug_unmap(
    this: &VmMapping,
    base: usize,
    size: usize,
) -> zx_status_t {
    // SAFETY: `size` is not zero, `base` and `size` are page aligned, and `[base, base + size)` is
    // a strict subrange of `[this.base(), this.base() + this.size())` that may be unmapped.
    Status::result_into_raw(unsafe { this.debug_unmap(base, size) })
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_get_attributed_memory(
    this: &VmMapping,
    out_counts: &mut MaybeUninit<AttributionCounts>,
) {
    out_counts.write(this.get_attributed_memory());
}

/// # Safety
///
/// `this` points to a live `VmMapping`. The region lock, the aspace lock, and the object lock are
/// not held, and the children of the parent are not being iterated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_force_writable(
    this: *const VmMapping,
    out_mapping: &mut *mut bindings::VmMapping,
) -> Result<(), Status> {
    // Take a ref to ourselves in case we drop the last one when removing from our parent.
    // SAFETY: `this` points to a live `VmMapping`.
    let this = RefPtr::from_ref(unsafe { &*this });
    *out_mapping = RefPtr::into_raw(VmMapping::force_writable(this)?).cast_mut().cast();
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_mark_mergeable(mapping: RefPtr<VmMapping>) {
    VmMapping::mark_mergeable(mapping);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_create(
    parent: &VmAddressRegion,
    private_clone: bool,
    base: usize,
    size: usize,
    vmar_flags: u32,
    vmo: RefPtr<VmObject>,
    vmo_offset: u64,
    arch_mmu_flags: ArchMmuFlags,
    mergeable: Mergeable,
) -> Option<RefPtr<VmMapping>> {
    let init = VmMapping::new(
        parent,
        private_clone,
        base,
        size,
        vmar_flags,
        vmo,
        vmo_offset,
        arch_mmu_flags,
        mergeable,
    );
    RefPtr::pin_init(init).ok()
}

/// # Safety
///
/// `this` is the last reference to a mapping allocated with `kalloc::Box`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_recycle(this: NonNull<VmMapping>) {
    // SAFETY: `this` is the last reference to a mapping allocated with `kalloc::Box`.
    unsafe { VmMapping::recycle(this) }
}
