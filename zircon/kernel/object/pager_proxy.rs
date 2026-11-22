// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::diagnostics::dump_handles_for_koid;
use super::pager_dispatcher::PagerDispatcher;
use super::pager_dispatcher_ffi::{
    cpp_pager_proxy_as_page_provider, cpp_pager_proxy_create, cpp_pager_proxy_free,
    cpp_pager_proxy_get_ref_counted,
};
use super::port_dispatcher::{PortAllocator, PortDispatcher, PortPacket};
use super::thread_dispatcher::{AutoBlocked, Blocked};
use crate::counters::define_kcounter;
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::event::Event;
use crate::kernel::thread::{ScopedMemoryStall, THREAD_SIGNAL_SUSPEND, ThreadPtr};
use crate::ksync_kernel::TrackedMutex;
use crate::platform_rs::timer::{DurationMono, current_mono_time};
use crate::vm::page_source::{
    PageProvider, PageProviderTag, PageRequest, PageRequestType, PageSource, PageSourceProperties,
};
use crate::vm::vm::{vm_ktrace_duration, vm_ktrace_flow_begin, vm_ktrace_flow_end};
use boot_options::BootOptions;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::CStr;
use core::mem::offset_of;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, RefPtr};
use ksync::{KMutex, LockToken, guarded, kcell_init};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx_status::Status;
use zx_types::{
    ZX_MAX_NAME_LEN, ZX_TIME_INFINITE_PAST, zx_instant_mono_t, zx_koid_t, zx_packet_page_request_t,
    zx_packet_type_t, zx_page_request_command_t, zx_port_packet_t, zx_status_t,
};

const LOCAL_TRACE: u32 = 0;

define_kcounter!(DISPATCHER_PAGER_OVERTIME_WAIT_COUNT, "dispatcher.pager.overtime_waits", Sum);
define_kcounter!(DISPATCHER_PAGER_IGNORED_SUSPEND_COUNT, "dispatcher.pager.ignored_suspends", Sum);
define_kcounter!(DISPATCHER_PAGER_TOTAL_REQUEST_COUNT, "dispatcher.pager.total_requests", Sum);
define_kcounter!(
    DISPATCHER_PAGER_SUCCEEDED_REQUEST_COUNT,
    "dispatcher.pager.succeeded_requests",
    Sum
);
define_kcounter!(DISPATCHER_PAGER_FAILED_REQUEST_COUNT, "dispatcher.pager.failed_requests", Sum);
define_kcounter!(
    DISPATCHER_PAGER_TIMED_OUT_REQUEST_COUNT,
    "dispatcher.pager.timed_out_requests",
    Sum
);

zr::static_assert_size_and_align!(
    PagerProxyState,
    object_constants::kPagerProxyStateSize,
    object_constants::kPagerProxyStateAlign,
);
zr::static_assert!(offset_of!(PagerProxyState, allocator) == 0);

fbl::impl_opaque_ref_counted_facade!(
    /// Page provider implementation that talks to a userspace pager service.
    ///
    /// The lifecycle of this class is a little complicated because the pager dispatcher's port
    /// potentially has an unmanaged reference to the PageSource that contains the PagerProxy
    /// through packet. Because of this, we need to ensure that the last RefPtr to the PageSource
    /// isn't released too early when the pager dispatcher gets closed. Normally, the dispatcher can
    /// retain its reference to the PageSource until the port frees its reference to packet
    /// (through the PortAllocator). However, if the dispatcher is destroyed, if we can't revoke the
    /// port's reference to packet, then we end up making the PagerProxy keep a reference to the
    /// containing PageSource until the packet is freed.
    ///
    /// This is a facade for the C++ `PagerProxy` shell, whose state is a `PagerProxyState`.
    #[repr(align(8))]
    pub struct PagerProxy,
    cpp_pager_proxy_free,
    cpp_pager_proxy_get_ref_counted,
);

impl DoublyLinkedListContainable<PagerProxy> for PagerProxy {
    fn get_node(&self) -> &DoublyLinkedListNode<PagerProxy> {
        &self.state().node
    }
}

impl PagerProxy {
    /// `options` is a bitmask of:
    pub const TRAP_DIRTY: u32 = 1 << 0;

    /// Creates a new `PagerProxy`.
    pub fn create(
        dispatcher: &PagerDispatcher,
        port: RefPtr<PortDispatcher>,
        key: u64,
        options: u32,
    ) -> Result<RefPtr<Self>, Status> {
        let port_raw = RefPtr::into_raw(port).cast_mut();
        let mut out_proxy: *mut PagerProxy = core::ptr::null_mut();
        // SAFETY: `dispatcher` is a valid reference, `port_raw` is an owned raw pointer from
        // `RefPtr::into_raw`, and `out_proxy` is a valid out reference.
        let status =
            unsafe { cpp_pager_proxy_create(dispatcher, port_raw, key, options, &mut out_proxy) };
        Status::ok(status)?;
        // SAFETY: `cpp_pager_proxy_create` succeeded and wrote a non-null owned `PagerProxy`
        // pointer to `out_proxy`.
        Ok(unsafe { RefPtr::from_raw(out_proxy) })
    }

    /// Converts a reference to `proxy` into a reference to its `PageProvider` base.
    pub fn as_page_provider(proxy: RefPtr<Self>) -> RefPtr<PageProvider> {
        let raw = RefPtr::into_raw(proxy).cast_mut();
        // SAFETY: `raw` is an owned reference to a live `PagerProxy`. The `PageProvider` base
        // shares the `PagerProxy`'s reference count, so the owned reference is transferred.
        unsafe { RefPtr::from_raw(cpp_pager_proxy_as_page_provider(raw)) }
    }

    /// Returns a reference to the underlying state object.
    pub fn state(&self) -> &PagerProxyState {
        // SAFETY: The state object is located at a verified offset within the same allocation as
        // the facade (see `DISPATCHER_VERIFY_OFFSET` in the C++ constructor), and is initialized
        // for the lifetime of the `PagerProxy`.
        unsafe {
            &*(self as *const Self)
                .wrapping_byte_add(object_constants::kPagerProxyStateOffset)
                .cast::<PagerProxyState>()
        }
    }
}

