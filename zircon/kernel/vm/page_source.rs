// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! At the high level the goal of the objects here is to
//! 1. Trigger external entities to do work based on VMO operations, such as asking a pager to
//!    supply a missing page of data.
//! 2. Have a way for external entities to let the VMO system know these requests have been
//!    fulfilled.
//! 3. Provide a way for the high level caller, who may not know what actions are being performed
//!    on what entities, to wait until their operation can be completed.
//!
//! The different objects can be summarized as:
//!  * PageRequest: Caller allocated object that the caller uses to perform the `wait`.
//!  * PageSource: Performs request and overlap tracking, forwarding unique ranges of requests to
//!    the underlying PageProvider.
//!  * PageProvider: Asynchronously performs requests. Requests are completed by actions being
//!    performed on the VMO.
//!
//! A typical flow would be
//!  * User allocates PageRequest on the stack, and passes it in to some VMO operation
//!  * VMO code needs something to happen and calls a PageSource method, passing in PageRequest it
//!    had been given.
//!  * PageSource populates fields of the PageRequest and adds it to the list of requests it is
//!    tracking, and determines how this request overlaps with any others. Based on overlap, it may
//!    or may not notify the underlying PageProvider that some work needs to be done (the page
//!    provider will complete this asynchronously somehow).
//!  * VMO returns ZX_ERR_SHOULD_WAIT and then the top level calls `PageRequest::wait`
//!  * `PageRequest::wait` asks the underlying PageSource how to wait for the operation to complete
//!  * As an optional path, if the PageRequest was not waited on for some reason, the PageRequest
//!    will also inform the PageSource that this request is no longer needed and can be canceled.
//!
//! For the other side, while the wait is happening some other thread will
//!  * Call a VMO operation, such as VmObject::SupplyPages
//!  * VMO will perform the operation, and then let the PageSource know by the corresponding
//!    interface method, such as `on_pages_supplied`.
//!  * PageSource will update request tracking, and notify any PageRequests that were waiting and
//!    can be woken up.
//!
//! There is more complexity of implementation and API, largely to handle the fact that the
//! PageRequest serves as the allocation of all data needed for all parties. Therefore every layer
//! needs to be told when requests are coming and going to ensure they update any lists and do not
//! refer to out of scope stack variables.

use crate::counters::define_kcounter;
use crate::kernel::event::{AutounsignalEvent, Event};
use crate::ksync_kernel::TrackedMutex;
use crate::vm::anonymous_page_request::AnonymousPageRequest;
use crate::vm::page::{VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::vm::{
    oom_ktrace_duration, vm_ktrace_duration, vm_ktrace_flow_begin, vm_ktrace_flow_end,
};
use core::cell::{Cell, UnsafeCell};
use core::convert::Infallible;
use core::ffi::{c_char, c_uint};
use core::fmt;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use fbl::{
    Canary, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, HasRefCount,
    Recyclable, RefCounted, RefPtr, WavlTree, WavlTreeContainable, WavlTreeNode, ref_counted,
};
use ksync::{KMutex, LockToken, guarded};
use page_bindings::vm_page_t;
use page_source_bindings as bindings;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zr::Opaque;
use zx_status::Status;
use zx_types::{ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_OK, zx_koid_t, zx_status_t};

const LOCAL_TRACE: u32 = 0;

define_kcounter!(PGREQ_OVERLAP_0, "vm.page_request.overlap_0", Sum);
define_kcounter!(PGREQ_OVERLAP_1, "vm.page_request.overlap_1", Sum);
define_kcounter!(PGREQ_OVERLAP_2, "vm.page_request.overlap_2", Sum);
define_kcounter!(PGREQ_OVERLAP_3, "vm.page_request.overlap_3", Sum);
define_kcounter!(PGREQ_OVERLAP_4, "vm.page_request.overlap_4", Sum);
define_kcounter!(PGREQ_OVERLAP_5_TO_10, "vm.page_request.overlap_5_to_10", Sum);
define_kcounter!(PGREQ_OVERLAP_MORE_THAN_10, "vm.page_request.overlap_more_than_10", Sum);

zr::static_assert!(size_of::<MultiPageRequest>() == vm_constants_rs::kMultiPageRequestSize);
zr::static_assert!(align_of::<MultiPageRequest>() == vm_constants_rs::kMultiPageRequestAlign);

/// Debug information identifying the VMO a page request is for.
pub type VmoDebugInfo = bindings::VmoDebugInfo;

/// The different types of page requests that can exist.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageRequestType {
    /// Request to provide the initial contents for the page.
    Read = 0,
    /// Request to alter contents of the page, i.e. transition it from clean to dirty.
    Dirty = 1,
    /// Request to write back modified page contents back to the source.
    Writeback = 2,
}

zr::static_assert!(size_of::<PageRequestType>() == size_of::<bindings::page_request_type>());
zr::static_assert!(align_of::<PageRequestType>() == align_of::<bindings::page_request_type>());
zr::static_assert!(PageRequestType::Read as u32 == bindings::page_request_type::READ as u32);
zr::static_assert!(PageRequestType::Dirty as u32 == bindings::page_request_type::DIRTY as u32);
zr::static_assert!(
    PageRequestType::Writeback as u32 == bindings::page_request_type::WRITEBACK as u32
);
zr::static_assert!(PageRequestType::COUNT == bindings::page_request_type::COUNT as usize);

impl PageRequestType {
    /// Number of page request types.
    pub const COUNT: usize = 3;

    /// Converts the raw `page_request_type` value `value` without bounds checking.
    ///
    /// # Safety
    ///
    /// `value` must be a valid `PageRequestType` discriminant (`< Self::COUNT as u32`).
    #[inline]
    const unsafe fn from_u32_unchecked(value: u32) -> Self {
        debug_assert!((value as usize) < Self::COUNT);
        // SAFETY: `PageRequestType` is `#[repr(u32)]` with discriminants `0..Self::COUNT as u32`,
        // and the caller guarantees `value` is in range.
        unsafe { core::mem::transmute(value) }
    }

    /// Returns a human readable name for this `PageRequestType`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "READ",
            Self::Dirty => "DIRTY",
            Self::Writeback => "WRITEBACK",
        }
    }
}

/// Every page request type, indexed by its value.
const PAGE_REQUEST_TYPES: [PageRequestType; PageRequestType::COUNT] =
    [PageRequestType::Read, PageRequestType::Dirty, PageRequestType::Writeback];

impl TryFrom<u32> for PageRequestType {
    type Error = u32;

    /// Converts the raw `page_request_type` value `value`, failing for out-of-range values.
    fn try_from(value: u32) -> Result<Self, u32> {
        match value {
            x if x == Self::Read as u32 => Ok(Self::Read),
            x if x == Self::Dirty as u32 => Ok(Self::Dirty),
            x if x == Self::Writeback as u32 => Ok(Self::Writeback),
            other => Err(other),
        }
    }
}

impl fmt::Display for PageRequestType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// These properties are constant per `PageProvider` type, so a given `VmCowPages` can query and
/// cache these properties once (if it has a `PageSource`) and know they won't change after that.
/// This also avoids per-property plumbing via `PageSource`.
///
/// TODO(dustingreen): (or rashaeqbal) Migrate more const per-PageProvider-type properties to
/// PageSourceProperties, after the initial round of merging is done.
pub type PageSourceProperties = bindings::PageSourceProperties;

/// Returns the printable prefix of a (possibly not NUL-terminated) VMO or provider name.
fn name_prefix(name: &[u8]) -> &str {
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let slice = &name[..len];
    match core::str::from_utf8(slice) {
        Ok(s) => s,
        Err(e) => {
            // SAFETY: `e.valid_up_to()` is in bounds of `slice` and guarantees a valid UTF-8
            // prefix.
            unsafe { core::str::from_utf8_unchecked(slice.get_unchecked(..e.valid_up_to())) }
        }
    }
}

/// The `PageRequest` provides the ability to be in two difference linked list. One owned by the
/// page source (for overlapping requests), and one owned by the page provider (for tracking
/// outstanding requests). These tags provide a way to distinguish between the two containers.
///
/// Tag for the list owned by the page source, for overlapping requests.
struct PageSourceTag;

/// Tag for the list owned by the page provider, for tracking outstanding requests.
///
/// `PageRequest`s are owned by their callers, so lists of this tag hold unmanaged pointers and do
/// not keep their members alive.
pub struct PageProviderTag;

/// The lock class of `PageSource::page_source_mtx`.
type PageSourceMtxClass = PageSourcePageSourceMtxClass;

/// The lock class of `PageSource::paged_vmo_mutex`.
pub type PagedVmoMutexClass = PageSourcePagedVmoMutexClass;

/// Interior mutable storage for state that is protected by the `page_source_mtx` of a
/// `PageSource`.
///
/// This is used both for `PageSource` state that needs to be accessed concurrently with other
/// `page_source_mtx` protected state (such as holding a cursor into a tree of requests while
/// modifying the requests), and for the fields of a `PageRequest` that are protected by the lock
/// of the `PageSource` the request is currently associated with.
///
/// The lock token only proves that *a* `page_source_mtx` is held. All accesses are module private
/// and are only ever performed by `PageSource` methods on its own state, or on requests that are
/// associated with it (i.e. are being initialized by it or are in its trees or overlap lists),
/// while holding its `page_source_mtx`.
#[repr(transparent)]
struct SourceLocked<T>(UnsafeCell<T>);

impl<T> SourceLocked<T> {
    /// Creates a new `SourceLocked` holding `value`.
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// Returns an in-place initializer for a `SourceLocked` holding the value initialized by
    /// `init`.
    fn pin_init<E>(init: impl PinInit<T, E>) -> impl PinInit<Self, E> {
        // SAFETY: `SourceLocked<T>` is a `#[repr(transparent)]` wrapper around `UnsafeCell<T>`,
        // which is itself `#[repr(transparent)]` around `T`.
        unsafe { pin_init::cast_pin_init(init) }
    }

    /// Returns a copy of the protected value.
    fn get(&self, _token: &LockToken<'_, PageSourceMtxClass>) -> T
    where
        T: Copy,
    {
        // SAFETY: The token proves the `page_source_mtx` protecting this value is held (see the
        // type level documentation), so there are no concurrent writes. No mutable references to
        // copyable values are ever handed out.
        unsafe { *self.0.get() }
    }

    /// Overwrites the protected value.
    fn set(&self, _token: &LockToken<'_, PageSourceMtxClass>, value: T)
    where
        T: Copy,
    {
        // SAFETY: The token proves the `page_source_mtx` protecting this value is held (see the
        // type level documentation), so there are no concurrent accesses. No references to
        // copyable values are ever handed out.
        unsafe { *self.0.get() = value }
    }

    /// Returns a shared reference to the protected value.
    ///
    /// # Safety
    ///
    /// No mutable reference to the protected value obtained via `get_mut_unchecked` may be live.
    unsafe fn get_unchecked<'a>(&'a self, _token: &'a LockToken<'_, PageSourceMtxClass>) -> &'a T {
        // SAFETY: The token proves the protecting lock is held so there are no concurrent writers,
        // and the caller guarantees there are no live mutable references.
        unsafe { self.0.get().as_ref_unchecked() }
    }

    /// Returns a mutable reference to the protected value.
    ///
    /// # Safety
    ///
    /// No other reference to the protected value may be live for the lifetime of the returned
    /// reference.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut_unchecked<'a>(
        &'a self,
        _token: &'a LockToken<'_, PageSourceMtxClass>,
    ) -> &'a mut T {
        // SAFETY: The token proves the protecting lock is held so there are no concurrent
        // accesses from other threads, and the caller guarantees there are no other live
        // references.
        unsafe { self.0.get().as_mut_unchecked() }
    }
}

