// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::anonymous_page_request::AnonymousPageRequest;
use super::continuous_attribution_tracker::AttributionTracker;
use super::page::{VmPageDoublyLinkedList, VmPagePtr};
use super::page_queues::PageQueues;
use super::page_source::PageSource;
use super::page_state::VmPageState;
use super::physmap::{paddr_to_physmap, paddr_to_physmap_mut, paddr_to_physmap_ptr};
use super::pmm::{self, ALLOC_FLAG_CAN_WAIT, page_queues as pmm_page_queues};
use super::vm::vm_get_zero_page_paddr;
pub use super::vm_object::RangeChangeOp;
use super::vm_object_paged::VmObjectPaged;
use super::vm_page_list::{VmPageList, VmPageOrMarkerRef, VmPageSpliceList};
use crate::arch_rs::cache::arch_clean_invalidate_cache_range;
use crate::arch_rs::ops::zero_page;
use crate::kernel::thread::ScopedMemoryStall;
use crate::kernel::types::PAddr;
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::page_cache::{PageCache, PageList};
use crate::vm::compressor::VmCompressor;
use crate::vm::discardable_vmo_tracker::{DiscardableState, DiscardableVmoTracker};
use crate::vm::page_source::MultiPageRequest;
use crate::vm::pmm_node::{FreeLoanedPagesHolder, PmmOptDelayReuse};
use bitflags::bitflags;
use boot_options::BootOptions;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};
use core::{cmp, debug_assert_matches};
use debug::dprintf;
use fbl::{
    Canary, DoublyLinkedListContainable, HasRefCount, HasRefCountUpgradeable, Recyclable,
    RefCounted, RefPtr,
};
use kalloc::AllocError;
use ksync::{KCell, KMutex, KMutexGuard, LockClass, LockToken};
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pin_init, pin_init_from_closure, stack_pin_init};
use vm_constants_rs as constants;
use vm_cow_pages_bindings as bindings;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::{
    ZX_CACHE_POLICY_CACHED, ZX_CACHE_POLICY_MASK, ZX_PAGER_VMO_STATS_MODIFIED, zx_paddr_t,
    zx_pager_vmo_stats_t, zx_status_t,
};

/// A copy-on-write page hierarchy range.
///
/// Arithmetic methods assume that callers have validated the range (e.g. via
/// [`VmCowRange::is_bounded_by`]) so that `offset + len` and any transformations do not overflow
/// or underflow `u64`.
#[repr(C)]
#[must_use]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmCowRange {
    pub offset: u64,
    pub len: u64,
}

impl VmCowRange {
    #[inline]
    pub const fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    #[inline]
    pub const fn end(self) -> u64 {
        self.offset + self.len
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn is_page_aligned(self) -> bool {
        ::page::is_page_rounded(self.offset as usize) && ::page::is_page_rounded(self.len as usize)
    }

    #[inline]
    pub const fn expand_till_page_aligned(self) -> VmCowRange {
        let start = ::page::round_down(self.offset as usize) as u64;
        VmCowRange::new(start, (::page::round_up(self.end() as usize) as u64).wrapping_sub(start))
    }

    #[inline]
    pub const fn offset_by(self, delta: u64) -> VmCowRange {
        VmCowRange::new(self.offset + delta, self.len)
    }

    /// Trims `amount` bytes from the start of the range. The caller must ensure
    /// `amount <= self.len`.
    #[inline]
    pub const fn trimmed_from_start(self, amount: u64) -> VmCowRange {
        debug_assert!(amount <= self.len);
        VmCowRange::new(self.offset + amount, self.len - amount)
    }

    /// Returns the minimal range that covers both `self` and `other`. If these ranges are disjoint
    /// then the returned range will be larger than combined length of `self` and `other` in
    /// order to span both using a single range.
    #[inline]
    pub fn cover(self, other: VmCowRange) -> VmCowRange {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let start = cmp::min(self.offset, other.offset);
        let end = cmp::max(self.end(), other.end());
        VmCowRange::new(start, end.wrapping_sub(start))
    }

    #[inline]
    pub const fn with_length(self, new_length: u64) -> VmCowRange {
        VmCowRange::new(self.offset, new_length)
    }

    #[inline]
    pub fn is_bounded_by(self, max: u64) -> bool {
        ::range_check::in_range_max(self.offset, self.len, max)
    }
}

impl From<bindings::VmCowRange> for VmCowRange {
    #[inline]
    fn from(bindings_range: bindings::VmCowRange) -> Self {
        Self::new(bindings_range.offset, bindings_range.len)
    }
}

impl From<VmCowRange> for bindings::VmCowRange {
    #[inline]
    fn from(range: VmCowRange) -> Self {
        Self { offset: range.offset, len: range.len }
    }
}

::zr::static_assert!(core::mem::size_of::<VmCowRange>() == 16);
::zr::static_assert!(
    core::mem::size_of::<VmCowRange>() == core::mem::size_of::<bindings::VmCowRange>()
);
::zr::static_assert!(core::mem::align_of::<VmCowRange>() == 8);
::zr::static_assert!(
    core::mem::align_of::<VmCowRange>() == core::mem::align_of::<bindings::VmCowRange>()
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowRange, offset)
        == core::mem::offset_of!(bindings::VmCowRange, offset)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowRange, len) == core::mem::offset_of!(bindings::VmCowRange, len)
);
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmCowReclaimFailure {
    EvictAccessed = 0,
    CompressAccessed = 1,
    CompressFailed = 2,
    IncorrectPage = 3,
    Other = 4,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimFailure,
    core::mem::size_of::<bindings::VmCowReclaimFailure>(),
    core::mem::align_of::<bindings::VmCowReclaimFailure>(),
);
::zr::static_assert!(
    VmCowReclaimFailure::EvictAccessed as u8 == bindings::VmCowReclaimFailure::EvictAccessed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::CompressAccessed as u8
        == bindings::VmCowReclaimFailure::CompressAccessed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::CompressFailed as u8
        == bindings::VmCowReclaimFailure::CompressFailed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::IncorrectPage as u8 == bindings::VmCowReclaimFailure::IncorrectPage as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::Other as u8 == bindings::VmCowReclaimFailure::Other as u8
);
impl From<bindings::VmCowReclaimFailure> for VmCowReclaimFailure {
    #[inline]
    fn from(value: bindings::VmCowReclaimFailure) -> Self {
        match value {
            bindings::VmCowReclaimFailure::EvictAccessed => Self::EvictAccessed,
            bindings::VmCowReclaimFailure::CompressAccessed => Self::CompressAccessed,
            bindings::VmCowReclaimFailure::CompressFailed => Self::CompressFailed,
            bindings::VmCowReclaimFailure::IncorrectPage => Self::IncorrectPage,
            bindings::VmCowReclaimFailure::Other => Self::Other,
        }
    }
}
impl From<VmCowReclaimFailure> for bindings::VmCowReclaimFailure {
    #[inline]
    fn from(value: VmCowReclaimFailure) -> Self {
        match value {
            VmCowReclaimFailure::EvictAccessed => Self::EvictAccessed,
            VmCowReclaimFailure::CompressAccessed => Self::CompressAccessed,
            VmCowReclaimFailure::CompressFailed => Self::CompressFailed,
            VmCowReclaimFailure::IncorrectPage => Self::IncorrectPage,
            VmCowReclaimFailure::Other => Self::Other,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmCowReclaimType {
    Evict = 0,
    Discard = 1,
    Compress = 2,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimType,
    core::mem::size_of::<bindings::VmCowReclaimSuccess_Type>(),
    core::mem::align_of::<bindings::VmCowReclaimSuccess_Type>(),
);
::zr::static_assert!(
    VmCowReclaimType::Evict as u8 == bindings::VmCowReclaimSuccess_Type::Evict as u8
);
::zr::static_assert!(
    VmCowReclaimType::Discard as u8 == bindings::VmCowReclaimSuccess_Type::Discard as u8
);
::zr::static_assert!(
    VmCowReclaimType::Compress as u8 == bindings::VmCowReclaimSuccess_Type::Compress as u8
);
impl From<bindings::VmCowReclaimSuccess_Type> for VmCowReclaimType {
    #[inline]
    fn from(value: bindings::VmCowReclaimSuccess_Type) -> Self {
        match value {
            bindings::VmCowReclaimSuccess_Type::Evict => Self::Evict,
            bindings::VmCowReclaimSuccess_Type::Discard => Self::Discard,
            bindings::VmCowReclaimSuccess_Type::Compress => Self::Compress,
        }
    }
}
impl From<VmCowReclaimType> for bindings::VmCowReclaimSuccess_Type {
    #[inline]
    fn from(value: VmCowReclaimType) -> Self {
        match value {
            VmCowReclaimType::Evict => Self::Evict,
            VmCowReclaimType::Discard => Self::Discard,
            VmCowReclaimType::Compress => Self::Compress,
        }
    }
}

/// Result of attempting to reclaim a single page. See `VmCowPages::ReclaimPage` for more details.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmCowReclaimSuccess {
    pub type_: VmCowReclaimType,
    pub num_pages: u64,
    pub num_loaned_pages: u64,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimSuccess,
    core::mem::size_of::<bindings::VmCowReclaimSuccess>(),
    core::mem::align_of::<bindings::VmCowReclaimSuccess>(),
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, type_)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, type_)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, num_pages)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, num_pages)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, num_loaned_pages)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, num_loaned_pages)
);
impl From<bindings::VmCowReclaimSuccess> for VmCowReclaimSuccess {
    #[inline]
    fn from(value: bindings::VmCowReclaimSuccess) -> Self {
        Self {
            type_: value.type_.into(),
            num_pages: value.num_pages,
            num_loaned_pages: value.num_loaned_pages,
        }
    }
}
impl From<VmCowReclaimSuccess> for bindings::VmCowReclaimSuccess {
    #[inline]
    fn from(value: VmCowReclaimSuccess) -> Self {
        Self {
            type_: value.type_.into(),
            __bindgen_padding_0: [0; 7],
            num_pages: value.num_pages,
            num_loaned_pages: value.num_loaned_pages,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSourceType {
    /// The VmCowPages is not directly backed by a `page_source`.
    Anonymous = 0,
    /// The VmCowPages is directly backed by a contiguous page source. That has the following
    /// implications:
    /// * The direct page source provides specific physical pages.
    /// * The direct page source (possibly with the help of the VmCowPages) provides zeroed pages.
    Contiguous = 1,
    /// The VmCowPages is directly backed by a user pager. That has the following implications:
    /// * The direct page source is responsible for freeing the page content.
    /// * The VmCowPages must implement dirty tracking.
    UserPager = 2,
}
::zr::static_assert_size_and_align!(
    PageSourceType,
    core::mem::size_of::<bindings::PageSourceType>(),
    core::mem::align_of::<bindings::PageSourceType>(),
);
::zr::static_assert!(PageSourceType::Anonymous as u8 == bindings::PageSourceType::Anonymous as u8);
::zr::static_assert!(
    PageSourceType::Contiguous as u8 == bindings::PageSourceType::Contiguous as u8
);
::zr::static_assert!(PageSourceType::UserPager as u8 == bindings::PageSourceType::UserPager as u8);
impl From<bindings::PageSourceType> for PageSourceType {
    #[inline]
    fn from(value: bindings::PageSourceType) -> Self {
        match value {
            bindings::PageSourceType::Anonymous => Self::Anonymous,
            bindings::PageSourceType::Contiguous => Self::Contiguous,
            bindings::PageSourceType::UserPager => Self::UserPager,
        }
    }
}
impl From<PageSourceType> for bindings::PageSourceType {
    #[inline]
    fn from(value: PageSourceType) -> Self {
        match value {
            PageSourceType::Anonymous => Self::Anonymous,
            PageSourceType::Contiguous => Self::Contiguous,
            PageSourceType::UserPager => Self::UserPager,
        }
    }
}

/// See DiscardableVmoTracker::DebugDiscardablePageCounts().
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DiscardablePageCounts {
    pub locked: u64,
    pub unlocked: u64,
}
::zr::static_assert_size_and_align!(
    DiscardablePageCounts,
    core::mem::size_of::<bindings::VmCowPages_DiscardablePageCounts>(),
    core::mem::align_of::<bindings::VmCowPages_DiscardablePageCounts>(),
);
::zr::static_assert!(
    core::mem::offset_of!(DiscardablePageCounts, locked)
        == core::mem::offset_of!(bindings::VmCowPages_DiscardablePageCounts, locked)
);
::zr::static_assert!(
    core::mem::offset_of!(DiscardablePageCounts, unlocked)
        == core::mem::offset_of!(bindings::VmCowPages_DiscardablePageCounts, unlocked)
);

/// Argument that specifies the context in which we are supplying pages.
pub type SupplyOptions = bindings::SupplyOptions;

/// Controls the type of `VmPageOrMarker` slot in `self`'s `page_list` that can be overwritten by
/// the `add_[new_]page[s]_locked` functions. It is the caller's responsibility to ensure that the
/// previous content is dealt with correctly (e.g. any pages and compressed references are freed).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanOverwriteSlot {
    /// Only overwrite empty slots.
    Empty = 0,
    /// Can overwrite parent content markers (in addition to empty slots).
    EmptyOrParent = 1,
    /// Can overwrite zero page markers and zero intervals (in addition to empty slots and
    /// parent content markers).
    ZeroMarkerOrInterval = 2,
    /// Can overwrite pages and compressed references (in addition to empty slots, parent content
    /// markers, zero markers and intervals), i.e. can overwrite all VmPageOrMarker types.
    PageOrRef = 3,
}
::zr::static_assert_size_and_align!(
    CanOverwriteSlot,
    core::mem::size_of::<bindings::VmCowPages_CanOverwriteSlot>(),
    core::mem::align_of::<bindings::VmCowPages_CanOverwriteSlot>(),
);
::zr::static_assert!(
    CanOverwriteSlot::Empty as u8 == bindings::VmCowPages_CanOverwriteSlot::Empty as u8
);
::zr::static_assert!(
    CanOverwriteSlot::EmptyOrParent as u8
        == bindings::VmCowPages_CanOverwriteSlot::EmptyOrParent as u8
);
::zr::static_assert!(
    CanOverwriteSlot::ZeroMarkerOrInterval as u8
        == bindings::VmCowPages_CanOverwriteSlot::ZeroMarkerOrInterval as u8
);
::zr::static_assert!(
    CanOverwriteSlot::PageOrRef as u8 == bindings::VmCowPages_CanOverwriteSlot::PageOrRef as u8
);
impl From<bindings::VmCowPages_CanOverwriteSlot> for CanOverwriteSlot {
    #[inline]
    fn from(value: bindings::VmCowPages_CanOverwriteSlot) -> Self {
        match value {
            bindings::VmCowPages_CanOverwriteSlot::Empty => Self::Empty,
            bindings::VmCowPages_CanOverwriteSlot::EmptyOrParent => Self::EmptyOrParent,
            bindings::VmCowPages_CanOverwriteSlot::ZeroMarkerOrInterval => {
                Self::ZeroMarkerOrInterval
            }
            bindings::VmCowPages_CanOverwriteSlot::PageOrRef => Self::PageOrRef,
        }
    }
}
impl From<CanOverwriteSlot> for bindings::VmCowPages_CanOverwriteSlot {
    #[inline]
    fn from(value: CanOverwriteSlot) -> Self {
        match value {
            CanOverwriteSlot::Empty => Self::Empty,
            CanOverwriteSlot::EmptyOrParent => Self::EmptyOrParent,
            CanOverwriteSlot::ZeroMarkerOrInterval => Self::ZeroMarkerOrInterval,
            CanOverwriteSlot::PageOrRef => Self::PageOrRef,
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct VmCowPagesLockClass;

impl LockClass for VmCowPagesLockClass {
    const ID: *mut c_void = core::ptr::null_mut();
    const FLAGS: ksync::LockFlags = TrackedCriticalMutex::LOCK_FLAGS | ksync::LOCK_FLAGS_NESTABLE;
}

pub type VmCowPagesLock = KMutex<VmCowPagesLockClass, TrackedCriticalMutex>;

/// Scoped container for pages and loaned-page holders to be freed after releasing locks or
/// updating mappings.
#[pin_data(PinnedDrop)]
#[repr(C)]
struct ScopedPageFreedList {
    // Safety invariant: every page linked into `list` is an allocated `OBJECT`-state page that has
    // been removed from its page list and from the page queues by a `VmCowPages` in the same
    // hierarchy as the one passed to `free_pages` (so it shares that VMO's page source freeing
    // policy).
    #[pin]
    list: VmPageDoublyLinkedList,
    /// The FLPH is a moderately large object and is kept uninitialized (tracked by
    /// `flph_has_value`) to defer its construction unless it is actually needed.
    // Safety invariant: `flph` holds an initialized, pinned `FreeLoanedPagesHolder` iff
    // `flph_has_value` is true. `Opaque` is `!Unpin`, so the holder can only be reached through
    // `flph.get()` and is never moved once initialized.
    #[pin]
    flph: Opaque<FreeLoanedPagesHolder>,
    flph_has_value: bool,
}

::zr::static_assert!(
    core::mem::size_of::<ScopedPageFreedList>()
        == core::mem::size_of::<bindings::ScopedPageFreedList>()
);
::zr::static_assert!(
    core::mem::align_of::<ScopedPageFreedList>()
        == core::mem::align_of::<bindings::ScopedPageFreedList>()
);

impl ScopedPageFreedList {
    /// Creates a new pinned initializer for `ScopedPageFreedList`.
    #[inline]
    fn new() -> impl PinInit<Self> {
        pin_init!(&_this in Self {
            list <- VmPageDoublyLinkedList::new(),
            flph: Opaque::uninit(),
            flph_has_value: false,
        })
    }

    /// Frees all pages and loaned pages held in this list.
    ///
    /// # Safety
    ///
    /// Every page in `list` must be an allocated `OBJECT`-state page that has been removed from
    /// its page list and from the page queues by `cow_pages` or by another `VmCowPages` in the
    /// same hierarchy (see the safety invariant on `list`).
    #[inline]
    unsafe fn free_pages(self: Pin<&mut Self>, cow_pages: &VmCowPages) {
        let mut this = self.project();
        if !this.list.is_empty() {
            // SAFETY: Contract of `VmCowPages::free_pages`: valid allocated pages removed from a
            // `VmCowPages` in `cow_pages`' hierarchy and not in any page queue. This is the safety
            // invariant on `list` plus this function's `# Safety`.
            unsafe {
                cow_pages.free_pages(this.list.as_mut());
            }
        }
        if *this.flph_has_value {
            // SAFETY: Field invariant: `flph_has_value` implies `flph` is initialized and pinned,
            // and `this` is an exclusive borrow of the enclosing list.
            let flph = unsafe { Pin::new_unchecked(&mut *this.flph.get()) };
            pmm::node().finish_free_loaned_pages(flph);
        }
    }

    /// Returns a pinned mutable reference to the non-loaned freed page list.
    ///
    /// # Safety
    ///
    /// Any pages added to the returned list must uphold `list`'s safety invariant (allocated
    /// `OBJECT`-state pages removed from their page list and from the page queues by a
    /// `VmCowPages` in the same hierarchy as the one later passed to `free_pages`).
    #[inline]
    unsafe fn list(self: Pin<&mut Self>) -> Pin<&mut VmPageDoublyLinkedList> {
        self.project().list
    }

    /// Lazily initializes the `FreeLoanedPagesHolder` in place and returns a raw pointer to it
    /// without minting a `&mut FreeLoanedPagesHolder` when already initialized.
    #[inline]
    fn flph_raw(self: Pin<&mut Self>) -> *mut FreeLoanedPagesHolder {
        let this = self.project();
        let flph: *mut FreeLoanedPagesHolder = this.flph.get();
        if !*this.flph_has_value {
            // SAFETY: Contract of `PinInit::__pinned_init`: valid, aligned, pinned storage for a
            // `FreeLoanedPagesHolder` that holds no value. `flph_has_value == false` gives the
            // latter by the field invariant; the initializer is infallible.
            unsafe {
                let Ok(()) = FreeLoanedPagesHolder::init().__pinned_init(flph);
            }
            *this.flph_has_value = true;
        }
        flph
    }

    /// Lazily initializes and returns a pinned mutable reference to the `FreeLoanedPagesHolder`.
    #[inline]
    fn flph(self: Pin<&mut Self>) -> Pin<&mut FreeLoanedPagesHolder> {
        let flph = self.flph_raw();
        // SAFETY: `flph_raw` initialized `flph` in place at a pinned address (`flph_has_value ==
        // true`), and `self` is an exclusive borrow of the enclosing list.
        unsafe { Pin::new_unchecked(&mut *flph) }
    }
}

#[pin_init::pinned_drop]
impl PinnedDrop for ScopedPageFreedList {
    fn drop(self: Pin<&mut Self>) {
        assert!(self.list.is_empty());
        if self.flph_has_value {
            // SAFETY: By the field invariant, `flph_has_value` means `flph` holds an initialized,
            // pinned holder, and `drop` has exclusive access to `self`.
            unsafe {
                ptr::drop_in_place(self.flph.get());
            }
        }
    }
}

/// Track any potential range change update that should be run over the cow children.
#[repr(C)]
#[derive(Clone, Copy)]
struct DeferredRangeOp {
    op: RangeChangeOp,
    range: VmCowRange,
}

/// C++-owned holder of the page source lock (`Guard<Mutex>` + `RefPtr<PageSource>`) acquired for
/// the lifetime of a [`DeferredOps`]; see the `page_source_lock` field for why it is held.
///
/// TODO(https://fxbug.dev/531878739): Port the lock walk and replace this with a Rust guard.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
struct DeferredPageSourceLock {
    #[pin]
    opaque: Opaque<bindings::DeferredPageSourceLock>,
}

// SAFETY: `DeferredPageSourceLock` is `#[repr(transparent)]` over `opaque`, which
// `cpp_vm_cow_pages_deferred_ops_init_page_source_lock` always initializes (even on the dead-VMO
// early return), so the destroy shim always sees a constructed C++ object.
unsafe_pinned_drop_ffi!(
    DeferredPageSourceLock,
    bindings::cpp_vm_cow_pages_deferred_ops_destroy_page_source_lock
);

impl DeferredPageSourceLock {
    /// Acquires the page source lock for `cow`'s hierarchy (if any) in place. `cow`'s lock must
    /// not be held.
    #[inline]
    fn new(cow: &VmCowPages) -> impl PinInit<Self> + '_ {
        // SAFETY: `cow.as_raw()` is a valid `VmCowPages` for the duration of the initializer, and
        // `cpp_vm_cow_pages_deferred_ops_init_page_source_lock` always initializes the
        // `DeferredPageSourceLock` in place (it takes `cow`'s lock itself, which the caller does
        // not hold).
        pin_init_ffi!(bindings::cpp_vm_cow_pages_deferred_ops_init_page_source_lock, cow.as_raw())
    }
}

/// Helper object for finishing VmCowPages operations that must occur after the lock is dropped.
/// This is necessary due to some operations being externally locked. Consider this sequence:
///
/// ```
/// stack_pin_init!(let deferred = DeferredOps::new(&cow_object));
/// ksync::lock!(let guard = cow_object.lock());
/// cow_object.do_operation_locked(&deferred);
/// ```
///
/// The destruction order will then allow `deferred` to perform its actions after `guard` is
/// destructed and the lock is dropped.
///
/// This struct is not thread safe.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct DeferredOps<'a> {
    /// A reference to the VmCowPages for any deferred operations to be run against.
    cow: &'a VmCowPages,
    /// Track any potential range change update that should be run over the cow children.
    range_op: Option<DeferredRangeOp>,
    /// Track any resources that need to be freed after the range change update.
    // Safety invariant: every page linked into `freed_list.list` is an allocated `OBJECT`-state
    // page that was removed from its page list and from the page queues by `cow` or another
    // `VmCowPages` in `cow`'s hierarchy.
    #[pin]
    freed_list: ScopedPageFreedList,
    /// When operating on a VMO from a hierarchy that has a page source the page source lock is
    /// held over both the operation and our deferred operations. This serves to serialize
    /// operations against all VMOs in the hierarchy. This serialization is necessary since a
    /// hierarchy with a page source has parent VMOs whose contents is able to change, and if we
    /// had parallelism between multiple mutating operations with range change updates user space
    /// would be able to see inconsistent views of memory.
    /// In addition to the lock itself, held via its Guard, we also hold a RefPtr to the PageSource
    /// itself. During the lifetime of the DeferredOps it is possible for the self vmo to become
    /// detached from the rest of the vmo tree, and for the remainder of the tree, including the
    /// root node with the page source to be destroyed. Holding a RefPtr to the page source of the
    /// mutex we are holding therefore prevents a use-after-free of the guard.
    ///
    /// Declared last so that the lock is released only after every other field has been dropped;
    /// the freed pages themselves are returned in `drop`, before any field is dropped.
    #[pin]
    page_source_lock: DeferredPageSourceLock,
    phantom: PhantomData<*const ()>,
}

::zr::static_assert!(
    core::mem::size_of::<DeferredOps<'_>>()
        == core::mem::size_of::<bindings::VmCowPages_DeferredOps>()
);
::zr::static_assert!(
    core::mem::align_of::<DeferredOps<'_>>()
        == core::mem::align_of::<bindings::VmCowPages_DeferredOps>()
);

impl<'a> DeferredOps<'a> {
    /// Construct a `DeferredOps` for the given `VmCowPages`. Must be constructed, and
    /// deconstructed, without the lock held. It is the caller's responsibility to ensure the
    /// pointer remains valid over the lifetime of the object.
    pub fn new(cow: &'a VmCowPages) -> impl PinInit<Self> {
        pin_init!(&_this in Self {
            cow,
            range_op: None,
            freed_list <- ScopedPageFreedList::new(),
            page_source_lock <- DeferredPageSourceLock::new(cow),
            phantom: PhantomData,
        })
    }

    // Methods below are private as they are only intended for use by the `VmCowPages` and not the
    // external caller holding this object on their stack.

    #[inline]
    fn as_raw(self: Pin<&mut Self>) -> *mut bindings::VmCowPages_DeferredOps {
        // SAFETY: Obtaining a raw pointer does not move the pinned `DeferredOps`.
        ptr::from_mut(unsafe { self.get_unchecked_mut() }).cast()
    }

    /// Indicate that the given range change operation should be performed later. Multiple ranges
    /// can be specified, although only a single range that covers all of them will actually be
    /// invalidated later, and the requested ops must all be the same (a mix of `Unmap` and
    /// `UnmapZeroPage` can be given, with the entire operation upgraded to `Unmap`).
    fn add_range(
        self: Pin<&mut Self>,
        self_cow: &VmCowPages,
        range: VmCowRange,
        op: RangeChangeOp,
    ) {
        debug_assert!(ptr::eq(self_cow, self.cow));
        let this = self.project();
        match this.range_op {
            Some(range_op) => {
                if range_op.op != op {
                    // Permit an UnmapZeroPage to be upgraded to an Unmap. If already an Unmap,
                    // then ignore any UnmapZeroPage.
                    if range_op.op == RangeChangeOp::UnmapZeroPage && op == RangeChangeOp::Unmap {
                        range_op.op = op;
                    } else {
                        debug_assert!(
                            range_op.op == RangeChangeOp::Unmap
                                && op == RangeChangeOp::UnmapZeroPage
                        );
                    }
                }
                range_op.range = range_op.range.cover(range);
            }
            None => {
                *this.range_op = Some(DeferredRangeOp { op, range });
            }
        }
    }

    /// Retrieves the underlying resource containers. Any pages (loaned or otherwise) that are
    /// added will be freed *after* any range change operations are first performed.
    #[inline]
    fn freed_list(self: Pin<&mut Self>, self_cow: &VmCowPages) -> Pin<&mut ScopedPageFreedList> {
        debug_assert!(ptr::eq(self_cow, self.cow));
        self.project().freed_list
    }
}

#[pin_init::pinned_drop]
impl PinnedDrop for DeferredOps<'_> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        let self_cow: &VmCowPages = this.cow;
        if let Some(range_op) = *this.range_op {
            // SAFETY: `self_cow` is a live `VmCowPages` for the lifetime `'a` of this
            // `DeferredOps`, and per `new` it is dropped without `self_cow`'s lock held, which the
            // shim acquires itself. The range is passed as `(offset, len)` rather than a
            // `VmCowRange` by value so ThinLTO can inline the shim on x86_64.
            unsafe {
                bindings::cpp_vm_cow_pages_range_change_update_cow_children(
                    self_cow.as_raw(),
                    range_op.range.offset,
                    range_op.range.len,
                    range_op.op,
                );
            }
        }
        // The pages must be freed *after* any range update is performed, but *before* dropping the
        // `page_source_lock`. In the case where the page source is handling free this is still a
        // logical operation involving the cow pages and must remain serialized, as demonstrated by
        // `free_pages` itself taking a reference to the `VmCowPages`.
        // SAFETY: Pages only enter `freed_list` through `freed_list(self_cow)` /
        // `remove_page_locked`, whose contracts require the pages to have been removed from their
        // page list and the page queues by `self_cow` or another `VmCowPages` in its hierarchy.
        unsafe { this.freed_list.free_pages(self_cow) };
    }
}

::zr::static_assert_size_and_align!(
    PAddr,
    core::mem::size_of::<zx_paddr_t>(),
    core::mem::align_of::<zx_paddr_t>()
);

/// Implements a cursor that allows for retrieving successive pages over a range in a VMO. The
/// range that is iterated is determined at construction from
/// [`get_lookup_cursor_locked`](super::vm_object_paged::VmObjectPaged::get_lookup_cursor_locked)
/// and cannot be modified, although it can be effectively shrunk by ceasing queries early.
///
/// The cursor is designed under the assumption that the caller is tracking, implicitly or
/// explicitly, how many queries have been done, and the methods do not return errors if more slots
/// are queried than was originally requested in the range. They will, however, assert and panic.
///
/// There are three controls provided by this object.
///
///   Zero forks: By default new zero pages will be considered zero forks and added to the zero page
///   scanner list, this can be disabled with `disable_zero_fork`.
///
///   Access time: By default pages that are returned will be considered accessed. This can be
///   changed with [`disable_mark_accessed`](Self::disable_mark_accessed).
///
///   Allocation lists: By default pages will be acquired from the pmm as needed. An allocation list
///   can be given using `give_alloc_list`.
///
/// The VMO lock *must* be held contiguously from the call to `get_lookup_cursor_locked` over the
/// entire usage of this object. Callers can assume that for any [`VmPagePtr`] that is returned
/// that the lock of the owner of that page is held up until the next operation performed on the
/// cursor.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct LookupCursor<'a> {
    #[pin]
    opaque: Opaque<bindings::VmCowPagesLookupCursorResult>,
    token: LockToken<'a, VmCowPagesLockClass>,
    phantom: PhantomData<(&'a VmCowPages, &'a mut LockToken<'a, VmCowPagesLockClass>)>,
}

zr::static_assert!(core::mem::offset_of!(LookupCursor<'static>, opaque) == 0);

// SAFETY: `LookupCursor` is `#[repr(C)]` with `opaque` at offset 0, and `opaque` is initialized
// whenever a `LookupCursor` exists, so passing its address to
// `cpp_vm_cow_pages_lookup_cursor_destroy` destroys the cursor in place.
unsafe_pinned_drop_ffi!(LookupCursor<'_>, bindings::cpp_vm_cow_pages_lookup_cursor_destroy);

/// Convenience struct holding the return result of the `require_*` methods.
#[derive(Debug, Copy, Clone)]
pub struct RequireResult {
    pub page: VmPagePtr,
    pub writable: bool,
}

impl<'a> LookupCursor<'a> {
    /// # Safety
    ///
    /// The caller must ensure `init` initializes the `VmCowPagesLookupCursorResult` it is passed
    /// with a cursor when it returns `Ok`, and leaves it uninitialized when it returns `Err`. The
    /// caller must also ensure `_token` is for the lock of the `VmCowPages` that the cursor
    /// targets, and that `VmCowPages` outlives `'a`.
    #[inline]
    pub(super) unsafe fn new(
        _token: &'a mut LockToken<'_, VmCowPagesLockClass>,
        init: impl FnOnce(*mut bindings::VmCowPagesLookupCursorResult) -> Result<(), Status>,
    ) -> impl PinInit<Self, Status> {
        let init = move |slot: *mut Self| {
            // SAFETY: `slot` is non-null, aligned, and points to an allocation for `Self`.
            let opaque = unsafe { (&raw mut (*slot).opaque).cast() };
            if let Err(err) = init(opaque) {
                core::hint::cold_path();
                return Err(err);
            }
            // SAFETY: `slot` is valid for writes. The lock is held for `'a`, because `_token`, the
            // caller's token for it, is borrowed exclusively for `'a`.
            unsafe { (&raw mut (*slot).token).write(LockToken::new()) };
            Ok(())
        };
        // SAFETY: `init` initializes `slot` when it returns `Ok`, and leaves it uninitialized when
        // it returns `Err`.
        unsafe { pin_init_from_closure(init) }
    }

    #[inline]
    fn as_raw(self: Pin<&mut Self>) -> *mut bindings::VmCowPages_LookupCursor {
        let result = self.project().opaque.get();
        // SAFETY: `result` is a live `zx::result` that holds a cursor.
        unsafe { bindings::cpp_vm_cow_pages_lookup_cursor_result_value(result) }
    }

    /// Attempts to lookup the next offset in the VMO and return an allocated and owned page in
    /// this VMO. As such this will never return a reference to the zero page. `will_write`
    /// indicates if this page needs to be writable or not, which for an owned and allocated page
    /// just involves a potential dirty request / transition.
    ///
    /// If a page is returned in `Ok`, the internal cursor is incremented by one page and future
    /// operations act on the next offset. If an error occurs, the internal cursor is not
    /// incremented.
    ///
    /// `page_request` is populated when returning `Status::SHOULD_WAIT`. For optimal page request
    /// generation `max_request_pages` controls how many pages you intend to lookup, and must not
    /// exceed the remaining window of the cursor.
    ///
    /// Unless newly allocated, the returned page has its access time updated unless disabled via
    /// [`disable_mark_accessed`](Self::disable_mark_accessed).
    ///
    /// # Safety
    ///
    /// The caller must ensure `deferred` was made for the VMO this cursor was made for,
    /// `max_request_pages` is non-zero and at most the number of pages left in the range of the
    /// cursor, and `page_request` does not hold an active request.
    #[inline]
    pub unsafe fn require_owned_page(
        self: Pin<&mut Self>,
        will_write: bool,
        max_request_pages: u64,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> Result<RequireResult, Status> {
        let mut page = ptr::null_mut();
        let mut writable = false;
        // SAFETY: `self.as_raw()`, `deferred`, and `page_request` point to live pinned objects,
        // `deferred` was made for the VMO of the cursor, `max_request_pages` is non-zero and at
        // most the number of pages left in the range of the cursor, `page_request` does not hold
        // an active request, `page` and `writable` are valid for writes, and the VMO lock is held.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_lookup_cursor_require_owned_page(
                self.as_raw(),
                will_write,
                max_request_pages,
                deferred.as_raw(),
                page_request.as_raw(),
                &mut page,
                &mut writable,
            )
        };
        if let Err(err) = Status::ok(status) {
            core::hint::cold_path();
            return Err(err);
        }
        // SAFETY: `page` is non-null and points to a valid `vm_page_t`.
        let page = unsafe { VmPagePtr::from_ffi_unchecked(page) };
        Ok(RequireResult { page, writable })
    }

    /// Attempts to lookup the next offset in the VMO and return a page that is readable or
    /// writable based on the `will_write` flag.
    ///
    /// Cursor advancement, `page_request`, and access-time behavior match
    /// [`require_owned_page`](Self::require_owned_page).
    ///
    /// # Safety
    ///
    /// The caller must ensure `deferred` was made for the VMO this cursor was made for,
    /// `max_request_pages` is non-zero and at most the number of pages left in the range of the
    /// cursor, and `page_request` does not hold an active request.
    #[inline]
    pub unsafe fn require_page(
        self: Pin<&mut Self>,
        will_write: bool,
        max_request_pages: u64,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> Result<RequireResult, Status> {
        let mut page = ptr::null_mut();
        let mut writable = false;
        // SAFETY: `self.as_raw()`, `deferred`, and `page_request` point to live pinned objects,
        // `deferred` was made for the VMO of the cursor, `max_request_pages` is non-zero and at
        // most the number of pages left in the range of the cursor, `page_request` does not hold
        // an active request, `page` and `writable` are valid for writes, and the VMO lock is held.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_lookup_cursor_require_page(
                self.as_raw(),
                will_write,
                max_request_pages,
                deferred.as_raw(),
                page_request.as_raw(),
                &mut page,
                &mut writable,
            )
        };
        if let Err(err) = Status::ok(status) {
            core::hint::cold_path();
            return Err(err);
        }
        // SAFETY: `page` is non-null and points to a valid `vm_page_t`.
        let page = unsafe { VmPagePtr::from_ffi_unchecked(page) };
        Ok(RequireResult { page, writable })
    }

    /// The `if_exist_pages` method is intended to be cheaper than the `require_*` methods and to
    /// allow for performing actions if pages already exist, without performing allocations. As a
    /// result this may fail to return pages in scenarios that `require_*` methods would, and in
    /// general is allowed to always fail for any reason.
    /// This method cannot generate page requests and will not perform allocations or otherwise
    /// mutate the VMO contents and will not update the access time of the pages.
    ///
    /// Walks up to `paddrs.len()` pages from the current offset, filling in `paddrs` as long as
    /// there are actual pages and, if `will_write` is true, that they can be written to. The
    /// return value is the slice of contiguous pages found and filled into `paddrs`, and the
    /// cursor is incremented by that many pages.
    ///
    /// # Safety
    ///
    /// The caller must ensure `paddrs.len()` is at most the number of pages left in the range of
    /// the cursor.
    #[inline]
    pub unsafe fn if_exist_pages<'p>(
        self: Pin<&mut Self>,
        will_write: bool,
        paddrs: &'p mut [MaybeUninit<PAddr>],
    ) -> &'p mut [PAddr] {
        if paddrs.is_empty() {
            return &mut [];
        }
        // SAFETY: `self.as_raw()` is a live `LookupCursor`, the VMO lock is held, `paddrs` is
        // non-empty and at most the number of pages left in the range of the cursor, and `paddrs`
        // is valid for writing `paddrs.len()` entries.
        let found = unsafe {
            bindings::cpp_vm_cow_pages_lookup_cursor_if_exist_pages(
                self.as_raw(),
                will_write,
                paddrs.len() as u64,
                paddrs.as_mut_ptr().cast::<zx_paddr_t>(),
            )
        } as usize;
        debug_assert!(found <= paddrs.len());
        // SAFETY: `IfExistPages` initialized the first `found` entries of `paddrs`, `PAddr` has the
        // same layout as `MaybeUninit<PAddr>`, `paddrs` is a valid slice so its pointer is non-null
        // and aligned even when `found == 0`, and the result exclusively borrows `paddrs` for `'p`.
        unsafe { core::slice::from_raw_parts_mut(paddrs.as_mut_ptr().cast::<PAddr>(), found) }
    }

