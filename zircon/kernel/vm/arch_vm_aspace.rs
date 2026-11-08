// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use crate::vm::page::{VmPage, VmPageDoublyLinkedList};
use arch_vm_aspace_bindings as bindings;
use bindings::zx_status_t;
use pin_init::{PinInit, pin_data};
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;

/// Function pointer to allocate a single page that the MMU routine uses to allocate
/// page tables.
pub type PageAllocFn = unsafe extern "C" fn(
    alloc_flags: u32,
    p: *mut *mut VmPage,
    pa: *mut PAddr,
) -> Result<(), Status>;

zr::static_assert!(
    core::mem::size_of::<Option<PageAllocFn>>()
        == core::mem::size_of::<bindings::ArchVmAspaceInterface_page_alloc_fn_t>()
);
zr::static_assert!(core::mem::size_of::<u32>() == core::mem::size_of::<bindings::uint>());
zr::static_assert!(core::mem::size_of::<PAddr>() == core::mem::size_of::<bindings::paddr_t>());
zr::static_assert!(core::mem::align_of::<PAddr>() == core::mem::align_of::<bindings::paddr_t>());
zr::static_assert!(
    core::mem::size_of::<Result<(), Status>>() == core::mem::size_of::<zx_status_t>()
);
zr::static_assert!(
    core::mem::align_of::<Result<(), Status>>() == core::mem::align_of::<zx_status_t>()
);
zr::static_assert!(
    // SAFETY: `Result<(), Status>` and `zx_status_t` have identical 4-byte layouts.
    unsafe { core::mem::transmute::<Result<(), Status>, zx_status_t>(Ok(())) } == 0
);

// TODO(https://fxbug.dev/529507187): Use bitflags! or equivalent once available.
pub type ArchMmuFlags = u8;

#[expect(clippy::identity_op)]
pub const ARCH_MMU_FLAG_CACHED: ArchMmuFlags = 0 << 0;
pub const ARCH_MMU_FLAG_UNCACHED: ArchMmuFlags = 1 << 0;
#[expect(clippy::identity_op)]
pub const ARCH_MMU_FLAG_UNCACHED_DEVICE: ArchMmuFlags = 2 << 0;
#[expect(clippy::identity_op)]
pub const ARCH_MMU_FLAG_WRITE_COMBINING: ArchMmuFlags = 3 << 0;
#[expect(clippy::identity_op)]
pub const ARCH_MMU_FLAG_CACHE_MASK: ArchMmuFlags = 3 << 0;
pub const ARCH_MMU_FLAG_PERM_USER: ArchMmuFlags = 1 << 2;
pub const ARCH_MMU_FLAG_PERM_READ: ArchMmuFlags = 1 << 3;
pub const ARCH_MMU_FLAG_PERM_WRITE: ArchMmuFlags = 1 << 4;
pub const ARCH_MMU_FLAG_PERM_EXECUTE: ArchMmuFlags = 1 << 5;
pub const ARCH_MMU_FLAG_PERM_RWX_MASK: ArchMmuFlags =
    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE;
pub const ARCH_MMU_FLAG_NS: ArchMmuFlags = 1 << 6;
pub const ARCH_MMU_FLAG_INVALID: ArchMmuFlags = 1 << 7;

