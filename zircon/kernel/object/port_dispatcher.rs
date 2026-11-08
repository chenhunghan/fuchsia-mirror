// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! # PortObserver Lifetime and Locking Notes
//!
//! This is the canonical copy of these notes; `port_dispatcher.h` points here.
//!
//! `PortDispatcher` is responsible for destroying `PortObserver`s (`maybe_reap`
//! or `on_zero_handles`), however, their destruction may be initiated by
//! either `Dispatcher` or `PortDispatcher`.
//!
//! `PortObserver`s can exist on up to two doubly linked lists:
//! - `observers` on `PortDispatcher`
//! - `DoublyLinkedList<SignalObserver*>` on `Dispatcher`
//!
//! The diagrams below show the *relevant* pointers on different
//! states of the system. The pure header view is really the
//! union of all these pointer which can be confusing.
//!
//! rc = ref counted
//! p  = raw pointer
//! o  = owning pointer
//!
//! 1) Situation after `object_wait_async(port, handle)` is issued:
//!
//! ```text
//!                                   list   +--------+
//!          +------p------+      +----p-----+  Port  |
//!          |             v      v          |        |
//!  +-------+--+        +-----------+       +-+------+
//!  | object   |        | Port      |         ^
//!  |          | <--p---+ Observer  |         |
//!  +----------+        |           +---rc----+
//!                      |           |
//!                      +-----------+
//!                      |  Port     |
//!                      |  Packet   |
//!                      +-----------+
//! ```
//!
//!   State changes of the object are propagated from the object
//!   to the port via `p` --> observer --> `rc` calls.
//!
//! 2) Situation after the packet is queued on signal match:
//!
//! ```text
//!                                          +--------+
//!                                          |  Port  |
//!                                          |        |
//!  +----------+        +-----------+       +-+---+--+
//!  | object   |        | Port      |         ^   |
//!  |          |        | Observer  |         |   |
//!  +----------+        |           +---rc----+   |
//!                +---> |           |             |
//!                |     +-----------+             | list
//!                |     |  Port     |             |
//!                +--o--|  Packet   | <-----p-----+
//!                      +-----------+
//! ```
//!
//!   Note that the object and the observer no longer have `p`s to each
//!   other, but the observer still owns the port via `rc`.
//!
//!   The `o` pointer (`PortPacket::observer`) is the observer's only owner
//!   while the packet is queued. Because the packet is embedded in the
//!   observer, releasing it destroys both. This happens when the packet is
//!   dequeued (`dequeue`) or canceled while still queued
//!   (`cancel_queued_packet`, `cancel_queued`, `cancel_key`, or
//!   `on_zero_handles`).
//!
//!   If the wait is instead canceled through the `Dispatcher` before the
//!   signal matches, no packet is queued and `maybe_reap` destroys the
//!   observer directly (unless `on_zero_handles` unlinked it first, in which
//!   case `on_zero_handles` destroys it).
//!
//!   The `PortObserver`'s raw pointer to the `Dispatcher` is a weak reference
//!   guarded by the port's lock. It is cleared in `maybe_reap`,
//!   `PortDispatcher::cancel_key`, or `PortDispatcher::on_zero_handles`.
//!
//! ## Cancelation Ordering
//!
//! Observers can be canceled by way of the object or the port. Observers are
//! canceled by the object when the object's handle count reaches zero and when
//! waits are canceled by `port_cancel`. In this path the `Dispatcher`'s lock is
//! acquired first and the observer is removed from the `Dispatcher`'s observer
//! list, then the `PortDispatcher` lock is acquired and the observer is removed
//! from the `PortDispatcher`'s observer list and may be cleaned up.
//!
//! When an observer is canceled through the port by way of `port_cancel_key` there
//! is a lock ordering issue. The `PortDispatcher` lock must be held to iterate
//! over pending observers but the locks for `Dispatcher`s associated with these
//! observers cannot be acquired while holding the `PortDispatcher` lock. In this
//! case, the `PortDispatcher` first cancels each pending observer with the
//! `PortDispatcher` lock held by assigning the observer's packet's type to a
//! sentinel value (`kPortPacketTypeCanceled`) and removing them from the
//! `PortDispatcher`'s list to a temporary list. Then the `PortDispatcher` lock is
//! released and the observers are deregistered from their `Dispatcher`s
//! (acquiring the `Dispatcher` lock) one at a time. If an observer fires in this
//! state and attempts to queue a packet `PortDispatcher` will check the type
//! field and not queue the packet.
//!
//! ## Locking
//!
//! The `PortDispatcher`'s lock guards `PortDispatcher`'s list of observers, the
//! `PortObserver`'s reference to its `Dispatcher`, and the packet type field on the
//! `PortObserver`'s stored `zx_port_packet_t` in pending observers. The
//! `PortDispatcher` lock can be acquired while holding the `Dispatcher`'s lock.
//!
//! The `Dispatcher`'s lock guards the `Dispatcher`'s list of observers and the
//! other fields on `PortObserver`, most notably the packet itself. The
//! `Dispatcher` lock cannot be acquired while holding the `PortDispatcher` lock.
//!
//! Both the `PortDispatcher` and `Dispatcher` locks are held when delivering a
//! packet and when deciding whether to destroy it in `maybe_reap`.
//!
//! ## Ephemeral Packet Memory Management
//!
//! Ephemeral packets (allocated via `PortPacketCacheAllocator` or external allocators
//! like `PagerProxy`) are queued in `packets`. When canceled, ephemeral packets
//! must be freed outside of the port's lock because external allocators may
//! acquire their own mutex before acquiring the port lock (e.g., `PagerProxy::mtx`).
//! Freeing an ephemeral packet while holding the port lock could result in an ABBA
//! deadlock.

use super::dispatcher::{Dispatcher, impl_dispatcher_facade_with_state};
use super::handle::{HandleRef, KernelHandle};
use super::port_dispatcher_ffi::*;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{AutoBlocked, Blocked};
use crate::ksync_kernel::{TrackedCriticalMutex, TrackedSpinlock};
use boot_options::BootOptions;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{Canary, DefaultObjectTag, DoublyLinkedList, DoublyLinkedListNode, RefPtr, TrackingSize};
use ksync::{KMutex, guarded};
use lazy_init::{LazyInit, NoCheck};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx_status::Status;
use zx_types::{
    ZX_EXCP_POLICY_CODE_PORT_TOO_MANY_OBSERVERS, ZX_EXCP_POLICY_CODE_PORT_TOO_MANY_PACKETS,
    ZX_MAX_NAME_LEN, ZX_OK, ZX_PORT_BIND_TO_INTERRUPT, ZX_SIGNAL_NONE,
    ZX_WAIT_ASYNC_BOOT_TIMESTAMP, ZX_WAIT_ASYNC_TIMESTAMP, zx_instant_boot_t, zx_koid_t,
    zx_packet_type_t, zx_port_packet_t, zx_rights_t, zx_signals_t,
};

use crate::counters::define_kcounter;
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::semaphore::Semaphore;
use crate::kernel::thread::signal_policy_exception;
use crate::object_cache::{ObjectCache, PerCpu, UniquePtr};
use crate::platform_rs::timer::{current_boot_time, current_mono_time};

use object_constants_rs as object_constants;

// Sub-packets overlaid on `zx_port_packet_t::union` (32 bytes).
#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct PacketSignal {
    trigger: zx_signals_t,
    observed: zx_signals_t,
    count: u64,
    timestamp: zx_types::zx_time_t,
    reserved1: u64,
}
zr::static_assert!(size_of::<PacketSignal>() == 32);

#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct PacketInterrupt {
    timestamp: zx_types::zx_time_t,
    reserved: [u64; 3],
}
zr::static_assert!(size_of::<PacketInterrupt>() == 32);

define_kcounter!(PORT_FULL_COUNT, "port.full.count", Sum);
define_kcounter!(PORT_DEQUEUE_COUNT, "port.dequeue.count", Sum);
define_kcounter!(PORT_DEQUEUE_SPURIOUS_COUNT, "port.dequeue.spurious.count", Sum);
define_kcounter!(DISPATCHER_PORT_CREATE_COUNT, "dispatcher.port.create", Sum);
define_kcounter!(DISPATCHER_PORT_DESTROY_COUNT, "dispatcher.port.destroy", Sum);
define_kcounter!(PORT_EPHEMERAL_PACKET_LIVE, "port.ephemeral_packet.live", Sum);
define_kcounter!(PORT_EPHEMERAL_PACKET_ALLOCATED, "port.ephemeral_packet.allocated", Sum);
define_kcounter!(PORT_EPHEMERAL_PACKET_FREED, "port.ephemeral_packet.freed", Sum);
define_kcounter!(PORT_OBSERVER_LIVE, "port.observer.live", Sum);
define_kcounter!(PORT_OBSERVER_ALLOCATED, "port.observer.allocated", Sum);
define_kcounter!(PORT_OBSERVER_FREED, "port.observer.freed", Sum);

/// Sentinel stored in [`RawPortPacket::packet_type`] by [`PortPacket::cancel`].
const PORT_PACKET_TYPE_CANCELED: u32 = 0xffff_ffff;

/// Raw ABI-compatible representation of `zx_port_packet_t` with `packet_type` as a plain `u32`.
///
/// Copying arbitrary user bytes in `zx_port_queue` therefore cannot construct an invalid Rust
/// enum discriminant, and the kernel can store internal sentinel values such as
/// `PORT_PACKET_TYPE_CANCELED` in `packet_type` without polluting the public SDK
/// `zx_packet_type_t`.
///
/// It is layout-identical to `zx_port_packet_t` (asserted below), so the C++ side passes
/// `zx_port_packet_t*` across the FFI boundary and Rust views the storage as this type; no
/// conversion back to the closed `zx_packet_type_t` enum is ever required.
#[repr(C)]
#[derive(
    Debug, Default, Copy, Clone, Eq, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawPortPacket {
    pub key: u64,
    pub packet_type: u32,
    pub status: i32,
    pub union: [u8; 32],
}
zr::static_assert_size_and_align!(RawPortPacket, 48, 8);
zr::static_assert_size_and_align!(
    RawPortPacket,
    size_of::<zx_port_packet_t>(),
    align_of::<zx_port_packet_t>(),
);
const _: () = {
    assert!(
        core::mem::offset_of!(RawPortPacket, key) == core::mem::offset_of!(zx_port_packet_t, key)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, packet_type)
            == core::mem::offset_of!(zx_port_packet_t, packet_type)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, status)
            == core::mem::offset_of!(zx_port_packet_t, status)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, union)
            == core::mem::offset_of!(zx_port_packet_t, union)
    );
};

impl From<zx_port_packet_t> for RawPortPacket {
    #[inline]
    fn from(packet: zx_port_packet_t) -> Self {
        Self {
            key: packet.key,
            packet_type: packet.packet_type as u32,
            status: packet.status,
            union: packet.union,
        }
    }
}

// BUG(53762) Increase from 16k packets to 32k packets.
// TODO(maniscalco): Enforce this limit per process via the job policy.
const MAX_ALLOCATED_PACKET_COUNT_PER_PORT: usize = 4096;

/// Allocator callback table for ephemeral [`PortPacket`]s.
///
/// Packets are returned to their allocator through [`PortPacket::free`].
#[repr(C)]
pub struct PortAllocator {
    free: unsafe extern "C" fn(*const PortAllocator, NonNull<PortPacket>),
}

zr::static_assert!(size_of::<PortAllocator>() == 8);
zr::static_assert!(align_of::<PortAllocator>() == 8);
zr::static_assert!(core::mem::offset_of!(PortAllocator, free) == 0);

// SAFETY: `PortAllocator` only holds a function pointer.
unsafe impl Send for PortAllocator {}
// SAFETY: `PortAllocator` only holds an immutable function pointer.
unsafe impl Sync for PortAllocator {}

impl PortAllocator {
    /// Creates a new `PortAllocator` that invokes `free` when an ephemeral packet is released.
    pub const fn new(
        free: unsafe extern "C" fn(*const PortAllocator, NonNull<PortPacket>),
    ) -> Self {
        Self { free }
    }
}