    /// Checks the current slot for a page and returns it. This does not return zero pages and, due
    /// to the lack of taking a page request, will not perform copy-on-write allocations or dirty
    /// transitions. In these cases it will return `None` even though there is content.
    /// The internal cursor is always incremented regardless of the return value.
    ///
    /// # Safety
    ///
    /// The caller must ensure the range of the cursor has a page left.
    #[inline]
    pub unsafe fn maybe_page(self: Pin<&mut Self>, will_write: bool) -> Option<VmPagePtr> {
        // SAFETY: `self.as_raw()` is a live `LookupCursor`, its range has a page left, and the VMO
        // lock is held.
        let page = unsafe {
            bindings::cpp_vm_cow_pages_lookup_cursor_maybe_page(self.as_raw(), will_write)
        };
        // SAFETY: `page` is either null or points to a valid `vm_page_t`.
        unsafe { VmPagePtr::from_ffi(page) }
    }

    /// Has similar properties of `maybe_page`, except it returns how many times in a row
    /// `maybe_page` would have returned `None`. Regardless of the return value of this method, it
    /// is not guaranteed that the next call to `maybe_page` will not be `None`. The cursor is
    /// incremented by the number of pages returned.
    ///
    /// # Safety
    ///
    /// The caller must ensure the range of the cursor has a page left.
    #[inline]
    pub unsafe fn skip_missing_pages(self: Pin<&mut Self>) -> u64 {
        // SAFETY: `self.as_raw()` is a live `LookupCursor`, its range has a page left, and the VMO
        // lock is held.
        unsafe { bindings::cpp_vm_cow_pages_lookup_cursor_skip_missing_pages(self.as_raw()) }
    }

    /// Indicates that any existing pages that are returned should not be considered accessed and
    /// have their accessed times updated.
    #[inline]
    pub fn disable_mark_accessed(self: Pin<&mut Self>) {
        // SAFETY: `self.as_raw()` is a live `LookupCursor`.
        unsafe { bindings::cpp_vm_cow_pages_lookup_cursor_disable_mark_accessed(self.as_raw()) }
    }

    /// Exposed for lock assertions.
    #[inline]
    pub fn lock_is_held(&self) -> bool {
        // SAFETY: `self.opaque` is a live `zx::result` that holds a cursor.
        unsafe {
            bindings::cpp_vm_cow_pages_lookup_cursor_lock_is_held(self.opaque.get().cast_const())
        }
    }

    /// # Safety
    ///
    /// The caller must ensure nothing done with the returned token changes the page lists, parent
    /// links, parent limits, or sizes of the VMO hierarchy of the cursor, or acquires ancestor
    /// locks in that hierarchy while the cursor is live.
    #[inline]
    pub(super) unsafe fn token_mut(
        self: Pin<&mut Self>,
    ) -> &mut LockToken<'a, VmCowPagesLockClass> {
        self.project().token
    }
}