/// Object which is used to make delayed page requests to a PageSource
#[derive(WavlTreeContainable, DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PageRequest {
    #[pin]
    #[wavl_node]
    tree_node: WavlTreeNode<PageRequest>,

    #[pin]
    #[dll_node(tag = PageSourceTag)]
    overlap_node: DoublyLinkedListNode<PageRequest>,

    #[pin]
    #[dll_node(tag = PageProviderTag)]
    provider_node: DoublyLinkedListNode<PageRequest>,

    /// Event signaled when the request is fulfilled.
    #[pin]
    event: AutounsignalEvent,

    /// The page source this request is currently associated with. This may only be modified by
    /// `init` and must otherwise be constant, allowing the PageRequest to safely inspect this value
    /// without races.
    src: UnsafeCell<Option<RefPtr<PageSource>>>,

    /// PageRequests are active if offset is not `u64::MAX`. In an inactive request, the
    /// only other valid field is src. Whilst a request is with a PageProvider (i.e.
    /// `send_async_request` has been called), these fields must be kept constant so the
    /// PageProvider can read them. Once the request has been cleared either by
    /// `swap_async_request` or `clear_async_request` they can be modified again. The
    /// `provider_owned` bool is used for assertions to validate this flow, but otherwise has
    /// no functional affect.
    ///
    /// `offset`, `len`, `vmo_id`, `type_`, and `provider_owned` use `Ordering::Relaxed`
    /// atomics because safe [`PageProvider`] accessors and ktrace instrumentation read them
    /// through `&PageRequest` without holding `page_source_mtx`, while `offset` is also polled
    /// in `is_initialized()` before acquiring `page_source_mtx`. Causal ordering comes from
    /// the locks on the handoff paths (`page_source_mtx` and the provider's lock across
    /// `send_async_request`, `swap_async_request`, and `clear_async_request`; the wait queue
    /// lock inside `Event::signal_worker` and `Event::wait_worker` on completion; and
    /// re-checking `is_initialized()` under `page_source_mtx` on cancellation or reuse).
    offset: AtomicU64,

    /// The total length of the request.
    len: AtomicU64,

    /// The offset into the request at which the event should next be signaled. This is request
    /// relative, so a value of 0 indicates that it should be signaled when the page at offset is
    /// provided. After being triggered, the wake_offset increments by the amount provided so that
    /// it can potentially get triggered again.
    wake_offset: SourceLocked<u64>,

    /// Keeps track of the size of the request that still needs to be fulfilled. This
    /// can become incorrect if some pages get supplied, decommitted, and then
    /// re-supplied. If that happens, then it will cause the page request to complete
    /// prematurely. However, page source clients should be operating in a loop to handle
    /// evictions, so this will simply result in some redundant read requests to the
    /// page source. Given the rarity in which this situation should arise, it's not
    /// worth the complexity of tracking it.
    pending_size: SourceLocked<u64>,

    /// The vmobject this page request is for.
    vmo_id: AtomicU64,

    /// The name of the vmobject this page request is for.
    vmo_name: SourceLocked<[c_char; 8]>,

    /// Linked list for overlapping requests.
    #[pin]
    overlap: SourceLocked<DoublyLinkedList<NonNull<PageRequest>, PageSourceTag>>,

    /// Tracks any error that we will send to the waiter when the request is completed. This allows
    /// for partial failure of a request, where we report the status of the first page in the
    /// request so that any partially provided pages can be processed.
    complete_status: SourceLocked<zx_status_t>,

    /// The type of the page request.
    ///
    /// Always holds a valid [`PageRequestType`] discriminant (`< PageRequestType::COUNT as u32`).
    type_: AtomicU32,

    /// Set on construction if the user of the PageRequest supports, and wants to be, woken early.
    /// If this is true then wake_offset will be set to zero when a request is initialized. Early
    /// waking is intended to allow for an optimization under the assumption that large requests
    /// will be filled sequentially, allowing for a single request to be made to the underlying
    /// page source, but processing being able to start before the entire request has been
    /// completed. When an early_wake request is signaled the user cannot assume that the request
    /// is fully complete, and as a consequence must not attempt to use the PageRequest in a new
    /// context without first cancelling it.
    ///
    /// Wrapped in `UnsafeCell` so `PageRequest` contains zero `Freeze` bytes.
    early_wake: UnsafeCell<bool>,

    /// See `offset`.
    provider_owned: AtomicBool,
}

// SAFETY: A `PageRequest` is owned by a single caller thread at a time (`!Sync`), while
// `NonNull<PageRequest>` pointers are shared with the associated `PageSource` and its
// `PageProvider`. The owner-only `src` field is only accessed by the owning thread (or
// synchronized by `page_source_mtx`). All other non-atomic fields are accessed under the
// associated `PageSource`'s `page_source_mtx` (see `SourceLocked`), the intrusive container
// nodes are manipulated under the container owner's lock, `early_wake` is immutable after
// initialization, and `event` is thread-safe.
unsafe impl Send for PageRequest {}

impl fbl::WavlTreeKeyable<u64> for PageRequest {
    type Key<'a> = u64;

    /// # Safety
    ///
    /// The caller must hold the `page_source_mtx` of the `PageSource` this request is associated
    /// with, so that `offset` and `len` (from which the key is derived) are not concurrently
    /// modified while the request is in that source's tree of outstanding requests.
    unsafe fn get_key(&self) -> u64 {
        self.get_end()
    }
}

#[pinned_drop]
impl PinnedDrop for PageRequest {
    fn drop(self: Pin<&mut Self>) {
        self.as_ref().cancel_request();
    }
}

impl PageRequest {
    /// Returns an in-place initializer for a `PageRequest` that does not support early waking.
    pub fn new() -> impl PinInit<Self, Infallible> {
        Self::with_early_wake(false)
    }

    /// Returns an in-place initializer for a `PageRequest`.
    ///
    /// If `early_wake` is true then the caller is asking to be woken up once some of the request is
    /// satisfied, potentially before all of it is satisfied. This is intended to allow users to
    /// process partial amounts of data as they come in before continuing to `wait` for the rest
    /// with only a single `PageRequest` sent to the `PageSource`.
    pub fn with_early_wake(early_wake: bool) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            tree_node: WavlTreeNode::new(),
            overlap_node: DoublyLinkedListNode::new(),
            provider_node: DoublyLinkedListNode::new(),
            event <- AutounsignalEvent::init_unsignaled(),
            src: UnsafeCell::new(None),
            offset: AtomicU64::new(u64::MAX),
            len: AtomicU64::new(0),
            wake_offset: SourceLocked::new(u64::MAX),
            pending_size: SourceLocked::new(0),
            vmo_id: AtomicU64::new(0),
            vmo_name: SourceLocked::new([0; 8]),
            overlap <- SourceLocked::pin_init(DoublyLinkedList::new()),
            complete_status: SourceLocked::new(ZX_OK),
            type_: AtomicU32::new(PageRequestType::Read as u32),
            early_wake: UnsafeCell::new(early_wake),
            provider_owned: AtomicBool::new(false),
        })
    }

    /// Domain-specific conversion: returns raw pointer for `PageRequest`.
    ///
    /// C++ page providers only pass this pointer back to Rust as a `&PageRequest`, and every field
    /// a provider may read is an atomic, so handing out a `*mut` derived from `&self` never writes
    /// through it outside interior mutability.
    fn as_raw(&self) -> *mut bindings::PageRequest {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns whether this request is configured for early waking.
    #[inline]
    fn is_early_wake(&self) -> bool {
        // SAFETY: `early_wake` is initialized on construction and never mutated afterwards.
        unsafe { *self.early_wake.get() }
    }

    /// Returns the current offset of the request, which is `u64::MAX` for inactive requests.
    fn cur_offset(&self) -> u64 {
        self.offset.load(Ordering::Relaxed)
    }

    /// Returns the current length of the request.
    fn cur_len(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }

    /// Returns the current type of the request.
    #[inline]
    fn cur_type(&self) -> PageRequestType {
        // SAFETY: `type_` is initialized to `PageRequestType::Read as u32` and only ever written
        // with valid `PageRequestType` discriminants in `init`.
        unsafe { PageRequestType::from_u32_unchecked(self.type_.load(Ordering::Relaxed)) }
    }

    /// Returns whether the request is currently owned by a `PageProvider`.
    fn is_provider_owned(&self) -> bool {
        self.provider_owned.load(Ordering::Relaxed)
    }

    /// Returns whether this request is in a `PageSourceTag` list.
    fn in_overlap_list(&self, _token: &LockToken<'_, PageSourceMtxClass>) -> bool {
        DoublyLinkedListContainable::<PageRequest, PageSourceTag>::get_node(self).in_container()
    }

    /// Returns whether this request is currently in a `PageProviderTag` list.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self`'s `PageProviderTag` node is not concurrently modified (either
    /// by holding the associated `PageSource`'s `page_source_mtx` while
    /// `!self.is_provider_owned()`, or by holding the owning `PageProvider`'s lock).
    pub unsafe fn in_container(&self) -> bool {
        DoublyLinkedListContainable::<PageRequest, PageProviderTag>::get_node(self).in_container()
    }

    /// PageRequests are initialized separately to being constructed to facilitate any PageSource
    /// specific logic. This method makes three assumptions on how it is called:
    ///  1. If previously initialized it has been separately uninitialized via `cancel_request` or
    ///     similar.
    ///  2. It is invoked under the src lock.
    ///  3. It is called on the thread that owns the PageRequest and is not thread safe with
    ///     parallel invocations of `cancel_request`.
    fn init(
        self: Pin<&Self>,
        token: &LockToken<'_, PageSourceMtxClass>,
        src: RefPtr<PageSource>,
        offset: u64,
        ty: PageRequestType,
        vmo_debug_info: VmoDebugInfo,
    ) {
        let this = self.get_ref();
        debug_assert!(!this.is_initialized());
        this.vmo_id.store(vmo_debug_info.vmo_id, Ordering::Relaxed);
        this.vmo_name.set(token, vmo_debug_info.vmo_name);
        this.len.store(0, Ordering::Relaxed);
        this.offset.store(offset, Ordering::Relaxed);
        if this.is_early_wake() {
            this.wake_offset.set(token, 0);
        }
        debug_assert!((ty as usize) < PageRequestType::COUNT);
        this.type_.store(ty as u32, Ordering::Relaxed);
        // SAFETY: `PageRequest` is `!Sync`, and `src` is only accessed by the thread that owns the
        // request while holding `page_source_mtx`, so there are no concurrent accesses.
        unsafe { *this.src.get() = Some(src) };
        this.complete_status.set(token, ZX_OK);

        // The event is only signaled by the source, under the source lock which we hold.
        let _ = this.event.unsignal();
    }

    /// Returns whether the request is currently active.
    fn is_initialized(&self) -> bool {
        self.cur_offset() != u64::MAX
    }

    /// Returns the end of the request.
    fn get_end(&self) -> u64 {
        // Assert on overflow, since it means vmobject made an out-of-bounds request.
        let (end, overflowed) = self.cur_offset().overflowing_add(self.cur_len());
        debug_assert!(!overflowed);

        end
    }

    fn range_overlaps(&self, start: u64, end: u64) -> bool {
        end > self.cur_offset() && start < self.get_end()
    }

    /// Converts a [start,end) range in provider (aka VMO) space to the sub range that overlaps
    /// with this request and returns it relative to this requests offset.
    fn trim_range_to_request_space(&self, start: u64, end: u64) -> (u64, u64) {
        let offset = self.cur_offset();
        // The operation started partway into this request. If the operation started before this
        // request, this saturates to 0.
        let req_offset = start.saturating_sub(offset);
        let req_end = if end < self.get_end() {
            // The operation ended partway into this request.
            let req_end = end.wrapping_sub(offset);

            debug_assert!(end.checked_sub(offset).is_some());
            req_end
        } else {
            // The operation ended past the end of this request.
            self.cur_len()
        };
        debug_assert!(req_end >= req_offset);
        (req_offset, req_end)
    }

    /// Returns `Ok(())` on success, or a permitted error code if the backing page provider
    /// explicitly failed this page request. Returns `Err(Status::INTERRUPTED_KILLED)` if the
    /// thread was killed. Returns `Err(Status::INTERRUPTED_RETRY)` if `suspendable` is true and
    /// the thread was suspended; the thread cannot be suspended in the wait if `suspendable` is
    /// false. If this page requested is allowed to early wake then this can return success with
    /// the request still active and queued with a PageSource. In this case it is invalid to
    /// attempt to use this request with any other PageSource or for any other range without first
    /// doing `cancel_request`.
    pub fn wait(self: Pin<&Self>, suspendable: bool) -> Result<(), Status> {
        lockdep::assert_no_locks_held();
        let this = self.get_ref();
        // SAFETY: `PageRequest` is `!Sync`, and `src` is only modified by the thread that owns the
        // request.
        let src = unsafe { &*this.src.get() };
        let src = src.as_ref().expect("waiting on a PageRequest that was never initialized");
        let mut pager_name = MaybeUninit::<[u8; ZX_MAX_NAME_LEN]>::uninit();
        let pager_name = &mut pager_name;
        oom_ktrace_duration!(
            1,
            "page_request_wait",
            "vmo_id" => this.vmo_id.load(Ordering::Relaxed),
            "offset" => this.cur_offset(),
            "length" => this.cur_len(),
            "type" => if this.cur_type() == PageRequestType::Read {
                "Read"
            } else {
                "Dirty"
            },
            "pager_name" => {
                let pager_name = MaybeUninit::write(pager_name, [0u8; ZX_MAX_NAME_LEN]);
                src.get_provider_debug_name(pager_name);
                name_prefix(pager_name)
            }
        );
        let status = src.wait_on_request(this, suspendable);
        vm_ktrace_flow_end!(1, "page_request_signal", this as *const Self as usize);
        if let Err(error) = status
            && !PageSource::is_valid_internal_failure_code(error)
        {
            src.cancel_request(this);
        }
        status
    }

    /// Asks the underlying `PageSource` to abort this request, by calling
    /// `PageSource::cancel_request`. As this can be called from non `PageSource` paths, and hence
    /// without the `PageSource` lock held, the `PageSource` must always be invoked to synchronize
    /// with this request being completed by another thread.
    /// This method is not thread safe and cannot be called in parallel with `init`.
    pub fn cancel_request(self: Pin<&Self>) {
        let this = self.get_ref();
        // Nothing to cancel if the request isn't initialized yet.
        // We may inspect src here as src is only set by `init`, and `init` and `cancel_request`
        // must only be called by the owner of the PageRequest (`!Sync`), providing implicit
        // serialization.
        // SAFETY: See above; only the owning thread accesses `src`.
        let Some(src) = (unsafe { &*this.src.get() }) else {
            return;
        };
        src.cancel_request(this);
    }
}