/// A packet queued on a `PortDispatcher` (`packets`).
///
/// Only `packet` is public. The list node, `handle`, `observer` and `allocator` uphold the
/// invariants that [`Self::free`] and [`Self::reset_observer`] rely on, so they are read through
/// accessors and only written by this module.
#[derive(fbl::DoublyLinkedListContainable)]
#[repr(C)]
pub struct PortPacket {
    #[dll_node]
    node: DoublyLinkedListNode<PortPacket>,
    pub packet: RawPortPacket,
    handle: *const core::ffi::c_void,
    observer: *mut PortObserver,
    allocator: *mut PortAllocator,
}

zr::static_assert_size_and_align!(
    PortPacket,
    object_constants::kPortPacketSize,
    object_constants::kPortPacketAlign,
);
// Both C++ and Rust read and write every field, so pin each offset (mirrored by the C++
// static_asserts in port_dispatcher.cc).
zr::static_assert!(
    core::mem::offset_of!(PortPacket, node) == object_constants::kPortPacketNodeOffset
);
zr::static_assert!(
    core::mem::offset_of!(PortPacket, packet) == object_constants::kPortPacketPacketOffset
);
zr::static_assert!(
    core::mem::offset_of!(PortPacket, handle) == object_constants::kPortPacketHandleOffset
);
zr::static_assert!(
    core::mem::offset_of!(PortPacket, observer) == object_constants::kPortPacketObserverOffset
);
zr::static_assert!(
    core::mem::offset_of!(PortPacket, allocator) == object_constants::kPortPacketAllocatorOffset
);

// SAFETY: `PortPacket` instances can be transferred across threads when queued on or dequeued from
// a `PortDispatcher` or slab cache.
unsafe impl Send for PortPacket {}

impl PortPacket {
    pub fn new(handle: *const core::ffi::c_void, allocator: *mut PortAllocator) -> Self {
        // Note that packet is initialized to zeros.
        Self {
            node: DoublyLinkedListNode::new(),
            packet: RawPortPacket::default(),
            handle,
            observer: core::ptr::null_mut(),
            allocator,
        }
    }

    #[inline]
    pub fn key(&self) -> u64 {
        self.packet.key
    }

    /// Returns the opaque identity token this packet was created with (compared, never
    /// dereferenced, by `cancel_queued`).
    #[inline]
    pub fn handle(&self) -> *const core::ffi::c_void {
        self.handle
    }

    /// Returns the allocator this packet is returned to by [`Self::free`] (null for packets
    /// embedded in a `PortObserver`).
    #[inline]
    pub fn allocator(&self) -> *mut PortAllocator {
        self.allocator
    }

    /// Sets the `zx_port_packet_t` payload on this `PortPacket`.
    ///
    /// Only valid while the packet is exclusively owned by the caller (i.e. before it is queued).
    #[inline]
    pub fn set_packet(&mut self, packet: &zx_port_packet_t) {
        self.packet = RawPortPacket::from(*packet);
    }

    #[inline]
    pub fn is_ephemeral(&self) -> bool {
        !self.allocator.is_null()
    }

    /// Returns the packet at `this` to its allocator if it is ephemeral. Mirrors
    /// `PortPacket::Free()`.
    ///
    /// If the packet is not ephemeral (`allocator` is null), this is a no-op.
    ///
    /// Takes a raw pointer rather than `&mut self` because the allocator deallocates the packet,
    /// which must not happen through a live reference.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `PortPacket` that is not linked in any list, the caller must
    /// own it exclusively, no port lock may be held (allocators such as `PagerProxy` take their
    /// own locks and re-enter the port), and the packet must not be accessed again afterwards if
    /// it is ephemeral. If `(*this).allocator` is non-null, it must point to a live
    /// `PortAllocator` that allocated `this` and whose `free` callback preconditions are
    /// satisfied.
    #[inline]
    pub unsafe fn free(this: *mut Self) {
        // SAFETY: `this` is valid per the caller contract.
        let allocator = unsafe { (*this).allocator };
        if !allocator.is_null() {
            // SAFETY: `allocator` is non-null, so this is an ephemeral packet that was allocated
            // from (and is still owned by) that live `PortAllocator`; the caller has relinquished
            // the packet.
            unsafe {
                let free_fn = (*allocator).free;
                free_fn(allocator, NonNull::new_unchecked(this));
            }
        }
    }

    #[inline]
    pub fn cancel(&mut self) {
        self.packet.packet_type = PORT_PACKET_TYPE_CANCELED;
    }

    #[inline]
    pub fn is_canceled(&self) -> bool {
        self.packet.packet_type == PORT_PACKET_TYPE_CANCELED
    }

    /// Reads `packet.key` directly through a raw pointer without forming a whole-struct
    /// `&PortPacket` reference.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `PortPacket` whose `packet.key` field is valid for reading.
    #[inline]
    pub unsafe fn key_raw(this: *const Self) -> u64 {
        // SAFETY: Guaranteed by caller; projects directly to `packet.key` without forming `&Self`.
        unsafe { (&raw const (*this).packet.key).read() }
    }

    /// Writes `PORT_PACKET_TYPE_CANCELED` directly to `packet.packet_type` through a raw pointer
    /// without forming a whole-struct `&mut PortPacket` reference.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `PortPacket` whose `packet.packet_type` field is exclusively
    /// writable by the caller (e.g., under the owning `PortDispatcher`'s `lock`).
    #[inline]
    pub unsafe fn cancel_raw(this: *mut Self) {
        // SAFETY: Guaranteed by caller; projects directly to `packet.packet_type` without forming
        // `&mut Self`.
        unsafe {
            (&raw mut (*this).packet.packet_type).write(PORT_PACKET_TYPE_CANCELED);
        }
    }

    /// Reads whether `packet.packet_type == PORT_PACKET_TYPE_CANCELED` directly through a raw
    /// pointer without forming a whole-struct `&PortPacket` reference.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `PortPacket` whose `packet.packet_type` field is valid for
    /// reading (e.g., under the owning `PortDispatcher`'s `lock`).
    #[inline]
    pub unsafe fn is_canceled_raw(this: *const Self) -> bool {
        // SAFETY: Guaranteed by caller; projects directly to `packet.packet_type` without forming
        // `&Self`.
        unsafe { (&raw const (*this).packet.packet_type).read() == PORT_PACKET_TYPE_CANCELED }
    }

    /// Destroys the observer owned by the packet at `this`, if any. Mirrors
    /// `packet->observer.reset()`.
    ///
    /// Takes a raw pointer rather than `&mut self` because a non-ephemeral packet is embedded
    /// in its observer, so destroying the observer frees the packet itself.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `PortPacket` that is not linked in any list, the owning
    /// port's lock must be held (it guards `observer`), and the packet must not be accessed
    /// again afterwards if it owned an observer.
    #[inline]
    unsafe fn reset_observer(this: *mut Self) {
        // Mirrors `ktl::move(packet->observer)`: detach the observer, transferring ownership to
        // this frame.
        // SAFETY: `this` is valid and the port lock guarding `observer` is held per the caller
        // contract.
        let observer = unsafe { core::mem::take(&mut (*this).observer) };
        if !observer.is_null() {
            // SAFETY: `observer` was owned by this packet and has been detached, so this is the
            // only remaining reference to it.
            let _destroyer = unsafe { UniquePtr::<PortObserver>::from_raw(observer) };
        }
    }
}

/// Interrupt packet embedded in an `InterruptDispatcher` and queued on `interrupt_packets`.
///
/// The fields are accessed through shared references, so each has a documented guard:
///
/// - The list node and `timestamp` are guarded by the port's `spinlock` while the packet is
///   queued (`queue_interrupt_packet` writes `timestamp`, `dequeue` reads it). Once the owning
///   `InterruptDispatcher` has established under its own spinlock that the packet is not queued
///   (`remove_interrupt_packet` returned, or it was never bound), no port can touch the packet
///   and the owner may read or reset `timestamp` under that spinlock instead.
/// - `key` is written by the owning `InterruptDispatcher` under its spinlock only while the
///   packet is not queued, and read either there or by the port's `dequeue` under the port's
///   `spinlock` immediately after unlinking the packet. The two never overlap because every
///   write is ordered after a `remove_interrupt_packet` / `queue_interrupt_packet` call that
///   took the port's `spinlock`.
#[derive(fbl::DoublyLinkedListContainable)]
#[repr(C)]
pub struct PortInterruptPacket {
    #[dll_node]
    node: DoublyLinkedListNode<PortInterruptPacket>,
    timestamp: UnsafeCell<zx_instant_boot_t>,
    key: UnsafeCell<u64>,
}

impl PortInterruptPacket {
    /// Returns a pin-initializer for an unlinked, zeroed `PortInterruptPacket`.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        Self {
            node: DoublyLinkedListNode::new(),
            timestamp: UnsafeCell::new(0),
            key: UnsafeCell::new(0),
        }
    }

    /// Returns the port packet key.
    ///
    /// # Safety
    ///
    /// Either the owning port's `spinlock` must be held (as in `dequeue`, right after unlinking
    /// the packet), or the packet must not be queued in any `PortDispatcher` and the owning
    /// `InterruptDispatcher`'s spinlock must be held.
    #[inline]
    pub unsafe fn key(&self) -> u64 {
        // SAFETY: The caller holds the lock that excludes the only writer (`set_key`).
        unsafe { *self.key.get() }
    }

    /// Sets the port packet key.
    ///
    /// # Safety
    ///
    /// The packet must not be queued in any `PortDispatcher` (established via
    /// `remove_interrupt_packet` or before the packet is ever queued) and the owning
    /// `InterruptDispatcher`'s spinlock must be held.
    #[inline]
    pub unsafe fn set_key(&self, key: u64) {
        // SAFETY: Unqueued plus the owner's spinlock gives exclusive access per the caller
        // contract.
        unsafe { *self.key.get() = key }
    }

    /// Returns the port packet timestamp.
    ///
    /// # Safety
    ///
    /// Either the owning port's `spinlock` must be held, or the packet must not be queued in any
    /// `PortDispatcher` and the owning `InterruptDispatcher`'s spinlock must be held.
    #[inline]
    pub unsafe fn timestamp(&self) -> zx_instant_boot_t {
        // SAFETY: The caller holds the lock that guards `timestamp` in the packet's current
        // (queued or unqueued) state.
        unsafe { *self.timestamp.get() }
    }

    /// Sets the port packet timestamp.
    ///
    /// # Safety
    ///
    /// Either the owning port's `spinlock` must be held (as in `queue_interrupt_packet`), or the
    /// packet must not be queued in any `PortDispatcher` and the owning `InterruptDispatcher`'s
    /// spinlock must be held.
    #[inline]
    pub unsafe fn set_timestamp(&self, timestamp: zx_instant_boot_t) {
        // SAFETY: The caller holds the lock that guards `timestamp` in the packet's current
        // (queued or unqueued) state.
        unsafe { *self.timestamp.get() = timestamp }
    }
}

/// Per-CPU cache allocator for `PortPacket`s.
static PACKET_ALLOCATOR: LazyInit<ObjectCache<PortPacket, PerCpu>, NoCheck> = LazyInit::uninit();

/// Per-CPU cache allocator for `PortObserver`s.
static OBSERVER_ALLOCATOR: LazyInit<ObjectCache<PortObserver, PerCpu>, NoCheck> =
    LazyInit::uninit();

/// A trivial instance of the default `PortAllocator` for comparisons and to supply the free
/// callback used for ephemeral packets allocated from `PACKET_ALLOCATOR`.
static DEFAULT_PORT_ALLOCATOR: PortAllocator =
    PortAllocator::new(PortPacketCacheAllocator::free_callback);