/// `PriorityChanger` is a transaction object for changing the high priority count of `VmCowPages`
/// objects. The high priority count is used to manage page reclamation. This object ensures that a
/// `DeferredOps` object is created against the correct `VmCowPages`, which might be an ancestor of
/// `target`. `PriorityChanger` does the following:
///
/// * Manages the lifetime of `VmCowPages` to ensure they last until the
/// priority change operation, by holding a `RefPtr`.
/// * Handles the construction of the `DeferredOps` object.
/// * Clarifies which operations need to be done within the `VmCowPages` lock and which need to
///   happen without the lock being held.
/// * Presents a high-level API to `VmMapping` and `VmObjectPaged`.
/// * Handles traversing up the `VmCowPages` hierarchy to ensure that high priority status is
/// propagated.
///
/// Callers should prefer `VmMapping::set_memory_priority_locked`,
/// `VmMapping::set_memory_priority_default_locked_object`, or
/// `VmMapping::set_memory_priority_high_already_positive_locked_object`, if applicable.
///
/// To change the high priority count of a `VmCowPages`, you should first construct the
/// `PriorityChanger` object. Then,
/// * If `delta` < 0, you must immediately call one of the change methods (such as
///   [`change_high_priority_count_locked`](Self::change_high_priority_count_locked)), which
///   change the high priority count of `target`.
/// * If `delta` > 0, you must call an applicable prepare method to set up the prerequisites for the
///   priority change operation, then call one of the change methods.
///
/// This struct is not thread-safe.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct PriorityChanger<'a> {
    #[pin]
    opaque: Opaque<bindings::PriorityChanger>,
    phantom: PhantomData<(&'a VmCowPages, *const ())>,
}

unsafe_pinned_drop_ffi!(PriorityChanger<'_>, bindings::cpp_priority_changer_destroy);

impl<'a> PriorityChanger<'a> {
    /// Construct a priority changer to change the high priority count of `target`. When `delta` is
    /// positive, we go towards high priority. When it's negative, we move towards default
    /// priority.
    ///
    /// The constructor may be called with or without `target`'s lock being held.
    ///
    /// The caller must ensure that `target` remains valid during the lifetime of `PriorityChanger`.
    ///
    /// `delta != 0` is required (`delta == 0` does not do anything meaningful).
    /// Before dropping this object, a change method (such as `change_high_priority_count_locked`)
    /// must be called, and the object must be dropped without holding `target`'s lock.
    pub fn new(cow: &'a VmCowPages, delta: i64) -> impl PinInit<Self> {
        pin_init_ffi!(bindings::cpp_priority_changer_construct, cow.as_raw(), delta)
    }

    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::PriorityChanger {
        // SAFETY: Obtaining a raw pointer to `opaque` does not move the pinned object.
        unsafe { self.get_unchecked_mut().opaque.get() }
    }

    /// Returns a reference to the target `VmCowPages`'s lock.
    pub fn lock(&self) -> &'a VmCowPagesLock {
        // SAFETY: `cpp_priority_changer_lock` returns `&target_->lock_`, a subobject of the
        // target `VmCowPages`. The C++ `Lock<CriticalMutex>` is layout-compatible with
        // `KMutex<VmCowPagesLockClass, TrackedCriticalMutex>`.
        unsafe { &*bindings::cpp_priority_changer_lock(self.opaque.get()).cast::<VmCowPagesLock>() }
    }

    /// This method must be called *without* holding `target`'s `VmCowPages` lock. In addition, the
    /// caller must not hold a `PriorityChanger` or `DeferredOps` on any other `VmCowPages` in
    /// `target`'s hierarchy.
    pub fn prepare_may_not_already_be_high_priority(self: Pin<&mut Self>) {
        // TODO(ethanws): Find a way for the caller to annotate their guarantee that they
        // don't hold the lock.

        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_prepare_may_not_already_be_high_priority(self.as_raw());
        }
    }

    /// This method may be called if `target` already has a `high_priority_count_` > 0.
    ///
    /// This method must be called with `target`'s lock held.
    pub fn prepare_is_already_high_priority_locked(
        self: Pin<&mut Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_prepare_is_already_high_priority_locked(self.as_raw());
        }
    }

    /// Increments or decrements the priority count of this VMO. The high priority count is used to
    /// control any page reclamation, and applies to the whole VMO, including its parents. The count
    /// is never allowed to go negative and so callers must only subtract what they have already
    /// added. Further, callers are required to remove any additions before the VMO is destroyed.
    pub fn change_high_priority_count_locked(
        self: Pin<&mut Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_change_high_priority_count_locked(self.as_raw());
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionAction {
    FollowHint = 0,
    IgnoreHint = 1,
}
::zr::static_assert_size_and_align!(
    EvictionAction,
    core::mem::size_of::<bindings::VmCowPages_EvictionAction>(),
    core::mem::align_of::<bindings::VmCowPages_EvictionAction>(),
);
::zr::static_assert!(
    EvictionAction::FollowHint as u8 == bindings::VmCowPages_EvictionAction::FollowHint as u8
);
::zr::static_assert!(
    EvictionAction::IgnoreHint as u8 == bindings::VmCowPages_EvictionAction::IgnoreHint as u8
);
impl From<bindings::VmCowPages_EvictionAction> for EvictionAction {
    #[inline]
    fn from(value: bindings::VmCowPages_EvictionAction) -> Self {
        match value {
            bindings::VmCowPages_EvictionAction::FollowHint => Self::FollowHint,
            bindings::VmCowPages_EvictionAction::IgnoreHint => Self::IgnoreHint,
        }
    }
}
impl From<EvictionAction> for bindings::VmCowPages_EvictionAction {
    #[inline]
    fn from(value: EvictionAction) -> Self {
        match value {
            EvictionAction::FollowHint => Self::FollowHint,
            EvictionAction::IgnoreHint => Self::IgnoreHint,
        }
    }
}

/// Used to track dirty_state in the vm_page_t.
///
/// The transitions between the three states can roughly be summarized as follows:
/// 1. A page starts off as Clean when supplied.
/// 2. A write transitions the page from Clean to Dirty.
/// 3. A writeback_begin moves the Dirty page to AwaitingClean.
/// 4. A writeback_end moves the AwaitingClean page to Clean.
/// 5. A write that comes in while the writeback is in progress (i.e. the page is AwaitingClean)
///    moves the AwaitingClean page back to Dirty.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyState {
    /// The page does not track dirty state. Used for non pager backed pages.
    Untracked = 0,
    /// The page is clean, i.e. its contents have not been altered from when the page was supplied.
    Clean = 1,
    /// The page's contents have been modified from the time of supply, and should be written back
    /// to the page source at some point.
    Dirty = 2,
    /// The page still has modified contents, but the page source is in the process of writing back
    /// the changes. This is used to ensure that a consistent version is written back, and that any
    /// new modifications that happen during the writeback are not lost. The page source will mark
    /// pages AwaitingClean before starting any writeback.
    AwaitingClean = 3,
    /// Number of states.
    NumStates = 4,
}
::zr::static_assert_size_and_align!(
    DirtyState,
    core::mem::size_of::<bindings::VmCowPages_DirtyState>(),
    core::mem::align_of::<bindings::VmCowPages_DirtyState>(),
);
::zr::static_assert!(
    DirtyState::Untracked as u8 == bindings::VmCowPages_DirtyState::Untracked as u8
);
::zr::static_assert!(DirtyState::Clean as u8 == bindings::VmCowPages_DirtyState::Clean as u8);
::zr::static_assert!(DirtyState::Dirty as u8 == bindings::VmCowPages_DirtyState::Dirty as u8);
::zr::static_assert!(
    DirtyState::AwaitingClean as u8 == bindings::VmCowPages_DirtyState::AwaitingClean as u8
);
::zr::static_assert!(
    DirtyState::NumStates as u8 == bindings::VmCowPages_DirtyState::NumStates as u8
);
::zr::static_assert!((DirtyState::NumStates as u32) <= crate::vm::page::object::MAX_DIRTY_STATES);
impl From<bindings::VmCowPages_DirtyState> for DirtyState {
    #[inline]
    fn from(value: bindings::VmCowPages_DirtyState) -> Self {
        match value {
            bindings::VmCowPages_DirtyState::Untracked => Self::Untracked,
            bindings::VmCowPages_DirtyState::Clean => Self::Clean,
            bindings::VmCowPages_DirtyState::Dirty => Self::Dirty,
            bindings::VmCowPages_DirtyState::AwaitingClean => Self::AwaitingClean,
            bindings::VmCowPages_DirtyState::NumStates => Self::NumStates,
        }
    }
}
impl From<DirtyState> for bindings::VmCowPages_DirtyState {
    #[inline]
    fn from(value: DirtyState) -> Self {
        match value {
            DirtyState::Untracked => Self::Untracked,
            DirtyState::Clean => Self::Clean,
            DirtyState::Dirty => Self::Dirty,
            DirtyState::AwaitingClean => Self::AwaitingClean,
            DirtyState::NumStates => Self::NumStates,
        }
    }
}

/// Tracks the life cycle of the VmCowPages. The primary purpose of the life cycle is to create an
/// invariant that by the time a VmCowPages destructor runs it does not contain any pages. This is
/// achieved by requiring an explicit Dead transition that provides a point to perform cleanup.
/// An Init state is introduced to allow for multi step creation that may fail.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum LifeCycle {
    Init = 0,
    Alive = 1,
    Dying = 2,
    Dead = 3,
}

::zr::static_assert!(LifeCycle::Init as u8 == bindings::VmCowPages_LifeCycle_Init);
::zr::static_assert!(LifeCycle::Alive as u8 == bindings::VmCowPages_LifeCycle_Alive);
::zr::static_assert!(LifeCycle::Dying as u8 == bindings::VmCowPages_LifeCycle_Dying);
::zr::static_assert!(LifeCycle::Dead as u8 == bindings::VmCowPages_LifeCycle_Dead);
::zr::static_assert_size_and_align!(
    LifeCycle,
    core::mem::size_of::<bindings::VmCowPages_LifeCycle>(),
    core::mem::align_of::<bindings::VmCowPages_LifeCycle>(),
);

#[repr(C)]
struct VmCowPagesState {
    /// magic value
    canary: Canary<{ fbl::magic(b"VMCP") }>,
    pmm_alloc_flags: u32,
    options: VmCowPagesOptions,

    /// length of children_list_
    children_list_len: KCell<u32, VmCowPagesLockClass>,

    /// Counts the total number of pages pinned by `commit_range`. If one page is pinned n times, it
    /// contributes n to this count.
    pinned_page_count: KCell<u64, VmCowPagesLockClass>,

    /// Count reclamation events so that we can report them to the user.
    reclamation_event_count: KCell<u64, VmCowPagesLockClass>,

    /// Count of how many references to this VMO are requesting this be high priority, where
    /// references include VmMappings and children. If this is >0 then it is considered high
    /// priority and any kind of reclamation will be disabled. Further, if this is >0 and this has
    /// a parent, then this will contribute a +1 count towards its parent.
    ///
    /// Due to the life cycle of a VmCowPages it is expected that at the point this is destroyed it
    /// has a count of 0. This is because that to be destroyed we must have no mappings and no
    /// children, i.e. no references, and so nothing can be contributing to a positive count.
    ///
    /// It is an error for this value to ever become negative.
    high_priority_count: KCell<i64, VmCowPagesLockClass>,

    /// Tracks whether this VMO was modified (written / resized) if backed by a pager. This gets
    /// reset to false if `query_pager_vmo_stats_locked` is called with `reset` set to true.
    pager_stats_modified: KCell<bool, VmCowPagesLockClass>,

    /// Tracks the life cycle of the VmCowPages. The primary purpose of the life cycle is to create
    /// an invariant that by the time a VmCowPages destructor runs it does not contain any pages.
    /// This is achieved by requiring an explicit Dead transition that provides a point to perform
    /// cleanup. An Init state is introduced to allow for multi step creation that may fail.
    life_cycle: KCell<LifeCycle, VmCowPagesLockClass>,

    /// This is a one-way bool (false -> true) indicating whether this object has ever held pinned
    /// pages.
    ///
    /// With this bool we achieve these things:
    ///  * Avoid using loaned pages for a VMO that will just get pinned and replace the loaned pages
    ///    with non-loaned pages again, possibly repeatedly.
    ///  * Avoid increasing pin latency in the (more) common case of pinning a VMO the 2nd or
    ///    subsequent times (vs the 1st time).
    ///  * Once we have any form of active sweeping (of data from non-loaned to loaned physical
    ///    pages) this bool is part of mitigating any potential DMA-while-not-pinned (which is not
    ///    permitted but is also difficult to detect or prevent without an IOMMU).
    ///
    /// This value is an atomic because it can be read without holding the lock.
    ///
    /// The rules for accessing this value are as follows:
    ///  * Writes must have release semantics and be performed while holding `lock()`.
    ///  * For reads performed while holding `lock()`, relaxed semantics are sufficient.
    ///  * All other reads must use acquire semantics.
    ever_pinned: AtomicBool,

    /// Size in bytes of this cow pages range. This will always be a multiple of the page size.
    size: KCell<u64, VmCowPagesLockClass>,

    /// Offset in the *parent* where this object starts.
    parent_offset: KCell<u64, VmCowPagesLockClass>,

    /// Offset in *this object* above which accesses will no longer access the parent.
    parent_limit: KCell<u64, VmCowPagesLockClass>,

    /// Offset in our root parent where this object would start if projected onto it. This value
    /// is used as an efficient summation of accumulated offsets to ensure that an offset projected
    /// all the way to the root would not overflow a 64-bit integer. Although actual page
    /// resolution would never reach the root in such a case, a childs full range projected onto
    /// its parent is used to simplify some operations and so this invariant of not overflowing
    /// accumulated offsets needs to be maintained.
    root_parent_offset: KCell<u64, VmCowPagesLockClass>,

    /// Tracks the populated bytes count and its high-water mark.
    continuous_attribution_tracker: KCell<AttributionTracker, VmCowPagesLockClass>,

    /// a tree of pages
    page_list: KCell<VmPageList, VmCowPagesLockClass>,
}

::zr::static_assert_size_and_align!(
    VmCowPagesState,
    constants::kVmCowPagesStateSize,
    constants::kVmCowPagesStateAlign,
);

bitflags! {
    #[repr(transparent)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct VmCowPagesOptions: u32 {
        // Externally-usable flags:
        const NONE = 0;
        const USER_PAGER_BACKED_ROOT = 1 << 0;
        const PAGE_SOURCE_ROOT = 1 << 1;
        /// With this clear, zeroing a page tries to decommit the page.  With this set, zeroing
        /// never decommits the page.  Currently this is only set for contiguous VMOs.
        //
        // TODO(dustingreen): Once we're happy with the reliability of page borrowing, we should
        // be able to relax this restriction.  We may still need to flush zeroes to RAM during
        // reclaim to mitigate a hypothetical client incorrectly assuming that cache-clean status
        // will remain intact while pages aren't pinned, but that mitigation should be sufficient
        // (even assuming such a client) to allow implicit decommit when zeroing or when zero
        // scanning, as long as no clients are doing DMA to/from contiguous while not pinned.
        const CANNOT_DECOMMIT_ZERO_PAGES = 1 << 2;

        // Internal-only flags:
        const HIDDEN = 1 << 3;
        const INTERNAL_ONLY_MASK = Self::HIDDEN.bits();
    }
}

::zr::static_assert!(VmCowPagesOptions::NONE.bits() == bindings::VmCowPagesOptions_kNone);
::zr::static_assert!(
    VmCowPagesOptions::USER_PAGER_BACKED_ROOT.bits()
        == bindings::VmCowPagesOptions_kUserPagerBackedRoot
);
::zr::static_assert!(
    VmCowPagesOptions::PAGE_SOURCE_ROOT.bits() == bindings::VmCowPagesOptions_kPageSourceRoot
);
::zr::static_assert!(
    VmCowPagesOptions::CANNOT_DECOMMIT_ZERO_PAGES.bits()
        == bindings::VmCowPagesOptions_kCannotDecommitZeroPages
);
::zr::static_assert!(VmCowPagesOptions::HIDDEN.bits() == bindings::VmCowPagesOptions_kHidden);
::zr::static_assert!(
    VmCowPagesOptions::INTERNAL_ONLY_MASK.bits() == bindings::VmCowPagesOptions_kInternalOnlyMask
);

/// A copy-on-write page hierarchy.
#[repr(C)]
pub struct VmCowPages {
    raw: Opaque<bindings::VmCowPages>,
    phantom: PhantomPinned,
}

impl HasRefCount for VmCowPages {
    fn ref_count(&self) -> &RefCounted {
        let raw = unsafe { bindings::cpp_vm_cow_pages_get_ref_counted(self.as_raw()) };
        unsafe { &*(raw.cast::<RefCounted>()) }
    }
}

impl HasRefCountUpgradeable for VmCowPages {}

unsafe impl Recyclable for VmCowPages {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            bindings::cpp_vm_cow_pages_free(ptr.as_ptr().cast());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}

impl VmCowPages {
    /// Returns the raw dirty state bits of `page`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    unsafe fn page_dirty_state(page: VmPagePtr) -> u8 {
        debug_assert_eq!(page.state(), VmPageState::OBJECT);
        // SAFETY: The caller guarantees `object` is the active union variant and that reading its
        // flags does not race with concurrent writes.
        unsafe { page.get_dirty_state() }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_dirty_tracked(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) != DirtyState::Untracked as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_dirty(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::Dirty as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_clean(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::Clean as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_awaiting_clean(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::AwaitingClean as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state (owned
    /// by a VMO and assigned to `PageQueues`).
    #[inline]
    pub unsafe fn is_page_reclaimable(page: VmPagePtr) -> bool {
        // SAFETY: The caller guarantees `page` is a valid `VmPage` in the `OBJECT` state owned by
        // a VMO and assigned to `PageQueues`.
        unsafe { PageQueues::is_page_reclaimable(page) }
    }

    /// Non-borrowing accessor to the Rust state in `VmCowPages`.
    #[inline]
    fn state(&self) -> &VmCowPagesState {
        // SAFETY: `self.as_raw()` points to a live `VmCowPages`. `cpp_vm_cow_pages_state` returns
        // a non-null, suitably aligned pointer to the `VmCowPagesState` that the C++ constructor
        // initialised via `rust_vm_cow_pages_state_init`. It lives as long as `self`, and all
        // fields mutated after construction (`KCell` and `AtomicBool`) wrap `UnsafeCell`, so
        // creating a shared reference is sound.
        unsafe {
            bindings::cpp_vm_cow_pages_state(self.as_raw())
                .cast::<VmCowPagesState>()
                .as_ref_unchecked()
        }
    }

    #[inline]
    pub(crate) fn options(&self) -> VmCowPagesOptions {
        self.state().options
    }

    #[inline]
    pub(crate) fn pmm_alloc_flags(&self) -> u32 {
        self.state().pmm_alloc_flags
    }

    #[inline]
    pub(crate) fn assert_canary(&self) {
        self.state().canary.assert();
    }

    /// Domain-specific conversion: returns raw pointer for `VmCowPages`.
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::VmCowPages {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr<VmCowPages>` from an exported pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid raw `VmCowPages` pointer exported from C++.
    pub unsafe fn from_raw(ptr: *mut bindings::VmCowPages) -> Option<RefPtr<Self>> {
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Domain-specific conversion: constructs a `&VmCowPages` from a raw pointer.
    ///
    /// Unlike [`VmCowPages::from_raw`] and [`VmCowPages::upgrade_from_raw`], this acquires no
    /// reference at all; the caller is responsible for keeping the object alive for `'a`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a `VmCowPages` that remains live for the
    /// lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::VmCowPages) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::VmCowPages` is layout-compatible with `VmCowPages`, and the caller
        // guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Upgrades a raw `VmCowPages` pointer to a `RefPtr<VmCowPages>`, or returns `None` if the
    /// object is being destroyed.
    ///
    /// # Safety
    ///
    /// `cow` must be a valid pointer to `VmCowPages`.
    pub unsafe fn upgrade_from_raw(cow: *mut bindings::VmCowPages) -> Option<RefPtr<Self>> {
        let ptr = unsafe { bindings::cpp_vm_cow_pages_upgrade_from_raw(cow) };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `cpp_vm_cow_pages_upgrade_from_raw` incremented the refcount,
            // so we can adopt it.
            unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
        }
    }

    /// Returns true if the `VmCowPages` is capable of borrowing pages.
    #[inline]
    pub fn can_borrow(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_can_borrow(self.as_raw()) }
    }

    /// Returns true if the `VmCowPages` can evict pages (i.e. is user-pager backed).
    #[inline]
    pub fn can_evict(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_can_evict(self.as_raw()) }
    }

    /// Returns true if the `VmCowPages` is discardable.
    #[inline]
    pub fn is_discardable(&self) -> bool {
        self.discardable_tracker().is_some()
    }

    /// Returns whether the root of the cow pages hierarchy has non-null `page_source`.
    #[inline]
    pub fn root_has_page_source(&self) -> bool {
        self.options().contains(VmCowPagesOptions::PAGE_SOURCE_ROOT)
    }

    /// If true this node, and all nodes in this hierarchy, are using parent content markers to
    /// indicate when a leaf node may need to walk up the tree to find content.
    ///
    /// When parent content markers are in use an empty page list slot in a leaf node means that
    /// there is *no* visible parent content above, and the parent hierarchy does not have to be
    /// searched.
    ///
    /// For memory efficiency, and because it would be redundant, parent content markers are never
    /// placed in the hidden nodes, only leaf nodes.
    ///
    /// The presence of a parent content marker in a leaf node indicates that there *might* be
    /// content in a parent node and that a tree walk *must* be performed to search for it. The
    /// reason for spurious parent content markers is that zero page deduplication could happen on
    /// hidden nodes, which could remove the content, but leave the parent content markers in the
    /// leaf nodes. Spurious parent content markers can persist in VMOs when they lose their parent.
    /// Spurious parent content markers are redundant and could be cleaned up.
    ///
    /// Use of parent content markers is just the inverse of having a page source, since if there is
    /// a page source we always have to go to it for content as the zero page cannot be assumed.
    /// Although some page sources do supply zero content (physical page provider for contiguous
    /// VMOs), optimizing this check for that is redundant since such page sources do not support
    /// copy-on-write, and so never have children to begin with.
    #[inline]
    pub fn tree_has_parent_content_markers(&self) -> bool {
        !self.root_has_page_source()
    }

    /// Indicates whether this node can have parent content markers placed in it. This is just
    /// checking if it is both a leaf node, and the tree overall can have parent content markers.
    ///
    /// Note that even if this is false, if `tree_has_parent_content_markers` is true then reasoning
    /// may need to be done about parent content markers.
    #[inline]
    pub fn node_has_parent_content_markers(&self) -> bool {
        !self.is_hidden() && self.tree_has_parent_content_markers()
    }

    /// Replaces a page at offset with a loaned page.
    pub fn replace_page_with_loaned(
        &self,
        before_page: VmPagePtr,
        offset: u64,
    ) -> Result<(), Status> {
        let status = unsafe {
            bindings::cpp_vm_cow_pages_replace_page_with_loaned(
                self.as_raw(),
                before_page.as_ffi(),
                offset,
            )
        };
        Status::ok(status)
    }

    /// Unlocked wrapper around `ReplacePageLocked`, exposed for the physical page provider to
    /// cancel loans with.
    ///
    /// If page is still at offset, replace it with a different page. If `with_loaned` is true,
    /// replace with a loaned page. If `with_loaned` is false, replace with a non-loaned page and a
    /// `page_request` is required to be provided.
    ///
    /// The replacement page is returned on success.
    pub fn replace_page(
        &self,
        before_page: VmPagePtr,
        offset: u64,
        with_loaned: bool,
        page_request: Option<&mut AnonymousPageRequest>,
    ) -> Result<VmPagePtr, Status> {
        let mut out_page = ptr::null_mut();
        let page_request_ptr = match page_request {
            Some(page_request) => page_request.as_raw(),
            None => ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, `before_page.as_ffi()` is
        // a valid `vm_page_t` pointer, `&raw mut out_page` is valid for writing a
        // `vm_page_t` pointer, and `page_request_ptr` is either null or points to a live
        // `AnonymousPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_replace_page(
                self.as_raw(),
                before_page.as_ffi(),
                offset,
                with_loaned,
                &raw mut out_page,
                page_request_ptr,
            )
        };

        // SAFETY: `out_page` must be non-null if `status == OK`, in which case the
        // conversion from FFI will succeed.
        Status::ok(status)
            .map(|()| unsafe { VmPagePtr::from_ffi(out_page).expect("page pointer is non-null") })
    }

    /// Returns whether this `VmCowPages`'s lock is held by the current thread.
    ///
    /// Intended for `debug_assert!`s that both document and check a caller's obligation to hold
    /// the VMO lock across a call.
    pub fn lock_is_held(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_lock_is_held(self.as_raw()) }
    }

