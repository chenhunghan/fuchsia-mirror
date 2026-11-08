// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::page::VmPageDoublyLinkedList;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{HasRefCount, Recyclable, RefCounted, RefPtr};
use page_source_bindings as bindings;
use pin_init::pin_data;
use zr::{Opaque, pin_init_ffi, static_assert, unsafe_pinned_drop_ffi};
use zx_status::Status;

/// The different types of page requests that can exist.
pub type PageRequestType = bindings::page_request_type;

/// These properties are constant per `PageProvider` type, so a given `VmCowPages` can query and
/// cache these properties once (if it has a `PageSource`) and know they won't change after that.
/// This also avoids per-property plumbing via `PageSource`.
pub type PageSourceProperties = bindings::PageSourceProperties;

/// Wrapper around tracking multiple different page requests that might need waiting. Only one
/// individual request is allowed to be considered 'active' at a time as the one that next needs
/// waiting on. Tracking whether a request is active is, depending on the request type, partially
/// automatic and partially requiring additional input from the user.
/// The PageRequest and LazyPageRequest access methods do not currently have a way to enforce that
/// those specific types of requests are made with the returned objects, however this could change
/// and callers are expected to use the correct method.
/// TODO(adanis): Implement an enforcement strategy.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct MultiPageRequest {
    /// The optional inner `PageRequest` can appear in intrusive containers, so this object must be
    /// pinned.
    #[pin]
    opaque: Opaque<bindings::MultiPageRequest>,
}

unsafe_pinned_drop_ffi!(MultiPageRequest, bindings::cpp_multi_page_request_destroy);

static_assert!(size_of::<MultiPageRequest>() == size_of::<bindings::MultiPageRequest>());
static_assert!(align_of::<MultiPageRequest>() == align_of::<bindings::MultiPageRequest>());

impl MultiPageRequest {
    /// Returns an in-place initializer for stack-pinning a `MultiPageRequest`.
    pub fn new() -> impl pin_init::PinInit<Self> {
        pin_init_ffi!(bindings::cpp_multi_page_request_construct)
    }

    /// Waits on the currently active page request.
    pub fn wait(self: Pin<&mut Self>, suspendable: bool) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a valid, pinned `MultiPageRequest`.
        Status::ok(unsafe { bindings::cpp_multi_page_request_wait(self.as_raw(), suspendable) })
    }

    /// Cancel all requests and have no active request.
    pub fn cancel_requests(self: Pin<&mut Self>) {
        // SAFETY: Calling C++ CancelRequests on the pinned instance does not move it.
        unsafe { bindings::cpp_multi_page_request_cancel_requests(self.as_raw()) }
    }

    /// Returns a raw pointer to the underlying C++ `MultiPageRequest`.
    ///
    /// Callers must not use the returned raw pointer to move the object in memory.
    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::MultiPageRequest {
        self.opaque.get()
    }

    /// Retrieve the anonymous page request. The caller may or may not arm the
    /// `AnonymousPageRequest`, if it does the anonymous request becomes considered active and no
    /// other request may be retrieved.
    #[inline]
    pub fn get_anonymous(
        self: Pin<&mut Self>,
    ) -> &mut super::anonymous_page_request::AnonymousPageRequest {
        // SAFETY: `self.as_raw()` points to a valid `MultiPageRequest`. The C++ method returns
        // a pointer to its inline `AnonymousPageRequest`, which is valid and uniquely borrowed
        // for the lifetime of `self`.
        unsafe { &mut *bindings::cpp_multi_page_request_get_anonymous(self.as_raw()).cast() }
    }

    /// Retrieve and commit to initializing the page request for read. After calling this it is
    /// assumed that the page request will be made waitable and no other request may be retrieved.
    ///
    /// # Safety
    ///
    /// No request may currently be active on `self`. Because this marks the read request active,
    /// the caller must either initialize the returned request via [`PageSource::get_pages`] (such
    /// that it returns `Err(Status::SHOULD_WAIT)`) before calling [`MultiPageRequest::wait`], or
    /// call [`MultiPageRequest::cancel_requests`] before waiting.
    #[inline]
    pub unsafe fn get_read_request(self: Pin<&mut Self>) -> Pin<&mut PageRequestRef> {
        // SAFETY: `self.as_raw()` points to a valid `MultiPageRequest`. The C++ method returns a
        // pointer to its inline `PageRequest`. Since `self` is pinned, the inline struct is
        // also pinned.
        unsafe {
            let ptr = bindings::cpp_multi_page_request_get_read_request(self.as_raw()).cast();
            Pin::new_unchecked(&mut *ptr)
        }
    }

    /// Retrieve a lazy accessor to page request. If a dirty request is generated the caller must
    /// then call `made_dirty_request` so that this helper knows that the page request is active
    /// and should be waited on.
    #[inline]
    pub fn get_lazy_dirty_request(self: Pin<&mut Self>) -> Pin<&mut LazyPageRequest> {
        // SAFETY: `self.as_raw()` points to a valid `MultiPageRequest`.
        unsafe {
            let ptr = bindings::cpp_multi_page_request_get_lazy_dirty_request(self.as_raw()).cast();
            Pin::new_unchecked(&mut *ptr)
        }
    }

    /// Indicate that the page request retrieved by `get_lazy_dirty_request` was used and should be
    /// waited on.
    ///
    /// # Safety
    ///
    /// No request may currently be active on `self`, and the request retrieved via
    /// [`MultiPageRequest::get_lazy_dirty_request`] must have been initialized by a call to
    /// [`PageSource::request_dirty_transition`] that returned `Err(Status::SHOULD_WAIT)` (or must
    /// be cancelled via [`MultiPageRequest::cancel_requests`] before calling
    /// [`MultiPageRequest::wait`]).
    #[inline]
    pub unsafe fn made_dirty_request(self: Pin<&mut Self>) {
        // SAFETY: `self.as_raw()` points to a valid `MultiPageRequest`.
        unsafe { bindings::cpp_multi_page_request_made_dirty_request(self.as_raw()) }
    }
}