/// References whose release must be deferred until after `PagerProxyState::mtx` has been dropped,
/// as releasing them may destroy the `PagerProxy` (and therefore the state containing the lock).
#[derive(Default)]
struct DeferredRelease {
    /// The proxy's reference to its `PageSource`. Dropped before `proxy` (fields drop in
    /// declaration order) so the `PageSource`, which holds its own `RefPtr` to the `PagerProxy`,
    /// is torn down while `proxy` still keeps the proxy alive.
    src: Option<RefPtr<PageSource>>,
    /// A reference to the `PagerProxy` (either returned by `PagerDispatcher::release_proxy` or
    /// cloned in `free`) kept alive until after `mtx` has been released.
    proxy: Option<RefPtr<PagerProxy>>,
}

/// State of a `PagerProxy`, stored in the `opaque_storage` of the C++ `PagerProxy` shell.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PagerProxyState {
    /// Allocator that the port returns `packet` through. It is kept at offset 0 so that
    /// `free_packet` can recover the state from the `PortAllocator` pointer the port hands back.
    allocator: PortAllocator,

    #[pin]
    #[mutex]
    mtx: KMutex<TrackedMutex>,

    /// Links the containing `PagerProxy` into `PagerDispatcherState::proxies`. Guarded by the
    /// owning `PagerDispatcher`'s lock, not by `mtx`.
    #[pin]
    node: DoublyLinkedListNode<PagerProxy>,

    /// The dispatcher that created this proxy. Cleared (under `mtx`) when the proxy is unlinked
    /// from the dispatcher's `proxies`: by `on_dispatcher_close` when the last handle to the
    /// dispatcher is closed, or by this proxy itself in `on_close`/`free`. The dispatcher is
    /// therefore live whenever this is `Some` under `mtx`.
    #[guarded_by(mtx)]
    pager: Option<NonNull<PagerDispatcher>>,

    /// Cache the pager's koid so we can still respond to `get_koid` after pager has been destroyed.
    pager_koid: u64,

    port: RefPtr<PortDispatcher>,
    key: u64,

    /// Options set at creation.
    options: u32,

    /// Whether the page_source is closed, i.e. this proxy object is no longer linked to the
    /// page_source and it can receive no more messages from the page_source.
    #[guarded_by(mtx)]
    page_source_closed: bool,

    /// Flag set when there is a pending ZX_PAGER_VMO_COMPLETE message. This serves as a proxy
    /// for whether or not the port has a reference to packet (as the complete message is the
    /// last message sent). This flag is used to delay cleanup if `on_close` is called
    /// while the port still has a reference to packet.
    #[guarded_by(mtx)]
    complete_pending: bool,

    /// Bool indicating whether or not packet is currently queued in the port.
    #[guarded_by(mtx)]
    packet_busy: bool,

    /// PortPacket used for sending all page requests to the pager service. This state serves as
    /// packet's allocator (through `allocator`). This informs the state when packet is freed by
    /// the port, which lets the single packet be continuously reused for all of the source's page
    /// requests.
    ///
    /// The port links, cancels and dequeues the packet through the pointer it was queued with
    /// while other threads hold `&PagerProxyState`, so it is wrapped in an `UnsafeCell`.
    #[pin]
    packet: UnsafeCell<PortPacket>,

    /// The page_request_t which corresponds to the current packet. Can be set to `None` if the
    /// PageSource calls `clear_async_request` to take back the request while the packet is still
    /// busy - this can happen if `clear_async_request` races with a `free` coming from port
    /// dequeue. More details about this race can be found in https://fxbug.dev/42173553.
    #[guarded_by(mtx)]
    active_request: Option<NonNull<PageRequest>>,

    /// Queue of page_request_t's that have come in while packet is busy. The
    /// head of this queue is sent to the port when packet is freed.
    #[pin]
    #[guarded_by(mtx)]
    pending_requests: DoublyLinkedList<NonNull<PageRequest>, PageProviderTag>,

    /// PageRequest used for the complete message.
    ///
    /// It is never sent to a PageSource, so none of its fields are populated or read; it is only
    /// compared by address against other request pointers, which needs no lock. Its
    /// `PageProviderTag` node, which `queue_packet_locked` links into `pending_requests` while
    /// the packet is busy, is only ever touched through that `mtx`-guarded list.
    #[pin]
    complete_request: PageRequest,

    /// Back pointer to the PageSource that owns this instance.
    /// The PageSource also has a RefPtr to this object, and so with this being a RefPtr there
    /// exists a cycle. This is deliberate and allows this object to control when deletion happens
    /// to ensure deletion doesn't happen whilst port packets are queued. The cycle will be
    /// explicitly cut during the graceful destruction triggered by `on_dispatcher_close` or
    /// `on_close`.
    #[guarded_by(mtx)]
    page_source: Option<RefPtr<PageSource>>,

    /// Timestamp of the last time an overtime page request triggered an informational dump. This
    /// is used to prevent multiple overtime requests on the same PagerProxy from all redundantly
    /// triggering the same informational dump and spamming the debuglog.
    #[guarded_by(mtx)]
    last_overtime_dump: zx_instant_mono_t,

    /// Monotonically generated flow ID for page request tracing to avoid duplicate flow ID errors.
    #[guarded_by(mtx)]
    flow_id: u64,
}

// SAFETY: All mutable state is guarded by `mtx`, except for `packet`, which is only written while
// `packet_busy` is false (under `mtx`) and is otherwise only touched by the port it is queued on,
// and `node`, which is only linked, unlinked and traversed under the owning `PagerDispatcher`'s
// lock. The raw pointers refer to the `PagerDispatcher` (cleared under `mtx`, by
// `on_dispatcher_close` or by the proxy when it unlinks itself, before the dispatcher is
// destroyed) and to page requests that are owned by the provider while they are referenced.
unsafe impl Send for PagerProxyState {}
// SAFETY: See the `Send` implementation above.
unsafe impl Sync for PagerProxyState {}

#[pinned_drop]
impl PinnedDrop for PagerProxyState {
    fn drop(self: Pin<&mut Self>) {
        let proxy_ptr = core::ptr::from_ref(self.as_ref().get_ref())
            .wrapping_byte_sub(object_constants::kPagerProxyStateOffset)
            .cast::<PagerProxy>();
        debug::ltracef!("{:p}\n", proxy_ptr);
        let this = self.project();
        // In error paths shortly after construction, we can destruct without page_source_closed
        // becoming true.
        debug_assert!(!*this.complete_pending.get_inner_mut());
        // We vend out a raw pointer to ourselves in the form of being the allocator for our
        // internal PortPacket. Ensure that the packet is not somehow still in use, and that a
        // PortDispatcher would therefore not be able to call free.
        debug_assert!(!*this.packet_busy.get_inner_mut());
    }
}