    /// Returns an appropriate `PmmOptDelayReuse` value for use when freeing pages from this
    /// object.
    #[inline]
    pub fn should_delay_reuse_on_free(&self) -> PmmOptDelayReuse {
        // If this object has ever held a pinned page, we want to delay reuse of its pages to
        // reduce impact of a "bad DMA" bug should one occur.
        if self.ever_pinned() { PmmOptDelayReuse::Yes } else { PmmOptDelayReuse::Default }
    }

    /// Returns the parent `VmCowPages` in the COW hierarchy, if any.
    pub fn debug_get_parent(&self) -> Option<RefPtr<VmCowPages>> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        let raw = unsafe { bindings::cpp_vm_cow_pages_debug_get_parent(self.as_raw()) };
        // SAFETY: cpp_vm_cow_pages_debug_get_parent returns a valid exported `VmCowPages` pointer,
        // or null if there is no parent.
        unsafe { Self::from_raw(raw) }
    }

    /// Deduplicates a committed zero page at `offset`.
    pub fn dedup_zero_page(&self, page: VmPagePtr, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `page.as_raw()` is a
        // valid `vm_page_t` pointer.
        unsafe { bindings::cpp_vm_cow_pages_dedup_zero_page(self.as_raw(), page.as_ffi(), offset) }
    }

    /// Returns the page at `offset`, if present in this node.
    pub fn debug_get_page(&self, offset: u64) -> Option<VmPagePtr> {
        self.assert_canary();
        ksync::lock!(let guard = self.lock());
        self.debug_get_page_locked(guard.token(), offset)
    }

    /// Returns the page at `offset`, if present in this node, with the lock held.
    pub fn debug_get_page_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
    ) -> Option<VmPagePtr> {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        let p = self.page_list_locked(token).lookup(offset)?;
        if p.is_page() { Some(p.page()) } else { None }
    }

    /// Returns the number of pages or references in this node's page list.
    pub fn debug_get_page_count_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u64 {
        self.assert_canary();
        let mut page_count: u64 = 0;
        let status = self.page_list_locked(token).for_every_page(|p, _offset| {
            if !p.is_page_or_ref() {
                return Status::NEXT;
            }
            page_count += 1;
            Status::NEXT
        });
        // We never stop early in closure above.
        debug_assert!(status.is_ok());
        page_count
    }

    /// Returns whether the slot at `offset` holds a parent content marker.
    pub fn debug_is_parent_content(&self, offset: u64) -> bool {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        ksync::lock!(let guard = self.lock());
        self.page_list_locked(guard.token()).lookup(offset).is_some_and(|p| p.is_parent_content())
    }

    /// Returns whether the slot at `offset` holds a zero marker.
    pub fn debug_is_marker(&self, offset: u64) -> bool {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        ksync::lock!(let guard = self.lock());
        self.page_list_locked(guard.token()).lookup(offset).is_some_and(|p| p.is_marker())
    }

    /// Returns the share count of the zero marker at `offset`.
    pub fn debug_get_marker_share_count(&self, offset: u64) -> u32 {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        ksync::lock!(let guard = self.lock());
        let p = self.page_list_locked(guard.token()).lookup(offset);
        debug_assert!(p.is_some_and(|p| p.is_marker()));
        p.expect("slot at offset holds a zero marker").marker_share_count()
    }

    /// Returns whether this node has no page at `offset`.
    pub fn debug_is_empty(&self, offset: u64) -> bool {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        ksync::lock!(let guard = self.lock());
        self.page_list_locked(guard.token()).lookup(offset).is_none_or(|p| p.is_empty())
    }

    pub fn debug_is_page(&self, offset: u64) -> bool {
        self.assert_canary();
        debug_assert!(::page::is_page_rounded(offset as usize));
        ksync::lock!(let guard = self.lock());
        self.page_list_locked(guard.token()).lookup(offset).is_some_and(|p| p.is_page())
    }

    /// Returns whether this VmCowPages is marked high memory priority.
    pub fn debug_is_high_memory_priority(&self) -> bool {
        self.assert_canary();
        ksync::lock!(let guard = self.lock());
        self.is_high_memory_priority_locked(guard.token())
    }

    /// Returns the debug discardable tracker, if present.
    pub fn debug_get_discardable_tracker(&self) -> Option<&DiscardableVmoTracker> {
        self.discardable_tracker()
    }

    /// Evict a specific loaned page for the use case of reclaiming loaned pages by the physical
    /// page provider. Unlike ReclaimPage this function can assume it just needs to evict, and
    /// has no requirements on updating any reclamation lists.
    ///
    /// # Safety
    ///
    /// `page` must be an object-associated page.
    pub unsafe fn evict_loaned_page(&self, page: VmPagePtr, offset: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `page.as_raw()` is a
        // valid `vm_page_t` pointer.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_evict_loaned_page(self.as_raw(), page.as_ffi(), offset)
        };
        Status::ok(status)
    }

    /// Asks the VMO to attempt to reclaim the specified page. There are a few possible outcomes:
    /// 1. Exactly this page is reclaimed.
    /// 2. This page and other pages are reclaimed.
    /// 3. Just other pages are reclaimed.
    /// 4. No pages are reclaimed.
    ///
    /// Pages other than the one requested may get reclaimed due to any internal relationships
    /// between pages that make it meaningless or difficult to reclaim just the single page in
    /// question. In the cases of (3) and (4) there are some guarantees provided:
    /// 1. If the `page` was not from this VMO (or not at the specified offset) then nothing about
    ///    the `page` or this VMO will be modified.
    /// 2. If the `page` is from this VMO and offset (and was not reclaimed) then the page will have
    ///    been removed from any candidate reclamation lists (such as the DontNeed pager backed
    ///    list).
    ///
    /// The effect of (2) is that the caller can assume in the case of reclamation failure it will
    /// not keep finding this page as a reclamation candidate and infinitely retry it.
    /// `eviction_action` indicates whether the `always_need` eviction hint should be respected or
    /// ignored. Require will force eviction.
    ///
    /// The actual number of pages reclaimed is returned if successful, or a failure reason if not.
    ///
    /// # Safety
    ///
    /// The caller must know that it is sound to reclaim `page` at `offset`.
    pub unsafe fn reclaim_page(
        &self,
        page: VmPagePtr,
        offset: u64,
        eviction_action: EvictionAction,
        compressor: Option<&VmCompressor>,
    ) -> Result<VmCowReclaimSuccess, VmCowReclaimFailure> {
        let compressor_ptr = match compressor {
            Some(c) => c.as_raw(),
            None => ptr::null_mut(),
        };
        let mut success = MaybeUninit::<bindings::VmCowReclaimSuccess>::zeroed();
        let mut failure = MaybeUninit::<bindings::VmCowReclaimFailure>::uninit();
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, `page.as_raw()` is a
        // valid `vm_page_t` pointer, and out-pointers are valid for writes.
        let ok = unsafe {
            bindings::cpp_vm_cow_pages_reclaim_page(
                self.as_raw(),
                page.as_ffi(),
                offset,
                eviction_action.into(),
                compressor_ptr,
                success.as_mut_ptr(),
                failure.as_mut_ptr(),
            )
        };
        if ok {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `success` when returning true.
            Ok(unsafe { success.assume_init() }.into())
        } else {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `failure` when returning false.
            Err(unsafe { failure.assume_init() }.into())
        }
    }

    /// Similar to `LookupLocked`, but enumerates all readable pages in the hierarchy within the
    /// requested range. The offset passed to `lookup_fn` is the offset this page is visible at
    /// in this object, even if the page itself is committed in a parent object. The physical
    /// addresses given to `lookup_fn` should not be retained in any way unless the range has
    /// also been pinned by the caller.
    ///
    /// Ranges of length zero are considered invalid and will return `Err(Status::INVALID_ARGS)`.
    /// Returning `Status::NEXT` from `lookup_fn` continues enumeration, `Status::STOP` stops
    /// iteration early and returns `Ok(())`, and any other `Status` aborts early with
    /// `Err(status)`.
    ///
    /// # Warning
    ///
    /// This function only exists for test code. There is no way non-test code can use this function
    /// in a correct way due to its internal locking.
    pub fn debug_lookup_readable<T: Sized>(
        &self,
        range: VmCowRange,
        ctx: &mut T,
        lookup_fn: fn(u64, PAddr, &mut T) -> Status,
    ) -> Result<(), Status> {
        struct LookupState<'a, T> {
            ctx: &'a mut T,
            lookup_fn: fn(u64, PAddr, &mut T) -> Status,
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `LookupState<'_, T>` created on the stack in
        /// `debug_lookup_readable` that remains valid for the duration of the C++ FFI lookup
        /// callback.
        unsafe extern "C" fn lookup_callback_shim<T>(
            ctx: *mut core::ffi::c_void,
            offset: u64,
            paddr: u64,
        ) -> zx_status_t {
            // SAFETY: `ctx` is guaranteed by `cpp_vm_cow_pages_debug_lookup_readable` to be the
            // non-null `ctx_ptr` passed from `debug_lookup_readable`, which points to a live
            // `LookupState<'_, T>` on the caller's stack.
            let state = unsafe { ctx.cast::<LookupState<'_, T>>().as_mut_unchecked() };
            (state.lookup_fn)(offset, paddr.into(), state.ctx).into_raw()
        }

        let mut state = LookupState { ctx, lookup_fn };
        let state_ptr: *mut LookupState<'_, T> = &mut state;
        // Erase the Rust type so we can pass our context pointer through C++'s void* argument.
        let ctx_ptr: *mut core::ffi::c_void = state_ptr.cast();
        // SAFETY: `self.as_raw()` is a live `VmCowPages`, and `ctx_ptr` points to `state` on the
        // stack for the duration of this synchronous call.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_debug_lookup_readable(
                self.as_raw(),
                range.into(),
                ctx_ptr,
                Some(lookup_callback_shim::<T>),
            )
        };
        Status::ok(status)
    }

    /// Acquires the lock via pin-init for use with `ksync::lock!`.
    #[inline]
    pub fn lock(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, VmCowPagesLockClass, TrackedCriticalMutex>, Infallible> {
        // SAFETY: `self.as_raw()` points to a live VmCowPages.
        let lock = unsafe { bindings::cpp_vm_cow_pages_get_lock(self.as_raw()) };
        let lock: *mut VmCowPagesLock = lock.cast();
        // SAFETY: `lock` is valid for reads for the lifetime of `self`.
        let lock = unsafe { lock.as_ref_unchecked() };
        lock.lock()
    }

    /// Adds a set of pages consecutively starting from the given offset. Regardless of the
    /// return result ownership of the pages is taken. Pages are assumed to be in the ALLOC state
    /// and can be optionally zeroed before inserting. `start_offset` must be page aligned.
    ///
    /// `overwrite` controls how the function handles pre-existing content in the range, however
    /// it is not valid to specify the `CanOverwriteSlot::PageOrRef` option, as any pages or
    /// compressed references that would get released as a consequence cannot be returned.
    pub fn add_new_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        list: Pin<&mut VmPageDoublyLinkedList>,
        overwrite: CanOverwriteSlot,
        zero: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
    ) -> Result<(), Status> {
        // SAFETY: We do not move `list`.
        let list: &mut VmPageDoublyLinkedList = unsafe { list.get_unchecked_mut() };
        let list: *mut VmPageDoublyLinkedList = list;
        let list: *mut bindings::VmPageDoublyLinkedList = list.cast();
        let deferred = deferred.as_raw();
        // SAFETY: `self.as_raw()` is a live VmCowPages.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_add_new_pages_locked(
                self.as_raw(),
                start_offset,
                list,
                overwrite.into(),
                zero,
                deferred,
            )
        };
        Status::ok(status)
    }

    /// After successful completion the range of pages will all read as zeros. The mechanism used
    /// to achieve this is not guaranteed to decommit, but it will try to.
    ///
    /// `range` must be page aligned offsets within the range of the object. `dirty_track`
    /// specifies whether the range being zeroed subscribes to dirty tracking, if `true` the range
    /// will start out as dirty. `dirty_track` only has meaning if the VMO supports dirty tracking,
    /// otherwise it is ignored.
    ///
    /// Returns a status, and the number of bytes that were actually zeroed. The byte count may be
    /// nonzero even if the status is an error.
    ///
    /// The returned status is one of the following:
    ///  * `Ok` => The whole range was successfully zeroed.
    ///  * `Err(Status::SHOULD_WAIT)` => The caller needs to wait on the `page_request` and then
    ///    retry the operation. The caller may advance the start offset by the number of zeroed
    ///    bytes before retrying.
    ///  * Any other error indicates a failure to zero a part of the range or the whole range.
    ///
    /// The caller must hold *this* object's lock. `token` proves only that some lock of the
    /// `VmCowPagesLockClass` class is held, and the C++ side runs with thread safety analysis
    /// disabled, so neither side can check this.
    pub fn zero_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        dirty_track: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        let deferred = deferred.as_raw();
        let page_request = page_request.as_raw();
        let mut zeroed_bytes = 0;
        // SAFETY: `self.as_raw()` is a live VmCowPages, `deferred` and `page_request` point to the
        // live pinned objects borrowed above, and `zeroed_bytes` is valid for writing.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_zero_pages_locked(
                self.as_raw(),
                range.into(),
                dirty_track,
                deferred,
                page_request,
                &mut zeroed_bytes,
            )
        };
        (Status::ok(status), zeroed_bytes)
    }

    /// See `VmObject::supply_pages`.
    ///
    /// Should never return `Err(Status::SHOULD_WAIT)`, waiting on page requests is managed by
    /// `ProcessPagesForSupply`.
    pub fn supply_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        pages: Pin<&mut VmPageSpliceList>,
        options: SupplyOptions,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Option<Pin<&mut MultiPageRequest>>,
    ) -> Result<(), Status> {
        let deferred = deferred.as_raw();
        let page_request_ptr = match page_request {
            Some(page_request) => page_request.as_raw(),
            None => ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` is a live VmCowPages, `pages.as_raw()` points to a live
        // `VmPageSpliceList`, `deferred` points to a live `DeferredOps` and `page_request_ptr` is
        // either null or points to a live `MultiPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_supply_pages_locked(
                self.as_raw(),
                range.into(),
                pages.as_raw(),
                options,
                deferred,
                page_request_ptr,
            )
        };
        Status::ok(status)
    }

    /// Applies the specific operation to all mappings in the given range. The mappings for the
    /// current object are operated on immediately, with any children being operated on using
    /// `deferred`. If the caller knows that no `DeferredOps` is needed (e.g. the VMO has no
    /// children and is not pager backed) then `None` can be provided.
    pub fn range_change_update_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        mut range: VmCowRange,
        op: RangeChangeOp,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) {
        self.assert_canary();
        // If we have children (or this is a pager backed hierarchy) then potentially need to
        // perform deferred operations.
        if self.children_list_len_locked(token) != 0 || self.root_has_page_source() {
            if let Some(deferred) = deferred {
                deferred.add_range(self, range, op);
            } else {
                // If the operation was RemoveWrite then, since children are copy-on-write and
                // cannot have writable mappings, they do not require a deferred operation. This is
                // still true for pager hierarchies as, since no content is actually changing, there
                // is no need for serialization.
                debug_assert_eq!(op, RangeChangeOp::RemoveWrite);
            }
        }
        if !range.is_empty()
            && let Some(paged_ref) = self.get_paged_backlink_locked(token)
        {
            range = range.expand_till_page_aligned();
            self.range_change_update_mappings_locked(token, paged_ref, range, op);
        }
    }

    /// Apply the specified operation to all mappings of `paged` in the given range.
    ///
    /// `paged` is assumed to be one of the aliases for this `VmCowPages` node. `range` must be
    /// page-aligned.
    ///
    /// Differs from `range_change_update_locked` in that this method does not apply the
    /// operation to copy-on-write children.
    pub fn range_change_update_mappings_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        paged: &VmObjectPaged,
        range: VmCowRange,
        op: RangeChangeOp,
    ) {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        debug_assert!(paged.lock_is_held());
        debug_assert!(range.is_page_aligned());

        if self.pinned_page_count_locked(token) == 0 {
            paged.range_change_update_locked(range, op, token);
        } else {
            core::hint::cold_path();
            // The operation is permitted for any page unless it is mapped within the kernel address
            // space without the VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING flag. Since being mapped in
            // the kernel requires a page to be pinned, we can avoid illegal operations by simply
            // skipping all pinned pages.
            let mut maybe_unpinned_start = range.offset;
            let status = self.page_list_locked(token).for_every_page_in_range(
                range.offset,
                range.end(),
                |slot, offset| {
                    // SAFETY: Pages in `self`'s page list are in the `OBJECT` state (active union
                    // variant), and `pin_count` is only written under `self`'s lock, which `token`
                    // proves is held.
                    if !slot.is_page() || unsafe { slot.page().get_pin_count() } == 0 {
                        return Status::NEXT; // we are looking for pinned pages
                    }
                    // `offset` is never below `maybe_unpinned_start`, which only ever trails the
                    // iteration offset.
                    let unpinned_portion_len = offset - maybe_unpinned_start;
                    if unpinned_portion_len > 0 {
                        paged.range_change_update_locked(
                            VmCowRange::new(maybe_unpinned_start, unpinned_portion_len),
                            op,
                            token,
                        );
                    }
                    maybe_unpinned_start = offset + (::page::SIZE as u64);
                    Status::NEXT
                },
            );
            debug_assert!(status.is_ok());

            // `maybe_unpinned_start` is at most `range.end()`.
            let unpinned_portion_len = range.end() - maybe_unpinned_start;
            if unpinned_portion_len > 0 {
                paged.range_change_update_locked(
                    VmCowRange::new(maybe_unpinned_start, unpinned_portion_len),
                    op,
                    token,
                );
            }
        }
    }

    /// Test-only interface to get the current populated slots count.
    ///
    /// This method panics if the kernel is built without support for the functional continuous
    /// attribution tracker (EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED).
    pub fn debug_get_populated_slots_count(&self) -> u32 {
        ksync::lock!(let guard = self.lock());
        self.continuous_attribution_tracker_locked(guard.token()).fetch_current()
    }

    /// Returns the number of locked and unlocked pages in this discardable VMO (zero counts if
    /// this VMO is not discardable, or has not opted into locking / unlocking yet).
    ///
    /// See `DiscardableVmoTracker::debug_discardable_page_counts()`.
    pub fn debug_get_discardable_page_counts(&self) -> DiscardablePageCounts {
        self.assert_canary();
        let mut counts = DiscardablePageCounts::default();

        // Not a discardable VMO.
        let Some(tracker) = self.discardable_tracker() else {
            return counts;
        };

        ksync::lock!(let guard = self.lock());
        let token = guard.token();

        // SAFETY: `tracker` belongs to `self` and `self`'s lock is held via `token`.
        let state = unsafe { tracker.discardable_state_locked(token) };
        // This is a discardable VMO but hasn't opted into locking / unlocking yet.
        if state == DiscardableState::Unset {
            return counts;
        }

        let mut pages: u64 = 0;
        let _ = self.page_list_locked(token).for_every_page(|p, _| {
            // TODO(https://fxbug.dev/42138396) Figure out attribution between pages and references.
            if p.is_page_or_ref() {
                pages += 1;
            }
            Status::NEXT
        });

        match state {
            DiscardableState::Reclaimable => {
                counts.unlocked = pages;
            }
            DiscardableState::Unreclaimable => {
                counts.locked = pages;
            }
            DiscardableState::Discarded => {
                debug_assert_eq!(pages, 0);
            }
            DiscardableState::Unset => {}
        }

        counts
    }

    /// Dumps all the reclamation counters to the debuglog.
    pub fn debug_dump_reclaim_counters() {
        // SAFETY: `cpp_vm_cow_pages_debug_dump_reclaim_counters` has no preconditions.
        unsafe {
            bindings::cpp_vm_cow_pages_debug_dump_reclaim_counters();
        }
    }

    #[inline]
    fn page_source_type(&self) -> PageSourceType {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_page_source_type(self.as_raw()) }.into()
    }

    #[inline]
    fn discardable_tracker(&self) -> Option<&DiscardableVmoTracker> {
        // SAFETY: `self.as_raw()` is a valid `VmCowPages` pointer, and `discardable_tracker_` is
        // const for the lifetime of `VmCowPages`.
        let raw = unsafe { bindings::cpp_vm_cow_pages_discardable_tracker(self.as_raw()) };
        if raw.is_null() {
            None
        } else {
            // SAFETY: `raw` points to a live `DiscardableVmoTracker` owned by `self`.
            Some(unsafe { DiscardableVmoTracker::from_raw_ref(raw) })
        }
    }

    /// The page source, if any.
    #[inline]
    fn page_source(&self) -> Option<&PageSource> {
        // SAFETY: `self.as_raw()` is a valid `VmCowPages` pointer, and `page_source_` is const
        // for the lifetime of `VmCowPages`.
        let ptr = unsafe { bindings::cpp_vm_cow_pages_page_source(self.as_raw()) };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `ptr` points to a live `PageSource` owned by `self.page_source_`, which is a
            // `const fbl::RefPtr<PageSource>` member, so the pointee outlives `&self`.
            Some(unsafe { PageSource::from_raw_ref(ptr.cast_const()) })
        }
    }

    #[inline]
    fn get_paged_backlink_locked<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmCowPagesLockClass>,
    ) -> Option<&'a VmObjectPaged> {
        // SAFETY: `self.as_raw()` is a valid `VmCowPages` pointer and the lock is held.
        let ptr = unsafe { bindings::cpp_vm_cow_pages_get_paged_backlink_locked(self.as_raw()) };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `ptr` is `paged_ref_`, a `VmObjectPaged*` that `vm_cow_pages_bindings` types
            // as `c_void` only to avoid a bindgen dependency cycle, so the cast is to its real
            // type. The backlink is live for as long as the lock is held, i.e. for `_token`.
            Some(unsafe {
                VmObjectPaged::from_raw_ref(ptr.cast::<vm_object_paged_bindings::VmObjectPaged>())
            })
        }
    }

    /// Query pager VMO stats, and reset them too if `reset` is set to true.
    pub fn query_pager_vmo_stats_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        reset: bool,
    ) -> Result<zx_pager_vmo_stats_t, Status> {
        self.assert_canary();
        // The modified state should only be set for VMOs directly backed by a pager.
        debug_assert!(
            !self.pager_stats_modified_locked(token)
                || self.page_source_type() == PageSourceType::UserPager
        );

        if self.page_source_type() != PageSourceType::UserPager {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        let stats = zx_pager_vmo_stats_t {
            modified: if self.pager_stats_modified_locked(token) {
                ZX_PAGER_VMO_STATS_MODIFIED
            } else {
                0
            },
        };
        if reset {
            self.reset_pager_vmo_stats_locked(token);
        }
        Ok(stats)
    }

    /// Moves an existing page to the wired queue as a consequence of the page being pinned.
    ///
    /// # Safety
    ///
    /// `page` must be a valid `VmPage` in the `OBJECT` state already in a page queue.
    unsafe fn move_to_pinned_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        _offset: u64,
    ) {
        // SAFETY: Caller guarantees `page` is valid and already in a page queue.
        unsafe {
            pmm_page_queues().move_to_wired(page);
        }
    }

    /// Updates the page queue of an existing non-pinned page, moving it to whichever queue is
    /// appropriate.
    ///
    /// # Safety
    ///
    /// `page` must be a valid `VmPage` in the `OBJECT` state owned by `self` and already in a page
    /// queue.
    unsafe fn move_to_not_pinned_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        _offset: u64,
    ) {
        let pq = pmm_page_queues();
        if self.page_source_type() == PageSourceType::UserPager {
            // SAFETY: `page` is in the `OBJECT` state (active `object` union variant) and owned by
            // `self`, whose lock, proven held by `token`, serialises all writers of `flags`.
            debug_assert!(unsafe { Self::is_page_dirty_tracked(page) });
            // SAFETY: As above.
            let clean = unsafe { Self::is_page_clean(page) };
            // We can only move Clean pages to the pager backed queues as they track age
            // information for eviction; only Clean pages can be evicted. Pages in AwaitingClean
            // and Dirty are protected from eviction in the Dirty queue.
            if clean {
                if self.high_priority_count_locked(token) != 0 {
                    core::hint::cold_path();
                    // If this VMO is high priority then do not place in the pager backed queue
                    // as that is reclaimable, place in the high priority queue instead.
                    // SAFETY: `page` is owned by `self` and already in a page queue.
                    unsafe {
                        pq.move_to_high_priority(page);
                    }
                } else {
                    // SAFETY: `page` is owned by `self` and already in a page queue.
                    unsafe {
                        pq.move_to_reclaim(page);
                    }
                }
            } else {
                // SAFETY: `page` is owned by `self` (so its loaned state cannot change under us)
                // and already in a page queue.
                unsafe {
                    debug_assert!(!page.is_loaned());
                    pq.move_to_pager_backed_dirty(page);
                }
            }
        } else {
            // Place pages from contiguous VMOs in the wired queue, as they are notionally pinned
            // until the owner explicitly releases them.
            if self.can_decommit_zero_pages() {
                if self.high_priority_count_locked(token) != 0 && !pq.reclaim_is_only_pager_backed()
                {
                    core::hint::cold_path();
                    // If anonymous pages are reclaimable, and this VMO is high priority, then
                    // places our pages in the high priority queue instead of the anonymous one to
                    // avoid reclamation.
                    // SAFETY: `page` is owned by `self` and already in a page queue.
                    unsafe {
                        pq.move_to_high_priority(page);
                    }
                } else {
                    let mut cannot_reclaim = false;
                    // If this is a discardable VMO but not currently unlocked, it cannot be
                    // reclaimed. The reclamation code is tolerant to this, but avoid wasted work.
                    if let Some(tracker) = self.discardable_tracker() {
                        core::hint::cold_path();
                        // SAFETY: `tracker` belongs to `self` and `self`'s lock is held.
                        cannot_reclaim =
                            !unsafe { tracker.is_eligible_for_reclamation_locked(token) };
                    }
                    // If the VMO is mapped uncached, it cannot be reclaimed. The reclamation code
                    // is tolerant to this and will skip the page anyway, but uncached memory is
                    // typically used by drivers and tends to back large buffers, so avoid wasted
                    // work.
                    if !cannot_reclaim && let Some(paged) = self.get_paged_backlink_locked(token) {
                        cannot_reclaim = (paged.get_mapping_cache_policy_locked(token) as u32
                            & ZX_CACHE_POLICY_MASK)
                            != ZX_CACHE_POLICY_CACHED;
                    }
                    // SAFETY: `page` is owned by `self` and already in a page queue.
                    unsafe {
                        pq.move_to_anonymous(page, /*skip_reclaim=*/ cannot_reclaim);
                    }
                }
            } else {
                // SAFETY: `page` is owned by `self` and already in a page queue.
                unsafe {
                    pq.move_to_wired(page);
                }
            }
        }
    }

    /// Places a newly added, not yet pinned, page into the appropriate page queue.
    ///
    /// # Safety
    ///
    /// `page` must be a valid `VmPage` in the `OBJECT` state owned by `self` and not currently in
    /// any page queue.
    unsafe fn set_not_pinned_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        offset: u64,
    ) {
        let pq = pmm_page_queues();
        if self.page_source_type() == PageSourceType::UserPager {
            // SAFETY: `page` is in the `OBJECT` state (active `object` union variant) and owned by
            // `self`, whose lock, proven held by `token`, serialises all writers of `flags`.
            debug_assert!(unsafe { Self::is_page_dirty_tracked(page) });
            // SAFETY: As above.
            let clean = unsafe { Self::is_page_clean(page) };
            // We can only move Clean pages to the pager backed queues as they track age
            // information for eviction; only Clean pages can be evicted. Pages in AwaitingClean
            // and Dirty are protected from eviction in the Dirty queue.
            if clean {
                if self.high_priority_count_locked(token) != 0 {
                    core::hint::cold_path();
                    // If this VMO is high priority then do not place in the pager backed queue
                    // as that is reclaimable, place in the high priority queue instead.
                    // SAFETY: `page` is owned by `self` and not yet in any page queue.
                    unsafe {
                        pq.set_high_priority(page, self, offset);
                    }
                } else {
                    // SAFETY: `page` is owned by `self` and not yet in any page queue.
                    unsafe {
                        pq.set_reclaim(page, self, offset);
                    }
                }
            } else {
                // SAFETY: `page` is owned by `self` (so its loaned state cannot change under us)
                // and not yet in any page queue.
                unsafe {
                    debug_assert!(!page.is_loaned());
                    pq.set_pager_backed_dirty(page, self, offset);
                }
            }
        } else {
            debug_assert!(
                self.page_source_type() == PageSourceType::Anonymous
                    || self.page_source_type() == PageSourceType::Contiguous
            );
            // Place pages from contiguous VMOs in the wired queue, as they are notionally pinned
            // until the owner explicitly releases them.
            if self.can_decommit_zero_pages() {
                if self.high_priority_count_locked(token) != 0 && !pq.reclaim_is_only_pager_backed()
                {
                    core::hint::cold_path();
                    // If anonymous pages are reclaimable, and this VMO is high priority, then
                    // places our pages in the high priority queue instead of the anonymous one to
                    // avoid reclamation.
                    // SAFETY: `page` is owned by `self` and not yet in any page queue.
                    unsafe {
                        pq.set_high_priority(page, self, offset);
                    }
                } else {
                    let mut cannot_reclaim = false;
                    // If this is a discardable VMO but not currently unlocked, it cannot be
                    // reclaimed. The reclamation code is tolerant to this, but avoid wasted work.
                    if let Some(tracker) = self.discardable_tracker() {
                        core::hint::cold_path();
                        // SAFETY: `tracker` belongs to `self` and `self`'s lock is held.
                        cannot_reclaim =
                            !unsafe { tracker.is_eligible_for_reclamation_locked(token) };
                    }
                    // If the VMO is mapped uncached, it cannot be reclaimed. The reclamation code
                    // is tolerant to this and will skip the page anyway, but uncached memory is
                    // typically used by drivers and tends to back large buffers, so avoid wasted
                    // work.
                    if !cannot_reclaim && let Some(paged) = self.get_paged_backlink_locked(token) {
                        cannot_reclaim = (paged.get_mapping_cache_policy_locked(token) as u32
                            & ZX_CACHE_POLICY_MASK)
                            != ZX_CACHE_POLICY_CACHED;
                    }
                    // SAFETY: `page` is owned by `self` and not yet in any page queue.
                    unsafe {
                        pq.set_anonymous(page, self, offset, /*skip_reclaim=*/ cannot_reclaim);
                    }
                }
            } else {
                // SAFETY: `page` is owned by `self` and not yet in any page queue.
                unsafe {
                    pq.set_wired(page, self, offset);
                }
            }
        }
    }

    /// Updates the page's dirty state to the one specified, and also moves the page between page
    /// queues if required by the dirty state. `dirty_state` should be a valid dirty tracking
    /// state, i.e. one of `Clean`, `AwaitingClean`, or `Dirty`.
    ///
    /// `offset` is the page-aligned offset of the page in this object.
    ///
    /// `is_pending_add` indicates whether this page is yet to be added to this object's page list.
    /// If the page is yet to be added, this function will skip updating the page queue as an
    /// optimization, since the page queue will be updated later when the page gets added to the
    /// page list. `is_pending_add` also helps determine certain validation checks that can be
    /// performed on the page.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not backed by a user pager, if `dirty_state` is not one of `Clean`,
    /// `AwaitingClean`, or `Dirty`, or if the requested transition is not valid for the page's
    /// current dirty state (see the per-state assertions below).
    ///
    /// # Safety
    ///
    /// `page` must be a valid `VmPage` in the `OBJECT` state that is either owned by `self` and
    /// already in `self`'s page list (`is_pending_add == false`), or exclusively owned by the
    /// caller and about to be added to `self` (`is_pending_add == true`).
    unsafe fn update_dirty_state_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        offset: u64,
        dirty_state: DirtyState,
        is_pending_add: bool,
    ) {
        assert!(self.page_source_type() == PageSourceType::UserPager);

        // SAFETY: Per this function's `# Safety`, `page` is in the `OBJECT` state (its `object`
        // union variant is active). When `!is_pending_add` the page is in `self`'s page list, so
        // every `PageQueues` update of `object_priv` / `page_offset_priv` and every writer of the
        // packed `flags` byte is initiated under `self`'s lock, proven held by `token`; when
        // `is_pending_add` the caller has exclusive ownership of the not-yet-inserted page. Either
        // way none of the reads below can race. Each `is_page_*` reads the `dirty_state` bits.
        let (is_dirty, is_clean, is_awaiting_clean) = unsafe {
            (
                Self::is_page_dirty(page),
                Self::is_page_clean(page),
                Self::is_page_awaiting_clean(page),
            )
        };

        // If the page is not pending being added to the page list, it should have valid object
        // info.
        // SAFETY: As above; these read `object_priv` and `page_offset_priv`.
        debug_assert!(
            is_pending_add
                || unsafe { page.get_object() } == self.as_raw().cast::<core::ffi::c_void>()
        );
        debug_assert!(is_pending_add || unsafe { page.get_page_offset() } == offset);

        // If the page is Dirty or AwaitingClean, it should not be loaned.
        // SAFETY: `page` is owned by `self` (or exclusively by the caller when `is_pending_add`),
        // so its loaned state cannot change under us.
        debug_assert!(!(is_dirty || is_awaiting_clean) || !unsafe { page.is_loaned() });

        // Perform state-specific checks. We will finally update the state below.
        let mut update_page_queues = false;
        match dirty_state {
            DirtyState::Clean => {
                // If the page is not in the process of being added, we can only see a transition
                // to Clean from AwaitingClean.
                assert!(is_pending_add || is_awaiting_clean);

                // If we are expecting a pending `add_[new_]page_locked`, we can defer updating the
                // page queue.
                if !is_pending_add {
                    update_page_queues = true;
                }
            }
            DirtyState::Dirty => {
                // If the page is not in the process of being added, we can only see a transition
                // to Dirty from Clean or AwaitingClean.
                assert!(is_pending_add || is_clean || is_awaiting_clean);

                // A loaned page cannot be marked Dirty as loaned pages are reclaimed by eviction;
                // Dirty pages cannot be evicted.
                // SAFETY: As above.
                debug_assert!(!unsafe { page.is_loaned() });

                // If we are expecting a pending `add_[new_]page_locked`, we can defer updating the
                // page queue.
                if !is_pending_add {
                    update_page_queues = true;
                }
            }
            DirtyState::AwaitingClean => {
                // A newly added page cannot start off as AwaitingClean.
                assert!(!is_pending_add);
                // A pinned page will be kept Dirty as long as it is pinned.
                //
                // Note that there isn't a similar constraint when setting the Clean state as it is
                // possible to pin a page for read after it has been marked AwaitingClean. Since it
                // is a pinned read it does not need to dirty the page. So when the writeback is
                // done it can transition from AwaitingClean -> Clean with a non-zero pin count.
                //
                // It is also possible for us to observe an intermediate pin count for a write-pin
                // that has not fully completed yet, as we will only attempt to dirty pages after
                // pinning them. So it is possible for a thread to be waiting on a DIRTY request on
                // a pinned page, while a racing writeback transitions the page from
                // AwaitingClean -> Clean with a non-zero pin count.
                // SAFETY: As above; `pin_count` lives in the packed `flags` byte.
                assert!(unsafe { page.get_pin_count() } == 0);
                // We can only transition to AwaitingClean from Dirty.
                assert!(is_dirty);
                // A loaned page cannot be marked AwaitingClean as loaned pages are reclaimed by
                // eviction; AwaitingClean pages cannot be evicted.
                // SAFETY: As above.
                debug_assert!(!unsafe { page.is_loaned() });
                // No page queue update. Leave the page in the Dirty queue for now as it is not
                // clean yet; it will be moved out on WritebackEnd.
                // SAFETY: `page` is in the `OBJECT` state and, being `!is_pending_add`, already
                // assigned to a page queue; the read of `page_queue_priv` is atomic.
                debug_assert!(unsafe { pmm_page_queues().debug_page_is_pager_backed_dirty(page) });
            }
            DirtyState::Untracked | DirtyState::NumStates => {
                panic!("Invalid dirty state");
            }
        }

        // SAFETY: `page` is in the `OBJECT` state and, as established above, no other writer of
        // the packed `flags` byte can run concurrently (owning lock held, or exclusive ownership
        // of a not-yet-inserted page).
        unsafe { page.set_dirty_state(dirty_state as u8) };
        // SAFETY: As above; `update_page_queues` implies `!is_pending_add`.
        if update_page_queues && unsafe { page.get_pin_count() } == 0 {
            // Move the page to the appropriate page queue, checking for global state such as
            // high priority count etc.
            //
            // If Clean:
            // Move to evictable pager backed queue to start tracking age information.
            //
            // If Dirty:
            // Move the page to the Dirty queue, which does not track page age. While the page
            // is in the Dirty queue, age information is not required (yet). It will be required
            // when the page becomes Clean (and hence evictable) again, at which point it will
            // get moved to the MRU pager backed queue and will age as normal.
            // TODO(rashaeqbal): We might want age tracking for the Dirty queue in the future
            // when the kernel generates writeback pager requests.
            // SAFETY: `update_page_queues` implies `!is_pending_add`, i.e. `page` is already in
            // `self`'s page list and therefore assigned to a page queue, as
            // `move_to_not_pinned_locked` requires.
            unsafe { self.move_to_not_pinned_locked(token, page, offset) };
        }
    }

    /// The `VmObjectPaged` is changing its cache mapping policy. Clean / invalidate all existing
    /// pages and update page queues if required.
    pub fn finish_cache_policy_transition_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        // No need to perform clean/invalidate if size is zero because there can be no pages.
        if self.size_locked(token) == 0 {
            return;
        }

        let _ = self.page_list_locked(token).for_every_page(move |p, off| {
            if !p.is_page() {
                return Status::NEXT;
            }
            let page = p.page();
            // SAFETY: `page` is in `self`'s page list in the `OBJECT` state while `self`'s lock is
            // held, and `vaddr` is its valid physmap virtual address of length `::page::SIZE`.
            unsafe {
                debug_assert_eq!(page.get_pin_count(), 0);
                // Refreshing the page queue will move the page to an unreclaimable one if
                // applicable.
                self.move_to_not_pinned_locked(token, page, off);
                let paddr = page.paddr();
                let vaddr = paddr_to_physmap(paddr);
                arch_clean_invalidate_cache_range(vaddr.0, ::page::SIZE);
            }
            Status::NEXT
        });
    }

    /// See `VmObject::debug_is_range_pinned`.
    pub fn debug_is_range_pinned_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> bool {
        self.assert_canary();
        debug_assert!(range.is_page_aligned());

        let mut pinned_count: u64 = 0;
        let _ = self.page_list_locked(token).for_every_page_in_range(
            range.offset,
            range.end(),
            |p, _off| {
                // SAFETY: `p.page()` is in `self`'s page list and `self`'s lock is held.
                if p.is_page() && unsafe { p.page().get_pin_count() } > 0 {
                    pinned_count += 1;
                    return Status::NEXT;
                }
                Status::STOP
            },
        );
        pinned_count == range.len / (::page::SIZE as u64)
    }

    /// Internal check if any pages in a range are pinned.
    fn any_pages_pinned_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        len: usize,
    ) -> bool {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        debug_assert!(::page::is_page_rounded(offset as usize));
        debug_assert!(::page::is_page_rounded(len));

        let start_page_offset = offset;
        let end_page_offset = offset + (len as u64);

        if self.pinned_page_count_locked(token) == 0 {
            return false;
        }
        core::hint::cold_path();

        let mut found_pinned = false;
        let _ = self.page_list_locked(token).for_every_page_in_range(
            start_page_offset,
            end_page_offset,
            |p, off| {
                debug_assert!(off >= start_page_offset && off < end_page_offset);
                // SAFETY: `p.page()` is in `self`'s page list and `self`'s lock is held.
                if p.is_page() && unsafe { p.page().get_pin_count() } > 0 {
                    found_pinned = true;
                    return Status::STOP;
                }
                Status::NEXT
            },
        );

        found_pinned
    }

    /// See `VmObject::lookup`.
    pub fn lookup_locked<F>(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        mut lookup_fn: F,
    ) -> Result<(), Status>
    where
        F: FnMut(u64, PAddr) -> Status,
    {
        self.assert_canary();
        if range.is_empty() {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        // verify that the range is within the object
        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        let start_page_offset = ::page::round_down(range.offset as usize) as u64;
        let end_page_offset = ::page::round_up(range.end() as usize) as u64;

        self.page_list_locked(token).for_every_page_in_range(
            start_page_offset,
            end_page_offset,
            |p, off| {
                if !p.is_page() {
                    // Skip non pages.
                    return Status::NEXT;
                }
                // SAFETY: `p.page()` is a valid `VmPage` in `self`'s page list.
                let pa = unsafe { p.page().paddr() };
                lookup_fn(off, pa)
            },
        )
    }

    /// See `VmObject::enumerate_dirty_ranges`.
    pub fn enumerate_dirty_ranges_locked<F>(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        mut dirty_range_fn: F,
    ) -> Result<(), Status>
    where
        F: FnMut(u64, u64, bool) -> Status,
    {
        self.assert_canary();

        // Dirty pages are only tracked if the page source is a user pager.
        if self.page_source_type() != PageSourceType::UserPager {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        let start_offset = ::page::round_down(range.offset as usize) as u64;
        let end_offset = ::page::round_up(range.end() as usize) as u64;

        let status = self.page_list_locked(token).for_every_page_and_contiguous_run_in_range(
            start_offset,
            end_offset,
            |p, _off| {
                // Enumerate both AwaitingClean and Dirty pages, i.e. anything that is not Clean.
                // AwaitingClean pages are "dirty" too for the purposes of this enumeration, since
                // their modified contents are still in the process of being written back.
                if p.is_page() {
                    let page = p.page();
                    // SAFETY: `page` is in `self`'s page list and `self`'s lock is held.
                    unsafe {
                        debug_assert!(Self::is_page_dirty_tracked(page));
                        debug_assert!(Self::is_page_clean(page) || !page.is_loaned());
                        return !Self::is_page_clean(page);
                    }
                }
                // Enumerate any dirty zero intervals.
                if p.is_interval_zero() {
                    // For now we do not support clean intervals.
                    debug_assert!(!p.is_zero_interval_clean());
                    return p.is_zero_interval_dirty();
                }
                // Pager-backed VMOs cannot have compressed references, so the only other type is a
                // marker.
                debug_assert!(p.is_marker());
                false
            },
            |p, off| {
                if p.is_page() {
                    let page = p.page();
                    // SAFETY: `page` is in `self`'s page list and `self`'s lock is held.
                    unsafe {
                        debug_assert!(Self::is_page_dirty_tracked(page));
                        debug_assert!(!Self::is_page_clean(page));
                        debug_assert!(!page.is_loaned());
                        debug_assert_eq!(page.get_page_offset(), off);
                    }
                } else if p.is_interval_zero() {
                    debug_assert!(p.is_zero_interval_dirty());
                }
                Status::NEXT
            },
            |start, end, is_interval| {
                // Zero intervals are enumerated as zero ranges.
                dirty_range_fn(start, end - start, /*range_is_zero=*/ is_interval)
            },
        );

        self.continuous_attribution_validation_assert(token);
        status
    }

    #[inline]
    fn continuous_attribution_validation_assert(&self, token: &LockToken<'_, VmCowPagesLockClass>) {
        if cfg!(lk_debuglevel_gt2)
            && cfg!(feature = "experimental_continuous_per_vmo_attribution_enabled")
        {
            assert!(self.debug_validate_continuous_attribution_locked(token));
        }
    }

    /// Slow validation of the continuously tracked populated slots count.
    ///
    /// Returns true if the tracked populated slots count equals the number of pages, references,
    /// and parent content markers in the local page list.
    ///
    /// Panics if the populated slots feature is not enabled.
    #[allow(clippy::assertions_on_constants)]
    pub fn debug_validate_continuous_attribution_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        assert!(
            cfg!(feature = "experimental_continuous_per_vmo_attribution_enabled"),
            "The feature is required to check the continuously tracked populated slots count."
        );

        let mut populated_slots: u32 = 0;
        let status = self.page_list_locked(token).for_every_page(|slot, _| {
            if slot.is_page_or_ref() || slot.is_parent_content() {
                populated_slots += 1;
            }
            Status::NEXT
        });
        debug_assert!(status.is_ok());

        let tracked_size = self.continuous_attribution_tracker_locked(token).fetch_current();
        let okay = populated_slots == tracked_size;
        if !okay {
            core::hint::cold_path();
            dprintf!(
                INFO,
                "debug_validate_continuous_attribution_locked: found {} populated slots, \
                 but we are tracking {} populated slots\n",
                populated_slots,
                tracked_size
            );
        }
        okay
    }

    #[inline]
    fn children_list_len_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u32 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().children_list_len.get(token) }
    }

    #[inline]
    fn set_children_list_len_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        len: u32,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().children_list_len.get_mut(token) } = len;
    }

    #[inline]
    pub(crate) fn has_no_children_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        self.children_list_len_locked(token) == 0
    }

    /// See description on `pinned_page_count` for meaning.
    #[inline]
    pub(crate) fn pinned_page_count_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().pinned_page_count.get(token) }
    }

    #[inline]
    fn set_pinned_page_count_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        count: u64,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().pinned_page_count.get_mut(token) } = count;
    }

    #[inline]
    pub(crate) fn reclamation_event_count_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().reclamation_event_count.get(token) }
    }

    #[inline]
    fn set_reclamation_event_count_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        count: u64,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().reclamation_event_count.get_mut(token) } = count;
    }

    #[inline]
    fn high_priority_count_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> i64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().high_priority_count.get(token) }
    }

    #[inline]
    fn set_high_priority_count_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        count: i64,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().high_priority_count.get_mut(token) } = count;
    }

    #[inline]
    pub(crate) fn is_high_memory_priority_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        let count = self.high_priority_count_locked(token);
        debug_assert!(count >= 0);
        count != 0
    }

    #[inline]
    fn pager_stats_modified_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> bool {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().pager_stats_modified.get(token) }
    }

    #[inline]
    fn set_pager_stats_modified_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        modified: bool,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().pager_stats_modified.get_mut(token) } = modified;
    }

    #[inline]
    pub(crate) fn reset_pager_vmo_stats_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) {
        self.set_pager_stats_modified_locked(token, false);
    }

    #[inline]
    fn life_cycle_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> LifeCycle {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().life_cycle.get(token) }
    }

    #[inline]
    fn set_life_cycle_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        life_cycle: LifeCycle,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().life_cycle.get_mut(token) } = life_cycle;
    }

    /// VmCowPages are initially created in the Init state and need to be transitioned to Alive
    /// prior to being used. This is exposed for VmObjectPaged to call after ensuring that creation
    /// is successful, i.e. after it can guarantee that it will transition this cow pages to Dead
    /// prior to it being destroyed.
    pub(crate) fn transition_to_alive_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) {
        assert!(self.life_cycle_locked(token) == LifeCycle::Init);
        self.set_life_cycle_locked(token, LifeCycle::Alive);
    }

    /// Returns whether this object has ever held pinned pages. See `ever_pinned` for the access
    /// rules; this is the acquire load for callers that do not hold the lock.
    #[inline]
    fn ever_pinned(&self) -> bool {
        self.state().ever_pinned.load(Ordering::Acquire)
    }

    /// Relaxed read of `ever_pinned`, sufficient while the lock is held.
    #[inline]
    fn ever_pinned_locked(&self, _token: &LockToken<'_, VmCowPagesLockClass>) -> bool {
        self.state().ever_pinned.load(Ordering::Relaxed)
    }

    /// Release store of `ever_pinned`; must be performed while holding the lock.
    #[inline]
    fn set_ever_pinned_locked(
        &self,
        _token: &mut LockToken<'_, VmCowPagesLockClass>,
        ever_pinned: bool,
    ) {
        self.state().ever_pinned.store(ever_pinned, Ordering::Release);
    }

    /// A hidden VmCowPages has no associated VMO.
    #[inline]
    fn is_hidden(&self) -> bool {
        self.options().contains(VmCowPagesOptions::HIDDEN)
    }

    #[inline]
    fn can_decommit_zero_pages(&self) -> bool {
        !self.options().contains(VmCowPagesOptions::CANNOT_DECOMMIT_ZERO_PAGES)
    }

    /// Returns the size in bytes of this cow pages range. This will always be a multiple of the
    /// page size.
    #[inline]
    pub(crate) fn size_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().size.get(token) }
    }

    #[inline]
    fn set_size_locked(&self, token: &mut LockToken<'_, VmCowPagesLockClass>, size: u64) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().size.get_mut(token) } = size;
    }

    #[inline]
    fn parent_offset_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().parent_offset.get(token) }
    }

    #[inline]
    fn set_parent_offset_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().parent_offset.get_mut(token) } = offset;
    }

    #[inline]
    fn parent_limit_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().parent_limit.get(token) }
    }

    #[inline]
    fn set_parent_limit_locked(&self, token: &mut LockToken<'_, VmCowPagesLockClass>, limit: u64) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().parent_limit.get_mut(token) } = limit;
    }

    #[inline]
    fn root_parent_offset_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u64 {
        // SAFETY: The VmCowPages lock is held.
        unsafe { *self.state().root_parent_offset.get(token) }
    }

    #[inline]
    fn page_list_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
    ) -> &'a VmPageList {
        // SAFETY: The VmCowPages lock is held.
        unsafe { self.state().page_list.get(token) }
    }

    #[inline]
    fn page_list_locked_ptr(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) -> *mut VmPageList {
        // SAFETY: The VmCowPages lock is held.
        unsafe { self.state().page_list.as_mut_ptr(token) }
    }

    #[inline]
    fn continuous_attribution_tracker_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
    ) -> &'a AttributionTracker {
        // SAFETY: The VmCowPages lock is held.
        unsafe { self.state().continuous_attribution_tracker.get(token) }
    }

    #[inline]
    fn continuous_attribution_tracker_locked_ptr(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) -> *mut AttributionTracker {
        // SAFETY: The VmCowPages lock is held.
        unsafe { self.state().continuous_attribution_tracker.as_mut_ptr(token) }
    }

    #[inline]
    pub(crate) fn heap_allocation_bytes_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> u64 {
        self.page_list_locked(token).heap_allocation_bytes()
    }

    #[inline]
    fn set_root_parent_offset_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
    ) {
        // SAFETY: The VmCowPages lock is held.
        *unsafe { self.state().root_parent_offset.get_mut(token) } = offset;
    }

    /// Helper to free `pages` to the PMM. This function will also try to invoke `free_pages()` on
    /// the backing page source if it supports it. Given the allowance of freeing pages from any
    /// object in the hierarchy, but the page source only being on the root, it is a requirement
    /// (and checked on clone creation), that if a page source is handling free then it may not
    /// have CoW children. There is also an equivalent assumption that if the page source is
    /// handling free, then the page source will be supplying all the pages and this object must
    /// never allocate directly from the PMM.
    ///
    /// Callers should avoid calling `pmm::free_list()` directly from inside `VmCowPages`, and
    /// instead should use this helper.
    ///
    /// # Safety
    ///
    /// `pages` must contain valid allocated pages owned by this `VmCowPages` that are not in any
    /// page queues.
    #[inline]
    pub unsafe fn free_pages(&self, pages: Pin<&mut VmPageDoublyLinkedList>) {
        match self.page_source_type() {
            PageSourceType::Anonymous | PageSourceType::UserPager => {
                // SAFETY: The caller guarantees `pages` contains valid allocated pages ready to be
                // freed.
                unsafe { Self::cache_free_list(pages, self.should_delay_reuse_on_free()) };
            }
            PageSourceType::Contiguous => {
                core::hint::cold_path();
                let page_source = self.page_source().expect("contiguous VMO must have page_source");
                // SAFETY: The caller guarantees `pages` contains valid pages belonging to `self`'s
                // contiguous page source.
                unsafe { page_source.free_pages(pages) };
            }
        }
    }

    /// Helper to free `page` to the PMM. This function will also try to invoke `free_pages()` on
    /// the backing page source if it supports it. Given the allowance of freeing pages from any
    /// object in the hierarchy, but the page source only being on the root, it is a requirement
    /// (and checked on clone creation), that if a page source is handling free then it may not
    /// have CoW children. There is also an equivalent assumption that if the page source is
    /// handling free, then the page source will be supplying all the pages and this object must
    /// never allocate directly from the PMM.
    ///
    /// Callers should avoid calling `pmm::free_page()` directly from inside `VmCowPages`, and
    /// instead should use this helper.
    ///
    /// # Safety
    ///
    /// `page` must be a valid allocated page owned by this `VmCowPages` that is not in any page
    /// queue or list.
    #[inline]
    pub unsafe fn free_page(&self, page: VmPagePtr) {
        // SAFETY: `page` is a valid `VmPage`.
        debug_assert!(!unsafe { page.as_ref() }.get_node().in_container());
        match self.page_source_type() {
            PageSourceType::Anonymous | PageSourceType::UserPager => {
                // SAFETY: The caller guarantees `page` is a valid allocated page ready to be freed.
                unsafe { Self::cache_free_page(page, self.should_delay_reuse_on_free()) };
            }
            PageSourceType::Contiguous => {
                core::hint::cold_path();
                let page_source = self.page_source().expect("contiguous VMO must have page_source");
                stack_pin_init!(let list = VmPageDoublyLinkedList::new());
                // SAFETY: `page` is not in any container.
                unsafe { list.as_mut().push_back_raw(page.as_non_null()) };
                // SAFETY: `list` contains `page` which belongs to `self`'s contiguous page source.
                unsafe { page_source.free_pages(list) };
            }
        }
    }

    /// Allocates a single uninitialized page from the COW `PageCache` (or directly from the PMM if
    /// the page cache is not yet initialized).
    fn cache_alloc_page(alloc_flags: u32) -> Result<VmPagePtr, Status> {
        if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) {
            core::hint::cold_path();
            return pmm::node().alloc_page(alloc_flags);
        }

        stack_pin_init!(let list = PageList::new());
        PAGE_CACHE.allocate(1, alloc_flags, list.as_mut())?;

        let page = list.as_mut().list_mut().pop_front();
        debug_assert!(page.is_some());
        // SAFETY: `allocate` succeeded, so `list` holds exactly the one page requested.
        let page = unsafe { page.unwrap_unchecked() };
        debug_assert!(list.is_empty());
        Ok(VmPagePtr::new(page))
    }

    /// Frees a list of pages to the COW `PageCache` (or directly to the PMM if the page cache is
    /// not yet initialized or `delay_reuse == PmmOptDelayReuse::Yes`).
    ///
    /// # Safety
    ///
    /// `list` must contain valid allocated pages that are not in any page queues.
    unsafe fn cache_free_list(
        list: Pin<&mut VmPageDoublyLinkedList>,
        delay_reuse: PmmOptDelayReuse,
    ) {
        if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) || delay_reuse == PmmOptDelayReuse::Yes {
            core::hint::cold_path();
            // SAFETY: The caller guarantees `list` contains valid allocated pages.
            unsafe { pmm::node().free_list(list, delay_reuse) };
            return;
        }

        // SAFETY: The caller guarantees `list` contains valid, exclusively owned, non-loaned
        // allocated PMM pages that are not in any page queue.
        stack_pin_init!(let page_list = unsafe { PageList::from_list(list) });
        PAGE_CACHE.free(page_list);
    }

    /// Frees a single page to the COW `PageCache` (or directly to the PMM if the page cache is
    /// not yet initialized or `delay_reuse == PmmOptDelayReuse::Yes`).
    ///
    /// # Safety
    ///
    /// `page` must be a valid allocated page that is not in any page queue or list.
    unsafe fn cache_free_page(page: VmPagePtr, delay_reuse: PmmOptDelayReuse) {
        if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) || delay_reuse == PmmOptDelayReuse::Yes {
            core::hint::cold_path();
            // SAFETY: The caller guarantees `page` is a valid allocated page.
            unsafe { pmm::node().free_page(page, delay_reuse) };
            return;
        }

        stack_pin_init!(let list = PageList::new());
        // SAFETY: The caller guarantees `page` is not in any list.
        unsafe { list.as_mut().list_mut().push_back_raw(page.as_non_null()) };
        PAGE_CACHE.free(list);
    }

    /// Helper for allocating a page for this VMO. The allocated page is not yet initialized, and
    /// `initialize_vm_page` must be called on it prior to use. Callers most likely want
    /// `alloc_page` instead, with this method being useful in rare cases where you want to defer
    /// initialization till `add_new_pages_locked` or similar.
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok`, the returned page is a valid `VmPage` in the `ALLOC` state, is in no list, and is
    /// conceptually owned by the caller.
    fn alloc_uninitialized_page(
        &self,
        mut request: Option<&mut AnonymousPageRequest>,
    ) -> Result<VmPagePtr, Status> {
        debug_assert_matches!(
            self.page_source_type(),
            PageSourceType::Anonymous | PageSourceType::UserPager
        );
        // Another layer has already allocated a page for us.
        if let Some(page) = request.as_deref_mut().and_then(AnonymousPageRequest::take_page) {
            return Ok(page);
        }

        let res = Self::cache_alloc_page(self.pmm_alloc_flags());
        if let Err(Status::SHOULD_WAIT) = res
            && let Some(req) = request
        {
            core::hint::cold_path();
            req.make_active();
        }
        res
    }

    /// Helper to allocate a new page for the VMO, filling out the page request if necessary.
    pub fn alloc_page(
        &self,
        request: Option<&mut AnonymousPageRequest>,
    ) -> Result<VmPagePtr, Status> {
        let page = self.alloc_uninitialized_page(request)?;
        // SAFETY: Per `alloc_uninitialized_page`'s postcondition, `page` is a valid `VmPage` in
        // the `ALLOC` state, in no list, and conceptually owned by this function.
        unsafe { initialize_vm_page(page) };
        Ok(page)
    }

    /// Changes a Reference in the provided `VmPageOrMarkerRef` into a real `VmPagePtr`. The
    /// allocated page is assumed to be for this `VmCowPages`, and so uses `pmm_alloc_flags`, but
    /// it is not assumed that `page_or_mark` is actually yet in this `page_list`, and so the
    /// allocated page is not added to the page queues. It is the responsibility of the caller to
    /// add to the page queues if `page_or_mark` is not stack owned.
    ///
    /// `page_request` must be `Some` if `pmm_alloc_flags` allow for delayed allocation, in which
    /// case this may return `Err(Status::SHOULD_WAIT)` if the `page_request` is filled out.
    ///
    /// # Safety
    ///
    /// * `page_or_mark` must currently hold a valid, live compressed reference belonging to the
    ///   PMM's `VmCompression` instance.
    /// * Either `page_or_mark` is a slot in `self`'s page list and the caller holds `self`'s lock,
    ///   or it is a slot that is not in any `VmCowPages` page list (such as a `VmPageSpliceList`
    ///   entry); in the latter case it cannot hold the compressor's temporary reference, because
    ///   `VmCompression::move_reference` converts a temporary reference back to a page before it
    ///   is moved out of the owning VMO's page list.
    /// * The caller must be in ordinary blocking thread context with no chain-lock transaction
    ///   active.
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok`, `page_or_mark` holds a freshly initialized `OBJECT`-state page owned by this
    /// `VmCowPages` that is not yet in any page queue.
    unsafe fn make_page_from_reference(
        &self,
        page_or_mark: &mut VmPageOrMarkerRef<'_>,
        page_request: Option<&mut AnonymousPageRequest>,
    ) -> Result<(), Status> {
        let _memory_stall =
            BootOptions::get().experimental_expand_memory_stall.then(ScopedMemoryStall::new);
        debug_assert!(page_or_mark.is_reference());
        let compression = pmm::node().get_page_compression().expect("compression must be present");

        let p = self.alloc_page(page_request)?;

        let r = page_or_mark.swap_reference_for_page(p);
        // SAFETY: `p` is a valid allocated physical page in the physmap.
        let page_data = paddr_to_physmap_mut::<u8>(unsafe { p.as_ref() }.paddr());
        let mut page_metadata: u32 = 0;
        // SAFETY: `decompress` contract: (1) by this function's `# Safety` contract `page_or_mark`
        // held a valid live reference of this compressor, and `r` was just swapped out of it, so
        // this function exclusively owns `r` and does not use it again; (2) by the same contract,
        // if `r` is the temporary reference then `page_or_mark` is a slot of `self`'s page list and
        // the caller holds `self`'s lock, which is the lock `decompress` requires; (3) `page_data`
        // is the physmap mapping of the freshly allocated, not-yet-shared page `p`, valid for
        // exclusive writes of `page::SIZE` bytes and disjoint from any compressed storage; (4) by
        // the same contract this runs in ordinary blocking thread context with no chain-lock
        // transaction active.
        unsafe { compression.decompress(r, page_data, &mut page_metadata) };
        // Ensure the share count is propagated from the compressed page.
        // SAFETY: `alloc_page` returned `p` in the `OBJECT` state (so `object` is the active union
        // variant), and `p` is reachable only through `page_or_mark`, which this function holds
        // `&mut` to, so no other thread can access `share_count` concurrently.
        unsafe { p.set_share_count(page_metadata) };

        Ok(())
    }

    /// Replaces the Reference in `VmPageOrMarkerRef` owned by this `page_list` for a real
    /// `VmPagePtr`. Unlike `make_page_from_reference` this updates the page queues to track the
    /// newly added page. Use of `page_request` and implications on return value are the same as
    /// `make_page_from_reference`.
    ///
    /// # Safety
    ///
    /// `page_or_mark` must be a slot of `self`'s page list at `offset` that currently holds a
    /// valid, live compressed reference belonging to the PMM's `VmCompression` instance.
    unsafe fn replace_reference_with_page_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        page_or_mark: &mut VmPageOrMarkerRef<'_>,
        offset: u64,
        page_request: Option<&mut AnonymousPageRequest>,
    ) -> Result<(), Status> {
        // First replace the ref with a page.
        // SAFETY: By this function's `# Safety` contract `page_or_mark` is a slot of `self`'s page
        // list holding a valid reference, and `token` proves `self`'s lock (a mutex) is held, so
        // this is blocking thread context with no chain-lock transaction active.
        unsafe { self.make_page_from_reference(page_or_mark, page_request)? };
        // Add the new page to the page queues for tracking. References are by definition not
        // pinned, so we know this is not wired.
        // SAFETY: Per `make_page_from_reference`'s postcondition, `page_or_mark` now holds an
        // initialized `OBJECT`-state page owned by `self` at `offset` that is not yet in any page
        // queue.
        unsafe {
            self.set_not_pinned_locked(token, page_or_mark.page(), offset);
        }
        Ok(())
    }

    /// Allocates a new page and populates it with the data at `parent_paddr`.
    ///
    /// # Safety
    ///
    /// * Any page in `alloc_list` must be a valid `VmPage` in the `ALLOC` state that the caller
    ///   conceptually owns.
    /// * If `parent_paddr != vm_get_zero_page_paddr()`, `parent_paddr` must be a valid
    ///   page-aligned physical RAM address in the physmap that is not concurrently mutated for the
    ///   duration of the copy.
    unsafe fn allocate_copy_page(
        &self,
        parent_paddr: PAddr,
        alloc_list: Option<Pin<&mut VmPageDoublyLinkedList>>,
        mut request: Option<&mut AnonymousPageRequest>,
    ) -> Result<VmPagePtr, Status> {
        debug_assert!(request.is_some() || (self.pmm_alloc_flags() & ALLOC_FLAG_CAN_WAIT) == 0);
        debug_assert_matches!(
            self.page_source_type(),
            PageSourceType::Anonymous | PageSourceType::UserPager
        );

        let p_clone = match request
            .as_deref_mut()
            .and_then(AnonymousPageRequest::take_page)
            .or_else(|| alloc_list.and_then(|list| list.pop_front().map(VmPagePtr::new)))
        {
            Some(page) => page,
            None => self.alloc_uninitialized_page(request)?,
        };
        // SAFETY: `p_clone` was taken from `AnonymousPageRequest`, popped from `alloc_list` (per
        // `# Safety`), or returned by `alloc_uninitialized_page`, so the caller conceptually owns
        // it in the `ALLOC` state and it is not in any list.
        unsafe { initialize_vm_page(p_clone) };

        // SAFETY: `p_clone` is a valid allocated page in the physmap.
        let dst_paddr = unsafe { p_clone.as_ref() }.paddr();
        let dst = paddr_to_physmap_mut::<u8>(dst_paddr);
        debug_assert!(!dst.is_null());

        if parent_paddr != vm_get_zero_page_paddr() {
            // Do a direct copy of the two pages.
            let src = paddr_to_physmap_ptr::<u8>(parent_paddr);
            debug_assert!(!src.is_null());
            // SAFETY: `src` and `dst` point to distinct, valid `PAGE_SIZE`-byte physmap pages.
            unsafe {
                ptr::copy_nonoverlapping(src, dst, ::page::SIZE);
            }
        } else {
            // Avoid pointless fetches by directly zeroing `dst`.
            // SAFETY: `dst_paddr` maps to a valid writable `PAGE_SIZE`-byte physmap page.
            unsafe {
                zero_page(paddr_to_physmap(dst_paddr));
            }
        }

        Ok(p_clone)
    }

    /// Helper for removing a page from the `PageQueues` and adding to a `DeferredOps` for later
    /// freeing.
    ///
    /// # Safety
    ///
    /// * `page` must be a valid `VmPage` in the `OBJECT` state owned by `self` and currently in
    ///   the page queues.
    /// * `ops` must be the `DeferredOps` that was constructed for `self` (its pages are freed
    ///   against the `VmCowPages` recorded in `ops`).
    unsafe fn remove_page_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        ops: Pin<&mut DeferredOps<'_>>,
    ) {
        // SAFETY: Contract of `VmPagePtr::is_loaned`: the caller conceptually owns the page or
        // knows its loaned state cannot change concurrently. By `# Safety`, `page` is owned by
        // `self` and `self`'s lock is held (`_token`).
        if unsafe { page.is_loaned() } {
            core::hint::cold_path();
            let flph = ops.freed_list(self).flph();
            // SAFETY: Contract of `PmmNode::begin_free_loaned_page`: `page` is valid, loaned
            // (checked above) and owned by the caller with a valid `OBJECT` backlink — all given
            // by this function's `# Safety`. The `release_page` callback is required to remove
            // the backlink; it calls `PageQueues::remove`, whose contract (page owned by a VMO
            // and assigned to this `PageQueues`) is also this function's `# Safety`.
            unsafe {
                pmm::node().begin_free_loaned_page(
                    page,
                    |page| pmm_page_queues().remove(page),
                    flph,
                );
            }
        } else {
            // SAFETY: Contract of `PageQueues::remove`: `page` is owned by a VMO (`self`) and in
            // the page queues — this function's `# Safety`.
            unsafe { pmm_page_queues().remove(page) };
            // SAFETY: Contract of `push_back_raw`: `page` is valid and in no list — it was just
            // unlinked from the page queues above and no code in between can re-link it (lock
            // held). Postcondition: `page` now satisfies the `ScopedPageFreedList::list` safety
            // invariant (owned by `self`, not queued).
            unsafe { ops.freed_list(self).list().push_back_raw(page.as_non_null()) };
        }
    }
}