zr::static_assert!(ARCH_MMU_FLAG_CACHED == bindings::ARCH_MMU_FLAG_CACHED as ArchMmuFlags);
zr::static_assert!(ARCH_MMU_FLAG_UNCACHED == bindings::ARCH_MMU_FLAG_UNCACHED as ArchMmuFlags);
zr::static_assert!(
    ARCH_MMU_FLAG_UNCACHED_DEVICE == bindings::ARCH_MMU_FLAG_UNCACHED_DEVICE as ArchMmuFlags
);
zr::static_assert!(
    ARCH_MMU_FLAG_WRITE_COMBINING == bindings::ARCH_MMU_FLAG_WRITE_COMBINING as ArchMmuFlags
);
zr::static_assert!(ARCH_MMU_FLAG_CACHE_MASK == bindings::ARCH_MMU_FLAG_CACHE_MASK as ArchMmuFlags);
zr::static_assert!(ARCH_MMU_FLAG_PERM_USER == bindings::ARCH_MMU_FLAG_PERM_USER as ArchMmuFlags);
zr::static_assert!(ARCH_MMU_FLAG_PERM_READ == bindings::ARCH_MMU_FLAG_PERM_READ as ArchMmuFlags);
zr::static_assert!(ARCH_MMU_FLAG_PERM_WRITE == bindings::ARCH_MMU_FLAG_PERM_WRITE as ArchMmuFlags);
zr::static_assert!(
    ARCH_MMU_FLAG_PERM_EXECUTE == bindings::ARCH_MMU_FLAG_PERM_EXECUTE as ArchMmuFlags
);
zr::static_assert!(
    ARCH_MMU_FLAG_PERM_RWX_MASK == bindings::ARCH_MMU_FLAG_PERM_RWX_MASK as ArchMmuFlags
);
zr::static_assert!(ARCH_MMU_FLAG_NS == bindings::ARCH_MMU_FLAG_NS as ArchMmuFlags);
zr::static_assert!(ARCH_MMU_FLAG_INVALID == bindings::ARCH_MMU_FLAG_INVALID as ArchMmuFlags);

// TODO(https://fxbug.dev/529507187): Use bitflags! or equivalent once available.
pub type ArchAspaceFlags = u8;

pub const ARCH_ASPACE_FLAG_KERNEL: ArchAspaceFlags = 1 << 0;
pub const ARCH_ASPACE_FLAG_GUEST: ArchAspaceFlags = 1 << 1;

zr::static_assert!(ARCH_ASPACE_FLAG_KERNEL == bindings::ARCH_ASPACE_FLAG_KERNEL as ArchAspaceFlags);
zr::static_assert!(ARCH_ASPACE_FLAG_GUEST == bindings::ARCH_ASPACE_FLAG_GUEST as ArchAspaceFlags);

// TODO(https://fxbug.dev/529507187): Use bitflags! or equivalent once available.
/// Options for unmapping the given virtual address range.
pub type ArchUnmapOptions = u8;

pub const ARCH_UNMAP_OPTION_NONE: ArchUnmapOptions = 0;
/// Controls whether the unmap region can be extended to be larger, or if only the exact region may
/// be unmapped. The unmap region might be extended, even if only temporarily, if large pages need
/// to be split.
pub const ARCH_UNMAP_OPTION_ENLARGE: ArchUnmapOptions = 1 << 0;
/// Requests that the accessed bit be harvested, and the page queues updated.
pub const ARCH_UNMAP_OPTION_HARVEST: ArchUnmapOptions = 1 << 1;

zr::static_assert!(
    ARCH_UNMAP_OPTION_NONE
        == bindings::ArchVmAspaceInterface_ArchUnmapOptions_None as ArchUnmapOptions
);
zr::static_assert!(
    ARCH_UNMAP_OPTION_ENLARGE
        == bindings::ArchVmAspaceInterface_ArchUnmapOptions_Enlarge as ArchUnmapOptions
);
zr::static_assert!(
    ARCH_UNMAP_OPTION_HARVEST
        == bindings::ArchVmAspaceInterface_ArchUnmapOptions_Harvest as ArchUnmapOptions
);