fn initialize_cache_allocators(_level: init::LkInitLevel) {
    let observer_reserve_pages = BootOptions::get().port_observer_reserve_pages as usize;
    let observer_cache = ObjectCache::<PortObserver, PerCpu>::try_new(observer_reserve_pages)
        .expect("failed to create PortObserver ObjectCache");

    // Reserve 1 page per CPU for servicing ephemeral PortPackets, unless
    // overridden on the command line.
    let packet_reserve_pages = BootOptions::get().port_packet_reserve_pages as usize;
    let packet_cache = ObjectCache::<PortPacket, PerCpu>::try_new(packet_reserve_pages)
        .expect("failed to create PortPacket ObjectCache");

    // SAFETY: Called once during single-threaded kernel init (`LkInitLevel::Kernel`) before any
    // port packets or observers are allocated.
    unsafe {
        OBSERVER_ALLOCATOR.init(observer_cache);
        PACKET_ALLOCATOR.init(packet_cache);
    }
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(
    port_observer_cache_init,
    initialize_cache_allocators,
    init::LK_INIT_LEVEL_KERNEL
);

/// Implements the `PortAllocator` interface and trivially forwards to the `ObjectCache` allocator
/// for `PortPacket`s defined above (`PACKET_ALLOCATOR`).
struct PortPacketCacheAllocator;

impl PortPacketCacheAllocator {
    /// Allocates a packet from the per-cpu packet cache; returns null on failure.
    pub fn alloc(&self) -> *mut PortPacket {
        // SAFETY: `PACKET_ALLOCATOR` is initialized at `LkInitLevel::Kernel` before any port
        // packets can be allocated.
        let cache = unsafe { PACKET_ALLOCATOR.get_unchecked() };
        // SAFETY: The closure initializes every field of `PortPacket` in `slot` and returns
        // `Ok(())`.
        let init = unsafe {
            pin_init::init_from_closure(|slot: *mut PortPacket| -> Result<(), Status> {
                slot.write(PortPacket::new(
                    core::ptr::null(),
                    PortDispatcher::default_port_allocator(),
                ));
                Ok(())
            })
        };
        match cache.allocate_init(init) {
            Ok(packet) => {
                PORT_EPHEMERAL_PACKET_LIVE.add(1);
                PORT_EPHEMERAL_PACKET_ALLOCATED.add(1);
                UniquePtr::into_raw(packet)
            }
            Err(status) => {
                kprint::kprintln!(
                    "WARNING: Could not allocate new port packet: {}",
                    status.into_raw()
                );
                core::ptr::null_mut()
            }
        }
    }

    /// Returns `port_packet` to the per-cpu packet cache.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been returned by [`Self::alloc`], must not be linked in any list,
    /// must be exclusively owned by the caller, and must not be accessed again afterwards.
    pub unsafe fn free(&self, port_packet: *mut PortPacket) {
        PORT_EPHEMERAL_PACKET_LIVE.add(-1);
        PORT_EPHEMERAL_PACKET_FREED.add(1);
        // SAFETY: `port_packet` is a live, unlinked cache packet owned by the caller per the
        // contract above.
        let _destroyer = unsafe { UniquePtr::<PortPacket>::from_raw(port_packet) };
    }

    /// Callback invoked by [`PortPacket::free`] for packets allocated by
    /// `PortPacketCacheAllocator`.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been returned by [`Self::alloc`], must not be linked in any list,
    /// and must be exclusively owned by the caller.
    unsafe extern "C" fn free_callback(
        _allocator: *const PortAllocator,
        port_packet: NonNull<PortPacket>,
    ) {
        // SAFETY: Guaranteed by caller.
        unsafe {
            Self.free(port_packet.as_ptr());
        }
    }
}

#[inline]
fn is_default_allocated_ephemeral(port_packet: &PortPacket) -> bool {
    port_packet.allocator == PortDispatcher::default_port_allocator() && port_packet.is_ephemeral()
}

// Rare diagnostic path: keep it (and the process-name scan) out of the `on_match` /
// `queue_and_remove_observer` instruction stream.
#[cold]
#[inline(never)]
fn raise_packet_limit_exception(koid: zx_koid_t, num_packets: usize) {
    let process = ProcessDispatcher::get_current();
    let mut pname = [0u8; ZX_MAX_NAME_LEN];
    let status = process.get_name(&mut pname);
    debug_assert!(status.is_ok());
    let len = pname.iter().position(|&b| b == 0).unwrap_or(pname.len());
    kprint::kprintln!(
        "KERN: port ({:u}) has {:u} packets ({:s}). Raising exception",
        koid,
        num_packets,
        &pname[..len]
    );
    signal_policy_exception(ZX_EXCP_POLICY_CODE_PORT_TOO_MANY_PACKETS, 0);
}

/// C++ `PortSignalObserver` base subobject (`SignalObserver` subclass).
#[repr(C, align(8))]
pub(super) struct PortSignalObserver {
    _opaque: zr::OpaqueBytes<{ object_constants::kPortSignalObserverSize }>,
}

zr::static_assert_size_and_align!(
    PortSignalObserver,
    object_constants::kPortSignalObserverSize,
    object_constants::kPortSignalObserverAlign,
);

/// Observers are weakly contained in `Dispatcher`s.
#[derive(fbl::DoublyLinkedListContainable)]
#[repr(C, align(8))]
pub struct PortObserver {
    signal_observer: PortSignalObserver,
    packet: UnsafeCell<PortPacket>,
    port: RefPtr<PortDispatcher>,
    /// Guarded by `port`'s lock.
    #[dll_node]
    node: DoublyLinkedListNode<PortObserver>,
    /// Guarded by `port`'s lock.
    /// This is a raw pointer as the `PortObserver` does not have an ownership stake in the
    /// dispatcher.
    dispatcher: UnsafeCell<*mut Dispatcher>,
}

zr::static_assert!(core::mem::offset_of!(PortObserver, signal_observer) == 0);

// SAFETY: `PortObserver` instances can be transferred across threads via `PortDispatcher` and slab
// caches; mutable state is synchronized by the dispatcher or port lock.
unsafe impl Send for PortObserver {}

impl Drop for PortObserver {
    fn drop(&mut self) {
        // SAFETY: `self.signal_observer` was initialized by `cpp_port_signal_observer_init` in
        // `PortObserver::try_new` and is no longer registered with any `Dispatcher`.
        unsafe {
            cpp_port_signal_observer_destroy(&raw mut self.signal_observer);
        }
        PORT_OBSERVER_LIVE.add(-1);
        PORT_OBSERVER_FREED.add(1);
    }
}

impl PortObserver {
    /// Allocates and initializes a new `PortObserver` from the per-CPU observer cache.
    fn try_new(
        options: u32,
        handle: HandleRef<'_>,
        port: RefPtr<PortDispatcher>,
        key: u64,
        signals: zx_signals_t,
    ) -> Result<UniquePtr<Self>, Status> {
        let handle_ptr = handle.as_ptr().cast::<core::ffi::c_void>();
        let dispatcher_ptr = RefPtr::as_ptr(handle.as_handle().dispatcher()).cast_mut();
        debug_assert!(!handle_ptr.is_null());
        debug_assert!(!dispatcher_ptr.is_null());

        // SAFETY: `OBSERVER_ALLOCATOR` is initialized during `LkInitLevel::Kernel` before any
        // `PortObserver` can be created.
        let cache = unsafe { OBSERVER_ALLOCATOR.get_unchecked() };
        // SAFETY: The closure initializes every field of `PortObserver` in `slot` and returns
        // `Ok(())`.
        let init = unsafe {
            pin_init::init_from_closure(move |slot: *mut Self| -> Result<(), Status> {
                let mut packet = PortPacket::new(handle_ptr, core::ptr::null_mut());
                packet.packet.status = ZX_OK;
                packet.packet.key = key;
                packet.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_SIGNAL_ONE as u32;
                packet.packet.union = zerocopy::transmute!(PacketSignal {
                    trigger: signals,
                    ..PacketSignal::default()
                });

                cpp_port_signal_observer_init((&raw mut (*slot).signal_observer).cast(), options);
                (&raw mut (*slot).packet).write(UnsafeCell::new(packet));
                (&raw mut (*slot).port).write(port);
                (&raw mut (*slot).node).write(DoublyLinkedListNode::new());
                (&raw mut (*slot).dispatcher).write(UnsafeCell::new(dispatcher_ptr));
                PORT_OBSERVER_LIVE.add(1);
                PORT_OBSERVER_ALLOCATED.add(1);
                Ok(())
            })
        };
        cache.allocate_init(init)
    }

    /// Invoked by C++ `SignalObserver::OnMatch` under the watched `Dispatcher`'s lock.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live `PortObserver` whose watched `Dispatcher`'s lock is
    /// currently held. This call may destroy `this`, so `this` must not be accessed after it
    /// returns.
    pub(super) unsafe fn on_match(
        this: *mut Self,
        options: u32,
        signals: zx_signals_t,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `this` is valid at entry and its `packet` and `port` fields are accessible under
        // the watched `Dispatcher`'s lock before `queue_and_remove_observer`.
        let packet_ptr = unsafe { (*this).packet.get() };
        if (options & (ZX_WAIT_ASYNC_TIMESTAMP | ZX_WAIT_ASYNC_BOOT_TIMESTAMP)) != 0 {
            core::hint::cold_path();
            // Getting the current time can be somewhat expensive.
            let timestamp = if (options & ZX_WAIT_ASYNC_TIMESTAMP) != 0 {
                current_mono_time().0
            } else {
                current_boot_time().0
            };
            // SAFETY: The watched `Dispatcher`'s lock is held, which guards the embedded packet's
            // signal payload prior to queuing.
            unsafe {
                let union = &mut (*packet_ptr).packet.union;
                let mut signal: PacketSignal = zerocopy::transmute!(*union);
                signal.timestamp = timestamp;
                *union = zerocopy::transmute!(signal);
            }
        }

        // Queuing the packet will transfer ownership of the observer's reference to the
        // `PortDispatcher` to the `observers` container guarded by `lock`. This container
        // including its reference(s) to the `PortDispatcher` could be destroyed by another
        // thread. `queue_and_remove_observer` requires that the `PortDispatcher` be alive for the
        // entire call so we have to retain a reference on this thread until it returns.
        // SAFETY: `this` is valid at entry prior to `queue_and_remove_observer`.
        let port_ref = unsafe { (*this).port.clone() };

        // SAFETY: `packet_ptr` is the packet embedded in `this`, and `this` was created on
        // `port_ref`.
        let status =
            unsafe { port_ref.queue_and_remove_observer(packet_ptr, signals, this, queue_to_own) };
        // The `queue_and_remove_observer` call may have deleted `this`, so it is not safe to
        // access any members now.

        // The packet is not allocated in the packet arena and does not count against the per-port
        // limit so `queue_and_remove_observer` cannot fail due to the packet count. However, the
        // last handle to the port may have been closed so it can still fail with
        // `Status::BAD_HANDLE`. Just ignore `Status::BAD_HANDLE` because there is nothing to be
        // done.
        debug_assert!(
            matches!(
                status,
                Ok(()) | Err(Status::BAD_HANDLE | Status::CANCELED | Status::SHOULD_WAIT)
            ),
            "status {}",
            Status::result_into_raw(status)
        );
    }

    /// Invoked by C++ `SignalObserver::OnCancel` under the watched `Dispatcher`'s lock.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, live `PortObserver` whose watched `Dispatcher`'s lock is
    /// currently held. This call may destroy `this`, so `this` must not be accessed after it
    /// returns.
    pub(super) unsafe fn on_cancel(this: *mut Self, _signals: zx_signals_t) {
        // `maybe_reap` may unlink this observer and release the port lock while a concurrent
        // `on_zero_handles` finishes and drops the last handle's reference; destroying `this`
        // would then drop `port` (the final reference) while `maybe_reap` still holds
        // `&PortDispatcher`. Retain a reference on this thread until the call returns, as
        // `on_match` does above.
        // SAFETY: `this` is valid at entry prior to `maybe_reap`.
        let (port_ref, packet_ptr) = unsafe { ((*this).port.clone(), (*this).packet.get()) };

        // SAFETY: `packet_ptr` is the packet embedded in `this`, and `this` was created on
        // `port_ref`.
        unsafe {
            port_ref.maybe_reap(this, packet_ptr);
        }
        // The `maybe_reap` call may have deleted `this`, so it is not safe to access any members
        // now.
    }

    /// Returns true if this observer was registered on `port` with `key`.
    pub(super) fn matches_key(&self, port: *const core::ffi::c_void, key: u64) -> bool {
        // SAFETY: `packet.key` is immutable after construction in `PortObserver::try_new`. Using
        // `key_raw` avoids forming a whole-struct `&PortPacket` reference while another thread may
        // concurrently write `packet.packet_type` under the port lock.
        let packet_key = unsafe { PortPacket::key_raw(self.packet.get()) };
        key == packet_key && core::ptr::eq(port, RefPtr::as_ptr(&self.port).cast())
    }

    /// Marks the observer's embedded packet canceled (`PortObserver::Cancel`).
    ///
    /// # Safety
    ///
    /// The owning `PortDispatcher`'s `lock` must be held: it guards the packet type field of a
    /// pending observer's packet.
    unsafe fn cancel(&self) {
        debug_assert!(self.port.state().lock.raw_mutex().is_held());
        // SAFETY: The caller holds the port lock that guards the packet type. Using `cancel_raw`
        // avoids forming a whole-struct `&mut PortPacket` reference while another thread may
        // concurrently access `packet.union` or `packet.key` under the watched dispatcher's lock.
        unsafe {
            PortPacket::cancel_raw(self.packet.get());
        }
    }

    /// Manually increment the reference count of our dispatcher.
    ///
    /// # Safety
    ///
    /// The owning `PortDispatcher`'s `lock` must be held: it guards the observer's weak
    /// `dispatcher` pointer, which must still be linked (non-null).
    unsafe fn add_dispatcher_ref_locked(&self) {
        debug_assert!(self.port.state().lock.raw_mutex().is_held());
        // SAFETY: The caller holds the port lock that guards `dispatcher` and guarantees it is
        // still linked and live.
        unsafe {
            let dispatcher = *self.dispatcher.get();
            debug_assert!(!dispatcher.is_null());
            RefPtr::add_ref(&*dispatcher);
        }
    }

    /// Clears and returns the observer's weak `Dispatcher` pointer (null if already unlinked).
    ///
    /// # Safety
    ///
    /// The owning `PortDispatcher`'s `lock` must be held: it guards the observer's `dispatcher`
    /// pointer.
    unsafe fn unlink_dispatcher_locked(&self) -> *mut Dispatcher {
        debug_assert!(self.port.state().lock.raw_mutex().is_held());
        // SAFETY: The caller holds the port lock that guards `dispatcher`.
        unsafe { core::mem::take(&mut *self.dispatcher.get()) }
    }

    /// Deregisters the observer at `observer` from `dispatcher` (`Dispatcher::RemoveObserver`).
    /// Returns `false` if the dispatcher had already removed it.
    ///
    /// # Safety
    ///
    /// `observer` must point to a live `PortObserver` that is not linked in `observers`, and the
    /// owning `PortDispatcher`'s `lock` must *not* be held (lock ordering: `Dispatcher` before
    /// `PortDispatcher`).
    unsafe fn remove_from_dispatcher(observer: *mut Self, dispatcher: &Dispatcher) -> bool {
        let mut unused_signals: zx_signals_t = 0;
        // SAFETY: `observer` is live and `dispatcher` is a valid reference per the caller
        // contract; the port lock is not held, so acquiring the dispatcher lock inside is safe.
        unsafe {
            dispatcher
                .remove_observer((&raw mut (*observer).signal_observer).cast(), &mut unused_signals)
        }
    }
}

zr::static_assert_size_and_align!(
    PortDispatcherState,
    object_constants::kPortDispatcherStateSize,
    object_constants::kPortDispatcherStateAlign,
);

#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PortDispatcherState {
    canary: Canary<{ fbl::magic(b"PORT") }>,
    options: u32,

    #[pin]
    sema: Semaphore,

    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,

    #[guarded_by(lock)]
    zero_handles: bool,

    // Next three members handle the object and manual notifications.
    #[guarded_by(lock)]
    num_ephemeral_packets: usize,

    #[pin]
    #[guarded_by(lock)]
    packets: DoublyLinkedList<*mut PortPacket>,

    // Keeps track of outstanding observers so they can be removed from dispatchers once handle
    // count drops to zero.
    #[pin]
    #[guarded_by(lock)]
    observers: DoublyLinkedList<*mut PortObserver, DefaultObjectTag, TrackingSize>,

    // Next two members handle the interrupt notifications.
    #[mutex]
    spinlock: KMutex<TrackedSpinlock>,

    #[pin]
    #[guarded_by(spinlock)]
    interrupt_packets: DoublyLinkedList<*mut PortInterruptPacket>,
}