/// Transitions a newly allocated `VmPage` from `ALLOC` to `OBJECT` and initializes its `object`
/// metadata fields.
///
/// Note that this takes `VmPagePtr` rather than `&mut VmPage` because `VmPage` instances live in
/// PMM arenas where `PmmNode` may concurrently access `loaned_state_priv` atomically (see the
/// `VmPage` type-level documentation on conceptual vs. Rust ownership).
///
/// # Safety
///
/// `p` must point to a valid `VmPage` in the `ALLOC` state that the caller conceptually owns and
/// that is not in any list or container (so the caller has exclusive access to `state_priv` and
/// `state_union`). Passing a page in any state other than `ALLOC` is undefined behavior, as
/// another subsystem may own `state_union` or rely on its active union variant.
#[inline]
unsafe fn initialize_vm_page(p: VmPagePtr) {
    // SAFETY: By this function's `# Safety` contract `p` is a live `VmPage` (so `as_ref` is
    // valid) in the `ALLOC` state that the caller conceptually owns and that is in no list, so no
    // other CPU or subsystem reads or writes `state_priv` or `state_union` during this function;
    // that discharges the "no data race" / ownership obligations of `set_state` and of every
    // `object`-variant accessor below. `set_state(OBJECT)` is executed first, after which `object`
    // is the active union variant required by `set_share_count`, `set_pin_count`,
    // `set_always_need`, `set_dirty_state`, `set_object`, `set_page_offset` and
    // `get_page_queue_ref`. A `Relaxed` store suffices because the page is not yet visible to
    // any other thread; publication happens later under the owning VMO lock.
    unsafe {
        let page = p.as_ref();
        debug_assert!(!page.get_node().in_container());
        // Page should be in the ALLOC state so we can transition it to the OBJECT state.
        debug_assert_eq!(page.state(), VmPageState::ALLOC);
        page.set_state(VmPageState::OBJECT);
        page.set_share_count(0);
        page.set_pin_count(0);
        page.set_always_need(false);
        page.set_dirty_state(DirtyState::Untracked as u8);
        page.set_object(ptr::null_mut());
        page.set_page_offset(0);
        page.get_page_queue_ref().store(0, Ordering::Relaxed);
    }
}