/// Wrapper around PageRequest that performs construction on first access. This is useful when a
/// PageRequest needs to be allocated eagerly in case it is used, even if the common case is that it
/// will not be needed.
#[repr(transparent)]
pub struct LazyPageRequest {
    opaque: Opaque<bindings::LazyPageRequest>,
}

impl LazyPageRequest {
    /// Initialize and return the internal `PageRequest`.
    #[inline]
    pub fn get(self: Pin<&mut Self>) -> Pin<&mut PageRequestRef> {
        // SAFETY: `self` is pinned.
        let raw = unsafe { self.get_unchecked_mut().opaque.get() };
        // SAFETY: `raw` points to a valid `LazyPageRequest`.
        unsafe { Pin::new_unchecked(&mut *bindings::cpp_lazy_page_request_get(raw).cast()) }
    }

    /// Returns whether this request has been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        // SAFETY: `self.opaque.get()` points to a valid `LazyPageRequest`.
        unsafe { bindings::cpp_lazy_page_request_is_initialized(self.opaque.get()) }
    }
}

/// Opaque client handle to a `PageRequest` owned by a `MultiPageRequest` or `LazyPageRequest`.
///
/// Unlike [`PageRequest`], which represents a provider-owned request view with `PageProvider`
/// accessors (`request_type`, `offset`, `len`) and intrusive list membership, `PageRequestRef`
/// is used by requestors (`VmCowPages` / `VmObject`) solely to initialize a request via
/// [`PageSource::get_pages`] or [`PageSource::request_dirty_transition`].
#[repr(C)]
pub struct PageRequestRef {
    raw: Opaque<bindings::PageRequest>,
    phantom: PhantomData<PhantomPinned>,
}

impl PageRequestRef {
    /// Domain-specific conversion: returns raw pointer for `PageRequestRef`.
    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::PageRequest {
        // SAFETY: Obtaining a raw pointer to `raw` does not move the pinned object.
        unsafe { self.get_unchecked_mut().raw.get() }
    }
}

/// A request for pages from a `PageSource`.
///
/// `PageRequest`s are allocated and owned by C++ (they serve as the allocation of all data needed
/// by all parties involved in a request), and page providers only ever observe them through
/// references or pointers handed over the FFI boundary while the request is owned by the provider
/// (i.e. from `SendAsyncRequest` or `SwapAsyncRequest` until `ClearAsyncRequest` or
/// `SwapAsyncRequest` returns). During that window C++ `PageSource` sets `provider_owned_` and
/// holds the request's `type_`, `offset_`, and `len_` fields constant so the provider can read
/// them. A `PageRequest` can appear in intrusive containers, so it must never be moved by Rust.
#[repr(C)]
pub struct PageRequest {
    raw: Opaque<bindings::PageRequest>,
    phantom: PhantomData<PhantomPinned>,
}