/// Helper to calculate the pager wait deadline.
fn make_deadline() -> Deadline {
    if BootOptions::get().userpager_overtime_wait_seconds == 0 {
        return Deadline::infinite();
    }
    Deadline::after_mono(
        DurationMono::from_seconds(BootOptions::get().userpager_overtime_wait_seconds as i64),
        TimerSlack::none(),
    )
}

/// Helper to determine if we've waited on the pager for longer than the specified timeout.
fn waited_too_long(waited: u32) -> bool {
    BootOptions::get().userpager_overtime_timeout_seconds > 0
        && u64::from(waited).wrapping_mul(BootOptions::get().userpager_overtime_wait_seconds)
            >= BootOptions::get().userpager_overtime_timeout_seconds
}

/// Returns the prefix of `name` up to its first NUL byte as a string.
#[inline(never)]
fn name_prefix(name: &[u8]) -> &str {
    let bytes = CStr::from_bytes_until_nul(name).map_or(name, CStr::to_bytes);
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Returns the trace label for the page request type `ty`.
fn request_type_label(ty: PageRequestType) -> &'static str {
    if ty == PageRequestType::Read { "Read" } else { "Dirty" }
}

/// `zx_packet_page_request_t`, overlaid on `zx_port_packet_t::union` (32 bytes).
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct PacketPageRequest {
    command: u16,
    flags: u16,
    reserved0: [u8; 4],
    offset: u64,
    length: u64,
    reserved1: u64,
}
zr::static_assert!(size_of::<PacketPageRequest>() == 32);
zr::static_assert!(size_of::<PacketPageRequest>() == size_of::<zx_packet_page_request_t>());
zr::static_assert!(
    offset_of!(PacketPageRequest, command) == offset_of!(zx_packet_page_request_t, command)
);
zr::static_assert!(
    offset_of!(PacketPageRequest, flags) == offset_of!(zx_packet_page_request_t, flags)
);
zr::static_assert!(
    offset_of!(PacketPageRequest, offset) == offset_of!(zx_packet_page_request_t, offset)
);
zr::static_assert!(
    offset_of!(PacketPageRequest, length) == offset_of!(zx_packet_page_request_t, length)
);

impl PagerProxyState {
    /// Returns an initializer for the state, to be run on the storage at `state`.
    fn init(
        state: NonNull<PagerProxyState>,
        dispatcher: &PagerDispatcher,
        port: RefPtr<PortDispatcher>,
        key: u64,
        options: u32,
    ) -> impl PinInit<Self, Infallible> {
        let pager = NonNull::from(dispatcher);
        let pager_koid = dispatcher.get_koid();
        // `allocator` is at offset 0 of the storage this initializer fills, so a pointer derived
        // from `state` carries provenance over the whole state for `free_packet`.
        let allocator_ptr = state.as_ptr().cast::<PortAllocator>();
        pin_init!(Self {
            allocator: PortAllocator::new(Self::free_packet),
            mtx <- KMutex::init(),
            node: DoublyLinkedListNode::new(),
            pager: Some(pager).into(),
            pager_koid,
            port,
            key,
            options,
            page_source_closed: false.into(),
            complete_pending: false.into(),
            packet_busy: false.into(),
            packet: UnsafeCell::new(PortPacket::new(core::ptr::null(), allocator_ptr)),
            active_request: None.into(),
            pending_requests <- kcell_init(DoublyLinkedList::new()),
            complete_request <- PageRequest::new(),
            page_source: None.into(),
            last_overtime_dump: ZX_TIME_INFINITE_PAST.into(),
            flow_id: 0.into(),
        })
    }

    /// Returns the `PagerProxy` containing this state.
    fn proxy(&self) -> &PagerProxy {
        // SAFETY: `self` only ever exists at byte offset `kPagerProxyStateOffset` inside the
        // `opaque_storage_` of a live C++ `PagerProxy` (verified by `DISPATCHER_VERIFY_OFFSET` in
        // its constructor; this is the inverse of `PagerProxy::state`), which outlives `self`.
        unsafe {
            &*(self as *const Self)
                .wrapping_byte_sub(object_constants::kPagerProxyStateOffset)
                .cast::<PagerProxy>()
        }
    }

    /// Returns a pointer to `packet` with write permission, for handing it to the port.
    fn packet_ptr(&self) -> NonNull<PortPacket> {
        // SAFETY: `UnsafeCell::get` returns a pointer derived from a reference, so it is non-null.
        unsafe { NonNull::new_unchecked(self.packet.get()) }
    }

    /// The returned properties can assumed to be const and never change. As such the caller may
    /// cache them.
    fn properties(&self) -> PageSourceProperties {
        PageSourceProperties {
            is_user_pager: true,
            supports_request_type: [true, (self.options & PagerProxy::TRAP_DIRTY) != 0, false],
        }
    }

    /// Retrieves the koid for this page provider.
    fn get_koid(&self) -> zx_koid_t {
        self.pager_koid
    }

    /// Informs the backing source of a page request. The provider has ownership
    /// of `request` until the async request is cancelled.
    ///
    /// # Safety
    ///
    /// `request` must not currently be in any `PageProviderTag` list, and must remain valid and
    /// owned by this provider until cleared or swapped out (or the provider closes).
    unsafe fn send_async_request(&self, request: &PageRequest) {
        ksync::lock!(let mut guard = self.lock_mtx());
        assert!(!*guard.page_source_closed());

        self.queue_packet_locked(guard.as_mut().token_mut(), NonNull::from(request));
    }