impl PortDispatcherState {
    /// Returns the initializer for a port created with `options`.
    ///
    /// `_dispatcher` is part of the signature `impl_dispatcher_state_init!` calls for every
    /// dispatcher state; `PortDispatcherState` has no back-reference to fill in with it.
    pub fn init(
        _dispatcher: *const PortDispatcher,
        options: u32,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            canary: Canary::new(),
            options,
            sema <- Semaphore::new(0),
            lock <- KMutex::init(),
            zero_handles: false.into(),
            num_ephemeral_packets: 0usize.into(),
            packets <- ksync::kcell_init(DoublyLinkedList::new()),
            observers <- ksync::kcell_init(DoublyLinkedList::new()),
            spinlock <- KMutex::init(),
            interrupt_packets <- ksync::kcell_init(DoublyLinkedList::new()),
            _: {
                DISPATCHER_PORT_CREATE_COUNT.add(1);
            },
        })
    }
}

#[pinned_drop]
impl PinnedDrop for PortDispatcherState {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        debug_assert!(*this.zero_handles.get_inner_mut());
        debug_assert_eq!(*this.num_ephemeral_packets.get_inner_mut(), 0);
        DISPATCHER_PORT_DESTROY_COUNT.add(1);
        // `sema` is destroyed by its own `PinnedDrop` once this body returns.
    }
}

// The PortDispatcher implements the port kernel object which is the cornerstone
// for waiting on object changes in Zircon. The PortDispatcher handles 3 usage
// cases:
//  1- Object state change notification: zx_object_wait_async()
//  2- Manual queuing: zx_port_queue()
//  3- Interrupt change notification: zx_interrupt_bind()
//
// This makes the implementation non-trivial. Cases 1 and 2 use the `packets`
// linked list and case 3 uses `interrupt_packets` linked list.
//
// The threads that wish to receive notifications block on dequeue() (which
// maps to zx_port_wait()) and will receive packets from any of the four sources
// depending on what kind of object the port has been 'bound' to.
//
// When a packet from any of the sources arrives to the port, one waiting
// thread unblocks and gets the packet. In all cases `sema` is used to signal
// and manage the waiting threads.
impl_dispatcher_facade_with_state!(
    pub struct PortDispatcher,
    PortDispatcherState,
    zx_types::ZX_OBJ_TYPE_PORT,
    object_constants::kPortDispatcherStateOffset
);

impl PortDispatcher {
    /// Returns a `*const PortDispatcher` pointer suitable for passing to FFI routines.
    #[inline]
    pub fn as_ffi(&self) -> *const Self {
        self as *const Self
    }

    /// Returns a `*mut PortDispatcher` pointer suitable for passing to FFI routines that
    /// take a non-const `PortDispatcher*`.
    #[inline]
    pub fn as_ffi_mut(&self) -> *mut Self {
        self.as_ffi() as *mut Self
    }

    /// Returns the address of the default `PortAllocator` singleton.
    #[inline]
    pub fn default_port_allocator() -> *mut PortAllocator {
        core::ptr::from_ref(&DEFAULT_PORT_ALLOCATOR).cast_mut()
    }

    pub fn default_rights() -> zx_rights_t {
        zx_types::ZX_DEFAULT_PORT_RIGHTS
    }

    /// Creates a new `PortDispatcher` with `options`.
    pub fn create(options: u32) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        if options != 0 && options != ZX_PORT_BIND_TO_INTERRUPT {
            return Err(Status::INVALID_ARGS);
        }

        // SAFETY: `cpp_port_dispatcher_create` initializes both out parameters on `ZX_OK`.
        let (handle, rights) = unsafe {
            KernelHandle::create_with_rights(|h, r| cpp_port_dispatcher_create(options, h, r))
        }?;