impl PageRequest {
    /// Domain-specific conversion: returns raw pointer for `PageRequest`.
    pub fn as_raw(&self) -> *mut bindings::PageRequest {
        self.raw.get()
    }

    /// Returns the type of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestType` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `type_`
    /// constant while the request is owned by the provider.
    pub fn request_type(&self) -> PageRequestType {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `type_` is initialized and not concurrently modified.
        let raw = unsafe { bindings::cpp_page_request_get_type(self.as_raw()) };
        match raw {
            // Request to provide the initial contents for the page.
            x if x == PageRequestType::READ as u32 => PageRequestType::READ,
            // Request to alter contents of the page, i.e. transition it from clean to dirty.
            x if x == PageRequestType::DIRTY as u32 => PageRequestType::DIRTY,
            // Request to write back modified page contents back to the source.
            x if x == PageRequestType::WRITEBACK as u32 => PageRequestType::WRITEBACK,
            _ => panic!("invalid page request type"),
        }
    }

    /// Returns the offset of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestOffset` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `offset_`
    /// constant while the request is owned by the provider.
    pub fn offset(&self) -> u64 {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `offset_` is initialized and not concurrently modified.
        unsafe { bindings::cpp_page_request_get_offset(self.as_raw()) }
    }

    /// Returns the length of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestLen` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `len_`
    /// constant while the request is owned by the provider.
    // A `PageRequest` is never zero-length, so an `is_empty` companion would always return false
    // and would be meaningless to callers.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `len_` is initialized and not concurrently modified.
        unsafe { bindings::cpp_page_request_get_len(self.as_raw()) }
    }

    /// Returns whether this request is currently in a `PageProviderTag` list.
    pub fn in_container(&self) -> bool {
        fbl::DoublyLinkedListContainable::<PageRequest, PageProviderTag>::get_node(self)
            .in_container()
    }
}

/// Tag for the list owned by the page provider, for tracking outstanding requests.
///
/// This names the same node as the C++ `PageProviderTag`, so a `PageRequest` may be in at most
/// one `PageProviderTag` list across both languages at a time. `PageRequest`s are owned by C++, so
/// lists of this tag hold unmanaged pointers and do not keep their members alive.
pub struct PageProviderTag;

// Mirrors the C++ `PageProviderNodeState` layout assertions in `page_source_ffi.cc`.
static_assert!(
    size_of::<fbl::DoublyLinkedListNode<PageRequest>>() == 2 * size_of::<*mut PageRequest>()
);
static_assert!(
    align_of::<fbl::DoublyLinkedListNode<PageRequest>>() == align_of::<*mut PageRequest>()
);

impl fbl::DoublyLinkedListContainable<PageRequest, PageProviderTag> for PageRequest {
    fn get_node(&self) -> &fbl::DoublyLinkedListNode<PageRequest> {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest`, and the accessor
        // only computes the address of its `PageProviderTag` node state.
        let node = unsafe { bindings::cpp_page_request_provider_node(self.as_raw()) };
        // SAFETY: `node` points to the `fbl::DoublyLinkedListNodeState<PageRequest*>` within
        // `self`, and so lives as long as `self`. That type is layout-compatible with
        // `fbl::DoublyLinkedListNode<PageRequest>` (a `#[repr(C)]` pair of `next` and `prev`
        // pointers, as asserted by `page_source_ffi.cc`), and its pointers target the
        // `PageRequest` itself, which has the same address as the Rust facade. The node's fields
        // are `UnsafeCell`s, so C++ mutating them through its own lists does not invalidate this
        // shared reference.
        unsafe { &*node.cast::<fbl::DoublyLinkedListNode<PageRequest>>() }
    }
}

/// A page source, which is the interface a VMO uses to request pages from a `PageProvider`.
///
/// The object is owned by C++, and Rust only ever observes it through pointers handed over the
/// FFI boundary.
#[repr(C)]
pub struct PageSource {
    raw: Opaque<bindings::PageSource>,
    phantom: PhantomData<PhantomPinned>,
}