/// Returns true if the MMU flags specify an uncached memory type.
#[inline]
pub const fn arch_mmu_flags_uncached(mmu_flags: ArchMmuFlags) -> bool {
    (mmu_flags & (ARCH_MMU_FLAG_UNCACHED | ARCH_MMU_FLAG_UNCACHED_DEVICE)) != 0
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingEntryAction {
    Skip = 0,
    Error = 1,
    Upgrade = 2,
}

zr::static_assert!(
    ExistingEntryAction::Skip as u8 == bindings::ArchVmAspaceInterface_ExistingEntryAction::Skip.0
);
zr::static_assert!(
    ExistingEntryAction::Error as u8
        == bindings::ArchVmAspaceInterface_ExistingEntryAction::Error.0
);
zr::static_assert!(
    ExistingEntryAction::Upgrade as u8
        == bindings::ArchVmAspaceInterface_ExistingEntryAction::Upgrade.0
);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonTerminalAction {
    /// If a non-terminal entry has no accessed information, unmap and free it. If it has accessed
    /// information, just remove the flag.
    FreeUnaccessed = 0,
    /// Retain both the non-terminal mappings and any accessed information.
    Retain = 1,
}

zr::static_assert!(
    NonTerminalAction::FreeUnaccessed as u8
        == bindings::ArchVmAspaceInterface_NonTerminalAction::FreeUnaccessed.0
);
zr::static_assert!(
    NonTerminalAction::Retain as u8 == bindings::ArchVmAspaceInterface_NonTerminalAction::Retain.0
);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAction {
    /// If the page is accessed update its age in the page queues, and remove the accessed flag.
    UpdateAgeAndHarvest = 0,
    /// If the page is accessed update its age in the page queues, but do not clear the flag.
    UpdateAge = 1,
}

zr::static_assert!(
    TerminalAction::UpdateAgeAndHarvest as u8
        == bindings::ArchVmAspaceInterface_TerminalAction::UpdateAgeAndHarvest.0
);
zr::static_assert!(
    TerminalAction::UpdateAge as u8 == bindings::ArchVmAspaceInterface_TerminalAction::UpdateAge.0
);

/// Per-arch interface to encapsulate the MMU routines on an aspace.
///
/// Beyond construction/destruction lifetimes users of this object must ensure that none of the
/// main methods are called before calling `init` or after calling `destroy`. Doing so is allowed
/// to cause a panic.
/// Aside from `init` and `destroy`, the main methods are all thread-safe.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct ArchVmAspace {
    #[pin]
    opaque: Opaque<bindings::ArchVmAspace>,
}

zr::static_assert!(core::mem::size_of::<ArchVmAspace>() == bindings::kArchVmAspaceSize);
zr::static_assert!(core::mem::align_of::<ArchVmAspace>() == bindings::kArchVmAspaceAlign);

// SAFETY: `cpp_arch_vm_aspace_destruct` invokes `~ArchVmAspace()` in place without moving the
// object, and all backends initialize internal pointers in their constructors so dropping an
// uninitialized or destroyed `ArchVmAspace` is well-defined.
unsafe_pinned_drop_ffi!(ArchVmAspace, bindings::cpp_arch_vm_aspace_destruct);

impl ArchVmAspace {
    /// Constructs an `ArchVmAspace` in place using the default PMM page allocator.
    ///
    /// # Safety
    ///
    /// `base`, `size`, and `flags` must define a valid address space configuration for the target
    /// architecture, and the caller must uphold the `ArchVmAspace` lifecycle invariants:
    /// - Operational methods must not be called before an `init*` method succeeds or after
    ///   `destroy` is called.
    /// - Whenever any `init*` method has been called (even if it failed), all mappings must be
    ///   cleaned up as required and `destroy` must be called before the `ArchVmAspace` is dropped.
    pub unsafe fn new(base: usize, size: usize, flags: ArchAspaceFlags) -> impl PinInit<Self> {
        pin_init_ffi!(bindings::cpp_arch_vm_aspace_construct, base, size, flags, None)
    }