        Ok((handle, rights))
    }

    /// Returns `true` if this port was created with `ZX_PORT_BIND_TO_INTERRUPT`.
    pub fn can_bind_to_interrupt(&self) -> bool {
        (self.state().options & ZX_PORT_BIND_TO_INTERRUPT) != 0
    }

    pub fn on_zero_handles(&self) {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.state().lock_lock());
        debug_assert!(!*guard.fields().zero_handles);
        *guard.as_mut().fields_mut().zero_handles = true;

        // Free any queued packets.
        while let Some(packet) = guard.as_mut().fields_mut().packets.as_mut().pop_front() {
            // If the packet is ephemeral, regardless of allocator, free it outside of
            // the lock. Otherwise, reset the observer if it is present.
            // SAFETY: `packet` was just unlinked from `packets` under `lock`, which also guards
            // its observer; it stays valid until it is freed / its observer is reset below.
            if unsafe { (*packet).is_ephemeral() } {
                // SAFETY: As above.
                if is_default_allocated_ephemeral(unsafe { &*packet }) {
                    *guard.as_mut().fields_mut().num_ephemeral_packets -= 1;
                }
                guard.as_mut().call_unlocked(|| {
                    // SAFETY: `packet` is unlinked and exclusively owned here (it is never on any
                    // list while `lock` is dropped, so a concurrent `cancel_queued_packet` sees it
                    // as not queued), and `lock` is not held, so the allocator may take its own
                    // locks.
                    unsafe {
                        PortPacket::free(packet);
                    }
                });
            } else {
                // The reference to the port that the observer holds cannot be the last one
                // because another reference was used to call on_zero_handles, so we don't
                // need to worry about destroying ourselves.
                // SAFETY: `packet` is unlinked, `lock` is held, and the packet is not touched
                // again after its observer (which embeds it) is destroyed.
                unsafe {
                    PortPacket::reset_observer(packet);
                }
            }
        }

        // For each of our outstanding observers, remove them from their dispatchers and destroy
        // them.
        //
        // We could be racing with the dispatcher calling on_match/on_cancel/maybe_reap. Only
        // destroy the observer after remove_observer completes to ensure we don't destroy it out
        // from under the dispatcher.
        while let Some(observer) = guard.as_mut().fields_mut().observers.as_mut().pop_front() {
            // Acquire a reference to the Dispatcher while we're holding our lock to
            // ensure that it is still alive when we call remove_observer below.
            // SAFETY: `observer` was just unlinked from `observers` under `lock`, which guards its
            // `dispatcher` pointer; the pointer is still linked because only this path and
            // `maybe_reap`/`cancel_key` unlink it, all under `lock`. `from_raw` imports the
            // reference taken by `add_dispatcher_ref_locked`.
            let dispatcher: RefPtr<Dispatcher> = unsafe {
                (*observer).add_dispatcher_ref_locked();
                let dispatcher = (*observer).unlink_dispatcher_locked();
                debug_assert!(!dispatcher.is_null());
                RefPtr::from_raw(dispatcher)
            };

            // Don't hold the lock while calling remove_observer because we don't want to create a
            // PortDispatcher-to-Dispatcher lock dependency.
            guard.as_mut().call_unlocked(|| {
                // We cannot assert that remove_observer returns true because it's possible that
                // the Dispatcher removed it before we got here.
                // SAFETY: `observer` is live and unlinked from `observers`, and `lock` is not held.
                unsafe {
                    PortObserver::remove_from_dispatcher(observer, &dispatcher);
                }
                drop(dispatcher);

                // At this point we know the dispatcher no longer has a reference to the observer.
                // SAFETY: `observer` is unlinked from both lists and no longer referenced by the
                // dispatcher, so this is the last reference to it.
                let _destroyer = unsafe { UniquePtr::<PortObserver>::from_raw(observer) };
            });
        }
    }

    /// Queues a user packet.
    ///
    /// `packet` is the kernel view of the caller-supplied `zx_port_packet_t`; its `packet_type`
    /// is ignored and forced to `ZX_PKT_TYPE_USER`.
    pub fn queue_user(&self, packet: &RawPortPacket) -> Result<(), Status> {
        self.state().canary.assert();

        let Some(mut port_packet) = NonNull::new(PortPacketCacheAllocator.alloc()) else {
            core::hint::cold_path();
            return Err(Status::NO_MEMORY);
        };

        // SAFETY: `port_packet` is a freshly allocated, exclusively owned packet.
        unsafe {
            let pkt = port_packet.as_mut();
            pkt.packet = *packet;
            pkt.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;
        }

        // SAFETY: `port_packet` points to a newly allocated valid PortPacket.
        let status = unsafe { self.queue(port_packet) };
        if status.is_err() {
            core::hint::cold_path();
            // SAFETY: `port_packet` was not queued, so we still own it exclusively, it is
            // unlinked, and no port lock is held.
            unsafe {
                PortPacket::free(port_packet.as_ptr());
            }
        }
        status
    }

    /// Removes `port_packet` from this port's interrupt queue if queued, returning `true` if it
    /// was removed.
    ///
    /// The packet's list node is guarded by this port's `spinlock`, so a shared reference is
    /// sufficient; the owning `InterruptDispatcher` keeps the packet alive while it is queued.
    ///
    /// # Safety
    ///
    /// `port_packet` must either not be in any container or be currently queued in this port's
    /// `interrupt_packets` list (calling this with a packet queued in another port is undefined
    /// behavior: it would be unlinked from that port's list without holding its `spinlock`). The
    /// owning `InterruptDispatcher` upholds this by only ever queueing the packet on the port it
    /// is bound to, under its own spinlock.
    pub unsafe fn remove_interrupt_packet(&self, port_packet: &PortInterruptPacket) -> bool {
        ksync::lock!(let mut guard = self.state().lock_spinlock());
        let mut fields = guard.as_mut().fields_mut();
        if port_packet.node.in_container() {
            // SAFETY: `port_packet` is in a container, which per this function's contract must be
            // this port's `interrupt_packets`, and `spinlock` is held.
            unsafe {
                fields.interrupt_packets.as_mut().erase(port_packet);
            }
            return true;
        }
        false
    }

    /// Queues an interrupt packet: links `port_packet` on this port with `timestamp`, returning
    /// `false` if it was already queued.
    ///
    /// The packet's list node and `timestamp` are guarded by this port's `spinlock`, so a shared
    /// reference is sufficient.
    ///
    /// # Safety
    ///
    /// On `true`, the port stores a raw pointer to `port_packet` in its `interrupt_packets` list,
    /// so `port_packet` must stay valid and pinned at its address until it is unlinked again
    /// (by `remove_interrupt_packet` or `dequeue`); otherwise `dequeue` dereferences a dangling
    /// pointer. `port_packet` must either not be in any container or be currently queued in this
    /// port's `interrupt_packets` list, and it must not be concurrently queued on or removed from
    /// another port (its list node is only protected by *this* port's `spinlock`, so a packet
    /// queued on another port would be inspected without holding that port's lock). The owning
    /// `InterruptDispatcher` upholds this by embedding the packet, only ever queueing it on the
    /// port it is bound to under its own spinlock, and removing it from that port before
    /// unbinding or being destroyed.
    pub unsafe fn queue_interrupt_packet(
        &self,
        port_packet: &PortInterruptPacket,
        timestamp: zx_instant_boot_t,
    ) -> bool {
        {
            ksync::lock!(let mut guard = self.state().lock_spinlock());
            let mut fields = guard.as_mut().fields_mut();
            if port_packet.node.in_container() {
                return false;
            }

            // SAFETY: `spinlock` is held, which guards `timestamp` and the packet's list node;
            // per this function's contract `port_packet` is a live, unlinked packet that its
            // owner keeps alive and pinned while queued.
            unsafe {
                port_packet.set_timestamp(timestamp);
                fields
                    .interrupt_packets
                    .as_mut()
                    .push_back_raw(core::ptr::from_ref(port_packet).cast_mut());
            }
        }

        // post() may unblock a waiting thread that will immediately acquire the spinlock.
        // We drop the spinlock before posting to avoid unnecessary spinning.
        self.state().sema.post(None);
        true
    }

    /// Queues a port packet to the port dispatcher.
    ///
    /// # Safety
    ///
    /// `port_packet` must point to a valid, initialized `PortPacket` that is not currently in
    /// any container, allocated from a `PortAllocator` that outlives any queued packets (or
    /// cancels them before destruction). On `Ok(())`, ownership of `port_packet` is transferred
    /// to the port; on `Err(...)`, the caller retains ownership of `port_packet`.
    pub unsafe fn queue(&self, port_packet: NonNull<PortPacket>) -> Result<(), Status> {
        // SAFETY: `port_packet` is valid per this function's contract, and `observer` is null.
        unsafe {
            self.queue_and_remove_observer(
                port_packet.as_ptr(),
                ZX_SIGNAL_NONE,
                core::ptr::null_mut(),
                None,
            )
        }
    }

    /// Queues a packet and removes the observer from the port dispatcher.
    ///
    /// # Safety
    ///
    /// `port_packet` must point to a valid, initialized `PortPacket` that is not currently in
    /// any container.
    /// `observer` must be either null or point to a valid `PortObserver` that was created on
    /// this `PortDispatcher` (via `make_observer`); when non-null, `port_packet` must be the
    /// packet embedded in `observer`.
    unsafe fn queue_and_remove_observer(
        &self,
        port_packet: *mut PortPacket,
        observed: zx_signals_t,
        observer: *mut PortObserver,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) -> Result<(), Status> {
        self.state().canary.assert();

        // This pointer is declared before the guard because we want the destructor to execute
        // outside the critical section below (if it ends up being the last/only references).
        let mut destroyer: Option<UniquePtr<PortObserver>> = None;

        let status = {
            ksync::lock!(let mut guard = self.state().lock_lock());

            let status = self.queue_packet_locked(guard.as_mut(), port_packet, observed);

            if !observer.is_null() {
                self.maybe_reap_locked(guard.as_mut(), observer, port_packet, &mut destroyer);
            }

            status
        };

        drop(destroyer);

        status?;

        // Assert that we haven't deleted ourselves by dropping our lock.
        self.state().canary.assert();

        // If post() unblocks a thread, that thread will attempt to acquire the lock. We drop
        // the lock before calling post() to allow the unblocked thread to acquire the lock
        // without blocking.
        self.state().sema.post(queue_to_own);
        Ok(())
    }

    fn queue_packet_locked(
        &self,
        mut guard: Pin<&mut PortDispatcherStateLockGuard<'_>>,
        port_packet: *mut PortPacket,
        observed: zx_signals_t,
    ) -> Result<(), Status> {
        self.state().canary.assert();

        let mut fields = guard.as_mut().fields_mut();

        if *fields.zero_handles {
            core::hint::cold_path();
            return Err(Status::BAD_HANDLE);
        }

        // We have to acquire lock before we can check the canceled flag.
        // SAFETY: `port_packet` is non-null and `lock` is held.
        if unsafe { PortPacket::is_canceled_raw(port_packet) } {
            core::hint::cold_path();
            return Err(Status::CANCELED);
        }

        // Evaluated once: `allocator` is immutable for the packet's lifetime, and hoisting it above
        // `push_back_raw` lets the compiler keep the result in a register across the list write.
        // SAFETY: `port_packet` is non-null.
        let is_default_ephemeral = is_default_allocated_ephemeral(unsafe { &*port_packet });
        if is_default_ephemeral
            && *fields.num_ephemeral_packets > MAX_ALLOCATED_PACKET_COUNT_PER_PORT
        {
            core::hint::cold_path();
            PORT_FULL_COUNT.add(1);
            raise_packet_limit_exception(self.get_koid(), *fields.num_ephemeral_packets);
            // The usermode caller sees the exception, not the return code.
            return Err(Status::SHOULD_WAIT);
        }

        if observed != ZX_SIGNAL_NONE {
            // SAFETY: `port_packet` is non-null and `lock` is held.
            let union = unsafe { &mut (*port_packet).packet.union };
            let mut signal: PacketSignal = zerocopy::transmute!(*union);
            signal.observed = observed;

            // count previously stored the number of pending messages on
            // a channel. It is now deprecated, but we set it to 1 for backwards
            // compatibility, so that readers attempt to read at least 1 message and
            // continue to make progress.
            signal.count = 1;
            *union = zerocopy::transmute!(signal);
        }
        // SAFETY: `port_packet` is non-null and unlinked.
        unsafe {
            fields.packets.as_mut().push_back_raw(port_packet);
        }
        if is_default_ephemeral {
            *fields.num_ephemeral_packets += 1;
        }
        Ok(())
    }

    /// Dequeues a packet from this port, waiting up to `deadline`.
    ///
    /// Blocks until a packet is available (or `deadline` passes); `packet` is only written on
    /// success (`Ok(())`), after which it is initialized.
    pub fn dequeue(
        &self,
        deadline: &Deadline,
        packet: &mut MaybeUninit<RawPortPacket>,
    ) -> Result<(), Status> {
        self.state().canary.assert();

        loop {
            // Wait until one of the queues has a packet.
            {
                let _by = AutoBlocked::new(Blocked::PORT);
                self.state().sema.wait(deadline)?;
            }

            // Interrupt packets are higher priority so service the interrupt packet queue first.
            if self.state().options == ZX_PORT_BIND_TO_INTERRUPT {
                ksync::lock!(let mut guard = self.state().lock_spinlock());
                let mut fields = guard.as_mut().fields_mut();
                if let Some(port_interrupt_packet) = fields.interrupt_packets.as_mut().pop_front() {
                    // SAFETY: `port_interrupt_packet` was just unlinked from the queue under
                    // `spinlock`, which guards its `timestamp`; its owner keeps it alive until
                    // `remove_interrupt_packet` and only touches `key` while it is not queued.
                    let (key, timestamp) = unsafe {
                        let port_interrupt_packet = &*port_interrupt_packet;
                        (port_interrupt_packet.key(), port_interrupt_packet.timestamp())
                    };
                    packet.write(RawPortPacket {
                        key,
                        packet_type: zx_packet_type_t::ZX_PKT_TYPE_INTERRUPT as u32,
                        status: ZX_OK,
                        union: zerocopy::transmute!(PacketInterrupt {
                            timestamp,
                            reserved: [0; 3]
                        }),
                    });
                    break;
                }
            }

            // No interrupt packets queued. Check the regular packets.
            let dequeued = {
                ksync::lock!(let mut guard = self.state().lock_lock());
                let mut fields = guard.as_mut().fields_mut();
                if let Some(port_packet) = fields.packets.as_mut().pop_front() {
                    // SAFETY: `port_packet` was just unlinked from `packets` under `lock`, which
                    // also guards its observer; it stays valid until `reset_observer` below.
                    let pkt = unsafe { &*port_packet };
                    if is_default_allocated_ephemeral(pkt) {
                        *fields.num_ephemeral_packets -= 1;
                    }
                    debug_assert!(!pkt.is_canceled());
                    // Copy straight into the caller's buffer while the packet is still alive.
                    packet.write(pkt.packet);

                    let is_ephemeral = pkt.is_ephemeral();
                    // The reference to the port that the observer holds cannot be the last one
                    // because another reference was used to call dequeue, so we don't need to
                    // worry about destroying ourselves.
                    // SAFETY: `port_packet` is unlinked, `lock` is held, and the packet is not
                    // used again below unless it is ephemeral (and thus has no observer).
                    unsafe {
                        PortPacket::reset_observer(port_packet);
                    }

                    Some((port_packet, is_ephemeral))
                } else {
                    None
                }
            };

            if let Some((port_packet, is_ephemeral)) = dequeued {
                // If the packet is ephemeral, free it outside of the lock. We need to read
                // is_ephemeral inside the lock because it's possible for a non-ephemeral packet
                // to get deleted after a call to maybe_reap() as soon as we release the lock.
                if is_ephemeral {
                    // SAFETY: an ephemeral packet is owned by its allocator and is still alive
                    // here; we own it exclusively now that it is unlinked, and `lock` has been
                    // released.
                    unsafe {
                        PortPacket::free(port_packet);
                    }
                }
                break;
            }

            // Both queues were empty. The packet must have been removed before we were able to
            // dequeue. Loop back and wait again.
            core::hint::cold_path();
            PORT_DEQUEUE_SPURIOUS_COUNT.add(1);
        }

        PORT_DEQUEUE_COUNT.add(1);
        Ok(())
    }

    /// This method determines the observer's fate. Upon return, one of the following will
    /// have occurred:
    ///
    /// 1. The observer is destroyed.
    ///
    /// 2. The observer is linked to an already queued packet and will be destroyed when
    ///    the packet is destroyed (dequeue() or cancel_queued_packet()).
    ///
    /// 3. The observer is left for on_zero_handles to be destroyed.
    ///
    /// # Safety
    ///
    /// `observer` must point to a valid `PortObserver` that was created on this
    /// `PortDispatcher` (via `make_observer`), so that it is linked in this port's `observers`
    /// list while its dispatcher reference is live.
    /// `port_packet` must point to a valid `PortPacket` and must be the packet embedded in
    /// `observer`.
    unsafe fn maybe_reap(&self, observer: *mut PortObserver, port_packet: *mut PortPacket) {
        self.state().canary.assert();

        // This pointer is declared before the guard because we want the destructor to execute
        // outside the critical section below (if it ends up being the last/only references).
        let mut destroyer: Option<UniquePtr<PortObserver>> = None;

        {
            ksync::lock!(let mut guard = self.state().lock_lock());

            self.maybe_reap_locked(guard.as_mut(), observer, port_packet, &mut destroyer);
        }

        drop(destroyer);
    }

    // Unlinks an observer from its dispatcher and remove it from our observer
    // list if it is not already removed. This may place the observer into
    // `destroyer` for the caller to destroy after releasing `lock`.
    //
    // This method determines the observer's fate. Upon return, one of the following will
    // have occurred:
    //
    // 1. The observer is destroyed.
    //
    // 2. The observer is linked to an already queued packet and will be destroyed when
    //    the packet is destroyed (dequeue() or cancel_queued_packet()).
    //
    // 3. The observer is left for on_zero_handles to be destroyed.
    fn maybe_reap_locked(
        &self,
        mut guard: Pin<&mut PortDispatcherStateLockGuard<'_>>,
        observer: *mut PortObserver,
        port_packet: *mut PortPacket,
        destroyer: &mut Option<UniquePtr<PortObserver>>,
    ) {
        self.state().canary.assert();

        // If the packet's already canceled, we do not have to do anything. The canceling thread
        // is responsible for reaping the observer - see PortDispatcher::cancel_key().
        // SAFETY: `port_packet` is non-null and `lock` is held.
        if unsafe { PortPacket::is_canceled_raw(port_packet) } {
            return;
        }

        // We may be racing with on_zero_handles. Whichever one of us unlinks the dispatcher will be
        // responsible for ensuring the observer is cleaned up.
        // SAFETY: `observer` is non-null and `guard` proves `lock` (which guards the observer's
        // `dispatcher` pointer) is held.
        let dispatcher = unsafe { (*observer).unlink_dispatcher_locked() };
        if !dispatcher.is_null() {
            let mut fields = guard.as_mut().fields_mut();
            // SAFETY: `observer` is non-null and, having been created on this port and still
            // holding its dispatcher reference, is linked in this port's `observers`.
            unsafe {
                fields.observers.as_mut().erase(&*observer);
            }

            // If the packet is queued, then the observer will be destroyed by dequeue() or
            // cancel_queued_packet().
            // SAFETY: `port_packet` is non-null.
            debug_assert!(!unsafe { (*port_packet).is_ephemeral() });
            // SAFETY: `port_packet` is non-null.
            if unsafe { (*port_packet).node.in_container() } {
                // SAFETY: `port_packet` is non-null.
                debug_assert!(unsafe { (*port_packet).observer.is_null() });
                // SAFETY: `port_packet` is non-null.
                unsafe {
                    (*port_packet).observer = observer;
                }
            } else {
                // Otherwise, it'll be destroyed after this method returns.
                debug_assert!(destroyer.is_none());
                // SAFETY: `observer` is unlinked from `observers` and its packet was not queued,
                // so this frame now owns the last reference to `observer`.
                *destroyer = Some(unsafe { UniquePtr::from_raw(observer) });
            }
        } // else on_zero_handles must have beat us and is responsible for destroying this observer.
    }

    /// Creates an observer registered with this port.
    ///
    /// `handle` is a [`HandleRef`], so the caller necessarily holds a read lock on the handle
    /// table containing it; the observer records `handle` and its dispatcher.
    pub fn make_observer(
        &self,
        options: u32,
        handle: HandleRef<'_>,
        key: u64,
        signals: zx_signals_t,
    ) -> Result<NonNull<PortObserver>, Status> {
        self.state().canary.assert();

        // Called under the handle table lock.
        let port_ref = RefPtr::from_ref(self);
        let observer = PortObserver::try_new(options, handle, port_ref, key, signals)?;
        // SAFETY: `UniquePtr::into_raw` returns a non-null pointer.
        let observer = unsafe { NonNull::new_unchecked(UniquePtr::into_raw(observer)) };

        {
            ksync::lock!(let mut guard = self.state().lock_lock());
            let mut fields = guard.as_mut().fields_mut();
            debug_assert!(!*fields.zero_handles);

            // If we're over the limit, raise an exception.
            if fields.observers.len() >= BootOptions::get().max_port_observers as usize {
                core::hint::cold_path();
                // We limit the number of observers to prevent a misbehaving program from
                // impacting system performance or stability.
                signal_policy_exception(ZX_EXCP_POLICY_CODE_PORT_TOO_MANY_OBSERVERS, 0);
            }

            // SAFETY: `observer` is a freshly allocated, unlinked observer and `lock` is held.
            unsafe {
                fields.observers.as_mut().push_front_raw(observer.as_ptr());
            }
        }

        Ok(observer)
    }

    // Cancel all queued port packets matching `handle` (if not null) and `key`.
    // Any canceled ephemeral packets are moved to `free_list`; the caller must drain
    // and free() these with `lock` released.
    // Returns true if any packets were canceled.
    fn cancel_queued_packets_locked(
        &self,
        mut guard: Pin<&mut PortDispatcherStateLockGuard<'_>>,
        handle: *const core::ffi::c_void,
        key: u64,
        mut free_list: Pin<&mut DoublyLinkedList<*mut PortPacket>>,
    ) -> bool {
        let mut packet_removed = false;

        // This loop can take a while if there are many items.
        // In practice, the number of pending signal packets is
        // approximately the number of signaled _and_ watched
        // objects plus the number of pending user-queued
        // packets.
        //
        // There are two strategies to deal with too much
        // looping here if that is seen in practice.
        //
        // 1. Swap the packets list for an empty list and
        //    release the lock. New arriving packets are
        //    added to the empty list while the loop happens.
        //    Readers will be blocked but the watched objects
        //    will be fully operational. Once processing
        //    is done the lists are appended.
        //
        // 2. Segregate user packets from signal packets
        //    and deliver them in order via timestamps or
        //    a side structure.

        let mut fields = guard.as_mut().fields_mut();
        let mut cursor = fields.packets.as_mut().cursor_front_mut();
        while let Some(pkt) = cursor.get() {
            if (handle.is_null() || pkt.handle == handle) && pkt.key() == key {
                let packet = cursor.erase().unwrap();

                // SAFETY: `packet` was just unlinked from `packets` under `lock`, which also
                // guards its observer; it stays valid until it is staged on `free_list` / its
                // observer is reset below. Only field reads go through the raw pointer: a
                // non-ephemeral packet is embedded in its observer, so no whole-struct reference
                // may outlive `reset_observer`.
                if unsafe { (*packet).is_ephemeral() } {
                    // Ephemeral packets do not have an observer to clean them up. It is up
                    // to us to free them; otherwise they are unlinked from packets and
                    // leak for the life of the port.
                    // We cannot free them here because we are holding lock: external
                    // allocators like PagerProxy take their own locks and re-enter this
                    // port during free(). Instead we stage the packets in a list and hand it
                    // back to the caller, which frees them once the lock is released.
                    // SAFETY: As above.
                    debug_assert!(unsafe { (*packet).observer.is_null() });
                    // SAFETY: As above.
                    if is_default_allocated_ephemeral(unsafe { &*packet }) {
                        *fields.num_ephemeral_packets -= 1;
                    }

                    // cancel_queued_packet checks both in_container and !is_canceled;
                    // by calling PortPacket::cancel_raw() we can safely add it to free_list.
                    // SAFETY: `packet` is unlinked from `packets`, exclusively owned by this
                    // frame (and then the caller) until it is freed, and `lock` is held while
                    // the type field is written.
                    unsafe {
                        PortPacket::cancel_raw(packet);
                        free_list.as_mut().push_front_raw(packet);
                    }
                } else {
                    // Destroyed as we go around the loop.
                    // SAFETY: `packet` is unlinked, `lock` is held, and the packet is not touched
                    // again after its observer (which embeds it) is destroyed.
                    unsafe {
                        PortPacket::reset_observer(packet);
                    }
                }

                packet_removed = true;
            } else {
                cursor.move_next();
            }
        }

        packet_removed
    }

    /// Returns `true` if at least one packet matching `handle` and `key` was removed from the
    /// queue. Any matching ephemeral `PortPacket`s are removed from the queue and freed.
    ///
    /// `handle` is a `HandleRef`, so the caller necessarily holds the handle table lock.
    #[inline]
    pub fn cancel_queued(&self, handle: HandleRef<'_>, key: u64) -> bool {
        self.cancel_queued_handle_key(handle.as_ptr().cast(), key)
    }

    /// Cancels queued packets matching the opaque `handle` token and `key`.
    ///
    /// `handle` is only ever compared against the token each packet was created with (never
    /// dereferenced), so like [`Self::cancel_queued`] this is safe to call with any non-null
    /// token (e.g. an allocator-owned identity such as the hypervisor `TrapMap`'s).
    #[inline]
    pub fn cancel_queued_handle(&self, handle: NonNull<core::ffi::c_void>, key: u64) -> bool {
        self.cancel_queued_handle_key(handle.as_ptr(), key)
    }

    /// Returns true if at least one packet was removed from the queue.
    /// Called under the handle table lock when `handle` is not null.
    /// Any matching ephemeral `PortPacket`s are removed from the queue and freed.
    ///
    /// `handle` is an opaque identity token that is compared, never dereferenced: C++ callers
    /// such as `TrapMap` pass a non-`Handle` pointer, so this takes a raw pointer; Rust callers
    /// should use [`Self::cancel_queued`] or [`Self::cancel_queued_handle`].
    fn cancel_queued_handle_key(&self, handle: *const core::ffi::c_void, key: u64) -> bool {
        self.state().canary.assert();

        stack_pin_init!(let free_list = DoublyLinkedList::<*mut PortPacket>::new());
        let packet_removed;
        {
            ksync::lock!(let mut guard = self.state().lock_lock());
            packet_removed =
                self.cancel_queued_packets_locked(guard.as_mut(), handle, key, free_list.as_mut());
        }

        // Free ephemeral packets with the lock dropped (see cancel_queued_packets_locked).
        while let Some(packet) = free_list.as_mut().pop_front() {
            // SAFETY: `packet` was canceled and unlinked from `packets` under `lock`, is owned
            // exclusively by this frame, and `lock` has been released.
            unsafe {
                PortPacket::free(packet);
            }
        }

        packet_removed
    }

    /// Removes `port_packet` from this port's queue. Returns false if the packet was
    /// not in this queue. It is undefined to call this with a packet queued in another
    /// port.
    ///
    /// # Safety
    ///
    /// `port_packet` must point to a valid, initialized `PortPacket`, and must either not be in
    /// any container or be currently queued in this port's `packets` list (calling this with a
    /// packet queued in another port is undefined behavior: it would be unlinked from that
    /// port's list without holding its `lock`). If `port_packet` owns an attached `PortObserver`
    /// (i.e. is a non-ephemeral packet to which `maybe_reap_locked` has transferred observer
    /// ownership by setting `port_packet.observer`), `port_packet` is freed when this returns
    /// `true` and must not be accessed afterwards.
    pub unsafe fn cancel_queued_packet(&self, port_packet: *mut PortPacket) -> bool {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.state().lock_lock());
        let mut fields = guard.as_mut().fields_mut();

        // Check is_canceled_raw() in addition to in_container(): cancel_key/
        // cancel_queued_packets_locked may move ephemeral packets onto a stack-local
        // free_list before unlocking and freeing them, during which in_container()
        // is true even though the packet has been unlinked from packets.
        //
        // is_canceled_raw() must be evaluated first: the type field is only written under `lock`,
        // so it is synchronized here, whereas a canceled packet's list node is relinked by the
        // free_list drain in cancel_queued_handle_key/cancel_key *after* `lock` is released.
        // Short-circuiting on the canceled flag therefore never reads a node another thread may
        // be writing.
        // SAFETY: `port_packet` is non-null and `lock` is held.
        if unsafe {
            !PortPacket::is_canceled_raw(port_packet) && (*port_packet).node.in_container()
        } {
            // SAFETY: `port_packet` is non-null.
            if is_default_allocated_ephemeral(unsafe { &*port_packet }) {
                *fields.num_ephemeral_packets -= 1;
            }
            // SAFETY: `port_packet` is non-null and in a container, which per this function's
            // contract must be this port's `packets`. `lock` is held and the packet is not
            // touched again after its observer (which embeds it) is destroyed. The caller's
            // pointer (not the one `erase` re-derives from a shared reference) is handed to
            // `reset_observer`, which writes through it.
            unsafe {
                fields.packets.as_mut().erase(&*port_packet);
                PortPacket::reset_observer(port_packet);
            }
            return true;
        }

        false
    }

    /// Cancel all pending waits and packets registered with `key`.
    /// Returns `Ok(())` if any waits or packets were canceled and `Err(Status::NOT_FOUND)`
    /// if no matching waits or packets were found.
    pub fn cancel_key(&self, key: u64) -> Result<(), Status> {
        self.state().canary.assert();

        stack_pin_init!(
            let canceled_observers =
                DoublyLinkedList::<*mut PortObserver, DefaultObjectTag, TrackingSize>::new()
        );
        stack_pin_init!(let free_list = DoublyLinkedList::<*mut PortPacket>::new());
        let observer_canceled;
        let packet_removed;
        {
            ksync::lock!(let mut guard = self.state().lock_lock());

            {
                let mut fields = guard.as_mut().fields_mut();
                let mut cursor = fields.observers.as_mut().cursor_front_mut();
                while let Some(obs) = cursor.get() {
                    if obs.matches_key(self.as_ffi().cast(), key) {
                        // SAFETY: `guard` proves `lock` is held, which guards the packet type
                        // field `cancel` writes.
                        unsafe {
                            obs.cancel();
                        }
                        // At this point, since we're still holding our lock we know that all of
                        // the observers in canceled_observers point to live dispatchers. We can't
                        // hold the port dispatcher lock while calling in to dispatchers in order to
                        // remove this observer due to lock ordering. So, we're about to drop our
                        // lock temporarily for each observer.
                        //
                        // Another thread could be trying to destroy a dispatcher instance and
                        // close out all of its registered observers. It will call on_cancel() on
                        // each observer in its list which will acquire the port dispatcher's lock,
                        // but will not find any observers that we've moved to our local
                        // canceled_observers list.
                        //
                        // To ensure that all of the dispatchers referenced by the canceled
                        // observers are still alive, we must acquire references while still holding
                        // the port dispatcher lock. This ensures that when we go through the loop
                        // below and release and re-acquire the port dispatcher lock we will always
                        // find a live dispatcher.
                        // SAFETY: `guard` proves `lock` is held, which guards the observer's
                        // `dispatcher` pointer; it is still linked because the observer is still
                        // in `observers`.
                        unsafe {
                            obs.add_dispatcher_ref_locked();
                        }
                        let item = cursor.erase().unwrap();
                        // SAFETY: `item` is non-null and unlinked.
                        unsafe {
                            canceled_observers.as_mut().push_front_raw(item);
                        }
                    } else {
                        cursor.move_next();
                    }
                }
            }

            observer_canceled = !canceled_observers.is_empty();
            packet_removed = self.cancel_queued_packets_locked(
                guard.as_mut(),
                core::ptr::null(),
                key,
                free_list.as_mut(),
            );

            while let Some(canceled_observer) = canceled_observers.as_mut().pop_front() {
                // We've already incremented the reference count to each dispatcher in the loop
                // above, so here we can import into a RefPtr instead of creating a new reference.
                // SAFETY: `canceled_observer` is non-null and `guard` proves `lock` (which
                // guards its `dispatcher` pointer) is held. The pointer was AddRef'd above and is
                // still linked because only this path unlinks observers on
                // `canceled_observers`.
                let dispatcher = unsafe {
                    RefPtr::<Dispatcher>::from_raw((*canceled_observer).unlink_dispatcher_locked())
                };
                guard.as_mut().call_unlocked(|| {
                    // SAFETY: `canceled_observer` is live and unlinked from `observers`, and
                    // `lock` is not held.
                    unsafe {
                        PortObserver::remove_from_dispatcher(canceled_observer, &dispatcher);
                    }
                    drop(dispatcher);

                    // SAFETY: `canceled_observer` is unlinked from both lists and no longer
                    // referenced by the dispatcher, so this is the last reference to it.
                    let _destroyer =
                        unsafe { UniquePtr::<PortObserver>::from_raw(canceled_observer) };
                });
            }
        }

        // Free ephemeral packets with the lock dropped. `PortPacket::free()` dispatches to the
        // allocator, and `PagerProxy::free_packet` takes the proxy's `mtx`, which
        // `PagerProxy::clear_async_request` acquires *before* the port lock. Freeing under `lock`
        // would be an ABBA deadlock.
        while let Some(packet) = free_list.as_mut().pop_front() {
            // SAFETY: `packet` was canceled and unlinked from `packets` under `lock`, is owned
            // exclusively by this frame, and `lock` has been released.
            unsafe {
                PortPacket::free(packet);
            }
        }

        if observer_canceled || packet_removed { Ok(()) } else { Err(Status::NOT_FOUND) }
    }
}