    /// Queues the page request, either sending it to the port or putting it in pending_requests.
    fn queue_packet_locked(
        &self,
        token: &mut LockToken<'_, PagerProxyStateMtxClass>,
        request: NonNull<PageRequest>,
    ) {
        // SAFETY: `token` proves that `self.mtx` is held.
        let mut locked = unsafe { self.guard_mtx_mut(token) };
        if *locked.packet_busy() {
            // SAFETY: `request` is not in any `PageProviderTag` list, and is either owned by the
            // provider and remains live until it is taken back by the PageSource (under `mtx`), or
            // is the complete request, which lives as long as this state.
            unsafe { locked.pending_requests_mut().push_back_raw(request) };
            return;
        }

        debug_assert!(locked.active_request().is_none());
        *locked.packet_busy_mut() = true;
        *locked.active_request_mut() = Some(request);

        let offset;
        let length;
        let cmd;
        if request != NonNull::from(&self.complete_request) {
            // SAFETY: The provider owns `request`, which remains live until it is taken back by the
            // PageSource (under `mtx`).
            let request = unsafe { request.as_ref() };
            cmd = match PageProvider::get_request_type(request) {
                PageRequestType::Read => zx_page_request_command_t::ZX_PAGER_VMO_READ,
                PageRequestType::Dirty => {
                    debug_assert!((self.options & PagerProxy::TRAP_DIRTY) != 0);
                    zx_page_request_command_t::ZX_PAGER_VMO_DIRTY
                }
                // Not reached
                _ => panic!("unexpected page request type"),
            };
            offset = PageProvider::get_request_offset(request);
            length = PageProvider::get_request_len(request);

            // The vm subsystem should guarantee this
            debug_assert!(offset.checked_add(length).is_some());

            // Trace flow events require an enclosing duration.
            // We cannot use the PortPacket pointer address (&packet) as the flow ID because packet
            // is a member field reused across consecutive page requests. If a previous flow end
            // event is lost or cancelled, reusing the same pointer address causes
            // flow_duplicate_id collisions.
            *locked.flow_id_mut() = flow_id::generate();
            vm_ktrace_duration!(
                1,
                "page_request_queue",
                "vmo_id" => PageProvider::get_request_vmo_id(request),
                "offset" => offset,
                "length" => length,
                "type" => request_type_label(PageProvider::get_request_type(request))
            );
            vm_ktrace_flow_begin!(1, "page_request_queue", *locked.flow_id());
        } else {
            offset = 0;
            length = 0;
            cmd = zx_page_request_command_t::ZX_PAGER_VMO_COMPLETE;
        }

        let packet = zx_port_packet_t {
            key: self.key,
            packet_type: zx_packet_type_t::ZX_PKT_TYPE_PAGE_REQUEST,
            status: 0,
            union: zerocopy::transmute!(PacketPageRequest {
                command: cmd as u16,
                offset,
                length,
                ..PacketPageRequest::default()
            }),
        };

        // SAFETY: `packet` is not queued in the port (`packet_busy` was false and `mtx` is held),
        // so there are no concurrent accesses.
        unsafe { (*self.packet.get()).set_packet(&packet) };

        // We can treat ZX_ERR_BAD_HANDLE as if the packet was queued
        // but the pager service never responds.
        // TODO: Bypass the port's max queued packet count to prevent ZX_ERR_SHOULD_WAIT
        // SAFETY: `packet` is not linked in any list, and it remains live until the port frees it
        // through `free_packet` or it is cancelled, as the state is not destroyed while
        // `packet_busy` is true.
        assert!(unsafe { self.port.queue(self.packet_ptr()) } != Err(Status::SHOULD_WAIT));
    }

    /// Informs the backing source that a page request has been fulfilled. This
    /// must be called for all requests that are raised.
    ///
    /// # Safety
    ///
    /// `request` must currently be owned by this provider (either `active_request` or queued in
    /// `pending_requests`).
    unsafe fn clear_async_request(&self, request: &PageRequest) {
        ksync::lock!(let mut guard = self.lock_mtx());
        assert!(!*guard.page_source_closed());

        let request_ptr = NonNull::from(request);
        if *guard.active_request() == Some(request_ptr) {
            if request_ptr != NonNull::from(&self.complete_request) {
                // Trace flow events require an enclosing duration.
                vm_ktrace_duration!(
                    1,
                    "page_request_queue",
                    "vmo_id" => PageProvider::get_request_vmo_id(request),
                    "offset" => PageProvider::get_request_offset(request),
                    "length" => PageProvider::get_request_len(request),
                    "type" => request_type_label(PageProvider::get_request_type(request))
                );
                vm_ktrace_flow_end!(1, "page_request_queue", *guard.flow_id());
            }
            // This request is being taken back by the PageSource, so we can't hold a reference to
            // it anymore. This will remain None until on_packet_freed_locked is called (and a new
            // packet gets queued as a result), either by us here below or by free,
            // since packet_busy is true and will be true until on_packet_freed_locked is called.
            *guard.as_mut().active_request_mut() = None;
            // Condition on whether or not we actually cancel the packet, to make sure
            // we don't race with a call to free.
            // SAFETY: `packet` is a member of this live state, and is only ever queued in `port`.
            if unsafe { self.port.cancel_queued_packet(self.packet.get()) } {
                self.on_packet_freed_locked(guard.as_mut().token_mut());
            }
        // SAFETY: `mtx` is held, serializing access to this provider's `PageProviderTag` list.
        } else if unsafe { request.in_container() } {
            // SAFETY: A request in a `PageProviderTag` list that is routed to this provider is in
            // `pending_requests`.
            let _ = unsafe { guard.as_mut().pending_requests_mut().erase(request) };
        }
    }

    /// Swaps the backing memory for a request. Assumes that `old`
    /// and `new_req` have the same type, offset, and length.
    ///
    /// # Safety
    ///
    /// `old` must currently be owned by this provider (either `active_request` or queued in
    /// `pending_requests`). `new_req` must not be in any `PageProviderTag` list, and must remain
    /// valid and owned by this provider until cleared or swapped out (or the provider closes).
    unsafe fn swap_async_request(&self, old: &PageRequest, new_req: &PageRequest) {
        ksync::lock!(let mut guard = self.lock_mtx());
        assert!(!*guard.page_source_closed());

        // SAFETY: `mtx` is held, serializing access to this provider's `PageProviderTag` list.
        if unsafe { old.in_container() } {
            let mut pending = guard.as_mut().pending_requests_mut();
            // SAFETY: `old` is in `pending_requests` (see `clear_async_request`), and `new_req` is
            // owned by the provider, is not in any `PageProviderTag` list and remains live until
            // it is taken back by the PageSource (under `mtx`).
            unsafe {
                pending.as_mut().cursor_at(old).insert_before_raw(NonNull::from(new_req));
                let _ = pending.erase(old);
            }
        } else if *guard.active_request() == Some(NonNull::from(old)) {
            *guard.as_mut().active_request_mut() = Some(NonNull::from(new_req));
        }
    }

    /// `on_detach` is called once no more calls to `send_async_request` will be made. It will be
    /// called before `on_close` and will only be called once.
    fn on_detach(&self) {
        ksync::lock!(let mut guard = self.lock_mtx());
        assert!(!*guard.page_source_closed());

        *guard.as_mut().complete_pending_mut() = true;
        self.queue_packet_locked(guard.as_mut().token_mut(), NonNull::from(&self.complete_request));
    }