/// Interface for providing pages to a VMO through page requests.
///
/// This is a facade for the C++ `PageProvider` interface, whose virtual methods are reached
/// through the `cpp_page_provider_*` shims in `page_source_ffi.cc`. The object is owned by C++,
/// and Rust only ever observes it through pointers handed over the FFI boundary.
#[repr(C)]
pub struct PageProvider {
    raw: Opaque<bindings::PageProvider>,
    phantom: PhantomData<PhantomPinned>,
}

impl HasRefCount for PageProvider {
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider`, and
        // `cpp_page_provider_get_ref_counted` returns a valid pointer to its `fbl::RefCounted`
        // base.
        unsafe {
            &*(bindings::cpp_page_provider_get_ref_counted(self.as_raw()).cast::<RefCounted>())
        }
    }
}

// SAFETY: `PageProvider` is a C++ `fbl::RefCounted` object freed via `cpp_page_provider_free`.
unsafe impl Recyclable for PageProvider {
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: Caller guarantees `ptr` points to a valid `PageProvider` with zero remaining
        // references.
        unsafe {
            bindings::cpp_page_provider_free(ptr.as_ptr().cast());
        }
    }
}

// SAFETY: `PageProvider` implementations are internally synchronized and may be used from any
// thread.
unsafe impl Send for PageProvider {}
// SAFETY: See the `Send` implementation above.
unsafe impl Sync for PageProvider {}

impl PageProvider {
    /// Domain-specific conversion: returns raw pointer for `PageProvider`.
    fn as_raw(&self) -> *mut bindings::PageProvider {
        self.raw.get()
    }

    // Methods a PageProvider implementation can use to retrieve fields from a PageRequest.

    /// Returns the type of `request`.
    ///
    /// `request` must currently be owned by the calling provider: `PageSource` hands a request to
    /// the provider before calling `send_async_request(request)` (or `swap_async_request` with it
    /// as `new_req`) and takes it back once `clear_async_request(request)` (or `swap_async_request`
    /// with it as `old`) returns, and only modifies a request's type, offset, length and VMO id
    /// while it is not owned by the provider.
    #[inline]
    pub fn get_request_type(request: &PageRequest) -> PageRequestType {
        debug_assert!(request.is_provider_owned());
        request.cur_type()
    }

    /// Returns the offset of `request`, which must currently be owned by the calling provider (see
    /// [`PageProvider::get_request_type`]).
    #[inline]
    pub fn get_request_offset(request: &PageRequest) -> u64 {
        debug_assert!(request.is_provider_owned());
        request.cur_offset()
    }

    /// Returns the length of `request`, which must currently be owned by the calling provider (see
    /// [`PageProvider::get_request_type`]).
    #[inline]
    pub fn get_request_len(request: &PageRequest) -> u64 {
        debug_assert!(request.is_provider_owned());
        request.cur_len()
    }

    /// Returns the id of the VMO `request` is for. `request` must currently be owned by the calling
    /// provider (see [`PageProvider::get_request_type`]).
    #[inline]
    pub fn get_request_vmo_id(request: &PageRequest) -> u64 {
        debug_assert!(request.is_provider_owned());
        request.vmo_id.load(Ordering::Relaxed)
    }

    /// The returned properties can assumed to be const and never change. As such the caller may
    /// cache them.
    ///
    /// # Safety
    ///
    /// `self` must point to a valid, initialized C++ `PageProvider`.
    unsafe fn properties(&self) -> PageSourceProperties {
        // SAFETY: The caller guarantees `self.as_raw()` points to an initialized C++
        // `PageProvider`.
        unsafe { bindings::cpp_page_provider_properties(self.as_raw()) }
    }

    /// Informs the backing source of a page request. The provider has ownership
    /// of `request` until the async request is cancelled.
    ///
    /// # Safety
    ///
    /// This provider must not have been detached or closed. `request` must not currently be in any
    /// `PageProviderTag` list, and must remain valid and owned by this provider until cleared or
    /// swapped out (or the provider closes).
    unsafe fn send_async_request(&self, request: &PageRequest) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and the caller
        // upholds the ownership and lifetime invariants for `request`.
        unsafe { bindings::cpp_page_provider_send_async_request(self.as_raw(), request.as_raw()) }
    }

    /// Informs the backing source that a page request has been fulfilled. This
    /// must be called for all requests that are raised.
    ///
    /// # Safety
    ///
    /// This provider must not have been closed, and `request` must currently be owned by this
    /// provider (sent via `send_async_request` or `swap_async_request` and not yet cleared).
    unsafe fn clear_async_request(&self, request: &PageRequest) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and the caller
        // guarantees `request` is currently owned by this provider.
        unsafe { bindings::cpp_page_provider_clear_async_request(self.as_raw(), request.as_raw()) }
    }

    /// Swaps the backing memory for a request. Assumes that `old`
    /// and `new_req` have the same type, offset, and length.
    ///
    /// # Safety
    ///
    /// This provider must not have been closed. `old` must currently be owned by this provider,
    /// and `new_req` must not be in any `PageProviderTag` list and must remain valid and owned by
    /// this provider until cleared or swapped out (or the provider closes).
    unsafe fn swap_async_request(&self, old: &PageRequest, new_req: &PageRequest) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and the caller
        // upholds the ownership and lifetime invariants for `old` and `new_req`.
        unsafe {
            bindings::cpp_page_provider_swap_async_request(
                self.as_raw(),
                old.as_raw(),
                new_req.as_raw(),
            )
        }
    }

    /// Frees `pages`.
    /// This will assert unless is_handling_free is true, in which case this will make the pages
    /// FREE.
    ///
    /// # Safety
    ///
    /// `pages` must contain valid pages owned by this provider's VMO, the owning `VmCowPages` must
    /// be live, and the current thread must hold the associated `PageSource`'s `paged_vmo_lock`.
    unsafe fn free_pages(&self, pages: Pin<&mut VmPageDoublyLinkedList>) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider`, and the caller
        // guarantees `pages` contains valid pages owned by this provider's VMO with the VMO live
        // and `paged_vmo_lock` held. `PageProvider::FreePages` implementations only move elements
        // out of the list in place and never move the list header.
        unsafe {
            bindings::cpp_page_provider_free_pages(
                self.as_raw(),
                core::ptr::from_mut(pages.get_unchecked_mut()).cast(),
            )
        }
    }

    /// For asserting purposes only.  This gives the PageProvider a chance to check that a page is
    /// consistent with any rules the PageProvider has regarding which pages can go where in the
    /// VmCowPages. PhysicalPageProvider implements this to verify that page at offset makes sense
    /// with respect to `phys_base`, since VmCowPages can't do that on its own due to lack of
    /// knowledge of `phys_base` and lack of awareness of contiguous.
    ///
    /// # Safety
    ///
    /// `page` must point to a valid `vm_page_t`.
    unsafe fn debug_is_page_ok(&self, page: *mut vm_page_t, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider`, and the caller
        // guarantees `page` is valid.
        unsafe { bindings::cpp_page_provider_debug_is_page_ok(self.as_raw(), page, offset) }
    }

    /// `on_detach` is called once no more calls to `send_async_request` will be made. It will be
    /// called before `on_close` and will only be called once.
    ///
    /// # Safety
    ///
    /// Must be called at most once, before `on_close`, after all `send_async_request` calls have
    /// completed, and with the owning `VmCowPages` live (and its lock held if initialized).
    unsafe fn on_detach(&self) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and the caller
        // upholds the `on_detach` lifecycle invariants.
        unsafe { bindings::cpp_page_provider_on_detach(self.as_raw()) }
    }

    /// After `on_close` is called, no more calls will be made except for `wait_on_event`.
    ///
    /// # Safety
    ///
    /// Must be called at most once, after `on_detach`, and no provider methods other than
    /// `wait_on_event`, `dump`, `get_koid`, or `get_debug_name` may be called afterwards.
    unsafe fn on_close(&self) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and the caller
        // upholds the `on_close` lifecycle invariants.
        unsafe { bindings::cpp_page_provider_on_close(self.as_raw()) }
    }

    /// Waits on an `event` associated with a page request. The waiting thread can return early
    /// from the wait due to a suspend signal only if `suspendable` is true.
    ///
    /// # Safety
    ///
    /// `event` must be a valid `Event` associated with a request on this provider, and the caller
    /// must be in a blockable thread context with no locks held.
    unsafe fn wait_on_event(&self, event: &Event, suspendable: bool) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and `event` is a
        // live `Event`.
        Status::ok(unsafe {
            bindings::cpp_page_provider_wait_on_event(self.as_raw(), event.as_raw(), suspendable)
        })
    }

    /// Dumps relevant state for debugging purposes. The `max_items` parameter should be used to
    /// cap the number of elements printed from any kind of variable sized list to prevent spam.
    ///
    /// # Safety
    ///
    /// `self` must point to a valid, initialized C++ `PageProvider`.
    unsafe fn dump(&self, depth: u32, max_items: u32) {
        // SAFETY: The caller guarantees `self.as_raw()` points to an initialized C++
        // `PageProvider`.
        unsafe { bindings::cpp_page_provider_dump(self.as_raw(), depth, max_items) }
    }

    /// Retrieves the koid for this page provider, if it has one.
    ///
    /// # Safety
    ///
    /// `self` must point to a valid, initialized C++ `PageProvider`.
    unsafe fn get_koid(&self) -> Option<zx_koid_t> {
        // SAFETY: The caller guarantees `self.as_raw()` points to an initialized C++
        // `PageProvider`.
        let koid = unsafe { bindings::cpp_page_provider_get_koid(self.as_raw()) };
        if koid == ZX_KOID_INVALID { None } else { Some(koid) }
    }

    /// Retrieves the debug name for this page provider, if it has one.
    ///
    /// # Safety
    ///
    /// `self` must point to a valid, initialized C++ `PageProvider`.
    unsafe fn get_debug_name(&self, name: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageProvider` and `name` is a
        // valid, writable buffer of `ZX_MAX_NAME_LEN` bytes. `[u8; ZX_MAX_NAME_LEN]` and
        // `[c_char; ZX_MAX_NAME_LEN]` have identical layout.
        unsafe {
            bindings::cpp_page_provider_get_debug_name(
                self.as_raw(),
                core::ptr::from_mut(name).cast(),
            )
        }
    }
}

/// How a new page request range relates to an existing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuationType {
    NotContinuation,
    SameRequest,
    SameSource,
}

/// The type of the trees of outstanding requests.
type RequestTree = WavlTree<u64, NonNull<PageRequest>>;