#[cfg(ktest)]
#[unittest::suite(name = "port")]
/// Tests for PortDispatcher.
mod tests {
    // `dequeue()` cannot be exercised here: it wraps the wait in `ThreadDispatcher::AutoBlocked`,
    // which requires a current user thread, and kernel unittests run on bare kernel threads.
    // Dequeue is covered by the userspace core tests (`zircon/system/utest/core/port`).
    use super::super::event_dispatcher::EventDispatcher;
    use super::super::handle::HandleOwner;
    use super::{
        PORT_EPHEMERAL_PACKET_FREED, PORT_OBSERVER_FREED, PortAllocator, PortDispatcher,
        PortInterruptPacket, PortPacket, PortPacketCacheAllocator, RawPortPacket,
    };
    use core::ptr::NonNull;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_err, assert_false, assert_ok, assert_true, unwrap_ok, unwrap_some,
    };
    use zx_status::Status;
    use zx_types::{
        ZX_EVENT_SIGNALED, ZX_PORT_BIND_TO_INTERRUPT, zx_packet_type_t, zx_port_packet_t,
    };

    /// Returns the number of ephemeral packets ever returned to the default cache allocator.
    fn ephemeral_packets_freed() -> i64 {
        PORT_EPHEMERAL_PACKET_FREED.sum_across_all_cpus()
    }

    /// Returns the number of `PortObserver`s ever destroyed.
    fn observers_freed() -> i64 {
        PORT_OBSERVER_FREED.sum_across_all_cpus()
    }

    /// Allocates a packet from the default cache allocator and queues it on `port` with `key`.
    fn queue_cache_packet(port: &PortDispatcher, key: u64) -> NonNull<PortPacket> {
        let mut packet = NonNull::new(PortPacketCacheAllocator.alloc()).unwrap();
        // SAFETY: `packet` was just allocated from the default cache allocator, is valid and
        // not yet shared with anyone else.
        unsafe {
            let pkt = packet.as_mut();
            pkt.packet.key = key;
            pkt.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;
            port.queue(packet).unwrap();
        }
        packet
    }

    /// Creates an event with a real `Handle` so a `PortObserver` can be registered on it.
    fn make_event_handle() -> HandleOwner {
        let (event, rights) = EventDispatcher::create().unwrap();
        HandleOwner::make(event, rights).unwrap()
    }

    /// Tests creating a port with invalid and valid options.
    #[test]
    fn test_create_and_options() {
        assert_err!(PortDispatcher::create(0x1234), Status::INVALID_ARGS);
        let (handle, rights) = unwrap_ok!(PortDispatcher::create(0));
        assert_eq!(rights, PortDispatcher::default_rights());
        assert_false!(handle.dispatcher().can_bind_to_interrupt());

        let (handle_irq, _) = unwrap_ok!(PortDispatcher::create(ZX_PORT_BIND_TO_INTERRUPT));
        assert_true!(handle_irq.dispatcher().can_bind_to_interrupt());
    }

    /// Tests that cancelling by key unlinks an ephemeral packet AND returns it to its allocator.
    #[test]
    fn test_cancel_key_frees_ephemeral_packets() {
        use core::cell::Cell;

        #[repr(C)]
        struct CountingPortAllocator {
            allocator: PortAllocator,
            free_count: Cell<usize>,
        }

        /// # Safety
        ///
        /// `allocator` must point to the `allocator` field at offset 0 of a valid
        /// `CountingPortAllocator`.
        unsafe extern "C" fn counting_free(
            allocator: *const PortAllocator,
            _packet: NonNull<PortPacket>,
        ) {
            // SAFETY: Guaranteed by caller.
            let state = unsafe { &*allocator.cast::<CountingPortAllocator>() };
            state.free_count.set(state.free_count.get() + 1);
        }

        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();

        let counting_allocator = CountingPortAllocator {
            allocator: PortAllocator::new(counting_free),
            free_count: Cell::new(0),
        };
        let allocator_ptr: *mut PortAllocator =
            core::ptr::from_ref(&counting_allocator).cast_mut().cast();

        const CANCEL_KEY: u64 = 0xF1;
        const KEEP_KEY: u64 = 0x72;

        let mut cancel_pkt = PortPacket::new(core::ptr::null(), allocator_ptr);
        cancel_pkt.packet.key = CANCEL_KEY;
        cancel_pkt.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;

        let mut keep_pkt = PortPacket::new(core::ptr::null(), allocator_ptr);
        keep_pkt.packet.key = KEEP_KEY;
        keep_pkt.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;

        // SAFETY: `cancel_pkt` and `keep_pkt` are valid stack-allocated `PortPacket`s that outlive
        // `handle`.
        unsafe {
            assert_ok!(port.queue(NonNull::from(&mut cancel_pkt)));
            assert_ok!(port.queue(NonNull::from(&mut keep_pkt)));
        }
        assert_eq!(counting_allocator.free_count.get(), 0);

        // Cancelling `CANCEL_KEY` must free only `cancel_pkt` and leave `keep_pkt` queued.
        assert_ok!(port.cancel_key(CANCEL_KEY));
        assert_eq!(counting_allocator.free_count.get(), 1);

        // Subsequent cancellation with `CANCEL_KEY` should find nothing.
        assert_err!(port.cancel_key(CANCEL_KEY), Status::NOT_FOUND);
        assert_eq!(counting_allocator.free_count.get(), 1);

        // Dropping the port handle runs `on_zero_handles`, which frees `keep_pkt`.
        drop(handle);
        assert_eq!(counting_allocator.free_count.get(), 2);

        // Also exercise the default per-CPU cache allocator through `cancel_key`.
        let (handle2, _) = unwrap_ok!(PortDispatcher::create(0));
        let port2 = handle2.dispatcher();
        let freed_before = ephemeral_packets_freed();
        let mut cache_pkt = unwrap_some!(NonNull::new(PortPacketCacheAllocator.alloc()));
        // SAFETY: `cache_pkt` was just allocated from the default cache allocator and is uniquely
        // owned prior to `queue`.
        unsafe {
            let pkt = cache_pkt.as_mut();
            assert_true!(pkt.is_ephemeral());
            assert_eq!(pkt.allocator(), PortDispatcher::default_port_allocator());
            assert_true!(pkt.handle().is_null());
            pkt.packet.key = CANCEL_KEY;
            pkt.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;
            assert_ok!(port2.queue(cache_pkt));
        }
        assert_ok!(port2.cancel_key(CANCEL_KEY));
        assert_true!(ephemeral_packets_freed() > freed_before);
        assert_err!(port2.cancel_key(CANCEL_KEY), Status::NOT_FOUND);
    }

    /// Tests queueing a user packet via `zx_port_packet_t` and cancelling it by key.
    #[test]
    fn test_queue_user_and_cancel_key() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();

        const KEY: u64 = 12345;
        let freed_before = ephemeral_packets_freed();

        let packet = RawPortPacket::from(zx_port_packet_t {
            key: KEY,
            // User input should be overridden to ZX_PKT_TYPE_USER.
            packet_type: zx_packet_type_t::ZX_PKT_TYPE_SIGNAL_ONE,
            status: 0,
            union: [0; 32],
        });
        assert_ok!(port.queue_user(&packet));

        // Nothing is queued under a different key.
        assert_err!(port.cancel_key(KEY + 1), Status::NOT_FOUND);

        // The user packet was allocated from the default cache allocator by `queue_user`, so
        // cancelling it must unlink it and return it to the allocator.
        assert_ok!(port.cancel_key(KEY));
        assert_true!(ephemeral_packets_freed() > freed_before);
        assert_err!(port.cancel_key(KEY), Status::NOT_FOUND);
    }

    /// Tests that `on_zero_handles` frees every still-queued ephemeral packet.
    #[test]
    fn test_on_zero_handles_frees_queued_packets() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();

        let freed_before = ephemeral_packets_freed();
        queue_cache_packet(port, 1);
        queue_cache_packet(port, 2);

        // Dropping the last handle runs `on_zero_handles`, which must return both packets to
        // the allocator (with the port lock dropped around each `free`).
        drop(handle);
        assert_true!(ephemeral_packets_freed() >= freed_before + 2);
    }

    /// Tests that `cancel_queued_packet` unlinks without freeing and is `false` once unqueued.
    #[test]
    fn test_cancel_queued_packet() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();

        let freed_before = ephemeral_packets_freed();
        let packet = queue_cache_packet(port, 7);

        // SAFETY: `packet` is a live cache packet queued on `port`.
        unsafe {
            assert_true!(port.cancel_queued_packet(packet.as_ptr()));
            // The packet was handed back to us (unlinked, not canceled, not freed).
            assert_false!(packet.as_ref().is_canceled());
            // A second cancellation finds the packet no longer queued.
            assert_false!(port.cancel_queued_packet(packet.as_ptr()));
        }
        // The key is gone from the port as well.
        assert_err!(port.cancel_key(7), Status::NOT_FOUND);

        // SAFETY: `packet` is unlinked and exclusively owned by this test.
        unsafe {
            PortPacketCacheAllocator.free(packet.as_ptr());
        }
        assert_true!(ephemeral_packets_freed() > freed_before);
    }

    /// Tests that `cancel_queued_handle` only matches packets carrying the same identity token.
    #[test]
    fn test_cancel_queued_handle_matches_token() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();

        const KEY: u64 = 9;
        queue_cache_packet(port, KEY);

        // Any object address serves as an identity token (TrapMap uses its allocator's).
        let token_storage = 0u8;
        let token = NonNull::from(&token_storage).cast::<core::ffi::c_void>();
        assert_false!(port.cancel_queued_handle(token, KEY));

        // The packet is still queued and is reclaimed by key.
        assert_ok!(port.cancel_key(KEY));
    }

    /// Tests that `on_zero_handles` deregisters and destroys a live (unfired) observer.
    #[test]
    fn test_on_zero_handles_with_live_observer() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let event = make_event_handle();

        const KEY: u64 = 0x0b5e;
        let freed_before = observers_freed();
        assert_ok!(event.dispatcher_ref().make_port_observer(
            0,
            event.as_ref(),
            ZX_EVENT_SIGNALED,
            KEY,
            handle.dispatcher()
        ));

        drop(handle);
        assert_true!(observers_freed() > freed_before);

        // The observer is gone from the event, so this must not try to queue onto the dead port.
        assert_ok!(event.dispatcher_ref().user_signal_self(0, ZX_EVENT_SIGNALED));
    }

    /// Tests that `on_zero_handles` destroys the observer owning a queued signal packet.
    #[test]
    fn test_on_zero_handles_with_queued_signal_packet() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let event = make_event_handle();

        const KEY: u64 = 0x0b5f;
        let freed_before = observers_freed();
        assert_ok!(event.dispatcher_ref().make_port_observer(
            0,
            event.as_ref(),
            ZX_EVENT_SIGNALED,
            KEY,
            handle.dispatcher()
        ));
        // Fire the observer: `on_match` queues the embedded packet and `maybe_reap` hands the
        // observer to the packet, which now owns it.
        assert_ok!(event.dispatcher_ref().user_signal_self(0, ZX_EVENT_SIGNALED));

        drop(handle);
        assert_true!(observers_freed() > freed_before);
    }

    /// Tests interrupt packet queue/remove round trips, including re-queueing after removal.
    #[test]
    fn test_interrupt_packet_queue_and_remove() {
        let (handle1, _) = unwrap_ok!(PortDispatcher::create(ZX_PORT_BIND_TO_INTERRUPT));
        let (handle2, _) = unwrap_ok!(PortDispatcher::create(ZX_PORT_BIND_TO_INTERRUPT));
        let port1 = handle1.dispatcher();
        let port2 = handle2.dispatcher();

        // Stands in for the packet an `InterruptDispatcher` embeds; this test is its sole owner,
        // so there is no owner spinlock to hold and the "not queued" half of the accessor
        // contracts is what the SAFETY comments below establish.
        stack_pin_init!(let packet = PortInterruptPacket::init());
        let packet: &PortInterruptPacket = &packet;

        // SAFETY: The packet is not queued on any port and is exclusively owned by this test.
        unsafe {
            packet.set_key(42);
            assert_eq!(packet.key(), 42);
            assert_eq!(packet.timestamp(), 0);
        }

        // Not yet queued: remove returns false.
        // SAFETY: The packet is not in any container.
        unsafe {
            assert_false!(port1.remove_interrupt_packet(packet));
        }

        // Queue onto port1; the timestamp is recorded under the port spinlock.
        // SAFETY: The packet is pinned on this test's stack, outlives both ports' use of it (it is
        // unlinked below before the test returns), is not in any container, and is exclusively
        // owned by this test, so nothing queues it concurrently.
        unsafe {
            assert_true!(port1.queue_interrupt_packet(packet, 1234));
        }

        // Cannot queue twice while in container, and the first timestamp is kept. (Queueing on
        // a different port while linked here is outside `queue_interrupt_packet`'s contract, so
        // it is deliberately not exercised.)
        // SAFETY: The packet is queued on exactly this port and nothing else touches it.
        unsafe {
            assert_false!(port1.queue_interrupt_packet(packet, 5678));
        }

        // Remove from port1 succeeds exactly once.
        // SAFETY: The packet is queued on `port1` (and then in no container).
        unsafe {
            assert_true!(port1.remove_interrupt_packet(packet));
            assert_false!(port1.remove_interrupt_packet(packet));
        }

        // SAFETY: The packet is unlinked again and exclusively owned by this test.
        unsafe {
            assert_eq!(packet.timestamp(), 1234);
            assert_eq!(packet.key(), 42);
        }

        // The packet can be re-queued on another port once unlinked, and the key is readable
        // again afterwards.
        // SAFETY: The packet is unlinked, still pinned on this test's stack for the duration of
        // the queueing below, and exclusively owned by this test.
        unsafe {
            assert_true!(port2.queue_interrupt_packet(packet, 9));
        }
        // SAFETY: The packet is queued on `port2`.
        unsafe {
            assert_true!(port2.remove_interrupt_packet(packet));
        }
        // SAFETY: The packet is unlinked and exclusively owned by this test.
        unsafe {
            assert_eq!(packet.timestamp(), 9);
        }
    }

    /// Tests `cancel_key` against a live observer and against a queued signal packet.
    #[test]
    fn test_cancel_key_observers() {
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();
        let event = make_event_handle();

        const KEY: u64 = 0x0c00;
        let freed_before = observers_freed();

        // Live observer: cancel_key must deregister and destroy it.
        assert_ok!(event.dispatcher_ref().make_port_observer(
            0,
            event.as_ref(),
            ZX_EVENT_SIGNALED,
            KEY,
            port
        ));
        assert_ok!(port.cancel_key(KEY));
        assert_true!(observers_freed() > freed_before);
        assert_err!(port.cancel_key(KEY), Status::NOT_FOUND);

        // Queued signal packet: cancel_key must unlink the packet and destroy its observer.
        let freed_before = observers_freed();
        assert_ok!(event.dispatcher_ref().make_port_observer(
            0,
            event.as_ref(),
            ZX_EVENT_SIGNALED,
            KEY,
            port
        ));
        assert_ok!(event.dispatcher_ref().user_signal_self(0, ZX_EVENT_SIGNALED));
        assert_ok!(port.cancel_key(KEY));
        assert_true!(observers_freed() > freed_before);
        assert_err!(port.cancel_key(KEY), Status::NOT_FOUND);
    }

    /// Tests that `Free` during `on_zero_handles` drops the port lock and can re-enter the port.
    #[test]
    fn test_cancel_queued_from_free_during_on_zero_handles() {
        use core::cell::Cell;
        use fbl::RefPtr;

        #[repr(C)]
        struct FreeContext<'a> {
            allocator: PortAllocator,
            freed: Cell<usize>,
            first_packet: *mut PortPacket,
            second_packet: *mut PortPacket,
            port: &'a PortDispatcher,
            cancel_result: Cell<bool>,
        }

        /// # Safety
        ///
        /// `allocator` must point to the `allocator` field at offset 0 of a valid `FreeContext`.
        unsafe extern "C" fn on_free(allocator: *const PortAllocator, packet: NonNull<PortPacket>) {
            // SAFETY: `allocator` is the `allocator` field at offset 0 of a valid `FreeContext`.
            let state = unsafe { &*allocator.cast::<FreeContext<'_>>() };
            state.freed.set(state.freed.get() + 1);
            if packet.as_ptr() == state.first_packet {
                // Re-enter the port from within Free(). The port must not be holding its lock
                // here, and the second packet must still be observably queued.
                // SAFETY: `state.second_packet` is a valid `PortPacket` queued on `state.port`.
                let canceled = unsafe { state.port.cancel_queued_packet(state.second_packet) };
                state.cancel_result.set(canceled);
            }
        }

        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port: RefPtr<PortDispatcher> = handle.dispatcher().clone();

        let mut a = PortPacket::new(core::ptr::null(), core::ptr::null_mut());
        a.packet.key = 1;
        a.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;
        let mut b = PortPacket::new(core::ptr::null(), core::ptr::null_mut());
        b.packet.key = 2;
        b.packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_USER as u32;
        let a_ptr: *mut PortPacket = &raw mut a;
        let b_ptr: *mut PortPacket = &raw mut b;

        let ctx = FreeContext {
            allocator: PortAllocator::new(on_free),
            freed: Cell::new(0),
            first_packet: a_ptr,
            second_packet: b_ptr,
            port: &port,
            cancel_result: Cell::new(false),
        };
        let allocator: *mut PortAllocator = core::ptr::from_ref(&ctx).cast_mut().cast();

        // SAFETY: `a_ptr` and `b_ptr` point to valid stack-pinned `PortPacket`s that outlive
        // `port`, and writing through `a_ptr` / `b_ptr` preserves their Stacked Borrows provenance.
        unsafe {
            (*a_ptr).allocator = allocator;
            (*b_ptr).allocator = allocator;
            assert_ok!(port.queue(NonNull::new_unchecked(a_ptr)));
            assert_ok!(port.queue(NonNull::new_unchecked(b_ptr)));
        }

        // Dropping the last handle runs `on_zero_handles`, which pops `a` first and frees it with
        // the port lock dropped; `on_free(a)` cancels `b`.
        drop(handle);

        // `b` was still queued when `on_free(a)` ran, so `cancel_queued_packet(b)` succeeded and
        // `b` is now owned by the test again rather than freed by the port.
        assert_true!(ctx.cancel_result.get());
        assert_eq!(ctx.freed.get(), 1);
        assert_false!(b.node.in_container());
        assert_false!(b.is_canceled());

        // The port must have nothing further to do with `b`.
        // SAFETY: `b_ptr` is a valid unlinked `PortPacket`.
        assert_false!(unsafe { port.cancel_queued_packet(b_ptr) });
        // SAFETY: `b_ptr` is unlinked and owned by the test, and `ctx` is still live.
        unsafe {
            PortPacket::free(b_ptr);
        }
        assert_eq!(ctx.freed.get(), 2);
    }
}