    /// After `on_close` is called, no more calls will be made except for `wait_on_event`.
    ///
    /// Returns the references to release once `self` is no longer borrowed: dropping the last
    /// `RefPtr<PagerProxy>` may destroy the state that `self` borrows, so the caller must drop
    /// the result after `self` is dead.
    #[must_use]
    fn on_close(&self) -> DeferredRelease {
        let mut deferred = DeferredRelease::default();
        ksync::lock!(let mut guard = self.lock_mtx());
        assert!(!*guard.page_source_closed());

        *guard.as_mut().page_source_closed_mut() = true;
        // If there isn't a complete packet pending, we're free to clean up our ties with the
        // PageSource and PagerDispatcher right now, as we're not expecting a free to
        // perform final delayed clean up later. The PageSource is closing, so it won't need to send
        // us any more requests. The PagerDispatcher doesn't need to refer to us anymore as we won't
        // be queueing any more pager requests.
        if !*guard.complete_pending() {
            // We know PagerDispatcher::on_zero_handles hasn't been invoked, since that would
            // have already closed this pager proxy via on_dispatcher_close. Therefore we are free
            // to immediately clean up.
            let pager = *guard.pager();
            debug_assert!(pager.is_some());
            // SAFETY: `pager` is only cleared (under `mtx`) before the dispatcher is destroyed, so
            // it is live while `mtx` is held.
            let pager = unsafe { pager.expect("PagerProxy closed without a pager").as_ref() };
            // SAFETY: `pager` is the dispatcher that created this proxy: it is set once from the
            // creating dispatcher in `init` and only ever cleared.
            deferred.proxy = unsafe { pager.release_proxy(self.proxy(), guard.token()) };
            // If release_proxy unlinked this proxy, on_zero_handles will not find it to call
            // on_dispatcher_close, so clear pager here rather than let it outlive the dispatcher.
            *guard.as_mut().pager_mut() = None;
            deferred.src = guard.as_mut().page_source_mut().take();
        } else {
            // There is still a pending complete message that we would like to wait to be received
            // and so we do not perform cancel_queued_packet like on_dispatcher_close does. However,
            // we must leave the reference to ourselves in pager so that on_dispatcher_close (and
            // the forced packet cancelling) can happen if needed. Otherwise final delayed cleanup
            // will happen in free.
        }
        deferred
    }

    /// Called by the pager dispatcher when it is about to go away. Handles cleaning up port's
    /// reference to any in flight packets.
    pub fn on_dispatcher_close(&self) {
        // Declared before `guard` so that it is dropped after `mtx` is released: `close` below may
        // call back into `on_close`, which takes `page_source`, leaving this as the last reference
        // to the `PageSource` that owns this `PagerProxy` (and so `mtx`).
        let mut self_src: Option<RefPtr<PageSource>> = None;
        ksync::lock!(let mut guard = self.lock_mtx());

        // The PagerDispatcher is going away and there won't be a way to service any pager
        // requests. Close the PageSource from our end so that no more requests can be sent. Closing
        // the PageSource will clear/cancel any outstanding requests that it had forwarded, i.e. any
        // requests except the complete request (which is owned by us and is not visible to the
        // PageSource).
        if !*guard.page_source_closed() {
            // page_source is only reset to None if we already closed it.
            debug_assert!(guard.page_source().is_some());
            let src = self_src
                .insert(guard.page_source().clone().expect("open PagerProxy without a PageSource"));
            // Call close without the lock to
            //  * Not violate lock ordering
            //  * Allow it to call back into on_close
            guard.as_mut().call_unlocked(|| src.close());
        }

        // The pager dispatcher's reference to this object is the only one we completely control.
        // Now that it's gone, we need to make sure that port doesn't end up with an invalid
        // pointer to packet if all external RefPtrs to this object go away.
        // As the Pager dispatcher is going away, we are not content to keep these objects alive
        // indefinitely until messages are read, instead we want to cancel everything as soon as
        // possible to avoid memory leaks. Therefore we will attempt to cancel any queued final
        // packet.
        if *guard.complete_pending() {
            // SAFETY: `packet` is a member of this live state, and is only ever queued in `port`.
            if unsafe { self.port.cancel_queued_packet(self.packet.get()) } {
                // We successfully cancelled the message, so we don't have to worry about
                // free being called, and can immediately break the refptr cycle.
                *guard.as_mut().complete_pending_mut() = false;
            } else {
                // If we failed to cancel the message, then there is a pending call to
                // free. It will cleanup the RefPtr cycle, although only if
                // page_source_closed is true, which should be the case since we performed the
                // close step earlier.
                debug_assert!(*guard.page_source_closed());
            }
        } else {
            // Either the complete message had already been dispatched when this object was closed
            // or free was called between this object being closed and this method
            // taking the lock. In either case, the port no longer has a reference, any RefPtr
            // cycles have been broken and cleanup is already done.
            debug_assert!(guard.page_source().is_none());
        }
        // The pager dispatcher calls on_dispatcher_close when it is going away on zero handles.
        // Clear pager to ensure we don't attempt to access an already-freed pager dispatcher.
        *guard.as_mut().pager_mut() = None;
    }

    /// Callback invoked by [`PortPacket::free`] when the port releases `packet`.
    ///
    /// # Safety
    ///
    /// `allocator` must point to the `allocator` field at offset 0 of a live `PagerProxyState`
    /// with provenance over the entire state, and `port_packet` must be that state's `packet`.
    unsafe extern "C" fn free_packet(
        allocator: *const PortAllocator,
        port_packet: NonNull<PortPacket>,
    ) {
        // SAFETY: Guaranteed by caller. `free` ensures `deferred` holds a strong reference to the
        // `PagerProxy` (or its `PageSource`) whenever `mtx` is released, so the state outlives the
        // temporary `&Self` reference and is only destroyed when `deferred` is dropped below.
        let deferred = unsafe { &*allocator.cast::<Self>() }.free(port_packet);
        drop(deferred);
    }