/// A page source is responsible for fulfilling page requests from a VMO with backing pages.
/// The PageSource class mostly contains generic functionality around managing
/// the lifecycle of VMO page requests. The PageSource contains an reference to a PageProvider
/// implementation, which is responsible for actually providing the pages. (E.g. for VMOs backed by
/// a userspace pager, the PageProvider is a PagerProxy instance which talks to the userspace pager
/// service.)
///
/// The synchronous fulfillment of requests is fairly straightforward, with direct calls
/// from the vm object to the PageSource to the PageProvider.
///
/// For asynchronous requests, the lifecycle is as follows:
///   1) A vm object requests a page with `PageSource::get_pages`.
///   2) PageSource starts tracking the request's PageRequest and then
///      forwards the request to `PageProvider::send_async_request`.
///   3) The caller waits for the request with `PageRequest::wait`.
///   4) At some point, whatever is backing the PageProvider provides pages
///      to the vm object (e.g. with VmObjectPaged::SupplyPages).
///   5) The vm object calls `PageSource::on_pages_supplied`, which signals
///      any PageRequests that have been fulfilled.
///   6) The caller wakes up and queries the vm object again, by which
///      point the requested page will be present.
///
/// For a contiguous VMO requesting physical pages back, step 4 above just frees the pages from
/// some other use, and step 6 finds the requested pages available, but not yet present in the VMO,
/// similar to what can happen with a normal PageProvider where pages can be read and then
/// decommitted before the caller queries the vm object again.
///
/// Object which provides pages to a vm_object.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PageSource {
    canary: Canary<{ fbl::magic(b"VMPS") }>,

    /// Lock used by the VMO to perform synchronization across its hierarchy. This lock does not
    /// strictly belong here, but this is a convenient and efficient place to put it.
    /// See VmCowPages::DeferredOps for more.
    #[pin]
    #[mutex]
    paged_vmo_mutex: KMutex<TrackedMutex>,

    #[pin]
    #[mutex]
    page_source_mtx: KMutex<TrackedMutex>,

    #[guarded_by(page_source_mtx)]
    detached: bool,

    #[guarded_by(page_source_mtx)]
    closed: bool,

    /// We cache the immutable `page_provider.properties()` to avoid many virtual calls.
    page_provider_properties: PageSourceProperties,

    /// Trees of outstanding requests which have been sent to the PageProvider, one for each
    /// supported page request type. These lists are keyed by the end offset of the requests (not
    /// the start offsets).
    ///
    /// Guarded by `page_source_mtx`. This uses `SourceLocked` instead of `#[guarded_by]` so that a
    /// cursor into a tree can be held while the `page_source_mtx` protected fields of the
    /// contained requests are accessed.
    #[pin]
    outstanding_request_trees: SourceLocked<[RequestTree; PageRequestType::COUNT]>,

    /// PageProvider instance that will provide pages asynchronously (e.g. a userspace pager, see
    /// PagerProxy for details).
    page_provider: RefPtr<PageProvider>,
}

// SAFETY: All mutable state of a `PageSource` is protected by its `page_source_mtx`, and the
// `PageProvider` it references is internally synchronized.
unsafe impl Send for PageSource {}
// SAFETY: See the `Send` implementation above.
unsafe impl Sync for PageSource {}

#[pinned_drop]
impl PinnedDrop for PageSource {
    fn drop(self: Pin<&mut Self>) {
        debug::ltracef!("{:p}\n", self.as_ref().get_ref());
        let this = self.project();
        debug_assert!(*this.detached.get_inner_mut());
        debug_assert!(*this.closed.get_inner_mut());
    }
}

impl PageSource {
    /// Creates a `PageSource` that forwards page requests to `page_provider`.
    pub fn create(page_provider: RefPtr<PageProvider>) -> Result<RefPtr<Self>, Status> {
        // SAFETY: `page_provider` is a valid `RefPtr` to an initialized `PageProvider`.
        let page_provider_properties = unsafe { page_provider.properties() };
        let src = fbl::pin_make_ref_counted!(Self {
            canary: Canary::new(),
            paged_vmo_mutex <- KMutex::init(),
            page_source_mtx <- KMutex::init(),
            detached: false.into(),
            closed: false.into(),
            page_provider_properties,
            outstanding_request_trees <- SourceLocked::pin_init(pin_init::pin_init_array_from_fn(
                |_| RequestTree::new()
            )),
            page_provider,
        })
        .map_err(|_: kalloc::AllocError| Status::NO_MEMORY)?;
        debug::ltracef!("{:p}\n", RefPtr::as_ptr(&src));
        Ok(src)
    }

