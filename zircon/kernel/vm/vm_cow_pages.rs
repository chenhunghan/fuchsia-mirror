// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::anonymous_page_request::AnonymousPageRequest;
use super::arch_vm_aspace::ARCH_MMU_FLAG_CACHED;
use super::attribution::{AttributionCounts, FractionalBytes};
use super::continuous_attribution_tracker::AttributionTracker;
use super::page::object::MAX_PIN_COUNT;
use super::page::{VmPageDoublyLinkedList, VmPagePtr};
use super::page_queues::PageQueues;
use super::page_source::{
    LazyPageRequest, MultiPageRequest, PageRequest, PageSource, PagedVmoMutexClass, VmoDebugInfo,
};
use super::page_state::VmPageState;
use super::physmap::{paddr_to_physmap, paddr_to_physmap_mut, paddr_to_physmap_ptr};
use super::pmm::{self, ALLOC_FLAG_CAN_WAIT, page_queues as pmm_page_queues};
use super::vm::{vm_get_zero_page, vm_get_zero_page_paddr};
pub use super::vm_object::RangeChangeOp;
use super::vm_object_paged::VmObjectPaged;
use super::vm_page_list::{
    IntervalHandling, ReferenceValue, VmPageList, VmPageListCursor, VmPageOrMarker,
    VmPageOrMarkerRef, VmPageSpliceList, ZeroRangeDirtyState,
};
use crate::arch_rs::cache::arch_clean_invalidate_cache_range;
use crate::arch_rs::ops::zero_page as arch_zero_page;
use crate::kernel::thread::ScopedMemoryStall;
use crate::kernel::types::PAddr;
use crate::ksync_kernel::{CriticalMutexOrderedPolicy, TrackedCriticalMutex, TrackedMutex};
use crate::page_cache::{PageCache, PageList};
use crate::vm::compression::VmCompression;
use crate::vm::compressor::VmCompressor;
use crate::vm::discardable_vmo_tracker::{DiscardableState, DiscardableVmoTracker};
use crate::vm::pmm_node::{FreeLoanedPagesHolder, PmmOptDelayReuse};
use bitflags::bitflags;
use boot_options::BootOptions;
use core::cell::Cell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{self, MaybeUninit};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};
use core::{cmp, debug_assert_matches};
use debug::{dprintf, ltracef};
use fbl::{
    Canary, DoublyLinkedListContainable, HasRefCount, HasRefCountUpgradeable, Recyclable,
    RefCounted, RefPtr,
};
use kalloc::AllocError;
use ksync::{Adoptable, KCell, KMutex, KMutexGuard, LockClass, LockToken};
use lazy_init::LazyInit;
use pin_init::{
    PinInit, pin_data, pin_init, pin_init_from_closure, stack_pin_init, stack_try_pin_init,
};
use vm_constants_rs as constants;
use vm_cow_pages_bindings as bindings;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::{
    ZX_CACHE_POLICY_CACHED, ZX_CACHE_POLICY_MASK, ZX_KOID_INVALID, ZX_PAGER_VMO_STATS_MODIFIED,
    zx_koid_t, zx_paddr_t, zx_pager_vmo_stats_t, zx_status_t, zx_vmo_lock_state_t,
};

const LOCAL_TRACE: u32 = 0;

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

/// Guard holding a [`PageSource`]'s `paged_vmo_lock()` on behalf of a [`DeferredPageSourceLock`].
/// `'static` because the mutex is kept alive by the `RefPtr<PageSource>` stored alongside the
/// guard rather than by a borrow.
type PageSourceLockGuard = KMutexGuard<'static, PagedVmoMutexClass, TrackedMutex>;

/// Holder of the page source lock (`KMutexGuard` + `RefPtr<PageSource>`) acquired for the
/// lifetime of a [`DeferredOps`]; see the `page_source_lock` field for why it is held.
#[pin_data]
#[repr(C)]
struct DeferredPageSourceLock {
    // Safety invariant: `guard` holds `source`'s `paged_vmo_lock()` if `source` is `Some`, and is
    // empty otherwise. `source` keeps the `PageSource` (and so the mutex `guard` points at) alive
    // until after `guard` has released the lock.
    //
    // The declaration order of these two fields is load-bearing. Fields are dropped in declaration
    // order, and as we could be holding the last reference to the object the mutex must be
    // released first (`guard`), prior to potentially destroying the object by releasing the
    // refptr (`source`).
    #[pin]
    guard: PageSourceLockGuard,
    source: Option<RefPtr<PageSource>>,
}

impl DeferredPageSourceLock {
    /// Walks up from `cow`, whose hierarchy has a page source, to the root of the hierarchy to
    /// obtain the `RefPtr<PageSource>` whose lock serializes operations on the hierarchy. Returns
    /// `None` if `cow` is no longer alive. `cow`'s lock must not be held.
    fn find_root_page_source(cow: &VmCowPages) -> Option<RefPtr<PageSource>> {
        debug_assert!(cow.root_has_page_source());
        ksync::lock!(let guard = cow
            .lock_ref()
            .lock_policy_with::<CriticalMutexOrderedPolicy>(cow.lock_order() as usize));
        let guard_tok = guard.token();
        if cow.life_cycle_locked(guard_tok) != LifeCycle::Alive {
            core::hint::cold_path();
            // Although the object is guaranteed to be valid by the caller, it's possible that the
            // VMO has transitioned into a dead state. This race can occur typically due to
            // reclamation having to first acquire a RefPtr, then acquire the lock, then check if
            // the page is still present in the VMO. If the VMO has transitioned to dead then its
            // pages will have been cleared, and so the operation will get skipped.
            // Unfortunately at this point the main lock acquisition and check has not been
            // performed. This is a problem since when dead transitioning the parent reference is
            // cleared, meaning we will find a 'fake' root, that will consequently not have a valid
            // page_source. So to avoid failing to find a root page_source we make sure to terminate
            // if this object is dead.
            // As it is dead and no longer connected to the tree, there is no rest of the hierarchy
            // to synchronize with and so failing to acquire the lock is safe.
            return None;
        }
        ksync::lock!(let current = LockedPtr::empty());
        while current.as_mut().step_to_parent(cow, guard_tok) {}
        let (root, _tok) = current.locked_or(cow, guard_tok);
        let source = root.state().page_source.clone();
        debug_assert!(source.is_some());
        source
    }

    /// Acquires the page source lock for `cow`'s hierarchy (if any) in place. `cow`'s lock must
    /// not be held.
    #[inline]
    fn new(cow: &VmCowPages) -> impl PinInit<Self> + '_ {
        let init = move |slot: *mut Self| -> Result<(), Infallible> {
            // If we are referencing a pager backed object then we must acquire the pager hierarchy
            // lock, which requires walking up to the root to find the page_source. That walk is
            // kept out of line so that the anonymous case, which has no lock to acquire, can be
            // initialized inline by the caller.
            if cow.root_has_page_source() {
                // SAFETY: `slot` points to valid, pinned, uninitialized storage for `Self`.
                unsafe { Self::init_pager_backed(cow, slot) };
                return Ok(());
            }
            // SAFETY: `slot` points to valid, pinned, uninitialized storage for `Self`, so
            // `guard_slot` is valid, pinned, uninitialized storage for its `guard` field.
            let guard_slot = unsafe { &raw mut (*slot).guard };
            // SAFETY: `guard_slot` is valid per above.
            let Ok(()) = unsafe { PageSourceLockGuard::empty().__pinned_init(guard_slot) };
            // SAFETY: `slot` is valid for writing its `source` field.
            unsafe { (&raw mut (*slot).source).write(None) };
            Ok(())
        };
        // SAFETY: `init` initializes both `guard` and `source` in place before returning `Ok(())`,
        // on either path, and `guard` holds the lock if and only if `source` is `Some`,
        // establishing the struct's safety invariant.
        unsafe { pin_init_from_closure(init) }
    }

    /// Initializes `slot` for a `cow` whose hierarchy has a page source: finds the root page
    /// source and acquires its lock. `cow`'s lock must not be held.
    ///
    /// # Safety
    ///
    /// `slot` must point to valid, pinned, uninitialized storage for `Self`. Both fields are
    /// initialized when this returns.
    #[inline(never)]
    unsafe fn init_pager_backed(cow: &VmCowPages, slot: *mut Self) {
        let source = Self::find_root_page_source(cow);
        // SAFETY: Per caller `slot` points to valid, pinned, uninitialized storage for `Self`, so
        // `guard_slot` is valid, pinned, uninitialized storage for its `guard` field.
        let guard_slot = unsafe { &raw mut (*slot).guard };
        let Ok(()) = match &source {
            // SAFETY: `guard_slot` is valid per above. The mutex is a subobject of `*ps`, which
            // `source` (stored alongside `guard`) keeps alive until after `guard` has been
            // dropped, and nothing borrowed for the guard's `'static` lifetime escapes
            // this struct. The caller does not hold `cow`'s lock, and `find_root_page_source`
            // released `cow`'s lock and every ancestor lock before returning, so this thread may
            // block on the mutex and holds none of this hierarchy's locks while acquiring it, as
            // the `paged_vmo_lock` lock ordering requires. Not holding any other `VmCowPages` lock
            // is a requirement on the caller of `DeferredOps::new`.
            Some(ps) => unsafe {
                PageSourceLockGuard::from_raw(ptr::from_ref(ps.paged_vmo_lock()))
                    .__pinned_init(guard_slot)
            },
            // SAFETY: `guard_slot` is valid per above.
            None => unsafe { PageSourceLockGuard::empty().__pinned_init(guard_slot) },
        };
        // SAFETY: Per caller `slot` is valid for writing its `source` field.
        unsafe { (&raw mut (*slot).source).write(source) };
    }
}

/// Pinned guard holding a `VmCowPages` lock on behalf of a [`LockedPtr`], mirroring the
/// `Guard<CriticalMutex>::Adoptable` member of the C++ `LockedPtr`.
type CowLockGuard<'a> =
    Adoptable<'a, VmCowPagesLockClass, TrackedCriticalMutex, CriticalMutexOrderedPolicy>;

/// Helper struct for managing a locked `VmCowPages` referenced by a pointer. This helper makes it
/// easy to pass around references to locked objects while retaining static lock-token enforcement.
///
/// `'a` bounds the borrow of the `VmCowPages` this `LockedPtr` was created from; for a parent
/// acquired through a hierarchy walk `'a` is unconstrained, and validity comes from the lock hold
/// alone (see the safety invariant). References and `LockToken`s handed out by a `LockedPtr` are
/// bounded by the borrow of the `LockedPtr` itself, so they can never outlive the lock hold.
///
/// The C++ `LockedPtr` is movable because `Guard::Adoptable`'s move constructor relocates the
/// lockdep `AcquiredLockEntry`; Rust moves are bitwise, so a `LockedPtr` is pinned instead
/// ([`Adoptable`] is `!Unpin`): construct it in place with [`ksync::lock!`] (which, unlike
/// `stack_pin_init!`, keeps the acquisition inlined) or as a `#[pin]` field, and transfer a held
/// lock between slots with [`LockedPtr::move_from`], the equivalent of C++
/// `LockedPtr::operator=(LockedPtr&&)`.
#[pin_data]
struct LockedPtr<'a> {
    // Underlying object pointer and lock. The invariant that this struct maintains is that if
    // `ptr` is `None`, then `guard` is empty, otherwise if `ptr` is `Some` then `guard` holds the
    // adoptable lock acquisition of that object.
    //
    // Safety invariant: while `guard` holds the lock, the object `ptr` points to remains valid: a
    // `VmCowPages` cannot be unlinked from its hierarchy (`DeadTransitionLocked`,
    // `MergeContentWithChildLocked`, `RemoveChildLocked`) and so cannot lose its last `RefPtr`
    // without its own lock being held, and code that does hold that lock must not drop the last
    // `RefPtr` while a `LockedPtr` holds it (see `set_parent_locked` and `take_parent_locked`).
    // Nothing else keeps the object alive: another thread may destroy it as soon as the lock is
    // released, so `ptr` is never used after `guard` has released the lock. `release` and
    // `move_from` clear it first; when a `LockedPtr` is dropped nothing can read `ptr` again.
    #[pin]
    guard: CowLockGuard<'a>,
    ptr: Option<NonNull<VmCowPages>>,
}

impl<'a> LockedPtr<'a> {
    /// Creates an empty `LockedPtr` that holds no lock.
    #[inline]
    fn empty() -> impl PinInit<Self> {
        pin_init!(Self { guard <- CowLockGuard::empty(), ptr: None })
    }

    /// Acquires `cow`'s lock using `cow.lock_order()`. `cow`'s lock must not already be held.
    #[inline]
    fn new(cow: &'a VmCowPages) -> impl PinInit<Self> {
        Self::with_order(cow, cow.lock_order())
    }

    /// Acquires `cow`'s lock with an explicit `lock_order`. `cow`'s lock must not already be held.
    #[inline]
    fn with_order(cow: &'a VmCowPages, lock_order: u64) -> impl PinInit<Self> {
        // SAFETY: `cow` is borrowed for `'a`, so `cow` is valid during acquisition and for the
        // whole lock hold. The lock is acquired as a `lock_policy_with` call on `cow.lock_ref()`
        // would acquire it, with the same requirements on whoever runs the initializer.
        unsafe { Self::acquire_raw(NonNull::from(cow), lock_order) }
    }

    /// Returns an initializer that acquires `cow`'s lock with `lock_order` when run.
    ///
    /// # Safety
    ///
    /// * `cow` must point to a valid `VmCowPages` when this is called (it is accessed immediately
    ///   to locate the lock) and until the initializer has run, and the pointee must then remain
    ///   valid for as long as the initialized `LockedPtr` (or any `LockedPtr` the lock is moved
    ///   into) holds its lock.
    /// * When the initializer runs, the acquisition preconditions of `CriticalMutexOrderedPolicy`
    ///   must hold: the thread is in a blockable context with no spinlocks held and no active
    ///   `ChainLockTransaction`, it does not already hold `cow`'s lock, and (under lockdep) every
    ///   `VmCowPages` lock it holds has a lock order below `lock_order`.
    #[inline]
    unsafe fn acquire_raw(cow: NonNull<VmCowPages>, lock_order: u64) -> impl PinInit<Self> {
        // SAFETY: `cow` is valid per caller.
        let lock = ptr::from_ref(unsafe { cow.as_ref() }.lock_ref());
        pin_init!(Self {
            // SAFETY: `lock` is a subobject of `*cow`, which per caller remains valid for the
            // duration of the lock hold (if the guard is forgotten without dropping, the lock
            // remains held forever so `*cow` is never destroyed). No reference or `LockToken`
            // with lifetime `'a` ever escapes `LockedPtr`. The policy's acquisition preconditions
            // hold when this runs, also per caller.
            guard <- unsafe { CowLockGuard::from_raw_with_args(lock, lock_order as usize) },
            ptr: Some(cow),
        })
    }

    /// Runs `init` if there is one, and otherwise creates an empty `LockedPtr`. This initializes
    /// a `LockedPtr` in place either way, without a temporary to move the lock out of.
    #[inline]
    fn init_or_empty(init: Option<impl PinInit<Self>>) -> impl PinInit<Self> {
        let init = move |slot: *mut Self| -> Result<(), Infallible> {
            match init {
                // SAFETY: `slot` is valid, pinned, uninitialized storage for `Self`, which is
                // what `init` requires.
                Some(init) => unsafe { init.__pinned_init(slot) },
                // SAFETY: As above.
                None => unsafe { Self::empty().__pinned_init(slot) },
            }
        };
        // SAFETY: Both arms fully initialize `slot` in place (neither can fail), using
        // initializers that uphold the struct's invariants.
        unsafe { pin_init_from_closure(init) }
    }

    /// Releases any lock held by `self`, then moves the lock held by `other` (if any) into `self`,
    /// leaving `other` empty. Mirrors C++ `LockedPtr::operator=(LockedPtr&&)`.
    #[inline]
    fn move_from(self: Pin<&mut Self>, other: Pin<&mut Self>) {
        let this = self.project();
        let other = other.project();
        // Whatever ptr and guard are they come from a LockedPtr that is assumed to be valid, and
        // so assigning them into ourselves is assumed to be valid and maintain our lock invariant.
        // Both pointers are cleared before the guards change so that a `Some` pointer always has
        // its lock held.
        let ptr = other.ptr.take();
        *this.ptr = None;
        this.guard.move_from(other.guard);
        *this.ptr = ptr;
    }

    /// Steps `self` up to the parent of the node it holds (or of `cow`, whose lock is proven held
    /// by `cow_token`, if `self` is empty): acquires the parent's lock, then releases the lock
    /// `self` held. Returns `false`, leaving `self` unchanged, if there is no parent.
    #[inline(always)]
    fn step_to_parent(
        self: Pin<&mut Self>,
        cow: &'a VmCowPages,
        cow_token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        let (cur, tok) = self.locked_or(cow, cow_token);
        // SAFETY: Nothing here releases a reference to `cur`'s parent, and holding a `LockedPtr`
        // gives no safe way to do so afterwards: a parent link can only be dropped through the
        // `unsafe` `set_parent_locked` and `take_parent_locked`, whose contracts forbid releasing
        // the last reference while a `LockedPtr` holds that parent's lock.
        let Some(parent) = (unsafe { cur.parent_lock_init_locked(tok) }) else {
            return false;
        };
        // `cur`'s lock is still held by `self` (or `cow_token`) while `parent` acquires the
        // parent's lock, as `parent_lock_init_locked` requires.
        ksync::lock!(let parent = parent);
        self.move_from(parent);
        true
    }

    /// Provides locked access to the underlying `VmCowPages` and its `LockToken`.
    ///
    /// Must not be empty.
    #[inline]
    fn locked<'s>(&'s self) -> (&'s VmCowPages, &'s LockToken<'s, VmCowPagesLockClass>) {
        let ptr = self.ptr.expect("LockedPtr::locked called on empty LockedPtr");
        // SAFETY: Per the struct safety invariant, `guard` holds a lock (so `token_unchecked` is
        // allowed) and `ptr` is valid for as long as it does, which is at least the borrow `'s` of
        // `self`. `LockToken` is covariant in its lifetime parameter, so the token shrinks from
        // `'a` to `'s`.
        unsafe { (ptr.as_ref(), self.guard.token_unchecked()) }
    }

    /// Provides locked access to the underlying `VmCowPages` and an exclusive `LockToken`.
    ///
    /// Must not be empty.
    #[inline]
    fn locked_mut<'s>(
        self: Pin<&'s mut Self>,
    ) -> (&'s VmCowPages, &'s mut LockToken<'s, VmCowPagesLockClass>) {
        let this = self.project();
        let ptr = this.ptr.expect("LockedPtr::locked_mut called on empty LockedPtr");
        // SAFETY: Per the struct safety invariant, `guard` holds a lock (so `token_mut_unchecked`
        // is allowed) and `ptr` is valid for as long as it does, which is at least the borrow `'s`
        // of `self`.
        unsafe { (ptr.as_ref(), this.guard.token_mut_unchecked().reborrow()) }
    }

    /// Returns a shared `LockToken` for the held lock. Must not be empty.
    #[inline]
    fn locked_token<'s>(&'s self) -> &'s LockToken<'s, VmCowPagesLockClass> {
        self.locked().1
    }

    /// Provides locked access to the underlying `VmCowPages`, or if empty, to `cow`.
    #[inline]
    fn locked_or<'s>(
        &'s self,
        cow: &'s VmCowPages,
        cow_token: &'s LockToken<'_, VmCowPagesLockClass>,
    ) -> (&'s VmCowPages, &'s LockToken<'s, VmCowPagesLockClass>) {
        // `LockToken` is covariant in its lifetime, so `cow_token` shrinks to `'s` safely.
        if self.ptr.is_some() { self.locked() } else { (cow, cow_token) }
    }

    /// Releases the lock, leaving this `LockedPtr` empty.
    #[inline]
    fn release(self: Pin<&mut Self>) {
        let this = self.project();
        *this.ptr = None;
        this.guard.release();
    }

    /// Returns the locked `VmCowPages`, if any. The reference cannot outlive the borrow of `self`,
    /// and so cannot outlive the lock hold.
    #[inline]
    fn get(&self) -> Option<&VmCowPages> {
        // SAFETY: Per the struct safety invariant, `ptr` is valid while `guard` holds its lock,
        // which is for at least the borrow of `self`.
        self.ptr.as_ref().map(|p| unsafe { p.as_ref() })
    }

    #[inline]
    fn is_some(&self) -> bool {
        self.ptr.is_some()
    }

    #[inline]
    fn is_none(&self) -> bool {
        self.ptr.is_none()
    }
}

/// Helper for walking up a VmCowPages hierarchy where the start node is locked, and the immediate
/// parent may or may not be locked.
///
/// Pinned, as it holds a [`LockedPtr`]; construct it in place with [`ksync::lock!`] (see
/// [`LockedPtr`]).
#[pin_data]
struct LockedParentWalker<'w, 'a> {
    /// Tracks whether a call to `current` should return the `pre_locked_parent`, or the normal
    /// `current` tracker.
    current_is_pre_locked_parent: bool,
    pre_locked_parent: Option<(&'w VmCowPages, &'w LockToken<'w, VmCowPagesLockClass>)>,
    #[pin]
    current: LockedPtr<'a>,
}

impl<'w, 'a> LockedParentWalker<'w, 'a> {
    /// Construct the parent walker with the immediate parent and a token for its lock, if the
    /// caller has locked it. This can be `None` if the immediate parent is either not locked, or
    /// does not exist.
    #[inline]
    fn new(
        maybe_locked_parent: Option<(&'w VmCowPages, &'w LockToken<'w, VmCowPagesLockClass>)>,
    ) -> impl PinInit<Self> {
        pin_init!(Self {
            current_is_pre_locked_parent: false,
            pre_locked_parent: maybe_locked_parent,
            current <- LockedPtr::empty(),
        })
    }

    /// Returns a locked reference to the current node in the walk, which is `cow` (whose lock is
    /// proven held by `cow_token`) until the walker first walks up.
    #[inline]
    fn current<'s>(
        &'s self,
        cow: &'s VmCowPages,
        cow_token: &'s LockToken<'_, VmCowPagesLockClass>,
    ) -> (&'s VmCowPages, &'s LockToken<'s, VmCowPagesLockClass>) {
        // `walk_up` only sets `current_is_pre_locked_parent` after matching `pre_locked_parent`.
        if let Some(pre_locked_parent) = self.pre_locked_parent
            && self.current_is_pre_locked_parent
        {
            return pre_locked_parent;
        }
        self.current.locked_or(cow, cow_token)
    }

    /// Resets the walker to its initial state, allowing for a new walk.
    #[inline]
    fn reset(self: Pin<&mut Self>) {
        let this = self.project();
        this.current.release();
        *this.current_is_pre_locked_parent = false;
    }

    /// Walk up the hierarchy, changing the current node to the current nodes parent. It is an
    /// error to call this if current has no parent.
    // Kept out of line: this runs once per ancestor.
    #[inline(never)]
    fn walk_up(
        self: Pin<&mut Self>,
        cow: &VmCowPages,
        cow_token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        let (cur, cur_token) = self.as_ref().get_ref().current(cow, cow_token);
        let next = cur.parent_locked(cur_token);
        debug_assert!(next.is_some());
        // If the next node in the chain matches the pre locked parent, then use that, otherwise
        // move current up and acquire the lock.
        if let Some(next) = next
            && self.pre_locked_parent.is_some_and(|(pre_locked, _)| ptr::eq(next, pre_locked))
        {
            // Double check that the pre_locked_parent is actually the immediate parent.
            debug_assert!(cow.parent_locked(cow_token).is_some_and(|p| ptr::eq(p, next)));
            *self.project().current_is_pre_locked_parent = true;
        } else {
            // Acquire the parent's lock while `cur`'s is still held, then move it into `current`
            // (releasing `current`'s previous lock, if any).
            // SAFETY: The walk never releases a reference to a parent, and gives its users no
            // safe way to do so (see `LockedPtr::step_to_parent`).
            let next_init = unsafe { cur.parent_lock_init_locked(cur_token) };
            ksync::lock!(let next_locked = next_init.expect("walk_up requires a parent"));
            let this = self.project();
            *this.current_is_pre_locked_parent = false;
            this.current.move_from(next_locked);
        }
    }
}

/// Selects the entry type (the `P` template parameter of the C++
/// `ForEveryOwnedHierarchyPageInRange`) handed to the callbacks of
/// `VmCowPages::for_every_owned_hierarchy_page_in_range`, and the matching page list traversal.
trait OwnedHierarchyWalk {
    /// The entry type handed to the callback for each slot.
    type Entry<'s>: Deref<Target = VmPageOrMarker>;

    /// Walks `[start_offset, end_offset)` of `cow`'s page list, calling `per_page_fn` for every
    /// populated slot and `per_gap_fn` for every gap.
    ///
    /// # Safety
    ///
    /// `token` must be for the lock of `cow`: a `LockToken` only proves that some `VmCowPages`
    /// lock is held.
    ///
    /// If `Self` mutates slots in `cow`'s page list (`MutEntry`):
    ///
    /// * `per_page_fn` and `per_gap_fn` must not access `cow.state().page_list` (other than through
    ///   the `Self::Entry` passed to `per_page_fn`).
    /// * No reference or cursor into `cow`'s page list may be live across the call (obtained before
    ///   it and used during or after it). The traversal holds a `&mut` to the page list, which the
    ///   shared `token` does not make exclusive, and it can remove nodes from the list.
    unsafe fn traverse<PageFunc, GapFunc>(
        cow: &VmCowPages,
        token: &LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        end_offset: u64,
        per_page_fn: PageFunc,
        per_gap_fn: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: for<'s> FnMut(Self::Entry<'s>, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status;
}

/// `P = const VmPageOrMarker*`: entries are constant and may not be modified in any way.
struct ConstEntry;

impl OwnedHierarchyWalk for ConstEntry {
    type Entry<'s> = &'s VmPageOrMarker;

    /// Traverses `cow`'s page list by shared reference.
    ///
    /// # Safety
    ///
    /// No additional preconditions beyond `OwnedHierarchyWalk::traverse`.
    #[inline]
    unsafe fn traverse<PageFunc, GapFunc>(
        cow: &VmCowPages,
        token: &LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        end_offset: u64,
        per_page_fn: PageFunc,
        per_gap_fn: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: for<'s> FnMut(Self::Entry<'s>, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        cow.page_list_locked(token).for_every_page_and_gap_in_range(
            start_offset,
            end_offset,
            per_page_fn,
            per_gap_fn,
        )
    }
}

/// Entries are `&mut VmPageOrMarker`: the callback may modify them or replace them with an empty
/// entry.
struct MutEntry;

impl OwnedHierarchyWalk for MutEntry {
    type Entry<'s> = &'s mut VmPageOrMarker;

    /// Traverses `cow`'s page list via `remove_pages_and_iterate_gaps`.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `cow`.
    /// * Neither `per_page_fn` nor `per_gap_fn` may access `cow.state().page_list` other than
    ///   through the `&mut VmPageOrMarker` passed to `per_page_fn`.
    /// * No reference or cursor into `cow`'s page list may be live across the call (obtained before
    ///   it and used during or after it).
    #[inline]
    unsafe fn traverse<PageFunc, GapFunc>(
        cow: &VmCowPages,
        token: &LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        end_offset: u64,
        per_page_fn: PageFunc,
        per_gap_fn: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: for<'s> FnMut(Self::Entry<'s>, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        // SAFETY: Contract of `page_list_locked_ptr_unchecked`: per `# Safety` `token` is for
        // `cow`'s lock, and it is borrowed for this whole call, which is as long as `list` is used.
        // Nothing else accesses the page list while `list` is live: per `# Safety` the callbacks
        // only reach it through the `&mut VmPageOrMarker` handed to `per_page_fn`, and no reference
        // or cursor into it from before the call is in use. The pointer is to the page list inside
        // `cow.state()`, so it is valid to dereference.
        let list = unsafe { &mut *cow.page_list_locked_ptr_unchecked(token) };
        list.remove_pages_and_iterate_gaps(start_offset, end_offset, per_page_fn, per_gap_fn)
    }
}

/// Result of `VmCowPages::find_page_content_locked`; see that method for the meaning of the
/// fields.
///
/// Pinned, as it holds a [`LockedPtr`]; construct it in place with [`ksync::lock!`] (see
/// [`LockedPtr`]) from the initializer returned by `find_page_content_locked` or
/// `find_initial_page_content_locked`, which acquire the owner's lock.
#[pin_data]
struct PageLookup<'a> {
    // Safety invariant: `cursor` may point into the page list of the node whose lock `owner` holds,
    // or, when `owner` is empty, of the node the lookup was started from, whose lock the caller
    // holds for `'a`. It is only valid while that lock is held, so `cursor` must not be used after
    // `owner` has released its lock or moved on to another node, and must not be moved out of the
    // `PageLookup` to outlive `owner`. For as long as `cursor`, or a pointer returned by its
    // `current_ptr()`, is used, the conditions of `VmPageListCursor::new` must hold for that page
    // list: no node may be inserted into or removed from it, and nothing may write to a node while
    // `current()`, `for_every_contiguous()`, or `VmPageList::for_every_page_in_cursor_range()`
    // holds a reference into it.
    cursor: VmPageListCursor<'a>,
    #[pin]
    owner: LockedPtr<'a>,
    owner_offset: u64,
    visible_end: u64,
}

impl<'a> PageLookup<'a> {
    /// Creates a lookup with no content and an empty `owner`, i.e. one that refers to
    /// `[owner_offset, visible_end)` of the `VmCowPages` it was looked up in.
    #[inline]
    fn empty(owner_offset: u64, visible_end: u64) -> impl PinInit<Self> {
        pin_init!(Self {
            cursor: VmPageListCursor::default(),
            owner <- LockedPtr::empty(),
            owner_offset,
            visible_end,
        })
    }

    /// Returns the current `VmPageOrMarker` pointed at by the lookup cursor, tied to the borrow of
    /// `self` so that `owner`'s lock remains held for the lifetime of the returned reference.
    #[inline]
    fn current(&self) -> Option<&VmPageOrMarker> {
        self.cursor.current()
    }

    /// Returns a reference to the `LockedPtr` holding the owning ancestor's lock (or an empty
    /// `LockedPtr` when `self` is the owner).
    #[inline]
    fn owner(&self) -> &LockedPtr<'a> {
        &self.owner
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
    #[inline]
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
    // Kept out of line: every `DeferredOps` owner (Rust callers and the C++ shell's
    // `rust_vm_cow_pages_deferred_ops_destroy`) would otherwise inline a copy of the range update
    // and page freeing below, which is not on any hot path.
    #[inline(never)]
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
        // `remove_page_locked` / `release_owned_pages_range_locked`, whose contracts require the
        // pages to have been removed from their page list and the page queues by `self_cow` or
        // another `VmCowPages` in its hierarchy.
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
    /// Target always exists. This is provided in the constructor and will always be non-null.
    target: &'a VmCowPages,

    /// The current offset in `target`. This will always be <= `end_offset` and is only allowed to
    /// increase. The validity of this range is checked prior to construction by
    /// `get_lookup_cursor_locked`.
    offset: u64,

    /// The offset in `target` at which the cursor ceases being valid. The `end_offset` itself will
    /// never be used as a valid offset. VMOs are designed such that the end of a VMO+1 will not
    /// overflow.
    end_offset: u64,

    /// Captures information about the cursor owner. The different fields can be interpreted as
    /// follows.
    ///
    /// `owner_info.cursor`:
    /// Cursor in the page list of the current `owner_info.owner` or `target`, depending on who
    /// owns the page. Is only valid if `is_valid` is true. This is used to efficiently pull
    /// contiguous pages in the owner and the `current_ptr()` value of it is cached in
    /// `owner_cursor`.
    ///
    /// `owner_info.owner`:
    /// Represents the current owner of `owner_cursor`/`owner_info.cursor`. Can hold a lock while
    /// `owner_info.cursor` is invalid to indicate a lack of content, although in this case the
    /// owner can also be assumed to be the root. If `owner_info.owner` is empty while `is_valid`
    /// is true, `target` is the owner of the cursor.
    ///
    /// `owner_info.owner_offset`:
    /// The `offset` normalized to the current owner. This is equal to `offset` when
    /// `target_is_owner()`.
    ///
    /// `owner_info.visible_end`:
    /// Tracks the offset in `target` at which the current `owner_info.cursor` becomes invalid.
    /// This range essentially means that no VMO between `target` and `owner_info.owner` had any
    /// content, and so the cursor in owner is free to walk contiguous pages up to this point. This
    /// does not mean that there is no content in the parent of the owner, and so even if
    /// `owner_info.visible_end` is not reached, if an empty slot is found the parent must then be
    /// checked. See `increment_cursor` for more details.
    #[pin]
    owner_info: PageLookup<'a>,

    /// This is a cache of `owner_info.cursor.current_ptr()`.
    // Safety invariant: `owner_cursor` is `Some` only while `is_valid` is true, and then points to
    // the slot at `owner_info.owner_offset` in the page list of the owner (`owner_info.owner`, or
    // `target` if that is empty), which stays live and is only modified through this cursor until
    // the cursor is invalidated: the owner's lock is held (by `owner_info.owner`, or for `target`
    // as proven by `token`), and nothing else modifies the page lists of `target` or of the owner
    // while the cursor is in use. The exception is a cursor that, per the contract of `new`, may
    // only be dropped because something else has modified them: its pointer may dangle, and is
    // never dereferenced. The pointer is derived from the slot's node without creating a reference
    // to the node, and no reference into the slot outlives the method that creates it.
    owner_cursor: Option<NonNull<VmPageOrMarker>>,

    /// Value of `target.page_source_type() == PageSourceType::UserPager` cached on creation as
    /// there is spare padding space to store it here, and needed to retrieve this value to
    /// initialize `zero_fork` anyway.
    target_directly_backed_by_user_pager: bool,

    /// Tracks whether zero forks should be tracked and placed in the corresponding page queue.
    /// This is initialized to true if it's legal to place pages in the zero fork queue, which
    /// requires that `target` not be pager backed.
    zero_fork: bool,

    /// Whether existing pages should be have their access time updated when they are returned.
    mark_accessed: bool,

    /// Whether the cursor is valid. The `owner_info` can only be used if `is_valid` is true,
    /// otherwise it needs to be computed with `establish_cursor()`.
    is_valid: bool,

    /// Optional allocation list that will be used for any page allocations.
    // Safety invariant: when `Some`, points to a live list that only this cursor accesses, that is
    // never moved again until it is dropped, and whose pages are `ALLOC`-state pages owned by
    // whoever gave the list (the contract of `give_alloc_list`).
    alloc_list: Option<NonNull<VmPageDoublyLinkedList>>,

    /// Proof that the lock of `target` is held, which it is for `'a`: the token the cursor was
    /// created with is exclusively borrowed for `'a`.
    token: LockToken<'a, VmCowPagesLockClass>,
    phantom: PhantomData<&'a mut LockToken<'a, VmCowPagesLockClass>>,
}

::zr::static_assert!(
    core::mem::size_of::<LookupCursor<'_>>()
        == core::mem::size_of::<bindings::VmCowPages_LookupCursor>()
);
::zr::static_assert!(
    core::mem::align_of::<LookupCursor<'_>>()
        == core::mem::align_of::<bindings::VmCowPages_LookupCursor>()
);

#[pin_init::pinned_drop]
impl PinnedDrop for LookupCursor<'_> {
    #[inline]
    fn drop(self: Pin<&mut Self>) {
        // This impl exists only for the debug assertion below: any list given to `give_alloc_list`
        // must have been cleared with `clear_alloc_list` before the cursor is dropped. Dropping
        // the fields releases any owner lock held by `owner_info`; dropping a cursor never
        // dereferences `owner_cursor`.
        debug_assert!(self.alloc_list.is_none());
    }
}

/// Convenience struct holding the return result of the `require_*` methods.
#[derive(Debug, Copy, Clone)]
pub struct RequireResult {
    pub page: VmPagePtr,
    pub writable: bool,
}

impl<'a> LookupCursor<'a> {
    /// Creates a cursor over `range` of `target`. `range` must be page aligned, must not be empty
    /// and must be within `target`. Before it constructs a cursor,
    /// `VmCowPages::get_lookup_cursor_locked` checks that `range` is within `target`, and
    /// `debug_assert!`s that it is page aligned and not empty.
    ///
    /// # Safety
    ///
    /// * The lock of `target` must be held for `'a`.
    /// * While the cursor is used, that is from its first use until its last use other than being
    ///   dropped, nothing other than the cursor may change the page lists, parent links, parent
    ///   limits or sizes of `target` or of the owner the cursor has locked. Once something else has
    ///   changed one of them, the cursor may only be dropped.
    /// * `target` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * A method of the cursor may acquire the lock of each ancestor of `target` in turn (see
    ///   `establish_cursor_slow`), so whenever one is called the conditions of
    ///   [`LockedPtr::acquire_raw`] must hold for those acquisitions: the thread is in a blockable
    ///   context with no spinlocks held and no active `ChainLockTransaction`, the caller holds the
    ///   lock of none of those ancestors, and (under lockdep) every `VmCowPages` lock the caller
    ///   holds has a lock order below that of each of them.
    #[inline]
    unsafe fn new(target: &'a VmCowPages, range: VmCowRange) -> impl PinInit<Self> {
        debug_assert!(target.lock_is_held());
        let target_directly_backed_by_user_pager =
            target.page_source_type() == PageSourceType::UserPager;
        let zero_fork = !target_directly_backed_by_user_pager && target.can_decommit_zero_pages();
        pin_init!(Self {
            target,
            offset: range.offset,
            end_offset: range.end(),
            owner_info <- PageLookup::empty(0, 0),
            owner_cursor: None,
            target_directly_backed_by_user_pager,
            zero_fork,
            mark_accessed: true,
            is_valid: false,
            alloc_list: None,
            // SAFETY: Per `# Safety` the lock of `target` is held for `'a`.
            token: unsafe { LockToken::new() },
            phantom: PhantomData,
        })
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
    /// `max_request_pages` must be non-zero and at most the number of pages left in the range of
    /// the cursor, and `page_request` must not hold an active request.
    ///
    /// # Safety
    ///
    /// The caller must ensure `deferred` was made for the VMO this cursor was made for.
    pub unsafe fn require_owned_page(
        mut self: Pin<&mut Self>,
        will_write: bool,
        max_request_pages: u64,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> Result<RequireResult, Status> {
        // Make sure the cursor is valid.
        self.as_mut().establish_cursor();

        // Convert any references to pages.
        if self.cursor_is_reference() {
            // Decompress in place.
            // SAFETY: The reference is only used by the call below, which does not otherwise
            // access `page_request`.
            let anonymous = unsafe { page_request.get_anonymous() };
            // SAFETY: `cursor_is_reference` returned true.
            unsafe { self.as_mut().cursor_reference_to_page(anonymous) }?;
        }

        // If page exists in the target, i.e. the owner is the target, then we handle this case
        // separately as it's the only scenario where we might be dirtying an existing committed
        // page.
        if self.target_is_owner() && self.cursor_is_page() {
            // If we're writing to a root VMO backed by a user pager, we might need to mark pages
            // Dirty so that they can be written back later. This is the only path that can result
            // in a write to such a page; if the page was not present, we would have already blocked
            // on a read request the first time, and ended up here when unblocked, at which point
            // the page would be present.
            if will_write && self.target_directly_backed_by_user_pager {
                // SAFETY: `cursor_is_page` returned true.
                let mut page = unsafe { self.cursor_page() };
                // If this page was loaned, it should be replaced with a non-loaned page, so that we
                // can make progress with marking pages dirty. `prepare_for_write_locked` terminates
                // its page walk when it encounters a loaned page; loaned pages are reclaimed by
                // evicting them and we cannot evict dirty pages.
                // SAFETY: `page` is in the page list of `target`, whose lock is held.
                if unsafe { page.is_loaned() } {
                    debug_assert!({
                        // SAFETY: `page` is in the page list of `target`, whose lock is held.
                        unsafe { VmCowPages::is_page_clean(page) }
                    });
                    let this = self.as_mut().project();
                    let target: &VmCowPages = this.target;
                    // SAFETY: The reference is only used by the call below, which does not
                    // otherwise access `page_request`.
                    let anonymous = unsafe { page_request.get_anonymous() };
                    // SAFETY: `page` is the page at `offset` in `target`, whose lock `this.token`
                    // proves is held, no reference into `target`'s page list is live
                    // (`owner_cursor` is a raw pointer and `replace_page_locked` updates the slot
                    // at `offset` in place without otherwise modifying the page list), and per
                    // `# Safety` `deferred` was made for `target`.
                    let res_page = unsafe {
                        target.replace_page_locked(
                            this.token,
                            page,
                            *this.offset,
                            /*with_loaned=*/ false,
                            deferred.as_mut(),
                            Some(anonymous),
                        )
                    }?;
                    // Cursor should remain valid and have been replaced with the page.
                    debug_assert!(self.cursor_is_page());
                    debug_assert!({
                        // SAFETY: `cursor_is_page` was asserted above.
                        unsafe { self.cursor_page() == res_page }
                    });
                    debug_assert!({
                        // SAFETY: `res_page` is in the page list of `target`, whose lock is held.
                        unsafe { !res_page.is_loaned() }
                    });
                    page = res_page;
                }
                // If the page is not already dirty, then generate a dirty request. The dirty
                // request code can handle the page already being dirty, this is just a short
                // circuit optimization.
                // SAFETY: `page` is in the page list of `target`, whose lock is held.
                if !unsafe { VmCowPages::is_page_dirty(page) }
                    && let Err(status) =
                        self.dirty_request(max_request_pages, page_request.get_lazy_dirty_request())
                {
                    if status == Status::SHOULD_WAIT {
                        page_request.made_dirty_request();
                    }
                    return Err(status);
                }
            }
            // Return the page.
            // SAFETY: `cursor_is_page` returned true above, and neither replacing the page nor a
            // dirty request takes the page out of the slot.
            return Ok(unsafe { self.cursor_as_result() });
        }

        // Should there be page, but it not be owned by the target, then we are performing copy on
        // write into the target. As the target cannot have a page source do not need to worry about
        // writes or dirtying.
        if self.cursor_is_page() {
            debug_assert!(!self.target_is_owner());
            // SAFETY: `cursor_is_page` returned true.
            let page = unsafe { self.cursor_page() };
            // Although we are not returning the page, the act of forking counts as an access, and
            // this is an access regardless of whether the final returned page should be considered
            // accessed, so ignore the `mark_accessed` check here.
            // SAFETY: `page` is in the page list of the owner, whose lock this cursor holds.
            unsafe { pmm_page_queues().mark_accessed(page) };
            if !self.owner_info.owner.locked_or(self.target, &self.token).0.is_hidden() {
                // Directly copying the page from the owner into the target.
                // SAFETY: The reference is only used by the call below, which does not otherwise
                // access `page_request`.
                let anonymous = unsafe { page_request.get_anonymous() };
                // SAFETY: The cursor is valid, `page` is the page in its current slot, and per
                // `# Safety` `deferred` was made for `target`.
                return unsafe {
                    self.target_allocate_copy_page_as_result(
                        page,
                        DirtyState::Untracked,
                        deferred,
                        anonymous,
                    )
                };
            }
            let this = self.as_mut().project();
            let target: &VmCowPages = this.target;
            let owner_info = this.owner_info.project();
            let owner_offset = *owner_info.owner_offset;
            let (page_owner, owner_token) = owner_info.owner.locked_mut();
            // SAFETY: `this.alloc_list` is the `alloc_list` field of this cursor.
            let alloc_list = unsafe { Self::alloc_list_pin(this.alloc_list) };
            // SAFETY: The reference is only used by the call below, which does not otherwise
            // access `page_request`.
            let anonymous = unsafe { page_request.get_anonymous() };
            // SAFETY: `page` is the page in the slot at `owner_offset` of `page_owner`, whose lock
            // `owner_token` proves is held. `page_owner` is not `target`, which is not the owner
            // here: that case returned above. After a successful return the cursor is invalidated
            // below, so that no reference or cursor into the page list of `page_owner` is used
            // again. The allocation list satisfies the contract of `give_alloc_list`, and per
            // `# Safety` `deferred` was made for `target`. `page_owner` is a hidden node: the
            // branch for an owner that is not hidden returned above.
            let res_page = unsafe {
                target.clone_cow_page_locked(
                    this.token,
                    *this.offset,
                    alloc_list,
                    page_owner,
                    owner_token,
                    page,
                    owner_offset,
                    deferred,
                    anonymous,
                )
            }?;
            // Cloning the cow page may have impacted our cursor due to a page being moved so
            // invalidate the cursor to perform a fresh lookup on the next page requested.
            self.increment_offset_and_invalidate_cursor(::page::SIZE as u64);
            // This page as just allocated so no need to worry about update access times, can just
            // return.
            return Ok(RequireResult { page: res_page, writable: true });
        }

        // Zero content is the most complicated cases where, even if reading, dirty requests might
        // need to be performed and the resulting committed pages may / may not be dirty.
        if self.cursor_is_content_zero() {
            // If the page source is a PagerProxy, and is configured to trap dirty
            // transitions, we first need to generate a DIRTY request *before* the zero page
            // can be forked and marked dirty. If dirty transitions are not trapped, we will
            // fall through to allocate the page and then mark it dirty below.
            //
            // Note that the check for `should_trap_dirty_transitions()` is an optimization here.
            // `prepare_for_write_locked()` would do the right thing depending on
            // `should_trap_dirty_transitions()`, however we choose to avoid the extra work only to
            // have it be a no-op if dirty transitions should not be trapped.
            let target_page_dirty = self.target_zero_content_supply_dirty(will_write);
            if target_page_dirty
                && self.target.page_source().is_some_and(|ps| ps.should_trap_dirty_transitions())
            {
                let status =
                    self.dirty_request(max_request_pages, page_request.get_lazy_dirty_request());
                // Since we know we have a page source that traps, and page sources will never
                // succeed synchronously, our dirty request must have 'failed'.
                debug_assert!(status.is_err());
                let Err(status) = status else {
                    core::hint::cold_path();
                    panic!("dirty request of a trapping page source succeeded synchronously");
                };
                if status == Status::SHOULD_WAIT {
                    page_request.made_dirty_request();
                }
                return Err(status);
            }

            // If there is a marker in a hidden node not owned by the target and we will write, fork
            // the marker.
            if let Some(marker) = self.owner_cursor
                && self.cursor_is_marker()
                && !self.target_is_owner()
                && will_write
                && self.owner_info.owner.locked_or(self.target, &self.token).0.is_hidden()
            {
                let this = self.as_mut().project();
                let target: &VmCowPages = this.target;
                let owner_info = this.owner_info.project();
                let owner_offset = *owner_info.owner_offset;
                let (marker_owner, owner_token) = owner_info.owner.locked_mut();
                // SAFETY: `this.alloc_list` is the `alloc_list` field of this cursor.
                let alloc_list = unsafe { Self::alloc_list_pin(this.alloc_list) };

                // SAFETY: The reference is only used by the call below, which does not otherwise
                // access `page_request`.
                let anonymous = unsafe { page_request.get_anonymous() };
                // SAFETY: `marker` is `owner_cursor`, which per the safety invariant on that field
                // is the slot at `owner_offset` of `marker_owner`, whose lock `owner_token` proves
                // is held, and it holds a marker as `cursor_is_marker` returned true; no reference
                // to the slot is live. `marker_owner` is not `target`, which is not the owner
                // (checked above). After a successful return the cursor is invalidated below, so
                // that no reference or cursor into the page list of `marker_owner` is used again.
                // The allocation list satisfies the contract of `give_alloc_list`, and per
                // `# Safety` `deferred` was made for `target`.
                let res_page = unsafe {
                    target.fork_marker_locked(
                        this.token,
                        *this.offset,
                        alloc_list,
                        marker_owner,
                        owner_token,
                        marker,
                        owner_offset,
                        deferred,
                        anonymous,
                    )
                }?;

                self.increment_offset_and_invalidate_cursor(::page::SIZE as u64);
                // This page as just allocated so no need to worry about update access times, can
                // just return.
                return Ok(RequireResult { page: res_page, writable: true });
            }

            // Allocate the page and mark it dirty or clean as previously determined.
            // SAFETY: The reference is only used by the call below, which does not otherwise
            // access `page_request`.
            let anonymous = unsafe { page_request.get_anonymous() };
            // SAFETY: The cursor is valid, the source is the zero page, and per `# Safety`
            // `deferred` was made for `target`.
            return unsafe {
                self.target_allocate_copy_page_as_result(
                    vm_get_zero_page(),
                    if target_page_dirty { DirtyState::Dirty } else { DirtyState::Clean },
                    deferred,
                    anonymous,
                )
            };
        }
        debug_assert!(self.cursor_is_empty());

        // Generate a read request to populate the content in the owner. Even if this is a write, we
        // still populate content first, then perform any dirty transitions / requests.
        Err(self.read_request(max_request_pages, page_request.get_read_request()))
    }

    /// Attempts to lookup the next offset in the VMO and return a page that will only be read
    /// from. This can return zero pages or pages from a parent VMO.
    /// A `DeferredOps` is required to be passed in, even though a read does not ever directly
    /// generate any deferred actions, to enforce the requirement that all operations on a pager
    /// backed VMO are serialized with the `paged_vmo_lock`. Having to present a `DeferredOps` here
    /// is a simple way to ensure this lock is held.
    ///
    /// Cursor advancement, `page_request`, and access-time behavior match
    /// [`require_owned_page`](Self::require_owned_page).
    ///
    /// `max_request_pages` must be non-zero and at most the number of pages left in the range of
    /// the cursor, and `page_request` must not hold an active request.
    pub fn require_read_page(
        mut self: Pin<&mut Self>,
        max_request_pages: u64,
        _deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> Result<RequireResult, Status> {
        // Make sure the cursor is valid.
        self.as_mut().establish_cursor();

        // If there's a page or reference, return it.
        if self.cursor_is_page() || self.cursor_is_reference() {
            if self.cursor_is_reference() {
                // SAFETY: The reference is only used by the call below, which does not
                // otherwise access `page_request`.
                let anonymous = unsafe { page_request.get_anonymous() };
                // SAFETY: `cursor_is_reference` returned true.
                unsafe { self.as_mut().cursor_reference_to_page(anonymous) }?;
                debug_assert!(self.cursor_is_page());
            }
            // SAFETY: Either `cursor_is_page` returned true, or the reference in the slot was just
            // replaced with a page.
            return Ok(unsafe { self.cursor_as_result() });
        }

        // Check for zero page options.
        if self.cursor_is_content_zero() {
            self.increment_cursor();
            return Ok(RequireResult { page: vm_get_zero_page(), writable: false });
        }

        // No available content, need to fetch it from the page source. `read_request` performs all
        // the requisite asserts to ensure we are not doing this mistakenly.
        Err(self.read_request(max_request_pages, page_request.get_read_request()))
    }

    /// Attempts to lookup the next offset in the VMO and return a page that is readable or
    /// writable based on the `will_write` flag.
    ///
    /// Cursor advancement, `page_request`, and access-time behavior match
    /// [`require_owned_page`](Self::require_owned_page).
    ///
    /// `max_request_pages` must be non-zero and at most the number of pages left in the range of
    /// the cursor, and `page_request` must not hold an active request.
    ///
    /// # Safety
    ///
    /// The caller must ensure `deferred` was made for the VMO this cursor was made for.
    #[inline]
    pub unsafe fn require_page(
        self: Pin<&mut Self>,
        will_write: bool,
        max_request_pages: u64,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> Result<RequireResult, Status> {
        // Being writable implies owning the page, so forward to the correct operation.
        if will_write {
            // SAFETY: The caller upholds the contract of `require_owned_page`, which is the same
            // as this function's.
            return unsafe {
                self.require_owned_page(true, max_request_pages, deferred, page_request)
            };
        }
        self.require_read_page(max_request_pages, deferred, page_request)
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
    /// `paddrs.len()` must be at most the number of pages left in the range of the cursor.
    pub fn if_exist_pages<'p>(
        mut self: Pin<&mut Self>,
        will_write: bool,
        paddrs: &'p mut [MaybeUninit<PAddr>],
    ) -> &'p mut [PAddr] {
        if paddrs.is_empty() {
            return &mut [];
        }
        let mut max_pages = paddrs.len() as u64;
        // Ensure that the requested range is valid.
        debug_assert!(self.offset + (::page::SIZE as u64) * max_pages <= self.end_offset);

        self.as_mut().establish_cursor();

        // We only return actual pages that are ready to use right now without any dirty transitions
        // or copy-on-write or needing to mark them accessed.
        if !self.cursor_is_usable_page(will_write) || self.mark_accessed {
            return &mut [];
        }

        // Trim max pages to the visible length of the current owner. This only has an effect when
        // `target` is not the owner as otherwise the `visible_end` is the same as `end_offset` and
        // we already validated that we are within that range.
        if !self.target_is_owner() {
            max_pages = cmp::min(
                max_pages,
                (self.owner_info.visible_end - self.offset) / (::page::SIZE as u64),
            );
        }
        debug_assert!(max_pages > 0);

        // The entries of `paddrs` that may be filled. Taking this sub-slice cannot panic:
        // `max_pages` starts as `paddrs.len()` and is only ever reduced, so it is at most
        // `paddrs.len()` and converts back to `usize` without loss.
        let mut out = paddrs[..max_pages as usize].iter_mut();

        // Take up to the max_pages as long as they exist contiguously.
        let mut pages: u64 = 0;
        let _ = self.as_mut().project().owner_info.project().cursor.for_every_contiguous(|page| {
            // An entry is left whenever this is called, unless `max_pages` is 0: the iteration
            // stops in the call that fills the last entry.
            if page.is_page()
                && let Some(entry) = out.next()
            {
                entry.write(page.page_as_paddr());
                pages += 1;
                return if out.len() == 0 { Status::STOP } else { Status::NEXT };
            }
            Status::STOP
        });
        // Update the cursor to reflect the number of pages we found and are returning.
        // We could check if cursor is still valid, but it's more efficient to just invalidate it
        // and let any potential next page request recalculate it.
        self.increment_offset_and_invalidate_cursor(pages * (::page::SIZE as u64));
        // SAFETY: The loop above initialized the first `pages` entries of `paddrs`: `pages` counts
        // the entries it took from `out`, which yields the entries of a sub-slice of `paddrs` in
        // order from the first, each once, and it wrote every entry it took. `PAddr` has the same
        // layout as `MaybeUninit<PAddr>`, `paddrs` is a valid slice so its pointer is non-null and
        // aligned even when `pages == 0`, and the result exclusively borrows `paddrs` for `'p`.
        unsafe {
            core::slice::from_raw_parts_mut(paddrs.as_mut_ptr().cast::<PAddr>(), pages as usize)
        }
    }

    /// Checks the current slot for a page and returns it. This does not return zero pages and, due
    /// to the lack of taking a page request, will not perform copy-on-write allocations or dirty
    /// transitions. In these cases it will return `None` even though there is content.
    /// The internal cursor is always incremented regardless of the return value.
    ///
    /// The range of the cursor must have a page left.
    pub fn maybe_page(mut self: Pin<&mut Self>, will_write: bool) -> Option<VmPagePtr> {
        self.as_mut().establish_cursor();

        // If the page is immediately usable, i.e. no dirty transitions etc needed, then we can
        // provide it. Otherwise just increment the cursor and return `None`.
        let page = if self.cursor_is_usable_page(will_write) {
            // SAFETY: `cursor_is_usable_page` returned true, which implies `cursor_is_page`.
            Some(unsafe { self.cursor_page() })
        } else {
            None
        };

        if let Some(page) = page
            && self.mark_accessed
        {
            // SAFETY: `page` is in the page list of the owner, whose lock this cursor holds.
            unsafe { pmm_page_queues().mark_accessed(page) };
        }

        self.increment_cursor();

        page
    }

    /// Has similar properties of `maybe_page`, except it returns how many times in a row
    /// `maybe_page` would have returned `None`. Regardless of the return value of this method, it
    /// is not guaranteed that the next call to `maybe_page` will not be `None`. The cursor is
    /// incremented by the number of pages returned.
    ///
    /// The range of the cursor must have a page left.
    pub fn skip_missing_pages(mut self: Pin<&mut Self>) -> u64 {
        self.as_mut().establish_cursor();

        // Check if the cursor is truly empty
        if !self.cursor_is_empty() || self.cursor_is_in_interval_zero() {
            return 0;
        }

        let mut possibly_empty = self.owner_info.visible_end - self.offset;
        // Limit possibly_empty by the first page visible in the owner which, since our cursor is
        // empty, would also be the root vmo.
        if possibly_empty > (::page::SIZE as u64) {
            let owner_offset = self.owner_info.owner_offset;
            let (owner, owner_token) = self.owner_info.owner.locked_or(self.target, &self.token);
            let _ = owner.page_list_locked(owner_token).for_every_page_in_range(
                owner_offset,
                owner_offset + possibly_empty,
                |p, offset| {
                    // Content should have been empty initially, so should not find anything at the
                    // start offset.
                    debug_assert!(offset > owner_offset);
                    // If this is an interval sentinel, it can only be a start or slot, since we
                    // know we started in a true gap outside of an interval.
                    debug_assert!(
                        !p.is_interval() || p.is_interval_slot() || p.is_interval_start()
                    );
                    let new_size = offset - owner_offset;
                    // Due to the limited range of the operation, the only way this callback ever
                    // fires is if the range is actually getting trimmed.
                    debug_assert!(new_size < possibly_empty);
                    possibly_empty = new_size;
                    Status::STOP
                },
            );
        }
        // The cursor was empty, so we should have ended up with at least one page.
        debug_assert!(possibly_empty >= (::page::SIZE as u64));
        debug_assert!(::page::is_page_rounded(possibly_empty as usize));
        debug_assert!(possibly_empty + self.offset <= self.end_offset);
        self.increment_offset_and_invalidate_cursor(possibly_empty);
        possibly_empty / (::page::SIZE as u64)
    }

    /// Provides a list of pages that can be used to service any allocations. This is useful if you
    /// know you will be looking up multiple absent pages and want to avoid repeatedly hitting the
    /// pmm for single pages.
    /// If a list is provided then `clear_alloc_list` must be called prior to the cursor being
    /// destroyed.
    ///
    /// # Safety
    ///
    /// Until `clear_alloc_list` is called, `alloc_list` must point to a live list that nothing
    /// other than this cursor accesses, and every page in it must be a valid `VmPage` in the
    /// `ALLOC` state that the caller conceptually owns. The list must never be moved again until it
    /// is dropped, before or after `clear_alloc_list` is called.
    #[inline]
    pub unsafe fn give_alloc_list(
        self: Pin<&mut Self>,
        alloc_list: NonNull<VmPageDoublyLinkedList>,
    ) {
        *self.project().alloc_list = Some(alloc_list);
    }

    /// Clears any remaining allocation list. This does not free any remaining pages, and it is the
    /// caller's responsibility to check the list and free any pages.
    #[inline]
    pub fn clear_alloc_list(self: Pin<&mut Self>) {
        let this = self.project();
        debug_assert!(this.alloc_list.is_some());
        *this.alloc_list = None;
    }

    /// Disables placing newly allocated zero pages in the zero fork list.
    #[inline]
    pub fn disable_zero_fork(self: Pin<&mut Self>) {
        *self.project().zero_fork = false;
    }

    /// Indicates that any existing pages that are returned should not be considered accessed and
    /// have their accessed times updated.
    #[inline]
    pub fn disable_mark_accessed(self: Pin<&mut Self>) {
        *self.project().mark_accessed = false;
    }

    /// Exposed for lock assertions.
    #[inline]
    pub fn lock_is_held(&self) -> bool {
        self.target.lock_is_held()
    }

    /// Returns the token of the cursor for the lock of its target, for operating on the target
    /// while the cursor is live.
    ///
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

    // Note: Some of these methods are marked `#[inline(always)]` as doing so has a dramatic
    // performance improvement, and is worth the increase in code size.

    /// Increments the cursor to the next offset. Doing so may invalidate the cursor and requiring
    /// recalculating.
    #[inline(always)]
    fn increment_cursor(mut self: Pin<&mut Self>) {
        let this = self.as_mut().project();
        *this.offset += ::page::SIZE as u64;
        if *this.offset == this.owner_info.visible_end {
            // Have reached either the end of the valid iteration range, or the end of the visible
            // portion of the owner. In the latter case we invalidate the cursor as we need to walk
            // up the hierarchy again to find the next owner that applies to this slot.
            // In the case where we have reached the end of the range, i.e. `offset` is also equal
            // to `end_offset`, there is nothing we need to do, but to ensure that an error is
            // generated if the user incorrectly attempts to get another page we also invalidate the
            // owner.
            self.invalidate_cursor();
        } else {
            // Increment the owner offset and step the page list cursor to the next slot.
            let owner_info = this.owner_info.project();
            *owner_info.owner_offset += ::page::SIZE as u64;
            owner_info.cursor.step();
            *this.owner_cursor = owner_info.cursor.current_ptr();

            // When iterating, it's possible that we need to find a new owner even before we hit the
            // `visible_end`. This happens since even if we have no content at our cursor, we might
            // have a parent with content, and the `visible_end` is tracking the range visible in us
            // from the target and does not imply we have all the content.
            // Consider a simple hierarchy where the root has a page in slot 1, [.P.], then its
            // child has a page in slot 0 [P...] and then its child, the target, has no pages [...]
            // A cursor on this range will initially find the owner as this middle object, and a
            // visible length of 3 pages. However, when we step the cursor we clearly need to then
            // walk up to our parent to get the page. In this case we would ideally walk up to the
            // parent, if there is one, and check for content, or if no parent keep returning empty
            // slots. Unfortunately once the cursor returns `None` we cannot know where the next
            // content might be. To make things simpler we just invalidate owner if we hit this case
            // and re-walk from the bottom again.
            // Whether or not a parent might have content is a combination of
            //  1. There must be a parent and the offset within the parent limit
            //  2. Either the slot is empty, meaning we see the parent, and the node does not use
            //     parent content markers. Or there is a parent content marker.
            let can_see_parent = |owner_cursor: &VmPageOrMarker| -> bool {
                let (owner, owner_token) =
                    self.owner_info.owner.locked_or(self.target, &self.token);
                if !owner.has_parent_locked(owner_token) {
                    return false;
                }
                if self.owner_info.owner_offset >= owner.parent_limit_locked(owner_token) {
                    return false;
                }
                if owner.node_has_parent_content_markers() {
                    return owner_cursor.is_parent_content();
                }
                owner_cursor.is_empty()
            };
            if self.owner_cursor().is_none_or(can_see_parent) {
                self.invalidate_cursor();
            }
        }
    }

    /// Increments the current offset by the given delta, but invalidates the cursor itself
    /// requiring it to be recalculated next time `establish_cursor` is called.
    fn increment_offset_and_invalidate_cursor(mut self: Pin<&mut Self>, delta: u64) {
        *self.as_mut().project().offset += delta;
        self.invalidate_cursor();
    }

    /// Returns whether the cursor is currently valid or needs to be re-calculated.
    #[inline]
    fn is_cursor_valid(&self) -> bool {
        self.is_valid
    }

    /// Calculates the current cursor, finding the correct owner, owner offset etc. There is always
    /// an owner and this process can never fail.
    #[inline]
    fn establish_cursor(self: Pin<&mut Self>) {
        // Check if the cursor needs recalculating.
        if !self.is_cursor_valid() {
            self.establish_cursor_slow();
        }
    }

    /// Recalculates the cursor by looking up the owner of the current offset. `establish_cursor`
    /// calls this only when the cursor is not valid. It is out of line so that the check in
    /// `establish_cursor` can be inlined into every caller.
    #[inline(never)]
    fn establish_cursor_slow(self: Pin<&mut Self>) {
        let mut this = self.project();
        let target: &'a VmCowPages = this.target;
        // Release any lock held from any previous cursor. As per the comment on
        // `invalidate_cursor`, the lock is dropped here to ensure that up until the next page is
        // required, the caller can assume the lock of the owning VMO is held.
        this.owner_info.as_mut().project().owner.release();

        // Ensure still in the valid range.
        debug_assert!(*this.offset < *this.end_offset);

        // SAFETY: `this.token` proves the lock of `target` is held, and it stays held for `'a` (see
        // the `token` field), which outlives this cursor and so `owner_info`. For the second
        // clause: this cursor never calls `current()` or
        // `VmPageList::for_every_page_in_cursor_range()` on `owner_info.cursor`, and calls its
        // `for_every_contiguous()` only in `if_exist_pages`, with a closure that does not write to
        // the page list. While `owner_info.cursor` is positioned at a node, every path of this
        // cursor that inserts a node into or removes one from the page list it points into
        // invalidates this cursor before `owner_info.cursor` or `owner_cursor` is used again, and
        // this function then replaces both. That includes a `begin_add_page_locked` that fails:
        // `target_allocate_copy_page_as_result` invalidates this cursor before returning the error.
        // Per the contract of `new` nothing else changes that page list while the cursor is used.
        // For the third clause: the owner lock was released above, so the search starts from
        // `target`, and the conditions for acquiring the locks of its ancestors are the last clause
        // of the contract of `new`.
        unsafe {
            target.find_page_content_into_locked(
                this.token,
                this.owner_info.as_mut(),
                *this.offset,
                *this.end_offset - *this.offset,
            );
        }
        *this.owner_cursor = this.owner_info.cursor.current_ptr();
        *this.is_valid = true;
    }

    /// Returns true if `target` is the owner.
    #[inline]
    fn target_is_owner(&self) -> bool {
        self.owner_info.owner.is_none()
    }

    /// Invalidates the owner, so that the next page will have to perform the lookup again, walking
    /// up the hierarchy if needed.
    #[inline]
    fn invalidate_cursor(self: Pin<&mut Self>) {
        let this = self.project();
        // Mark the cursor as invalid, but do not drop the owner lock. This ensures that the up
        // until the next page lookup is performed the lock for the owner of the page most recently
        // returned remains held.
        *this.is_valid = false;
        // The slot may only be reached through `owner_cursor` while the cursor is valid.
        *this.owner_cursor = None;
    }

    /// Returns the slot that `owner_cursor` points to, if any.
    #[inline]
    fn owner_cursor(&self) -> Option<&VmPageOrMarker> {
        // SAFETY: Per the safety invariant on `owner_cursor` it points to a live slot of the
        // owner's page list, whose lock is held: this cursor is being used, so it is not one that
        // may only be dropped. No `&mut` to the slot is live: those are only created by methods
        // that borrow the cursor exclusively.
        self.owner_cursor.map(|slot| unsafe { slot.as_ref() })
    }

    /// Returns the page in the slot that `owner_cursor` points to.
    ///
    /// # Safety
    ///
    /// `self.cursor_is_page()` must be true.
    #[inline]
    unsafe fn cursor_page(&self) -> VmPagePtr {
        // SAFETY: Per `# Safety` `cursor_is_page()` is true, so `owner_cursor` is `Some`.
        unsafe { self.owner_cursor().unwrap_unchecked() }.page()
    }

    /// Returns the list given to `give_alloc_list`, if any, for use by a page allocation.
    ///
    /// # Safety
    ///
    /// `alloc_list` must be the `alloc_list` field of a `LookupCursor`.
    #[inline]
    unsafe fn alloc_list_pin(
        alloc_list: &mut Option<NonNull<VmPageDoublyLinkedList>>,
    ) -> Option<Pin<&mut VmPageDoublyLinkedList>> {
        // SAFETY: Two operations, `as_mut()` and `Pin::new_unchecked`. Per `# Safety` and the
        // safety invariant on that field (the contract of `give_alloc_list`) the list is live and
        // only accessed by the cursor, which `alloc_list` is exclusively borrowed from, so the
        // reference is valid and unique. The list is only ever popped from through the returned
        // reference, and per the same invariant it is never moved again until it is dropped, so
        // pinning it is sound; it stays owned, and is eventually dropped in place, by whoever gave
        // it.
        alloc_list.as_mut().map(|list| unsafe { Pin::new_unchecked(list.as_mut()) })
    }

    /// Helpers for querying the state of the cursor.
    #[inline]
    fn cursor_is_page(&self) -> bool {
        self.owner_cursor().is_some_and(|slot| slot.is_page())
    }
    #[inline]
    fn cursor_is_marker(&self) -> bool {
        self.owner_cursor().is_some_and(|slot| slot.is_marker())
    }
    #[inline]
    fn cursor_is_empty(&self) -> bool {
        self.owner_cursor().is_none_or(|slot| slot.is_empty())
    }
    #[inline]
    fn cursor_is_parent_content(&self) -> bool {
        self.owner_cursor().is_some_and(|slot| slot.is_parent_content())
    }
    #[inline]
    fn cursor_is_reference(&self) -> bool {
        self.owner_cursor().is_some_and(|slot| slot.is_reference())
    }
    /// Checks if the cursor is exactly at a sentinel, and not generally inside an interval.
    #[inline]
    fn cursor_is_interval_zero(&self) -> bool {
        self.owner_cursor().is_some_and(|slot| slot.is_interval_zero())
    }

    /// Checks if the cursor, as determined by the current offset and not the literal
    /// `owner_info.cursor`, is in a zero interval.
    #[inline]
    fn cursor_is_in_interval_zero(&self) -> bool {
        self.cursor_is_interval_zero() || {
            let (owner, owner_token) = self.owner_info.owner.locked_or(self.target, &self.token);
            owner
                .page_list_locked(owner_token)
                .is_offset_in_zero_interval(self.owner_info.owner_offset)
        }
    }

    /// The cursor can be considered to have content of zero if either it points at a zero marker,
    /// or the cursor itself is empty and content is initially zero. Content is initially zero if
    /// either there isn't a page source, or the offset is in a zero interval.
    /// If a page source is not a user pager then we could consider it to be zero, except we
    /// would not necessarily be able to fork that zero page to create an owned/writable page. In
    /// practice this case only exists for contiguous VMOs, and the way they are used makes
    /// optimizing to return the zero page in the case of reads not beneficial.
    fn cursor_is_content_zero(&self) -> bool {
        // Markers are always zero.
        if self.cursor_is_marker() {
            return true;
        }

        if self.owner_info.owner.locked_or(self.target, &self.token).0.page_source().is_some() {
            // With a page source emptiness implies needing to request content, however we can have
            // zero intervals which do start as zero content.
            return self.cursor_is_in_interval_zero();
        }
        // Without a page source emptiness is filled with zeros and intervals are only permitted if
        // there is a page source.
        // We consider parent content to be empty since a parent content marker can be spurious, and
        // the only time the cursor would actually point to the parent content marker is if there is
        // no content to be found in the parent.
        self.cursor_is_empty() || self.cursor_is_parent_content()
    }

    /// A usable page is either just any page, if not writing, or if writing, a page that is owned
    /// by the target and doesn't need any dirty transitions. i.e., a page that is ready to use
    /// right now.
    #[inline]
    fn cursor_is_usable_page(&self, writing: bool) -> bool {
        self.cursor_is_page()
            && (!writing || (self.target_is_owner() && !self.target_dirty_tracked()))
    }

    /// Determines whether the zero content at the current cursor should be supplied as dirty or
    /// not. This is only allowed to be called if `cursor_is_content_zero` is true.
    fn target_zero_content_supply_dirty(&self, writing: bool) -> bool {
        if !self.target_dirty_tracked() {
            return false;
        }
        if writing {
            return true;
        }
        // Markers start clean
        if self.cursor_is_marker() {
            return false;
        }
        // The only way this offset can have been zero content and reach here, is if we are in an
        // interval. If this slot were empty then, since we are dirty tracked and hence must have a
        // page source, we would not consider this zero.
        debug_assert!(self.cursor_is_in_interval_zero());
        // Zero intervals are considered implicitly dirty and allocating them, even for reading,
        // causes them to be supplied as new dirty pages.
        true
    }

    /// Returns whether the target is tracking the dirtying of content with dirty pages and dirty
    /// transitions.
    #[inline]
    fn target_dirty_tracked(&self) -> bool {
        self.target_directly_backed_by_user_pager
    }

    /// Turns the supplied page into a result. Does not increment the cursor. `in_target` specifies
    /// whether the page is known to be in `target` or in some parent object.
    ///
    /// # Safety
    ///
    /// If `in_target` is true, `page` must be a page in the `OBJECT` state that is in the page list
    /// of `target`.
    #[inline]
    unsafe fn page_as_result_no_increment(
        &self,
        page: VmPagePtr,
        in_target: bool,
    ) -> RequireResult {
        // The page is writable if it's present in the target (non owned pages are never writable)
        // and it does not need a dirty transition. A page doesn't need a dirty transition if the
        // target isn't a user pager, or if the page is just already dirty.
        RequireResult {
            page,
            writable: in_target
                && (!self.target_directly_backed_by_user_pager || {
                    // SAFETY: `in_target` is true, so per `# Safety` `page` is an `OBJECT`-state
                    // page of `target`, whose lock `token` proves is held.
                    unsafe { VmCowPages::is_page_dirty(page) }
                }),
        }
    }

    /// Turns the current cursor, which must be a page, into a result and handles any access time
    /// updating. Increments the cursor.
    ///
    /// # Safety
    ///
    /// `self.cursor_is_page()` must be true.
    #[inline(always)]
    unsafe fn cursor_as_result(self: Pin<&mut Self>) -> RequireResult {
        // SAFETY: Per `# Safety` `cursor_is_page()` is true.
        let page = unsafe { self.cursor_page() };
        if self.mark_accessed {
            // SAFETY: `page` is in the page list of the owner, whose lock this cursor holds.
            unsafe { pmm_page_queues().mark_accessed(page) };
        }
        // Inform `page_as_result_no_increment` whether the owner is the `target`, but otherwise let
        // it calculate the actual writability of the page.
        // SAFETY: `page` is in the page list of the owner, which is `target` if
        // `target_is_owner()`.
        let result = unsafe { self.page_as_result_no_increment(page, self.target_is_owner()) };
        self.increment_cursor();
        result
    }

    /// Allocates a new page for the target that is a copy of the provided `source` page. On
    /// success page is inserted into target at the current `offset` and the cursor is incremented.
    /// If beginning the insertion fails the cursor is invalidated and `offset` is unchanged.
    ///
    /// # Safety
    ///
    /// * The cursor must be valid (established and not invalidated since).
    /// * `source` must be the zero page, or the page in the slot the cursor is currently at.
    /// * `deferred` must have been made for `target`.
    unsafe fn target_allocate_copy_page_as_result(
        mut self: Pin<&mut Self>,
        source: VmPagePtr,
        dirty_state: DirtyState,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        page_request: &mut AnonymousPageRequest,
    ) -> Result<RequireResult, Status> {
        let target: &'a VmCowPages = self.target;
        let offset = self.offset;
        // SAFETY: Per `# Safety` `source` is the zero page or a page in the page list of the
        // owner, whose lock this cursor holds, so it is a live page.
        let source_paddr = unsafe { source.paddr() };
        // SAFETY: The argument is the `alloc_list` field of this cursor.
        let alloc_list = unsafe { Self::alloc_list_pin(self.as_mut().project().alloc_list) };
        // SAFETY: The allocation list, if any, satisfies the contract of `give_alloc_list`.
        // `source_paddr` is the zero page, or a page that stays in the owner's page list, and so
        // allocated and in the physmap, while this cursor holds the owner's lock.
        let out_page =
            unsafe { target.allocate_copy_page(source_paddr, alloc_list, Some(page_request)) }?;
        // The forked page was just allocated, and so cannot be a loaned page.
        debug_assert!({
            // SAFETY: `out_page` was just allocated and is owned by this function.
            unsafe { !out_page.is_loaned() }
        });

        // We could be allocating a page to replace a zero page marker in a pager-backed VMO. If so
        // then set its dirty state to what was requested, `complete_add_page_locked` below will
        // then insert the page into the appropriate page queue.
        if self.target_directly_backed_by_user_pager {
            // The only page we can be forking here is the zero page.
            debug_assert!(source == vm_get_zero_page());
            // The object directly owns the page.
            debug_assert!(self.target_is_owner());

            // SAFETY: `allocate_copy_page` returned `out_page` in the `OBJECT` state, and it is
            // about to be added to `target` at `offset`.
            unsafe {
                target.update_dirty_state_locked(
                    &self.token,
                    out_page,
                    offset,
                    dirty_state,
                    /*is_pending_add=*/ true,
                );
            }
        }

        // For efficiently we would like to use the slot we already have in our cursor if possible,
        // however that can only be done if all of the following hold:
        //  * `target_is_owner()` - If not true then we do not even have a cursor (and hence slot)
        //    for where the insertion is happening.
        //  * `owner_cursor`, the cached `owner_info.cursor.current_ptr()`, is `Some` - Must be an
        //    actual node and slot already allocated, it is either empty or a marker type.
        //  * `page_source_type() != PageSourceType::UserPager` - May have intervals, which are
        //    zeroes that we could be overwriting here, but the slot itself we have found could be
        //    empty and the interval may need splitting. For simplicity we do not attempt to check
        //    for and handle interval splitting, and just skip reusing our slot in this case.
        let reuse_slot = if self.target_is_owner()
            && self.owner_info.owner.locked_or(target, &self.token).0.page_source_type()
                != PageSourceType::UserPager
        {
            self.owner_cursor
        } else {
            None
        };
        let can_reuse_slot = reuse_slot.is_some();
        let page_transaction = if let Some(slot) = reuse_slot {
            // SAFETY: `slot` is `owner_cursor`, so per the safety invariant on that field it is the
            // live slot at `owner_info.owner_offset` in the page list of the owner. `target` is the
            // owner, so that is the slot at `offset` in the page list of `target`. `target` is not
            // backed by a user pager, so has no intervals. Nothing touches the slot until the
            // transaction is completed below.
            unsafe {
                target.begin_add_page_with_slot_locked(
                    self.as_mut().project().token,
                    offset,
                    slot,
                    CanOverwriteSlot::ZeroMarkerOrInterval,
                )
            }
        } else {
            target.begin_add_page_locked(
                self.as_mut().project().token,
                offset,
                CanOverwriteSlot::ZeroMarkerOrInterval,
            )
        };
        let mut page_transaction = match page_transaction {
            Ok(page_transaction) => page_transaction,
            Err(status) => {
                core::hint::cold_path();
                // SAFETY: The contract of `rollback_allocated_page` is that of `free_page`:
                // `out_page` was allocated for `target` above, is still owned by this function, and
                // is in no page queue or list.
                unsafe { target.rollback_allocated_page(out_page) };
                // A failed `begin_add_page_locked` can have changed the nodes of the page list of
                // `target`, which the cursor may point into, so do not keep the cursor.
                self.as_mut().invalidate_cursor();
                return Err(status);
            }
        };

        // Skip range change updates in the case where we're forking the singleton zero page so that
        // can we use a more optimal unmap request ourselves.
        // SAFETY: `page_transaction` was begun on `target` just above with no page list operation
        // since, `out_page` is an `OBJECT`-state page owned by this function that is in no page
        // queue, and per `# Safety` `deferred` was made for `target`.
        let old = unsafe {
            target.complete_add_page_locked(
                self.as_mut().project().token,
                &mut page_transaction,
                VmPageOrMarker::from_page(out_page),
                if source == vm_get_zero_page() { None } else { Some(deferred.as_mut()) },
            )
        };
        debug_assert!(!old.is_page_or_ref());
        if source == vm_get_zero_page() {
            target.range_change_update_locked(
                &self.token,
                VmCowRange::new(offset, ::page::SIZE as u64),
                RangeChangeOp::UnmapZeroPage,
                Some(deferred.as_mut()),
            );
        }

        // If asked to explicitly mark zero forks, and this is actually fork of the zero page, move
        // to the correct queue. Discardable pages are not considered zero forks as they are always
        // in the reclaimable page queues.
        if self.zero_fork && source == vm_get_zero_page() && !target.is_discardable() {
            // SAFETY: `out_page` was just added to `target`, which placed it in the page queues.
            unsafe { pmm_page_queues().move_anonymous_to_anonymous_zero_fork(out_page) };
        }

        // This is the only path where we can allocate a new page without being a clone (clones are
        // always cached). So we check here if we are not fully cached and if so perform a
        // clean/invalidate to flush our zeroes. After doing this we will not touch the page via the
        // physmap and so we can pretend there isn't an aliased mapping.
        // There are three potential states that may exist
        //  * VMO is cached, paged_ref might be null, we might have children -> no cache op needed
        //  * VMO is uncached, paged_ref is not null, we have no children -> cache op needed
        //  * VMO is uncached, paged_ref is null, we have no children -> cache op not needed /
        //                                                                state cannot happen
        // In the uncached case we know we have no children, since it is by definition not valid to
        // have copy-on-write children of uncached pages. The third case cannot happen, but even if
        // it could with no children and no paged_ref the pages cannot actually be referenced so any
        // cache operation is pointless.
        // The paged_ref could be null if the VmObjectPaged has been destroyed.
        if let Some(paged) = target.get_paged_backlink_locked(&self.token)
            && paged.get_mapping_cache_policy_locked(&self.token) != ARCH_MMU_FLAG_CACHED
        {
            core::hint::cold_path();
            // SAFETY: Two operations. `out_page.paddr()`: `out_page` was just added to `target`,
            // whose lock `self.token` proves is held, so it is a live page that is safe to inspect.
            // `arch_clean_invalidate_cache_range`: `vaddr` is the physmap address of that page, so
            // `[vaddr, vaddr + page::SIZE)` is a valid mapped range that does not wrap.
            unsafe {
                let vaddr = paddr_to_physmap(out_page.paddr());
                arch_clean_invalidate_cache_range(vaddr.0, ::page::SIZE);
            }
        }

        // Need to increment the cursor, but we have also potentially modified the page lists in the
        // process of inserting the page.
        if self.target_is_owner() {
            // In the case of `target_is_owner()` we may have to create a node and need to establish
            // a cursor. However, if we were able to reuse the slot and insert directly into the
            // cursor, then we know that we already had a node and that the cursor remains valid as
            // the page list was not otherwise modified.
            if !can_reuse_slot {
                self.as_mut().increment_offset_and_invalidate_cursor(::page::SIZE as u64);
            } else {
                // Cursor should have been updated to the new page
                debug_assert!(self.cursor_is_page());
                debug_assert!({
                    // SAFETY: `cursor_is_page` was asserted above.
                    unsafe { self.cursor_page() == out_page }
                });
                self.as_mut().increment_cursor();
            }
        } else {
            // If `!target_is_owner()` then the owner's page list will not have been modified, so
            // safe to just increment.
            self.as_mut().increment_cursor();
        }

        // Return the page. We know it's in the target, since we just put it there, but let
        // `page_as_result_no_increment` determine if that means it is actually writable or not.
        // SAFETY: `out_page` is an `OBJECT`-state page that was just added to `target`.
        Ok(unsafe { self.page_as_result_no_increment(out_page, true) })
    }

    /// Attempts to turn the current cursor, which must be a reference, into a page.
    ///
    /// # Safety
    ///
    /// `self.cursor_is_reference()` must be true.
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok`, the slot that the cursor is at holds a page: `self.cursor_is_page()` is true.
    unsafe fn cursor_reference_to_page(
        self: Pin<&mut Self>,
        page_request: &mut AnonymousPageRequest,
    ) -> Result<(), Status> {
        debug_assert!(self.cursor_is_reference());

        let (owner, owner_token) = self.owner_info.owner.locked_or(self.target, &self.token);
        // SAFETY: Per `# Safety` `cursor_is_reference()` is true, so `owner_cursor` is `Some`.
        let mut slot = unsafe { self.owner_cursor.unwrap_unchecked() };
        // SAFETY: Per the safety invariant on `owner_cursor`, `slot` points to a live slot of the
        // owner's page list (this cursor is being used, so it is not one that may only be dropped),
        // and no other reference to the slot is live as this method borrows the cursor exclusively.
        let mut page_or_mark = VmPageOrMarkerRef::new(unsafe { slot.as_mut() });
        // SAFETY: `page_or_mark` is the slot at `owner_info.owner_offset` in the page list of
        // `owner`, whose lock `owner_token` proves is held, and per `# Safety` it holds a
        // reference, which a page list slot only does for a live reference of the compressor.
        unsafe {
            owner.replace_reference_with_page_locked(
                owner_token,
                &mut page_or_mark,
                self.owner_info.owner_offset,
                Some(page_request),
            )
        }
    }

    /// Helper for generating a read request for the given maximal range.
    fn read_request(&self, max_request_pages: u64, page_request: Pin<&PageRequest>) -> Status {
        let (owner, owner_token) = self.owner_info.owner.locked_or(self.target, &self.token);
        // The owner must have a page_source to be doing a read request.
        let page_source = owner.page_source().expect("read_request requires a page source");
        // The cursor should be explicitly empty as read requests are only for complete content
        // absence.
        debug_assert!(self.cursor_is_empty());
        debug_assert!(!self.cursor_is_in_interval_zero());
        // The total range requested should not be beyond the cursors valid range.
        debug_assert!(self.offset + (::page::SIZE as u64) * max_request_pages <= self.end_offset);
        debug_assert!(max_request_pages > 0);

        let mut vmo_debug_info = VmoDebugInfo { vmo_id: 0, vmo_name: [0; _] };
        // The page owner has a page source so it cannot be a hidden node, but the VmObjectPaged
        // could have been destroyed. We could be looking up a page via a lookup in a child after
        // the parent VmObjectPaged has gone away, so paged_ref could be null. Let the page source
        // handle any failures requesting the pages.
        if let Some(paged) = owner.get_paged_backlink_locked(owner_token) {
            vmo_debug_info.vmo_id = paged.user_id();
            // Write the name in place, rather than into a second buffer that is then copied.
            paged.get_name_locked(
                zerocopy::IntoBytes::as_mut_bytes(&mut vmo_debug_info.vmo_name),
                owner_token,
            );
        }

        // Try and batch more pages up to `max_request_pages`.
        let mut request_size = max_request_pages * (::page::SIZE as u64);
        if !self.target_is_owner() {
            debug_assert!(self.owner_info.visible_end > self.offset);
            // Limit the request by the number of pages that are actually visible from the `target`
            // to owner.
            request_size = cmp::min(request_size, self.owner_info.visible_end - self.offset);
        }
        // Limit `request_size` to the first page visible in the page owner to avoid requesting
        // pages that are already present. If there is one page present in an otherwise long run of
        // absent pages then it might be preferable to have one big page request, but for now only
        // request absent pages. If already requesting a single page then can avoid the page list
        // operation.
        let owner_offset = self.owner_info.owner_offset;
        if request_size > (::page::SIZE as u64) {
            let _ = owner.page_list_locked(owner_token).for_every_page_in_range(
                owner_offset,
                owner_offset + request_size,
                |p, offset| {
                    debug_assert!(!p.is_parent_content());
                    // Content should have been empty initially, so should not find anything at the
                    // start offset.
                    debug_assert!(offset > owner_offset);
                    // If this is an interval sentinel, it can only be a start or slot, since we
                    // know we started in a true gap outside of an interval.
                    debug_assert!(
                        !p.is_interval() || p.is_interval_slot() || p.is_interval_start()
                    );
                    let new_size = offset - owner_offset;
                    // Due to the limited range of the operation, the only way this callback ever
                    // fires is if the range is actually getting trimmed.
                    debug_assert!(new_size < request_size);
                    request_size = new_size;
                    Status::STOP
                },
            );
        }
        debug_assert!(request_size >= (::page::SIZE as u64));

        let status =
            page_source.get_pages(owner_offset, request_size, page_request, vmo_debug_info);
        // Pager page sources will never synchronously return a page.
        debug_assert!(status.is_err());
        let Err(status) = status else {
            core::hint::cold_path();
            panic!("page source synchronously returned a page");
        };
        status
    }

    /// Helper for generating a dirty request for the given maximal range.
    fn dirty_request(
        &self,
        max_request_pages: u64,
        page_request: Pin<&LazyPageRequest>,
    ) -> Result<(), Status> {
        // Dirty requests, unlike read requests, happen directly against the target, and not the
        // owner. This is because to make something dirty you must own it. Simply checking for
        // `target_is_owner()` is insufficient, since the cursor may have been made invalid
        // (clearing the owner) just prior to generating this dirty request, and we do not otherwise
        // need the cursor here.
        // So we also validate that we have no parent, and that we have a page source.
        debug_assert!(self.target_is_owner());
        debug_assert!(!self.target.has_parent_locked(&self.token));
        debug_assert!(self.target.page_source().is_some());
        debug_assert!(max_request_pages > 0);
        debug_assert!(self.offset + (::page::SIZE as u64) * max_request_pages <= self.end_offset);

        // As we know `target` is the owner there is no need to trim the requested range to any kind
        // of visible range, so just attempt to dirty the entire range.
        let dirty_len = self.target.prepare_for_write_locked(
            &self.token,
            VmCowRange::new(self.offset, (::page::SIZE as u64) * max_request_pages),
            page_request,
        )?;
        // If success is claimed then it must be the case that at least one page was dirtied,
        // allowing us to make progress.
        debug_assert!(dirty_len != 0 && dirty_len <= max_request_pages * (::page::SIZE as u64));
        Ok(())
    }
}

/// The value of 32 was chosen as there is minimal performance gains originally measured by using
/// higher values. There is an incentive on this being as small as possible due to this typically
/// being created on the stack, and our stack space is limited.
const BATCH_PQ_REMOVE_MAX_PAGES: usize = 32;

/// Helper class for collecting pages to perform batched removes from the page queue so as not to
/// incur its spinlock overhead for every single page. Pages that it removes from the page queue
/// get placed into a provided list. Note that pages are not moved into the list until *after*
/// `flush` has been called, and `flush` must be called prior to object destruction.
///
/// This struct has a large internal array; `pages` is left uninitialized and only `pages[..count]`
/// is ever written.
struct BatchPQRemove<'a> {
    count: usize,
    freed_count: usize,
    is_loaned: bool,
    // Safety invariant: `pages[..count]` are initialized and `count < BATCH_PQ_REMOVE_MAX_PAGES`
    // between calls (`push` flushes as soon as `count` reaches the limit). Every initialized page
    // is a valid `OBJECT`-state page in the page queues that was removed from the page list of a
    // `VmCowPages` in the same hierarchy as the one `freed_list` is later freed against (see
    // `ScopedPageFreedList::list`), with no remaining owners, and the lock of that `VmCowPages`
    // stays held until `flush` removes the page from the page queues. Holding that lock prevents
    // the owning `VmCowPages` from being merged or destroyed and prevents concurrent page-queue
    // consumers from reclaiming or racing on the page while its back pointer is still visible in
    // the page queues. When `count > 0` every initialized page has `is_loaned() == self.is_loaned`.
    pages: [MaybeUninit<VmPagePtr>; BATCH_PQ_REMOVE_MAX_PAGES],
    freed_list: Pin<&'a mut ScopedPageFreedList>,
}

impl<'a> BatchPQRemove<'a> {
    #[inline]
    fn new(freed_list: Pin<&'a mut ScopedPageFreedList>) -> Self {
        Self {
            count: 0,
            freed_count: 0,
            is_loaned: false,
            pages: [const { MaybeUninit::uninit() }; BATCH_PQ_REMOVE_MAX_PAGES],
            freed_list,
        }
    }

    /// Adds a page to the batch set. Automatically calls `flush` if the limit is reached.
    ///
    /// # Safety
    ///
    /// `page` must be a valid `VmPage` in the `OBJECT` state in the page queues that has been
    /// removed from the page list of a `VmCowPages` in the same hierarchy as the one `freed_list`
    /// is later freed against, with no remaining owners, while that `VmCowPages`' lock is held by
    /// the caller, and that lock must remain held until `flush` is called.
    #[inline]
    unsafe fn push(&mut self, page: VmPagePtr) {
        // SAFETY: Caller guarantees `page` is a valid `VmPage` in the `OBJECT` state under the
        // owning `VmCowPages` lock.
        let (pin_count, page_is_loaned) = unsafe {
            let page_ref = page.as_ref();
            (page_ref.get_pin_count(), page_ref.is_loaned())
        };
        assert!(pin_count == 0);
        debug_assert!(self.count < BATCH_PQ_REMOVE_MAX_PAGES);
        if self.count != 0 && page_is_loaned != self.is_loaned {
            core::hint::cold_path();
            self.flush();
        }
        if self.count == 0 {
            self.is_loaned = page_is_loaned;
        }

        // SAFETY: Per the struct safety invariant `self.count < BATCH_PQ_REMOVE_MAX_PAGES` holds
        // between calls (the flush below resets `count` to 0 as soon as it reaches the limit), so
        // `self.count` is in bounds. `get_unchecked_mut` is used because LLVM cannot prove the
        // bound across the out-of-line `self.flush()` calls.
        unsafe { self.pages.get_unchecked_mut(self.count).write(page) };
        self.count += 1;
        if self.count == BATCH_PQ_REMOVE_MAX_PAGES {
            core::hint::cold_path();
            self.flush();
        }
    }

    /// Removes any content from the supplied `page_or_marker` and either calls `push` or otherwise
    /// frees it. Always leaves `page_or_marker` in the empty state. Automatically calls `flush` if
    /// the limit on pages is reached.
    ///
    /// # Safety
    ///
    /// `page_or_marker` must be (or have been removed from) a slot of the page list of a
    /// `VmCowPages` in the same hierarchy as the one `freed_list` is later freed against, while
    /// that `VmCowPages`' lock is held by the caller; any page it holds is in the `OBJECT` state,
    /// owned by that VMO with no remaining shared owners and present in the page queues, that
    /// `VmCowPages`' lock must remain held until `flush` is called, and any reference it holds is
    /// owned by that slot.
    #[inline]
    unsafe fn push_content(&mut self, page_or_marker: &mut VmPageOrMarker) {
        if page_or_marker.is_page() {
            let page = page_or_marker.release_page();
            // SAFETY: Per `# Safety`, `page` was released from a populated `VmPageOrMarker` slot
            // in a VMO's page list under its lock and is in the `OBJECT` state in the page queues.
            unsafe { self.push(page) };
        } else if page_or_marker.is_reference() {
            // TODO(https://fxbug.dev/42138396): Consider whether it is worth batching these.
            let r = page_or_marker.release_reference();
            // SAFETY: Per `# Safety`, `r` was released from a populated reference slot of a VMO's
            // page list, so it belongs to the global compressor and is owned by this function from
            // here on, and the VMO's lock is held in case it is the temporary reference.
            unsafe { free_reference(r) };
        } else {
            *page_or_marker = VmPageOrMarker::empty();
        }
    }

    /// Performs `remove` on any pending pages. This allows you to know that all pages are in the
    /// original list so that you can do operations on the list.
    fn flush(&mut self) {
        if self.count > 0 {
            debug_assert!(self.count <= BATCH_PQ_REMOVE_MAX_PAGES);
            // SAFETY: Per the struct safety invariant, `self.pages[..self.count]` has been
            // initialized by `push` and `self.count <= BATCH_PQ_REMOVE_MAX_PAGES`.
            let pages = unsafe { self.pages.get_unchecked(..self.count).assume_init_ref() };
            if self.is_loaned {
                core::hint::cold_path();
                // SAFETY: Per the struct safety invariant, all `pages` are valid loaned
                // `OBJECT`-state pages in the page queues, removed from the page list of a
                // `VmCowPages` in `freed_list`'s hierarchy with no remaining owners.
                unsafe {
                    pmm::node().begin_free_loaned_array(
                        pages,
                        |pages, free_list| {
                            pmm_page_queues().remove_array_into_list(pages, free_list);
                        },
                        self.freed_list.as_mut().flph(),
                    );
                }
            } else {
                // SAFETY: Per the struct safety invariant, all `pages` are non-loaned
                // `OBJECT`-state pages in the page queues, removed from the page list of a
                // `VmCowPages` in `freed_list`'s hierarchy with no remaining owners, which is what
                // `ScopedPageFreedList::list` requires of the pages added to it.
                unsafe {
                    pmm_page_queues()
                        .remove_array_into_list(pages, self.freed_list.as_mut().list());
                }
                self.freed_count = self.freed_count.wrapping_add(self.count);
            }
            self.count = 0;
        }
    }

    /// Returns the number of pages that were added to `freed_list` by calls to `flush()`. The
    /// `freed_count` counter keeps a running count of freed pages as they are removed and added to
    /// `freed_list`, avoiding having to walk `freed_list` to compute its length.
    #[inline]
    fn freed_count(&self) -> usize {
        self.freed_count
    }
}

impl Drop for BatchPQRemove<'_> {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(self.count, 0);
    }
}

/// Helper class for managing a two-part add page transaction. This object allows adding a page to
/// be split into a check and allocation, which can fail, with the final insertion, which cannot
/// fail.
#[repr(C)]
struct AddPageTransaction<'a> {
    // Safety invariant: `Some(slot)` iff this transaction was constructed via `from_raw` and has
    // not yet been completed, cancelled, or moved from.
    slot: Option<NonNull<VmPageOrMarker>>,
    offset: u64,
    phantom: PhantomData<&'a mut VmPageOrMarker>,
}

::zr::static_assert!(
    core::mem::size_of::<AddPageTransaction<'_>>()
        == core::mem::size_of::<bindings::VmCowPages_AddPageTransaction>()
);
::zr::static_assert!(
    core::mem::align_of::<AddPageTransaction<'_>>()
        == core::mem::align_of::<bindings::VmCowPages_AddPageTransaction>()
);

impl Default for AddPageTransaction<'_> {
    /// Returns a transaction that holds no slot. Used to leave C++-owned storage in a state whose
    /// destructor is a no-op when `begin_add_page*_locked` fails or after a move.
    #[inline]
    fn default() -> Self {
        Self { slot: None, offset: 0, phantom: PhantomData }
    }
}

impl<'a> AddPageTransaction<'a> {
    /// Creates an active transaction for the page-list slot at `offset`.
    ///
    /// # Safety
    ///
    /// `slot` must point to the live `VmPageOrMarker` slot at `offset` in a `VmPageList` and
    /// remain valid until this transaction is completed or cancelled.
    #[inline]
    unsafe fn from_raw(slot: NonNull<VmPageOrMarker>, offset: u64) -> Self {
        Self { slot: Some(slot), offset, phantom: PhantomData }
    }

    /// Ends the transaction without inserting content, returning the slot to `pl` if it is still
    /// empty.
    ///
    /// # Safety
    ///
    /// This transaction must still be active (neither [`Self::cancel`] nor [`Self::complete`] has
    /// been called on it and it has not been moved from, so `slot` is `Some`), the `VmCowPages`
    /// lock that was held when it was begun must still be held, no mutating page-list operation
    /// other than through this transaction may have been performed since, and `pl` must be the
    /// page list the slot was looked up in.
    #[inline]
    unsafe fn cancel(&mut self, pl: &mut VmPageList) {
        debug_assert!(self.slot.is_some());
        // SAFETY: Per `# Safety` the transaction is still active, so `slot` is `Some`, and
        // `slot_ptr` points to a valid `VmPageOrMarker` slot in `pl`.
        let is_empty = unsafe {
            let slot_ptr = self.slot.take().unwrap_unchecked();
            slot_ptr.as_ref().is_empty()
        };
        if is_empty {
            pl.return_empty_slot(self.offset);
        }
    }

    /// Ends the transaction by placing `p`, which must not be empty, in the slot and returning the
    /// slot's previous content.
    ///
    /// # Safety
    ///
    /// Same active-transaction, lock and no-intervening-page-list-mutation preconditions as
    /// [`Self::cancel`]; ownership of any page or reference in `p` transfers to the slot.
    #[inline]
    unsafe fn complete(&mut self, p: VmPageOrMarker) -> VmPageOrMarker {
        debug_assert!(self.slot.is_some());
        debug_assert!(!p.is_empty());
        // SAFETY: Per `# Safety` the transaction is still active, so `slot` is `Some`, and
        // `slot_ptr` points to a valid `VmPageOrMarker` slot in `VmPageList` exclusively reserved
        // by this transaction while the `VmCowPages` lock is held.
        let mut slot = unsafe {
            let mut slot_ptr = self.slot.take().unwrap_unchecked();
            VmPageOrMarkerRef::new(slot_ptr.as_mut())
        };
        slot.swap_content(p)
    }

    #[inline]
    fn offset(&self) -> u64 {
        self.offset
    }
}

impl Drop for AddPageTransaction<'_> {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(self.slot.is_none());
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

    /// length of children_list
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
    // Invariant: `parent_limit <= size`. Every writer of either field keeps it, and
    // `release_owned_pages_locked` relies on it: once it has released up to the end of the node, no
    // part of the released range can still see the parent.
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

    /// As lock orders are only validated when lockdep is enabled, the storage is only defined if
    /// lockdep is enabled.
    #[cfg(with_lock_dep)]
    lock_order: u64,

    /// parent pointer (may be null)
    parent: KCell<Option<RefPtr<VmCowPages>>, VmCowPagesLockClass>,

    /// The page source, if any.
    page_source: Option<RefPtr<PageSource>>,
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
        // SAFETY: `self.as_raw()` points to a live C++ `VmCowPages`.
        let raw = unsafe { bindings::cpp_vm_cow_pages_get_ref_counted(self.as_raw()) };
        // SAFETY: `raw` points to the `fbl::RefCounted` base of `self`, which is suitably aligned
        // and lives as long as `self`.
        unsafe { raw.cast::<RefCounted>().as_ref_unchecked() }
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
        // initialized via `rust_vm_cow_pages_state_init`. It lives as long as `self`: `canary`,
        // `options`, `pmm_alloc_flags`, `lock_order`, and `page_source` are immutable after
        // construction (until `rust_vm_cow_pages_destruct`), and all fields mutated after
        // construction (`KCell` and `AtomicBool`) wrap `UnsafeCell`, so creating a shared
        // reference is sound.
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

    /// When acquiring multiple locks they must be acquired in order from lowest to highest. To
    /// support unidirectional clones, where nodes gain new children, and bidirectional clones,
    /// where nodes gain new parents, lock ordering is determined using the following scheme:
    ///  * A node with a page source, as it will always be the root, is given the highest order of
    ///    `LOCK_ORDER_ROOT`.
    ///  * The first anonymous node in a chain is given a lock order in the middle of
    ///    `LOCK_ORDER_FIRST_ANON`. This is nodes such as:
    ///    - Direct child of a root page source node.
    ///    - Direct Child of a hidden node.
    ///    - New anonymous root node.
    ///  * Children of visible anonymous nodes, i.e. unidirectional clones of a non-hidden non pager
    ///    backed node, take their parents lock order minus `LOCK_ORDER_DELTA`.
    ///  * Hidden nodes take either `LOCK_ORDER_ROOT`, if they are becoming the root node, or their
    ///    parents lock order minus `LOCK_ORDER_DELTA`.
    ///
    /// The goal of this scheme is to provide room in the numbering for both unidirectional children
    /// to grow down at the bottom, and hidden nodes to grow down in the middle, without colliding.
    /// If children of hidden nodes did not start at `LOCK_ORDER_FIRST_ANON`, but instead just took
    /// a minimum lock order, then a collision would occur if:
    ///  1. A pager backed node is created that then has a hidden node below it, with two anonymous
    ///     leaf nodes below it.
    ///  2. A new clone is created from one of those leafs that can hang directly off the hidden
    ///     node.
    ///  3. Both the original leaf nodes are closed, merging the remaining child with the hidden
    ///     node.
    ///  4. A unidirectional clone is now created from what is now a unidirectional hierarchy.
    ///
    /// Here, space is needed to grow down, as we have effectively found a way to promote a leaf
    /// child of a hidden node to being part of a unidirectional clone chain.
    ///
    /// Having a non-contiguous numbering allows for using an alternate lock ordering scheme during
    /// clone construction and dead transitions. When creating new nodes since there are no other
    /// references the lock cannot be held and so we cannot deadlock. However we still need to
    /// provide a lock order to satisfy lockdep. Here the gaps created by `LOCK_ORDER_DELTA` can be
    /// used as the order for these newly created nodes.
    ///
    /// During a dead transition we potentially need to hold locks of three nodes: the parent node
    /// and two of its children. Here the order is that the children must be acquired in list order,
    /// and then the parent. When acquiring the second child, since its lock order would be equal to
    /// the first child, the guaranteed gap between the first child and the parent lock order is
    /// used instead.
    const LOCK_ORDER_DELTA: u64 = 3;
    const LOCK_ORDER_ROOT: u64 = u64::MAX - Self::LOCK_ORDER_DELTA;
    const LOCK_ORDER_FIRST_ANON: u64 = u64::MAX / 2;

    /// Returns whether this cow pages node is ultimately backed by a user pager to fulfill initial
    /// content, and not zero pages. Contiguous VMOs have `page_source` set, but are not pager
    /// backed in this sense.
    ///
    /// This should only be used to report to user mode whether a VMO is user-pager backed, not for
    /// any other purpose.
    #[inline]
    pub fn is_root_source_user_pager_backed(&self) -> bool {
        self.options().contains(VmCowPagesOptions::USER_PAGER_BACKED_ROOT)
    }

    #[inline]
    pub fn is_parent_hidden_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> bool {
        self.parent_locked(token).is_some_and(Self::is_hidden)
    }

    /// `can_borrow()` returns true if the `VmCowPages` is capable of borrowing pages, but whether
    /// the `VmCowPages` should actually borrow pages also depends on a borrowing-site-specific flag
    /// that the caller is responsible for checking (in addition to checking `can_borrow()`). Only
    /// if both are true should the caller actually borrow at the caller's specific potential
    /// borrowing site.
    /// Aside from the general borrowing in the `PhysicalPageBorrowingConfig` being turned on and
    /// off, the ability to borrow is constant over the lifetime of the `VmCowPages`.
    #[inline]
    pub fn can_borrow(&self) -> bool {
        // TODO(dustingreen): Or rashaeqbal@. We can only borrow while the page is not dirty.
        // Currently we enforce this by checking should_trap_dirty_transitions() below and leaning
        // on the fact that !should_trap_dirty_transitions() dirtying isn't implemented yet. We
        // currently evict to reclaim instead of replacing the page, and we can't evict a dirty page
        // since the contents would be lost. Option 1: When a loaned page is about to become dirty,
        // we could replace it with a non-loaned page. Option 2: When reclaiming a loaned page we
        // could replace instead of evicting (this may be simpler).

        // Currently there needs to be a page source for any borrowing to be possible, due to
        // requirements of a backlink and other assumptions in the VMO code. Returning early here in
        // the absence of a page source simplifies the rest of the logic.
        let Some(page_source) = self.page_source() else {
            return false;
        };

        let source_is_suitable = page_source.properties().is_user_pager;

        // Avoid borrowing and trapping dirty transitions overlapping for now; nothing really stops
        // these from being compatible AFAICT - we're just avoiding overlap of these two things
        // until later.
        let overlapping_with_other_features = page_source.should_trap_dirty_transitions();

        source_is_suitable && !overlapping_with_other_features
    }

    /// Returns true if the `VmCowPages` can evict pages (i.e. is user-pager backed).
    #[inline]
    pub fn can_evict(&self) -> bool {
        self.page_source().is_some_and(|ps| ps.properties().is_user_pager)
    }

    /// Returns whether this cow pages node is dirty tracked.
    #[inline]
    pub fn is_dirty_tracked(&self) -> bool {
        // Pager-backed VMOs require dirty tracking either if they are directly backed by the pager,
        // i.e. the root VMO.
        self.page_source().is_some_and(|ps| ps.properties().is_user_pager)
    }

    /// The modified state is only supported for root pager-backed VMOs, and will get queried (and
    /// possibly reset) on the next `query_pager_vmo_stats_locked()` call. Although the modified
    /// state is only tracked for the root VMO.
    #[inline]
    pub fn mark_modified_locked(&self, token: &mut LockToken<'_, VmCowPagesLockClass>) {
        if !self.is_dirty_tracked() {
            return;
        }
        debug_assert!(self.page_source_type() == PageSourceType::UserPager);
        self.set_pager_stats_modified_locked(token, true);
    }

    #[inline]
    pub fn get_page_source_koid(&self) -> Option<zx_koid_t> {
        self.page_source()?.get_provider_koid()
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

    /// If page is still at offset, replace it with a different page. If `with_loaned` is true,
    /// replace with a loaned page. If `with_loaned` is false, replace with a non-loaned page and a
    /// `page_request` is required to be provided.
    ///
    /// # Safety
    ///
    /// * `before_page` must be a valid `VmPage`, and no reference into the page list of `self` may
    ///   be live.
    /// * `deferred` must have been constructed for `self`.
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok`, the returned page is a valid `VmPage` in the `OBJECT` state that has replaced
    /// `before_page` in the slot at `offset`; the page list is not otherwise modified.
    unsafe fn replace_page_locked(
        &self,
        _token: &mut LockToken<'_, VmCowPagesLockClass>,
        before_page: VmPagePtr,
        offset: u64,
        with_loaned: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Option<&mut AnonymousPageRequest>,
    ) -> Result<VmPagePtr, Status> {
        let mut after_page = ptr::null_mut();
        let page_request_ptr = match page_request {
            Some(page_request) => page_request.as_raw(),
            None => ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` is a valid `VmCowPages` whose lock `_token` proves is held, per
        // `# Safety` `before_page` is a valid page and `deferred` is the live `DeferredOps` of
        // `self`, `after_page` is valid for writing a page pointer, and `page_request_ptr` is null
        // or points to a live `AnonymousPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_replace_page_locked(
                self.as_raw(),
                before_page.as_ffi(),
                offset,
                with_loaned,
                &raw mut after_page,
                deferred.as_raw().cast(),
                page_request_ptr,
            )
        };
        Status::ok(status)?;
        // SAFETY: `ReplacePageLocked` returned `ZX_OK`, so it stored the replacement page, which
        // is a valid page, in `after_page`.
        Ok(unsafe { VmPagePtr::from_ffi_unchecked(after_page) })
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

    /// Returns the parent of this cow pages, may be null. Generally the parent should never be
    /// directly accessed externally, but this is exposed specifically for tests.
    pub fn debug_get_parent(&self) -> Option<RefPtr<VmCowPages>> {
        self.assert_canary();
        ksync::lock!(let guard = self.lock());
        self.parent_ref_locked(guard.token()).cloned()
    }

    /// See `VmObject::DebugLookupDepth`.
    pub fn debug_lookup_depth_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> u32 {
        self.assert_canary();
        // Count the number of parents we need to traverse to find the root, and call this our
        // lookup depth.
        let mut depth: u32 = 0;
        ksync::lock!(let ptr = LockedPtr::empty());
        while ptr.as_mut().step_to_parent(self, token) {
            depth += 1;
        }
        depth
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
        self.discardable_tracker().map(Pin::get_ref)
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

    /// Acquires the lock via pin-init for use with `ksync::lock!`.
    #[inline]
    pub fn lock(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, VmCowPagesLockClass, TrackedCriticalMutex>, Infallible> {
        self.lock_ref().lock()
    }

    #[inline]
    pub fn lock_ref(&self) -> &VmCowPagesLock {
        // SAFETY: `self.as_raw()` points to a live VmCowPages.
        let lock = unsafe { bindings::cpp_vm_cow_pages_get_lock(self.as_raw()) };
        let lock: *mut VmCowPagesLock = lock.cast();
        // SAFETY: `lock` is valid for reads for the lifetime of `self`.
        unsafe { lock.as_ref_unchecked() }
    }

    #[inline]
    pub fn lock_order(&self) -> u64 {
        #[cfg(with_lock_dep)]
        {
            self.state().lock_order
        }
        #[cfg(not(with_lock_dep))]
        {
            // When the lock order isn't in use just return a garbage value, whatever is calculated
            // using it will get thrown away regardless.
            0
        }
    }

    /// Adds a set of pages consecutively starting from the given offset. Regardless of the
    /// return result ownership of the pages is taken. Pages are assumed to be in the ALLOC state
    /// and can be optionally zeroed before inserting. `start_offset` must be page aligned.
    ///
    /// `overwrite` controls how the function handles pre-existing content in the range, however
    /// it is not valid to specify the `CanOverwriteSlot::PageOrRef` option, as any pages or
    /// compressed references that would get released as a consequence cannot be returned.
    ///
    /// # Safety
    ///
    /// * Every page in `pages` must be a live `VmPage` in the `ALLOC` state that is exclusively
    ///   owned by the caller and is not in any page queue or container.
    /// * If `zero` is true, each page's physical frame must be writable in the physmap with no
    ///   other access during the call; otherwise, if `self` is user-pager backed, each must be
    ///   readable in the physmap with no concurrent writes.
    /// * If `deferred` is `Some`, it must have been constructed for `self`.
    pub unsafe fn add_new_pages_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        mut pages: Pin<&mut VmPageDoublyLinkedList>,
        overwrite: CanOverwriteSlot,
        zero: bool,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) -> Result<(), Status> {
        assert!(overwrite != CanOverwriteSlot::PageOrRef);
        self.assert_canary();

        debug_assert!(::page::is_page_rounded(start_offset as usize));

        let mut offset = start_offset;
        while let Some(p) = pages.as_mut().pop_front() {
            let page = VmPagePtr::new(p);
            // Defer the range change update by passing None as we will do it in bulk at the end if
            // needed.
            // SAFETY: Per this function's `# Safety` contract `page`, which was popped from
            // `pages`, is a live `ALLOC`-state page owned by the caller.
            match unsafe { self.add_new_page_locked(token, offset, page, overwrite, zero, None) } {
                // `overwrite` excludes `PageOrRef`, so nothing that needs freeing is released.
                Ok(old) => debug_assert!(!old.is_page_or_ref()),
                Err(status) => {
                    // Put the page back on the list so that someone owns it and it'll get freed.
                    // SAFETY: `p` is not in any list.
                    unsafe { pages.as_mut().push_front_raw(p) };
                    // SAFETY: `pages` holds the caller's remaining `ALLOC`-state pages (per
                    // `# Safety`) and every slot in `[start_offset, offset)` was populated by
                    // `add_new_page_locked` above under `token`.
                    unsafe {
                        self.rollback_add_new_pages_locked(token, start_offset, offset, pages)
                    };
                    return Err(status);
                }
            }
            offset += ::page::SIZE as u64;
        }

        if let Some(deferred) = deferred {
            // other mappings may have covered this offset into the vmo, so unmap those ranges
            self.range_change_update_locked(
                token,
                VmCowRange::new(start_offset, offset - start_offset),
                RangeChangeOp::Unmap,
                Some(deferred),
            );
        }

        self.continuous_attribution_validation_assert(token);
        Ok(())
    }

    /// Failure path of [`Self::add_new_pages_locked`]: removes and frees the pages already placed
    /// in `[start_offset, offset)` and frees the remaining `pages`.
    ///
    /// Kept out of line so that the `ScopedPageFreedList` and `BatchPQRemove` locals are not
    /// reserved in `add_new_pages_locked`'s stack frame on the success path.
    ///
    /// # Safety
    ///
    /// Every page in `pages` must be a live `VmPage` in the `ALLOC` state that is exclusively
    /// owned by the caller and is not in the page queues, and every slot in
    /// `[start_offset, offset)` of `self.page_list` must hold a page added by
    /// `add_new_page_locked` under the lock proven by `token`.
    #[cold]
    #[inline(never)]
    unsafe fn rollback_add_new_pages_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        offset: u64,
        pages: Pin<&mut VmPageDoublyLinkedList>,
    ) {
        // Remove any pages we already placed.
        if offset > start_offset {
            let mut populated_slots_removed: u32 = 0;
            stack_pin_init!(let freed_list = ScopedPageFreedList::new());
            {
                let mut page_remover = BatchPQRemove::new(freed_list.as_mut());

                let _ = self.page_list_locked_mut(token).remove_pages(
                    start_offset,
                    offset,
                    |p, _off| {
                        // We only added pages, so we should only expect pages on rollback.
                        debug_assert!(p.is_page());
                        populated_slots_removed += 1;
                        // SAFETY: `p` is a slot of `self.page_list` handed to this closure by
                        // `remove_pages` while `token` proves `self`'s lock is held, and
                        // `freed_list` is freed against `self` below.
                        unsafe { page_remover.push_content(p) };
                        Status::NEXT
                    },
                );
                page_remover.flush();
            }
            // SAFETY: All pages placed in `freed_list` by `page_remover` were owned by `self` and
            // removed from `self.page_list` and the page queues above.
            unsafe { freed_list.free_pages(self) };
            if populated_slots_removed > 0 {
                self.continuous_attribution_tracker_locked_mut(token)
                    .decrement(populated_slots_removed);
            }
        }

        // Free all the pages back as we had ownership of them.
        // SAFETY: Per `# Safety`, `pages` contains valid `ALLOC`-state pages that are not in any
        // page queues.
        unsafe { self.free_pages(pages) };
    }

    /// Performs initial checks and slot allocations for inserting a new page at the specified
    /// `offset`.
    ///
    /// `overwrite` controls how the function handles pre-existing content at `offset`. If
    /// `overwrite` does not permit replacing the content, `Status::ALREADY_EXISTS` will be
    /// returned.
    ///
    /// On success the returned [`AddPageTransaction`] *must* be used in a call to either
    /// [`Self::complete_add_page_locked`], [`Self::complete_add_new_page_locked`], or
    /// [`Self::cancel_add_page_locked`].
    ///
    /// No other `page_list` operations should be performed until the transaction is complete, and
    /// starting or ending a transaction should be assumed to invalidate any `page_list` iterators
    /// or slots that have been looked up unless the caller explicitly knows otherwise.
    fn begin_add_page_locked<'a>(
        &'a self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        overwrite: CanOverwriteSlot,
    ) -> Result<AddPageTransaction<'a>, Status> {
        self.assert_canary();
        let mut interval_handling = IntervalHandling::NoIntervals;
        // If we're backed by a user pager, we cannot directly update empty slots in the
        // page list. An empty slot might lie in a sparse zero interval, which would
        // require splitting the interval around the required offset before it can be
        // manipulated.
        if self.page_source_type() == PageSourceType::UserPager {
            // We can overwrite zero intervals if we're allowed to overwrite zeros (or non-zeros).
            interval_handling = if overwrite != CanOverwriteSlot::Empty {
                IntervalHandling::SplitInterval
            } else {
                IntervalHandling::CheckForInterval
            };
        }
        // Take the slot as a node-derived raw pointer: the transaction outlives this call and
        // callers may perform non-mutating page-list lookups before completing it.
        let (slot_ptr, is_in_interval) =
            self.page_list_locked_mut(token).lookup_or_allocate_ptr(offset, interval_handling);
        if is_in_interval {
            // We should not have found an interval if we were not expecting any.
            debug_assert!(interval_handling != IntervalHandling::NoIntervals);
            // Return error if the offset lies in an interval but we cannot overwrite intervals.
            if interval_handling != IntervalHandling::SplitInterval {
                // The lookup should not have returned a slot for us to manipulate if it was in an
                // interval that cannot be overwritten, even if that slot was already populated (by
                // an interval sentinel).
                debug_assert!(slot_ptr.is_none());
                core::hint::cold_path();
                return Err(Status::ALREADY_EXISTS);
            }
            // If offset was in an interval, we should have an interval slot to overwrite at this
            // point.
            debug_assert!(slot_ptr.is_some_and(|p| {
                // SAFETY: `p` points to a valid `VmPageOrMarker` slot in `self.page_list`.
                unsafe { p.as_ref().is_interval_slot() }
            }));
        }

        let Some(slot_ptr) = slot_ptr else {
            core::hint::cold_path();
            return Err(Status::NO_MEMORY);
        };
        // SAFETY: `slot_ptr` points to a valid `VmPageOrMarker` slot in `self.page_list` while
        // `self.lock()` is held.
        let slot_ref = unsafe { slot_ptr.as_ref() };
        let is_empty = slot_ref.is_empty();

        // SAFETY: `slot_ref` is the live slot at `offset` in `self.page_list`.
        if let Err(status) =
            unsafe { self.check_overwrite_conditions_locked(token, offset, slot_ref, overwrite) }
        {
            core::hint::cold_path();
            if is_empty {
                self.page_list_locked_mut(token).return_empty_slot(offset);
            }
            return Err(status);
        }

        // SAFETY: `slot_ptr` is the live page-list slot at `offset` in `self.page_list` and
        // remains valid until the returned transaction is completed or cancelled.
        Ok(unsafe { AddPageTransaction::from_raw(slot_ptr, offset) })
    }

    /// Similar to [`Self::begin_add_page_locked`] except only the `overwrite` checks are performed
    /// and a ready-to-use slot is provided by the caller. It is a requirement by the caller to
    /// ensure that `slot`:
    ///  * Is for `offset` in this `page_list`
    ///  * Does not fall in the middle of an interval.
    ///
    /// All other requirements of [`Self::begin_add_page_locked`] otherwise apply.
    ///
    /// # Safety
    ///
    /// `slot` must point to the live `VmPageOrMarker` slot at `offset` in `self.page_list`, and
    /// `offset` must not fall in the middle of an interval. `*slot` must not be mutated (except
    /// through the returned transaction) until the transaction is completed or cancelled.
    #[inline]
    unsafe fn begin_add_page_with_slot_locked<'a>(
        &'a self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        slot: NonNull<VmPageOrMarker>,
        overwrite: CanOverwriteSlot,
    ) -> Result<AddPageTransaction<'a>, Status> {
        self.assert_canary();
        // SAFETY: Per `# Safety`, `slot` points to a valid `VmPageOrMarker` slot in
        // `self.page_list` while `self.lock()` is held.
        let slot_ref = unsafe { slot.as_ref() };
        // SAFETY: Per `# Safety`, `slot_ref` is the live slot at `offset` in `self.page_list`.
        if let Err(status) =
            unsafe { self.check_overwrite_conditions_locked(token, offset, slot_ref, overwrite) }
        {
            core::hint::cold_path();
            return Err(status);
        }
        // Do additional checks. The `is_offset_in_zero_interval` check is expensive, but the
        // assumption is that this method is not used when this node is directly backed by a user
        // pager, so the assertion should short circuit.
        debug_assert!(
            self.page_source_type() == PageSourceType::Anonymous
                || self.page_source_type() == PageSourceType::Contiguous
                || !slot_ref.is_empty()
                || !self.page_list_locked(token).is_offset_in_zero_interval(offset)
        );
        // SAFETY: `slot` is the live page-list slot at `offset` in `self.page_list` and remains
        // valid until the returned transaction is completed or cancelled.
        Ok(unsafe { AddPageTransaction::from_raw(slot, offset) })
    }

    /// Helper for checking the `overwrite` conditions on a given slot.
    ///
    /// # Safety
    ///
    /// `slot` must be the live `VmPageOrMarker` slot at `offset` in `self.page_list`.
    #[inline]
    unsafe fn check_overwrite_conditions_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        slot: &VmPageOrMarker,
        overwrite: CanOverwriteSlot,
    ) -> Result<(), Status> {
        // Pages can be added as part of Init, but not once we transition to dead.
        debug_assert!(self.life_cycle_locked(token) != LifeCycle::Dead);

        if offset >= self.size_locked(token) {
            return Err(Status::OUT_OF_RANGE);
        }

        // Make sure we're allowed to overwrite the VmPageOrMarker type in `slot`. See the comments
        // for CanOverwriteSlot for what each option means.
        if overwrite == CanOverwriteSlot::Empty && !slot.is_empty() {
            return Err(Status::ALREADY_EXISTS);
        }

        if overwrite == CanOverwriteSlot::EmptyOrParent
            && !slot.is_empty()
            && !slot.is_parent_content()
        {
            return Err(Status::ALREADY_EXISTS);
        }

        if overwrite == CanOverwriteSlot::ZeroMarkerOrInterval && slot.is_page_or_ref() {
            // If we have a page source, the page source should be able to validate the page.
            // Note that having a page source implies that any content must be an actual page and so
            // although we return an error for any kind of content, the debug check only gets run
            // for page sources where it will be a real page.
            debug_assert!(self.page_source().is_none_or(|ps| {
                slot.is_page() && {
                    // SAFETY: `slot.is_page()` holds, and per `# Safety` `slot` is in `self`'s
                    // page list under `self`'s lock, so `slot.page()` is a live `VmPage`.
                    unsafe { ps.debug_is_page_ok(slot.page(), offset) }
                }
            }));
            return Err(Status::ALREADY_EXISTS);
        }

        debug_assert!(overwrite == CanOverwriteSlot::PageOrRef || !slot.is_page_or_ref());
        Ok(())
    }

    /// Completes an add page transaction that had been started by inserting the provided page `p`
    /// into the slot looked up in `transaction`. Once complete the transaction must not be used in
    /// any other complete or cancel calls.
    ///
    /// `p` must not be empty.
    ///
    /// This operation unmaps the corresponding offset from any existing mappings, unless `deferred`
    /// is `None`, in which case it will skip updating mappings.
    ///
    /// Any previous content in the slot is returned and must be dealt with by the caller.
    ///
    /// # Safety
    ///
    /// * `transaction` was returned by `begin_add_page_locked` or `begin_add_page_with_slot_locked`
    ///   on `self` while the lock proven by `token` was held, is still active (not completed,
    ///   cancelled, or moved from), and no mutating page-list operation on `self` other than
    ///   through `transaction` has been performed since.
    /// * If `p` is a page, it is a valid `VmPage` in the `OBJECT` state (`initialize_vm_page` has
    ///   run), exclusively owned by the caller and not in any page queue; if `p` is a reference, it
    ///   is a valid compressed reference owned by the caller.
    unsafe fn complete_add_page_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        transaction: &mut AddPageTransaction<'_>,
        p: VmPageOrMarker,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) -> VmPageOrMarker {
        if p.is_page() {
            let page = p.page();
            ltracef!(
                "vmo {:p}, offset {:#x}, page {:p} ({:#x})\n",
                self.as_raw(),
                transaction.offset(),
                page.as_non_null().as_ptr(),
                p.page_as_paddr().0
            );
            // If the new page is an actual page and we have a page source, the page source should
            // be able to validate the page.
            // Note that having a page source implies that any content must be an actual page and
            // so although we return an error for any kind of content, the debug check only gets
            // run for page sources where it will be a real page.
            debug_assert!(self.page_source().is_none_or(|ps| {
                // SAFETY: Per `# Safety`, `page` is a live `VmPage` owned by the caller.
                unsafe { ps.debug_is_page_ok(page, transaction.offset()) }
            }));
            // Since this is a real page, we need to place it into the appropriate queue.
            // SAFETY: Per `# Safety`, `page` is a valid `VmPage` in the `OBJECT` state being added
            // to `self` and not yet in any page queue (freshly initialized by
            // `complete_add_new_page_locked` or released from a slot).
            unsafe {
                debug_assert_eq!(page.as_ref().state(), VmPageState::OBJECT);
                debug_assert_eq!(page.get_pin_count(), 0);
                self.set_not_pinned_locked(token, page, transaction.offset());
            }
        } else if p.is_reference() {
            ltracef!(
                "vmo {:p}, offset {:#x}, reference {:#x}\n",
                self.as_raw(),
                transaction.offset(),
                p.reference().value()
            );
            // A page source implies that any content must be an actual page.
            debug_assert!(self.page_source().is_none());
        } else {
            debug_assert!(p.is_marker());
            ltracef!("vmo {:p}, offset {:#x}, marker\n", self.as_raw(), transaction.offset());
            // Markers should never be placed in a node that uses parent content markers, since
            // doing so is completely redundant and any attempt to do so represents a logic bug
            // somewhere.
            debug_assert!(!self.node_has_parent_content_markers());
        }
        // `p` is either a page, reference, or a marker.
        let after_is_populated = p.is_page_or_ref();

        // SAFETY: Per `# Safety`, `transaction` is active for `self.page_list`, `token` proves
        // `self`'s lock is held, no mutating page-list operation has occurred since `transaction`
        // began, and ownership of `p` transfers to the slot.
        let old = unsafe { transaction.complete(p) };

        let before_was_populated = old.is_page_or_ref() || old.is_parent_content();
        if before_was_populated && !after_is_populated {
            self.continuous_attribution_tracker_locked_mut(token).decrement(1);
        } else if !before_was_populated && after_is_populated {
            self.continuous_attribution_tracker_locked_mut(token).increment(1);
        }

        if let Some(deferred) = deferred {
            // If the old entry is a reference then we know that there can be no mappings to it,
            // since a reference cannot be mapped in, and we can skip the range update.
            if !old.is_reference() {
                if old.is_empty() && self.page_source_type() == PageSourceType::UserPager {
                    // An empty slot where the page source is a user pager cannot have any mappings,
                    // either in self or the children, since the content is unknown (i.e. not the
                    // zero page), and so we do not need to perform any range change update.
                    // However, as we are modifying the contents we still must synchronize with any
                    // other modification to this hierarchy, which we know is true because a `Some`
                    // `deferred` was passed in.
                } else {
                    // other mappings may have covered this offset into the vmo, so unmap those
                    // ranges.
                    self.range_change_update_locked(
                        token,
                        VmCowRange::new(transaction.offset(), ::page::SIZE as u64),
                        RangeChangeOp::Unmap,
                        Some(deferred),
                    );
                }
            }
        }

        old
    }

    /// Cancels an add page transaction and potentially frees the unused slot. It is required to
    /// call this, instead of dropping an [`AddPageTransaction`] to ensure the page list does not
    /// gather empty nodes. If the transaction was created with
    /// [`Self::begin_add_page_with_slot_locked`] then it is the responsibility of the caller to
    /// know whether their slot might still be valid or not after cancelling. Once cancelled the
    /// transaction must not have another further complete or cancel calls against it.
    ///
    /// # Safety
    ///
    /// `transaction` was begun on `self` under the lock proven by `token`, is still active (not
    /// completed, cancelled, or moved from), and no mutating page-list operation on `self` other
    /// than through `transaction` has been performed since.
    #[inline]
    unsafe fn cancel_add_page_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        transaction: &mut AddPageTransaction<'_>,
    ) {
        // SAFETY: Caller upholds `AddPageTransaction::cancel` preconditions.
        unsafe { transaction.cancel(self.page_list_locked_mut(token)) };
    }

    /// Adds an allocated page to this cow pages at the specified offset, can be optionally zeroed
    /// and any mappings invalidated. If an error is returned the caller retains ownership of
    /// `page`. Offset must be page aligned. Mappings being invalidated is controlled by `deferred`
    /// where if `None` is passed then no mappings, in this object or any child, will be
    /// invalidated. If `deferred` is `Some` then mappings will be invalidated if necessary, both
    /// immediately in this object, and via `deferred` for the children.
    ///
    /// `overwrite` controls how the function handles pre-existing content at `offset`. If
    /// `overwrite` does not permit replacing the content, `Status::ALREADY_EXISTS` will be
    /// returned. Any content released from the page list as a result of overwriting is returned
    /// and the caller takes ownership of it.
    ///
    /// # Safety
    ///
    /// * `page` must be a valid `VmPage` in the `ALLOC` state, exclusively owned by the caller and
    ///   not in any page queue or container.
    /// * If `zero` is true, `page`'s physical frame must be writable in the physmap with no other
    ///   access during the call; otherwise, if `self` is user-pager backed, it must be readable in
    ///   the physmap with no concurrent writes.
    /// * If `deferred` is `Some`, it must have been constructed for `self`.
    #[inline]
    pub unsafe fn add_new_page_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        page: VmPagePtr,
        overwrite: CanOverwriteSlot,
        zero: bool,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) -> Result<VmPageOrMarker, Status> {
        self.assert_canary();

        let mut transaction = self.begin_add_page_locked(token, offset, overwrite)?;
        // SAFETY: `transaction` was begun immediately above on `self` with no intervening
        // page-list operation, and caller guarantees `page` is a valid `VmPage` in the `ALLOC`
        // state.
        Ok(unsafe {
            self.complete_add_new_page_locked(token, &mut transaction, page, zero, deferred)
        })
    }

    /// Similar to [`Self::complete_add_page_locked`] except a `VmPagePtr` is provided that is
    /// assumed to not yet be in the `OBJECT` state; this page may also be optionally zeroed.
    ///
    /// # Safety
    ///
    /// * `transaction` was begun on `self` under the lock proven by `token`, is still active (not
    ///   completed, cancelled, or moved from), and no mutating page-list operation on `self` other
    ///   than through `transaction` has been performed since.
    /// * `page` must be a valid `VmPage` in the `ALLOC` state, exclusively owned by the caller and
    ///   not in any page queue or container.
    /// * If `zero` is true, `page`'s physical frame must be writable in the physmap with no other
    ///   access during the call; otherwise, if `self` is user-pager backed, it must be readable in
    ///   the physmap with no concurrent writes.
    /// * If `deferred` is `Some`, it must have been constructed for `self`.
    unsafe fn complete_add_new_page_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        transaction: &mut AddPageTransaction<'_>,
        page: VmPagePtr,
        zero: bool,
        deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) -> VmPageOrMarker {
        debug_assert!(::page::is_page_rounded(transaction.offset() as usize));

        // SAFETY: Caller guarantees `page` is a valid, exclusively owned `VmPage` in the `ALLOC`
        // state.
        unsafe {
            initialize_vm_page(page);
            if zero {
                zero_page(page);
            }
        }

        // Pages being added to pager backed VMOs should have a valid dirty_state before being added
        // to the page list, so that they can be inserted in the correct page queue. New pages start
        // off clean.
        if self.page_source_type() == PageSourceType::UserPager {
            // Only zero pages can be added as new pages to pager backed VMOs.
            debug_assert!(
                zero || {
                    // SAFETY: Per `# Safety`, `page` is a live `VmPage` whose physmap page is
                    // readable with no concurrent writes.
                    unsafe { is_zero_page(page) }
                }
            );
            // SAFETY: `page` is in the `OBJECT` state and being added at `transaction.offset()`.
            unsafe {
                self.update_dirty_state_locked(
                    token,
                    page,
                    transaction.offset(),
                    DirtyState::Clean,
                    /*is_pending_add=*/ true,
                );
            }
        }
        // SAFETY: `transaction` is valid per `# Safety`, and `page` was just initialized to
        // `OBJECT` state and is not in any page queue.
        unsafe {
            self.complete_add_page_locked(
                token,
                transaction,
                VmPageOrMarker::from_page(page),
                deferred,
            )
        }
    }

    /// Helper function that 'forks' a page into `offset` of the current node, which must be a
    /// visible node. If this function successfully inserts the page, it returns the page.
    /// If `Err(Status::SHOULD_WAIT)` is returned then this indicates a transient allocation
    /// failure that should be resolved by waiting on the `page_request` and retrying.
    ///
    /// The source page that is being forked has already been calculated - it is `page`, which
    /// is currently in `page_owner` at offset `owner_offset`.
    ///
    /// This function is responsible for ensuring that COW clones never result in worse memory
    /// consumption than simply creating a new VMO and memcpying the content. If `page` is not
    /// shared at all, then this function assumes it is only accessible to this node. In that case
    /// `page` is removed from `page_owner` and migrated into this node. Forking the page in that
    /// case would just make `page` inaccessible, leaving `page` committed for no benefit.
    ///
    /// To handle memory allocation failure, this function allocates a slot in this node for the
    /// page before modifying the source page or page list in `page_owner`. If that allocation
    /// fails, then these are not altered.
    ///
    /// `page` must not be the zero-page, as there is no need to do the complex page fork logic to
    /// reduce memory consumption in that case.
    ///
    /// # Safety
    ///
    /// * `page` must be the page in the slot at `owner_offset` of the page list of `page_owner`,
    ///   whose lock `owner_token` proves is held, and no reference or cursor into that page list
    ///   may be used after this returns successfully (the page may have been removed from it).
    /// * `page_owner` must not be `self`.
    /// * `page_owner` must be a hidden node: `page` is copied from it, or moved from it into
    ///   `self`, without being unmapped.
    /// * Any page in `alloc_list` must be a valid `VmPage` in the `ALLOC` state that the caller
    ///   conceptually owns.
    /// * `deferred` must have been constructed for `self`.
    #[expect(clippy::too_many_arguments)]
    unsafe fn clone_cow_page_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        alloc_list: Option<Pin<&mut VmPageDoublyLinkedList>>,
        page_owner: &VmCowPages,
        owner_token: &mut LockToken<'_, VmCowPagesLockClass>,
        page: VmPagePtr,
        owner_offset: u64,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: &mut AnonymousPageRequest,
    ) -> Result<VmPagePtr, Status> {
        debug_assert!(self.lock_is_held());
        debug_assert!(page_owner.lock_is_held());
        debug_assert!(page != vm_get_zero_page());
        debug_assert!(self.has_parent_locked(token));
        // We only clone pages from hidden to visible nodes.
        debug_assert!(page_owner.is_hidden());
        debug_assert!(!self.is_hidden());
        // We don't want to handle intervals here. They should only be present when this node is
        // backed by a user pager, and such nodes don't have parents so cannot be the target of a
        // forked page.
        debug_assert!(self.page_source_type() == PageSourceType::Anonymous);

        // Ensure this node is ready to accept a newly-allocated page. If a subsequent step fails
        // (such as allocating the page itself), cancelling the `page_transaction` will handle any
        // rollback logic.
        //
        // By the time this function returns, the transaction will be either completed or canceled.
        let mut page_transaction =
            self.begin_add_page_locked(token, offset, CanOverwriteSlot::EmptyOrParent)?;

        let out_page;
        // If the page is shared we must fork it, otherwise we can migrate it.
        // SAFETY: Per `# Safety` `page` is in the page list of `page_owner`, so it is in the
        // `OBJECT` state, and `owner_token` proves the lock guarding its share count is held.
        let share_count = unsafe { page.get_share_count() };
        if share_count > 0 {
            // Create a fork of the page. This may fail due to inability to allocate a new page.
            // The page is not writable so there is no need to unmap or protect it before reading it
            // for the fork.
            // SAFETY: Per `# Safety` `page` is a live page.
            let page_paddr = unsafe { page.paddr() };
            // SAFETY: Per `# Safety` the pages of `alloc_list` are `ALLOC`-state pages owned by
            // the caller, and `page_paddr` is a page of the hidden node `page_owner`, which is not
            // writable and stays in that node's page list, and so allocated and in the physmap,
            // while `owner_token` is borrowed.
            let forked_page = match unsafe {
                self.allocate_copy_page(page_paddr, alloc_list, Some(page_request))
            } {
                Ok(forked_page) => forked_page,
                Err(status) => {
                    core::hint::cold_path();
                    // SAFETY: `page_transaction` was begun on `self` above under the lock
                    // `token` proves is held, is still active, and no page list operation on
                    // `self` has been performed since.
                    unsafe { self.cancel_add_page_locked(token, &mut page_transaction) };
                    return Err(status);
                }
            };

            // The page is now shared one less time.
            // SAFETY: As for `get_share_count` above; `owner_token` is exclusively borrowed, so
            // nothing else accesses the share count.
            unsafe { page.set_share_count(share_count - 1) };

            out_page = forked_page;
        } else {
            // Remove the page from the owner.
            let mut removed =
                page_owner.page_list_locked_mut(owner_token).remove_content(owner_offset);
            page_owner.continuous_attribution_tracker_locked_mut(owner_token).decrement(1);
            let removed_page = removed.release_page();
            debug_assert!(removed_page == page);
            // TODO: This could be optimized to a `change_object_offset` instead of doing a `remove`
            // here and an insert in `complete_add_page_locked`.
            // SAFETY: `removed_page` was owned by `page_owner`, under its lock, and so in the page
            // queues.
            unsafe { pmm_page_queues().remove(removed_page) };

            out_page = removed_page;

            // Page is being migrated from a once-pinned hidden parent into a sibling child.
            // Sibling child must now carry the `ever_pinned` flag so that delayed reuse is
            // correctly triggered if it later frees the page.
            if page_owner.ever_pinned_locked(owner_token) {
                self.set_ever_pinned_locked(token, true);
            }
        }

        // Now that we can no longer fail to insert the new page into this node, complete the add
        // page transaction.
        //
        // If the new page is different from the original page, then we must remove the original
        // page from any mappings that reference this node or its descendants.
        let do_range_update = out_page != page;
        // SAFETY: `page_transaction` was begun on `self` above under the lock `token` proves is
        // held, is still active, and no page list operation on `self` has been performed since: the
        // removal above was from the page list of `page_owner`, which per `# Safety` is not `self`.
        // `out_page` is an `OBJECT`-state page owned by this function that is in no page queue:
        // either freshly forked, or just removed from `page_owner` and the page queues. Per
        // `# Safety` `deferred` was constructed for `self`.
        let prev_content = unsafe {
            self.complete_add_page_locked(
                token,
                &mut page_transaction,
                VmPageOrMarker::from_page(out_page),
                if do_range_update { Some(deferred) } else { None },
            )
        };
        // We should not have been trying to fork at this offset if something already existed.
        debug_assert!(prev_content.is_empty() || prev_content.is_parent_content());

        Ok(out_page)
    }

    /// Forks the zero marker `marker`, which is in `marker_owner` at offset `owner_offset`, into
    /// `offset` of the current node, which must be a visible node, and returns the new page.
    /// `marker_owner` must be a hidden node. If `Err(Status::SHOULD_WAIT)` is returned then this
    /// indicates a transient allocation failure that should be resolved by waiting on the
    /// `page_request` and retrying.
    ///
    /// # Safety
    ///
    /// * `marker` must point to the slot at `owner_offset` of the page list of `marker_owner`,
    ///   whose lock `owner_token` proves is held, the slot must hold a marker, and no reference to
    ///   the slot may be live. No reference or cursor into that page list may be used after this
    ///   returns successfully (the marker may have been removed from it).
    /// * `marker_owner` must not be `self`.
    /// * Any page in `alloc_list` must be a valid `VmPage` in the `ALLOC` state that the caller
    ///   conceptually owns.
    /// * `deferred` must have been constructed for `self`.
    #[expect(clippy::too_many_arguments)]
    unsafe fn fork_marker_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        alloc_list: Option<Pin<&mut VmPageDoublyLinkedList>>,
        marker_owner: &VmCowPages,
        owner_token: &mut LockToken<'_, VmCowPagesLockClass>,
        mut marker: NonNull<VmPageOrMarker>,
        owner_offset: u64,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: &mut AnonymousPageRequest,
    ) -> Result<VmPagePtr, Status> {
        debug_assert!(self.lock_is_held());
        debug_assert!(marker_owner.lock_is_held());
        debug_assert!({
            // SAFETY: Per `# Safety` `marker` points to a live slot with no `&mut` to it.
            unsafe { marker.as_ref() }.is_marker()
        });

        debug_assert!(self.has_parent_locked(token));
        // We only clone pages from hidden to visible nodes.
        debug_assert!(marker_owner.is_hidden());
        debug_assert!(!self.is_hidden());
        // We don't want to handle intervals here. They should only be present when this node is
        // backed by a user pager, and such nodes don't have parents so cannot be the target of a
        // forked page.
        debug_assert!(self.page_source_type() == PageSourceType::Anonymous);

        // Fork the zero page.
        // SAFETY: Per `# Safety` the pages of `alloc_list` are `ALLOC`-state pages owned by the
        // caller, and the source is the zero page.
        let out_page = unsafe {
            self.allocate_copy_page(vm_get_zero_page_paddr(), alloc_list, Some(page_request))
        }?;

        // Insert zero-page into our page list.
        let mut page_transaction =
            match self.begin_add_page_locked(token, offset, CanOverwriteSlot::ZeroMarkerOrInterval)
            {
                Ok(page_transaction) => page_transaction,
                Err(status) => {
                    core::hint::cold_path();
                    // SAFETY: The contract of `rollback_allocated_page` is that of `free_page`:
                    // `out_page` was allocated for `self` above, is still owned by this function,
                    // and is in no page queue or list.
                    unsafe { self.rollback_allocated_page(out_page) };
                    return Err(status);
                }
            };

        // SAFETY: `page_transaction` was begun on `self` just above with no page list operation
        // since, and `out_page` is an `OBJECT`-state page owned by this function that is in no page
        // queue.
        let _old = unsafe {
            self.complete_add_page_locked(
                token,
                &mut page_transaction,
                VmPageOrMarker::from_page(out_page),
                None,
            )
        };

        self.range_change_update_locked(
            token,
            VmCowRange::new(offset, ::page::SIZE as u64),
            RangeChangeOp::UnmapZeroPage,
            Some(deferred),
        );

        // Move to the correct queue.
        if self.is_discardable() {
            // SAFETY: `out_page` was just added to `self`, which placed it in the page queues.
            unsafe { pmm_page_queues().move_anonymous_to_anonymous_zero_fork(out_page) };
        }

        // Marker is now shared one less time.
        // SAFETY: Per `# Safety` `marker` points to a live slot with no `&mut` to it. The slot is
        // still live: the page list operations above were on `self`, which per `# Safety` is not
        // `marker_owner`.
        if unsafe { marker.as_ref() }.marker_share_count() == 0 {
            // Remove marker from owner
            let _ = marker_owner.page_list_locked_mut(owner_token).remove_content(owner_offset);
        } else {
            // SAFETY: Per `# Safety` `marker` points to a slot of `marker_owner`'s page list that
            // no other reference is live to, which is still live as explained above, and
            // `owner_token`, proving that node's lock is held, is exclusively borrowed.
            unsafe { marker.as_mut() }.decrement_marker_share_count();
        }

        Ok(out_page)
    }

    /// Helper function for reducing the share count of content in a hidden node, and freeing it if
    /// it is no longer referenced.
    ///
    /// This method assumes that the caller is overriding the slot in a child and that it will
    /// perform any necessary range change updates etc.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `content` must point to the live `VmPageOrMarker` slot at `offset` in `self.page_list`,
    ///   and no reference to `self.page_list`, to one of its nodes or to one of its slots may be
    ///   live across the call. `content` is a raw pointer because, when the share count is zero,
    ///   this takes `&mut self.page_list` and `remove_content` empties `*content` and may free its
    ///   node.
    /// * The slot must hold a page or a compressed reference. A page must be a valid `VmPage` in
    ///   the `OBJECT` state that is in the page queues and is not loaned (only `debug_assert!`ed).
    ///   A reference must be a valid compressed reference that belongs to the `VmCompression`
    ///   given in `compression` (with `None` this panics).
    /// * `self` must be a hidden node (only `debug_assert!`ed), and the caller must be giving up a
    ///   reference to the content that its share count accounts for: a share count of zero means
    ///   that it was the last one, and the content is then removed from `self` and freed.
    /// * `list` must only later be freed against `self` or another `VmCowPages` in the same
    ///   hierarchy.
    unsafe fn decrement_cow_content_share_count(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        content: NonNull<VmPageOrMarker>,
        offset: u64,
        list: Pin<&mut ScopedPageFreedList>,
        compression: Option<&VmCompression>,
    ) {
        debug_assert!(self.lock_is_held());
        // Only hidden nodes have content with a non-zero share count.
        debug_assert!(self.is_hidden());

        // SAFETY: Per `# Safety`, `content` points to a live slot in `self.page_list` and no other
        // reference to it is live. This shared borrow is last used to read the page or reference
        // out of the slot, before any `remove_content` mutation below.
        let content_ref = unsafe { content.as_ref() };

        // Release the reference we held to the forked page.
        if content_ref.is_page() {
            let page = content_ref.page();
            // SAFETY: Per `# Safety`, `page` is in `self.page_list` in the `OBJECT` state and
            // `token` is for `self`'s lock.
            let share_count = unsafe { page.get_share_count() };
            if share_count > 0 {
                // The page is now shared one less time.
                // SAFETY: As above; `token` is exclusively borrowed.
                unsafe { page.set_share_count(share_count - 1) };
            } else {
                // Remove the page from the owner.
                let mut removed = self.page_list_locked_mut(token).remove_content(offset);
                let removed_page = removed.release_page();
                self.continuous_attribution_tracker_locked_mut(token).decrement(1);
                debug_assert_eq!(removed_page, page);
                // SAFETY: `removed_page` was owned by `self` and in the page queues.
                unsafe { pmm_page_queues().remove(removed_page) };
                // SAFETY: `page` was owned by `self` under `self`'s lock.
                debug_assert!(!unsafe { page.is_loaned() });

                // SAFETY: `page` was just removed from the page queues above, so it is not in any
                // list, per `# Safety` it is not loaned, and per `# Safety` `list` will be freed
                // against `self`'s hierarchy.
                unsafe { list.list().push_back_raw(page.as_non_null()) };
            }
        } else {
            debug_assert!(content_ref.is_reference());
            let reference = content_ref.reference();
            let compression = compression.expect("reference requires page compression");
            // SAFETY: Per `# Safety`, `reference` is a valid compressed reference of `compression`
            // in `self`'s page list, and `token` is for `self`'s lock.
            let prev = unsafe { compression.get_metadata(reference) };
            if prev > 0 {
                // SAFETY: As above.
                unsafe { compression.set_metadata(reference, prev - 1) };
            } else {
                let mut removed = self.page_list_locked_mut(token).remove_content(offset);
                // SAFETY: `removed.release_reference()` was just removed from `self`'s page list
                // with share count 0 under `self`'s lock, so it is owned by this function and not
                // used again.
                unsafe { compression.free(removed.release_reference()) };
                self.continuous_attribution_tracker_locked_mut(token).decrement(1);
            }
        }
    }

    /// See `VmObject::get_attributed_memory_in_range`.
    #[inline]
    pub fn get_attributed_memory_in_range_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> AttributionCounts {
        self.assert_canary();

        debug_assert!(!self.is_hidden());

        let compression = pmm::get_page_compression();

        // Accumulate bytes for all pages and references this node has ownership over.
        let mut counts = AttributionCounts::zero();
        let status = self.for_every_owned_hierarchy_page_in_range_locked(
            token,
            |p, owner, _this_offset, _owner_offset| {
                // Capture `counts` as a whole, so that the closure holds a single pointer to it
                // rather than one pointer per accumulator field used below.
                let counts = &mut counts;
                // The short-circuit condition of (owner == this) greatly improves performance by
                // removing the need to dereference 'random' vm_page_ts/references in the common
                // case, greatly reducing memory stalls. For this reason the get_share_count is a
                // callback, and not a value.
                #[inline]
                fn do_attribution(
                    owner_is_self: bool,
                    get_share_count: impl FnOnce() -> u32,
                    bytes: &mut usize,
                    private_bytes: &mut usize,
                    scaled_bytes: &mut FractionalBytes,
                ) {
                    let share_count = if owner_is_self { 0 } else { get_share_count() };
                    if share_count == 0 {
                        *bytes += ::page::SIZE;
                        *private_bytes += ::page::SIZE;
                        *scaled_bytes += ::page::SIZE as u64;
                    } else {
                        // An unshared (i.e. private) page has a share count of 0, add 1 to get the
                        // number of owners and scale the full page by this.
                        let scaled_contribution = FractionalBytes::from_fraction(
                            ::page::SIZE as u64,
                            u64::from(share_count) + 1,
                        );
                        *bytes += ::page::SIZE;
                        *scaled_bytes += scaled_contribution;
                    }
                }

                let owner_is_self = ptr::eq(owner, self);
                if p.is_page() {
                    do_attribution(
                        owner_is_self,
                        || {
                            // SAFETY: `p` is a page in `owner`'s page list in the `OBJECT` state
                            // and `owner`'s lock is held by the walk.
                            unsafe { p.page().get_share_count() }
                        },
                        &mut counts.uncompressed_bytes,
                        &mut counts.private_uncompressed_bytes,
                        &mut counts.scaled_uncompressed_bytes,
                    );
                } else if p.is_reference() {
                    do_attribution(
                        owner_is_self,
                        || {
                            let compression = compression.expect("compression must be present");
                            // SAFETY: `p` holds a live reference owned by `owner`'s page list, and
                            // `owner`'s lock is held by the walk.
                            unsafe { compression.get_metadata(p.reference()) }
                        },
                        &mut counts.compressed_bytes,
                        &mut counts.private_compressed_bytes,
                        &mut counts.scaled_compressed_bytes,
                    );
                }
                Status::NEXT
            },
            range.offset,
            range.len,
        );
        debug_assert!(status.is_ok());

        counts
    }

    /// See `for_every_owned_hierarchy_page_in_range`. Each entry given to `func` is constant and
    /// may not be modified in any way.
    ///
    /// `token` must be for the lock of `self`.
    #[inline]
    fn for_every_owned_hierarchy_page_in_range_locked<T>(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        mut func: T,
        offset: u64,
        size: u64,
    ) -> Result<(), Status>
    where
        T: for<'s> FnMut(&'s VmPageOrMarker, &VmCowPages, u64, u64) -> Status,
    {
        debug_assert!(self.lock_is_held());
        let mut ctx = ();
        // SAFETY: `ConstEntry` only reads `cur`'s page list via shared reference (`&VmPageList`).
        // For the token, this safe function relies on its callers passing `token` as its doc says,
        // as every safe `*_locked` method relies on its `token`. No pre-locked parent is passed.
        unsafe {
            self.for_every_owned_hierarchy_page_in_range::<ConstEntry, _, _, _>(
                token,
                &mut ctx,
                |_, p, owner, this_off, owner_off| func(p, owner, this_off, owner_off),
                |_, _| {},
                offset,
                size,
                None,
            )
        }
    }

    /// Iterates owned hierarchy entries in `[offset, offset + size)`, giving `func` a mutable
    /// entry that it may modify or replace with an empty entry, and calling `on_node_done` after
    /// traversing each node while that node's lock is still held.
    ///
    /// See `for_every_owned_hierarchy_page_in_range`.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`, and the token in `parent` (if any) for the lock
    ///   of the parent it is paired with: a `LockToken` only proves that some `VmCowPages` lock
    ///   is held.
    /// * `func` must not access the page list of the node it is called for (`owner`) other than
    ///   through the `&mut VmPageOrMarker` passed to it.
    /// * No reference or cursor into the page list of `self` may be live across the call (obtained
    ///   before it and used during or after it).
    /// * If `parent` is `Some`, the same applies to the page list of that parent: the walk mutates
    ///   it, although the token passed for it is shared.
    ///
    /// # Safety-usable invariant
    ///
    /// While `func` or `on_node_done` is running, the lock of the `owner` passed to it is held,
    /// and `on_node_done` is invoked for each traversed node before the walk releases that node's
    /// lock.
    #[inline]
    #[expect(clippy::too_many_arguments)]
    unsafe fn remove_owned_hierarchy_pages_in_range_locked<Ctx, T, D>(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        ctx: &mut Ctx,
        func: T,
        on_node_done: D,
        offset: u64,
        size: u64,
        parent: Option<(&VmCowPages, &LockToken<'_, VmCowPagesLockClass>)>,
    ) -> Result<(), Status>
    where
        T: for<'s> FnMut(&mut Ctx, &'s mut VmPageOrMarker, &VmCowPages, u64, u64) -> Status,
        D: FnMut(&mut Ctx, &VmCowPages),
    {
        debug_assert!(self.lock_is_held());
        // SAFETY: The contract of `for_every_owned_hierarchy_page_in_range` for `MutEntry` is this
        // function's `# Safety`.
        unsafe {
            self.for_every_owned_hierarchy_page_in_range::<MutEntry, Ctx, T, D>(
                token,
                ctx,
                func,
                on_node_done,
                offset,
                size,
                parent,
            )
        }
    }

    /// Iterates a range within a visible node, invoking a callback for every `VmPageListEntry` the
    /// node owns (fully or partially) in that range.
    ///
    /// The callback is invoked at most once per each offset within the range, as the node can own
    /// at most one entry at each offset. Either:
    ///  * The node directly contains the first visible entry at the offset and thus fully owns it.
    ///  * A hidden parent contains the first visible entry at the offset and thus the node
    ///    partially owns it.
    ///  * A visible parent contains the first visible entry at the offset and thus that parent
    ///    fully owns it. This method doesn't evaluate such parents and skips the offset.
    ///  * There is no visible entry at the offset. This method skips the offset.
    ///
    /// Prefer using the non-generic methods above over invoking this function directly.
    ///
    /// The caller provides:
    ///  * `self`: Node to begin the iteration from. It must be a visible node.
    ///  * `func`: Callback function invoked for each non-empty entry.
    ///  * `offset`: Offset relative to `self` to begin iterating at.
    ///  * `size`: Size of the range to iterate.
    ///  * `parent`: If the caller has locked the immediate parent, then it can pass the parent and
    ///    a token for its lock in here to avoid double locking, otherwise if no parent or not
    ///    locked `None` can be given.
    ///
    /// The type `P` selects the entry type handed to `func` and the matching page list traversal;
    /// see [`OwnedHierarchyWalk`].
    /// The type `T` must be:
    /// `FnMut(P::Entry<'_>, owner: &VmCowPages, self_offset: u64, owner_offset: u64) -> Status`
    ///  The return value controls whether iteration continues:
    ///   * `Status::NEXT`: Continue iteration or stop with `Ok(())` if no more entries to iterate.
    ///   * `Status::STOP`: Stop iteration immediately with `Ok(())`.
    ///   * Any other code: Stop iteration immediately with that error code.
    ///
    /// Returns `Ok(())` if no `func` invocations returned any errors, otherwise returns the error
    /// code from the failing `func` invocation.
    ///
    /// # Safety
    ///
    /// `token` must be for the lock of `self`, and the token in `parent` (if any) for the lock of
    /// the parent it is paired with: a `LockToken` only proves that some `VmCowPages` lock is
    /// held.
    ///
    /// If `P` mutates slots in the walked nodes' page lists (`MutEntry`):
    ///
    /// * `func` must not access the page list of the node it is called for (`owner`) other than
    ///   through the entry passed to it.
    /// * No reference or cursor into the page list of `self`, or of the parent given in `parent`
    ///   (if any), may be live across the call (obtained before it and used during or after it).
    ///   `token` and the token in `parent` are shared, so they do not rule such a reference out.
    ///   Every other node the walk visits is an ancestor whose lock the walk acquires itself.
    ///
    /// # Safety-usable invariant
    ///
    /// While `func` or `on_node_done` is running, the lock of the `owner` passed to it is held,
    /// and `on_node_done` is invoked for each traversed node before the walk releases that node's
    /// lock.
    #[inline]
    #[expect(clippy::too_many_arguments)]
    unsafe fn for_every_owned_hierarchy_page_in_range<P: OwnedHierarchyWalk, Ctx, T, D>(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        ctx: &mut Ctx,
        mut func: T,
        mut on_node_done: D,
        offset: u64,
        size: u64,
        parent: Option<(&VmCowPages, &LockToken<'_, VmCowPagesLockClass>)>,
    ) -> Result<(), Status>
    where
        T: for<'s> FnMut(&mut Ctx, P::Entry<'s>, &VmCowPages, u64, u64) -> Status,
        D: FnMut(&mut Ctx, &VmCowPages),
    {
        debug_assert!(::page::is_page_rounded(offset as usize));
        debug_assert!(::page::is_page_rounded(size as usize));

        let mut start_in_self = offset;
        let end_in_self = {
            let (end, overflow) = offset.overflowing_add(size);
            debug_assert!(!overflow);
            end
        };
        let mut start_in_cur = start_in_self;
        let mut end_in_cur = end_in_self;

        ksync::lock!(let walker = LockedParentWalker::new(parent));

        while start_in_self < end_in_self {
            let mut stopped_early = false;
            // These are `Cell`s as both `page_callback` and `gap_callback` update them.
            let parent_content_start = Cell::new(u64::MAX);
            let parent_content_end = Cell::new(0u64);
            let (cur, cur_token) = walker.current(self, token);
            let page_callback = |p: P::Entry<'_>, page_offset: u64| {
                let cur_to_self = start_in_cur - start_in_self;
                // If we had started tracking a run of contiguous parent content then we must walk
                // up once it stops, either due to a gap or a switch to some other entry type.
                if parent_content_end.get() != 0
                    && (page_offset != parent_content_end.get() || !p.is_parent_content())
                {
                    return Status::STOP;
                }
                if p.is_parent_content() {
                    // ParentContent markers can exist spuriously (see explanation on
                    // tree_has_parent_content_markers) and so only consider walking up if within
                    // the parent_limit.
                    if page_offset < cur.parent_limit_locked(cur_token) {
                        // Either adding to or starting a new contiguous parent content run.
                        parent_content_start.set(cmp::min(parent_content_start.get(), page_offset));
                        parent_content_end.set(page_offset + (::page::SIZE as u64));
                    }
                    return Status::NEXT;
                }
                let status = func(ctx, p, cur, page_offset - cur_to_self, page_offset);
                if status == Status::STOP {
                    stopped_early = true;
                }
                status
            };
            // For efficiency of the gap_callback we pre-calculate whether or not gaps need to be
            // considered at all. Although we could use a different kind of page_list iteration
            // when not considering gaps, doing so creates a code branch that causes two usages of
            // the page_callback, which greatly hurts inlining and code generation.
            // If can see into a hidden parent, and cannot use content markers to optimize the walk
            // up, then we need to consider any gaps.
            let consider_gaps = cur.is_parent_hidden_locked(cur_token)
                && start_in_cur < cur.parent_limit_locked(cur_token)
                && !cur.node_has_parent_content_markers();

            let gap_callback = |gap_start_offset: u64, gap_end_offset: u64| {
                if !consider_gaps {
                    return Status::NEXT;
                }
                // The gap is empty, so walk up if the parent is accessible from any part of it.
                // Mark the range immediately preceding the gap as processed.

                // Gaps will never be considered on nodes that have parent content markers, so
                // should never be in the middle of calculating a parent content run.
                debug_assert!(parent_content_end.get() == 0);
                if gap_start_offset < cur.parent_limit_locked(cur_token) {
                    parent_content_start.set(gap_start_offset);
                    parent_content_end.set(gap_end_offset);
                    return Status::STOP;
                }
                Status::NEXT
            };

            // SAFETY: `cur_token` is for `cur`'s lock: for `self` and the parent given in `parent`
            // that is this function's `# Safety`, and any other `cur` is an ancestor that `walker`
            // locked during this call, whose `LockedPtr` gives the token for that lock. The caller
            // guarantees `func` does not touch `cur`'s page list other than through the entry
            // handed to it, and the callbacks reach `cur_token` only via `parent_limit_locked`,
            // which does not touch the page list. No reference or cursor into `cur`'s page list
            // from before this call is in use: for `self` and the parent given in `parent` that is
            // again this function's `# Safety`, and into the page list of an ancestor that `walker`
            // locked this function makes no reference outside `P::traverse`.
            let traverse_result = unsafe {
                P::traverse(cur, cur_token, start_in_cur, end_in_cur, page_callback, gap_callback)
            };
            // Invoke `on_node_done` while `cur`'s lock is still held by `walker`, on every exit
            // path (error, early stop, walk_up, or reset).
            on_node_done(ctx, cur);
            traverse_result?;

            // If the page callback wanted to stop early, then do so.
            if stopped_early {
                return Ok(());
            }

            let parent_content_start = parent_content_start.get();
            let parent_content_end = parent_content_end.get();
            if parent_content_end != 0 {
                // If we found a run of parent content, either via parent content markers or from a
                // gap, then need to walk up and look for it.
                start_in_self += parent_content_start - start_in_cur;
                start_in_cur = parent_content_start + cur.parent_offset_locked(cur_token);
                end_in_cur = cmp::min(parent_content_end, cur.parent_limit_locked(cur_token))
                    + cur.parent_offset_locked(cur_token);
                walker.as_mut().walk_up(self, token);
            } else {
                // If not walk up, then mark the entire range as processed and begin another walk
                // up from `self`.
                start_in_self += end_in_cur - start_in_cur;
                start_in_cur = start_in_self;
                end_in_cur = end_in_self;
                walker.as_mut().reset();
            }
        }

        Ok(())
    }

    /// Searches for the initial content, i.e. the content that would be used to initially populate
    /// the page, of `self` at `offset`. Whether there is presently any content populated in `self`
    /// is ignored, and if there is content then this will still return what would be used to
    /// re-populate that slot.
    #[inline]
    fn find_initial_page_content_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
    ) -> impl PinInit<PageLookup<'a>> {
        // If the parent can supply content, lock it directly into `owner` so that the search
        // below starts from it. Otherwise the result is no cursor and no owner.
        let parent = if self.has_parent_locked(token) && offset < self.parent_limit_locked(token) {
            // SAFETY: The initializer is only run into `PageLookup::owner`, which the search
            // below either keeps holding the parent's lock or walks further up with, and the
            // returned `PageLookup` gives its users no safe way to release a parent reference
            // while `owner` holds its lock.
            unsafe { self.parent_lock_init_locked(token) }
        } else {
            None
        };
        let search_parent = parent.is_some();
        pin_init!(PageLookup {
            cursor: VmPageListCursor::default(),
            owner <- LockedPtr::init_or_empty(parent),
            owner_offset: offset,
            visible_end: offset + (::page::SIZE as u64),
        })
        .pin_chain(move |lookup| {
            if search_parent {
                // SAFETY: `token` is borrowed for `'a`, so the lock of `self` stays held for as
                // long as the `PageLookup<'a>` being initialized exists. The second clause is the
                // safety invariant on `PageLookup::cursor`: the users of the initialized
                // `PageLookup` uphold the conditions of `VmPageListCursor::new` for as long as its
                // cursor, or a pointer from the cursor's `current_ptr()`, is used. The one user of
                // this initializer, `page_would_read_zero_locked`, only reads: it calls `current()`
                // on the result once, straight after the lookup, and does not keep the reference.
                // The third clause is not established here: nothing here shows the thread context
                // or which other `VmCowPages` locks the caller holds.
                unsafe {
                    self.find_page_content_into_locked(
                        token,
                        lookup,
                        offset + self.parent_offset_locked(token),
                        ::page::SIZE as u64,
                    );
                }
            }
            Ok(())
        })
    }

    /// Searches for the content for the page in `self` at `offset`. If the offset is already
    /// populated in `self` then that page is returned with the `owner` unset, otherwise the parent
    /// hierarchy is searched for any content. The result could be used to initialize a commit, or
    /// compare an existing commit with the original. The initial content is a `VmPageListCursor`
    /// and may be invalid if there was no explicit initial content. How to interpret an absence of
    /// content, whether it is zero or otherwise, is left up to the caller.
    ///
    /// If an ancestor has a committed page which corresponds to `offset`, returns a cursor with
    /// `current()` as that page as well as a `LockedPtr` to the `VmCowPages` and offset which own
    /// the page. If no ancestor has a committed page for the offset, returns a cursor whose
    /// `current()` is `None` or an empty slot, as well as the `VmCowPages`/offset which need to be
    /// queried to populate the page. If `self` needs to be queried to populate the page, `owner`
    /// is not set, otherwise it is set to the ancestor that owns the page. The reason being that
    /// `self` will already be externally locked by the caller, whereas an ancestor needs to be
    /// locked inside this function.
    ///
    /// The returned `visible_end` represents the size of the range in the owner for which it can
    /// be assumed that no child has content for, although for which the content might be in yet a
    /// higher up parent. This will always be a subset of the provided `max_owner_length`, which
    /// serves as a bound for the calculation and so passing in a smaller `max_owner_length` can
    /// sometimes be more efficient.
    /// It is an error for the `max_owner_length` to be < `page::SIZE`.
    #[inline]
    fn find_page_content_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        max_owner_length: u64,
    ) -> impl PinInit<PageLookup<'a>> {
        PageLookup::empty(offset, offset + max_owner_length).pin_chain(move |lookup| {
            // SAFETY: `token` is borrowed for `'a`, so the lock of `self` stays held for as long as
            // the `PageLookup<'a>` being initialized exists. The second clause is the safety
            // invariant on `PageLookup::cursor`: the users of the initialized `PageLookup` uphold
            // the conditions of `VmPageListCursor::new` for as long as its cursor, or a pointer
            // from the cursor's `current_ptr()`, is used. The one user of this initializer,
            // `lookup_readable_locked`, only reads, and never uses the cursor of the result: it
            // reads `visible_end`, `owner_offset` and `owner()`.
            // The third clause is not established here: nothing here shows the thread context or
            // which other `VmCowPages` locks the caller holds.
            unsafe { self.find_page_content_into_locked(token, lookup, offset, max_owner_length) };
            Ok(())
        })
    }

    /// `find_page_content_locked` searching from `lookup.owner` (or from `self` if it is empty),
    /// where `offset` is relative to that starting node, and filling in `lookup` with the result.
    ///
    /// `lookup` may be reused from a previous lookup: its `cursor`, `owner_offset` and
    /// `visible_end` are always overwritten.
    ///
    /// # Safety
    ///
    /// * The lock of `self`, which `token` proves is held now, must stay held for as long as
    ///   `lookup` holds the result. If `self` is the owner then `lookup.cursor` points into the
    ///   page list of `self` with `lookup.owner` empty, so nothing in `lookup` keeps that lock
    ///   held, and the borrow of `token` ends when this returns.
    /// * For as long as `lookup.cursor`, or a pointer returned by its `current_ptr()`, is used, the
    ///   conditions of [`VmPageListCursor::new`] must hold for the page list it points into: no
    ///   node may be inserted into or removed from that list, and nothing may write to a node while
    ///   `current()`, `for_every_contiguous()`, or `VmPageList::for_every_page_in_cursor_range()`
    ///   holds a reference into it.
    /// * The search may acquire the lock of each ancestor of the starting node in turn, so the
    ///   conditions of [`LockedPtr::acquire_raw`] must hold for those acquisitions: the thread is
    ///   in a blockable context with no spinlocks held and no active `ChainLockTransaction`, the
    ///   caller holds the lock of none of those ancestors, and (under lockdep) every `VmCowPages`
    ///   lock the caller holds has a lock order below that of each of them.
    unsafe fn find_page_content_into_locked<'a>(
        &'a self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        lookup: Pin<&mut PageLookup<'a>>,
        mut offset: u64,
        mut max_owner_length: u64,
    ) {
        let this_offset = offset;
        let lookup = lookup.project();
        let mut cur = lookup.owner;

        // Search up the clone chain for any committed pages. cur_offset is the offset
        // into cur we care about. The loop terminates either when that offset contains
        // a committed page or when that offset can't reach into the parent.
        loop {
            let (c, c_token) = cur.locked_or(self, token);
            if offset >= c.parent_limit_locked(c_token) {
                break;
            }
            debug_assert!(c.has_parent_locked(c_token));

            // SAFETY: `c`'s lock is held (`cur`, or `token` for `self`). The cursor is either
            // stored in `lookup` alongside `cur`, which keeps holding that lock, or is not used
            // again once `cur` moves on to the parent. Nothing conflicts with it: this function
            // only reads `c`'s page list, and only through the cursor, and once the cursor has been
            // handed to the caller in `lookup`, per `# Safety` no node is inserted into or removed
            // from that list, and no slot is written under a reference of the cursor, for as long
            // as the cursor is used (see `page_list_locked_ptr_unchecked` and
            // `VmPageListCursor::new`).
            let mut cursor = unsafe {
                VmPageList::lookup_nearest_mut_cursor_raw(
                    c.page_list_locked_ptr_unchecked(c_token),
                    offset,
                )
            };
            let p = cursor.current();
            let has_slot = p.is_some();
            let cursor_correct_offset = has_slot && cursor.offset() == offset;
            // If this slot has any actual content, then can immediately return it.
            if cursor_correct_offset
                && p.is_some_and(|slot| !slot.is_empty() && !slot.is_parent_content())
            {
                *lookup.cursor = cursor;
                *lookup.owner_offset = offset;
                *lookup.visible_end = max_owner_length + this_offset;
                return;
            }
            // If using parent content markers then unless there is a marker we can skip walking
            // up, as we know there is no content above us.
            if c.node_has_parent_content_markers()
                && (!cursor_correct_offset || !p.is_some_and(|slot| slot.is_parent_content()))
            {
                // `lookup` may hold the cursor of a previous lookup (see
                // `LookupCursor::establish_cursor_slow`), so it must be explicitly invalidated.
                *lookup.cursor = VmPageListCursor::default();
                *lookup.owner_offset = offset;
                *lookup.visible_end = max_owner_length + this_offset;
                return;
            }

            // Need to walk up, see if we need to trim the owner length.
            if max_owner_length > (::page::SIZE as u64) {
                // First trim to the parent limit.
                max_owner_length =
                    cmp::min(max_owner_length, c.parent_limit_locked(c_token) - offset);
                if max_owner_length > (::page::SIZE as u64) {
                    // There are three cases to consider for determining the range of the parent
                    // that we can actually see. The cases are considered in order, with each case
                    // also assuming the negation of the condition of all cases above it.
                    //  1. Leaf node using parent content markers - Here the current cursor must be
                    //     valid and be a ParentContent marker, otherwise we would have already
                    //     returned with content and would not be walking up. In this case the
                    //     visible length is the number of contiguous parentContent markers.
                    //  2. The cursor is valid - We know that the current offset does not have
                    //     content, but there is some content later on, and see we must find its
                    //     offset to determine if it limits the visible range or not.
                    //  3. The cursor is invalid - There is no content from here till the end of
                    //     the page list, in which case the visible length extends to the parent
                    //     limit (i.e. what was just calculated in max_owner_length), and we know
                    //     there is no content to look for to trim this length.
                    if c.node_has_parent_content_markers() {
                        let mut new_owner_length: u64 = 0;
                        let _ = cursor.for_every_contiguous(|p| {
                            if p.is_parent_content() && new_owner_length < max_owner_length {
                                new_owner_length += ::page::SIZE as u64;
                                return Status::NEXT;
                            }
                            Status::STOP
                        });
                        // The first slot in the cursor was parent content, so should always have
                        // incremented at least once.
                        debug_assert!(new_owner_length > 0);
                        max_owner_length = cmp::min(new_owner_length, max_owner_length);
                    } else if has_slot {
                        let _ = VmPageList::for_every_page_in_cursor_range(
                            cursor,
                            offset + max_owner_length,
                            |slot, slot_offset| {
                                debug_assert!(!slot.is_empty() && slot_offset >= offset);
                                let new_owner_length = slot_offset - offset;
                                debug_assert!(
                                    new_owner_length > 0 && new_owner_length <= max_owner_length
                                );
                                max_owner_length = new_owner_length;
                                Status::STOP
                            },
                        );
                    }
                }
            }

            offset += c.parent_offset_locked(c_token);
            // Acquire the parent's lock before `cur`'s is released by `move_from`.
            // SAFETY: This search never releases a reference to a parent, and the `PageLookup`
            // it fills in gives its users no safe way to do so (see `LockedPtr::step_to_parent`).
            // The parent is an ancestor of the starting node, so per `# Safety` the acquisition
            // conditions hold for it when `parent_init` is run just below; the one lock that this
            // search itself may hold then is that of `c`.
            let parent_init = unsafe { c.parent_lock_init_locked(c_token) };
            ksync::lock!(let parent = parent_init.expect("asserted above"));
            cur.as_mut().move_from(parent);
        }
        let (c, c_token) = cur.locked_or(self, token);
        // SAFETY: `c`'s lock is held (`cur`, or `token` for `self`), and `PageLookup` keeps
        // `cursor` alongside `cur` so that it is not used past the lock hold (see the invariant on
        // `PageLookup`). Nothing conflicts with it: no other access to `c`'s page list is made
        // here, and once the cursor has been handed to the caller in `lookup`, per `# Safety` no
        // node is inserted into or removed from that list, and no slot is written under a reference
        // of the cursor, for as long as the cursor is used (see `page_list_locked_ptr_unchecked`
        // and `VmPageListCursor::new`).
        let cursor = unsafe {
            VmPageList::lookup_mut_cursor_raw(c.page_list_locked_ptr_unchecked(c_token), offset)
        };
        *lookup.cursor = cursor;
        *lookup.owner_offset = offset;
        *lookup.visible_end = max_owner_length + this_offset;
    }

    /// Returns true if a page is not currently committed, and if the offset were to be read from,
    /// it would be read as zero. Requested offset must be page aligned and within range.
    #[inline]
    pub fn page_would_read_zero_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        page_offset: u64,
    ) -> bool {
        self.assert_canary();

        debug_assert!(::page::is_page_rounded(page_offset as usize));
        debug_assert!(page_offset < self.size_locked(token));
        let page_list = self.page_list_locked(token);
        let slot = page_list.lookup(page_offset);
        if self.node_has_parent_content_markers() {
            if slot.is_some_and(|slot| slot.is_page_or_ref()) {
                return false;
            }
            if !slot.is_some_and(|slot| slot.is_parent_content()) {
                return true;
            }
        }

        if slot.is_some_and(|slot| slot.is_marker()) {
            // This is already considered zero as there's a marker.
            return true;
        }
        if self.page_source_type() == PageSourceType::UserPager
            && (slot.is_some_and(|slot| slot.is_interval_zero())
                || page_list.is_offset_in_zero_interval(page_offset))
        {
            // Pages in zero intervals are supplied as zero by the kernel.
            return true;
        }
        // If we don't have a page or reference here we need to check our parent.
        if !slot.is_some_and(|slot| slot.is_page_or_ref()) {
            ksync::lock!(let content =
                self.find_initial_page_content_locked(token, page_offset));
            if content.current().is_none() {
                // Parent doesn't have a page either, so would also read as zero, assuming no page
                // source.
                return !self.is_root_source_user_pager_backed();
            }
        }
        // Content either locally or in our parent, assume it is non-zero and return false.
        false
    }

    /// Similar to `lookup_locked`, but enumerate all readable pages in the hierarchy within the
    /// requested range. The offset passed to the `lookup_fn` is the offset this page is visible at
    /// in this object, even if the page itself is committed in a parent object. The physical
    /// addresses given to the lookup_fn should not be retained in any way unless the range has
    /// also been pinned by the caller.
    /// Ranges of length zero are considered invalid and will return `Err(Status::INVALID_ARGS)`.
    /// The lookup_fn can terminate iteration early by returning `Status::STOP`.
    #[inline]
    pub fn lookup_readable_locked<F>(
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

        let mut current_page_offset = ::page::round_down(range.offset as usize) as u64;
        let end_page_offset = ::page::round_up(range.end() as usize) as u64;

        debug_assert!(!self.is_hidden());

        let mut stopped = false;
        while current_page_offset != end_page_offset {
            // Attempt to process any pages we have first. Skip over anything that's not a page
            // since the lookup_fn only applies to actual pages.
            let status = self.page_list_locked(token).for_every_page_in_range(
                current_page_offset,
                end_page_offset,
                |page_or_marker, mut offset| {
                    // The offset can advance ahead if we encounter gaps or sparse intervals.
                    if offset != current_page_offset {
                        if !page_or_marker.is_interval_end()
                            && !self.node_has_parent_content_markers()
                        {
                            // There was a gap before this offset and the tree does not use parent
                            // content markers so we must walk up to find the content.
                            return Status::STOP;
                        }
                        // Otherwise, we can advance our cursor to the interval/gap end.
                        offset = current_page_offset;
                    }
                    // Parent content is like a gap and so we need to exit and find the content.
                    if page_or_marker.is_parent_content() {
                        return Status::STOP;
                    }
                    debug_assert!(offset == current_page_offset);
                    current_page_offset = offset + (::page::SIZE as u64);
                    if !page_or_marker.is_page() {
                        return Status::NEXT;
                    }
                    let lookup_status = lookup_fn(offset, page_or_marker.page_as_paddr());
                    stopped = lookup_status == Status::STOP;
                    lookup_status
                },
            );
            status?;
            if stopped {
                return Ok(());
            }

            // Check if we've processed the whole range.
            if current_page_offset == end_page_offset {
                break;
            }

            // See if any of our parents have the content.
            // Note that page intervals are only supported in root VMOs, so if we ended the page
            // list traversal above partway into an interval, we will be able to continue the
            // traversal over the rest of the interval after this call - since we're the root, we
            // will be the owner and the owner length won't be clipped.
            ksync::lock!(let content = self.find_page_content_locked(
                token,
                current_page_offset,
                end_page_offset - current_page_offset,
            ));

            // This should always get filled out.
            debug_assert!(content.visible_end > current_page_offset);
            let owner_length = content.visible_end - current_page_offset;

            // Iterate over any potential content.
            let (owner_cow, owner_token) = content.owner().locked_or(self, token);
            let status = owner_cow.page_list_locked(owner_token).for_every_page_in_range(
                content.owner_offset,
                content.owner_offset + owner_length,
                |page_or_marker, offset| {
                    if !page_or_marker.is_page() {
                        return Status::NEXT;
                    }
                    let lookup_status = lookup_fn(
                        offset - content.owner_offset + current_page_offset,
                        page_or_marker.page_as_paddr(),
                    );
                    stopped = lookup_status == Status::STOP;
                    lookup_status
                },
            );
            status?;
            if stopped {
                return Ok(());
            }

            current_page_offset += owner_length;
        }
        Ok(())
    }

    /// This function merely calls `lookup_readable_locked` with the lock held. Note that there is
    /// no way non-test code can use this function in a correct way due to its internal locking.
    #[inline]
    pub fn debug_lookup_readable<F>(&self, range: VmCowRange, lookup_fn: F) -> Result<(), Status>
    where
        F: FnMut(u64, PAddr) -> Status,
    {
        self.assert_canary();
        #[rustfmt::skip]
        ksync::lock!(let guard = self
            .lock_ref()
            .lock_policy_with::<CriticalMutexOrderedPolicy>(self.lock_order() as usize));
        self.lookup_readable_locked(guard.token(), range, lookup_fn)
    }

    /// Attempts to release pages in the page list causing the range to become copy-on-write again.
    /// For consistency if there is a parent or a backing page source, such that the range would not
    /// explicitly copy-on-write the zero page then this will fail. Use `zero_pages_locked` for an
    /// operation that is guaranteed to succeed, but may not release memory.
    pub fn decommit_range(&self, range: VmCowRange) -> Result<(), Status> {
        self.assert_canary();

        stack_pin_init!(let deferred = DeferredOps::new(self));
        #[rustfmt::skip]
        ksync::lock!(let mut guard = self
            .lock_ref()
            .lock_policy_with::<CriticalMutexOrderedPolicy>(self.lock_order() as usize));
        let token = guard.as_mut().token_mut();
        // Validate the size and perform our zero-length hot-path check before we recurse
        // up to our top-level ancestor. Size bounding needs to take place relative
        // to the child the operation was originally targeted against.
        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        // was in range, just zero length
        if range.is_empty() {
            return Ok(());
        }

        // Currently, we can't decommit if the absence of a page doesn't imply zeroes. This is not a
        // cold path: `VmObjectPaged::ZeroRangeInternal` probes `decommit_range` first on every
        // non-contiguous VMO and falls back to zeroing when this fails.
        if self.has_parent_locked(token) || self.page_source_type() == PageSourceType::UserPager {
            return Err(Status::NOT_SUPPORTED);
        }

        debug_assert!(
            self.page_source_type() == PageSourceType::Anonymous
                || self.page_source_type() == PageSourceType::Contiguous
        );

        // Demand offset and length be correctly aligned to not give surprising user semantics.
        if !range.is_page_aligned() {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        self.unmap_and_free_pages_locked(token, range.offset, range.len, deferred.as_mut())
            .map(|_| ())
    }

    /// Unmaps and frees all the committed pages in the specified range.
    /// Upon success the removed pages are placed in the `DeferredOps` freed list, and the number of
    /// such pages is returned.
    ///
    /// Unlike `decommit_range()`, this function only operates on `self`, which must have no
    /// parent. `offset` must be page aligned. `len` must be less than or equal to `size - offset`.
    /// If `len` is less than `size - offset` it must be page aligned.
    fn unmap_and_free_pages_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        len: u64,
        mut deferred: Pin<&mut DeferredOps<'_>>,
    ) -> Result<u64, Status> {
        self.assert_canary();

        if self.any_pages_pinned_locked(token, offset, len as usize) {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        ltracef!("start offset {:#x}, end {:#x}\n", offset, offset + len);

        // We've already trimmed the range in `decommit_range()`.
        debug_assert!(VmCowRange::new(offset, len).is_bounded_by(self.size_locked(token)));

        // Verify page alignment.
        debug_assert!(::page::is_page_rounded(offset as usize));
        debug_assert!(
            ::page::is_page_rounded(len as usize) || (offset + len == self.size_locked(token))
        );

        // `decommit_range()` will call this function only on a VMO with no parent.
        debug_assert!(!self.has_parent_locked(token));

        // unmap all of the pages in this range on all the mapping regions
        self.range_change_update_locked(
            token,
            VmCowRange::new(offset, len),
            RangeChangeOp::Unmap,
            Some(deferred.as_mut()),
        );

        let mut populated_slots_removed: u32 = 0;
        let freed_count = {
            let freed_list = deferred.as_mut().freed_list(self);
            let mut page_remover = BatchPQRemove::new(freed_list);

            let _ =
                self.page_list_locked_mut(token).remove_pages(offset, offset + len, |p, _off| {
                    // Despite there being no `parent`, we must check for parent content markers
                    // here. See the comment on `tree_has_parent_content_markers` for more about
                    // spurious parent content markers.
                    if p.is_page_or_ref() || p.is_parent_content() {
                        populated_slots_removed += 1;
                    }
                    // SAFETY: `p` is a slot of `self.page_list` handed to this closure by
                    // `remove_pages` while `token` proves `self`'s lock is held.
                    unsafe { page_remover.push_content(p) };
                    Status::NEXT
                });
            page_remover.flush();
            page_remover.freed_count() as u64
        };
        if populated_slots_removed > 0 {
            self.continuous_attribution_tracker_locked_mut(token)
                .decrement(populated_slots_removed);
        }

        self.continuous_attribution_validation_assert(token);
        Ok(freed_count)
    }

    /// Tries to prepare the range [offset, offset + len) for writing by marking pages dirty or
    /// verifying that they are already dirty. It is possible for only some or none of the pages in
    /// the range to be dirtied at the end of this call. The returned (page-aligned) length starting
    /// at `offset` contains dirty pages, either already dirty before making the call or dirtied
    /// during the call. In other words, the range [offset, offset + dirty_len) will be dirty when
    /// this call returns, i.e. prepared for the write to proceed, where `dirty_len <= len`.
    ///
    /// If the specified range starts with pages that are not already dirty and need to request the
    /// page source before transitioning to dirty, a DIRTY page request will be forwarded to the
    /// page source. In this case `Err(Status::SHOULD_WAIT)` will be returned and the caller should
    /// wait on `page_request`. If no page requests need to be generated, i.e. we could find some
    /// pages that are already dirty at the start of the range, or if the VMO does not require dirty
    /// transitions to be trapped, `Ok(dirty_len)` is returned.
    ///
    /// `offset` and `len` should be page-aligned.
    pub fn prepare_for_write_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        page_request: Pin<&LazyPageRequest>,
    ) -> Result<u64, Status> {
        debug_assert!(range.is_page_aligned());
        debug_assert!(range.is_bounded_by(self.size_locked(token)));

        let page_source =
            self.page_source().expect("prepare_for_write_locked requires a page source");
        debug_assert!(self.page_source_type() == PageSourceType::UserPager);

        let start_offset = range.offset;
        let end_offset = range.end();

        // If the VMO does not require us to trap dirty transitions, simply mark the pages dirty,
        // and move them to the dirty page queue. Do this only for the first consecutive run of
        // committed pages within the range starting at offset. Any absent pages will need to be
        // provided by the page source, which might fail and terminate the lookup early. Any zero
        // page markers and zero intervals might need to be forked, which can fail too. Only mark
        // those pages dirty that the lookup is guaranteed to return successfully.
        if !page_source.should_trap_dirty_transitions() {
            let mut dirty_len: u64 = 0;
            // Capture `self`, `token` and `start_offset` by value and `dirty_len` by `&mut`, so
            // that the closure does not reload them through an extra pointer on every page.
            let dirty_len_ref = &mut dirty_len;
            let status = self.page_list_locked(token).for_every_page_and_gap_in_range(
                start_offset,
                end_offset,
                move |p, off| {
                    if p.is_marker() || p.is_interval_zero() {
                        // Found a marker or zero interval. End the traversal.
                        return Status::STOP;
                    }
                    // VMOs with a page source will never have compressed references, so this
                    // should be a real page.
                    debug_assert!(p.is_page());
                    let page = p.page();
                    // SAFETY: `page` is in `self`'s page list in the `OBJECT` state and `token`
                    // proves `self`'s lock is held, serializing all accesses to its object state.
                    unsafe {
                        debug_assert!(Self::is_page_dirty_tracked(page));
                        debug_assert!(page.get_object() == self.as_raw().cast::<c_void>());
                        debug_assert!(page.get_page_offset() == off);

                        // End the traversal if we encounter a loaned page. We reclaim loaned
                        // pages by evicting them, and dirty pages cannot be evicted.
                        if page.is_loaned() {
                            core::hint::cold_path();
                            // If this is a loaned page, it should be clean.
                            debug_assert!(Self::is_page_clean(page));
                            return Status::STOP;
                        }
                        debug_assert!(!page.is_loaned());

                        // Mark the page dirty.
                        if !Self::is_page_dirty(page) {
                            self.update_dirty_state_locked(
                                token,
                                page,
                                off,
                                DirtyState::Dirty,
                                /*is_pending_add=*/ false,
                            );
                        }
                    }
                    // The page was either already dirty, or we just marked it dirty. Proceed to
                    // the next one.
                    debug_assert!(start_offset + *dirty_len_ref == off);
                    *dirty_len_ref += ::page::SIZE as u64;
                    Status::NEXT
                },
                |_start, _end| {
                    // We found a gap. End the traversal.
                    Status::STOP
                },
            );
            // We don't expect a failure from the traversal.
            debug_assert!(status.is_ok());

            return Ok(dirty_len);
        }

        // Otherwise, generate a DIRTY page request for pages in the range which need to transition
        // to Dirty. Pages that qualify are:
        //  - Any contiguous run of non-Dirty pages (committed pages as well as zero page markers).
        //  For the purpose of generating DIRTY requests, both Clean and AwaitingClean pages are
        //  considered equivalent. This is because pages that are in AwaitingClean will need
        //  another acknowledgment from the user pager before they can be made Dirty (the
        //  filesystem might need to reserve additional space for them etc.).
        //  - Any zero intervals are implicit zero pages, i.e. the kernel supplies zero pages when
        //  they are accessed. Since these pages are not supplied by the user pager via
        //  zx_pager_supply_pages, we will need to wait on a DIRTY request before the sparse range
        //  can be replaced by an actual page for writing (the filesystem might need to reserve
        //  additional space).

        // Helpers used in the page list traversal below. Both helpers (and the traversal) update
        // `dirty_len` and `pages_to_dirty_len`, so they are held together in one struct, along
        // with the rest of the state that the page callback updates. This way the page callback
        // captures a single pointer.
        struct DirtyRuns {
            start_offset: u64,
            dirty_len: u64,
            pages_to_dirty_len: u64,
            // This tracks the beginning of an interval that falls in the specified range. Since we
            // might start partway inside an interval, this is initialized to start_offset so that
            // we only consider the portion of the interval inside the range. If we did not start
            // inside an interval, we will end up reinitializing this when we do find an interval
            // start, before this value is used, so it is safe to initialize to start_offset in all
            // cases.
            interval_start_off: u64,
            // This tracks whether we saw an interval start sentinel in the traversal, but have not
            // yet encountered a matching interval end sentinel. Should we end the traversal
            // partway in an interval, we will need to handle the portion of the interval between
            // the interval start and the end of the specified range.
            unmatched_interval_start: bool,
            // Whether the page callback ran. The gap callback tracks `found_gap` separately, as
            // the two callbacks cannot share one `&mut` flag.
            found_page: bool,
        }
        impl DirtyRuns {
            /// Try to add pages in the range [dirty_pages_start, dirty_pages_end) to the run of
            /// dirty pages being tracked. Return codes are the same as those used by
            /// `VmPageList::for_every_page_and_gap_in_range` to continue or terminate traversal.
            #[inline]
            fn accumulate_dirty_pages(
                &mut self,
                dirty_pages_start: u64,
                dirty_pages_end: u64,
            ) -> Status {
                // Bail if we were tracking a non-zero run of pages to be dirtied as we cannot
                // extend pages_to_dirty_len anymore.
                if self.pages_to_dirty_len > 0 {
                    return Status::STOP;
                }
                // Append the page to the dirty range being tracked if it immediately follows it.
                if self.start_offset + self.dirty_len == dirty_pages_start {
                    self.dirty_len += dirty_pages_end - dirty_pages_start;
                    return Status::NEXT;
                }
                // Otherwise we cannot accumulate any more contiguous dirty pages.
                Status::STOP
            }

            /// Try to add pages in the range [to_dirty_start, to_dirty_end) to the run of
            /// to-be-dirtied pages being tracked. Return codes are the same as those used by
            /// `VmPageList::for_every_page_and_gap_in_range` to continue or terminate traversal.
            #[inline]
            fn accumulate_pages_to_dirty(
                &mut self,
                to_dirty_start: u64,
                to_dirty_end: u64,
            ) -> Status {
                // Bail if we were already accumulating a non-zero run of Dirty pages.
                if self.dirty_len > 0 {
                    return Status::STOP;
                }
                // Append the pages to the range being tracked if they immediately follow it.
                if self.start_offset + self.pages_to_dirty_len == to_dirty_start {
                    self.pages_to_dirty_len += to_dirty_end - to_dirty_start;
                    return Status::NEXT;
                }
                // Otherwise we cannot accumulate any more contiguous to-dirty pages.
                Status::STOP
            }
        }
        let mut runs = DirtyRuns {
            start_offset,
            dirty_len: 0,
            pages_to_dirty_len: 0,
            interval_start_off: start_offset,
            unmatched_interval_start: false,
            found_page: false,
        };
        let mut found_gap = false;
        let status = self.page_list_locked(token).for_every_page_and_gap_in_range(
            start_offset,
            end_offset,
            |p, off| {
                runs.found_page = true;
                if p.is_page() {
                    let page = p.page();
                    // SAFETY: `page` is in `self`'s page list in the `OBJECT` state and `token`
                    // proves `self`'s lock is held, serializing all accesses to its object state.
                    unsafe {
                        debug_assert!(Self::is_page_dirty_tracked(page));
                        // VMOs that trap dirty transitions should not have loaned pages.
                        debug_assert!(!page.is_loaned());
                        // Page is already dirty. Try to add it to the dirty run.
                        if Self::is_page_dirty(page) {
                            return runs.accumulate_dirty_pages(off, off + (::page::SIZE as u64));
                        }
                        // If the page is clean, mark it accessed to grant it some protection from
                        // eviction until the pager has a chance to respond to the DIRTY request.
                        if Self::is_page_clean(page) {
                            pmm_page_queues().mark_accessed(page);
                        }
                    }
                } else if p.is_interval_zero() {
                    if p.is_interval_start() || p.is_interval_slot() {
                        runs.unmatched_interval_start = true;
                        runs.interval_start_off = off;
                    }
                    if p.is_interval_end() || p.is_interval_slot() {
                        runs.unmatched_interval_start = false;
                        // We need to commit pages if this is an interval, irrespective of the
                        // dirty state.
                        return runs.accumulate_pages_to_dirty(
                            runs.interval_start_off,
                            off + (::page::SIZE as u64),
                        );
                    }
                    return Status::NEXT;
                }

                // We don't compress pages in pager-backed VMOs.
                debug_assert!(!p.is_reference());
                // Parent content markers do not appear in pager-backed hierarchies.
                debug_assert!(!p.is_parent_content());
                // This is a either a zero page marker (which represents a clean zero page) or a
                // committed page which is not already Dirty. Try to add it to the range of pages
                // to be dirtied.
                // SAFETY: Any page here is in `self`'s page list under `self`'s lock (see above).
                debug_assert!(p.is_marker() || !unsafe { Self::is_page_dirty(p.page()) });
                runs.accumulate_pages_to_dirty(off, off + (::page::SIZE as u64))
            },
            |_start, _end| {
                found_gap = true;
                // We found a gap. End the traversal.
                Status::STOP
            },
        );

        // We don't expect an error from the traversal above. If an incompatible contiguous page or
        // a gap is encountered, we will simply terminate early.
        debug_assert!(status.is_ok());

        // Process the last remaining interval if there is one.
        if runs.unmatched_interval_start {
            core::hint::cold_path();
            runs.accumulate_pages_to_dirty(runs.interval_start_off, end_offset);
        }
        let DirtyRuns { dirty_len, mut pages_to_dirty_len, found_page, .. } = runs;

        // Account for the case where we started and ended in unpopulated slots inside an interval,
        // i.e we did not find either a page or a gap in the traversal. We would not have
        // accumulated any pages in that case.
        if !(found_page || found_gap) {
            core::hint::cold_path();
            debug_assert!(self.page_list_locked(token).is_offset_in_zero_interval(start_offset));
            debug_assert!(
                self.page_list_locked(token)
                    .is_offset_in_zero_interval(end_offset - (::page::SIZE as u64))
            );
            debug_assert!(dirty_len == 0);
            debug_assert!(pages_to_dirty_len == 0);
            // The entire range falls in an interval so it needs a DIRTY request.
            pages_to_dirty_len = end_offset - start_offset;
        }

        // We should either have found dirty pages or pages that need to be dirtied, but not both.
        debug_assert!(dirty_len == 0 || pages_to_dirty_len == 0);
        // Check that dirty_len and pages_to_dirty_len both specify valid ranges.
        debug_assert!(start_offset + dirty_len <= end_offset);
        debug_assert!(pages_to_dirty_len == 0 || start_offset + pages_to_dirty_len <= end_offset);

        // No pages need to transition to Dirty.
        if pages_to_dirty_len == 0 {
            return Ok(dirty_len);
        }

        // Found a contiguous run of pages that need to transition to Dirty. There might be more
        // such pages later in the range, but we will come into this call again for them via
        // another LookupCursor call after the waiting caller is unblocked for this range.

        let mut vmo_debug_info = VmoDebugInfo { vmo_id: 0, vmo_name: [0; _] };
        // We have a page source so this cannot be a hidden node, but the VmObjectPaged could have
        // been destroyed. We could be looking up a page via a lookup in a child (slice) after the
        // parent VmObjectPaged has gone away, so paged_ref could be null. Let the page source
        // handle any failures requesting the dirty transition.
        if let Some(paged_ref) = self.get_paged_backlink_locked(token) {
            vmo_debug_info.vmo_id = paged_ref.user_id();
            // Write the name in place, rather than into a second buffer that is then copied.
            paged_ref.get_name_locked(
                zerocopy::IntoBytes::as_mut_bytes(&mut vmo_debug_info.vmo_name),
                token,
            );
        }
        let status = page_source.request_dirty_transition(
            page_request.get(),
            start_offset,
            pages_to_dirty_len,
            vmo_debug_info,
        );
        // The page source will never succeed synchronously.
        debug_assert!(status.is_err());
        status.map(|()| dirty_len)
    }

    /// See `VmObjectPaged::get_lookup_cursor_locked`. `range` must be page aligned and must not be
    /// empty.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * A method of the cursor may acquire the lock of each ancestor of `self` in turn, so
    ///   whenever one is called the conditions for those acquisitions must hold: the thread is in a
    ///   blockable context with no spinlocks held and no active `ChainLockTransaction`, the caller
    ///   holds the lock of none of those ancestors, and (under lockdep) every `VmCowPages` lock the
    ///   caller holds has a lock order below that of each of them.
    #[inline]
    pub(super) unsafe fn get_lookup_cursor_locked<'a>(
        &'a self,
        token: &'a mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> impl PinInit<LookupCursor<'a>, Status> {
        let init = move |slot: *mut LookupCursor<'a>| {
            self.assert_canary();
            debug_assert!(!self.is_hidden());
            debug_assert!(!range.is_empty());
            debug_assert!(range.is_page_aligned());
            debug_assert!(self.life_cycle_locked(token) == LifeCycle::Alive);

            if range.offset >= self.size_locked(token)
                || !range.is_bounded_by(self.size_locked(token))
            {
                core::hint::cold_path();
                return Err(Status::OUT_OF_RANGE);
            }

            if let Some(discardable_tracker) = self.discardable_tracker() {
                // This vmo was discarded and has not been locked yet after the discard. Do not
                // return any pages.
                // SAFETY: `token` is the token for the lock of `self`, which owns the tracker.
                if unsafe { discardable_tracker.was_discarded_locked(token) } {
                    core::hint::cold_path();
                    return Err(Status::NOT_FOUND);
                }
            }

            // SAFETY: Per the contract of `pin_init_from_closure`, `slot` is valid, pinned,
            // uninitialized storage for a `LookupCursor`. Per `# Safety` `token` is the caller's
            // token for the lock of `self`, and it is exclusively borrowed for `'a`, so that lock
            // is held for `'a` and the caller cannot use its token while the cursor exists. That
            // leaves the cursor, which alone holds the lock of an owner, and
            // `LookupCursor::token_mut`, whose contract rules out the changes that
            // `LookupCursor::new` forbids. The last two clauses of its contract are the last two of
            // `# Safety`.
            let Ok(()) = unsafe { LookupCursor::new(self, range).__pinned_init(slot) };
            Ok(())
        };
        // SAFETY: `init` initializes `slot` when it returns `Ok`, and leaves it uninitialized when
        // it returns `Err`.
        unsafe { pin_init_from_closure(init) }
    }

    /// Releases any pages this VMO can reference from the provided start offset till the end of the
    /// VMO.
    ///
    /// This releases both directly owned pages, as well as pages in hidden parents that may be
    /// considered owned by this VMO.
    /// If applicable this method will update the `parent_limit` to reflect that it has removed any
    /// reference to its parent range, and it can be assumed that upon return that
    /// `parent_limit <= start`.
    /// The caller is responsible for actually freeing the pages, which are returned in
    /// `freed_list`.
    /// If the caller has locked the immediate parent, then it can pass the parent and a token for
    /// its lock in as `parent` to avoid double locking, otherwise if no parent or not locked `None`
    /// can be given.
    ///
    /// # Safety
    ///
    /// * `start` must be page aligned and at most the size of `self`.
    /// * `token` must be for the lock of `self`, and the token in `parent` (if any) for the lock
    ///   of the parent it is paired with: a `LockToken` only proves that some `VmCowPages` lock
    ///   is held.
    /// * `parent`, if given, must be the immediate parent of `self`.
    /// * This acquires the lock of each ancestor of `self` that it walks into, other than the one
    ///   given in `parent`, so the caller must not hold any of them and must be in a blockable
    ///   thread context, with no spinlocks held and no active `ChainLockTransaction`. Under
    ///   lockdep, every `VmCowPages` lock the caller holds must have a lock order below that of
    ///   each ancestor this acquires.
    /// * `freed_list` must only later be freed against `self` or another `VmCowPages` in the same
    ///   hierarchy.
    /// * `self` must not be a hidden node, and no reference or cursor into the page list of `self`,
    ///   or into the page list or to the continuous attribution tracker of the parent given in
    ///   `parent` (if any), may be live across the call (obtained before it and used during or
    ///   after it), as for `release_owned_pages_range_locked`.
    #[inline]
    unsafe fn release_owned_pages_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        start: u64,
        parent: Option<(&VmCowPages, &LockToken<'_, VmCowPagesLockClass>)>,
        freed_list: Pin<&mut ScopedPageFreedList>,
    ) {
        let len = self.size_locked(token) - start;
        // SAFETY: All but the last clause of the contract of `release_owned_pages_range_locked` are
        // this function's `# Safety`; for the range, `start` is page aligned and at most the size
        // of `self`, which is a multiple of the page size and is where the range ends. For the last
        // clause: the range runs to the end of `self`, so afterwards no part of it can see the
        // parent, as `parent_limit` is at most the size of `self` (the invariant of
        // `VmCowPagesState::parent_limit`).
        unsafe {
            self.release_owned_pages_range_locked(token, start, len, parent, freed_list);
        }
    }

    /// Similar to `release_owned_pages_locked`, but only releases the specified range, and as such
    /// provides no guarantee on the value of `parent_limit`, with respect to `offset`, requiring
    /// the caller to handle any issues with potential visibility into the parent range and content
    /// that should no longer be referenced.
    ///
    /// More specifically, whereupon the conclusion of `release_owned_pages_locked` it can be
    /// assumed that `parent_limit <= start` it CANNOT be assumed that `parent_limit <= offset`.
    ///
    /// # Safety
    ///
    /// * `[offset, offset + len)` must be page aligned and within the size of `self`.
    /// * `token` must be for the lock of `self`, and the token in `parent` (if any) for the lock
    ///   of the parent it is paired with: a `LockToken` only proves that some `VmCowPages` lock
    ///   is held.
    /// * `parent`, if given, must be the immediate parent of `self`.
    /// * This acquires the lock of each ancestor of `self` that it walks into, other than the one
    ///   given in `parent`, so the caller must not hold any of them and must be in a blockable
    ///   thread context, with no spinlocks held and no active `ChainLockTransaction`. Under
    ///   lockdep, every `VmCowPages` lock the caller holds must have a lock order below that of
    ///   each ancestor this acquires.
    /// * `freed_list` must only later be freed against `self` or another `VmCowPages` in the same
    ///   hierarchy.
    /// * `self` must not be a hidden node: content in `self`'s own page list is released without
    ///   looking at a share count, so it must have no other owner.
    /// * No reference or cursor into the page list of `self` may be live across the call (obtained
    ///   before it and used during or after it).
    /// * If `parent` is `Some`, no reference or cursor into the page list of that parent, and no
    ///   reference to its continuous attribution tracker, may be live across the call (obtained
    ///   before it and used during or after it): both are mutated, although the token passed for
    ///   the parent is shared.
    /// * If `self` can still see its parent in part of the released range when this returns (the
    ///   range ends below `parent_limit` and `self` does not use parent content markers), the
    ///   caller must stop that part from seeing the parent, for example by populating it, before it
    ///   is released again: a second release would walk into the hidden ancestors again and
    ///   decrement share counts there for content that `self` has already released.
    // Kept out of line: releasing a range is not on any fast path, and its callers (the dead
    // transition, resize, and supply paths) should not each inline a copy of it.
    #[inline(never)]
    unsafe fn release_owned_pages_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        len: u64,
        parent: Option<(&VmCowPages, &LockToken<'_, VmCowPagesLockClass>)>,
        freed_list: Pin<&mut ScopedPageFreedList>,
    ) {
        debug_assert!(self.lock_is_held());
        debug_assert!(!self.is_hidden());
        debug_assert!(offset <= self.size_locked(token));
        debug_assert!(offset + len <= self.size_locked(token));

        // A `Cell` because the count is shared: the deferred closure below holds a reference to it
        // until this function returns, while the callbacks given to the page list and to the
        // hierarchy walk increment it.
        let populated_slots_removed_from_this = Cell::new(0u32);
        // Must record these to the `continuous_attribution_tracker` before returning.
        let tracker_ptr = self.continuous_attribution_tracker_locked_ptr(token);
        let _do_record = ::zr::defer(|| {
            let removed = populated_slots_removed_from_this.get();
            if removed > 0 {
                // SAFETY: `tracker_ptr` points to `self.state().continuous_attribution_tracker`
                // (obtained up front so `_do_record` does not borrow `token` across the body),
                // `self`'s lock is held for the entire duration of this function, and no other
                // reference to `self`'s tracker is live when this deferred closure runs on return.
                unsafe { (*tracker_ptr).decrement(removed) };
            }
        });

        let mut page_remover = BatchPQRemove::new(freed_list);

        // If we know that the only pages in this range that need to be freed are from our own page
        // list, and we no longer need to consider our parent, then just remove them.
        if !self.is_parent_hidden_locked(token) || offset >= self.parent_limit_locked(token) {
            if offset == 0 && len == self.size_locked(token) {
                self.page_list_locked_mut(token).remove_all_content(|mut p| {
                    if p.is_page_or_ref() || p.is_parent_content() {
                        populated_slots_removed_from_this
                            .set(populated_slots_removed_from_this.get() + 1);
                    }
                    // SAFETY: `p` is a slot removed from `self`'s page list while `self`'s lock is
                    // held, and `page_remover`'s `freed_list` will be freed against `self`'s
                    // hierarchy per `# Safety`.
                    unsafe { page_remover.push_content(&mut p) };
                });
            } else {
                let _ = self.page_list_locked_mut(token).remove_pages(
                    offset,
                    offset + len,
                    |p, _off| {
                        if p.is_page_or_ref() || p.is_parent_content() {
                            populated_slots_removed_from_this
                                .set(populated_slots_removed_from_this.get() + 1);
                        }
                        // SAFETY: `p` is a slot of `self`'s page list while `self`'s lock is held,
                        // and `page_remover`'s `freed_list` will be freed against `self`'s
                        // hierarchy per `# Safety`.
                        unsafe { page_remover.push_content(p) };
                        Status::NEXT
                    },
                );
            }
            page_remover.flush();
            // Potentially trim the parent limit to reflect the range that has been freed.
            let parent_limit = self.parent_limit_locked(token);
            if offset + len >= parent_limit {
                self.set_parent_limit_locked(token, cmp::min(parent_limit, offset));
            }
            return;
        }

        let compression = pmm::node().get_page_compression();

        // Track whether any page was pushed from an ancestor (`owner != self`) so `on_node_done`
        // flushes `page_remover` before the walk releases that ancestor's lock.
        struct RemoveCtx<'a, 'b> {
            page_remover: &'a mut BatchPQRemove<'b>,
            ancestor_pushed: bool,
        }
        let mut remove_ctx = RemoveCtx { page_remover: &mut page_remover, ancestor_pushed: false };

        // Decrement the share count on all pages, both directly owned by us and shared via our
        // parents, that this node can see, and free any pages with a zero ref count.
        let remove_fn = |ctx: &mut RemoveCtx<'_, '_>,
                         p: &mut VmPageOrMarker,
                         owner: &VmCowPages,
                         _this_offset: u64,
                         _owner_offset: u64| {
            // Explicitly handle this case separately since although we would naturally find these
            // to have a share_count of 0 and free them, we would always like to free any markers,
            // however we can only free markers that are precisely in `self` since markers have no
            // refcount.
            if ptr::eq(self, owner) {
                if p.is_page_or_ref() || p.is_parent_content() {
                    populated_slots_removed_from_this
                        .set(populated_slots_removed_from_this.get() + 1);
                }
                // SAFETY: `p` is a slot of `self`'s page list while `self`'s lock is held,
                // `self`'s lock stays held until `page_remover.flush()` below, and `freed_list`
                // will be freed against `self`'s hierarchy per `# Safety`.
                unsafe { ctx.page_remover.push_content(p) };
                return Status::NEXT;
            }

            if p.is_page() {
                let page = p.page();
                // SAFETY: `page` is in `owner`'s page list (`OBJECT` state) and `owner`'s lock is
                // held per `remove_owned_hierarchy_pages_in_range_locked`'s postcondition.
                let share_count = unsafe { page.get_share_count() };
                if share_count == 0 {
                    // SAFETY: `remove_owned_hierarchy_pages_in_range_locked` holds `owner`'s lock
                    // while invoking this callback, and nothing else borrows `owner`'s attribution
                    // tracker during the callback: for the parent given in `parent` that is this
                    // function's `# Safety`, and any other `owner` here is an ancestor that the
                    // walk locked itself.
                    let mut owner_token = unsafe { LockToken::new() };
                    owner.continuous_attribution_tracker_locked_mut(&mut owner_token).decrement(1);
                    ctx.ancestor_pushed = true;
                    // SAFETY: `p` is a slot of `owner`'s page list (`owner` is in `self`'s
                    // hierarchy) with `share_count == 0` while `owner`'s lock is held, and
                    // `ctx.ancestor_pushed` ensures `on_node_done` flushes `ctx.page_remover`
                    // before the walk releases `owner`'s lock.
                    unsafe { ctx.page_remover.push_content(p) };
                } else {
                    // SAFETY: `page` is in `owner`'s page list (`OBJECT` state) and `owner`'s lock
                    // is held.
                    unsafe { page.set_share_count(share_count - 1) };
                }
            } else if p.is_reference() {
                let compression = compression.expect("reference requires page compression");
                // SAFETY: `p.reference()` is a valid compressed reference in `owner`'s page list
                // and `owner`'s lock is held.
                let share_count = unsafe { compression.get_metadata(p.reference()) };
                if share_count == 0 {
                    // SAFETY: `remove_owned_hierarchy_pages_in_range_locked` holds `owner`'s lock
                    // while invoking this callback, and nothing else borrows `owner`'s attribution
                    // tracker during the callback: for the parent given in `parent` that is this
                    // function's `# Safety`, and any other `owner` here is an ancestor that the
                    // walk locked itself.
                    let mut owner_token = unsafe { LockToken::new() };
                    owner.continuous_attribution_tracker_locked_mut(&mut owner_token).decrement(1);
                    // SAFETY: `p` is a reference slot of `owner`'s page list (`owner` is in
                    // `self`'s hierarchy) with `share_count == 0` while `owner`'s lock is held;
                    // `push_content` frees references immediately without buffering in `pages`.
                    unsafe { ctx.page_remover.push_content(p) };
                } else {
                    // SAFETY: `p.reference()` is a valid compressed reference in `owner`'s page
                    // list and `owner`'s lock is held.
                    unsafe { compression.set_metadata(p.reference(), share_count - 1) };
                }
            } else if p.is_marker() {
                if p.marker_share_count() == 0 {
                    // SAFETY: `p` is a marker slot of `owner`'s page list; `push_content` empties
                    // it without buffering in `pages`.
                    unsafe { ctx.page_remover.push_content(p) };
                } else {
                    p.decrement_marker_share_count();
                }
            }
            Status::NEXT
        };
        let on_node_done = |ctx: &mut RemoveCtx<'_, '_>, owner: &VmCowPages| {
            if ctx.ancestor_pushed {
                debug_assert!(!ptr::eq(self, owner));
                ctx.page_remover.flush();
                ctx.ancestor_pushed = false;
            }
        };
        // SAFETY: `remove_fn` only mutates the `p` handed to it and
        // `owner.continuous_attribution_tracker`, without accessing `owner`'s `page_list` other
        // than through `p`. Per `# Safety` no reference or cursor into the page list of `self`, or
        // of the parent given in `parent`, is live across this call, and this function holds none
        // of its own here. Also per `# Safety`, `token` and the token in `parent` are for the locks
        // of `self` and of that parent.
        let status = unsafe {
            self.remove_owned_hierarchy_pages_in_range_locked(
                token,
                &mut remove_ctx,
                remove_fn,
                on_node_done,
                offset,
                len,
                parent,
            )
        };
        debug_assert!(status.is_ok());

        if self.node_has_parent_content_markers() {
            // Any parent content markers for the pages that we removed the reference counts for
            // need to be separately removed.
            let _ = self.page_list_locked_mut(token).remove_pages(
                offset,
                offset + len,
                |slot, _off| {
                    debug_assert!(slot.is_parent_content());
                    populated_slots_removed_from_this
                        .set(populated_slots_removed_from_this.get() + 1);
                    *slot = VmPageOrMarker::empty();
                    Status::NEXT
                },
            );
        }

        // This node can no longer see into its parent in the range we just released.
        debug_assert!(offset < self.parent_limit_locked(token));
        if offset + len >= self.parent_limit_locked(token) {
            self.set_parent_limit_locked(token, offset);
        }

        page_remover.flush();
    }

    /// Attempts to commit a range of pages.
    ///
    /// This has three kinds of return status:
    /// * `Ok(())` => The whole range was successfully committed and `range.len` is returned as the
    ///   committed length.
    /// * `Err(Status::SHOULD_WAIT)` => A partial (potentially 0) range was committed (returned as
    ///   the committed length) and the passed in `page_request` should be waited on before retrying
    ///   the commit operation. The portion that was successfully committed does not need to be
    ///   retried.
    /// * `Err(_)` => Any other error, the number of pages committed is undefined.
    ///
    /// The `range.offset` and `range.len` are assumed to be page aligned and within the range of
    /// `size`.
    ///
    /// # Safety
    ///
    /// * `range` must be page-aligned.
    /// * `deferred` must have been constructed for `self`.
    /// * `page_request` must not hold an active request.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    pub unsafe fn commit_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        ltracef!("offset {:#x}, len {:#x}\n", range.offset, range.len);

        debug_assert!(range.is_page_aligned());
        debug_assert!(range.is_bounded_by(self.size_locked(token)));
        self.continuous_attribution_validation_assert(token);

        // If this vmo has a direct page source, then the source will provide the backing memory.
        // For children that eventually depend on a page source, we skip preallocating memory to
        // avoid potentially overallocating pages if something else touches the vmo while we're
        // blocked on the request. Otherwise we optimize things by preallocating all the pages.
        stack_pin_init!(let page_list = VmPageDoublyLinkedList::new());
        if !self.root_has_page_source() {
            // make a pass through the list to find out how many pages we need to allocate
            let mut count = (range.len / (::page::SIZE as u64)) as usize;
            let _ = self.page_list_locked(token).for_every_page_in_range(
                range.offset,
                range.end(),
                |p, _off| {
                    if p.is_page() {
                        count -= 1;
                    }
                    Status::NEXT
                },
            );

            if count == 0 {
                return (Ok(()), range.len);
            }

            let status = pmm::alloc_pages(count, self.pmm_alloc_flags(), page_list.as_mut());
            // Ignore Status::SHOULD_WAIT since the loop below will fall back to a page by page
            // allocation, allowing us to wait for single pages should we need to.
            if let Err(err) = status
                && err != Status::SHOULD_WAIT
            {
                core::hint::cold_path();
                return (Err(err), 0);
            }
        }

        // SAFETY: `page_list` is pinned on the stack and never moved; consuming `page_list` into a
        // raw pointer here avoids aliasing `&mut VmPageDoublyLinkedList` between `_list_cleanup`
        // and `cursor.give_alloc_list`.
        let page_list_ptr = NonNull::from(unsafe { page_list.get_unchecked_mut() });
        let _list_cleanup = ::zr::defer(|| {
            // SAFETY: Two operations. `&mut *page_list_ptr.as_ptr()`: `page_list_ptr` points to the
            // list that `stack_pin_init!` declared above. That declaration precedes this guard, so
            // the list is still live when this runs, and nothing else accesses it then: the only
            // other user, `cursor`, is declared in the inner block below and has been dropped.
            // `Pin::new_unchecked`: the list was pinned in place and is never moved; it is dropped
            // in place, after this guard has run.
            let page_list = unsafe { Pin::new_unchecked(&mut *page_list_ptr.as_ptr()) };
            if !page_list.is_empty() {
                // SAFETY: `page_list` only contains pages allocated above when
                // `!self.root_has_page_source()`, which are valid allocated pages not in any page
                // queue.
                unsafe { self.free_pages(page_list) };
            }
        });

        let start_offset = range.offset;
        let end = range.end();
        let (status, committed_len) = {
            stack_try_pin_init!(
                // SAFETY: Per `# Safety` `range` is page-aligned, `self` is alive and not hidden,
                // and the conditions for acquiring the locks of its ancestors hold for this call,
                // within which `cursor` is used and dropped.
                let cursor = unsafe { self.get_lookup_cursor_locked(token, range) }
            );
            let mut cursor = match cursor {
                Ok(cursor) => cursor,
                Err(err) => {
                    core::hint::cold_path();
                    return (Err(err), 0);
                }
            };
            // Commit represents an explicit desire to have pages and should not be deduped back to
            // the zero page.
            cursor.as_mut().disable_zero_fork();
            // SAFETY: `page_list_ptr` points to `page_list`, which is only accessed by `cursor`
            // until `clear_alloc_list` below, and every page in it was returned by
            // `pmm::alloc_pages` in the `ALLOC` state.
            unsafe { cursor.as_mut().give_alloc_list(page_list_ptr) };

            let mut status = Ok(());
            let mut offset = start_offset;
            while offset < end {
                // SAFETY: Per `# Safety` `deferred` was made for `self` and `page_request` has no
                // active request on entry (and the loop breaks on the first error, before a request
                // made by `require_owned_page` could be overwritten). `(end - offset) / page::SIZE`
                // is non-zero (since `offset < end` and both are page-aligned) and equals the
                // number of pages remaining in `cursor`.
                let result = unsafe {
                    cursor.as_mut().require_owned_page(
                        false,
                        (end - offset) / (::page::SIZE as u64),
                        deferred.as_mut(),
                        page_request,
                    )
                };

                if let Err(err) = result {
                    status = Err(err);
                    break;
                }
                offset += ::page::SIZE as u64;
            }
            // Record how much we were able to process.
            let committed_len = offset - start_offset;

            // Clear the alloc list from the cursor and let `_list_cleanup` free any remaining
            // pages.
            cursor.as_mut().clear_alloc_list();
            (status, committed_len)
        };

        self.continuous_attribution_validation_assert(token);
        (status, committed_len)
    }

    /// Helper to invalidate any READ requests in the specified range by spuriously resolving them.
    fn invalidate_read_requests_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        len: u64,
    ) {
        debug_assert!(self.lock_is_held());
        debug_assert!(::page::is_page_rounded(offset as usize));
        debug_assert!(::page::is_page_rounded(len as usize));
        debug_assert!(::range_check::in_range_max(offset, len, self.size_locked(token)));

        let page_source =
            self.page_source().expect("invalidate_read_requests_locked requires a page source");

        let start = offset;
        let end = offset + len;

        let status = self.page_list_locked(token).for_every_page_and_gap_in_range(
            start,
            end,
            |_p, _off| Status::NEXT,
            |gap_start, gap_end| {
                page_source.on_pages_supplied(gap_start, gap_end - gap_start);
                Status::NEXT
            },
        );
        debug_assert!(status.is_ok());
    }

    /// Helper to invalidate any DIRTY requests in the specified range by spuriously resolving them.
    fn invalidate_dirty_requests_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        len: u64,
    ) {
        debug_assert!(self.lock_is_held());
        debug_assert!(::page::is_page_rounded(offset as usize));
        debug_assert!(::page::is_page_rounded(len as usize));
        debug_assert!(::range_check::in_range_max(offset, len, self.size_locked(token)));

        debug_assert!(self.page_source_type() == PageSourceType::UserPager);
        let page_source =
            self.page_source().expect("invalidate_dirty_requests_locked requires a page source");
        debug_assert!(page_source.should_trap_dirty_transitions());

        let start = offset;
        let end = offset + len;

        let status = self.page_list_locked(token).for_every_page_and_contiguous_run_in_range(
            start,
            end,
            #[inline(always)]
            |p, _off| {
                // A marker is a clean zero page and might have an outstanding DIRTY request.
                if p.is_marker() {
                    return true;
                }
                // An interval is an uncommitted zero page and might have an outstanding DIRTY
                // request irrespective of dirty state.
                if p.is_interval_zero() {
                    return true;
                }
                // Although a reference is implied to be clean, VMO backed by a page source should
                // never have references.
                debug_assert!(!p.is_reference());
                // Not parent content in pager-backed VMOs.
                debug_assert!(!p.is_parent_content());

                let page = p.page();
                // SAFETY: `page` is in `self`'s page list (`OBJECT` state) and `token` proves
                // `self`'s lock is held.
                debug_assert!(unsafe { Self::is_page_dirty_tracked(page) });

                // A page that is not Dirty already might have an outstanding DIRTY request.
                // SAFETY: As above.
                !unsafe { Self::is_page_dirty(page) }
            },
            |_p, _off| {
                // Nothing to update for the page as we're not actually marking it Dirty.
                Status::NEXT
            },
            |start, end, _unused| {
                // Resolve any DIRTY requests in this contiguous range.
                page_source.on_pages_dirtied(start, end - start);
                Status::NEXT
            },
        );
        // We don't expect an error from the traversal.
        debug_assert!(status.is_ok());

        // Now resolve DIRTY requests for any gaps. After request generation, pages could either
        // have been evicted, or zero intervals written back, leading to gaps. So it is possible
        // for gaps to have outstanding DIRTY requests.
        let status = self.page_list_locked(token).for_every_page_and_gap_in_range(
            start,
            end,
            |_p, _off| {
                // Nothing to do for pages. We already handled them above.
                Status::NEXT
            },
            |gap_start, gap_end| {
                // Resolve any DIRTY requests in this gap.
                page_source.on_pages_dirtied(gap_start, gap_end - gap_start);
                Status::NEXT
            },
        );
        // We don't expect an error from the traversal.
        debug_assert!(status.is_ok());
    }

    /// Resizes the range of this cow pages. `s` must be a multiple of the page size.
    ///
    /// # Safety
    ///
    /// * `self` must not be a hidden node.
    /// * `s` must be page-aligned and `<= VmPageList::MAX_SIZE`.
    /// * This acquires `self.lock()`, and can acquire the `paged_vmo_lock` of the page source at
    ///   the root of `self`'s hierarchy and the locks of other nodes in that hierarchy. The caller
    ///   must not hold `self.lock()`, the lock of any other `VmCowPages`, or that
    ///   `paged_vmo_lock`, and must be in a blockable thread context, with no spinlocks held and no
    ///   active `ChainLockTransaction`.
    pub unsafe fn resize(&self, s: u64) -> Result<(), Status> {
        self.assert_canary();

        ltracef!("vmcp {:p}, size {}\n", self, s);

        stack_pin_init!(let deferred = DeferredOps::new(self));
        // In the case where we are shrinking any child limits may need to be updated, but the
        // locking order requires their locks to be acquired without our lock held, and so we do
        // this after dropping the main lock, but before any pages are freed from the deferred ops.
        // See the comment and checks where this is set to true for details on the correctness.
        let mut update_child_limits = false;
        {
            #[rustfmt::skip]
            ksync::lock!(let mut guard = self
                .lock_ref()
                .lock_policy_with::<CriticalMutexOrderedPolicy>(self.lock_order() as usize));
            let token = guard.as_mut().token_mut();

            // make sure everything is aligned before we get started
            debug_assert!(::page::is_page_rounded(self.size_locked(token) as usize));
            debug_assert!(::page::is_page_rounded(s as usize));

            // see if we're shrinking or expanding the vmo
            if s < self.size_locked(token) {
                // shrinking
                let start = s;
                let end = self.size_locked(token);
                let len = end - start;

                // bail if there are any pinned pages in the range we're trimming
                if self.any_pages_pinned_locked(token, start, len as usize) {
                    core::hint::cold_path();
                    return Err(Status::BAD_STATE);
                }

                // unmap all of the pages in this range on all the mapping regions
                self.range_change_update_locked(
                    token,
                    VmCowRange::new(start, len),
                    RangeChangeOp::Unmap,
                    Some(deferred.as_mut()),
                );

                // Resolve any outstanding page requests tracked by the page source that are now
                // out-of-bounds.
                if let Some(page_source) = self.page_source() {
                    // Tell the page source that any non-resident pages that are now out-of-bounds
                    // were supplied, to ensure that any reads of those pages get woken up.
                    self.invalidate_read_requests_locked(token, start, len);

                    // If DIRTY requests are supported, also tell the page source that any
                    // non-Dirty pages that are now out-of-bounds were dirtied (without actually
                    // dirtying them), to ensure that any threads blocked on DIRTY requests for
                    // those pages get woken up.
                    if self.page_source_type() == PageSourceType::UserPager
                        && page_source.should_trap_dirty_transitions()
                    {
                        self.invalidate_dirty_requests_locked(token, start, len);
                    }
                }

                // If pager-backed and the new size falls partway in an interval, we will need to
                // clip the interval.
                if self.page_source_type() == PageSourceType::UserPager {
                    // Check if the first populated slot we find in the now-invalid range is an
                    // interval end.
                    let mut interval_end = u64::MAX;
                    let status = self.page_list_locked(token).for_every_page_in_range(
                        start,
                        end,
                        |p, off| {
                            if p.is_interval_end() {
                                interval_end = off;
                            }
                            // We found the first populated slot. Stop the traversal.
                            Status::STOP
                        },
                    );
                    debug_assert!(status.is_ok());

                    if interval_end != u64::MAX {
                        let status = self.page_list_locked_mut(token).clip_interval_end(
                            interval_end,
                            interval_end - start + (::page::SIZE as u64),
                        );
                        if let Err(err) = status {
                            core::hint::cold_path();
                            debug_assert_eq!(err, Status::NO_MEMORY);
                            return Err(err);
                        }
                    }
                }

                // Clip the parent limit and release any pages, if any, in this node or the parents.
                //
                // It should never exceed this node's size, either the current size (which is `end`)
                // or the new size (which is `start`).
                debug_assert!(self.parent_limit_locked(token) <= end);

                // SAFETY: `token` is from the guard on `self`'s lock taken above. `deferred` was
                // constructed for `self`, so `deferred.freed_list(self)` will be freed against
                // `self`. Per `# Safety`, `self` is not a hidden node and `start` (`s`) is
                // page-aligned, and this branch is only taken when it is below the size of `self`.
                // No reference or cursor into `self`'s page list is live here, and `parent` is
                // `None`. The ancestor locks that this may acquire are covered by the lock clause
                // of `# Safety`: the only `VmCowPages` lock held here is that of `self`.
                unsafe {
                    self.release_owned_pages_locked(
                        token,
                        start,
                        None,
                        deferred.as_mut().freed_list(self),
                    );
                }

                // If the tail of a parent disappears, the children shouldn't be able to see that
                // region again, even if the parent is later reenlarged. So update the children's
                // parent limits.
                if self.children_list_len_locked(token) != 0 {
                    // The only scenario where we can have children is if this is a pager backed
                    // hierarchy, in which case the DeferredOps constructed at the top of this
                    // function holds the pager hierarchy lock, which is held over all resize
                    // operations. Due to this lock being held we know that, even once the VMO lock
                    // is dropped, no resize operation to reenlarge can occur till after we have
                    // completed updating the child limits.
                    // In the present state, with our size reduced but child parent_limit not
                    // updated, the children will just walk up to us, see that the offset is beyond
                    // our size, and substitute a zero page. Once the child parent_limits are
                    // updated they will instead not walk up to us, and substitute a zero page.
                    assert!(self.root_has_page_source());
                    update_child_limits = true;
                }
            } else if s > self.size_locked(token) {
                // Check that this VMOs new size would not cause it to overflow if projected onto
                // the root.
                if self.root_parent_offset_locked(token).checked_add(s).is_none() {
                    core::hint::cold_path();
                    return Err(Status::INVALID_ARGS);
                }
                // expanding
                // figure the starting and ending page offset that is affected
                let start = self.size_locked(token);
                let end = s;
                let len = end - start;

                // inform all our children or mapping that there's new bits
                self.range_change_update_locked(
                    token,
                    VmCowRange::new(start, len),
                    RangeChangeOp::Unmap,
                    Some(deferred.as_mut()),
                );

                // If pager-backed, need to insert a dirty zero interval beyond the old size.
                if self.page_source_type() == PageSourceType::UserPager {
                    let status = self.page_list_locked_mut(token).add_zero_interval(
                        start,
                        end,
                        ZeroRangeDirtyState::Dirty,
                    );
                    if let Err(err) = status {
                        core::hint::cold_path();
                        debug_assert_eq!(err, Status::NO_MEMORY);
                        return Err(err);
                    }
                }
            }

            // save bytewise size
            self.set_size_locked(token, s);

            // We were able to successfully resize. Mark as modified.
            self.mark_modified_locked(token);

            self.continuous_attribution_validation_assert(token);
        }
        // Now that the lock is dropped, check if we need to update the child limits before the
        // DeferredOps get finalized. When iterating over our children it is important that we
        // iterate precisely over *all* of our children and exactly our direct children (i.e. not
        // our children's children). The TreeWalkCursor is able to provide these guarantees in this
        // case since clone creation is serialized with the page_source_lock in the DeferredOps,
        // just like here.
        // Serializing the clone calls with resize ensures that any child we are iterating cannot
        // move down in the tree and gain a new parent, which happens when a hidden node needs to
        // be inserted. The deletion path is not an issue since if the node we are iterating at
        // gets deleted then the cursor will just move to its sibling (or get deleted if no
        // sibling), which is the behavior that we want anyway.
        if update_child_limits {
            // SAFETY: `self.as_raw()` is a valid `VmCowPages` whose lock was just released above,
            // and `deferred` holds the hierarchy `page_source_lock`.
            unsafe {
                bindings::cpp_vm_cow_pages_resize_update_child_limits(self.as_raw(), s);
            }
        }
        Ok(())
    }

    /// See `VmObject::fail_page_requests`.
    ///
    /// This is a transient operation used only to fail currently outstanding page requests. It
    /// does not alter the state of the VMO, or any pages that might have already been populated
    /// within the specified range.
    ///
    /// If certain pages in this range are populated, we must have done so via a previous
    /// `supply_pages` call that succeeded. So it might be fine for clients to continue accessing
    /// them, despite the larger range having failed.
    pub fn fail_page_requests_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        error_status: Status,
    ) -> Result<(), Status> {
        // TODO(rashaeqbal): If we support a more permanent failure mode in the future, we will
        // need to free populated pages in the specified range, and possibly detach the VMO from
        // the page source.
        self.assert_canary();

        debug_assert!(range.is_page_aligned());

        let page_source =
            self.page_source().expect("fail_page_requests_locked requires a page source");

        if !PageSource::is_valid_internal_failure_code(error_status) {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }

        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        if page_source.is_detached() {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        }

        page_source.on_pages_failed(range.offset, range.len, error_status);
        Ok(())
    }

    /// See `VmObject::writeback_begin`.
    pub fn writeback_begin_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        is_zero_range: bool,
    ) -> Result<(), Status> {
        self.assert_canary();

        debug_assert!(range.is_page_aligned());

        assert!(self.page_source().is_some());

        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        if self.page_source_type() != PageSourceType::UserPager {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        let start_offset = range.offset;
        let end_offset = range.end();
        // We only need to consider transitioning committed pages if the caller has specified that
        // this is not a zero range. For a zero range, we cannot start cleaning any pages because
        // the caller has expressed intent to write back zeros in this range; any pages we clean
        // might get evicted and incorrectly supplied again as zero pages, leading to data loss.
        //
        // When querying dirty ranges, zero page intervals are indicated as dirty zero ranges. So
        // it's perfectly reasonable for the user pager to write back these zero ranges efficiently
        // without having to read the actual contents of the range, which would read zeroes anyway.
        // There can exist a race however, where the user pager has just discovered a dirty zero
        // range, and before it starts writing it out, an actual page gets dirtied in that range.
        // Consider the following example that demonstrates the race:
        //  1. The zero interval [5, 10) is indicated as a dirty zero range when the user pager
        //  queries dirty ranges.
        //  2. A write comes in for page 7 and it is marked Dirty. The interval is split up into
        //  two: [5, 7) and [8, 10).
        //  3. The user pager prepares to write the range [5, 10) with `writeback_begin_locked`.
        //  4. Both the intervals as well as page 7 are marked AwaitingClean.
        //  5. The user pager still thinks that [5, 10) is zero and writes back zeroes for the
        //  range.
        //  6. The user pager does a `writeback_end_locked` on [5, 10), and page 7 gets marked
        //  Clean.
        //  7. At some point in the future, page 7 gets evicted. The data on page 7 (which was
        //  prematurely marked Clean) is now lost.
        //
        // This race occurred because there was a mismatch between what the user pager and the
        // kernel think the contents of the range being written back are. The user pager intended
        // to mark only zero ranges clean, not actual pages. The is_zero_range flag captures this
        // intent, so that the kernel does not incorrectly clean actual committed pages. Committed
        // dirty pages will be returned as actual dirty pages (not dirty zero ranges) on a
        // subsequent call to query dirty ranges, and can be cleaned then.

        // The start sentinel of the dirty interval currently being tracked, and its offset. Held
        // as the raw slot pointer handed out by the traversal rather than a `VmPageOrMarkerRef`
        // because it outlives the callback that received it.
        let mut interval_start: Option<(NonNull<VmPageOrMarker>, u64)> = None;
        // SAFETY: The traversal below is the only page list access until it returns and the
        // post-loop fix-up of `interval_start` (see below); the callback reaches `token` only via
        // `update_dirty_state_locked`, which does not touch the page list. The pointer is valid
        // while the lock is held, which `token` proves.
        let page_list = unsafe { &mut *self.page_list_locked_ptr(token) };
        // Capture the `Copy` state by value and the interval tracker by `&mut`, so that the
        // closure does not reload constants through pointers on every page.
        let per_page = {
            let interval_start = &mut interval_start;
            let token = &*token;
            move |slot: NonNull<VmPageOrMarker>, off: u64| {
                // SAFETY: `slot` points to a live slot of `self`'s page list. It is only read
                // through `p`, and nothing writes to it while `p` is in use: the traversal holds
                // no reference to it, and the only other page list pointer kept here,
                // `interval_start`, is not written through until after the last use of `p` (see
                // below).
                let p = unsafe { slot.as_ref() };
                // VMOs with a page source should never have references.
                debug_assert!(!p.is_reference());
                if p.is_page() {
                    // If the caller indicates that they're only cleaning zero pages, any committed
                    // pages need to be left dirty. This is checked first as it does not need the
                    // page to be looked up.
                    if is_zero_range {
                        return Status::NEXT;
                    }
                    let page = p.page();
                    // If the page is pinned we have to leave it Dirty in case it is still being
                    // written to via DMA. The VM system will be unaware of these writes, and so we
                    // choose to be conservative here and might end up with pinned pages being left
                    // dirty for longer, until a writeback is attempted after the unpin.
                    // SAFETY: `page` is in `self`'s page list in the `OBJECT` state and `token`
                    // proves `self`'s lock is held, serializing all accesses to its object state.
                    if unsafe { page.get_pin_count() } > 0 {
                        return Status::NEXT;
                    }
                    // Transition pages from Dirty to AwaitingClean.
                    // SAFETY: See above.
                    if unsafe { Self::is_page_dirty(page) } {
                        // SAFETY: See above; the page is already in `self`'s page list.
                        unsafe {
                            self.update_dirty_state_locked(
                                token,
                                page,
                                off,
                                DirtyState::AwaitingClean,
                                /*is_pending_add=*/ false,
                            );
                        }
                    }
                    // The page is not Dirty at this point: either it was not, or it has just been
                    // transitioned.
                    // SAFETY: See above.
                    debug_assert!(!unsafe { Self::is_page_dirty(page) });
                    return Status::NEXT;
                }
                // Transition dirty zero intervals to AwaitingClean.
                if p.is_interval_zero() {
                    if !p.is_zero_interval_dirty() {
                        // The only other state we support is Untracked.
                        debug_assert!(p.is_zero_interval_untracked());
                        return Status::NEXT;
                    }
                    if p.is_interval_start() || p.is_interval_slot() {
                        // Start tracking a dirty interval. It will only transition once the end is
                        // encountered.
                        debug_assert!(interval_start.is_none());
                        *interval_start = Some((slot, off));
                    }
                    if p.is_interval_end() || p.is_interval_slot() {
                        // Now that we've encountered the end, the entire interval can be
                        // transitioned to AwaitingClean. This is done by setting the
                        // `zero_interval_awaiting_clean_length` of the start sentinel.
                        // TODO: If the writeback began partway into the interval, try to coalesce
                        // the start's awaiting clean length with the range being cleaned here if it
                        // immediately follows.
                        // Taking the start also resets it so we can track a new one later.
                        if let Some((mut start, start_off)) = interval_start.take() {
                            // Set the new AwaitingClean length to the max of the old value and the
                            // new one. See comments in writeback_end_locked for an explanation.
                            // SAFETY: `start` is a slot pointer handed out by this traversal (to
                            // an earlier callback, or to this one for an interval slot). The
                            // traversal frees no nodes, so the slot is live, and nothing else
                            // accesses it: the traversal holds no reference to it, and `p`, which
                            // for an interval slot refers to the same slot, is not used again.
                            let start = unsafe { start.as_mut() };
                            let old_len = start.zero_interval_awaiting_clean_length();
                            start.set_zero_interval_awaiting_clean_length(cmp::max(
                                off - start_off + (::page::SIZE as u64),
                                old_len,
                            ));
                        }
                    }
                    return Status::NEXT;
                }
                // This was a marker (which is already clean).
                debug_assert!(p.is_marker());
                Status::NEXT
            }
        };
        let status = page_list.for_every_page_in_range_ptr(start_offset, end_offset, per_page);
        // We don't expect a failure from the traversal.
        debug_assert!(status.is_ok());

        // Process the last partial interval.
        if let Some((mut start, start_off)) = interval_start {
            // SAFETY: `start` is a slot pointer handed out by the traversal above, which freed no
            // nodes, and nothing has touched the page list since it returned, so the slot is live
            // and no reference to it exists.
            let start = unsafe { start.as_mut() };
            debug_assert!(start.is_interval_start());
            let old_len = start.zero_interval_awaiting_clean_length();
            start
                .set_zero_interval_awaiting_clean_length(cmp::max(end_offset - start_off, old_len));
        }

        // Set any mappings for this range to read-only, so that a permission fault is triggered
        // the next time the page is written to in order for us to track it as dirty. This might
        // cover more pages than the Dirty pages found in the page list traversal above, but we
        // choose to do this once for the entire range instead of per page; pages in the
        // AwaitingClean and Clean states will already have their write permission removed, so this
        // is a no-op for them.
        let range_update = VmCowRange::new(start_offset, end_offset - start_offset);
        self.range_change_update_locked(token, range_update, RangeChangeOp::RemoveWrite, None);
        // No range change needs to be processed for the children since children, by virtue of
        // being copy-on-write, cannot have a writable mapping.

        self.continuous_attribution_validation_assert(token);
        Ok(())
    }

    /// See `VmObject::writeback_end`.
    pub fn writeback_end_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> Result<(), Status> {
        self.assert_canary();

        debug_assert!(range.is_page_aligned());

        assert!(self.page_source().is_some());

        if !range.is_bounded_by(self.size_locked(token)) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        if self.page_source_type() != PageSourceType::UserPager {
            core::hint::cold_path();
            return Err(Status::NOT_SUPPORTED);
        }

        let start_offset = range.offset;
        let end_offset = range.end();

        // This tracks the end offset until which all zero intervals can be marked clean. This is a
        // running counter that is maintained across multiple zero intervals. Each time we
        // encounter a new interval start, we take the max of the existing value and the
        // `zero_interval_awaiting_clean_length` of the new interval. This is because when zero
        // intervals are truncated at the end or split, their awaiting clean length does not get
        // updated, even if it's larger than the current interval length. This is an optimization
        // to avoid having to potentially walk to another node to find the relevant start to
        // update. The reason it is safe to leave the awaiting clean length unchanged is that it
        // should be possible to apply it to any new zero intervals that get added later beyond the
        // truncated interval. The user pager has indicated its intent to write a range as zeros,
        // so until the point that it actually completes the writeback, it doesn't matter if zero
        // intervals are removed and re-added, as long as they fall in the range that was initially
        // indicated as being written back as zeros.
        let mut interval_awaiting_clean_end: u64 = 0;

        // Mark any AwaitingClean pages Clean. Remove AwaitingClean intervals that can be fully
        // cleaned, otherwise clip the interval start removing the part that has been cleaned. Any
        // intervals are handled outside the remove_pages callback in the main loop body to allow
        // for safely modifying the page_list.
        let mut remove_start = start_offset;
        while remove_start < end_offset {
            let mut interval_offset = u64::MAX;
            {
                // SAFETY: This traversal is the only page list access until it returns; the
                // callback reaches `token` only via `update_dirty_state_locked`, which does not
                // touch the page list. The pointer is valid while the lock is held, which `token`
                // proves.
                let page_list = unsafe { &mut *self.page_list_locked_ptr(token) };
                let _ = page_list.remove_pages(remove_start, end_offset, |p, off| {
                    // VMOs with a page source should never have references.
                    debug_assert!(!p.is_reference());
                    if p.is_page() {
                        let page = p.page();
                        // Transition pages from AwaitingClean to Clean.
                        // SAFETY: `page` is in `self`'s page list in the `OBJECT` state and
                        // `token` proves `self`'s lock is held, serializing all accesses to its
                        // object state.
                        if unsafe { Self::is_page_awaiting_clean(page) } {
                            // SAFETY: See above; the page is already in `self`'s page list.
                            unsafe {
                                self.update_dirty_state_locked(
                                    token,
                                    page,
                                    off,
                                    DirtyState::Clean,
                                    /*is_pending_add=*/ false,
                                );
                            }
                        }
                        // The page is not AwaitingClean at this point: either it was not, or it
                        // has just been transitioned.
                        // SAFETY: See above.
                        debug_assert!(!unsafe { Self::is_page_awaiting_clean(page) });
                        return Status::NEXT;
                    }
                    // Exit out to handle any zero intervals.
                    if p.is_interval_zero() {
                        if !p.is_zero_interval_dirty() {
                            // The only other state we support is Untracked.
                            debug_assert!(p.is_zero_interval_untracked());
                            return Status::NEXT;
                        }
                        // If we see an end interval then we have started processing somewhere
                        // inside a zero interval. In this case there's nothing for us to do.
                        if p.is_interval_end() {
                            return Status::NEXT;
                        }
                        interval_offset = off;
                        return Status::STOP;
                    }
                    // This was a marker (which is already clean).
                    debug_assert!(p.is_marker());
                    Status::NEXT
                });
            }
            // If no interval found then we are done.
            if interval_offset >= end_offset {
                break;
            }
            let page_list = self.page_list_locked(token);
            let interval = page_list
                .lookup(interval_offset)
                .expect("interval sentinel found by the traversal above");
            debug_assert!(
                interval.is_interval_zero()
                    && interval.is_zero_interval_dirty()
                    && !interval.is_interval_end()
            );
            // First determine where this interval ends, which includes where we would resume
            // iteration from.
            let mut interval_end = interval_offset;
            if interval.is_interval_start() {
                let _ = page_list.for_every_page_in_range(
                    interval_offset + (::page::SIZE as u64),
                    VmPageList::MAX_SIZE,
                    |slot, offset| {
                        debug_assert!(slot.is_interval_end());
                        interval_end = offset;
                        Status::STOP
                    },
                );
                debug_assert!(interval_end > interval_offset);
            }
            remove_start = interval_end + (::page::SIZE as u64);

            // See if we can advance interval_awaiting_clean_end to include the
            // `zero_interval_awaiting_clean_length` of this interval.
            interval_awaiting_clean_end = cmp::max(
                interval_awaiting_clean_end,
                interval_offset + interval.zero_interval_awaiting_clean_length(),
            );

            // Handle interval slots separately for simplicity.
            if interval.is_interval_slot() {
                if interval_offset < interval_awaiting_clean_end {
                    self.page_list_locked_mut(token).remove_content(interval_offset);
                }
                continue;
            }
            debug_assert!(interval.is_interval_start());

            if interval_end <= end_offset && interval_awaiting_clean_end > interval_end {
                // The entire interval is clean (and within the range of our operation), so can
                // remove it.
                let page_list = self.page_list_locked_mut(token);
                page_list.remove_content(interval_offset);
                page_list.remove_content(interval_end);
            } else {
                // The entire interval cannot be marked clean. Move forward the start by awaiting
                // clean length, which will also set the `zero_interval_awaiting_clean_length` for
                // the resulting interval.
                let clean_length =
                    cmp::min(interval_awaiting_clean_end, end_offset) - interval_offset;
                // Ignore any errors. Cleaning is best effort. If this fails, the interval will
                // remain as is and get retried on another writeback attempt.
                let _ = self
                    .page_list_locked_mut(token)
                    .clip_interval_start(interval_offset, clean_length);
            }
        }

        self.continuous_attribution_validation_assert(token);
        Ok(())
    }

    /// Detaches the page source and removes all pages except those that are pinned or
    /// dirty-tracked and not Clean.
    pub fn detach_source(&self) {
        self.assert_canary();

        stack_pin_init!(let deferred = DeferredOps::new(self));
        #[rustfmt::skip]
        ksync::lock!(let mut guard = self
            .lock_ref()
            .lock_policy_with::<CriticalMutexOrderedPolicy>(self.lock_order() as usize));
        let token = guard.as_mut().token_mut();

        let page_source = self.page_source().expect("detach_source requires a page source");
        page_source.detach();

        // We would like to remove all committed pages so that all future page faults on this VMO
        // and its clones can fail in a deterministic manner. However, if the page source is a
        // userpager, we need to hold on to un-Clean (Dirty and AwaitingClean pages) so that they
        // can be written back by the page source. If the page source is not a userpager, its pages
        // will not be dirty tracked to begin with i.e. their dirty state will be Untracked, so we
        // will end up removing all pages.

        // We should only be removing pages from the root VMO.
        debug_assert!(!self.has_parent_locked(token));

        // Even though we might end up removing only a subset of the pages, unmap them all at once
        // as an optimization. Only the userpager is expected to access (dirty) pages beyond this
        // point, in order to write back their contents, where the cost of the writeback is
        // presumably much larger than page faults to update hardware page table mappings for
        // resident pages.
        let size = self.size_locked(token);
        self.range_change_update_locked(
            token,
            VmCowRange::new(0, size),
            RangeChangeOp::Unmap,
            Some(deferred.as_mut()),
        );

        let mut populated_slots_removed: u32 = 0;
        {
            let freed_list = deferred.as_mut().freed_list(self);
            let mut page_remover = BatchPQRemove::new(freed_list);

            // Remove all clean (or untracked) pages.
            // TODO(rashaeqbal): Pages that linger after this will be written back and marked clean
            // at some point, and will age through the pager-backed queues and eventually get
            // evicted. We could adopt an eager approach instead, and decommit those pages as soon
            // as they get marked clean. If we do that, we could also extend the eager approach to
            // supply_pages, where pages get decommitted on supply, i.e. the supply is a no-op.
            let _ = self.page_list_locked_mut(token).remove_pages(0, size, |p, _off| {
                // A marker is a clean zero page. Replace it with an empty slot.
                if p.is_marker() {
                    *p = VmPageOrMarker::empty();
                    return Status::NEXT;
                }

                // Zero intervals are dirty so they cannot be removed.
                if p.is_interval_zero() {
                    // TODO: Remove clean intervals once they are supported.
                    debug_assert!(!p.is_zero_interval_clean());
                    return Status::NEXT;
                }

                // VMOs with a page source cannot have references.
                debug_assert!(p.is_page());
                let page = p.page();

                // We cannot remove the page if it is dirty-tracked but not clean.
                // SAFETY: `page` is in `self`'s page list in the `OBJECT` state and the lock is
                // held by `guard`, serializing all accesses to its object state.
                if unsafe { Self::is_page_dirty_tracked(page) && !Self::is_page_clean(page) } {
                    // SAFETY: See above.
                    debug_assert!(!unsafe { page.is_loaned() });
                    return Status::NEXT;
                }

                // We cannot remove the page if it's pinned.
                // SAFETY: See above.
                if unsafe { page.get_pin_count() } > 0 {
                    return Status::NEXT;
                }

                // SAFETY: `page` is released from a populated slot of `self`'s page list under
                // `self`'s lock, so it is owned by `self` and present in the page queues.
                unsafe { page_remover.push(p.release_page()) };
                populated_slots_removed += 1;
                Status::NEXT
            });

            page_remover.flush();
        }
        if populated_slots_removed > 0 {
            self.continuous_attribution_tracker_locked_mut(token)
                .decrement(populated_slots_removed);
        }
    }

    /// Looks up and zeroes a pinned page at `offset` in a direct user-pager-backed VMO.
    ///
    /// Looking up through a `LookupCursor` ensures that we request a dirty transition if needed by
    /// the pager before zeroing.
    ///
    /// # Safety
    ///
    /// * `_token` must be for the lock of `self`.
    /// * `offset` must be page-aligned and within `self`'s size, and `self.page_list` must hold
    ///   the pinned page `slot_page` at `offset`.
    /// * `deferred` must have been constructed for `self`.
    /// * `page_request` must not hold an active request.
    /// * `self` must be `LifeCycle::Alive`, directly backed by a user pager, and not a hidden
    ///   node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    #[cold]
    #[inline(never)]
    unsafe fn zero_pinned_user_pager_page_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        offset: u64,
        slot_page: VmPagePtr,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> Result<(), Status> {
        // SAFETY: Per `# Safety`, `_token` proves `self`'s lock is held for as long as `cursor`
        // exists. `cursor` is only used for the `require_owned_page` call below and is then
        // dropped, so nothing else changes a page list, parent link, parent limit, or size while
        // it is used. `offset` is page-aligned and within `self`'s size, `self` is alive and not
        // hidden, and ancestor lock-acquisition conditions hold.
        stack_pin_init!(let cursor = unsafe {
            LookupCursor::new(self, VmCowRange::new(offset, ::page::SIZE as u64))
        });
        // SAFETY: `deferred` was constructed for `self`, `1` is non-zero and at most the 1-page
        // window of `cursor`, and `page_request` has no active request.
        let result =
            unsafe { cursor.as_mut().require_owned_page(true, 1, deferred, page_request) }?;
        debug_assert_eq!(result.page, slot_page);
        // Zero the page we looked up.
        // SAFETY: `result.page` is a valid allocated page in `self.page_list` with a writable
        // physmap mapping.
        unsafe { zero_page_paddr(result.page.paddr()) };
        Ok(())
    }

    /// Specialized internal version of [`Self::zero_pages_locked`] that only operates for a VMO
    /// with a user pager.
    ///
    /// `dirty_track` can be set to `true` if any zeroes inserted are to be treated as Dirty,
    /// otherwise they are not dirty tracked.
    ///
    /// Returns the number of bytes that were actually zeroed, which may be nonzero even if the
    /// returned status is an error.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `range` must be page-aligned.
    /// * `deferred` must have been constructed for `self`.
    /// * `page_request` must not hold an active request.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    #[inline(never)]
    unsafe fn zero_pages_direct_user_pager_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        dirty_track: bool,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        // Validate inputs.
        debug_assert!(range.is_page_aligned());
        debug_assert!(range.is_bounded_by(self.size_locked(token)));
        debug_assert!(self.page_source_type() == PageSourceType::UserPager);

        // Give us easier names for our range.
        let start = range.offset;
        let end = range.end();

        let mut processed_len: u64 = 0;

        if start == end {
            return (Ok(()), processed_len);
        }

        // If we're not asked to dirty track, we will need to drop pages, because if a page is
        // present it is going to be in one of the dirty tracked states (Clean, Dirty,
        // AwaitingClean). So check for any pinned pages first.
        if !dirty_track && self.any_pages_pinned_locked(token, start, (end - start) as usize) {
            core::hint::cold_path();
            return (Err(Status::BAD_STATE), processed_len);
        }

        // Unmap any page that is touched by this range in any of our, or our children's, mapping
        // regions. We do this on the assumption we are going to be able to free pages either
        // completely or by turning them into markers and it's more efficient to unmap once in bulk
        // here.
        self.range_change_update_locked(
            token,
            range,
            RangeChangeOp::Unmap,
            Some(deferred.as_mut()),
        );

        // Inserting zero intervals can modify the page list such that new nodes are added and
        // deleted. So we cannot safely insert zero intervals while iterating the page list. The
        // pattern we follow here is:
        // 1. Traverse the page list to find a range that can be represented by a zero interval
        //    instead.
        // 2. When such a range is found, break out of the traversal, and insert the zero interval.
        // 3. Advance past the zero interval we inserted and resume the traversal from there, until
        //    we've covered the entire range.

        // The start offset at which to start the next traversal loop.
        let mut next_start_offset = start;
        // Dirty state for zero intervals we insert.
        let required_state =
            if dirty_track { ZeroRangeDirtyState::Dirty } else { ZeroRangeDirtyState::Untracked };
        let page_source =
            self.page_source().expect("zero_pages_direct_user_pager_locked requires a page source");
        #[derive(Copy, Clone, Default)]
        struct ZeroIntervalState {
            add_zero_interval: bool,
            start: u64,
            end: u64,
            replace_page: bool,
            overwrite_interval: bool,
        }
        loop {
            // Track whether we find ourselves in a zero interval.
            let mut in_interval = false;
            // The start of the zero interval if we are in one.
            let mut interval_start = next_start_offset;
            let prev_start_offset = next_start_offset;
            // State tracking information for inserting a new zero interval.
            let state = Cell::new(ZeroIntervalState::default());

            let page_list_ptr = self.page_list_locked_ptr(token);
            let per_page = |mut p: NonNull<VmPageOrMarker>, off: u64| {
                // SAFETY: `p` points to a live slot in the current node of `self.page_list`, and
                // no other reference to the slot is live.
                let slot = unsafe { p.as_ref() };
                // We cannot have references in pager-backed VMOs.
                debug_assert!(!slot.is_reference());

                // If this is a page, see if we can remove it and absorb it into a zero interval.
                if slot.is_page() {
                    let slot_page = slot.page();
                    // SAFETY: `slot_page` is in `self.page_list` in the `OBJECT` state under
                    // `self`'s lock.
                    if unsafe { slot_page.get_pin_count() } > 0 {
                        core::hint::cold_path();
                        debug_assert!(dirty_track);
                        // Cannot remove this page if it is pinned. Lookup the page and zero it.
                        // Looking up ensures that we request dirty transition if needed by the
                        // pager.
                        // SAFETY: Per `# Safety`, `token` is for `self`'s lock, `self` is alive,
                        // directly user-pager backed, and not hidden, `deferred` was constructed
                        // for `self`, `page_request` has no active request (any error aborts the
                        // traversal and returns), and ancestor lock-acquisition conditions hold.
                        // `slot_page` is the pinned page at `off` in `self.page_list`.
                        if let Err(err) = unsafe {
                            self.zero_pinned_user_pager_page_locked(
                                token,
                                off,
                                slot_page,
                                deferred.as_mut(),
                                page_request,
                            )
                        } {
                            return err;
                        }
                        processed_len += ::page::SIZE as u64;
                        next_start_offset = off + (::page::SIZE as u64);
                        return Status::NEXT;
                    }
                    // Break out of the traversal. We can release the page and add a zero interval
                    // instead.
                    state.set(ZeroIntervalState {
                        add_zero_interval: true,
                        start: off,
                        end: off,
                        replace_page: true,
                        overwrite_interval: false,
                    });
                    return Status::STOP;
                }

                // Otherwise this is a marker or zero interval, in which case we already have
                // zeroes, but we might need to change the dirty state.
                debug_assert!(slot.is_marker() || slot.is_interval_zero());
                if slot.is_interval_start() {
                    // Track the interval start so we know how much to add to processed_len later.
                    interval_start = off;
                    in_interval = true;
                    if slot.zero_interval_dirty_state() != required_state {
                        // If we find the matching end, we will update state.end with the correct
                        // offset. Do not terminate the traversal yet.
                        state.set(ZeroIntervalState {
                            add_zero_interval: true,
                            start: interval_start,
                            end: u64::MAX,
                            replace_page: false,
                            overwrite_interval: true,
                        });
                    }
                } else if slot.is_interval_end() {
                    if slot.zero_interval_dirty_state() != required_state {
                        state.set(ZeroIntervalState {
                            add_zero_interval: true,
                            start: if in_interval { interval_start } else { u64::MAX },
                            end: off,
                            replace_page: false,
                            overwrite_interval: true,
                        });
                        return Status::STOP;
                    }
                    // Add the range from interval start to end.
                    processed_len += off + (::page::SIZE as u64) - interval_start;
                    in_interval = false;
                } else {
                    // This is either a single interval slot or a marker. Terminate the traversal
                    // to overwrite with a zero interval if:
                    //  - this is an interval slot with a different dirty state, OR
                    //  - this is a marker and we're asked to not dirty track, since a marker is a
                    //    clean zero page.
                    // SAFETY: `p` points to a live slot in `self.page_list` with no other
                    // reference to it (`slot` is not used below).
                    let slot_mut = unsafe { p.as_mut() };
                    if slot_mut.is_marker() && !dirty_track {
                        // Release the marker so that it can be replaced by a gap by the traversal
                        // loop first, where the new zero interval will then be added.
                        *slot_mut = VmPageOrMarker::empty();
                    }
                    if slot_mut.is_empty()
                        || (slot_mut.is_interval_slot()
                            && slot_mut.zero_interval_dirty_state() != required_state)
                    {
                        state.set(ZeroIntervalState {
                            add_zero_interval: true,
                            start: off,
                            end: off,
                            replace_page: false,
                            overwrite_interval: slot_mut.is_interval_slot(),
                        });
                        return Status::STOP;
                    }
                    processed_len += ::page::SIZE as u64;
                }
                next_start_offset = off + (::page::SIZE as u64);
                Status::NEXT
            };
            let per_gap = |gap_start: u64, gap_end: u64| {
                // This gap will be replaced with a zero interval. Invalidate any read requests in
                // this range. Since we have just validated that this is a gap in the page list we
                // can directly call `on_pages_supplied`, instead of iterating through the gaps
                // using `invalidate_read_requests_locked`.
                page_source.on_pages_supplied(gap_start, gap_end - gap_start);
                // We have found a new zero interval to insert. Break out of the traversal.
                state.set(ZeroIntervalState {
                    add_zero_interval: true,
                    start: gap_start,
                    end: gap_end - (::page::SIZE as u64),
                    replace_page: false,
                    overwrite_interval: false,
                });
                Status::STOP
            };
            // SAFETY: Per `# Safety`, `token` is for `self`'s lock and `page_list_ptr` carries raw
            // `KCell`/`UnsafeCell` provenance (not derived from `&mut VmPageList`), with no
            // reference into `self.page_list` live across the call. When a pinned page is
            // encountered, `zero_pinned_user_pager_page_locked` ->
            // `LookupCursor::require_owned_page` finds the already-owned, non-loaned pinned page
            // in `self` and only performs read-only lookups on `self.page_list` (via
            // `lookup_nearest_mut_cursor_raw` and `prepare_for_write_locked`), without
            // inserting/erasing B-tree nodes or materializing `&mut VmPageList` or
            // `&mut VmPageListNode`. Interval additions and overwrites happen only after the
            // traversal stops, and no slot pointer is used after `per_page` returns.
            let status = unsafe {
                VmPageList::remove_pages_and_iterate_gaps_ptr(
                    page_list_ptr,
                    prev_start_offset,
                    end,
                    per_page,
                    per_gap,
                )
            };
            // Bubble up any errors from LookupCursor.
            if let Err(err) = status {
                core::hint::cold_path();
                return (Err(err), processed_len);
            }

            let mut state = state.get();
            // Add any new zero interval.
            if state.add_zero_interval {
                let status = if state.replace_page {
                    debug_assert_eq!(state.start, state.end);
                    let page = self
                        .page_list_locked_mut(token)
                        .replace_page_with_zero_interval(state.start, required_state);
                    // SAFETY: `page` was just released from `self.page_list` under `self`'s lock.
                    debug_assert_eq!(unsafe { page.get_pin_count() }, 0);
                    // SAFETY: `page` was just released from `self.page_list` (in the `OBJECT` state
                    // in the page queues and owned by `self`), and `deferred` was constructed for
                    // `self`.
                    unsafe { self.remove_page_locked(token, page, deferred.as_mut()) };
                    self.continuous_attribution_tracker_locked_mut(token).decrement(1);
                    Ok(())
                } else if state.overwrite_interval {
                    let old_start = (state.start != u64::MAX).then_some(state.start);
                    let old_end = (state.end != u64::MAX).then_some(state.end);
                    if state.start == u64::MAX {
                        state.start = next_start_offset;
                    }
                    if state.end == u64::MAX {
                        state.end = end - (::page::SIZE as u64);
                    }
                    self.page_list_locked_mut(token).overwrite_zero_interval(
                        old_start,
                        old_end,
                        state.start,
                        state.end,
                        required_state,
                    )
                } else {
                    self.page_list_locked_mut(token).add_zero_interval(
                        state.start,
                        state.end + (::page::SIZE as u64),
                        required_state,
                    )
                };
                if let Err(err) = status {
                    core::hint::cold_path();
                    debug_assert_eq!(err, Status::NO_MEMORY);
                    return (Err(err), processed_len);
                }
                processed_len += state.end - state.start + (::page::SIZE as u64);
                next_start_offset = state.end + (::page::SIZE as u64);
            } else {
                // Handle the last partial interval. Or the case where we did not advance
                // `next_start_offset` at all, which can only happen if the range fell entirely
                // inside an interval.
                if in_interval || next_start_offset == prev_start_offset {
                    // If the range fell entirely inside an interval, verify that it was indeed a
                    // zero interval.
                    debug_assert!(
                        next_start_offset != prev_start_offset
                            || self
                                .page_list_locked(token)
                                .is_offset_in_zero_interval(next_start_offset)
                    );
                    // If entirely inside an interval, we have one of two possibilities:
                    //  (1) The interval is already in `required_state` in which case we don't need
                    //      to do anything.
                    //  (2) The interval is not in `required_state`. We do not expect this case in
                    //      practice, so instead of splitting up a zero interval in the middle just
                    //      to change its dirty state, claim that we processed the range.
                    processed_len += end - interval_start;
                    next_start_offset = end;
                }
            }
            // Ensure we're making progress.
            debug_assert!(next_start_offset > prev_start_offset);
            if next_start_offset >= end {
                break;
            }
        }

        self.continuous_attribution_validation_assert(token);
        (Ok(()), processed_len)
    }

    /// Specialized internal version of [`Self::zero_pages_locked`] that only operates for
    /// pager-backed VMOs backed by non-user pagers.
    ///
    /// `range` must be page-aligned and within the size of `self`. Both are only `debug_assert!`ed.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `self` must be contiguous (`PageSourceType::Contiguous`). This relies on a contiguous VMO
    ///   holding nothing but pages: every populated slot in `range` is zeroed as a page, and both
    ///   that `self` is contiguous and that a slot holds a page are only `debug_assert!`ed.
    #[inline(never)]
    unsafe fn zero_pages_contiguous(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) {
        debug_assert!(range.is_bounded_by(self.size_locked(token)));
        debug_assert!(range.is_page_aligned());
        debug_assert!(!self.is_root_source_user_pager_backed());
        debug_assert!(self.page_source_type() == PageSourceType::Contiguous);

        // The presence of a `page_source` is inferred from `PageSourceType::Contiguous`.
        debug_assert!(self.page_source().is_some());
        // If there is a direct page source, there is no parent.
        debug_assert!(!self.has_parent_locked(token));

        // There is no need to call `range_change_update_locked` since we do not add or
        // remove any pages.

        // Gaps already represent zero, so we do not need to handle them.
        let status = self.page_list_locked(token).for_every_page_in_range(
            range.offset,
            range.end(),
            #[inline(always)]
            |p, _off| {
                // Contiguous VMOs only contain pages.
                debug_assert!(p.is_page());
                // SAFETY: Per `# Safety`, `self` is contiguous, so the slot holds a valid
                // `VmPage` in `self.page_list` while `self`'s lock is held, with a valid writable
                // physical page in the physmap.
                unsafe { zero_page_paddr(p.page().paddr()) };
                Status::NEXT
            },
        );

        debug_assert!(status.is_ok());
    }

    /// Decrements the cow share count on the owner of the content at `offset`. If there is no
    /// content, or if the content isn't associated with a refcount (i.e. it's a marker), this
    /// function does nothing.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `offset` must be page-aligned and within `self`'s size, and the walk to find content at
    ///   `offset` must pass `self` (the slot in `self` is empty in a pager-backed hierarchy, or a
    ///   parent content marker in an anonymous hierarchy), so `self` holds a counted CoW share on
    ///   any page or reference owned by a hidden ancestor at `offset`.
    /// * `deferred` must have been constructed for `self`.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    #[inline(never)]
    unsafe fn decrement_potential_ancestor_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        offset: u64,
    ) -> Result<(), Status> {
        stack_try_pin_init!(
            // SAFETY: Per `# Safety`, `token` is for `self`'s lock, `offset` is page-aligned and
            // within `self`'s size (so the 1-page range is valid), `self` is alive and not hidden,
            // and ancestor lock-acquisition conditions hold for this call, within which `cursor`
            // is used and dropped.
            let cursor = unsafe {
                self.get_lookup_cursor_locked(token, VmCowRange::new(offset, ::page::SIZE as u64))
            }
        );
        let mut cursor = match cursor {
            Ok(cursor) => cursor,
            Err(status) => {
                core::hint::cold_path();
                return Err(status);
            }
        };
        cursor.as_mut().establish_cursor();

        // Caller promises that `offset` references content in a parent or ancestor of the current
        // node, so the following assertion is valid.
        debug_assert!(cursor.is_cursor_valid() && !cursor.target_is_owner());
        let has_ref_count = cursor.cursor_is_page() || cursor.cursor_is_reference();
        if !has_ref_count {
            return Ok(());
        }
        // We know that `cursor.owner_cursor` is non-empty, since we either have a page or a
        // reference. We know that `cursor.owner_info.owner` exists, because we're not the owner.
        let owner_cursor = cursor.owner_cursor;
        debug_assert!(owner_cursor.is_some() && !cursor.target_is_owner());
        let this_cursor = cursor.as_mut().project();
        let owner_info = this_cursor.owner_info.project();
        let owner_offset = *owner_info.owner_offset;
        let (owner, owner_token) = owner_info.owner.locked_mut();
        if !owner.is_hidden() {
            return Ok(());
        }
        // SAFETY: `owner_token` comes from the `LockedPtr` that holds `owner`'s lock.
        // `has_ref_count` is true, so `owner_cursor` is `Some` and points to the page or reference
        // slot at `owner_offset` in `owner`'s page list; no reference into that page list is live,
        // and `cursor` is only dropped after this call. A page in a locked node's page list is in
        // the `OBJECT` state and in the page queues, and a reference belongs to `pmm::node()`'s
        // `VmCompression`. `owner.is_hidden()` was checked above, and loaned pages are restricted
        // by `can_borrow` to direct user-pager root VMOs, so a page in a hidden node is never
        // loaned. Per `# Safety`, `self` holds a counted CoW share on `owner`'s content at
        // `offset`, and `deferred` was constructed for `self` in the same hierarchy as `owner`.
        unsafe {
            owner.decrement_cow_content_share_count(
                owner_token,
                owner_cursor.unwrap_unchecked(),
                owner_offset,
                deferred.as_mut().freed_list(self),
                pmm::node().get_page_compression(),
            );
        }
        Ok(())
    }

    /// Specialized internal version of [`Self::zero_pages_locked`] that only operates for a
    /// VMO where there is no direct `page_source`.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `range` must be page-aligned.
    /// * `deferred` must have been constructed for `self`.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    #[inline(never)]
    unsafe fn zero_pages_no_direct_page_source_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        mut deferred: Pin<&mut DeferredOps<'_>>,
        _page_request: Pin<&MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        debug_assert!(range.is_bounded_by(self.size_locked(token)));
        debug_assert!(range.is_page_aligned());

        // If the VMO is directly backed by a page source, it should be zeroed by
        // `zero_pages_contiguous` or `zero_pages_direct_user_pager_locked`.
        debug_assert!(self.page_source().is_none());

        // Since there is no direct `page_source`, it is always safe to decommit zero
        // pages.
        debug_assert!(self.can_decommit_zero_pages());

        // This function tries to zero pages as optimally as possible for most cases, so we attempt
        // increasingly expensive actions only if certain preconditions do not allow us to perform
        // the cheaper action. Broadly speaking, the sequence of actions that are attempted are as
        // follows.
        //  1) Try to decommit each page if the VMO allows it and doing so doesn't expose content in
        //     the parent (if any) that shouldn't be visible.
        //  2) Otherwise, if this is a child VMO and there is no committed page yet, allocate a zero
        //     page.
        //  3) Otherwise, look up the page, faulting it in if necessary, and zero the page. If the
        //     page source needs to supply or dirty track the page, a page request is initialized
        //     and we return early with `Status::SHOULD_WAIT`. The caller is expected to wait on the
        //     page request, and then retry. On the retry, we should be able to look up the page
        //     successfully and zero it.

        // Give us easier names for our range.
        let start = range.offset;
        let end = range.end();

        // Read once here and not at each use: the gap callback of the traversal below runs while
        // `per_page` holds the token mutably, so it cannot read them itself. Nothing this function
        // calls changes the parent link or the parent limit of `self` while its lock is held.
        let has_parent = self.has_parent_locked(token);
        let parent_limit = self.parent_limit_locked(token);
        // Helper closure to determine if this VMO can see parent contents at offset, or if a
        // length is specified as well in the range [offset, offset + length).
        let can_see_parent = |offset: u64, length: u64| -> bool {
            if !has_parent {
                return false;
            }
            offset < parent_limit && offset + length <= parent_limit
        };

        // Replace the page or reference in `slot_ptr` at `offset` with a zero marker in place.
        // If there is parent content, it must not be from a hidden owner.
        let replace_with_marker =
            |token: &mut LockToken<'_, VmCowPagesLockClass>,
             deferred: &mut Pin<&mut DeferredOps<'_>>,
             offset: u64,
             slot_ptr: NonNull<VmPageOrMarker>| {
                debug_assert!(has_parent && !self.node_has_parent_content_markers());
                // Remove any page that could be hanging around in the slot and replace it with a
                // marker in place, avoiding a redundant B-tree lookup or `&mut VmPageList` borrow.
                // SAFETY: `slot_ptr` is the live page/reference slot at `offset` in
                // `self.page_list`, with no other reference to the slot live.
                let mut transaction = unsafe {
                    self.begin_add_page_with_slot_locked(
                        token,
                        offset,
                        slot_ptr,
                        CanOverwriteSlot::PageOrRef,
                    )
                }
                // Absent bugs, `begin_add_page_with_slot_locked` on an existing page/ref slot with
                // `CanOverwriteSlot::PageOrRef` cannot fail.
                .expect("begin_add_page_with_slot_locked on existing page/ref slot");
                // SAFETY: `transaction` was begun on `self` just above with no intervening
                // page-list operation, `VmPageOrMarker::marker()` is neither a page nor a
                // reference, and `deferred` is `None`.
                let mut released_page = unsafe {
                    self.complete_add_page_locked(
                        token,
                        &mut transaction,
                        VmPageOrMarker::marker(),
                        None,
                    )
                };
                // Free the old page.
                if released_page.is_page() {
                    let page = released_page.release_page();
                    // SAFETY: `page` was just released from `self.page_list` (so it is in the
                    // `OBJECT` state in the page queues and owned by `self`), and `deferred` was
                    // constructed for `self`.
                    unsafe { self.remove_page_locked(token, page, deferred.as_mut()) };
                } else if released_page.is_reference() {
                    // SAFETY: `released_page` was just released from `self.page_list` under
                    // `self`'s lock.
                    unsafe { free_reference(released_page.release_reference()) };
                }
            };

        let zeroed_len = Cell::new(0u64);

        let mut already_unmapped = false;
        let do_unmap = |token: &LockToken<'_, VmCowPagesLockClass>,
                        already_unmapped: &mut bool,
                        deferred: &mut Pin<&mut DeferredOps<'_>>,
                        zeroed_len: u64| {
            if !*already_unmapped {
                *already_unmapped = true;
                // Let's apply the range change to the entire remaining range, which may be more
                // efficient than repeatedly unmapping smaller ranges (and doing the associated
                // mapping walk).
                self.range_change_update_locked(
                    token,
                    range.trimmed_from_start(zeroed_len),
                    RangeChangeOp::Unmap,
                    Some(deferred.as_mut()),
                );
            }
        };

        // Main page list traversal loop to remove any existing pages / markers, zero existing
        // pages, and also insert any new markers / zero pages in gaps as applicable. We use the
        // `VmPageList` traversal helper here instead of iterating over each offset in the range so
        // we can efficiently skip over gaps if possible.
        let mut status = Ok(());
        let mut current_start = start;
        struct GapState {
            found_gap: bool,
            gap_start: u64,
            gap_end: u64,
        }
        while status.is_ok() && current_start < end {
            let mut state = GapState { found_gap: false, gap_start: 0, gap_end: 0 };
            let page_list_ptr = self.page_list_locked_ptr(token);
            let per_page = |mut slot_ptr: NonNull<VmPageOrMarker>, offset: u64| {
                // SAFETY: `slot_ptr` points to a live slot in the current node of
                // `self.page_list`, and no other reference to the slot is live.
                let slot = unsafe { slot_ptr.as_ref() };

                // We don't expect intervals in non pager-backed VMOs.
                debug_assert!(!slot.is_interval());

                if slot.is_marker() {
                    // The slot is already zero.
                    zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                    return Status::NEXT;
                }

                // SAFETY: Any page in `slot` is in `self.page_list` in the `OBJECT` state under
                // `self`'s lock.
                if slot.is_page() && unsafe { slot.page().get_pin_count() } > 0 {
                    core::hint::cold_path();
                    let page = slot.page();
                    // Loaned pages cannot be pinned.
                    // SAFETY: `page` is in `self.page_list` under `self`'s lock.
                    debug_assert!(!unsafe { page.is_loaned() });
                    // SAFETY: `page` is a valid allocated physical page in `self.page_list` with a
                    // valid writable physical page in the physmap.
                    unsafe { zero_page_paddr(page.paddr()) };
                    zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                    return Status::NEXT;
                }

                if slot.is_reference() && !self.is_root_source_user_pager_backed() {
                    // We have a reference in an anonymous hierarchy, so we can just free it. Since
                    // a compressed reference wasn't mapped in the first place, we don't have to do
                    // an unmap here.
                    debug_assert!(self.node_has_parent_content_markers());
                    self.continuous_attribution_tracker_locked_mut(token).decrement(1);
                    // SAFETY: `slot_ptr` is a live slot in `self.page_list` with no other
                    // reference (`slot` is not used below), and the released reference belongs to
                    // the global compressor under `self`'s lock.
                    unsafe { free_reference(slot_ptr.as_mut().release_reference()) };
                    zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                    return Status::NEXT;
                }

                // All of the below cases require us to invalidate the mappings of the affected
                // pages.
                do_unmap(token, &mut already_unmapped, &mut deferred, zeroed_len.get());

                if slot.is_page() && !self.is_root_source_user_pager_backed() {
                    // We have a page in an anonymous hierarchy, so we can just remove the content.
                    debug_assert!(self.node_has_parent_content_markers());
                    self.continuous_attribution_tracker_locked_mut(token).decrement(1);
                    // SAFETY: `slot_ptr` is a live slot in `self.page_list` with no other
                    // reference (`slot` is not used below).
                    let page = unsafe { slot_ptr.as_mut().release_page() };
                    // SAFETY: `page` was just released from `self.page_list` (in the `OBJECT`
                    // state in the page queues and owned by `self`), and `deferred` was
                    // constructed for `self`.
                    unsafe { self.remove_page_locked(token, page, deferred.as_mut()) };
                    zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                    return Status::NEXT;
                }

                if slot.is_page_or_ref() {
                    // We have a page in a pager-backed hierarchy, so markers are needed to
                    // indicate zero.
                    debug_assert!(
                        self.is_root_source_user_pager_backed()
                            && !self.node_has_parent_content_markers()
                    );
                    replace_with_marker(token, &mut deferred, offset, slot_ptr);
                    zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                    return Status::NEXT;
                }

                // We handled all of the other node types, so this must be a parent content marker.
                // These only appear in non- user pager hierarchies.
                debug_assert!(
                    slot.is_parent_content()
                        && !self.is_root_source_user_pager_backed()
                        && self.node_has_parent_content_markers()
                );
                // We will no longer be referencing potential content in the parent or ancestor
                // node after we remove the parent content marker. We must decrement any share
                // count we have on that content.
                // SAFETY: Per `# Safety`, `token` is for `self`'s lock, `deferred` was constructed
                // for `self`, `self` is alive and not hidden, and ancestor lock-acquisition
                // conditions hold. `offset` is a slot offset within `self`'s size, and the slot in
                // `self` is a parent content marker in an anonymous hierarchy, so the walk passes
                // `self` and `self` holds a counted CoW share on any hidden ancestor content.
                if let Err(err) = unsafe {
                    self.decrement_potential_ancestor_locked(token, deferred.as_mut(), offset)
                } {
                    return err;
                }
                // An empty slot is safe to represent zero in non- user pager hierarchies.
                self.continuous_attribution_tracker_locked_mut(token).decrement(1);
                // SAFETY: `slot_ptr` points to the live slot at `offset` in `self.page_list` with
                // no other reference (`slot` is not used after `debug_assert!` above, and
                // `decrement_potential_ancestor_locked`'s `LookupCursor` has been dropped).
                unsafe { *slot_ptr.as_mut() = VmPageOrMarker::empty() };
                zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
                Status::NEXT
            };
            // SAFETY: Per `# Safety`, `token` is for `self`'s lock and `page_list_ptr` carries raw
            // `KCell`/`UnsafeCell` provenance (not derived from `&mut VmPageList`), with no
            // reference into `self.page_list` live across the call. `do_unmap` only performs
            // read-only `for_every_page_in_range` reads of `self.page_list` if `self` has pinned
            // pages; `replace_with_marker` modifies only the passed `slot_ptr` in place via
            // `begin_add_page_with_slot_locked` + `complete_add_page_locked`;
            // `decrement_potential_ancestor_locked` performs only a read-only lookup in
            // `self.page_list` before mutating an ancestor's page list; and gap insertion runs
            // after the traversal stops. Neither callback inserts/erases B-tree nodes,
            // materializes `&mut VmPageList` or `&mut VmPageListNode`, or uses a slot pointer
            // after `per_page` returns.
            status = unsafe {
                VmPageList::remove_pages_and_iterate_gaps_ptr(
                    page_list_ptr,
                    current_start,
                    end,
                    per_page,
                    #[inline(always)]
                    |gap_start: u64, gap_end: u64| {
                        if self.node_has_parent_content_markers() {
                            // Gaps are already zero when using parent content markers.
                            zeroed_len.set(zeroed_len.get() + (gap_end - gap_start));
                            return Status::NEXT;
                        }

                        // Since the node doesn't have parent content markers, and the current node
                        // isn't hidden, we must be in a user pager hierarchy.
                        debug_assert!(self.is_root_source_user_pager_backed());

                        // If empty slots imply zeroes, and the gap does not see parent contents, we
                        // already have zeroes.
                        if !can_see_parent(gap_start, gap_end - gap_start) {
                            zeroed_len.set(zeroed_len.get() + (gap_end - gap_start));
                            return Status::NEXT;
                        }

                        // `can_see_parent` implies that there is a `parent`.
                        debug_assert!(has_parent);

                        // Need to examine each offset in the gap to determine the action to
                        // perform. As this might modify the page list exit out of this iteration to
                        // ensure we do not corrupt the iterators.
                        state.found_gap = true;
                        state.gap_start = gap_start;
                        state.gap_end = gap_end;
                        Status::STOP
                    },
                )
            };
            if status.is_err() || !state.found_gap {
                break;
            }
            // Assuming gap processing goes well resume processing from the end of the gap next time
            // around.
            current_start = state.gap_end;

            // Process the gap found in the previous iteration.
            for offset in (state.gap_start..state.gap_end).step_by(::page::SIZE) {
                // First see if we can simply get done with an empty slot in the page list.
                if can_see_parent(offset, ::page::SIZE as u64) {
                    // Inform potential mappings of the marker we are inserting.
                    do_unmap(token, &mut already_unmapped, &mut deferred, zeroed_len.get());

                    // Begin the page list addition transaction. This is fallible and will
                    // allocate the slot/node.
                    let mut page_transaction = match self.begin_add_page_locked(
                        token,
                        offset,
                        CanOverwriteSlot::PageOrRef,
                    ) {
                        Ok(tx) => tx,
                        Err(err) => {
                            core::hint::cold_path();
                            status = Err(err);
                            break;
                        }
                    };

                    // Decrement the share count on the potential ancestor page/reference.
                    // SAFETY: Per `# Safety`, `token` is for `self`'s lock, `deferred` was
                    // constructed for `self`, `self` is alive and not hidden, and ancestor
                    // lock-acquisition conditions hold. `offset` is a page-aligned gap offset
                    // within `self`'s size in a user-pager hierarchy without parent content
                    // markers, and `begin_add_page_locked` left the slot empty until completion,
                    // so the walk passes `self` and `self` holds a counted CoW share on any hidden
                    // ancestor content.
                    status = unsafe {
                        self.decrement_potential_ancestor_locked(token, deferred.as_mut(), offset)
                    };
                    if status.is_ok() {
                        // Infallibly complete the transaction by inserting the zero marker.
                        // SAFETY: `page_transaction` was begun on `self` above and
                        // `decrement_potential_ancestor_locked` only mutates an ancestor's page
                        // list (not `self`'s), so `page_transaction` remains active and valid.
                        let old = unsafe {
                            self.complete_add_page_locked(
                                token,
                                &mut page_transaction,
                                VmPageOrMarker::marker(),
                                None,
                            )
                        };
                        debug_assert!(old.is_empty());
                    } else {
                        core::hint::cold_path();
                        // SAFETY: `page_transaction` was begun on `self` above with no mutating
                        // page-list operation on `self` since.
                        unsafe { self.cancel_add_page_locked(token, &mut page_transaction) };
                        break;
                    }
                }

                zeroed_len.set(zeroed_len.get() + (::page::SIZE as u64));
            }
        }

        self.continuous_attribution_validation_assert(token);
        (status, zeroed_len.get())
    }

    /// Zeroes `range` so that after successful completion all pages in the range read as zeros.
    ///
    /// The mechanism used to achieve this is not guaranteed to decommit, but it will try to.
    /// `range` must be page aligned offsets within the range of the object. `dirty_track`
    /// specifies whether the range being zeroed subscribes to dirty tracking, if `true` the range
    /// will start out as dirty. `dirty_track` only has meaning if the VMO supports dirty tracking,
    /// otherwise it is ignored.
    ///
    /// Returns a status code, and the number of bytes that were actually zeroed.
    /// This may be nonzero even if the returned status is an error.
    ///
    /// The returned status is one of the following:
    ///  * `Ok(())` => The whole range was successfully zeroed.
    ///  * `Err(Status::SHOULD_WAIT)` => The caller needs to wait on the `page_request` and then
    ///    retry the operation. The caller may advance the start offset by the number of zeroed
    ///    bytes before retrying.
    ///  * Any other error code indicates a failure to zero a part of the range or the whole
    ///    range.
    ///
    /// # Safety
    ///
    /// * `token` must be for the lock of `self`.
    /// * `range` must be page-aligned.
    /// * `deferred` must have been constructed for `self`.
    /// * `page_request` must not hold an active request.
    /// * `self` must be `LifeCycle::Alive` and must not be a hidden node.
    /// * The conditions of `get_lookup_cursor_locked` for acquiring the locks of the ancestors of
    ///   `self` must hold for the duration of the call.
    pub unsafe fn zero_pages_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        dirty_track: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        self.assert_canary();
        debug_assert!(self.lock_is_held());

        debug_assert!(range.is_bounded_by(self.size_locked(token)));
        debug_assert!(range.is_page_aligned());
        // This function is only valid on a visible node as it will not handle zeroing children.
        debug_assert!(!self.is_hidden());

        // If the VMO is directly backed by a user pager, it should be the root
        // VMO of the hierarchy.
        debug_assert!(
            self.page_source_type() == PageSourceType::Anonymous
                || self.page_source_type() == PageSourceType::Contiguous
                || !self.has_parent_locked(token)
        );

        // The methods we call below will call `range_change_update_locked` themselves if
        // needed.

        if self.page_source_type() == PageSourceType::Contiguous {
            // `zero_pages_contiguous` always succeeds.
            // SAFETY: Per `# Safety` `token` is for the lock of `self`, and `self` is contiguous:
            // its `page_source_type()` was checked just above.
            unsafe { self.zero_pages_contiguous(token, range) };
            return (Ok(()), range.len);
        }

        if self.page_source_type() == PageSourceType::UserPager {
            // We can perform efficient zeroing by inserting dirty zero intervals.
            // Handle this case separately.
            // SAFETY: `token`, `range`, `deferred` and `page_request` are passed on as they are,
            // and the callee's clauses for them are those of this function's `# Safety`, as are
            // its last two clauses, on `self` and on the locks of its ancestors.
            return unsafe {
                self.zero_pages_direct_user_pager_locked(
                    token,
                    range,
                    dirty_track,
                    deferred,
                    page_request,
                )
            };
        }

        // We already handled the non-user-pager and user-pager backed cases, so
        // there must be no pager.
        debug_assert!(
            self.page_source().is_none() && self.page_source_type() == PageSourceType::Anonymous
        );
        // SAFETY: Per `# Safety` `token` is for the lock of `self`, `range` is page-aligned and
        // `deferred` was constructed for `self`. The callee's last two clauses, on `self` and on
        // the locks of its ancestors, are the last two of `# Safety`.
        unsafe {
            self.zero_pages_no_direct_page_source_locked(token, range, deferred, page_request)
        }
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
        page_request: Option<Pin<&MultiPageRequest>>,
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
        let Some(ps) = self.page_source() else {
            return PageSourceType::Anonymous;
        };
        if ps.properties().is_user_pager {
            PageSourceType::UserPager
        } else {
            PageSourceType::Contiguous
        }
    }

    #[inline]
    fn discardable_tracker(&self) -> Option<Pin<&DiscardableVmoTracker>> {
        // SAFETY: `self.as_raw()` is a valid `VmCowPages` pointer, and `discardable_tracker_` is
        // const for the lifetime of `VmCowPages`.
        let raw = unsafe { bindings::cpp_vm_cow_pages_discardable_tracker(self.as_raw()) };
        if raw.is_null() {
            None
        } else {
            // SAFETY: `raw` is non-null, so it points to the `DiscardableVmoTracker` owned by
            // `self`'s `const ktl::unique_ptr`, which is live for `self`'s lifetime (making the
            // `from_raw_ref` reference valid). Pinning it is sound because the C++ tracker is
            // neither movable nor copyable, so its address is stable until `~VmCowPages` runs,
            // and that destructor drops it in place (`rust_discardable_vmo_tracker_destroy`)
            // before releasing the allocation.
            Some(unsafe { Pin::new_unchecked(DiscardableVmoTracker::from_raw_ref(raw)) })
        }
    }

    /// The page source, if any.
    #[inline]
    fn page_source(&self) -> Option<&PageSource> {
        self.state().page_source.as_deref()
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
    /// Does not access `self`'s page list, so callers may hold an exclusive page list borrow.
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
    /// Does not access `self`'s page list, so callers may hold an exclusive page list borrow.
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
                // clean yet; it will be moved out on `writeback_end_locked`.
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

    /// Increases the pin count of the range of pages given by `range`. The full range must already
    /// be committed and this either pins all pages in the range, or pins no pages and returns an
    /// error. The caller can assume that on success `range.len / page::SIZE` pages were pinned.
    /// The `offset` and `len` are assumed to be page aligned and within the range of `size`.
    /// All pages in the specified range are assumed to be non-loaned pages, so the caller is
    /// expected to replace any loaned pages beforehand if required.
    pub fn pin_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> Result<(), Status> {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        ltracef!("offset {:#x}, len {:#x}\n", range.offset, range.len);

        debug_assert!(range.is_page_aligned());
        debug_assert!(range.is_bounded_by(self.size_locked(token)));

        self.set_ever_pinned_locked(token, true);

        // Tracks our expected page offset when iterating to ensure all pages are present.
        let mut next_offset = range.offset;

        let mut status = self.page_list_locked(token).for_every_page_in_range(
            range.offset,
            range.end(),
            #[inline(always)]
            |p, page_offset| {
                if page_offset != next_offset || !p.is_page() {
                    core::hint::cold_path();
                    return Status::BAD_STATE;
                }
                let page = p.page();
                debug_assert_eq!(page.state(), VmPageState::OBJECT);
                // SAFETY: `page` is owned by `self` while `self`'s lock is held via `token`.
                debug_assert!(!unsafe { page.is_loaned() });

                // SAFETY: `page` is in `self`'s page list (`OBJECT` state) while `self`'s lock is
                // held via `token`, excluding all concurrent writers of the packed `flags` byte.
                let pin_count = unsafe { page.get_pin_count() };
                if u32::from(pin_count) == MAX_PIN_COUNT {
                    core::hint::cold_path();
                    return Status::UNAVAILABLE;
                }

                let new_pin_count = pin_count + 1;
                // SAFETY: `page` is in the `OBJECT` state and `self`'s held lock excludes all
                // concurrent writers of the packed `flags` byte.
                unsafe { page.set_pin_count(new_pin_count) };
                if new_pin_count == 1 {
                    // SAFETY: `page` is owned by `self` (`OBJECT` state) and already in a page
                    // queue.
                    unsafe { self.move_to_pinned_locked(token, page, page_offset) };
                }

                // Pinning every page in the largest vmo possible as many times as possible can't
                // overflow
                ::zr::static_assert!(
                    VmPageList::MAX_SIZE / (::page::SIZE as u64)
                        < u64::MAX / (MAX_PIN_COUNT as u64)
                );
                next_offset += ::page::SIZE as u64;
                Status::NEXT
            },
        );

        let actual = next_offset.wrapping_sub(range.offset) / (::page::SIZE as u64);
        // Count whatever pages we pinned, in the failure scenario this will get decremented on the
        // unpin.
        let new_pinned_count = self.pinned_page_count_locked(token) + actual;
        self.set_pinned_page_count_locked(token, new_pinned_count);

        if status.is_ok() {
            // If the missing pages were at the end of the range (or the range was empty) then our
            // iteration will have just returned Ok(()). Perform one final check that we actually
            // pinned the number of pages we expected to.
            let expected = range.len / (::page::SIZE as u64);
            if actual != expected {
                core::hint::cold_path();
                status = Err(Status::BAD_STATE);
            } else {
                return Ok(());
            }
        }

        // Should any errors occur we need to unpin everything.
        core::hint::cold_path();
        if next_offset > range.offset {
            self.unpin_locked(
                token,
                VmCowRange::new(range.offset, next_offset.wrapping_sub(range.offset)),
                None,
            );
        }
        status
    }

    /// See `VmObject::unpin`.
    /// An optional `DeferredOps` can be provided for the purposes of performing extra debugging
    /// checks, but otherwise has no functional requirement. The debug checks are optional as call
    /// sites may not be able to satisfy the locking requirements to construct a `DeferredOps`, and
    /// may know (due to be undoing a pin they themselves had started), that no checks need to be
    /// done.
    pub fn unpin_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        // `deferred` is only used by the `cfg(debug_assertions)` DebugUnpin range-change updates.
        #[cfg_attr(not(debug_assertions), allow(unused_variables, unused_mut))]
        mut deferred: Option<Pin<&mut DeferredOps<'_>>>,
    ) {
        self.assert_canary();
        debug_assert!(self.lock_is_held());

        // verify that the range is within the object
        assert!(range.is_bounded_by(self.size_locked(token)));
        // forbid zero length unpins as zero length pins return errors.
        assert!(!range.is_empty());

        let start_page_offset = ::page::round_down(range.offset as usize) as u64;
        let end_page_offset = ::page::round_up(range.end() as usize) as u64;

        #[cfg(debug_assertions)]
        // For any pages that have their pin count transition to 0, i.e. become unpinned, we want
        // to perform a range change op. For efficiency track contiguous ranges.
        let (mut completely_unpin_start, mut completely_unpin_len) = (0u64, 0u64);

        let mut unpin_count: u64 = 0;
        let status = self.page_list_locked(token).for_every_page_and_gap_in_range(
            start_page_offset,
            end_page_offset,
            |page, off| {
                // Only real pages can be pinned.
                assert!(page.is_page());

                let p = page.page();
                // SAFETY: `p` is in `self`'s page list (`OBJECT` state) while `self`'s lock is held
                // via `token`, excluding all concurrent writers of the packed `flags` byte.
                let pin_count = unsafe { p.get_pin_count() };
                assert!(pin_count > 0);
                let new_pin_count = pin_count - 1;
                // SAFETY: `p` is in the `OBJECT` state and `self`'s held lock excludes all
                // concurrent writers of the packed `flags` byte.
                unsafe { p.set_pin_count(new_pin_count) };
                if new_pin_count == 0 {
                    // SAFETY: `p` is in `self`'s page list in the `OBJECT` state and already in a
                    // page queue.
                    unsafe { self.move_to_not_pinned_locked(token, p, off) };
                    #[cfg(debug_assertions)]
                    {
                        // Check if the current range can be extended.
                        if completely_unpin_start + completely_unpin_len == off {
                            completely_unpin_len += ::page::SIZE as u64;
                        } else {
                            // Complete any existing range and then start again at this offset.
                            if completely_unpin_len > 0
                                && let Some(deferred) = deferred.as_mut()
                            {
                                let range_update =
                                    VmCowRange::new(completely_unpin_start, completely_unpin_len);
                                self.range_change_update_locked(
                                    token,
                                    range_update,
                                    RangeChangeOp::DebugUnpin,
                                    Some(deferred.as_mut()),
                                );
                            }
                            completely_unpin_start = off;
                            completely_unpin_len = ::page::SIZE as u64;
                        }
                    }
                }
                unpin_count += 1;
                Status::NEXT
            },
            |_gap_start, _gap_end| Status::NOT_FOUND,
        );
        assert!(status.is_ok(), "Tried to unpin an uncommitted page");

        // Possible that we were entirely inside a spare interval without any committed pages, in
        // which case neither the page nor gap callback would have triggered, and the assert above
        // would succeed. This is still an error though and can catch this, and any other mistakes,
        // by ensuring we found and decremented the pin counts from the exact expected number of
        // pages.
        assert!(
            unpin_count == end_page_offset.wrapping_sub(start_page_offset) / (::page::SIZE as u64)
        );

        #[cfg(debug_assertions)]
        // Check any leftover range.
        if completely_unpin_len > 0
            && let Some(deferred) = deferred
        {
            let range_update = VmCowRange::new(completely_unpin_start, completely_unpin_len);
            self.range_change_update_locked(
                token,
                range_update,
                RangeChangeOp::DebugUnpin,
                Some(deferred),
            );
        }

        let old_pinned_count = self.pinned_page_count_locked(token);
        let (new_pinned_count, overflow) = old_pinned_count.overflowing_sub(unpin_count);
        self.set_pinned_page_count_locked(token, new_pinned_count);
        assert!(!overflow);
    }

    /// Helper to check whether the requested range for `lock_range_locked` /
    /// `try_lock_range_locked` / `unlock_range_locked` is valid.
    #[inline]
    fn is_lock_range_valid_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> bool {
        range.offset == 0 && range.len == self.size_locked(token)
    }

    /// See `VmObject::lock_range`.
    pub fn lock_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        lock_state_out: Option<&mut MaybeUninit<zx_vmo_lock_state_t>>,
    ) -> Result<(), Status> {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        let discardable_tracker =
            self.discardable_tracker().expect("lock_range_locked requires discardable_tracker");

        if !self.is_lock_range_valid_locked(token, range) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        let Some(lock_state_out) = lock_state_out else {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        };

        // SAFETY: `discardable_tracker` belongs to `self` and `self`'s lock is held via `token`.
        let res = unsafe {
            discardable_tracker.lock_discardable_locked(token, /*try_lock=*/ false)
        };
        // Locking must succeed if try_lock was false.
        debug_assert!(res.is_ok());
        let ret = res?;

        // If the VMO just became unreclaimable as a result of this lock, refresh the page queue
        // state of all of its pages, which will move them out of any reclaimable queue.
        if ret.updated_reclaim_candidates {
            let _ = self.page_list_locked(token).for_every_page(|page_or_marker, offset| {
                if page_or_marker.is_page() {
                    let page = page_or_marker.page();
                    // SAFETY: `page` is in `self`'s page list (`OBJECT` state) while `self`'s lock
                    // is held via `token`, excluding all concurrent writers of the packed `flags`
                    // byte.
                    if unsafe { page.get_pin_count() } == 0 {
                        // SAFETY: `page` is owned by `self` (`OBJECT` state) and already in a page
                        // queue.
                        unsafe { self.move_to_not_pinned_locked(token, page, offset) };
                    }
                }
                Status::NEXT
            });
        }

        lock_state_out.write(zx_vmo_lock_state_t {
            offset: range.offset,
            size: range.len,
            discarded_offset: 0,
            discarded_size: if ret.was_discarded { self.size_locked(token) } else { 0 },
        });

        Ok(())
    }

    /// See `VmObject::try_lock_range`.
    pub fn try_lock_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> Result<(), Status> {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        let discardable_tracker =
            self.discardable_tracker().expect("try_lock_range_locked requires discardable_tracker");

        if !self.is_lock_range_valid_locked(token, range) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        // SAFETY: `discardable_tracker` belongs to `self` and `self`'s lock is held via `token`.
        let ret = unsafe {
            discardable_tracker.lock_discardable_locked(token, /*try_lock=*/ true)
        }?;

        // If the VMO just became unreclaimable as a result of this lock, refresh the page queue
        // state of all of its pages, which will move them out of any reclaimable queue.
        if ret.updated_reclaim_candidates {
            let _ = self.page_list_locked(token).for_every_page(|page_or_marker, offset| {
                if page_or_marker.is_page() {
                    let page = page_or_marker.page();
                    // SAFETY: `page` is in `self`'s page list (`OBJECT` state) while `self`'s lock
                    // is held via `token`, excluding all concurrent writers of the packed `flags`
                    // byte.
                    if unsafe { page.get_pin_count() } == 0 {
                        // SAFETY: `page` is owned by `self` (`OBJECT` state) and already in a page
                        // queue.
                        unsafe { self.move_to_not_pinned_locked(token, page, offset) };
                    }
                }
                Status::NEXT
            });
        }

        Ok(())
    }

    /// See `VmObject::unlock_range`.
    pub fn unlock_range_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
    ) -> Result<(), Status> {
        self.assert_canary();
        debug_assert!(self.lock_is_held());
        let discardable_tracker =
            self.discardable_tracker().expect("unlock_range_locked requires discardable_tracker");

        if !self.is_lock_range_valid_locked(token, range) {
            core::hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }

        // SAFETY: `discardable_tracker` belongs to `self` and `self`'s lock is held via `token`.
        let updated_reclaim_candidates =
            unsafe { discardable_tracker.unlock_discardable_locked(token) }?;

        // If the VMO just became reclaimable as a result of this unlock, refresh the page queue
        // state of all of its pages, which will move them into the reclaimable queue.
        if updated_reclaim_candidates {
            // SAFETY: `discardable_tracker` belongs to `self` and `self`'s lock is held via
            // `token`.
            debug_assert!(unsafe { discardable_tracker.is_eligible_for_reclamation_locked(token) });
            let _ = self.page_list_locked(token).for_every_page(|page_or_marker, offset| {
                if page_or_marker.is_page() {
                    let page = page_or_marker.page();
                    // SAFETY: `page` is in `self`'s page list (`OBJECT` state) while `self`'s lock
                    // is held via `token`, excluding all concurrent writers of the packed `flags`
                    // byte.
                    if unsafe { page.get_pin_count() } == 0 {
                        // SAFETY: `page` is owned by `self` (`OBJECT` state) and already in a page
                        // queue.
                        unsafe { self.move_to_not_pinned_locked(token, page, offset) };
                    }
                }
                Status::NEXT
            });
        }

        Ok(())
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
    ///
    /// `lookup_fn` is called while the page list of `self` is being traversed and must not modify
    /// it.
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
    ///
    /// `dirty_range_fn` is called while the page list of `self` is being traversed and must not
    /// modify it.
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
    fn page_list_locked_mut<'a>(
        &'a self,
        token: &'a mut LockToken<'_, VmCowPagesLockClass>,
    ) -> &'a mut VmPageList {
        // SAFETY: `token` proves the VmCowPages lock guarding `page_list` is held, and the
        // exclusive borrow of `token` for `'a` prevents any other token-derived access.
        unsafe { self.state().page_list.get_mut(token) }
    }

    /// Parent of this node, if any.
    #[inline]
    fn parent_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
    ) -> Option<&'a VmCowPages> {
        // SAFETY: `token` proves the VmCowPages lock guarding `parent` is held.
        unsafe { self.state().parent.get(token) }.as_deref()
    }

    #[inline]
    fn parent_ref_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmCowPagesLockClass>,
    ) -> Option<&'a RefPtr<VmCowPages>> {
        // SAFETY: `token` proves the VmCowPages lock guarding `parent` is held.
        unsafe { self.state().parent.get(token) }.as_ref()
    }

    #[inline]
    fn has_parent_locked(&self, token: &LockToken<'_, VmCowPagesLockClass>) -> bool {
        // SAFETY: `token` proves the VmCowPages lock guarding `parent` is held.
        unsafe { self.state().parent.get(token) }.is_some()
    }

    /// Unlinks and returns the parent of this node, if any.
    ///
    /// # Safety
    ///
    /// A `LockedPtr`, in Rust or in C++, keeps no reference to the node whose lock it holds, and
    /// relies on that node not being destroyed while its lock is held. Unlinking therefore
    /// follows the protocol of the C++ callers: the caller must hold the parent's lock as well as
    /// `self`'s, so that no other thread is using the parent through a `LockedPtr`, and must not
    /// drop the returned `RefPtr`, if that could release the last reference to the parent, until
    /// this thread has released the parent's lock.
    #[inline]
    unsafe fn take_parent_locked(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) -> Option<RefPtr<VmCowPages>> {
        // SAFETY: `token` proves the VmCowPages lock guarding `parent` is held exclusively.
        unsafe { self.state().parent.get_mut(token) }.take()
    }

    /// Replaces the parent of this node, dropping the reference to the previous parent, if any.
    ///
    /// # Safety
    ///
    /// As for [`Self::take_parent_locked`]: if there is a previous parent, the caller must hold
    /// its lock, and this call must not release the last reference to it.
    #[inline]
    unsafe fn set_parent_locked(
        &self,
        parent: Option<RefPtr<VmCowPages>>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: `token` proves the VmCowPages lock guarding `parent` is held exclusively.
        let old = mem::replace(unsafe { self.state().parent.get_mut(token) }, parent);
        // Releasing the last reference runs `~VmCowPages`, so only drop the previous parent once
        // the borrow of the `parent` cell has ended.
        drop(old);
    }

    /// Returns an initializer that acquires the lock of `self`'s parent (if any) into a
    /// `LockedPtr`. The parent's lock is not acquired until the initializer is run. The
    /// initializer borrows `token`, so it can only be run while `self`'s lock is still held.
    ///
    /// The initialized `LockedPtr` is not tied to the borrow of `self`: the parent stays alive for
    /// as long as its lock is held (see the `LockedPtr` safety invariant), independent of `self`,
    /// so `'a` is chosen by the caller.
    ///
    /// # Safety
    ///
    /// The initialized `LockedPtr` keeps no reference to the parent. While it (or any `LockedPtr`
    /// the lock is moved into) holds the parent's lock, the caller must not release the last
    /// reference to the parent, for example with [`Self::set_parent_locked`] or by dropping the
    /// result of [`Self::take_parent_locked`]. When the initializer runs, the acquisition
    /// preconditions listed on [`LockedPtr::acquire_raw`] must hold.
    #[inline]
    unsafe fn parent_lock_init_locked<'a, 't>(
        &'t self,
        token: &'t LockToken<'_, VmCowPagesLockClass>,
    ) -> Option<impl PinInit<LockedPtr<'a>> + use<'a, 't>> {
        let parent_ref = self.parent_ref_locked(token)?;
        // Take the pointer stored in the `RefPtr` rather than going through a `&VmCowPages`, so
        // that the `LockedPtr` does not derive its access from a shared reference.
        // SAFETY: A `RefPtr` is never null.
        let parent_ptr = unsafe { NonNull::new_unchecked(RefPtr::as_ptr(parent_ref).cast_mut()) };
        let order = parent_ref.lock_order();
        // SAFETY: The initializer borrows `self` and `token` (`'t`), so `self` is live and its
        // lock is held from here until the initializer has run, keeping `parent_ref`
        // (`RefPtr<VmCowPages>`) alive during lock acquisition.
        // Once `parent`'s lock is acquired, `parent` cannot undergo `DeadTransitionLocked` or lose
        // its last child `RefPtr` until `parent`'s lock is released (`DeadTransitionLocked`,
        // `MergeContentWithChildLocked`, and `detach_source` all either require `parent`'s lock or
        // transfer the `RefPtr` into a locked child without dropping the last reference, and
        // per `# Safety` the caller does not drop it either), so `parent_ptr` remains valid for
        // at least as long as the `LockedPtr` holds `parent`'s lock.
        // The acquisition preconditions are the caller's, per `# Safety`. The lock order is
        // respected because a parent always has a higher lock order than its children, and
        // `self`'s lock, a child's, is the one `token` shows is held.
        Some(unsafe { LockedPtr::acquire_raw(parent_ptr, order) })
    }

    #[inline]
    fn page_list_locked_ptr(
        &self,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) -> *mut VmPageList {
        // SAFETY: The VmCowPages lock is held.
        unsafe { self.state().page_list.as_mut_ptr(token) }
    }

    /// Like [`Self::page_list_locked_ptr`], but for callers that only hold a shared `token`:
    /// `find_page_content_locked` reaches each node through a shared token (its own, or an
    /// ancestor's from `LockedPtr::locked_or`), and still needs a mutable page list cursor. The
    /// caller's exclusivity obligation below replaces the `&mut LockToken` that
    /// `page_list_locked_ptr` and `page_list_locked_mut` require.
    ///
    /// # Safety
    ///
    /// The lock proven by `token` must stay held for as long as the returned pointer or anything
    /// derived from it is used. Because the shared `token` does not make mutation exclusive, for
    /// as long as a cursor or reference derived from the pointer is used the caller must also
    /// ensure that:
    ///
    /// * From creating a derived cursor until its last use, no node is inserted or removed, and
    ///   no slot is written while a derived cursor or shared reference is reading it, other than
    ///   through that cursor.
    /// * Nothing else accesses a node while a mutable reference to it derived from the pointer is
    ///   live.
    ///
    /// Reading the same list through another accessor (for example
    /// `page_list_locked(token).for_every_page_in_range(..)`) is therefore fine while a derived
    /// cursor is held but not being used, and the `KCell` scalar accessors (`parent_limit_locked`,
    /// `parent_offset_locked`, ...) never touch the page list.
    #[inline]
    unsafe fn page_list_locked_ptr_unchecked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> *mut VmPageList {
        // SAFETY: `token` proves the lock is held; exclusivity is the caller's obligation above.
        unsafe { self.state().page_list.as_ptr(token).cast_mut() }
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
    fn continuous_attribution_tracker_locked_mut<'a>(
        &'a self,
        token: &'a mut LockToken<'_, VmCowPagesLockClass>,
    ) -> &'a mut AttributionTracker {
        // SAFETY: `token` proves the VmCowPages lock guarding `continuous_attribution_tracker` is
        // held, and the exclusive borrow of `token` for `'a` prevents any other token-derived
        // access.
        unsafe { self.state().continuous_attribution_tracker.get_mut(token) }
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

    /// Frees `page`, which was allocated for an insertion into this VMO that then failed. Out of
    /// line so that this error path does not grow the functions that roll back.
    ///
    /// # Safety
    ///
    /// `page` must be a valid allocated page owned by this `VmCowPages` that is not in any page
    /// queue or list.
    #[cold]
    #[inline(never)]
    unsafe fn rollback_allocated_page(&self, page: VmPagePtr) {
        // SAFETY: Per `# Safety` `page` is a valid allocated page owned by this `VmCowPages` that
        // is not in any page queue or list, which is the contract of `free_page`.
        unsafe { self.free_page(page) }
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
    ///
    /// # Safety-usable invariant
    ///
    /// On `Ok`, `page_or_mark` holds a page in place of the reference.
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
                arch_zero_page(paddr_to_physmap(dst_paddr));
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

/// Zeroes the physical page at `pa` via the physmap.
///
/// # Safety
///
/// `pa` must be the page-aligned physical address of a valid writable physical page in the
/// physmap.
#[inline]
unsafe fn zero_page_paddr(pa: PAddr) {
    let vaddr = paddr_to_physmap(pa);
    debug_assert_ne!(vaddr.0, 0);
    // SAFETY: Caller guarantees `pa` maps to a valid writable page in the physmap.
    unsafe { arch_zero_page(vaddr) };
}

/// Zeroes the physical page backing `p` via the physmap.
///
/// # Safety
///
/// `p` must point to a valid `VmPage` whose physical page is writable in the physmap, and the
/// caller must have exclusive access to that page's memory for the duration of the call.
#[inline]
unsafe fn zero_page(p: VmPagePtr) {
    // SAFETY: Caller guarantees `p` is a valid `VmPage` with an exclusively writable physmap page.
    unsafe { zero_page_paddr(p.as_ref().paddr()) };
}

/// # Safety
///
/// `p` must point to a valid `VmPage` whose physical page is readable in the physmap, and no other
/// CPU, thread, or DMA engine may write to the page during the call (e.g. the page is newly
/// allocated and not yet mapped).
#[inline]
unsafe fn is_zero_page(p: VmPagePtr) -> bool {
    // SAFETY: Caller guarantees `p` is a valid `VmPage` in the physmap.
    let base = paddr_to_physmap_ptr::<u64>(unsafe { p.as_ref() }.paddr());
    let count = ::page::SIZE / core::mem::size_of::<u64>();
    // Form the slice once so the pointer validity checks are hoisted out of the loop (this runs
    // under `debug_assert!`, where raw-pointer `ub_checks` would otherwise be emitted per word).
    // SAFETY: Caller guarantees `p` is a valid `VmPage` in the physmap, so `base` points to a
    // `PAGE_SIZE`-byte page aligned to at least 8 bytes with no concurrent writes for the
    // duration of this call.
    let words = unsafe { core::slice::from_raw_parts(base, count) };
    words.iter().all(|&w| w == 0)
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

/// Frees the compressed reference `content` without decompressing it.
///
/// # Safety
///
/// `content` must be a valid compressed reference belonging to `pmm::node()`'s `VmCompression`
/// instance, owned by the caller and not used again; if `content` is the temporary reference, the
/// lock of the VMO that owns the temporary reference must be held.
#[inline]
unsafe fn free_reference(content: ReferenceValue) {
    // `ReferenceValue::new` is safe, so holding one does not by itself prove a compressor exists;
    // the `# Safety` contract (a live reference from `pmm::node()`'s compressor) does.
    let compression = pmm::node().get_page_compression().expect("compression must be present");
    // SAFETY: Per `# Safety`, `content` is a valid reference belonging to this `VmCompression`,
    // owned by the caller and not used again, and the owning VMO's lock is held if it is the
    // temporary reference.
    unsafe { compression.free(content) };
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
/// * `page_source` must be either null or an owned raw `PageSource` pointer exported via
///   `fbl::ExportToRawPtr`, whose reference is transferred to the initialized state.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_state_init(
    storage: &mut MaybeUninit<VmCowPagesState>,
    pmm_alloc_flags: u32,
    options: u32,
    size: u64,
    page_source: *mut page_source_bindings::PageSource,
    #[cfg_attr(not(with_lock_dep), allow(unused_variables))] lock_order: u64,
) {
    let options = VmCowPagesOptions::from_bits_retain(options);
    // SAFETY: Caller guarantees `page_source` is null or an exported `RefPtr<PageSource>`.
    let page_source = unsafe { RefPtr::try_from_raw(page_source.cast::<PageSource>()) };
    debug_assert!(::page::is_page_rounded(size as usize));
    // If we are tracking correct lock orders then add some asserts that nodes are created with lock
    // orders that at least vaguely make sense.
    #[cfg(with_lock_dep)]
    {
        let is_hidden = options.contains(VmCowPagesOptions::HIDDEN);
        // Nodes with a page source must always be the root, and have the respective lock order.
        debug_assert!(page_source.is_none() || lock_order == VmCowPages::LOCK_ORDER_ROOT);
        // Hidden nodes must always have a lock order above the anonymous numbering area.
        debug_assert!(!is_hidden || lock_order > VmCowPages::LOCK_ORDER_FIRST_ANON);
        // First anonymous nodes (i.e. not hidden and not with a direct page source) should fall
        // into the anonymous numbering area.
        debug_assert!(
            page_source.is_some() || is_hidden || lock_order <= VmCowPages::LOCK_ORDER_FIRST_ANON
        );
    }
    storage.write(VmCowPagesState {
        canary: Canary::default(),
        pmm_alloc_flags,
        options,
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
        #[cfg(with_lock_dep)]
        lock_order,
        parent: KCell::new(None),
        page_source,
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
/// * `lookup_fn` must point to a valid `VmObject::LookupFunction` for the duration of the call,
///   and must not modify the page list of `this`.
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
///   the call, and must not modify the page list of `this`.
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

/// `slot` is received as the raw slot pointer (the C++ `VmPageOrMarkerRef` is a trivially copyable
/// pointer wrapper with the same ABI) rather than as `VmPageOrMarkerRef<'a>`, because C++ callers
/// such as `DedupZeroPage` keep reading the slot through their own copy of the pointer while the
/// returned transaction is live, which a by-value `&'a mut` argument would forbid.
///
/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `slot` must point to the valid `VmPageOrMarker` slot at `offset` in `this.page_list`,
///   `offset` must not fall in the middle of an interval, and `*slot` must not be mutated before
///   the returned transaction is completed or cancelled.
/// * `overwrite` must be a valid `CanOverwriteSlot` discriminant.
/// * `out_transaction` must point to valid uninitialized storage for `AddPageTransaction`. It is
///   always initialized on return (to an inert transaction on failure) because the C++
///   `AddPageTransaction` destructor runs on it either way.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_begin_add_page_with_slot_locked<'a>(
    this: &'a VmCowPages,
    offset: u64,
    slot: NonNull<VmPageOrMarker>,
    overwrite: CanOverwriteSlot,
    out_transaction: &mut MaybeUninit<AddPageTransaction<'a>>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    // SAFETY: The C++ caller upholds `begin_add_page_with_slot_locked` preconditions.
    match unsafe { this.begin_add_page_with_slot_locked(&mut token, offset, slot, overwrite) } {
        Ok(tx) => {
            out_transaction.write(tx);
            Ok(())
        }
        Err(status) => {
            core::hint::cold_path();
            out_transaction.write(AddPageTransaction::default());
            Err(status)
        }
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `overwrite` must be a valid `CanOverwriteSlot` discriminant.
/// * `out_transaction` must point to valid uninitialized storage for `AddPageTransaction`. It is
///   always initialized on return (to an inert transaction on failure) because the C++
///   `AddPageTransaction` destructor runs on it either way.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_begin_add_page_locked<'a>(
    this: &'a VmCowPages,
    offset: u64,
    overwrite: CanOverwriteSlot,
    out_transaction: &mut MaybeUninit<AddPageTransaction<'a>>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    match this.begin_add_page_locked(&mut token, offset, overwrite) {
        Ok(tx) => {
            out_transaction.write(tx);
            Ok(())
        }
        Err(status) => {
            core::hint::cold_path();
            out_transaction.write(AddPageTransaction::default());
            Err(status)
        }
    }
}

/// Move constructor for the C++ `AddPageTransaction` shell: moves the transaction out of `src`
/// into `dst`, leaving `src` inert so its destructor is a no-op.
///
/// # Safety
///
/// * `dst` must point to valid uninitialized storage for `AddPageTransaction`.
/// * `src` must point to a valid `AddPageTransaction`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_add_page_transaction_move<'a>(
    dst: &mut MaybeUninit<AddPageTransaction<'a>>,
    src: &mut AddPageTransaction<'a>,
) {
    dst.write(AddPageTransaction {
        slot: src.slot.take(),
        offset: src.offset,
        phantom: PhantomData,
    });
}

/// Destructor for the C++ `AddPageTransaction` shell.
///
/// # Safety
///
/// `transaction` must be a properly aligned pointer to a valid `AddPageTransaction` that is not
/// used again.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_add_page_transaction_destroy(
    transaction: *mut AddPageTransaction<'_>,
) {
    // SAFETY: Per `# Safety`, `transaction` is valid and C++ never uses it again.
    unsafe { ptr::drop_in_place(transaction) };
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `transaction` must point to a valid active `AddPageTransaction` for `this` with no
///   intervening mutating page-list operation since it began.
/// * `p` must point to a valid non-empty `VmPageOrMarker` whose ownership is transferred and
///   that satisfies [`VmCowPages::complete_add_page_locked`]'s page/reference ownership
///   preconditions.
/// * `deferred` must be null or point to an initialized `DeferredOps` constructed for `this`,
///   exclusively accessed for the duration of the call, and pinned in memory until
///   `rust_vm_cow_pages_deferred_ops_destroy` runs on it.
/// * `out_old` must point to an empty `VmPageOrMarker` to receive the previous slot contents.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_complete_add_page_locked(
    this: &VmCowPages,
    transaction: &mut AddPageTransaction<'_>,
    p: &mut VmPageOrMarker,
    deferred: Option<Pin<&mut DeferredOps<'_>>>,
    out_old: &mut VmPageOrMarker,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    let p = mem::replace(p, VmPageOrMarker::empty());
    // SAFETY: The C++ caller upholds `complete_add_page_locked` preconditions.
    *out_old = unsafe { this.complete_add_page_locked(&mut token, transaction, p, deferred) };
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `page` must satisfy [`VmCowPages::add_new_page_locked`]'s preconditions: a valid non-null
///   `VmPage` in the `ALLOC` state, exclusively owned, not in any page queue or container, with
///   the physmap access `zero` requires.
/// * `overwrite` must be a valid `CanOverwriteSlot` discriminant.
/// * `released_page` must be null or point to an initialized `VmPageOrMarker` not holding a page
///   or reference, with exclusive access for the duration of the call.
/// * `deferred` must be null or point to an initialized `DeferredOps` constructed for `this`,
///   exclusively accessed for the duration of the call, and pinned in memory until
///   `rust_vm_cow_pages_deferred_ops_destroy` runs on it.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_add_new_page_locked(
    this: &VmCowPages,
    offset: u64,
    page: VmPagePtr,
    overwrite: CanOverwriteSlot,
    released_page: Option<&mut VmPageOrMarker>,
    zero: bool,
    deferred: Option<Pin<&mut DeferredOps<'_>>>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    // SAFETY: Caller upholds `add_new_page_locked` safety preconditions.
    let old =
        unsafe { this.add_new_page_locked(&mut token, offset, page, overwrite, zero, deferred)? };
    // The C++ caller may pass a null `released_page` only when `overwrite` cannot release a page.
    if let Some(released_page) = released_page {
        *released_page = old;
    } else {
        debug_assert!(!old.is_page_or_ref());
    }
    Ok(())
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `pages` must point to a valid pinned `VmPageDoublyLinkedList`, exclusively accessed for the
///   duration of the call, whose pages satisfy [`VmCowPages::add_new_pages_locked`]'s
///   preconditions (`ALLOC` state, exclusively owned, not in any page queue or container, with the
///   physmap access `zero` requires).
/// * `overwrite` must be a valid `CanOverwriteSlot` discriminant.
/// * `deferred` must be null or point to an initialized `DeferredOps` constructed for `this`,
///   exclusively accessed for the duration of the call, and pinned in memory until
///   `rust_vm_cow_pages_deferred_ops_destroy` runs on it.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_add_new_pages_locked(
    this: &VmCowPages,
    start_offset: u64,
    pages: Pin<&mut VmPageDoublyLinkedList>,
    overwrite: CanOverwriteSlot,
    zero: bool,
    deferred: Option<Pin<&mut DeferredOps<'_>>>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    // SAFETY: Caller upholds `add_new_pages_locked` safety preconditions.
    unsafe { this.add_new_pages_locked(&mut token, start_offset, pages, overwrite, zero, deferred) }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and `this.lock()` must not be held.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_decommit_range(
    this: &VmCowPages,
    range: VmCowRange,
) -> Result<(), Status> {
    this.decommit_range(range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * This acquires the locks of ancestors of `this`, so the caller must not already hold the lock
///   of any ancestor and must be in a blockable thread context.
/// * `out_counts` must be valid for writing an `AttributionCounts`, and must not be accessed
///   through any other pointer during the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_get_attributed_memory_in_range_locked(
    this: &VmCowPages,
    range: VmCowRange,
    out_counts: &mut MaybeUninit<AttributionCounts>,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    out_counts.write(this.get_attributed_memory_in_range_locked(&token, range));
}

/// Returns 1 if the page would read as zero and 0 otherwise.
///
/// TODO(https://github.com/rust-lang/rust/issues/163911): This returns `u8` rather than `bool`
/// because rustc currently omits `zeroext` on `extern "C"` bool returns on x86-64, so a `bool`
/// computed from a flag test can reach C++ with bits 1..7 of `%al` set and read as true.
///
/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * This acquires the locks of ancestors of `this`, so the caller must not already hold the lock
///   of any ancestor and must be in a blockable thread context.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_page_would_read_zero_locked(
    this: &VmCowPages,
    page_offset: u64,
) -> u8 {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    u8::from(this.page_would_read_zero_locked(&token, page_offset))
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * This acquires the locks of ancestors of `this`, so the caller must not already hold the lock
///   of any ancestor and must be in a blockable thread context.
/// * `lookup_fn` must point to a valid `VmCowPages::LookupReadableFunction` for the duration of
///   the call. It is called with the lock of `this`, and possibly of an ancestor, held, and must
///   neither release those locks nor modify the page list of `this` or of any of its ancestors.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_readable_locked(
    this: &VmCowPages,
    range: VmCowRange,
    lookup_fn: *mut c_void,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    this.lookup_readable_locked(&token, range, move |offset, pa| {
        // SAFETY: `lookup_fn` is a valid `LookupReadableFunction` for the duration of this call.
        let raw = unsafe {
            bindings::cpp_vm_cow_pages_call_lookup_fn(lookup_fn, offset, pa.0 as zx_paddr_t)
        };
        // `ZX_OK` is not representable as a `Status`; the only C++ caller
        // (`VmObjectPaged::CacheOp`) returns `ZX_ERR_NEXT` to continue, so map `ZX_OK` to
        // `Status::STOP`. All other codes map 1:1.
        Status::try_from_raw(raw).unwrap_or(Status::STOP)
    })
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `page_request` must point to a valid `LazyPageRequest` that no other code accesses until
///   this call either returns or queues its inner `PageRequest` with the page source (which it
///   does when returning `ZX_ERR_SHOULD_WAIT`), after which the page source may access it from
///   other threads. The `LazyPageRequest` must not be moved until it is destroyed.
/// * `dirty_len_out` must be non-null, properly aligned, and valid for writing a `u64`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_prepare_for_write_locked(
    this: &VmCowPages,
    range: VmCowRange,
    page_request: Pin<&LazyPageRequest>,
    dirty_len_out: &mut MaybeUninit<u64>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    match this.prepare_for_write_locked(&token, range, page_request) {
        Ok(dirty_len) => {
            dirty_len_out.write(dirty_len);
            Ok(())
        }
        Err(status) => {
            dirty_len_out.write(0);
            Err(status)
        }
    }
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_fail_page_requests_locked(
    this: &VmCowPages,
    range: VmCowRange,
    error_status: zx_status_t,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let token = unsafe { LockToken::new() };
    // `ZX_OK` cannot be represented as a `Status`; `err_from_raw` maps it to `INTERNAL`, which,
    // like `ZX_OK`, `is_valid_internal_failure_code` rejects after the same precondition checks.
    this.fail_page_requests_locked(&token, range, Status::err_from_raw(error_status))
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_writeback_begin_locked(
    this: &VmCowPages,
    range: VmCowRange,
    is_zero_range: bool,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.writeback_begin_locked(&mut token, range, is_zero_range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_writeback_end_locked(
    this: &VmCowPages,
    range: VmCowRange,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.writeback_end_locked(&mut token, range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and `this.lock()` must not be held.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_detach_source(this: &VmCowPages) {
    this.detach_source();
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `deferred` must be a `DeferredOps` for `this`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_unmap_and_free_pages_locked(
    this: &VmCowPages,
    offset: u64,
    len: u64,
    deferred: Pin<&mut DeferredOps<'_>>,
    out_freed_count: &mut u64,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.unmap_and_free_pages_locked(&mut token, offset, len, deferred).map(|count| {
        *out_freed_count = count;
    })
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_pin_range_locked(
    this: &VmCowPages,
    range: VmCowRange,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.pin_range_locked(&mut token, range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
/// * `deferred` must be null or point to an initialized `DeferredOps` constructed for `this`,
///   exclusively accessed for the duration of the call, and pinned in memory until
///   `rust_vm_cow_pages_deferred_ops_destroy` runs on it.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_unpin_locked(
    this: &VmCowPages,
    range: VmCowRange,
    deferred: Option<Pin<&mut DeferredOps<'_>>>,
) {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.unpin_locked(&mut token, range, deferred);
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lock_range_locked(
    this: &VmCowPages,
    range: VmCowRange,
    lock_state_out: Option<&mut MaybeUninit<zx_vmo_lock_state_t>>,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.lock_range_locked(&mut token, range, lock_state_out)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_try_lock_range_locked(
    this: &VmCowPages,
    range: VmCowRange,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.try_lock_range_locked(&mut token, range)
}

/// # Safety
///
/// * `this` must point to a valid `VmCowPages`, and the caller must hold `this.lock()`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_unlock_range_locked(
    this: &VmCowPages,
    range: VmCowRange,
) -> Result<(), Status> {
    // SAFETY: The C++ caller holds `this.lock()` (TA_REQ).
    let mut token = unsafe { LockToken::new() };
    this.unlock_range_locked(&mut token, range)
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lock_order(cow: &VmCowPages) -> u64 {
    cow.lock_order()
}

/// Returns a borrowed pointer to the parent (null if there is none). No reference is transferred:
/// the pointer is valid for as long as the caller continues to hold `cow.lock()`.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` with no
///   conflicting Rust borrow of the state it guards live.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_parent_locked(
    cow: &VmCowPages,
) -> *mut bindings::VmCowPages {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let token = unsafe { LockToken::new() };
    // Hand out the pointer stored in the `RefPtr` rather than one derived from a `&VmCowPages`,
    // as C++ mutates through it.
    cow.parent_ref_locked(&token)
        .map_or(ptr::null_mut(), |parent| RefPtr::as_ptr(parent).cast_mut().cast())
}

/// Returns an owned pointer to the previous parent (null if there was none), which the caller
/// must adopt with `fbl::ImportFromRawPtr`.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` exclusively
///   with no conflicting Rust borrow of the state it guards live.
/// * The caller must uphold the contract of [`VmCowPages::take_parent_locked`] for the returned
///   reference.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_take_parent_locked(
    cow: &VmCowPages,
) -> *mut bindings::VmCowPages {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: Per `# Safety`, the caller upholds the contract for the returned reference.
    match unsafe { cow.take_parent_locked(&mut token) } {
        Some(parent) => RefPtr::into_raw(parent).cast_mut().cast::<bindings::VmCowPages>(),
        None => ptr::null_mut(),
    }
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` exclusively
///   with no conflicting Rust borrow of the state it guards live.
/// * `parent` must be either null or an owned raw `VmCowPages` pointer exported via
///   `fbl::ExportToRawPtr`, whose reference is transferred to `cow`.
/// * The caller must uphold the contract of [`VmCowPages::set_parent_locked`] for the previous
///   parent, whose reference is dropped.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_set_parent_locked(
    cow: &VmCowPages,
    parent: *mut bindings::VmCowPages,
) {
    // SAFETY: Caller guarantees `parent` is null or an exported `RefPtr<VmCowPages>`.
    let parent = unsafe { VmCowPages::from_raw(parent) };
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: Per `# Safety`, the caller upholds the contract for the previous parent.
    unsafe { cow.set_parent_locked(parent, &mut token) };
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` with no
///   conflicting Rust borrow of the state it guards live.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_is_parent_hidden_locked(cow: &VmCowPages) -> bool {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let token = unsafe { LockToken::new() };
    cow.is_parent_hidden_locked(&token)
}

/// Returns a borrowed pointer to the page source (null if there is none). No reference is
/// transferred: the pointer is valid for the lifetime of `cow`.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_page_source(
    cow: &VmCowPages,
) -> *mut page_source_bindings::PageSource {
    // Hand out the pointer stored in the `RefPtr` rather than one derived from a `&PageSource`,
    // as C++ mutates through it.
    cow.state()
        .page_source
        .as_ref()
        .map_or(ptr::null_mut(), |source| RefPtr::as_ptr(source).cast_mut().cast())
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_page_source_type(
    cow: &VmCowPages,
) -> bindings::PageSourceType {
    cow.page_source_type().into()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_can_evict(cow: &VmCowPages) -> bool {
    cow.can_evict()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_can_borrow(cow: &VmCowPages) -> bool {
    cow.can_borrow()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_is_dirty_tracked(cow: &VmCowPages) -> bool {
    cow.is_dirty_tracked()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` exclusively
///   with no conflicting Rust borrow of the state it guards live.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_mark_modified_locked(cow: &VmCowPages) {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    cow.mark_modified_locked(&mut token);
}

/// Returns an owned pointer to the parent (null if there is none), which the caller must adopt
/// with `fbl::ImportFromRawPtr`.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
/// * This acquires `cow.lock()`, so the caller must not hold it and must be in a blockable thread
///   context, with no spinlocks held and no active `ChainLockTransaction`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_get_parent(
    cow: &VmCowPages,
) -> *mut bindings::VmCowPages {
    match cow.debug_get_parent() {
        Some(parent) => RefPtr::into_raw(parent).cast_mut().cast::<bindings::VmCowPages>(),
        None => ptr::null_mut(),
    }
}

/// Returns `ZX_KOID_INVALID` if `cow` has no page source, or its page provider has no koid.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_get_page_source_koid(cow: &VmCowPages) -> zx_koid_t {
    cow.get_page_source_koid().unwrap_or(ZX_KOID_INVALID)
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` with no
///   conflicting Rust borrow of the state it guards live.
/// * This acquires the lock of each ancestor of `cow` in turn, so the caller must not hold any of
///   them and must be in a blockable thread context, with no spinlocks held and no active
///   `ChainLockTransaction`. Under lockdep, the caller must not hold any `VmCowPages` lock whose
///   order is at or above the `lock_order()` of `cow`'s parent.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_debug_lookup_depth_locked(cow: &VmCowPages) -> u32 {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let token = unsafe { LockToken::new() };
    cow.debug_lookup_depth_locked(&token)
}

/// Initializes `cursor` as a `LookupCursor` over `[offset, offset + len)` of `target`, which must
/// be a page aligned range within `target` that is not empty.
///
/// # Safety
///
/// * `cursor` must point to valid, aligned, uninitialized storage for a `LookupCursor`. The
///   storage must not be moved, copied, or reused until
///   `rust_vm_cow_pages_lookup_cursor_destroy` has returned.
/// * `target` must point to a valid `VmCowPages` that outlives the cursor, and the caller must
///   hold `target.lock()` from this call until the cursor has been destroyed.
/// * If anything other than the cursor changes the page lists, parent links, parent limits or sizes
///   of `target`, or of the owner the cursor has locked, after the cursor has been used, the cursor
///   must not be used again other than to destroy it.
/// * `target` must be `LifeCycle::Alive` and must not be a hidden node.
/// * A function that takes the cursor may acquire the lock of each ancestor of `target` in turn,
///   so whenever one is called the caller must not hold any of them and must be in a blockable
///   thread context, with no spinlocks held and no active `ChainLockTransaction`. Under lockdep,
///   every `VmCowPages` lock the caller holds must have a lock order below that of each of them.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_init<'a>(
    cursor: &mut MaybeUninit<LookupCursor<'a>>,
    target: &'a VmCowPages,
    offset: u64,
    len: u64,
) {
    // The range is passed as two scalars and not as a `VmCowRange` because Clang and rustc lower a
    // by-value `VmCowRange` to different LLVM types on x86-64, stopping cross-language inlining.
    // TODO(https://fxbug.dev/537458631): Take a `VmCowRange` once cross-language inlining works.
    let range = VmCowRange::new(offset, len);
    // SAFETY: Two operations. `LookupCursor::new`: per the second bullet of `# Safety` the lock of
    // `target` is held for as long as the cursor exists, which is what `'a` stands for here, and
    // the third, fourth and fifth bullets are the other three clauses of its contract.
    // `__pinned_init`: per the first bullet `cursor` is valid uninitialized storage at a pinned
    // address.
    unsafe {
        let Ok(()) = LookupCursor::new(target, range).__pinned_init(cursor.as_mut_ptr());
    }
}

/// # Safety
///
/// `cursor` must point to an initialized `LookupCursor` that will not be accessed again, and the
/// lock of its target must still be held. This releases the lock of an ancestor of the target if
/// the cursor holds one, so the caller must be the thread that acquired it, in a blockable thread
/// context with no active `ChainLockTransaction`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_destroy(cursor: *mut LookupCursor<'_>) {
    // SAFETY: The caller guarantees `cursor` is initialized and not used afterwards.
    unsafe {
        ptr::drop_in_place(cursor);
    }
}

/// # Safety
///
/// `cursor` must point to a live, initialized `LookupCursor`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_target(
    cursor: &LookupCursor<'_>,
) -> *mut bindings::VmCowPages {
    cursor.target.as_raw()
}

/// Flattens the result of a `LookupCursor::require_*` method into the form that the C++
/// `LookupCursor` wrapper receives in two registers.
#[inline]
fn require_result_into_ffi(
    result: Result<RequireResult, Status>,
) -> bindings::VmCowPagesLookupCursorRequireResult {
    match result {
        Ok(result) => bindings::VmCowPagesLookupCursorRequireResult {
            page: result.page.as_ffi().expose_provenance(),
            status: Status::result_into_raw(Ok(())),
            writable: result.writable,
            __bindgen_padding_0: [0; 3],
        },
        Err(status) => bindings::VmCowPagesLookupCursorRequireResult {
            page: 0,
            status: status.into_raw(),
            writable: false,
            __bindgen_padding_0: [0; 3],
        },
    }
}

/// `max_request_pages` must be non-zero and at most the number of pages left in the range of the
/// cursor, and `page_request` must not hold an active request.
///
/// # Safety
///
/// * `cursor` must point to a live, initialized, pinned `LookupCursor` that upholds the contract
///   of `rust_vm_cow_pages_lookup_cursor_init`.
/// * `deferred` must point to a live, pinned `DeferredOps` that was constructed for the target of
///   the cursor.
/// * `page_request` must point to a live, pinned `MultiPageRequest`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_require_owned_page(
    cursor: Pin<&mut LookupCursor<'_>>,
    will_write: bool,
    max_request_pages: u64,
    deferred: Pin<&mut DeferredOps<'_>>,
    page_request: Pin<&MultiPageRequest>,
) -> bindings::VmCowPagesLookupCursorRequireResult {
    // SAFETY: The C++ caller upholds the contract of `require_owned_page`.
    require_result_into_ffi(unsafe {
        cursor.require_owned_page(will_write, max_request_pages, deferred, page_request)
    })
}

/// `max_request_pages` and `page_request` must be as for
/// `rust_vm_cow_pages_lookup_cursor_require_owned_page`.
///
/// # Safety
///
/// Same as `rust_vm_cow_pages_lookup_cursor_require_owned_page`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_require_page(
    cursor: Pin<&mut LookupCursor<'_>>,
    will_write: bool,
    max_request_pages: u64,
    deferred: Pin<&mut DeferredOps<'_>>,
    page_request: Pin<&MultiPageRequest>,
) -> bindings::VmCowPagesLookupCursorRequireResult {
    // SAFETY: The C++ caller upholds the contract of `require_page`.
    require_result_into_ffi(unsafe {
        cursor.require_page(will_write, max_request_pages, deferred, page_request)
    })
}

/// # Safety
///
/// `cursor` must point to a live, initialized, pinned `LookupCursor` that upholds the contract of
/// `rust_vm_cow_pages_lookup_cursor_init`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_maybe_page(
    cursor: Pin<&mut LookupCursor<'_>>,
    will_write: bool,
) -> Option<VmPagePtr> {
    cursor.maybe_page(will_write)
}

/// # Safety
///
/// `cursor` must point to a live, initialized, pinned `LookupCursor`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_disable_zero_fork(
    cursor: Pin<&mut LookupCursor<'_>>,
) {
    cursor.disable_zero_fork()
}

/// # Safety
///
/// `cursor` must point to a live, initialized, pinned `LookupCursor`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_lookup_cursor_disable_mark_accessed(
    cursor: Pin<&mut LookupCursor<'_>>,
) {
    cursor.disable_mark_accessed()
}

/// Releases any pages `cow` can reference in `[offset, offset + len)`, placing freed pages into
/// `freed_list`.
///
/// # Safety
///
/// * `[offset, offset + len)` must be page aligned and within the size of `cow`.
/// * `cow` must point to a valid `VmCowPages` that is not a hidden node, and the caller must hold
///   `cow.lock()` exclusively with no conflicting Rust borrow of the state it guards live.
/// * If `parent` is `Some`, it must point to `cow`'s immediate parent, and the caller must hold
///   `parent.lock()` for the duration of this call with no conflicting Rust borrow of the state it
///   guards live.
/// * This acquires the lock of each ancestor of `cow` that it walks into, other than `parent`, so
///   the caller must not hold any of them and must be in a blockable thread context, with no
///   spinlocks held and no active `ChainLockTransaction`. Under lockdep, every `VmCowPages` lock
///   the caller holds must have a lock order below that of each ancestor this acquires.
/// * `freed_list` must point to a valid, pinned `ScopedPageFreedList` that will only later be
///   freed against `cow` or another `VmCowPages` in the same hierarchy.
/// * The caller must deal with any part of the range in which `cow` can still see its parent
///   afterwards, as [`VmCowPages::release_owned_pages_range_locked`] requires.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_release_owned_pages_range_locked(
    cow: &VmCowPages,
    offset: u64,
    len: u64,
    parent: Option<&VmCowPages>,
    freed_list: Pin<&mut ScopedPageFreedList>,
) {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: If `parent` is `Some`, per `# Safety` the caller holds `parent.lock()` for the
    // duration of this call with no conflicting borrow live, and the token is only passed on in
    // that case. `cow.lock()`, a lock of the same class, is held either way.
    let parent_token = unsafe { LockToken::new() };
    // SAFETY: `token` and `parent_token` are for the locks of `cow` and of `parent`, as made above.
    // Every other clause of the contract of `release_owned_pages_range_locked` is part of this
    // function's `# Safety`: the range, `parent` being the immediate parent, the ancestor locks,
    // `freed_list`, `cow` not being hidden, no live Rust borrow of the state that the locks of
    // `cow` and `parent` guard (which includes their page lists and the attribution tracker of
    // `parent`), and the part of the range that still sees the parent.
    unsafe {
        cow.release_owned_pages_range_locked(
            &mut token,
            offset,
            len,
            parent.map(|p| (p, &parent_token)),
            freed_list,
        );
    }
}

/// Invalidates any DIRTY requests in `[offset, offset + len)` by spuriously resolving them.
///
/// # Safety
///
/// `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` with no
/// conflicting Rust borrow of the state it guards live.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_invalidate_dirty_requests_locked(
    cow: &VmCowPages,
    offset: u64,
    len: u64,
) {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let token = unsafe { LockToken::new() };
    cow.invalidate_dirty_requests_locked(&token, offset, len);
}

/// Attempts to commit `range` in `cow`, writing the committed length to `committed_len`.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` exclusively
///   with no conflicting Rust borrow of the state it guards live.
/// * This may acquire the lock of each ancestor of `cow` in turn, so the caller must not hold any
///   of them and must be in a blockable thread context, with no spinlocks held and no active
///   `ChainLockTransaction`. Under lockdep, every `VmCowPages` lock the caller holds must have a
///   lock order below that of each of them.
/// * `cow` must be `LifeCycle::Alive` and must not be a hidden node.
/// * `range` must be page-aligned.
/// * `deferred` must point to a valid, pinned `DeferredOps` constructed for `cow`.
/// * `committed_len` must be valid for writes.
/// * `page_request` must point to a valid, pinned `MultiPageRequest` with no active request.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_commit_range_locked(
    cow: &VmCowPages,
    range: VmCowRange,
    deferred: Pin<&mut DeferredOps<'_>>,
    committed_len: &mut MaybeUninit<u64>,
    page_request: Pin<&MultiPageRequest>,
) -> Result<(), Status> {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: The caller upholds the `range`, `deferred`, and `page_request` preconditions. The
    // callee's last two clauses are the third and second bullets of `# Safety`.
    let (status, len) =
        unsafe { cow.commit_range_locked(&mut token, range, deferred, page_request) };
    committed_len.write(len);
    status
}

/// Resizes `cow` to `s` bytes. `s` must be a multiple of the page size.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages` that is not a hidden node.
/// * `s` must be page-aligned and `<= VmPageList::MAX_SIZE`.
/// * This acquires `cow.lock()`, and can acquire the `paged_vmo_lock` of the page source at the
///   root of `cow`'s hierarchy and the locks of other nodes in that hierarchy. The caller must not
///   hold `cow.lock()`, the lock of any other `VmCowPages`, or that `paged_vmo_lock`, and must be
///   in a blockable thread context, with no spinlocks held and no active `ChainLockTransaction`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_resize(cow: &VmCowPages, s: u64) -> Result<(), Status> {
    // SAFETY: Caller upholds the `# Safety` preconditions of `VmCowPages::resize`.
    unsafe { cow.resize(s) }
}

/// Zeroes `range` in `cow`, returning the status and the number of bytes zeroed.
///
/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`, and the caller must hold `cow.lock()` exclusively
///   with no conflicting Rust borrow of the state it guards live.
/// * `cow` must be `LifeCycle::Alive` and must not be a hidden node.
/// * This may acquire the lock of each ancestor of `cow` in turn, so the caller must not hold any
///   of them and must be in a blockable thread context, with no spinlocks held and no active
///   `ChainLockTransaction`. Under lockdep, every `VmCowPages` lock the caller holds must have a
///   lock order below that of each of them.
/// * `range` must be page-aligned.
/// * `deferred` must point to a valid `DeferredOps` constructed for `cow`, to which the caller has
///   exclusive access for the call, and `page_request` to a valid `MultiPageRequest` with no active
///   request. Both must be pinned: neither may be moved to another address, or have its storage
///   reused, before it is destroyed in place.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_cow_pages_zero_pages_locked(
    cow: &VmCowPages,
    range: VmCowRange,
    dirty_track: bool,
    deferred: Pin<&mut DeferredOps<'_>>,
    page_request: Pin<&MultiPageRequest>,
) -> bindings::VmCowPagesZeroPagesResult {
    // SAFETY: Per `# Safety`, the caller holds `cow.lock()` with no conflicting borrow live.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: `token` was created just above for `cow.lock()`, which the caller holds, so it is for
    // the lock of `cow`. The caller upholds the `range`, `deferred`, and `page_request` clauses.
    // The callee's last two clauses are the second and third bullets of `# Safety`.
    let (status, zeroed_len) =
        unsafe { cow.zero_pages_locked(&mut token, range, dirty_track, deferred, page_request) };
    bindings::VmCowPagesZeroPagesResult {
        status: Status::result_into_raw(status),
        __bindgen_padding_0: [0; 4],
        zeroed_len,
    }
}

#[cfg(ktest)]
#[unittest::suite]
/// Tests for `VmCowRange`, `LockedPtr`, and `LookupCursor`.
mod vm_cow_pages_rs {
    use super::{LockedPtr, VmCowPages, VmCowRange};
    use crate::kernel::types::PAddr;
    use crate::vm::page_source::MultiPageRequest;
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object::{Resizability, SnapshotType, VmObject};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::mem::MaybeUninit;
    use core::ptr;
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
                    // page-aligned, and `page_size` is non-zero. The cow pages of `vmo` are alive
                    // and not hidden, and have no ancestor for a cursor to lock: `vmo` was created
                    // above by `VmObjectPaged::create` and has not been cloned.
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
                    // `vmo_size` is non-zero. The cow pages of `vmo` are alive, not hidden and have
                    // no ancestor, as above.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(guard.as_mut().token_mut(), 0, vmo_size)
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                expect_true!(cursor.lock_is_held());
                cursor.as_mut().disable_mark_accessed();

                // `paddrs` is empty, so 0 pages are requested within the 3-page window.
                expect_eq!(cursor.as_mut().if_exist_pages(true, &mut []).len(), 0);

                let mut paddrs = [MaybeUninit::<PAddr>::uninit(); 3];
                // `paddrs.len() == 3` fits in the 3 remaining pages of `cursor`.
                let found = cursor.as_mut().if_exist_pages(true, &mut paddrs);
                expect_eq!(found.len(), 1);

                // 2 pages remain in `cursor`'s 3-page window after `found.len() == 1`.
                let skipped = cursor.as_mut().skip_missing_pages();
                expect_eq!(skipped, 2);
            }

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `0` and `page_size` are page-aligned,
                    // and `page_size` is non-zero. The cow pages of `vmo` are alive, not hidden and
                    // have no ancestor, as above.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(guard.as_mut().token_mut(), 0, page_size)
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                cursor.as_mut().disable_mark_accessed();
                // 1 page remains in `cursor`, and page 0 was committed above.
                expect_true!(cursor.as_mut().maybe_page(false).is_some());
            }

            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `page_size` and `2 * page_size` are
                    // page-aligned, and `2 * page_size` is non-zero. The cow pages of `vmo` are
                    // alive, not hidden and have no ancestor, as above.
                    let cursor = unsafe {
                        vmo.get_lookup_cursor_locked(
                            guard.as_mut().token_mut(),
                            page_size,
                            2 * page_size,
                        )
                    }
                );
                let mut cursor = unwrap_ok!(cursor);
                // 2 pages remain in `cursor`.
                expect_true!(cursor.as_mut().maybe_page(false).is_none());

                // SAFETY: `deferred` belongs to `vmo`, `1` is non-zero and fits in the 1 remaining
                // page of `cursor`, and `page_req` has no active request.
                let owned = unwrap_ok!(unsafe {
                    cursor.as_mut().require_owned_page(
                        true,
                        1,
                        deferred.as_mut(),
                        page_req.as_ref(),
                    )
                });
                expect_true!(owned.writable);
            }
            {
                stack_try_pin_init!(
                    // SAFETY: `guard` holds `vmo`'s lock, `page_size` is page-aligned, and
                    // `page_size` is non-zero. The cow pages of `vmo` are alive, not hidden and
                    // have no ancestor, as above.
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
                    cursor.as_mut().require_page(false, 1, deferred.as_mut(), page_req.as_ref())
                });
                expect_false!(readable.writable);
            }
        }
    }

    /// Tests `LockedPtr` acquire, `locked_or`, `move_from`, `step_to_parent`, and release/drop.
    #[test]
    fn locked_ptr_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        let page_size = ::page::SIZE as u64;
        let vmo1 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, page_size));
        let vmo2 = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, 2 * page_size));
        let cow1 = vmo1.cow_pages();
        let cow2 = vmo2.cow_pages();
        // Both are anonymous roots and so share a lock order; holding both at once (as `move_from`
        // below does) needs `cow2` acquired with a strictly greater order.
        let cow2_order = cow1.lock_order() + VmCowPages::LOCK_ORDER_DELTA;

        expect_false!(vmo1.lock_is_held());
        ksync::lock!(let lp = LockedPtr::empty());
        expect_true!(lp.is_none());
        expect_false!(lp.is_some());
        expect_true!(lp.get().is_none());
        lp.as_mut().release();
        expect_true!(lp.is_none());

        // `locked_or` falls back to `cow1` when `lp` is empty.
        {
            ksync::lock!(let guard = cow1.lock());
            let (target, tok) = lp.locked_or(cow1, guard.token());
            expect_eq!(target.size_locked(tok), page_size);
        }
        expect_false!(vmo1.lock_is_held());

        // A root has no parent to step to.
        {
            ksync::lock!(let guard = cow1.lock());
            expect_false!(lp.as_mut().step_to_parent(cow1, guard.token()));
            expect_true!(lp.is_none());
        }

        // Move a freshly acquired `cow1` into `lp`, test `locked` and `locked_mut`.
        {
            ksync::lock!(let held = LockedPtr::new(cow1));
            expect_true!(held.is_some());
            lp.as_mut().move_from(held.as_mut());
            expect_true!(held.is_none());
        }
        expect_true!(lp.is_some());
        expect_true!(vmo1.lock_is_held());
        {
            let (target, tok) = lp.locked();
            expect_eq!(target.size_locked(tok), page_size);
        }
        {
            let (target, tok) = lp.as_mut().locked_mut();
            expect_eq!(target.size_locked(tok), page_size);
        }

        // `move_from` releases `cow1` and takes over `cow2`, acquired before `cow1` is released.
        {
            ksync::lock!(let held = LockedPtr::with_order(cow2, cow2_order));
            expect_true!(vmo1.lock_is_held());
            expect_true!(vmo2.lock_is_held());
            lp.as_mut().move_from(held.as_mut());
            expect_true!(held.is_none());
        }
        expect_false!(vmo1.lock_is_held());
        expect_true!(vmo2.lock_is_held());
        {
            let (target, tok) = lp.locked();
            expect_eq!(target.size_locked(tok), 2 * page_size);
        }

        // Moving from an empty `LockedPtr` releases the lock and leaves `lp` empty.
        {
            ksync::lock!(let none = LockedPtr::empty());
            lp.as_mut().move_from(none);
        }
        expect_true!(lp.is_none());
        expect_false!(vmo2.lock_is_held());

        // Explicit release of a held `LockedPtr` drops the lock.
        {
            ksync::lock!(let held = LockedPtr::new(cow2));
            expect_true!(vmo2.lock_is_held());
            held.as_mut().release();
            expect_true!(held.is_none());
            expect_false!(vmo2.lock_is_held());
        }

        // Dropping a held `LockedPtr` releases the lock.
        {
            ksync::lock!(let held = LockedPtr::new(cow2));
            expect_true!(vmo2.lock_is_held());
        }
        expect_false!(vmo2.lock_is_held());

        // `step_to_parent` acquires the parent before releasing the child, and leaves the child's
        // lock (held by the caller's token) alone.
        let child = unwrap_ok!(vmo1.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            page_size,
            false,
        ));
        let child = VmObject::downcast_paged(child).expect("clone is paged");
        let child_cow = child.cow_pages();
        let parent = child_cow.debug_get_parent().expect("clone has a parent");
        // The lockdep precondition for acquiring the parent while holding the child (without
        // lockdep `lock_order()` is a placeholder).
        if cfg!(with_lock_dep) {
            expect_true!(parent.lock_order() > child_cow.lock_order());
        }
        {
            // Acquire the child with its lock order, as a hierarchy walk does, so that lockdep
            // checks the ordered child-to-parent acquisition.
            ksync::lock!(let guard = LockedPtr::new(child_cow));
            ksync::lock!(let walker = LockedPtr::empty());
            expect_true!(walker.as_mut().step_to_parent(child_cow, guard.locked_token()));
            expect_true!(walker.get().is_some_and(|p| ptr::eq(p, &*parent)));
            expect_true!(parent.lock_is_held());
            expect_true!(child.lock_is_held());
            // The hidden root has no parent to step to. `walker` holds the parent, so the
            // fallback node and token passed here are not used.
            expect_false!(walker.as_mut().step_to_parent(child_cow, guard.locked_token()));
            expect_true!(walker.get().is_some_and(|p| ptr::eq(p, &*parent)));
            walker.as_mut().release();
            expect_false!(parent.lock_is_held());
            expect_true!(child.lock_is_held());
        }
        expect_false!(child.lock_is_held());
    }
}