    /// Handles the port releasing `packet` (see `free_packet`).
    ///
    /// Returns a [`DeferredRelease`] holding references that keep this `PagerProxyState` alive
    /// until after `mtx` has been unlocked and the caller's `&self` borrow has ended.
    #[must_use]
    fn free(&self, _port_packet: NonNull<PortPacket>) -> DeferredRelease {
        let mut deferred = DeferredRelease::default();
        ksync::lock!(let mut guard = self.lock_mtx());
        let active_request = *guard.active_request();
        if active_request != Some(NonNull::from(&self.complete_request)) {
            // Hold a strong reference across the unlock so that if `on_packet_freed_locked`
            // queues `complete_request` while `page_source_closed` is already true, another thread
            // cannot dequeue and free `complete_request` and destroy this state before `guard`
            // finishes unlocking `mtx`.
            deferred.proxy = Some(RefPtr::from_ref(self.proxy()));
            // This request is still active, i.e. it has not been taken back by the PageSource
            // with clear_async_request. So we are responsible for relinquishing ownership of
            // the request.
            if let Some(active_request) = active_request {
                // SAFETY: The provider owns the active request, which remains live until it is
                // taken back by the PageSource (under `mtx`).
                let active_request = unsafe { active_request.as_ref() };
                // Trace flow events require an enclosing duration.
                vm_ktrace_duration!(
                    1,
                    "page_request_queue",
                    "vmo_id" => PageProvider::get_request_vmo_id(active_request),
                    "offset" => PageProvider::get_request_offset(active_request),
                    "length" => PageProvider::get_request_len(active_request),
                    "type" => request_type_label(PageProvider::get_request_type(active_request))
                );
                vm_ktrace_flow_end!(1, "page_request_queue", *guard.flow_id());
                *guard.as_mut().active_request_mut() = None;
            }
            self.on_packet_freed_locked(guard.as_mut().token_mut());
        } else {
            // Freeing the complete_request indicates we have completed a pending action that
            // might have been delaying cleanup.
            *guard.as_mut().complete_pending_mut() = false;
            // Should be nothing else queued.
            debug_assert!(guard.pending_requests().is_empty());
            *guard.as_mut().active_request_mut() = None;
            *guard.as_mut().packet_busy_mut() = false;
            // If the source is closed, we need to do delayed cleanup. Make sure we are not
            // still in the pager's proxy list (if the pager is not closed yet), and then break
            // our refptr cycle.
            if *guard.page_source_closed() {
                debug_assert!(guard.page_source().is_some());
                // If the PagerDispatcher is already closed, the proxy has already been
                // released.
                if let Some(pager) = *guard.pager() {
                    // deferred.proxy could be None if we have ended up racing with
                    // PagerDispatcher::on_zero_handles which calls on_dispatcher_close
                    // *after* removing the proxy from its list. This is fine as the proxy will
                    // be removed from the pager's proxy list either way.
                    // SAFETY: `pager` is only cleared (under `mtx`) before the dispatcher is
                    // destroyed, so it is live while `mtx` is held.
                    let pager = unsafe { pager.as_ref() };
                    // SAFETY: `pager` is the dispatcher that created this proxy: it is set
                    // once from the creating dispatcher in `init` and only ever cleared.
                    deferred.proxy = unsafe { pager.release_proxy(self.proxy(), guard.token()) };
                    // As in on_close: if release_proxy unlinked this proxy, nothing else
                    // clears pager before the dispatcher can go away.
                    *guard.as_mut().pager_mut() = None;
                }
                deferred.src = guard.as_mut().page_source_mut().take();
            } else {
                // Hold a strong reference across the unlock so another thread cannot race to close
                // the page source and destroy this `PagerProxyState` (and `complete_request`)
                // after `complete_pending` was cleared above but before `free` returns.
                deferred.proxy = Some(RefPtr::from_ref(self.proxy()));
            }
        }
        deferred
    }

    /// Called when the packet becomes free. If pending_requests is non-empty, queues the
    /// next request.
    fn on_packet_freed_locked(&self, token: &mut LockToken<'_, PagerProxyStateMtxClass>) {
        let next = {
            // SAFETY: `token` proves that `self.mtx` is held.
            let mut locked = unsafe { self.guard_mtx_mut(token) };
            // We are here because the active request has been freed. And packet_busy is still
            // true, so no new request will have become active yet.
            debug_assert!(locked.active_request().is_none());
            *locked.packet_busy_mut() = false;
            locked.pending_requests_mut().pop_front()
        };
        if let Some(next) = next {
            self.queue_packet_locked(token, next);
        }
    }

    /// Called by the page dispatcher to set the PageSource reference. This is guaranteed to happen
    /// exactly once just after construction.
    ///
    /// # Safety
    ///
    /// Must be called at most once, immediately after construction and before the proxy is
    /// accessible from any other thread. The caller must ensure `on_dispatcher_close` is later
    /// invoked to break the `RefPtr` cycle between the `PagerProxy` and `PageSource`.
    pub unsafe fn set_page_source_unchecked(&self, src: RefPtr<PageSource>) {
        // set_page_source_unchecked is only called by the PagerDispatcher just after
        // construction, unfortunately it needs to be called under the PagerDispatcher lock and lock
        // ordering is always PagerProxy->PagerDispatcher, and so we cannot acquire the lock here.
        // SAFETY: The caller guarantees no other thread can access the proxy yet, so access to the
        // `mtx`-guarded fields is exclusive without holding `mtx`.
        unsafe {
            let mut token = LockToken::<PagerProxyStateMtxClass>::new();
            *self.guard_mtx_mut(&mut token).page_source_mut() = Some(src);
        }
    }