    /// Returns the tree of outstanding requests of type `ty`.
    ///
    /// # Safety
    ///
    /// No mutable reference to the trees (see `outstanding_requests_mut`) may be live for the
    /// lifetime of the returned reference.
    unsafe fn outstanding_requests<'a>(
        &'a self,
        token: &'a LockToken<'_, PageSourceMtxClass>,
        ty: PageRequestType,
    ) -> &'a RequestTree {
        // SAFETY: `token` proves `page_source_mtx` is held, and the caller guarantees there are no
        // live mutable references to the trees.
        unsafe { &self.outstanding_request_trees.get_unchecked(token)[ty as usize] }
    }

    /// Returns the tree of outstanding requests of type `ty`, for mutating it.
    ///
    /// # Safety
    ///
    /// No other reference to the trees may be live for the lifetime of the returned reference.
    #[allow(clippy::mut_from_ref)]
    unsafe fn outstanding_requests_mut<'a>(
        &'a self,
        token: &'a LockToken<'_, PageSourceMtxClass>,
        ty: PageRequestType,
    ) -> Pin<&'a mut RequestTree> {
        // SAFETY: `token` proves `page_source_mtx` is held, and the caller guarantees there are no
        // other live references to the trees. `outstanding_request_trees` is a `#[pin]` field of
        // `PageSource`, which is only constructed in place (by `pin_make_ref_counted!` in
        // `PageSource::create`) and never moved, and elements of a pinned array are never moved
        // either, so the tree stays pinned for as long as it is alive.
        unsafe {
            Pin::new_unchecked(
                &mut self.outstanding_request_trees.get_mut_unchecked(token)[ty as usize],
            )
        }
    }

    /// Returns the list of requests overlapping `request`.
    ///
    /// # Safety
    ///
    /// `request` must be associated with this source, and no other reference to its overlap list
    /// may be live for the lifetime of the returned reference.
    #[allow(clippy::mut_from_ref)]
    unsafe fn overlap_of<'a>(
        request: &'a PageRequest,
        token: &'a LockToken<'_, PageSourceMtxClass>,
    ) -> Pin<&'a mut DoublyLinkedList<NonNull<PageRequest>, PageSourceTag>> {
        // SAFETY: `token` proves the lock of the source `request` is associated with is held, and
        // the caller guarantees there are no other live references to the list. `overlap` is
        // structurally pinned within the pinned `request`.
        unsafe { Pin::new_unchecked(request.overlap.get_mut_unchecked(token)) }
    }

    /// Domain-specific conversion: constructs a `&PageSource` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to an initialized `PageSource` that remains live for
    /// the lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::PageSource) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: The C++ `PageSource` shell is the storage of the Rust `PageSource` at offset 0,
        // and the caller guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Returns whether this `PageSource`'s `paged_vmo_lock` is held by the current thread.
    ///
    /// Intended for `debug_assert!`s that both document and check a caller's obligation to hold
    /// the `paged_vmo_lock` across a call.
    pub fn paged_vmo_lock_is_held(&self) -> bool {
        self.paged_vmo_mutex.raw_mutex().is_held()
    }

    /// Retrieves the debug name of the backing page provider, if any.
    fn get_provider_debug_name(&self, name: &mut [u8; ZX_MAX_NAME_LEN]) {
        // SAFETY: `self.page_provider` is a valid, initialized `PageProvider`.
        unsafe { self.page_provider.get_debug_name(name) };
    }

    /// Sends a request to the backing source to provide the requested page at `offset`.
    ///
    /// Returns `Err(Status::NOT_FOUND)` if the request cannot be fulfilled.
    /// Returns `Err(Status::SHOULD_WAIT)` if the request will be asynchronously fulfilled and the
    /// caller should wait on `req`.
    #[inline]
    pub fn get_pages(
        &self,
        offset: u64,
        len: u64,
        req: Pin<&PageRequest>,
        vmo_debug_info: VmoDebugInfo,
    ) -> Result<(), Status> {
        self.populate_request(req, offset, len, vmo_debug_info, PageRequestType::Read)
    }

    /// Frees `pages`.
    ///
    /// # Safety
    ///
    /// `pages` must contain valid pages owned by this page source's VMO, the owning `VmCowPages`
    /// must be live, and the current thread must hold [`Self::paged_vmo_lock`].
    #[inline]
    pub unsafe fn free_pages(&self, pages: Pin<&mut VmPageDoublyLinkedList>) {
        // SAFETY: The caller guarantees `pages` contains valid pages owned by this page source's
        // VMO (which is the VMO of its provider), that the owning `VmCowPages` is live, and that
        // the current thread holds `paged_vmo_lock`.
        unsafe { self.page_provider.free_pages(pages) };
    }

    /// For asserting purposes only.  This gives the PageProvider a chance to check that a page is
    /// consistent with any rules the PageProvider has regarding which pages can go where in the
    /// VmCowPages. PhysicalPageProvider implements this to verify that page at offset makes sense
    /// with respect to `phys_base`, since VmCowPages can't do that on its own due to lack of
    /// knowledge of `phys_base` and lack of awareness of contiguous.
    ///
    /// # Safety
    ///
    /// `page` must point to a live `VmPage`.
    #[inline]
    pub unsafe fn debug_is_page_ok(&self, page: VmPagePtr, offset: u64) -> bool {
        // SAFETY: Per `# Safety` `page.as_ffi()` is a live `vm_page_t*` that the provider only
        // reads.
        unsafe { self.page_provider.debug_is_page_ok(page.as_ffi(), offset) }
    }

    /// Updates the request tracking metadata to account for pages [offset, offset + len) having
    /// been supplied to the owning vmo.
    ///
    /// Note that the range [offset, offset + len) should not have been previously supplied. The
    /// page request tracking in PageSource works by tracking only a fulfilled length, and not
    /// exact fulfilled offsets, to save on memory required for metadata. So in order to prevent
    /// over-accounting errors, the caller must ensure that they are only calling
    /// `on_pages_supplied` for newly supplied ranges.
    /// TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
    /// offsets with a bitmap. Might require capping the max permissible length of a page request.
    pub fn on_pages_supplied(&self, offset: u64, len: u64) {
        ksync::lock!(let guard = self.lock_page_source_mtx());
        self.resolve_requests_locked(guard.token(), PageRequestType::Read, offset, len, ZX_OK);
    }

    /// Fails outstanding page requests in the range [offset, offset + len). Events associated with
    /// the failed page requests are signaled with the `error_status`, and any waiting threads are
    /// unblocked.
    pub fn on_pages_failed(&self, offset: u64, len: u64, error_status: Status) {
        self.canary.assert();
        debug::ltracef_level!(2, "{:p} offset {:x}, len {:x}\n", self, offset, len);

        debug_assert!(PageSource::is_valid_internal_failure_code(error_status));

        let (_end, overflow) = offset.overflowing_add(len);
        debug_assert!(!overflow); // vmobject should have already validated overflow

        ksync::lock!(let guard = self.lock_page_source_mtx());
        if *guard.detached() {
            return;
        }

        for ty in PAGE_REQUEST_TYPES {
            if !self.supports_page_request_type(ty) {
                continue;
            }
            self.resolve_requests_locked(guard.token(), ty, offset, len, error_status.into_raw());
        }
    }

    /// Returns true if `error_status` is a valid ZX_PAGER_OP_FAIL failure error code (input,
    /// specified by user mode pager).  These codes can be used with `on_pages_failed` (and so can
    /// any failure codes for which `is_valid_internal_failure_code` returns true).
    ///
    /// Not every error code is supported, since these errors can get returned via a zx_vmo_read()
    /// or a zx_vmo_op_range(), if those calls resulted in a page fault.  So the `error_status`
    /// should be a supported return error code for those syscalls _and_ be an error code that we
    /// want to be supported for the user mode pager to specify via ZX_PAGER_OP_FAIL.  Currently,
    /// `is_valid_external_failure_code(Status::NO_MEMORY)` returns false, as we don't want
    /// ZX_ERR_NO_MEMORY to be specified via ZX_PAGER_OP_FAIL (at least so far).
    pub fn is_valid_external_failure_code(error_status: Status) -> bool {
        matches!(
            error_status,
            Status::IO
                | Status::IO_DATA_INTEGRITY
                | Status::BAD_STATE
                | Status::NO_SPACE
                | Status::BUFFER_TOO_SMALL
        )
    }

    /// Returns true if `error_status` is a valid provider failure error code, which can be used
    /// with `on_pages_failed`.
    ///
    /// This returns true for every error code that `is_valid_external_failure_code` returns true
    /// for, plus any additional error codes that are valid as an internal PageProvider status but
    /// not valid for ZX_PAGER_OP_FAIL.
    ///
    /// `Status::NO_MEMORY` will return true, unlike
    /// `is_valid_external_failure_code(Status::NO_MEMORY)` which returns false.
    ///
    /// Not every error code is supported, since these errors can get returned via a zx_vmo_read()
    /// or a zx_vmo_op_range(), if those calls resulted in a page fault.  So the `error_status`
    /// should be a supported return error code for those syscalls.  An error code need not be
    /// specifiable via ZX_PAGER_OP_FAIL for this function to return true.
    pub fn is_valid_internal_failure_code(error_status: Status) -> bool {
        error_status == Status::NO_MEMORY || Self::is_valid_external_failure_code(error_status)
    }

    /// Returns whether the source supports requests of type `ty`.
    #[inline]
    fn supports_page_request_type(&self, ty: PageRequestType) -> bool {
        self.properties().supports_request_type[ty as usize]
    }

    /// Whether transitions from clean to dirty should be trapped.
    #[inline]
    pub fn should_trap_dirty_transitions(&self) -> bool {
        self.supports_page_request_type(PageRequestType::Dirty)
    }

    /// Request the page provider for clean pages in the range [offset, offset + len) to become
    /// dirty, in order for a write to proceed. Returns `Err(Status::SHOULD_WAIT)` if the request
    /// will be asynchronously fulfilled; the caller should wait on `request`. Depending on the
    /// state of pages in the range, the `request` might be generated for a range that is a subset
    /// of [offset, offset + len).
    #[inline]
    pub fn request_dirty_transition(
        &self,
        request: Pin<&PageRequest>,
        offset: u64,
        len: u64,
        vmo_debug_info: VmoDebugInfo,
    ) -> Result<(), Status> {
        self.populate_request(request, offset, len, vmo_debug_info, PageRequestType::Dirty)
    }

    /// Updates the request tracking metadata to account for pages [offset, offset + len) having
    /// been dirtied in the owning VMO.
    ///
    /// Note that the range [offset, offset + len) should not have been previously dirtied. The
    /// page request tracking in PageSource works by tracking only a fulfilled length, and not
    /// exact fulfilled offsets, to save on memory required for metadata. So in order to prevent
    /// over-accounting errors, the caller must ensure that they are only calling `on_pages_dirtied`
    /// for newly dirtied ranges.
    /// TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
    /// offsets with a bitmap. Might require capping the max permissible length of a page request.
    pub fn on_pages_dirtied(&self, offset: u64, len: u64) {
        ksync::lock!(let guard = self.lock_page_source_mtx());
        self.resolve_requests_locked(guard.token(), PageRequestType::Dirty, offset, len, ZX_OK);
    }

    /// Detaches the source from the VMO. All future calls into the page source will fail. All
    /// pending read transactions are aborted. Pending flush transactions will still
    /// be serviced.
    pub fn detach(&self) {
        self.canary.assert();
        debug::ltracef!("{:p}\n", self);
        ksync::lock!(let mut guard = self.lock_page_source_mtx());
        if *guard.detached() {
            return;
        }

        *guard.as_mut().detached_mut() = true;
        let token = guard.token();

        // Cancel all requests except writebacks, which can be completed after detach.
        for ty in PAGE_REQUEST_TYPES {
            if ty == PageRequestType::Writeback || !self.supports_page_request_type(ty) {
                continue;
            }
            // SAFETY: No other reference to the trees is live.
            while !unsafe { self.outstanding_requests(token, ty) }.is_empty() {
                // SAFETY: No other reference to the trees is live.
                let req = unsafe { self.outstanding_requests_mut(token, ty) }.pop_front().unwrap();
                {
                    // SAFETY: Requests in the tree are live, as their owners must take this lock
                    // to cancel them before they are destroyed.
                    let req_ref = unsafe { req.as_ref() };
                    debug::ltracef!(
                        "dropping request with offset {:x} len {:x}\n",
                        req_ref.cur_offset(),
                        req_ref.cur_len()
                    );
                }

                // Tell the clients the request is complete - they'll fail when they
                // reattempt the page request for the same pages after failing this time.
                self.complete_request_locked(token, req);
            }
        }

        // No writebacks supported yet.
        // SAFETY: No other reference to the trees is live.
        debug_assert!(
            unsafe { self.outstanding_requests(token, PageRequestType::Writeback) }.is_empty()
        );

        // SAFETY: `detached` transitioned from false to true under `page_source_mtx`, so
        // `on_detach` is called at most once, before `on_close`, and no more `send_async_request`
        // calls will be made.
        unsafe { self.page_provider.on_detach() };
    }

    /// Closes the source. Will call `detach()` if the source is not already detached. All pending
    /// transactions will be aborted and all future calls will fail.
    pub fn close(&self) {
        self.canary.assert();
        debug::ltracef!("{:p}\n", self);
        // TODO: Close will have more meaning once writeback is implemented

        // This will be a no-op if the page source has already been detached.
        self.detach();

        ksync::lock!(let mut guard = self.lock_page_source_mtx());
        if *guard.closed() {
            return;
        }

        *guard.as_mut().closed_mut() = true;
        // SAFETY: `self.detach()` was called above, and `closed` transitioned from false to true
        // under `page_source_mtx`, so `on_close` is called at most once after `on_detach`.
        unsafe { self.page_provider.on_close() };
    }

    /// The returned properties can assumed to be const and never change. As such the caller may
    /// cache them.
    #[inline]
    pub fn properties(&self) -> PageSourceProperties {
        self.page_provider_properties
    }

    /// Prints state of the page source and any pending requests. The maximum number of requests
    /// printed is capped by `max_items`.
    pub fn dump(&self, depth: u32, max_items: u32) {
        self.dump_self(depth, max_items);
        // SAFETY: `self.page_provider` is a valid, initialized `PageProvider`.
        unsafe { self.page_provider.dump(depth, max_items) };
    }

    /// Similar to `dump`, but only dumps information about this exact object, and will not forward
    /// the `dump` request to the related PageProvider.
    pub fn dump_self(&self, depth: u32, max_items: u32) {
        ksync::lock!(let guard = self.lock_page_source_mtx());
        let token = guard.token();
        let mut printer = dump::DepthPrinter::console(depth as usize);
        dump::emit!(
            printer,
            "page_source {:p} detached {} closed {}",
            self,
            u8::from(*guard.detached()),
            u8::from(*guard.closed())
        );
        for ty in PAGE_REQUEST_TYPES {
            printer.begin_list(max_items as usize);
            // SAFETY: No mutable reference to the trees is live.
            let trees = unsafe { self.outstanding_request_trees.get_unchecked(token) };
            for req in trees[ty as usize].iter() {
                let name = req.vmo_name.get(token);
                // SAFETY: No mutable reference to the overlap list is live.
                let overlap = unsafe { req.overlap.get_unchecked(token) };
                dump::emit!(
                    printer,
                    "  vmo <{}>/k{} {} req [0x{:x}, 0x{:x}) pending 0x{:x} overlap {} {}",
                    name_prefix(zerocopy::IntoBytes::as_bytes(&name)),
                    req.vmo_id.load(Ordering::Relaxed),
                    ty,
                    req.cur_offset(),
                    req.get_end(),
                    req.pending_size.get(token),
                    overlap.iter().count(),
                    if req.is_provider_owned() { "[sent]" } else { "" }
                );
            }
            printer.end_list();
        }
    }

    /// Returns whether the source has been detached.
    pub fn is_detached(&self) -> bool {
        ksync::lock!(let guard = self.lock_page_source_mtx());
        *guard.detached()
    }

    /// Method for the VmCowPages to retrieve the lock for paged VMOs.
    /// See VmCowPages::DeferredOps.
    #[inline]
    pub fn paged_vmo_lock(&self) -> &KMutex<PagedVmoMutexClass, TrackedMutex> {
        &self.paged_vmo_mutex
    }

    /// Retrieves any koid for the linked page provider.
    #[inline]
    pub fn get_provider_koid(&self) -> Option<zx_koid_t> {
        // SAFETY: `self.page_provider` is a valid, initialized `PageProvider`.
        unsafe { self.page_provider.get_koid() }
    }

    /// Helper to perform early waking on a request and any overlapping requests. The provided
    /// range should be in local request space, and this method is only valid to be called if
    /// `request.wake_offset == req_start`.
    fn early_wake_request_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        request: &PageRequest,
        req_start: u64,
        req_end: u64,
    ) {
        debug_assert!(req_end >= req_start);
        debug_assert!(req_start == request.wake_offset.get(token));
        // By default set the wake_offset to the end of the supplied range so the caller can just
        // wait again. In practice they are likely to call `get_pages` first, which will
        // reinitialize wake_offset to what is actually the next desired offset, which may or may
        // not be the same as what is set here.
        request.wake_offset.set(token, req_end);
        request.event.signal_result(Status::ok(request.complete_status.get(token)));
        // For simplicity convert the request relative range back into a provider (aka VMO) range.
        let provider_start = req_start.wrapping_add(request.cur_offset());
        let provider_end = req_end.wrapping_add(request.cur_offset());
        // SAFETY: `request` is associated with this source and no mutable reference to its overlap
        // list is live.
        for overlap in unsafe { request.overlap.get_unchecked(token) }.iter() {
            if !overlap.is_early_wake() {
                continue;
            }
            // If the parent range has processed beyond the overlap wake offset then this is most
            // likely a sign of pages having not been linearly supplied, so want to wake up this
            // request anyway.
            if provider_start > overlap.cur_offset()
                && provider_start - overlap.cur_offset() > overlap.wake_offset.get(token)
            {
                // In the case that something unusual has happened we do not want to keep on
                // waking up the request for any future completions, since it is waiting for
                // wake_offset and either it already got supplied and we missed it, which will get
                // fixed by doing this signal, or it's not been supplied and this (and any future)
                // signal is a waste. To prevent future wakes we set the wake_offset to a large so
                // that we do not continuously signal as the parent request is given more content.
                // The largest obvious safe value, i.e. would not overflow anywhere, is the end of
                // the parent request range.
                overlap.wake_offset.set(token, request.cur_len());
                overlap.event.signal_result(Status::ok(overlap.complete_status.get(token)));
                continue;
            }
            // If there is otherwise no overlap, then can skip.
            if !overlap.range_overlaps(provider_start, provider_end) {
                continue;
            }
            // Get the overlapping portion and see if it intersects with the wake_offset.
            let (overlap_start, overlap_end) =
                overlap.trim_range_to_request_space(provider_start, provider_end);
            let wake_offset = overlap.wake_offset.get(token);
            if overlap_start <= wake_offset && overlap_end > wake_offset {
                overlap.wake_offset.set(token, overlap_end);
                overlap.event.signal_result(Status::ok(overlap.complete_status.get(token)));
            }
        }
    }

    /// Helper that updates request tracking metadata to resolve requests of `ty` in the range
    /// [offset, offset + len).
    fn resolve_requests_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        ty: PageRequestType,
        offset: u64,
        len: u64,
        error_status: zx_status_t,
    ) {
        self.canary.assert();
        debug::ltracef_level!(2, "{:p} offset {:x}, len {:x}\n", self, offset, len);
        let (end, overflow) = offset.overflowing_add(len);
        debug_assert!(!overflow); // vmobject should have already validated overflow
        debug_assert!((ty as usize) < PageRequestType::COUNT);

        // SAFETY: `token` proves that this source's `page_source_mtx` is held.
        if *unsafe { self.guard_page_source_mtx(token) }.detached() {
            return;
        }

        // The first possible request we could fulfill is the one with the smallest
        // end address that is greater than offset. Then keep looking as long as the
        // target request's start offset is less than the end.
        // SAFETY: No other reference to the trees is live while the cursor exists; the helpers
        // called below only access request fields and overlap lists.
        let tree = unsafe { self.outstanding_requests_mut(token, ty) };
        let mut start = tree.upper_bound_mut(&offset);
        while let Some(cur) = start.get().map(NonNull::from) {
            // SAFETY: Requests in the tree are live, as their owners must take this lock to cancel
            // them before they are destroyed.
            let cur = unsafe { cur.as_ref() };
            if cur.cur_offset() >= end {
                break;
            }

            // Because of upper_bound and our loop condition we know the range partially overlaps
            // this request.
            debug_assert!(cur.range_overlaps(offset, end));

            // Calculate how many pages were resolved in this request by finding the start and
            // end offsets of the operation in this request.
            let (req_offset, req_end) = cur.trim_range_to_request_space(offset, end);

            if error_status != ZX_OK {
                core::hint::cold_path();
                if req_offset == 0 || req_offset == cur.wake_offset.get(token) {
                    cur.complete_status.set(token, error_status);
                }
                // SAFETY: `cur` is associated with this source and no mutable reference to its
                // overlap list is live.
                for overlap in unsafe { cur.overlap.get_unchecked(token) }.iter() {
                    // If there is otherwise no overlap, then can skip.
                    if !overlap.range_overlaps(offset, end) {
                        continue;
                    }
                    let (overlap_start, _overlap_end) =
                        overlap.trim_range_to_request_space(offset, end);
                    if overlap_start == 0 || overlap_start == overlap.wake_offset.get(token) {
                        overlap.complete_status.set(token, error_status);
                    }
                }
            }

            let fulfill = req_end - req_offset;
            let pending_size = cur.pending_size.get(token);

            // If we're not done, continue to the next request.
            if fulfill < pending_size {
                // Only Signal if the offset being supplied is exactly at the wake_offset. The
                // wake_offset is the next one that the caller wants, and so waking up for anything
                // before this is pointless. In the case where the page source supplies this offset
                // last it does mean we will still block until the full request is provided.
                if req_offset == cur.wake_offset.get(token) {
                    self.early_wake_request_locked(token, cur, req_offset, req_end);
                }
                cur.pending_size.set(token, pending_size - fulfill);
                start.move_next();
                continue;
            } else if fulfill > pending_size {
                // This just means that part of the request was decommitted. That's not
                // an error, but it's good to know when we're tracing.
                debug::ltracef!("{:p}, excessive page count\n", self);
            }

            debug::ltracef_level!(2, "{:p}, signaling {:x}\n", self, cur.cur_offset());

            let cur_ptr = NonNull::from(cur);
            // Notify anything waiting on this range.
            let erased = start.erase();
            debug_assert!(erased == Some(cur_ptr));
            self.complete_request_locked(token, cur_ptr);
        }
    }

    /// Helper that takes an existing request and a new request range and returns whether the new
    /// range is any kind of continuation of the existing request. This is used for a mixture of
    /// correctness validation and supporting early wake requests.
    fn request_continuation_type_locked(
        &self,
        _token: &LockToken<'_, PageSourceMtxClass>,
        request: &PageRequest,
        offset: u64,
        _len: u64,
        ty: PageRequestType,
    ) -> ContinuationType {
        // Check for obvious mismatches in initialization.
        if !request.is_initialized() {
            return ContinuationType::NotContinuation;
        }
        // SAFETY: `src` is only modified by `init` on the thread that owns the request, and this is
        // only called from `populate_request` on that thread.
        let src = unsafe { &*request.src.get() };
        if !src.as_ref().is_some_and(|src| core::ptr::eq(RefPtr::as_ptr(src), self)) {
            return ContinuationType::NotContinuation;
        }
        if request.cur_type() != ty {
            return ContinuationType::NotContinuation;
        }
        // If the start of the new range overlaps at all with the existing request then we can
        // continue using the existing request. For any portion of the new range that extends
        // beyond the existing request, this is fine as once the current request is completed that
        // range can be re-generated a new request for just that range can be made.
        let existing_offset = request.cur_offset();
        if offset >= existing_offset && offset < existing_offset.wrapping_add(request.cur_len()) {
            return ContinuationType::SameRequest;
        }
        // The new request is for a completely different range, so we cannot keep using the current
        // one. A typical cause for this would be racing with a operations on a clone that bypassed
        // the need to wait for the original request. In this case we already checked that the
        // source and type are the same above.
        ContinuationType::SameSource
    }

    /// Populates `request` for the range [offset, offset + len) of type `ty`, and forwards it to
    /// the provider.
    fn populate_request(
        &self,
        request: Pin<&PageRequest>,
        offset: u64,
        len: u64,
        vmo_debug_info: VmoDebugInfo,
        ty: PageRequestType,
    ) -> Result<(), Status> {
        self.canary.assert();
        assert!(len > 0);
        assert!(::page::is_aligned(offset as usize));
        assert!(::page::is_aligned(len as usize));

        if !self.supports_page_request_type(ty) {
            return Err(Status::NOT_SUPPORTED);
        }

        debug::ltracef_level!(2, "{:p} offset {:x} prefetch_len {:x}", self, offset, len);

        // SAFETY: `PageRequest` is `!Sync`, so only the owning thread accesses `src`. If the
        // request was previously used with a different `PageSource`, synchronize with that
        // source's `page_source_mtx` before acquiring our own lock so we never hold two
        // `PageSourceMtxClass` locks at once or race with a finishing `complete_request_locked` on
        // the previous source.
        if let Some(prev) = (unsafe { &*request.src.get() }).as_ref()
            && !core::ptr::eq(RefPtr::as_ptr(prev), self)
        {
            core::hint::cold_path();
            {
                ksync::lock!(let _prev_guard = prev.lock_page_source_mtx());
                assert!(
                    !request.is_initialized(),
                    "Request at offset {:x} len {:x} is not any kind of continuation",
                    offset,
                    len
                );
            }
            // SAFETY: `PageRequest: !Sync` so only the owning thread accesses `src`, and
            // acquiring `prev`'s lock above synchronized with any finishing completion on `prev`.
            let _ = unsafe { (*request.src.get()).take() };
        }

        ksync::lock!(let guard = self.lock_page_source_mtx());
        if *guard.detached() {
            return Err(Status::BAD_STATE);
        }
        let token = guard.token();

        if request.is_initialized() {
            core::hint::cold_path();
            // The only time we should see an already initialized request is if the request was an
            // early waking one and this new request is a valid continuation of that one, anything
            // else is a programming error.
            debug_assert!(request.is_early_wake());
            match self.request_continuation_type_locked(token, &request, offset, len, ty) {
                ContinuationType::NotContinuation => {
                    // If the request is initialized and the new request is not some kind of
                    // continuation then we consider this a hard error. Although we could just
                    // cancel the existing request and generate a new one, this case indicates a
                    // serious logic error in the page request handling code and we should not
                    // attempt to continue.
                    panic!(
                        "Request at offset {:x} len {:x} is not any kind of continuation",
                        offset, len
                    );
                }
                ContinuationType::SameRequest => {
                    debug_assert!(offset >= request.cur_offset());
                    // By default the wake_offset was previously incremented by whatever was
                    // supplied, but to accommodate a page source supply pages out of order we reset
                    // our wake offset to the next actual offset that is missing.
                    request.wake_offset.set(token, offset - request.cur_offset());
                    return Err(Status::SHOULD_WAIT);
                }
                ContinuationType::SameSource => {
                    // The requested range does not overlap the existing request, but it's for the
                    // same source so this is just a case of the original range no longer being
                    // needed and so can cancel the request and make a new one.
                    self.cancel_request_locked(token, &request);
                }
            }
        }
        self.populate_request_locked(token, request, offset, len, vmo_debug_info, ty)
    }

    /// Helper that adds the span of `len` pages at `offset` to `request` and forwards it to the
    /// provider. `request` must already be initialized, and its `PageRequestType` must be set to
    /// `ty`. `offset` must be page-aligned.
    ///
    /// This function will always return `Err(Status::SHOULD_WAIT)`.
    fn populate_request_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        request: Pin<&PageRequest>,
        offset: u64,
        len: u64,
        vmo_debug_info: VmoDebugInfo,
        ty: PageRequestType,
    ) -> Result<(), Status> {
        assert!(::page::is_aligned(offset as usize));
        assert!(len > 0);
        assert!(::page::is_aligned(len as usize));
        debug_assert!((ty as usize) < PageRequestType::COUNT);
        debug_assert!(!request.is_initialized());

        // Assert on overflow, since it means vmobject is trying to get out-of-bounds pages.
        let cur_end =
            offset.checked_add(len).expect("page request [offset, offset + len) overflow");
        debug_assert!(len >= ::page::SIZE as u64);

        request.init(token, RefPtr::from_ref(self), offset, ty, vmo_debug_info);
        let request = request.get_ref();

        // SAFETY: No mutable reference to the trees is live.
        let node = unsafe { self.outstanding_requests(token, ty) }
            .upper_bound(&offset)
            .get()
            .map(NonNull::from);
        let mut final_len = len;
        if let Some(node) = node {
            // SAFETY: Requests in the tree are live, as their owners must take this lock to cancel
            // them before they are destroyed.
            let node = unsafe { node.as_ref() };
            let node_offset = node.cur_offset();
            if offset >= node_offset && cur_end >= node.get_end() {
                // If the beginning part of this request is covered by an existing request, end the
                // request at the existing request's end and wait for that request to be resolved
                // first.
                final_len = node.get_end() - offset;
            } else if offset < node_offset && cur_end >= node_offset {
                // If offset is less than node.cur_offset(), then we end the request when we'd start
                // overlapping.
                final_len = node_offset - offset;
            }
        }
        request.len.store(final_len, Ordering::Relaxed);

        self.send_request_to_provider_locked(token, request, ty, offset, final_len);

        Err(Status::SHOULD_WAIT)
    }

    /// Sends a request to the backing source, or adds the request to the overlap list if
    /// the needed region has already been requested from the source.
    fn send_request_to_provider_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        request: &PageRequest,
        ty: PageRequestType,
        offset: u64,
        len: u64,
    ) {
        debug::ltracef_level!(2, "{:p} {:p}\n", self, request);
        debug_assert!(request.is_initialized());
        debug_assert!(self.supports_page_request_type(ty));
        // Find the node with the smallest endpoint greater than offset and then
        // check to see if offset falls within that node.
        // SAFETY: No mutable reference to the trees is live.
        let overlap = unsafe { self.outstanding_requests(token, ty) }
            .upper_bound(&offset)
            .get()
            .map(NonNull::from);
        // SAFETY: Requests in the tree are live, as their owners must take this lock to cancel
        // them before they are destroyed.
        match overlap.map(|overlap| unsafe { overlap.as_ref() }) {
            Some(overlap) if overlap.cur_offset() <= offset => {
                // `get_pages` guarantees that if offset lies in an existing node, then it is
                // completely contained in that node.
                // SAFETY: `overlap` is associated with this source, no other reference to its
                // overlap list is live, and `request` is not in any `PageSourceTag` list and
                // remains live until it is removed from the list (under this lock).
                unsafe {
                    Self::overlap_of(overlap, token).push_back_raw(NonNull::from(request));
                }
            }
            _ => {
                debug_assert!(!request.is_provider_owned());
                request.pending_size.set(token, len);

                // SAFETY: `page_source_mtx` is held and `!request.is_provider_owned()`.
                debug_assert!(!unsafe { request.in_container() });
                request.provider_owned.store(true, Ordering::Relaxed);
                // SAFETY: This source is not detached or closed, `request` is not in any
                // `PageProviderTag` list, and `PageSource` keeps `request` valid and
                // provider-owned until cleared or swapped under `page_source_mtx`.
                unsafe { self.page_provider.send_async_request(request) };
                // SAFETY: No other reference to the trees is live, `request` is not in any tree,
                // and it remains live until it is removed from the tree (under this lock).
                assert!(
                    unsafe {
                        self.outstanding_requests_mut(token, ty)
                            .insert_or_find_raw(NonNull::from(request))
                    }
                    .is_ok()
                );
            }
        }
    }

    /// Wakes up the given PageRequest and all overlapping requests.
    fn complete_request_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        request: NonNull<PageRequest>,
    ) {
        let (status, event_ptr) = {
            // SAFETY: `request` was just removed from `outstanding_requests` under
            // `page_source_mtx` while `offset != u64::MAX`, so its owner cannot destroy it
            // without first acquiring `page_source_mtx` until `offset` is set to `u64::MAX` below.
            let first_req = unsafe { request.as_ref() };
            vm_ktrace_duration!(
                1,
                "page_request_complete",
                "vmo_id" => first_req.vmo_id.load(Ordering::Relaxed),
                "offset" => first_req.cur_offset(),
                "length" => first_req.cur_len(),
                "type" => if first_req.cur_type() == PageRequestType::Read {
                    "Read"
                } else {
                    "Dirty"
                }
            );
            debug_assert!((first_req.cur_type() as usize) < PageRequestType::COUNT);
            debug_assert!(self.supports_page_request_type(first_req.cur_type()));

            // Take the request back from the provider before waking up the corresponding thread.
            // Once the request has been taken back we are also free to modify offset.
            // SAFETY: `first_req` is currently owned by `self.page_provider` and `self` is not
            // closed.
            unsafe { self.page_provider.clear_async_request(first_req) };
            first_req.provider_owned.store(false, Ordering::Relaxed);

            {
                // SAFETY: `first_req` is associated with this source and no other reference to its
                // overlap list is live.
                let mut overlap = unsafe { Self::overlap_of(first_req, token) };
                if overlap.is_empty() {
                    PGREQ_OVERLAP_0.add(1);
                } else {
                    core::hint::cold_path();
                    // Even though we're doing iter().count(), this is okay because the vast
                    // majority of page requests have no overlap and won't end up in this
                    // else-block in the first place.
                    let overlap_count = overlap.iter().count();
                    debug_assert!(overlap_count > 0);
                    match overlap_count {
                        1 => PGREQ_OVERLAP_1.add(1),
                        2 => PGREQ_OVERLAP_2.add(1),
                        3 => PGREQ_OVERLAP_3.add(1),
                        4 => PGREQ_OVERLAP_4.add(1),
                        _ => {
                            if overlap_count <= 10 {
                                PGREQ_OVERLAP_5_TO_10.add(1);
                            } else {
                                PGREQ_OVERLAP_MORE_THAN_10.add(1);
                            }
                        }
                    }
                }

                while let Some(waiter_ptr) = overlap.as_mut().pop_front() {
                    let (waiter_status, waiter_event) = {
                        // SAFETY: Requests in an overlap list are live while `offset != u64::MAX`,
                        // as their owners must take `page_source_mtx` to cancel them before
                        // destruction.
                        let waiter = unsafe { waiter_ptr.as_ref() };
                        vm_ktrace_flow_begin!(
                            1,
                            "page_request_signal",
                            waiter as *const PageRequest as usize
                        );
                        debug_assert!(!waiter.is_provider_owned());
                        let status = Status::ok(waiter.complete_status.get(token));
                        let event_ptr = core::ptr::from_ref(waiter.event.as_event());
                        waiter.offset.store(u64::MAX, Ordering::Relaxed);
                        (status, event_ptr)
                    };
                    // SAFETY: `waiter_event` points to the `Event` of `waiter_ptr`, which stays
                    // live at least until `signal_worker` releases the wait queue lock (any
                    // cancellation or drop of the request acquires `page_source_mtx`, which we
                    // hold), and no `ChainLockTransaction` is active.
                    unsafe { Event::signal_worker(waiter_event, waiter_status, None) };
                }
            }

            vm_ktrace_flow_begin!(
                1,
                "page_request_signal",
                first_req as *const PageRequest as usize
            );
            let status = Status::ok(first_req.complete_status.get(token));
            let event_ptr = core::ptr::from_ref(first_req.event.as_event());
            first_req.offset.store(u64::MAX, Ordering::Relaxed);
            (status, event_ptr)
        };
        // SAFETY: `event_ptr` points to the `Event` of `request`, which stays live at least until
        // `signal_worker` releases the wait queue lock (any cancellation or drop of the request
        // acquires `page_source_mtx`, which we hold), and no `ChainLockTransaction` is active.
        unsafe { Event::signal_worker(event_ptr, status, None) };
    }

    /// Removes `request` from any internal tracking. Called by a PageRequest if
    /// it needs to abort itself.
    fn cancel_request(&self, request: &PageRequest) {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_page_source_mtx());
        self.cancel_request_locked(guard.token(), request);
    }

    /// Removes `request` from any internal tracking while holding the source lock.
    fn cancel_request_locked(
        &self,
        token: &LockToken<'_, PageSourceMtxClass>,
        request: &PageRequest,
    ) {
        debug::ltracef!("{:p} {:x}\n", self, request.cur_offset());

        if !request.is_initialized() {
            return;
        }
        let ty = request.cur_type();
        debug_assert!(self.supports_page_request_type(ty));

        // SAFETY: `request` is associated with this source and no other reference to its overlap
        // list is live.
        let mut request_overlap = unsafe { Self::overlap_of(request, token) };
        if request.in_overlap_list(token) {
            debug::ltracef!("Overlap node\n");
            // This node is overlapping some other node, so just remove the request
            // SAFETY: No mutable reference to the trees is live.
            let main_node = unsafe { self.outstanding_requests(token, ty) }
                .upper_bound(&request.cur_offset())
                .get()
                .map(NonNull::from);
            assert!(main_node.is_some());
            // SAFETY: Requests in the tree are live, as their owners must take this lock to cancel
            // them before they are destroyed.
            let main_node = unsafe { main_node.unwrap().as_ref() };
            // SAFETY: `main_node` is associated with this source and is distinct from `request`
            // (which is in an overlap list and so not in a tree), so no other reference to its
            // overlap list is live. `request` is in that overlap list.
            let _ = unsafe { Self::overlap_of(main_node, token).erase(request) };
        } else if !request_overlap.is_empty() {
            debug::ltracef!("Outstanding with overlap\n");
            // This node is an outstanding request with overlap, so replace it with the
            // first overlap node.
            let new_node = request_overlap.as_mut().pop_front().unwrap();
            // SAFETY: Requests in an overlap list are live, as their owners must take this lock to
            // cancel them before they are destroyed.
            let new_node = unsafe { new_node.as_ref() };
            debug_assert!(!new_node.is_provider_owned());
            // SAFETY: `new_node` is associated with this source and distinct from `request`, so no
            // other reference to its overlap list is live.
            unsafe { Self::overlap_of(new_node, token) }.swap(request_overlap);
            new_node.offset.store(request.cur_offset(), Ordering::Relaxed);
            new_node.len.store(request.cur_len(), Ordering::Relaxed);
            new_node.pending_size.set(token, request.pending_size.get(token));
            debug_assert!(new_node.cur_type() == request.cur_type());

            // SAFETY: `page_source_mtx` is held and `!new_node.is_provider_owned()`.
            debug_assert!(!unsafe { new_node.in_container() });
            // SAFETY: No other reference to the trees is live, `request` is in this tree, and
            // `new_node` is not in any tree and remains live until it is removed (under this lock).
            unsafe {
                let mut tree = self.outstanding_requests_mut(token, ty);
                let _ = tree.as_mut().erase_raw(request as *const PageRequest as *mut PageRequest);
                assert!(tree.insert_or_find_raw(NonNull::from(new_node)).is_ok());
            }

            new_node.provider_owned.store(true, Ordering::Relaxed);
            // SAFETY: `self` is not closed, `request` is currently owned by `self.page_provider`,
            // and `new_node` is not in any `PageProviderTag` list and remains live and
            // provider-owned until cleared or swapped under `page_source_mtx`.
            unsafe { self.page_provider.swap_async_request(request, new_node) };
            request.provider_owned.store(false, Ordering::Relaxed);
        } else if WavlTreeContainable::<PageRequest>::get_node(request).in_container() {
            debug::ltracef!("Outstanding no overlap\n");
            // This node is an outstanding request with no overlap
            // SAFETY: No other reference to the trees is live and `request` is in this tree.
            let _ = unsafe {
                self.outstanding_requests_mut(token, ty)
                    .erase_raw(request as *const PageRequest as *mut PageRequest)
            };
            // SAFETY: `self` is not closed and `request` is currently owned by
            // `self.page_provider`.
            unsafe { self.page_provider.clear_async_request(request) };
            request.provider_owned.store(false, Ordering::Relaxed);
        }

        // Request has been cleared from the PageProvider, so we're free to modify the offset
        request.offset.store(u64::MAX, Ordering::Relaxed);
    }

    /// Ask the page source to wait on this request, typically by forwarding to the page provider.
    /// Note this gets called without a lock and so due to races the implementation needs to be
    /// tolerant of having already been detached/closed. The waiting thread can return early from
    /// the wait due to a suspend signal only if `suspendable` is true.
    fn wait_on_request(&self, request: &PageRequest, suspendable: bool) -> Result<(), Status> {
        self.canary.assert();

        // If we have been detached the request will already have been completed in `detach` and so
        // the provider should instantly wake from the event.
        // SAFETY: `request.event` is a valid `Event` associated with a request on this source, and
        // `PageRequest::wait` asserted no locks are held.
        unsafe { self.page_provider.wait_on_event(request.event.as_event(), suspendable) }
    }
}