    /// Constructs an `ArchVmAspace` in place with a custom page table allocator callback.
    ///
    /// # Safety
    ///
    /// In addition to the safety requirements of [`ArchVmAspace::new`]:
    /// - `page_alloc_fn` must be safe for the `ArchVmAspace` implementation to invoke whenever it
    ///   allocates page tables (including while `ArchVmAspace`'s internal lock is held, so it must
    ///   not re-enter `ArchVmAspace`, violate lock ordering, or unwind), requiring at most that
    ///   `p` and `pa` are non-null, valid, aligned, and non-overlapping for writes.
    /// - Whenever `page_alloc_fn` returns `Ok(())`, it must write a freshly allocated, exclusively
    ///   owned PMM `VmPage` pointer in `ALLOC` state to `*p` and its physical address to `*pa`,
    ///   transferring ownership of that page to the `ArchVmAspace`.
    pub unsafe fn new_with_page_alloc_fn(
        base: usize,
        size: usize,
        flags: ArchAspaceFlags,
        page_alloc_fn: PageAllocFn,
    ) -> impl PinInit<Self> {
        // SAFETY: `VmPage` has the exact same layout as C++ `vm_page`, `PAddr` is
        // `#[repr(transparent)]` over `usize` (`paddr_t`), and `Result<(), Status>` has the exact
        // same 4-byte niche layout and C ABI as `zx_status_t`, so `Option<PageAllocFn>` and
        // `ArchVmAspaceInterface_page_alloc_fn_t` have identical function pointer ABIs.
        let page_alloc_fn: bindings::ArchVmAspaceInterface_page_alloc_fn_t =
            unsafe { core::mem::transmute::<Option<PageAllocFn>, _>(Some(page_alloc_fn)) };
        pin_init_ffi!(bindings::cpp_arch_vm_aspace_construct, base, size, flags, page_alloc_fn)
    }