    /// Waits on an `event` associated with a page request. The waiting thread can return early
    /// from the wait due to a suspend signal only if `suspendable` is true.
    fn wait_on_event(&self, event: &Event, suspendable: bool) -> Result<(), Status> {
        let _by = AutoBlocked::new(Blocked::PAGER);
        let mut _memory_stall: Option<ScopedMemoryStall> = None;
        if BootOptions::get().experimental_expand_memory_stall {
            // Note: This counts *all* pager waits as stalls, not just ones resulting from a refault
            // (which is what Linux measures). This can result in false positives, e.g. during boot
            // or app launch when there is a lot of pager activity, we might incorrectly conclude
            // that there is a memory stall even in the absence of memory pressure. However,
            // incorporating this distinction involves non trivial complexity so as an
            // approximation for now just count every pager wait.
            // See https://fxbug.dev/508774420
            _memory_stall = Some(ScopedMemoryStall::new());
        }
        DISPATCHER_PAGER_TOTAL_REQUEST_COUNT.add(1);
        let mut waited: u32 = 0;
        // Ignore the suspend signal if not suspendable.
        let signal_mask = if suspendable { 0 } else { THREAD_SIGNAL_SUSPEND };
        let mut result;
        loop {
            result = event.wait_mask(&make_deadline(), signal_mask);

            if result == Err(Status::INTERRUPTED_RETRY) {
                core::hint::cold_path();
                if suspendable {
                    // Terminate the wait early if suspendable.
                    DISPATCHER_PAGER_FAILED_REQUEST_COUNT.add(1);
                    return result;
                }
                // Count how often we ignore suspend signals as a debugging aid.
                DISPATCHER_PAGER_IGNORED_SUSPEND_COUNT.add(1);
            } else if result == Err(Status::TIMED_OUT) {
                core::hint::cold_path();
                waited += 1;
                // We might trigger this loop multiple times as we exceed multiples of the overtime
                // counter, but we only want to count each unique overtime event in the kcounter.
                if waited == 1 {
                    DISPATCHER_PAGER_OVERTIME_WAIT_COUNT.add(1);
                }

                // Error out if we've been waiting for longer than the specified timeout, to allow
                // the rest of the system to make progress (if possible).
                if waited_too_long(waited) {
                    let src: *const PageSource = {
                        ksync::lock!(let guard = self.lock_mtx());
                        guard.page_source().as_ref().map_or(core::ptr::null(), RefPtr::as_ptr)
                    };
                    kprint::kprint!(
                        "ERROR Page source {:p} blocked for {} seconds. Page request timed out.\n",
                        src,
                        BootOptions::get().userpager_overtime_timeout_seconds
                    );
                    self.dump(0, BootOptions::get().userpager_overtime_printout_limit);

                    // This function is called from the context of waiting on a page request, so we
                    // know that we don't hold any locks. It should be safe to iterate the root job
                    // tree to dump handle info.
                    kprint::kprint!("Dumping all handles for the pager object:\n");
                    dump_handles_for_koid(self.pager_koid);
                    kprint::kprint!("Dumping all handles for the pager port object:\n");
                    dump_handles_for_koid(self.port.get_koid());

                    // SAFETY: This is called in thread context after threading initialization
                    // while waiting on a page request, so the current thread holds no locks
                    // (including its own thread lock) and has no active chain lock transaction.
                    unsafe { ThreadPtr::current().dump(false) };
                    DISPATCHER_PAGER_TIMED_OUT_REQUEST_COUNT.add(1);
                    return Err(Status::TIMED_OUT);
                }

                // Do an informational printout of the source and ourselves if the overtime period
                // has elapsed.
                self.print_overtime(
                    u64::from(waited)
                        .wrapping_mul(BootOptions::get().userpager_overtime_wait_seconds),
                );
            }

            // Hold off on suspension until after the page request is resolved (or fails with a
            // timeout).
            if result != Err(Status::TIMED_OUT) && result != Err(Status::INTERRUPTED_RETRY) {
                break;
            }
        }

        if result.is_ok() {
            DISPATCHER_PAGER_SUCCEEDED_REQUEST_COUNT.add(1);
        } else {
            core::hint::cold_path();
            // Only counts failures that are *not* pager timeouts. Timeouts are tracked with
            // DISPATCHER_PAGER_TIMED_OUT_REQUEST_COUNT, which is updated above when we
            // return early with Status::TIMED_OUT.
            DISPATCHER_PAGER_FAILED_REQUEST_COUNT.add(1);
        }

        result
    }

    /// Helper to do an informational printout if the thread has been waiting on the page request
    /// for too long.
    #[cold]
    #[inline(never)]
    fn print_overtime(&self, waited_seconds: u64) {
        let mut do_printout = false;
        let src: Option<RefPtr<PageSource>>;
        {
            ksync::lock!(let mut guard = self.lock_mtx());
            src = guard.page_source().clone();
            let now = current_mono_time().0;
            let wait_seconds = BootOptions::get().userpager_overtime_wait_seconds as i64;
            if now
                >= guard
                    .last_overtime_dump()
                    .saturating_add(DurationMono::from_seconds(wait_seconds).0)
            {
                do_printout = true;
                *guard.as_mut().last_overtime_dump_mut() = now;
            }
        }
        kprint::kprint!(
            "WARNING Page source {:p} blocked for {} seconds. {:s}\n",
            src.as_ref().map_or(core::ptr::null(), RefPtr::as_ptr),
            waited_seconds,
            if do_printout { "Dump:" } else { "Dump skipped." }
        );
        // Dump out the rest of the state of the outstanding requests.
        if do_printout {
            self.dump(0, BootOptions::get().userpager_overtime_printout_limit);
            if let Some(src) = src {
                // Use dump_self to avoid it calling our dump method that we already performed.
                src.dump_self(0, BootOptions::get().userpager_overtime_printout_limit);
            }
        }
    }

    /// Retrieves the debug name for this page provider.
    fn get_debug_name(&self, name: &mut [u8; ZX_MAX_NAME_LEN]) {
        ksync::lock!(let guard = self.lock_mtx());
        match *guard.pager() {
            Some(pager) => {
                // SAFETY: `pager` is only cleared (under `mtx`) before the dispatcher is destroyed,
                // so it is live while `mtx` is held.
                unsafe { pager.as_ref() }.get_debug_name(name);
            }
            None => {
                name[0] = 0;
            }
        }
    }