impl HasRefCount for PageSource {
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`, and
        // `cpp_page_source_get_ref_counted` returns a valid pointer to its `fbl::RefCounted` base.
        let raw = unsafe { bindings::cpp_page_source_get_ref_counted(self.as_raw()) };
        unsafe { &*(raw.cast::<RefCounted>()) }
    }
}

// SAFETY: `PageSource` is a C++ `fbl::RefCounted` object freed via `cpp_page_source_free`.
unsafe impl Recyclable for PageSource {
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: Caller guarantees `ptr` points to a valid `PageSource` with zero remaining
        // references.
        unsafe {
            bindings::cpp_page_source_free(ptr.as_ptr().cast());
        }
    }
}

impl PageSource {
    /// Domain-specific conversion: returns raw pointer for `PageSource`.
    pub fn as_raw(&self) -> *mut bindings::PageSource {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr<PageSource>` from an exported pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid pointer to a C++ `PageSource` with an owned reference count, or null.
    pub unsafe fn from_raw(ptr: *mut bindings::PageSource) -> Option<RefPtr<Self>> {
        // SAFETY: Caller guarantees `ptr` is null or a valid `PageSource` with an owned reference.
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Domain-specific conversion: constructs a `&PageSource` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to an initialized C++ `PageSource` that remains
    /// live for the lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::PageSource) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::PageSource` is layout-compatible with `PageSource`, and the caller
        // guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Fails outstanding page requests in the range [offset, offset + len). Events associated with
    /// the failed page requests are signaled with the `error_status`, and any waiting threads are
    /// unblocked.
    pub fn on_pages_failed(&self, offset: u64, len: u64, error_status: Status) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe {
            bindings::cpp_page_source_on_pages_failed(
                self.as_raw(),
                offset,
                len,
                error_status.into_raw(),
            )
        }
    }