/// Wrapper around PageRequest that performs construction on first access. This is useful when a
/// PageRequest needs to be allocated eagerly in case it is used, even if the common case is that
/// it will not be needed.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct LazyPageRequest {
    #[pin]
    request: UnsafeCell<MaybeUninit<PageRequest>>,
    /// Early wake parameter to be passed onto the PageRequest constructor.
    early_wake: bool,
    /// Whether `request` has been initialized.
    initialized: Cell<bool>,
}

// SAFETY: `LazyPageRequest` is owned by a single caller thread at a time (`!Sync`), matching
// `PageRequest`.
unsafe impl Send for LazyPageRequest {}

#[pinned_drop]
impl PinnedDrop for LazyPageRequest {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.initialized.get() {
            // Cancel the request through a shared `Pin<&PageRequest>` first while the `PageSource`
            // or `PageProvider` may still hold a `NonNull<PageRequest>` to it, so no
            // `&mut PageRequest` is formed until after the request is unlinked.
            // SAFETY: `this.initialized` is true, so `this.request` contains an initialized,
            // pinned `PageRequest`.
            unsafe {
                Pin::new_unchecked(this.request.get().as_ref_unchecked().assume_init_ref())
                    .cancel_request();
            }
            // SAFETY: `cancel_request` above removed `request` from any `PageSource` and
            // `PageProvider`, so no other references or pointers to it exist, and it is dropped in
            // place exactly once.
            unsafe { this.request.get().as_mut_unchecked().assume_init_drop() };
        }
    }
}