    /// Dumps relevant state for debugging purposes. The `max_items` parameter should be used to
    /// cap the number of elements printed from any kind of variable sized list to prevent spam.
    fn dump(&self, depth: u32, max_items: u32) {
        ksync::lock!(let guard = self.lock_mtx());
        let mut printer = dump::DepthPrinter::console(depth as usize);
        let mut name = [0u8; ZX_MAX_NAME_LEN];
        match *guard.pager() {
            Some(pager) => {
                // SAFETY: `pager` is only cleared (under `mtx`) before the dispatcher is destroyed,
                // so it is live while `mtx` is held.
                unsafe { pager.as_ref() }.get_debug_name(&mut name);
            }
            None => {
                let closed = b"<closed>";
                name[..closed.len()].copy_from_slice(closed);
            }
        }
        dump::emit!(
            printer,
            "pager_dispatcher <{}> page_source {:p} key {}",
            name_prefix(&name),
            guard.page_source().as_ref().map_or(core::ptr::null(), RefPtr::as_ptr),
            self.key
        );

        dump::emit!(
            printer,
            "  source_closed {} pager_closed {} packet_busy {} complete_pending {}",
            u8::from(*guard.page_source_closed()),
            u8::from(guard.pager().is_none()),
            u8::from(*guard.packet_busy()),
            u8::from(*guard.complete_pending())
        );

        match *guard.active_request() {
            Some(active_request) => {
                if active_request != NonNull::from(&self.complete_request) {
                    // SAFETY: The provider owns the active request, which remains live until it is
                    // taken back by the PageSource (under `mtx`).
                    let active_request = unsafe { active_request.as_ref() };
                    let offset = PageProvider::get_request_offset(active_request);
                    dump::emit!(
                        printer,
                        "  active {} request on pager port [0x{:x}, 0x{:x}) (port koid 0x{:x})",
                        PageProvider::get_request_type(active_request),
                        offset,
                        offset.wrapping_add(PageProvider::get_request_len(active_request)),
                        self.port.get_koid()
                    );
                } else {
                    dump::emit!(
                        printer,
                        "  active COMPLETE request on pager port (port koid 0x{:x})",
                        self.port.get_koid()
                    );
                }
            }
            None => {
                dump::emit!(printer, "  no active request on pager port");
            }
        }

        if guard.pending_requests().is_empty() {
            dump::emit!(printer, "  no pending requests to queue on pager port");
            return;
        }

        printer.begin_list(max_items as usize);
        for req in guard.pending_requests().iter() {
            if NonNull::from(req) != NonNull::from(&self.complete_request) {
                let offset = PageProvider::get_request_offset(req);
                dump::emit!(
                    printer,
                    "  pending {} req to queue on pager port [0x{:x}, 0x{:x})",
                    PageProvider::get_request_type(req),
                    offset,
                    offset.wrapping_add(PageProvider::get_request_len(req))
                );
            } else {
                dump::emit!(printer, "  pending COMPLETE request to queue on pager port");
            }
        }
        printer.end_list();
    }
}

/// Initializes a `PagerProxyState` in place within the `opaque_storage` of a C++ `PagerProxy`.
///
/// # Safety
///
/// `state` must point to the uninitialized `opaque_storage_` of a C++ `PagerProxy` (at
/// `kPagerProxyStateOffset`) with provenance over the whole storage, which will not move.
/// `dispatcher` must point to a live `PagerDispatcher`, and `port` must be an owned reference to a
/// live `PortDispatcher` (which is transferred).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_state_init(
    state: *mut PagerProxyState,
    dispatcher: *const PagerDispatcher,
    port: *mut PortDispatcher,
    key: u64,
    options: u32,
) {
    // SAFETY: The caller transfers an owned reference to a live `PortDispatcher`.
    let port = unsafe { RefPtr::from_raw(port) };
    // SAFETY: The caller guarantees `dispatcher` is live.
    let dispatcher = unsafe { &*dispatcher };
    let state_ptr = NonNull::new(state).expect("PagerProxyState requires storage");
    let init = PagerProxyState::init(state_ptr, dispatcher, port, key, options);
    // SAFETY: The caller guarantees `state` is valid storage that will not move. The initializer
    // is infallible.
    let Ok(()) = unsafe { init.__pinned_init(state) };
    debug::ltracef!(
        "{:p} key {:x} options {:x}\n",
        state.wrapping_byte_sub(object_constants::kPagerProxyStateOffset).cast::<PagerProxy>(),
        key,
        options
    );
}

/// Destroys a `PagerProxyState` in place.
///
/// # Safety
///
/// `state` must point to an initialized `PagerProxyState` that is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_state_destroy(state: *mut PagerProxyState) {
    // SAFETY: The caller guarantees `state` is initialized and not used again.
    unsafe { core::ptr::drop_in_place(state) };
}

/// FFI for `PagerProxyState::properties`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_properties(state: &PagerProxyState) -> PageSourceProperties {
    state.properties()
}

/// FFI for `PagerProxyState::send_async_request`.
///
/// # Safety
///
/// `request` must not currently be in any `PageProviderTag` list, and must remain valid and owned
/// by this provider until cleared or swapped out (or the provider closes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_send_async_request(
    state: &PagerProxyState,
    request: &PageRequest,
) {
    // SAFETY: Forwarded from caller.
    unsafe { state.send_async_request(request) };
}

/// FFI for `PagerProxyState::clear_async_request`.
///
/// # Safety
///
/// `request` must currently be owned by this provider (either `active_request` or queued in
/// `pending_requests`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_clear_async_request(
    state: &PagerProxyState,
    request: &PageRequest,
) {
    // SAFETY: Forwarded from caller.
    unsafe { state.clear_async_request(request) };
}

/// FFI for `PagerProxyState::swap_async_request`.
///
/// # Safety
///
/// `old` must currently be owned by this provider (either `active_request` or queued in
/// `pending_requests`). `new_req` must not be in any `PageProviderTag` list, and must remain valid
/// and owned by this provider until cleared or swapped out (or the provider closes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_swap_async_request(
    state: &PagerProxyState,
    old: &PageRequest,
    new_req: &PageRequest,
) {
    // SAFETY: Forwarded from caller.
    unsafe { state.swap_async_request(old, new_req) };
}

/// FFI for `PagerProxyState::on_close`.
///
/// # Safety
///
/// `state` must point to an initialized `PagerProxyState`. The state may be destroyed before this
/// function returns, so it is passed as a raw pointer rather than a reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pager_proxy_on_close(state: *const PagerProxyState) {
    // SAFETY: The caller guarantees `state` is initialized. The reference does not outlive this
    // call, after which the deferred references (which may destroy the state) are released.
    let deferred = unsafe { &*state }.on_close();
    drop(deferred);
}

/// FFI for `PagerProxyState::on_detach`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_on_detach(state: &PagerProxyState) {
    state.on_detach();
}

/// FFI for `PagerProxyState::wait_on_event`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_wait_on_event(
    state: &PagerProxyState,
    event: &Event,
    suspendable: bool,
) -> zx_status_t {
    Status::result_into_raw(state.wait_on_event(event, suspendable))
}

/// FFI for `PagerProxyState::dump`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_dump(state: &PagerProxyState, depth: u32, max_items: u32) {
    state.dump(depth, max_items);
}

/// FFI for `PagerProxyState::get_koid`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_get_koid(state: &PagerProxyState) -> zx_koid_t {
    state.get_koid()
}

/// FFI for `PagerProxyState::get_debug_name`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_pager_proxy_get_debug_name(
    state: &PagerProxyState,
    name: &mut [u8; ZX_MAX_NAME_LEN],
) {
    state.get_debug_name(name);
}