    /// Returns whether this `PageSource`'s `paged_vmo_lock` is held by the current thread.
    ///
    /// Intended for `debug_assert!`s that both document and check a caller's obligation to hold
    /// the `paged_vmo_lock` across a call.
    pub fn paged_vmo_lock_is_held(&self) -> bool {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_paged_vmo_lock_is_held(self.as_raw()) }
    }

    /// Returns true if `error_status` is a valid `ZX_PAGER_OP_FAIL` failure error code.
    pub fn is_valid_external_failure_code(error_status: Status) -> bool {
        // SAFETY: Pure function checking status code validity.
        unsafe { bindings::cpp_page_source_is_valid_external_failure_code(error_status.into_raw()) }
    }

    /// Returns true if `error_status` is a valid provider failure error code, which can be used
    /// with `on_pages_failed`.
    ///
    /// This returns true for every error code that `IsValidExternalFailureCode` returns true for,
    /// plus any additional error codes that are valid as an internal `PageProvider` status but not
    /// valid for ZX_PAGER_OP_FAIL.
    ///
    /// `Status::NO_MEMORY` will return true, unlike
    /// `IsValidExternalFailureCode(ZX_ERR_NO_MEMORY)` which returns false.
    ///
    /// Not every error code is supported, since these errors can get returned via a zx_vmo_read()
    /// or a zx_vmo_op_range(), if those calls resulted in a page fault.  So the `error_status`
    /// should be a supported return error code for those syscalls.  An error code need not be
    /// specifiable via ZX_PAGER_OP_FAIL for this function to return true.
    pub fn is_valid_internal_failure_code(error_status: Status) -> bool {
        // SAFETY: The C++ helper only inspects the status value.
        unsafe { bindings::cpp_page_source_is_valid_internal_failure_code(error_status.into_raw()) }
    }

    /// Prints state of the page source and any pending requests. The maximum number of requests
    /// printed is capped by `max_items`.
    pub fn dump(&self, depth: u32, max_items: u32) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe {
            bindings::cpp_page_source_dump(self.as_raw(), depth, max_items);
        }
    }

    /// The returned properties can be assumed to be const and never change. As such the caller may
    /// cache them.
    #[inline]
    pub fn properties(&self) -> PageSourceProperties {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_properties(self.as_raw()) }
    }

    /// Whether transitions from clean to dirty should be trapped.
    #[inline]
    pub fn should_trap_dirty_transitions(&self) -> bool {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_should_trap_dirty_transitions(self.as_raw()) }
    }

    /// Sends a request to the backing source to provide the requested page at `offset`.
    ///
    /// Returns `Err(Status::NOT_FOUND)` if the request cannot be fulfilled.
    /// Returns `Err(Status::SHOULD_WAIT)` if the request will be asynchronously fulfilled and the
    /// caller should wait on `req`. Depending on the state of pages in the range, `req` might be
    /// generated for a range that is a subset of `[offset, offset + len)`. Never returns `Ok(())`.
    #[inline]
    pub fn get_pages(
        &self,
        offset: u64,
        len: u64,
        req: Pin<&mut PageRequestRef>,
        vmo_debug_info: bindings::VmoDebugInfo,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        // `req.as_raw()` gets a pointer to a valid `PageRequest` without moving it.
        Status::ok(unsafe {
            bindings::cpp_page_source_get_pages(
                self.as_raw(),
                offset,
                len,
                req.as_raw(),
                vmo_debug_info,
            )
        })
    }

    /// Request the page provider for clean pages in the range `[offset, offset + len)` to become
    /// dirty, in order for a write to proceed. Returns `Err(Status::SHOULD_WAIT)` if the request
    /// will be asynchronously fulfilled; the caller should wait on `req`. Depending on the state
    /// of pages in the range, `req` might be generated for a range that is a subset of
    /// `[offset, offset + len)`. Never returns `Ok(())`.
    #[inline]
    pub fn request_dirty_transition(
        &self,
        req: Pin<&mut PageRequestRef>,
        offset: u64,
        len: u64,
        vmo_debug_info: bindings::VmoDebugInfo,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        // `req.as_raw()` gets a pointer to a valid `PageRequest` without moving it.
        Status::ok(unsafe {
            bindings::cpp_page_source_request_dirty_transition(
                self.as_raw(),
                req.as_raw(),
                offset,
                len,
                vmo_debug_info,
            )
        })
    }

    /// Updates the request tracking metadata to account for pages `[offset, offset + len)` having
    /// been supplied to the owning VMO.
    ///
    /// Note that the range `[offset, offset + len)` should not have been previously supplied. The
    /// page request tracking in `PageSource` works by tracking only a fulfilled length, and not
    /// exact fulfilled offsets, to save on memory required for metadata. So in order to prevent
    /// over-accounting errors, the caller must ensure that they are only calling
    /// `on_pages_supplied` for newly supplied ranges.
    // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
    // offsets with a bitmap. Might require capping the max permissible length of a page request.
    #[inline]
    pub fn on_pages_supplied(&self, offset: u64, len: u64) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_on_pages_supplied(self.as_raw(), offset, len) }
    }

    /// Updates the request tracking metadata to account for pages `[offset, offset + len)` having
    /// been dirtied in the owning VMO.
    ///
    /// Note that the range `[offset, offset + len)` should not have been previously dirtied. The
    /// page request tracking in `PageSource` works by tracking only a fulfilled length, and not
    /// exact fulfilled offsets, to save on memory required for metadata. So in order to prevent
    /// over-accounting errors, the caller must ensure that they are only calling
    /// `on_pages_dirtied` for newly dirtied ranges.
    // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
    // offsets with a bitmap. Might require capping the max permissible length of a page request.
    #[inline]
    pub fn on_pages_dirtied(&self, offset: u64, len: u64) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_on_pages_dirtied(self.as_raw(), offset, len) }
    }

    /// Frees `pages`.
    ///
    /// # Safety
    ///
    /// `pages` must contain valid pages owned by this page source's VMO.
    #[inline]
    pub unsafe fn free_pages(&self, pages: Pin<&mut VmPageDoublyLinkedList>) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`, and `pages` is a valid
        // pinned `VmPageDoublyLinkedList` whose layout matches C++ `VmPageDoublyLinkedList`.
        // `PageSource::FreePages` only pops elements in place and never moves the list header.
        unsafe {
            bindings::cpp_page_source_free_pages(
                self.as_raw(),
                core::ptr::from_mut(pages.get_unchecked_mut()).cast(),
            )
        }
    }
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