impl LazyPageRequest {
    /// Construct a page request that does not support early waking.
    pub fn new() -> impl PinInit<Self, Infallible> {
        Self::with_early_wake(false)
    }

    /// Construct a page request that optionally supports early waking. See PageRequest
    /// constructor.
    pub fn with_early_wake(early_wake: bool) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            request: UnsafeCell::new(MaybeUninit::uninit()),
            early_wake,
            initialized: Cell::new(false),
        })
    }

    /// Slow path that initializes the internal `PageRequest` on first access.
    #[cold]
    #[inline(never)]
    fn init_slow(self: Pin<&Self>) {
        let this = self.get_ref();
        debug_assert!(!this.initialized.get());
        // SAFETY: `initialized` is false, so no reference to the contents of `request` exists, and
        // `request` is uninitialized storage within the pinned `self` that `UnsafeCell` permits
        // writing through a shared reference, so it is valid to initialize it in place. We call
        // `PinInit::__pinned_init` directly rather than `InPlaceWrite::write_pin_init` because
        // `InPlaceWrite` is only implemented for `&'static mut MaybeUninit<T>` and `Box`.
        let Ok(()) = unsafe {
            PageRequest::with_early_wake(this.early_wake)
                .__pinned_init(this.request.get().cast::<PageRequest>())
        };
        this.initialized.set(true);
    }

    /// Initialize and return the internal PageRequest.
    pub fn get(self: Pin<&Self>) -> Pin<&PageRequest> {
        if !self.is_initialized() {
            self.init_slow();
        }
        // SAFETY: `self.request` is initialized above (if it was not already) and is structurally
        // pinned within `self`. After construction a `LazyPageRequest` is only reached through
        // `Pin<&Self>`, so no `&mut` borrow of it is formed that could invalidate the returned
        // reference or the `NonNull<PageRequest>` the `PageSource` / `PageProvider` hold while
        // the request is queued.
        unsafe {
            Pin::new_unchecked(self.get_ref().request.get().as_ref_unchecked().assume_init_ref())
        }
    }

    /// Returns the internal `PageRequest` if it has been initialized, without initializing it.
    #[inline]
    fn as_initialized(self: Pin<&Self>) -> Option<Pin<&PageRequest>> {
        let this = self.get_ref();
        if this.initialized.get() {
            // SAFETY: `this.initialized` is true, so `this.request` contains an initialized
            // `PageRequest` structurally pinned within `self`.
            Some(unsafe {
                Pin::new_unchecked(this.request.get().as_ref_unchecked().assume_init_ref())
            })
        } else {
            None
        }
    }

    /// Returns whether the internal PageRequest has been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.get()
    }
}

/// Wrapper around tracking multiple different page requests that might need waiting. Only one
/// individual request is allowed to be considered 'active' at a time as the one that next needs
/// waiting on. Tracking whether a request is active is, depending on the request type, partially
/// automatic and partially requiring additional input from the user.
///
/// The PageRequest and LazyPageRequest access methods do not currently have a way to enforce that
/// those specific types of requests are made with the returned objects, however this could change
/// and callers are expected to use the correct method.
/// TODO(adanis): Implement an enforcement strategy.
#[pin_data]
#[repr(C)]
pub struct MultiPageRequest {
    /// Declared before `anonymous` so that it is dropped (and cancelled) first.
    #[pin]
    page_request: LazyPageRequest,
    anonymous: UnsafeCell<AnonymousPageRequest>,
    /// Track which request is active. This is multiple bools for consistency since the anonymous
    /// request being active is tracked directly in the AnonymousPageRequest and could not be part
    /// of an enum.
    read_active: Cell<bool>,
    dirty_active: Cell<bool>,
}

impl MultiPageRequest {
    /// Returns an in-place initializer for a `MultiPageRequest` that does not support early
    /// waking.
    pub fn new() -> impl PinInit<Self, Infallible> {
        Self::with_early_wake(false)
    }