/// C++ `InitializeVmPage` entry point.
///
/// # Safety
///
/// `page` must satisfy the preconditions of [`initialize_vm_page`]: it must point to a valid
/// `VmPage` in the `ALLOC` state that the caller conceptually owns and that is not in any list.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_initialize_vm_page(page: VmPagePtr) {
    // SAFETY: Caller upholds the `initialize_vm_page` preconditions.
    unsafe { initialize_vm_page(page) };
}

/// `PageCache` instance for COW page allocations.
static PAGE_CACHE: LazyInit<PageCache> = LazyInit::uninit();
static PAGE_CACHE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initializes the PageCache instance for COW page allocations.
fn initialize_page_cache(level: init::LkInitLevel) {
    static INIT_ONCE: AtomicBool = AtomicBool::new(false);
    assert!(!INIT_ONCE.swap(true, Ordering::AcqRel));
    assert!(level < init::LK_INIT_LEVEL_THREADING);

    const RESERVE_PAGES: usize = 64;
    let cache = PageCache::new(RESERVE_PAGES).expect("PageCache::new failed");

    if BootOptions::get().pmm_alloc_random_should_wait {
        cache.seed_random_should_wait();
    }

    // SAFETY: `INIT_ONCE` ensures single initialization on the boot CPU prior to
    // `LK_INIT_LEVEL_THREADING`, and callers gate access on `PAGE_CACHE_INITIALIZED` with
    // Acquire/Release ordering.
    unsafe { PAGE_CACHE.init(cache) };
    PAGE_CACHE_INITIALIZED.store(true, Ordering::Release);
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(vm_cow_pages_cache_init, initialize_page_cache, init::LK_INIT_LEVEL_KERNEL);

/// # Safety
///
/// * The pointer passed for `storage` must be non-null, suitably aligned, point to
///   `kVmCowPagesStateSize` bytes of storage, and must not be aliased for the duration of the call.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_cow_pages_state_init(
    storage: &mut MaybeUninit<VmCowPagesState>,
    pmm_alloc_flags: u32,
    options: u32,
    size: u64,
) {
    storage.write(VmCowPagesState {
        canary: Canary::default(),
        pmm_alloc_flags,
        options: VmCowPagesOptions::from_bits_retain(options),
        children_list_len: KCell::new(0),
        pinned_page_count: KCell::new(0),
        reclamation_event_count: KCell::new(0),
        high_priority_count: KCell::new(0),
        pager_stats_modified: KCell::new(false),
        life_cycle: KCell::new(LifeCycle::Init),
        ever_pinned: AtomicBool::new(false),
        size: KCell::new(size),
        parent_offset: KCell::new(0),
        parent_limit: KCell::new(0),
        root_parent_offset: KCell::new(0),
        continuous_attribution_tracker: KCell::new(AttributionTracker::new()),
        page_list: KCell::new(VmPageList::new()),
    });
}