    /// This is used to create a regular address space with no special features. In
    /// architectures that do not support unified address spaces, it is also used to create
    /// shared and restricted address spaces. However, when unified address spaces are
    /// supported, the shared and restricted address spaces should be created with `init_shared`
    /// and `init_restricted`.
    ///
    /// # Safety
    ///
    /// `self` must be a newly constructed, uninitialized `ArchVmAspace` with no concurrent
    /// accesses, and early architecture MMU initialization must have completed (at most one
    /// `ARCH_ASPACE_FLAG_KERNEL` instance may be initialized).
    pub unsafe fn init(&self) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle and synchronization preconditions of `Init()`.
        let status: zx_status_t = unsafe { bindings::cpp_arch_vm_aspace_init(self.as_ffi_ptr()) };
        Status::ok(status)
    }

    fn as_ffi_ptr(&self) -> *mut bindings::ArchVmAspace {
        self.opaque.get()
    }

    /// This is used to create a shared address space, whose contents can be
    /// accessed from multiple unified address spaces. These address spaces have a statically
    /// initialized top level page.
    ///
    /// # Safety
    ///
    /// `self` must be a newly constructed, uninitialized user `ArchVmAspace` (`flags == 0`) with
    /// valid shared-aspace `base`/`size` bounds and no concurrent accesses.
    pub unsafe fn init_shared(&self) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle and synchronization preconditions of `InitShared()`.
        let status: zx_status_t =
            unsafe { bindings::cpp_arch_vm_aspace_init_shared(self.as_ffi_ptr()) };
        Status::ok(status)
    }

    /// This is used to create a restricted address space, whose contents can be
    /// accessed from a single unified address space.
    ///
    /// # Safety
    ///
    /// `self` must be a newly constructed, uninitialized user `ArchVmAspace` (`flags == 0`) with
    /// valid restricted-aspace `base`/`size` bounds and no concurrent accesses.
    pub unsafe fn init_restricted(&self) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle and synchronization preconditions of
        // `InitRestricted()`.
        let status: zx_status_t =
            unsafe { bindings::cpp_arch_vm_aspace_init_restricted(self.as_ffi_ptr()) };
        Status::ok(status)
    }

    /// `init_unified`: This is used to create a unified address space. This type of address space
    /// owns no mappings of its own; rather, it is composed of a shared address space and a
    /// restricted address space. As a result, it expects `init_shared` to have been called
    /// on the shared address space, and expects `init_restricted` to have been called on the
    /// restricted address space.
    ///
    /// # Safety
    ///
    /// `self` must be a newly constructed, uninitialized user `ArchVmAspace` (`flags == 0`)
    /// distinct from `shared` and `restricted` with no concurrent accesses. `shared` must be
    /// initialized via `init_shared`, `restricted` must be initialized via `init_restricted` with
    /// non-overlapping top-level entries, and both `shared` and `restricted` must remain live and
    /// undestroyed until after `self.destroy()` completes.
    pub unsafe fn init_unified(
        &self,
        shared: &ArchVmAspace,
        restricted: &ArchVmAspace,
    ) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle, distinctness, and lifetime preconditions of
        // `InitUnified()`.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_init_unified(
                self.as_ffi_ptr(),
                shared.as_ffi_ptr(),
                restricted.as_ffi_ptr(),
            )
        };
        Status::ok(status)
    }

    /// This method puts the instance into read-only mode and asserts that it contains no mappings.
    ///
    /// Note, this method may be a no-op on some architectures. See https://fxbug.dev/42159319.
    ///
    /// It is an error to call this method on an instance that contains mappings. Once called,
    /// subsequent operations that modify the page table will trigger a panic.
    ///
    /// The purpose of this method is to help enforce lifecycle and state transitions of VmAspace
    /// and ArchVmAspaceInterface.
    pub fn disable_updates(&self) {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        unsafe { bindings::cpp_arch_vm_aspace_disable_updates(self.as_ffi_ptr()) }
    }

    /// Destroy expects the aspace to be fully unmapped, as any mapped regions indicate incomplete
    /// cleanup at the higher layers. Note that this does not apply to unified aspaces, which may
    /// still contain some mappings when destroy() is called.
    ///
    /// It is safe to call destroy even if init, init_shared, init_restricted, or init_unified
    /// failed. Once destroy has been called it is a user error to call any of the other methods on
    /// the aspace, unless specifically stated otherwise, and doing so may cause a panic.
    ///
    /// # Safety
    ///
    /// `self` must not be the active kernel address space or active on any CPU, must have no
    /// remaining mappings (unless unified) or live unified dependents, and no other methods (such
    /// as `mark_accessed`) may execute concurrently with or after `destroy`.
    pub unsafe fn destroy(&self) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle, unmapped, and non-concurrency preconditions of
        // `Destroy()`.
        let status: zx_status_t =
            unsafe { bindings::cpp_arch_vm_aspace_destroy(self.as_ffi_ptr()) };
        Status::ok(status)
    }

    /// Map a physically contiguous region into the virtual address space. This is allowed to use
    /// any page size the architecture allows given the input parameters.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`, and the virtual and physical
    /// ranges must be valid to map.
    pub unsafe fn map_contiguous(
        &self,
        vaddr: usize,
        paddr: PAddr,
        count: usize,
        mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_map_contiguous(
                self.as_ffi_ptr(),
                vaddr,
                paddr.0,
                count,
                mmu_flags,
            )
        };
        Status::ok(status)
    }

    /// Map the given array of pages into the virtual address space starting at
    /// `vaddr`, in the order they appear in `phys`.
    ///
    /// If any address in the range `[vaddr, vaddr + count * page::SIZE)` is already
    /// mapped when this is called, `existing_action` controls the behavior used:
    ///  - `Skip` - Skip updating any existing mappings.
    ///  - `Error` - Existing mappings result in a `Status::ALREADY_EXISTS` error.
    ///  - `Upgrade` - Upgrade any existing mappings, meaning a read-only mapping
    ///    can be converted to read-write, or the mapping can have its
    ///    paddr changed. Must not be used on kernel address spaces, as
    ///    changing an existing mapping's paddr may perform
    ///    break-before-make and transiently unmap the page.
    ///
    /// On error none of the provided pages will be mapped. In the case of `Upgrade` the state of
    /// any previous mappings is undefined, and could either still be present or be unmapped.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`. When `count > 0`, `phys` must be
    /// non-null, aligned, and valid to read `count` initialized `PAddr` elements without concurrent
    /// mutation, and the virtual and physical ranges must be valid to map.
    pub unsafe fn map(
        &self,
        vaddr: usize,
        phys: *mut PAddr,
        count: usize,
        mmu_flags: ArchMmuFlags,
        existing_action: ExistingEntryAction,
    ) -> Result<(), Status> {
        let existing_action: bindings::ArchVmAspaceInterface_ExistingEntryAction =
            bindings::ArchVmAspaceInterface_ExistingEntryAction(existing_action as _);
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace` and `phys` points to
        // `count` entries.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_map(
                self.as_ffi_ptr(),
                vaddr,
                phys.cast(),
                count,
                mmu_flags,
                existing_action,
            )
        };
        Status::ok(status)
    }

    /// Unmaps the given virtual address range.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`, the specified virtual range must
    /// no longer be in use, and if `ARCH_UNMAP_OPTION_HARVEST` is set, any mapped pages in the
    /// range must be valid PMM-managed `VmPage`s.
    pub unsafe fn unmap(
        &self,
        vaddr: usize,
        count: usize,
        enlarge: ArchUnmapOptions,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        let status: zx_status_t =
            unsafe { bindings::cpp_arch_vm_aspace_unmap(self.as_ffi_ptr(), vaddr, count, enlarge) };
        Status::ok(status)
    }

    /// Returns whether or not an unmap might need to enlarge an operation for reasons other than
    /// being out of memory. If this returns true, then unmapping a partial large page will fail
    /// always require an enlarged operation.
    pub fn unmap_only_enlarge_on_oom(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        unsafe { bindings::cpp_arch_vm_aspace_unmap_only_enlarge_on_oom(self.as_ffi_ptr()) }
    }

    /// Change the page protections on the given virtual address range
    ///
    /// May return ZX_ERR_NO_MEMORY if the operation requires splitting
    /// a large page and the next level page table allocation fails. In
    /// this case, mappings in the input range may be a mix of the old and
    /// new flags.
    /// ArchUnmapOptions controls whether a larger range than requested is permitted to experience
    /// a temporary permissions change. A temporary change may be required if a break-before-make
    /// style unmap -> remap of the large page is required.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`, the permissions for the given
    /// virtual address range must be valid, and if `ARCH_UNMAP_OPTION_HARVEST` is set, any mapped
    /// pages in the range must be valid PMM-managed `VmPage`s.
    pub unsafe fn protect(
        &self,
        vaddr: usize,
        count: usize,
        mmu_flags: ArchMmuFlags,
        enlarge: ArchUnmapOptions,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_protect(
                self.as_ffi_ptr(),
                vaddr,
                count,
                mmu_flags,
                enlarge,
            )
        };
        Status::ok(status)
    }

    /// Queries the translation for `vaddr`.
    pub fn query(&self, vaddr: usize) -> Result<(PAddr, ArchMmuFlags), Status> {
        let mut paddr: usize = 0;
        let mut mmu_flags: ArchMmuFlags = 0;
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`, and `paddr` and
        // `mmu_flags` are valid out-pointers.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_query(self.as_ffi_ptr(), vaddr, &mut paddr, &mut mmu_flags)
        };
        Status::ok(status).map(|_| (PAddr(paddr), mmu_flags))
    }

    /// Picks a spot in the virtual address space.
    pub fn pick_spot(
        &self,
        base: usize,
        end: usize,
        align: usize,
        size: usize,
        mmu_flags: ArchMmuFlags,
    ) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        unsafe {
            bindings::cpp_arch_vm_aspace_pick_spot(
                self.as_ffi_ptr(),
                base,
                end,
                align,
                size,
                mmu_flags,
            )
        }
    }

    /// Walks the given range of pages and for any pages that are mapped and have their access bit
    /// set:
    /// - Tells the page queues it has been accessed via `PageQueues::mark_accessed`.
    /// - Potentially removes the accessed flag.
    /// - Potentially frees unaccessed page tables.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`, all mapped pages in the range
    /// must be valid PMM-managed `VmPage`s eligible for `PageQueues::mark_accessed`, and freeing
    /// unaccessed page tables (when `NonTerminalAction::FreeUnaccessed` is used) must be valid.
    pub unsafe fn harvest_accessed(
        &self,
        vaddr: usize,
        count: usize,
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) -> Result<(), Status> {
        let non_terminal_action: bindings::ArchVmAspaceInterface_NonTerminalAction =
            bindings::ArchVmAspaceInterface_NonTerminalAction(non_terminal_action as u8);
        let terminal_action: bindings::ArchVmAspaceInterface_TerminalAction =
            bindings::ArchVmAspaceInterface_TerminalAction(terminal_action as u8);
        // SAFETY: Caller upholds the lifecycle and PMM page-queue preconditions of
        // `HarvestAccessed()`.
        let status: zx_status_t = unsafe {
            bindings::cpp_arch_vm_aspace_harvest_accessed(
                self.as_ffi_ptr(),
                vaddr,
                count,
                non_terminal_action,
                terminal_action,
            )
        };
        Status::ok(status)
    }

    /// Marks any pages in the given virtual address range as being accessed.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized, non-unified `ArchVmAspace`, and `destroy` must not be called
    /// concurrently with `mark_accessed`.
    pub unsafe fn mark_accessed(&self, vaddr: usize, count: usize) -> Result<(), Status> {
        // SAFETY: Caller upholds the lifecycle and non-concurrent `destroy` preconditions of
        // `MarkAccessed()`.
        let status: zx_status_t =
            unsafe { bindings::cpp_arch_vm_aspace_mark_accessed(self.as_ffi_ptr(), vaddr, count) };
        Status::ok(status)
    }

    /// Returns whether or not this aspace might have additional accessed information since the last
    /// time this method was called with clear=true. If this returns `false` then, modulo races,
    /// harvest_accessed is defined to not find any set bits and not call PageQueues::MarkAccessed.
    ///
    /// This is intended for use by the harvester to avoid scanning for any accessed or dirty bits
    /// if the aspace has not been accessed at all.
    ///
    /// Note that restricted and shared ArchVmAspace's will report that they have been accessed if
    /// an associated unified ArchVmAspace has been accessed. However, the reverse is not true; the
    /// unified ArchVmAspace will not return true if the associated shared/restricted aspaces have
    /// been accessed.
    ///
    /// The `clear` flag controls whether the aspace having been accessed should be cleared or not.
    /// Not clearing makes this function const and not modify any state. If `clear` is true then
    /// this method is only thread-compatible and must be externally synchronized.
    pub fn accessed_since_last_check(&self, clear: bool) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        unsafe { bindings::cpp_arch_vm_aspace_accessed_since_last_check(self.as_ffi_ptr(), clear) }
    }

    /// Physical address of the backing data structure used for translation.
    ///
    /// This should be treated as an opaque value outside of
    /// architecture-specific components.
    pub fn arch_table_phys(&self) -> PAddr {
        // SAFETY: `self.as_ffi_ptr()` points to a live `ArchVmAspace`.
        let phys: bindings::paddr_t =
            unsafe { bindings::cpp_arch_vm_aspace_arch_table_phys(self.as_ffi_ptr()) };
        PAddr(phys)
    }

    #[inline(always)]
    pub fn next_user_page_table_offset(va: usize) -> usize {
        // SAFETY: This has no preconditions.
        unsafe { bindings::cpp_arch_vm_aspace_next_user_page_table_offset(va) }
    }

    /// Transfer ownership of pre-PMM kernel page table pages from physboot to the arch VM
    /// backend.
    ///
    /// # Safety
    ///
    /// Every page in `mmu_pages` must be a valid allocated kernel page table page handed off from
    /// physboot during early boot.
    pub unsafe fn handoff_page_tables_from_physboot(
        mmu_pages: core::pin::Pin<&mut VmPageDoublyLinkedList>,
    ) {
        // SAFETY: `VmPageDoublyLinkedList` is binary layout compatible with C++'s
        // `DoublyLinkedList`, so a typecast here is fine.  The called function also does not move
        // the mmu_pages list, so the pinned invariant is maintained.
        unsafe {
            bindings::cpp_arch_vm_aspace_handoff_page_tables_from_physboot(
                (mmu_pages.get_unchecked_mut() as *mut VmPageDoublyLinkedList).cast(),
            );
        }
    }
}