    /// Returns an in-place initializer for a `MultiPageRequest`.
    pub fn with_early_wake(early_wake: bool) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            page_request <- LazyPageRequest::with_early_wake(early_wake),
            anonymous: UnsafeCell::new(AnonymousPageRequest::new()),
            read_active: Cell::new(false),
            dirty_active: Cell::new(false),
        })
    }

    /// Projects to the structurally pinned `page_request`.
    #[inline]
    fn lazy_request(self: Pin<&Self>) -> Pin<&LazyPageRequest> {
        // SAFETY: `page_request` is structurally pinned (`#[pin]`) within `self`.
        unsafe { self.map_unchecked(|this| &this.page_request) }
    }

    /// Wait on the currently active page request. The waiting thread is suspendable by default.
    pub fn wait(self: Pin<&Self>, suspendable: bool) -> Result<(), Status> {
        let this = self.get_ref();
        // SAFETY: `MultiPageRequest` is `!Sync`, so only the owning thread accesses `anonymous`,
        // and no reference returned by `get_anonymous` is live (its safety contract); the shared
        // borrow ends with this expression.
        if unsafe { this.anonymous.get().as_ref_unchecked() }.is_active() {
            debug_assert!(!this.dirty_active.get() && !this.read_active.get());
            // SAFETY: As above; nothing else borrows `anonymous` until `allocate` returns, and
            // `AnonymousPageRequest::allocate` never re-enters this `MultiPageRequest`.
            return unsafe { this.anonymous.get().as_mut_unchecked() }.allocate(suspendable);
        }
        // Exactly one of read and dirty should be considered active.
        debug_assert!(this.dirty_active.get() ^ this.read_active.get());
        this.read_active.set(false);
        this.dirty_active.set(false);
        self.lazy_request()
            .as_initialized()
            .expect("active page request must be initialized")
            .wait(suspendable)
    }

    /// Retrieve the anonymous page request. The caller may or may not arm the
    /// AnonymousPageRequest, if it does the anonymous request becomes considered active and no
    /// other request may be retrieved.
    ///
    /// # Safety
    ///
    /// `Pin<&Self>` is `Copy`, so the borrow checker cannot tie the returned reference to `self`:
    /// the caller must not use it after `self` is next accessed through `get_anonymous`, `wait`,
    /// `cancel_requests`, or (in debug builds) any accessor asserting `no_request_active`.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_anonymous(self: Pin<&Self>) -> &mut AnonymousPageRequest {
        // Check before forming the exclusive reference below, as `no_request_active` borrows
        // `anonymous`.
        debug_assert!(self.no_request_active());
        // SAFETY: `MultiPageRequest` is `!Sync`, the shared borrow taken by `no_request_active`
        // has ended, and the caller guarantees the returned reference is not live across another
        // access to `self`.
        unsafe { self.get_ref().anonymous.get().as_mut_unchecked() }
    }

    /// Retrieve and commit to initializing the page request for read. After calling this it is
    /// assumed that the page request will be made waitable and no other request may be retrieved.
    #[inline]
    pub fn get_read_request(self: Pin<&Self>) -> Pin<&PageRequest> {
        debug_assert!(self.no_request_active());
        self.read_active.set(true);
        self.lazy_request().get()
    }

    /// Retrieve a lazy accessor to page request. If a dirty request is generated the caller must
    /// then call `made_dirty_request` so that this helper knows that the page request is active and
    /// should be waited on.
    #[inline]
    pub fn get_lazy_dirty_request(self: Pin<&Self>) -> Pin<&LazyPageRequest> {
        debug_assert!(self.no_request_active());
        self.lazy_request()
    }

    /// Indicate that the page request retrieved by `get_lazy_dirty_request` was used and should be
    /// waited on.
    #[inline]
    pub fn made_dirty_request(self: Pin<&Self>) {
        debug_assert!(self.no_request_active());
        self.dirty_active.set(true);
    }

    /// Cancel all requests and have no active request.
    pub fn cancel_requests(self: Pin<&Self>) {
        let this = self.get_ref();
        // SAFETY: `MultiPageRequest` is `!Sync`, so only the owning thread accesses `anonymous`,
        // and no reference returned by `get_anonymous` is live (its safety contract); the
        // exclusive borrow ends with this statement.
        unsafe { this.anonymous.get().as_mut_unchecked() }.cancel();
        // In case a request is still initialized, despite being waited on, explicitly cancel
        // regardless of active status.
        if let Some(req) = self.lazy_request().as_initialized() {
            req.cancel_request();
        }
        this.read_active.set(false);
        this.dirty_active.set(false);
    }

    /// Returns a raw pointer to the `MultiPageRequest` for passing to C++.
    ///
    /// C++ never reads or writes the storage itself and only passes this pointer back to the
    /// `rust_multi_page_request_*` shims as `Pin<&MultiPageRequest>`, through which every mutation
    /// goes via `Cell` / `UnsafeCell`, so a `*mut` derived from a shared reference is never written
    /// outside interior mutability. Callers must not use the returned raw pointer to move the
    /// object in memory.
    pub fn as_raw(self: Pin<&Self>) -> *mut bindings::MultiPageRequest {
        (self.get_ref() as *const Self).cast_mut().cast()
    }

    fn no_request_active(&self) -> bool {
        // SAFETY: `MultiPageRequest` is `!Sync`, so only the owning thread accesses `anonymous`,
        // and no reference returned by `get_anonymous` is live (its safety contract); the shared
        // borrow ends with this expression.
        !unsafe { self.anonymous.get().as_ref_unchecked() }.is_active()
            && !self.read_active.get()
            && !self.dirty_active.get()
    }
}

/// Creates a `PageSource` for `page_provider`, storing it in `out`.
///
/// # Safety
///
/// `page_provider` must be a valid pointer to a C++ `PageProvider` with an owned reference, which
/// is transferred to this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_source_create(
    page_provider: *mut PageProvider,
    out: &mut *mut PageSource,
) -> zx_status_t {
    // SAFETY: The caller transfers an owned reference to a valid `PageProvider`.
    let page_provider = unsafe { RefPtr::from_raw(page_provider) };
    match PageSource::create(page_provider) {
        Ok(src) => {
            *out = RefPtr::into_raw(src).cast_mut();
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Recycles a `PageSource` when its reference count drops to zero.
///
/// # Safety
///
/// `src` must be a valid pointer to a `PageSource` (allocated by `PageSource::create` or
/// `rust_page_source_create` and exported via `RefPtr::into_raw`) whose reference count has
/// reached zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_source_recycle(src: *mut PageSource) {
    // SAFETY: The caller guarantees `src` is valid and its reference count has reached zero.
    unsafe { PageSource::recycle(NonNull::new_unchecked(src)) };
}

/// FFI for `PageSource::on_pages_supplied`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_on_pages_supplied(src: &PageSource, offset: u64, len: u64) {
    src.on_pages_supplied(offset, len);
}

/// FFI for `PageSource::on_pages_failed`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_on_pages_failed(
    src: &PageSource,
    offset: u64,
    len: u64,
    error_status: zx_status_t,
) {
    src.on_pages_failed(offset, len, Status::err_from_raw(error_status));
}

/// FFI for `PageSource::is_valid_internal_failure_code`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_is_valid_internal_failure_code(
    error_status: zx_status_t,
) -> bool {
    Status::try_from_raw(error_status).is_some_and(PageSource::is_valid_internal_failure_code)
}

/// FFI for `PageSource::should_trap_dirty_transitions`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_should_trap_dirty_transitions(src: &PageSource) -> bool {
    src.should_trap_dirty_transitions()
}

/// FFI for `PageSource::request_dirty_transition`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_request_dirty_transition(
    src: &PageSource,
    request: Pin<&PageRequest>,
    offset: u64,
    len: u64,
    vmo_debug_info: VmoDebugInfo,
) -> zx_status_t {
    Status::result_into_raw(src.request_dirty_transition(request, offset, len, vmo_debug_info))
}

/// FFI for `PageSource::on_pages_dirtied`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_on_pages_dirtied(src: &PageSource, offset: u64, len: u64) {
    src.on_pages_dirtied(offset, len);
}

/// FFI for `PageSource::detach`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_detach(src: &PageSource) {
    src.detach();
}

/// FFI for `PageSource::close`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_close(src: &PageSource) {
    src.close();
}

/// FFI for `PageSource::properties`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_properties(src: &PageSource) -> PageSourceProperties {
    src.properties()
}

/// FFI for `PageSource::dump`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_dump(src: &PageSource, depth: c_uint, max_items: u32) {
    src.dump(depth, max_items);
}

/// FFI for `PageSource::is_detached`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_is_detached(src: &PageSource) -> bool {
    src.is_detached()
}

/// FFI for `PageSource::paged_vmo_lock`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_paged_vmo_lock(
    src: &PageSource,
) -> *mut KMutex<PagedVmoMutexClass, TrackedMutex> {
    zr::ToMutPtr::to_mut_ptr(src.paged_vmo_lock())
}

/// FFI for `PageSource::get_provider_koid`. Returns false if the provider has no koid.
#[unsafe(no_mangle)]
pub extern "C" fn rust_page_source_get_provider_koid(
    src: &PageSource,
    out_koid: &mut MaybeUninit<zx_koid_t>,
) -> bool {
    match src.get_provider_koid() {
        Some(koid) => {
            out_koid.write(koid);
            true
        }
        None => false,
    }
}

/// FFI for `LazyPageRequest::get`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_lazy_page_request_get(req: Pin<&LazyPageRequest>) -> Pin<&PageRequest> {
    req.get()
}

/// Constructs a `MultiPageRequest` in place.
///
/// # Safety
///
/// `req` must point to uninitialized, suitably sized and aligned storage that will not move.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_multi_page_request_init(
    req: *mut MultiPageRequest,
    early_wake: bool,
) {
    // SAFETY: The caller guarantees `req` is valid storage that will not move. The initializer is
    // infallible.
    let Ok(()) = unsafe { MultiPageRequest::with_early_wake(early_wake).__pinned_init(req) };
}

/// Destroys a `MultiPageRequest` in place.
///
/// # Safety
///
/// `req` must point to a live `MultiPageRequest` that is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_multi_page_request_destroy(req: *mut MultiPageRequest) {
    // SAFETY: The caller guarantees `req` is live and not used again.
    unsafe { core::ptr::drop_in_place(req) };
}

/// FFI for `MultiPageRequest::wait`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_multi_page_request_wait(
    req: Pin<&MultiPageRequest>,
    suspendable: bool,
) -> zx_status_t {
    Status::result_into_raw(req.wait(suspendable))
}

/// FFI for `MultiPageRequest::get_anonymous`.
///
/// # Safety
///
/// The returned pointer must not be used after `req` is next passed to any
/// `rust_multi_page_request_*` function (see `MultiPageRequest::get_anonymous`).
#[unsafe(no_mangle)]
#[allow(clippy::mut_from_ref)]
pub unsafe extern "C" fn rust_multi_page_request_get_anonymous(
    req: Pin<&MultiPageRequest>,
) -> &mut AnonymousPageRequest {
    // SAFETY: The caller upholds the contract of `MultiPageRequest::get_anonymous`.
    unsafe { req.get_anonymous() }
}

/// FFI for `MultiPageRequest::get_lazy_dirty_request`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_multi_page_request_get_lazy_dirty_request(
    req: Pin<&MultiPageRequest>,
) -> Pin<&LazyPageRequest> {
    req.get_lazy_dirty_request()
}

/// FFI for `MultiPageRequest::made_dirty_request`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_multi_page_request_made_dirty_request(req: Pin<&MultiPageRequest>) {
    req.made_dirty_request();
}

/// FFI for `MultiPageRequest::cancel_requests`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_multi_page_request_cancel_requests(req: Pin<&MultiPageRequest>) {
    req.cancel_requests();
}

/// Kernel console `vm_page_source` command implementation.
#[cfg(console_enabled)]
mod console {
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_status::Status;

    /// Console command handler for `k vm_page_source`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - For the `dump` subcommand, `argv[2].arg_uint` must be the aligned address of a live
    ///   `PageSource` that remains valid for the duration of the call.
    unsafe extern "C" fn cmd_page_source(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} dump <address>", cmd_name);
            Status::INTERNAL.into_raw()
        };
        let not_enough_args = || -> c_int {
            kprintln!("not enough arguments");
            usage()
        };

        if args.len() < 2 {
            return not_enough_args();
        }

        if args[1].as_str() == "dump" {
            if args.len() < 3 {
                return not_enough_args();
            }
            let ps =
                core::ptr::with_exposed_provenance::<super::PageSource>(args[2].arg_uint as usize);
            if ps.is_null() || !ps.is_aligned() {
                kprintln!("invalid address");
                return usage();
            }
            // SAFETY: Console debugging command dumping a user-supplied `PageSource` pointer.
            unsafe { (*ps).dump(0, u32::MAX) };
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }

    static_command!(
        PS_OBJECT,
        c"vm_page_source".as_ptr(),
        c"page source debug commands".as_ptr(),
        cmd_page_source,
        CMD_AVAIL_NORMAL
    );
}