/// # Safety
///
/// * The pointer passed for `storage` must be non-null, suitably aligned, point to
///   `kVmCowPagesStateSize` bytes of initialized storage, and must not be aliased for the duration
///   of the call.
/// * After this call returns, the caller must not use `storage` again as an initialized
///   `VmCowPagesState`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_destruct(storage: &mut MaybeUninit<VmCowPagesState>) {
    // SAFETY: The caller guarantees `storage` is initialised and is not used afterwards.
    unsafe {
        storage.assume_init_drop();
    }
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_assert_canary(cow: &VmCowPages) {
    cow.assert_canary();
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_options(cow: &VmCowPages) -> u32 {
    cow.options().bits()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_pmm_alloc_flags(cow: &VmCowPages) -> u32 {
    cow.pmm_alloc_flags()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_children_list_len_locked(this: &VmCowPages) -> u32 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.children_list_len_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_children_list_len_locked(this: &VmCowPages, v: u32) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_children_list_len_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_pinned_page_count_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.pinned_page_count_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_pinned_page_count_locked(this: &VmCowPages, v: u64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_pinned_page_count_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_reclamation_event_count_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.reclamation_event_count_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_reclamation_event_count_locked(
    this: &VmCowPages,
    v: u64,
) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_reclamation_event_count_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_high_priority_count_locked(this: &VmCowPages) -> i64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.high_priority_count_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_high_priority_count_locked(this: &VmCowPages, v: i64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_high_priority_count_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_pager_stats_modified_locked(this: &VmCowPages) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.pager_stats_modified_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_pager_stats_modified_locked(this: &VmCowPages, v: bool) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_pager_stats_modified_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_life_cycle_locked(this: &VmCowPages) -> LifeCycle {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.life_cycle_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_life_cycle_locked(this: &VmCowPages, v: LifeCycle) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_life_cycle_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_ever_pinned(this: &VmCowPages) -> bool {
    this.ever_pinned()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_ever_pinned_locked(this: &VmCowPages) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.ever_pinned_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_ever_pinned_locked(this: &VmCowPages, v: bool) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_ever_pinned_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_is_high_memory_priority_locked(this: &VmCowPages) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.is_high_memory_priority_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_has_no_children_locked(this: &VmCowPages) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.has_no_children_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_transition_to_alive_locked(this: &VmCowPages) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.transition_to_alive_locked(&mut token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_reset_pager_vmo_stats_locked(this: &VmCowPages) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.reset_pager_vmo_stats_locked(&mut token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_is_hidden(this: &VmCowPages) -> bool {
    this.is_hidden()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_can_decommit_zero_pages(this: &VmCowPages) -> bool {
    this.can_decommit_zero_pages()
}

/// Returns the size in bytes of this cow pages range. This will always be a multiple of the page
/// size.
///
/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_size_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.size_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_size_locked(this: &VmCowPages, v: u64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_size_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_parent_offset_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.parent_offset_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_parent_offset_locked(this: &VmCowPages, v: u64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_parent_offset_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_parent_limit_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.parent_limit_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_parent_limit_locked(this: &VmCowPages, v: u64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_parent_limit_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_root_parent_offset_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.root_parent_offset_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
/// * The returned pointer is valid only while the lock is held and the object lives.
/// * C++ must not retain the returned pointer across a lock drop.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_page_list_locked(this: &VmCowPages) -> *mut VmPageList {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.page_list_locked_ptr(&mut token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
/// * The returned pointer is valid only while the lock is held and the object lives.
/// * C++ must not retain the returned pointer across a lock drop.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_continuous_attribution_tracker_locked(
    this: &VmCowPages,
) -> *mut AttributionTracker {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.continuous_attribution_tracker_locked_ptr(&mut token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_heap_allocation_bytes_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.heap_allocation_bytes_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_root_parent_offset_locked(this: &VmCowPages, v: u64) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.set_root_parent_offset_locked(&mut token, v)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `stats` must be non-null, aligned and valid for a write of `zx_pager_vmo_stats_t`; it may
///   point to uninitialised memory (it is only written, and only on `ZX_OK`).
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_query_pager_vmo_stats_locked(
    this: &VmCowPages,
    reset: bool,
    stats: *mut zx_pager_vmo_stats_t,
) -> zx_status_t {
    debug_assert!(!stats.is_null());
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    Status::result_into_raw(this.query_pager_vmo_stats_locked(&mut token, reset).map(|value| {
        // SAFETY: `stats` is valid for writes per `# Safety`; `ptr::write` does not read or drop
        // the (possibly uninitialised) previous contents.
        unsafe { stats.write(value) };
    }))
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must be a valid `VmPagePtr` in the `OBJECT` state already in a page queue.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_move_to_pinned_locked(
    this: &VmCowPages,
    page: VmPagePtr,
    offset: u64,
) {
    // SAFETY: the C++ caller holds the lock (TA_REQ) and guarantees `page` is valid.
    unsafe {
        let token = LockToken::new();
        this.move_to_pinned_locked(&token, page, offset);
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must be a valid `VmPagePtr` in the `OBJECT` state owned by `this` and already in a
///   page queue.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_move_to_not_pinned_locked(
    this: &VmCowPages,
    page: VmPagePtr,
    offset: u64,
) {
    // SAFETY: the C++ caller holds the lock (TA_REQ) and guarantees `page` is valid.
    unsafe {
        let token = LockToken::new();
        this.move_to_not_pinned_locked(&token, page, offset);
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must be a valid `VmPagePtr` in the `OBJECT` state owned by `this` and not yet in any
///   page queue.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_not_pinned_locked(
    this: &VmCowPages,
    page: VmPagePtr,
    offset: u64,
) {
    // SAFETY: the C++ caller holds the lock (TA_REQ) and guarantees `page` is valid.
    unsafe {
        let token = LockToken::new();
        this.set_not_pinned_locked(&token, page, offset);
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must be null or a valid `vm_page_t` in the `OBJECT` state owned by `this` (or
///   pending add). A null `page` panics, matching the C++ `ASSERT(page)`.
/// * `dirty_state` must be one of the `VmCowPages::DirtyState` enumerators.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_update_dirty_state_locked(
    this: &VmCowPages,
    page: Option<VmPagePtr>,
    offset: u64,
    dirty_state: DirtyState,
    is_pending_add: bool,
) {
    let page = page.expect("page");
    // SAFETY: the C++ caller holds the lock (TA_REQ) and guarantees `page` is valid.
    unsafe {
        let token = LockToken::new();
        this.update_dirty_state_locked(&token, page, offset, dirty_state, is_pending_add);
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_finish_cache_policy_transition_locked(this: &VmCowPages) {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.finish_cache_policy_transition_locked(&token);
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_any_pages_pinned_locked(
    this: &VmCowPages,
    offset: u64,
    len: usize,
) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.any_pages_pinned_locked(&token, offset, len)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_range_pinned_locked(
    this: &VmCowPages,
    range: VmCowRange,
) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.debug_is_range_pinned_locked(&token, range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `lookup_fn` must point to a valid `VmObject::LookupFunction` for the duration of the call.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_locked(
    this: &VmCowPages,
    range: VmCowRange,
    lookup_fn: *mut c_void,
) -> zx_status_t {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    Status::result_into_raw(this.lookup_locked(&token, range, move |offset, pa| {
        // SAFETY: `lookup_fn` is a valid `VmObject::LookupFunction` for the duration of this call.
        // `ZX_OK` is not representable as a `Status`; a C++ callback returning it ends the walk
        // with `ZX_OK`, which is exactly `Status::STOP`'s contract. All other codes map 1:1.
        Status::try_from_raw(unsafe {
            bindings::cpp_vm_cow_pages_call_lookup_fn(lookup_fn, offset, pa.0 as zx_paddr_t)
        })
        .unwrap_or(Status::STOP)
    }))
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `dirty_range_fn` must point to a valid `DirtyRangeEnumerateFunction` for the duration of
///   the call.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_enumerate_dirty_ranges_locked(
    this: &VmCowPages,
    range: VmCowRange,
    dirty_range_fn: *mut c_void,
) -> zx_status_t {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    Status::result_into_raw(this.enumerate_dirty_ranges_locked(
        &token,
        range,
        move |range_offset, range_len, range_is_zero| {
            // SAFETY: `dirty_range_fn` is a valid `DirtyRangeEnumerateFunction` for the duration
            // of this call. `ZX_OK` is not representable as a `Status`; a C++ callback returning
            // it ends the walk with `ZX_OK`, which is exactly `Status::STOP`'s contract. All other
            // codes map 1:1.
            Status::try_from_raw(unsafe {
                bindings::cpp_vm_cow_pages_call_dirty_range_fn(
                    dirty_range_fn,
                    range_offset,
                    range_len,
                    range_is_zero,
                )
            })
            .unwrap_or(Status::STOP)
        },
    ))
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_validate_continuous_attribution_locked(
    this: &VmCowPages,
) -> bool {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.debug_validate_continuous_attribution_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_page_count_locked(this: &VmCowPages) -> u64 {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.debug_get_page_count_locked(&token)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_page(this: &VmCowPages, offset: u64) -> bool {
    this.debug_is_page(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_marker(this: &VmCowPages, offset: u64) -> bool {
    this.debug_is_marker(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_marker_share_count(
    this: &VmCowPages,
    offset: u64,
) -> u32 {
    this.debug_get_marker_share_count(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_parent_content(
    this: &VmCowPages,
    offset: u64,
) -> bool {
    this.debug_is_parent_content(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_empty(this: &VmCowPages, offset: u64) -> bool {
    this.debug_is_empty(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_page(
    this: &VmCowPages,
    offset: u64,
) -> Option<VmPagePtr> {
    this.debug_get_page(offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * The C++ caller must hold `lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_page_locked(
    this: &VmCowPages,
    offset: u64,
) -> Option<VmPagePtr> {
    // SAFETY: the C++ caller holds the lock (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.debug_get_page_locked(&token, offset)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_is_high_memory_priority(this: &VmCowPages) -> bool {
    this.debug_is_high_memory_priority()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_populated_slots_count(this: &VmCowPages) -> u32 {
    this.debug_get_populated_slots_count()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_should_delay_reuse_on_free(
    this: &VmCowPages,
) -> PmmOptDelayReuse {
    this.should_delay_reuse_on_free()
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `pages` must contain allocated pages conceptually owned by `this` that are not in any page
///   queue.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_free_pages(
    this: &VmCowPages,
    pages: Pin<&mut VmPageDoublyLinkedList>,
) {
    // SAFETY: Caller guarantees `pages` contains allocated pages owned by `this` that are not in
    // any page queue.
    unsafe { this.free_pages(pages) };
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must be a valid allocated `VmPage` owned by `this` that is not in any list or page
///   queue.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_free_page(this: &VmCowPages, page: VmPagePtr) {
    // SAFETY: Caller upholds `free_page` safety preconditions.
    unsafe { this.free_page(page) };
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must point to writable, properly aligned storage for a `VmPagePtr`.
/// * `page_request` must be null or point to a valid `AnonymousPageRequest` that no other code
///   accesses for the duration of the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_alloc_uninitialized_page(
    this: &VmCowPages,
    page: &mut MaybeUninit<VmPagePtr>,
    page_request: *mut AnonymousPageRequest,
) -> Result<(), Status> {
    // SAFETY: `page_request` is either null or a valid `AnonymousPageRequest` that no other
    // code accesses for the duration of the call.
    let req = unsafe { page_request.as_mut() };
    this.alloc_uninitialized_page(req).map(|p| {
        page.write(p);
    })
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page` must point to writable, properly aligned storage for a `VmPagePtr`.
/// * `page_request` must be null or point to a valid `AnonymousPageRequest` that no other code
///   accesses for the duration of the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_alloc_page(
    this: &VmCowPages,
    page: &mut MaybeUninit<VmPagePtr>,
    page_request: *mut AnonymousPageRequest,
) -> Result<(), Status> {
    // SAFETY: `page_request` is either null or a valid `AnonymousPageRequest` that no other
    // code accesses for the duration of the call.
    let req = unsafe { page_request.as_mut() };
    this.alloc_page(req).map(|p| {
        page.write(p);
    })
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * If `parent_paddr != vm_get_zero_page_paddr()`, `parent_paddr` must be a valid page-aligned
///   physical RAM address in the physmap that is not concurrently mutated for the duration of the
///   copy.
/// * `alloc_list` must be null or a non-null, aligned pointer to an initialized
///   `VmPageDoublyLinkedList` that no other code accesses for the duration of the call and that
///   is never moved, and every page in `alloc_list` must be a valid `VmPage` in the `ALLOC` state
///   that the caller conceptually owns.
/// * `request` must be null or point to a valid `AnonymousPageRequest` that no other code
///   accesses for the duration of the call.
/// * `clone` must point to writable, properly aligned storage for a `VmPagePtr`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_allocate_copy_page(
    this: &VmCowPages,
    parent_paddr: PAddr,
    alloc_list: *mut VmPageDoublyLinkedList,
    request: *mut AnonymousPageRequest,
    clone: &mut MaybeUninit<VmPagePtr>,
) -> Result<(), Status> {
    // SAFETY: `alloc_list` is either null or valid, exclusively accessed and never moved.
    let list = unsafe { alloc_list.as_mut().map(|l| Pin::new_unchecked(l)) };
    // SAFETY: `request` is either null or a valid `AnonymousPageRequest` that no other
    // code accesses for the duration of the call.
    let req = unsafe { request.as_mut() };
    // SAFETY: Caller upholds the `allocate_copy_page` preconditions on `parent_paddr` and
    // `alloc_list`.
    unsafe { this.allocate_copy_page(parent_paddr, list, req) }.map(|p| {
        clone.write(p);
    })
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`.
/// * `page_or_mark` must wrap a non-null `VmPageOrMarker*` holding a valid, live compressed
///   reference of the PMM's `VmCompression` that no other code accesses for the duration of the
///   call. If it is a slot of a `VmCowPages` page list the caller must hold that VMO's lock;
///   otherwise it must not be in any `VmCowPages` page list.
/// * The caller must be in ordinary blocking thread context with no chain-lock transaction
///   active.
/// * `page_request` must be null or point to a valid `AnonymousPageRequest` that no other code
///   accesses for the duration of the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_make_page_from_reference(
    this: &VmCowPages,
    mut page_or_mark: VmPageOrMarkerRef<'_>,
    page_request: *mut AnonymousPageRequest,
) -> Result<(), Status> {
    // SAFETY: `page_request` is either null or a valid `AnonymousPageRequest` that no other
    // code accesses for the duration of the call.
    let req = unsafe { page_request.as_mut() };
    // SAFETY: The caller upholds `make_page_from_reference`'s preconditions (see above).
    unsafe { this.make_page_from_reference(&mut page_or_mark, req) }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `page_or_mark` must wrap a non-null `VmPageOrMarker*` in `this.page_list` at `offset`
///   holding a valid, live compressed reference of the PMM's `VmCompression` that no other code
///   accesses for the duration of the call.
/// * `page_request` must be null or point to a valid `AnonymousPageRequest` that no other code
///   accesses for the duration of the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_replace_reference_with_page_locked(
    this: &VmCowPages,
    mut page_or_mark: VmPageOrMarkerRef<'_>,
    offset: u64,
    page_request: *mut AnonymousPageRequest,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    // SAFETY: `page_request` is either null or a valid `AnonymousPageRequest` that no other
    // code accesses for the duration of the call.
    let req = unsafe { page_request.as_mut() };
    // SAFETY: The caller upholds `replace_reference_with_page_locked`'s slot precondition (see
    // above).
    unsafe { this.replace_reference_with_page_locked(&token, &mut page_or_mark, offset, req) }
}

/// # Safety
///
/// * `list` must point to valid, aligned, uninitialized storage for `ScopedPageFreedList`.
/// * The storage must not be moved, copied, or reused until `rust_scoped_page_freed_list_destroy`
///   has returned.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_scoped_page_freed_list_init(list: &mut MaybeUninit<ScopedPageFreedList>) {
    // SAFETY: Caller guarantees `list` is valid uninitialized storage at a pinned address.
    unsafe {
        let Ok(()) = ScopedPageFreedList::new().__pinned_init(list.as_mut_ptr());
    }
}

/// # Safety
///
/// * `list` must point to an initialized `ScopedPageFreedList` that will not be accessed again.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_scoped_page_freed_list_destroy(list: *mut ScopedPageFreedList) {
    // SAFETY: Caller guarantees `list` is initialized and not used afterwards.
    unsafe {
        ptr::drop_in_place(list);
    }
}

/// # Safety
///
/// * `list` must point to a valid pinned `ScopedPageFreedList`.
/// * `cow_pages` must be the `VmCowPages` that owns every page in `list` and its loaned-page
///   holder, and every page in `list` must already have been removed from the page queues.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_scoped_page_freed_list_free_pages(
    list: *mut ScopedPageFreedList,
    cow_pages: &VmCowPages,
) {
    // SAFETY: Caller guarantees `list` is a valid pinned `ScopedPageFreedList`.
    let list = unsafe { Pin::new_unchecked(&mut *list) };
    // SAFETY: Caller upholds `free_pages` preconditions.
    unsafe { list.free_pages(cow_pages) };
}

/// # Safety
///
/// * `list` must point to a valid pinned `ScopedPageFreedList`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_scoped_page_freed_list_list(
    list: *mut ScopedPageFreedList,
) -> *mut VmPageDoublyLinkedList {
    // SAFETY: In-bounds raw field projection of a live `ScopedPageFreedList` (caller); the result
    // keeps `list`'s provenance. `list` is `#[pin]`, so taking its address does not move it.
    unsafe { &raw mut (*list).list }
}

/// # Safety
///
/// * `list` must point to a valid pinned `ScopedPageFreedList`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_scoped_page_freed_list_flph(
    list: *mut ScopedPageFreedList,
) -> *mut FreeLoanedPagesHolder {
    // SAFETY: Caller guarantees `list` is a live pinned `ScopedPageFreedList`.
    unsafe { Pin::new_unchecked(&mut *list) }.flph_raw()
}

/// # Safety
///
/// * `ops` must point to valid, aligned, uninitialized storage for `DeferredOps`.
/// * The storage must not be moved, copied, or reused until
///   `rust_vm_cow_pages_deferred_ops_destroy` has returned, and `ops` must only be used from the
///   constructing thread.
/// * `cow` must point to a valid `VmCowPages` that outlives `ops`, and `cow.lock()` must not be
///   held.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_deferred_ops_init<'a>(
    ops: &mut MaybeUninit<DeferredOps<'a>>,
    cow: &'a VmCowPages,
) {
    // SAFETY: Caller guarantees `ops` is valid uninitialized storage at a pinned address.
    unsafe {
        let Ok(()) = DeferredOps::new(cow).__pinned_init(ops.as_mut_ptr());
    }
}

/// # Safety
///
/// * `ops` must point to an initialized `DeferredOps` that will not be accessed again, and the
///   lock of the `VmCowPages` it was constructed with must not be held (the destructor acquires
///   it).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_deferred_ops_destroy(ops: *mut DeferredOps<'_>) {
    // SAFETY: Caller guarantees `ops` is initialized and not used afterwards.
    unsafe {
        ptr::drop_in_place(ops);
    }
}

/// # Safety
///
/// * `ops` must point to a live, initialized `DeferredOps` that is not being accessed by any other
///   thread for the duration of the call.
/// * `cow` must be the `VmCowPages` `ops` was initialized with.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_deferred_ops_freed_list(
    ops: *mut DeferredOps<'_>,
    cow: &VmCowPages,
) -> *mut ScopedPageFreedList {
    // SAFETY: `ops` is a live `DeferredOps` (caller); `cow` is read through a raw place so no
    // reference to `*ops` is minted.
    debug_assert!(ptr::eq(unsafe { (*ops).cow }, cow));
    // SAFETY: In-bounds raw field projection of a live `DeferredOps`. The result carries the
    // provenance of `ops` itself (not of a transient `&mut`), so C++ may legitimately keep it
    // across later `rust_vm_cow_pages_deferred_ops_*` calls on the same object (e.g.
    // `BatchPQRemove`). `freed_list` is `#[pin]`, so exposing its address does not move it.
    unsafe { &raw mut (*ops).freed_list }
}

/// # Safety
///
/// * `ops` must point to a live, initialized `DeferredOps`.
/// * No mutable reference to `*ops` may be live or created concurrently for the duration of the
///   call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_deferred_ops_self(
    ops: &DeferredOps<'_>,
) -> *const VmCowPages {
    ptr::from_ref(ops.cow)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `page` must be a valid `VmPage` in the `OBJECT` state owned by `this` and in the page
///   queues.
/// * `ops` must point to a valid pinned `DeferredOps` for `this`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_remove_page_locked(
    this: &VmCowPages,
    page: VmPagePtr,
    ops: *mut DeferredOps<'_>,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    // SAFETY: Caller guarantees `ops` is a valid pinned `DeferredOps`.
    let ops = unsafe { Pin::new_unchecked(&mut *ops) };
    // SAFETY: Caller upholds `remove_page_locked` safety preconditions.
    unsafe { this.remove_page_locked(&token, page, ops) };
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `deferred` must be null or point to a valid pinned `DeferredOps` for `this` with exclusive
///   access for the duration of the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_range_change_update_locked(
    this: &VmCowPages,
    range: VmCowRange,
    op: RangeChangeOp,
    deferred: *mut DeferredOps<'_>,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    // SAFETY: `deferred` is either null or a valid pinned `DeferredOps` exclusively accessed by
    // the caller for this call, so `as_mut()` minting `&mut DeferredOps` is sound.
    let deferred = unsafe { deferred.as_mut().map(|d| Pin::new_unchecked(d)) };
    this.range_change_update_locked(&token, range, op, deferred);
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `paged` must point to a valid `VmObjectPaged` sharing `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_range_change_update_mappings_locked(
    this: &VmCowPages,
    paged: &VmObjectPaged,
    range: VmCowRange,
    op: RangeChangeOp,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.range_change_update_mappings_locked(&token, paged, range, op);
}

#[cfg(ktest)]
#[unittest::suite]
/// Tests for the Rust port of VmCowRange and the Rust `LookupCursor` wrapper.
mod vm_cow_pages_rs {
    use super::VmCowRange;
    use crate::kernel::types::PAddr;
    use crate::vm::page_source::MultiPageRequest;
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::mem::MaybeUninit;
    use pin_init::{stack_pin_init, stack_try_pin_init};
    use unittest::{assert_ok, expect_eq, expect_err, expect_false, expect_true, unwrap_ok};
    use zx_status::Status;

    /// Tests various methods of VmCowRange.
    #[test]
    fn vm_cow_range_test() {
        // construction/end/is_empty
        let r1 = VmCowRange::new(0, 0);
        expect_true!(r1.is_empty());
        expect_eq!(r1.end(), 0);

        let r2 = VmCowRange::new(4096, 8192);
        expect_false!(r2.is_empty());
        expect_eq!(r2.end(), 12288);
        expect_eq!(r2.offset + r2.len, r2.end());

        let r3 = VmCowRange::new(u64::MAX, 0);
        expect_true!(r3.is_empty());
        expect_eq!(r3.end(), u64::MAX);

        let r4 = VmCowRange::new(0, u64::MAX);
        expect_false!(r4.is_empty());
        expect_eq!(r4.end(), u64::MAX);

        // page alignment
        let page_size = ::page::SIZE as u64;

        let r_aligned = VmCowRange::new(page_size, page_size * 2);
        expect_true!(r_aligned.is_page_aligned());

        let r_unaligned_len = VmCowRange::new(page_size, page_size * 2 - 1);
        expect_false!(r_unaligned_len.is_page_aligned());
        let r_unaligned_len_expanded = r_unaligned_len.expand_till_page_aligned();
        expect_true!(r_unaligned_len_expanded.is_page_aligned());
        expect_eq!(r_unaligned_len_expanded.offset, page_size);
        expect_eq!(r_unaligned_len_expanded.len, page_size * 2);

        let r_unaligned_offset = VmCowRange::new(page_size + 1, page_size);
        expect_false!(r_unaligned_offset.is_page_aligned());
        let r_unaligned_offset_expanded = r_unaligned_offset.expand_till_page_aligned();
        expect_true!(r_unaligned_offset_expanded.is_page_aligned());
        expect_eq!(r_unaligned_offset_expanded.offset, page_size);
        expect_eq!(r_unaligned_offset_expanded.len, page_size * 2);

        // offset_by/trimmed_from_start/with_length
        let r_arith = VmCowRange::new(4096, 8192);

        let r_offset = r_arith.offset_by(4096);
        expect_eq!(r_offset.offset, 8192);
        expect_eq!(r_offset.len, 8192);

        let r_trimmed = r_arith.trimmed_from_start(4096);
        expect_eq!(r_trimmed.offset, 8192);
        expect_eq!(r_trimmed.len, 4096);

        let r_length = r_arith.with_length(4096);
        expect_eq!(r_length.offset, 4096);
        expect_eq!(r_length.len, 4096);

        // cover
        let cover_base = VmCowRange::new(4096, 8192);
        let cover_next = VmCowRange::new(8192, 8192);
        let cover1 = cover_base.cover(cover_next);
        expect_eq!(cover1.offset, 4096);
        expect_eq!(cover1.len, 12288);

        let disjoint1 = VmCowRange::new(4096, 4096);
        let disjoint2 = VmCowRange::new(16384, 4096);
        let cover2 = disjoint1.cover(disjoint2);
        expect_eq!(cover2.offset, 4096);
        expect_eq!(cover2.len, 16384);

        let empty_range = VmCowRange::new(0, 0);
        let cover_empty1 = cover_base.cover(empty_range);
        expect_eq!(cover_empty1.offset, cover_base.offset);
        expect_eq!(cover_empty1.len, cover_base.len);

        let cover_empty2 = empty_range.cover(cover_base);
        expect_eq!(cover_empty2.offset, cover_base.offset);
        expect_eq!(cover_empty2.len, cover_base.len);

        // is_bounded_by incl. the overflow case
        let r_bounded = VmCowRange::new(4096, 8192);
        expect_true!(r_bounded.is_bounded_by(16384));
        expect_true!(r_bounded.is_bounded_by(12288));
        expect_false!(r_bounded.is_bounded_by(12287));
        expect_false!(r_bounded.is_bounded_by(4096));

        let overflow_r = VmCowRange::new(u64::MAX - 4095, 8192);
        expect_false!(overflow_r.is_bounded_by(u64::MAX));
    }

    /// Tests `LookupCursor`.
    #[test]
    fn lookup_cursor_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        let page_size = ::page::SIZE as u64;
        let vmo_size = 3 * page_size;
        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, vmo_size));
        assert_ok!(vmo.commit_range(0, page_size));

        {
            stack_pin_init!(let deferred = vmo.make_deferred_ops());
            stack_pin_init!(let page_req = MultiPageRequest::new());
            ksync::lock!(let mut guard = vmo.lock());

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `vmo_size` and `page_size` are
                    // page-aligned, and `page_size` is non-zero.
                    let bad = unsafe {
                        vmo.get_lookup_cursor_locked(
                            guard.as_mut().token_mut(),
                            vmo_size,
                            page_size,
                        )
                    }
                );
                expect_err!(bad, Status::OUT_OF_RANGE);
            }

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `0` and `vmo_size` are page-aligned, and
                    // `vmo_size` is non-zero.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(guard.as_mut().token_mut(), 0, vmo_size)
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                expect_true!(cursor.lock_is_held());
                cursor.as_mut().disable_mark_accessed();

                // SAFETY: `paddrs` is empty, so 0 pages are requested within the 3-page window.
                expect_eq!(unsafe { cursor.as_mut().if_exist_pages(true, &mut []) }.len(), 0);

                let mut paddrs = [MaybeUninit::<PAddr>::uninit(); 3];
                // SAFETY: `paddrs.len() == 3` fits in the 3 remaining pages of `cursor`.
                let found = unsafe { cursor.as_mut().if_exist_pages(true, &mut paddrs) };
                expect_eq!(found.len(), 1);

                // SAFETY: 2 pages remain in `cursor`'s 3-page window after `found.len() == 1`.
                let skipped = unsafe { cursor.as_mut().skip_missing_pages() };
                expect_eq!(skipped, 2);
            }

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `0` and `page_size` are page-aligned,
                    // and `page_size` is non-zero.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(guard.as_mut().token_mut(), 0, page_size)
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                cursor.as_mut().disable_mark_accessed();
                // SAFETY: 1 page remains in `cursor`, and page 0 was committed above.
                expect_true!(unsafe { cursor.as_mut().maybe_page(false) }.is_some());
            }

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `page_size` and `2 * page_size` are
                    // page-aligned, and `2 * page_size` is non-zero.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(
                            guard.as_mut().token_mut(),
                            page_size,
                            2 * page_size,
                        )
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                // SAFETY: 2 pages remain in `cursor`.
                expect_true!(unsafe { cursor.as_mut().maybe_page(false) }.is_none());

                // SAFETY: `deferred` belongs to `vmo`, `1` is non-zero and fits in the 1 remaining
                // page of `cursor`, and `page_req` has no active request.
                let owned = unwrap_ok!(unsafe {
                    cursor.as_mut().require_owned_page(
                        true,
                        1,
                        deferred.as_mut(),
                        page_req.as_mut(),
                    )
                });
                expect_true!(owned.writable);
            }
            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `page_size` is page-aligned, and
                    // `page_size` is non-zero.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(
                            guard.as_mut().token_mut(),
                            page_size,
                            page_size,
                        )
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                // SAFETY: `deferred` belongs to `vmo`, `1` is non-zero and fits in the 1 remaining
                // page of `cursor`, and `page_req` has no active request.
                let readable = unwrap_ok!(unsafe {
                    cursor.as_mut().require_page(false, 1, deferred.as_mut(), page_req.as_mut())
                });
                expect_false!(readable.writable);
            }
        }
    }
}
