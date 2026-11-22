// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor guest trap map and blocking port packet allocator.

use crate::ksync_kernel::{KSpinlock, TrackedSpinlock};
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use fbl::{
    Recyclable, RefPtr, UniquePtr, WavlTree, WavlTreeContainable, WavlTreeKeyable, WavlTreeNode,
};
use ksync::{KCell, LockClass};
use pin_init::{PinInit, pin_data, pin_init};
use range_check::intersects_offset_len;
use zr::static_assert;
use zx_status::Status;
#[cfg(target_arch = "x86_64")]
use zx_types::ZX_GUEST_TRAP_IO;
use zx_types::{ZX_GUEST_TRAP_BELL, ZX_GUEST_TRAP_MEM, zx_gpaddr_t, zx_port_packet_t};

use crate::kernel::deadline::Deadline;
use crate::kernel::semaphore::Semaphore;
use crate::object::{PortAllocator, PortDispatcher, PortPacket};
use crate::object_cache::{self, ObjectCache};

use super::ktrace::{VcpuBlockOp, VcpuMeta, ktrace_vcpu};
use super::state_invalidator::Invalidate;

const MAX_PACKETS_PER_RANGE: usize = 256;

/// Number of empty slabs each [`BlockingPortAllocator`] retains in its [`ObjectCache`].
///
/// Large enough to hold all `MAX_PACKETS_PER_RANGE` packets, so slabs backing packets are not
/// returned to the PMM until the `Trap` is destroyed: once a trap has had a given number of
/// packets outstanding, later allocations up to that count cannot fail with `Status::NO_MEMORY`.
const PACKET_CACHE_RESERVE_SLABS: usize =
    MAX_PACKETS_PER_RANGE.div_ceil(ObjectCache::<PortPacket>::OBJECTS_PER_SLAB);

#[allow(clippy::needless_bool)]
#[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
fn valid_range(kind: u32, addr: zx_gpaddr_t, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    #[cfg(target_arch = "x86_64")]
    if kind == ZX_GUEST_TRAP_IO && end > u16::MAX as usize {
        return false;
    }
    true
}

/// Blocks on allocation if all `MAX_PACKETS_PER_RANGE` packets are outstanding.
#[pin_data]
#[repr(C)]
pub struct BlockingPortAllocator {
    allocator: PortAllocator,
    #[pin]
    semaphore: Semaphore,
    #[pin]
    cache: ObjectCache<PortPacket>,
}

static_assert!(offset_of!(BlockingPortAllocator, allocator) == 0);

impl BlockingPortAllocator {
    /// Creates an in-place pinned initializer for `BlockingPortAllocator`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            allocator: PortAllocator::new(Self::free_packet),
            semaphore <- Semaphore::new(MAX_PACKETS_PER_RANGE as i64),
            cache <- ObjectCache::new(PACKET_CACHE_RESERVE_SLABS),
        })
    }

    /// Callback invoked by [`PortPacket::free`] when a packet allocated by this
    /// `BlockingPortAllocator` is released.
    ///
    /// # Safety
    ///
    /// `allocator` must point to the `allocator` field at offset 0 of a live
    /// `BlockingPortAllocator` with provenance over the entire `BlockingPortAllocator`, and
    /// `port_packet` must satisfy the `# Safety` requirements of [`Self::free`].
    unsafe extern "C" fn free_packet(
        allocator: *const PortAllocator,
        port_packet: NonNull<PortPacket>,
    ) {
        // SAFETY: The caller guarantees `allocator` is the `allocator` field of a live
        // `BlockingPortAllocator` and that `port_packet` satisfies `Self::free`'s contract.
        unsafe {
            (*allocator.cast::<Self>()).free(port_packet);
        }
    }

    /// Waits on the semaphore and allocates a `PortPacket` from the cache.
    pub fn alloc_blocking(&self) -> Option<NonNull<PortPacket>> {
        ktrace_vcpu(VcpuBlockOp::Block, VcpuMeta::Port);
        let result = self.semaphore.wait(&Deadline::infinite());
        ktrace_vcpu(VcpuBlockOp::Unblock, VcpuMeta::Port);
        if result.is_err() {
            return None;
        }
        self.alloc()
    }

    /// Allocates a `PortPacket` from the cache without waiting on the semaphore.
    pub(super) fn alloc(&self) -> Option<NonNull<PortPacket>> {
        let this = NonNull::from(self);
        // `this` is both the packet's identity token (`handle`) and its `PortAllocator`.
        // `BlockingPortAllocator` cancels all queued packets in `Trap::recycle` before being
        // destroyed. Deriving the `PortAllocator` pointer from `NonNull::from(self)` (with
        // `allocator` at offset 0) preserves provenance over the entire `BlockingPortAllocator`
        // when `BlockingPortAllocator::free_packet` casts `allocator` back to
        // `BlockingPortAllocator*`.
        // SAFETY: The closure initializes every field of `PortPacket` in `slot` and returns
        // `Ok(())`.
        let init = unsafe {
            pin_init::init_from_closure(move |slot: *mut PortPacket| -> Result<(), Status> {
                slot.write(PortPacket::new(
                    this.as_ptr().cast_const().cast(),
                    this.as_ptr().cast(),
                ));
                Ok(())
            })
        };
        let packet = self.cache.allocate_init(init).ok()?;
        NonNull::new(object_cache::UniquePtr::into_raw(packet))
    }

    /// Frees `port_packet` back to the cache and posts to the semaphore.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been allocated by this `BlockingPortAllocator` and not yet freed,
    /// must not be linked in any list, must be exclusively owned by the caller, and must not be
    /// accessed again afterwards.
    pub unsafe fn free(&self, port_packet: NonNull<PortPacket>) {
        // SAFETY: The caller guarantees `port_packet` was returned by `alloc` (which leaked it
        // from an `object_cache::UniquePtr<PortPacket>` allocated from `self.cache`), has not yet
        // been freed, is unlinked and exclusively owned, and is not accessed again, so dropping
        // the reconstructed `UniquePtr` destroys the packet and returns its slot to `self.cache`.
        drop(unsafe { object_cache::UniquePtr::<PortPacket>::from_raw(port_packet.as_ptr()) });
        self.semaphore.post(None);
    }

    fn as_handle_ptr(&self) -> NonNull<c_void> {
        NonNull::from(self).cast()
    }
}

/// Describes a single trap within a guest.
#[derive(WavlTreeContainable)]
#[pin_data]
pub struct Trap {
    #[pin]
    #[wavl_node]
    wavl_node: WavlTreeNode<Trap>,
    // Safety invariant: Holds the non-null root `SharedReadWrite` allocation pointer of this
    // pinned `Trap` instance; for any `Trap` inserted in a `TrapMap`, the allocation remains live
    // with no `&mut Trap` aliases until that `TrapMap` is dropped.
    self_ptr: NonNull<Trap>,
    kind: u32,
    addr: zx_gpaddr_t,
    len: usize,
    port: Option<RefPtr<PortDispatcher>>,
    key: u64, // Key for packets in this port range.
    #[pin]
    port_allocator: BlockingPortAllocator,
}

// SAFETY: `Trap` is synchronized internally (`PortDispatcher` is thread-safe and
// `BlockingPortAllocator` synchronizes via `Semaphore` and `ObjectCache`'s internal locks).
unsafe impl Send for Trap {}
// SAFETY: Shared references to `Trap` only read immutable fields or call thread-safe methods on
// `PortDispatcher` and `BlockingPortAllocator`.
unsafe impl Sync for Trap {}

impl WavlTreeKeyable<zx_gpaddr_t> for Trap {
    type Key<'a> = zx_gpaddr_t;

    unsafe fn get_key(&self) -> zx_gpaddr_t {
        self.addr
    }
}

unsafe impl Recyclable for Trap {
    unsafe fn recycle(ptr: NonNull<Self>) {
        // Cancel any queued packets via a shared reference before `Box::from_non_null` creates an
        // exclusive `&mut Trap` (which under Stacked/Tree Borrows would invalidate the shared-ref
        // tag stored in `(*packet).allocator` during `cancel_queued_handle` -> `PortPacket::free`
        // -> `BlockingPortAllocator::free_packet`).
        // SAFETY: `ptr` points to a valid, exclusively owned `Trap` about to be destroyed.
        unsafe {
            let trap = ptr.as_ref();
            if let Some(port) = trap.port.as_ref() {
                // `trap.port_allocator.as_handle_ptr()` is the allocator token used for ephemeral
                // packets queued by this `Trap`; the port only compares it, never dereferences it.
                let _ = port.cancel_queued_handle(trap.port_allocator.as_handle_ptr(), trap.key);
            }
            let _ = kalloc::Box::from_non_null(ptr);
        }
    }
}

#[allow(clippy::len_without_is_empty)]
impl Trap {
    /// Creates an in-place pinned initializer for `Trap`.
    fn new(
        kind: u32,
        addr: zx_gpaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> impl PinInit<Self, Infallible> {
        pin_init!(&this in Self {
            wavl_node: WavlTreeNode::new(),
            self_ptr: this,
            kind,
            addr,
            len,
            port,
            key,
            port_allocator <- BlockingPortAllocator::new(),
        })
    }

    /// Returns the root heap allocation pointer for `self` without narrowing provenance through a
    /// shared reference.
    #[inline]
    pub(super) fn as_mut_ptr(&self) -> *mut Self {
        self.self_ptr.as_ptr()
    }

    /// Queues `packet` on the trap's port, optionally invalidating `invalidator` first.
    pub fn queue(
        &self,
        packet: &zx_port_packet_t,
        invalidator: Option<&mut dyn Invalidate>,
    ) -> Result<(), Status> {
        if let Some(invalidator) = invalidator {
            invalidator.invalidate();
        }
        let Some(port) = self.port.as_ref() else {
            core::hint::cold_path();
            return Err(Status::NOT_FOUND);
        };
        let mut port_packet = self.port_allocator.alloc_blocking().ok_or(Status::NO_MEMORY)?;
        // SAFETY: `port_packet` is uniquely owned by this caller prior to `port.queue`.
        unsafe { port_packet.as_mut() }.set_packet(packet);
        // SAFETY: `port_packet` was allocated from `self.port_allocator`, which cancels all queued
        // packets in `Trap::recycle` before `port_allocator` is destroyed.
        let mut status = unsafe { port.queue(port_packet) };
        if let Err(ref mut err) = status {
            core::hint::cold_path();
            // SAFETY: `port.queue` failed, so ownership of `port_packet` remains with the caller.
            unsafe {
                self.port_allocator.free(port_packet);
            }
            if *err == Status::BAD_HANDLE {
                // If the last handle to the port has been closed, then we're in a bad state.
                *err = Status::BAD_STATE;
            }
        }
        status
    }

    /// Returns `true` if `val` falls within `[addr, addr + len)`.
    #[inline]
    pub fn contains(&self, val: zx_gpaddr_t) -> bool {
        val >= self.addr && val < self.addr + self.len
    }

    /// Returns `true` if this trap has an associated port.
    #[inline]
    pub fn has_port(&self) -> bool {
        self.port.is_some()
    }

    /// Returns the trap kind (`ZX_GUEST_TRAP_*`).
    #[inline]
    pub fn kind(&self) -> u32 {
        self.kind
    }

    /// Returns the starting address of the trap range.
    #[inline]
    pub fn addr(&self) -> zx_gpaddr_t {
        self.addr
    }

    /// Returns the length of the trap range.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the port packet key for this trap range.
    #[inline]
    pub fn key(&self) -> u64 {
        self.key
    }
}

type TrapTree = WavlTree<zx_gpaddr_t, UniquePtr<Trap>>;

/// Lock class tag for [`TrapMap`]'s spinlock.
#[derive(Debug, Default, Copy, Clone)]
struct TrapMapLockClass;

ksync::declare_interned_string!(TRAP_MAP_LOCK_STRING_REG, "TrapMap::lock");

#[unsafe(link_section = "rust_lock_classes")]
#[used]
static TRAP_MAP_LOCK_REGISTRATION: ksync::LockClassRegistration =
    ksync::LockClassRegistration::with_flags(
        TRAP_MAP_LOCK_STRING_REG,
        <TrapMapLockClass as LockClass>::FLAGS,
    );

impl LockClass for TrapMapLockClass {
    const ID: *mut c_void = TRAP_MAP_LOCK_REGISTRATION.get();
    const FLAGS: ksync::LockFlags = TrackedSpinlock::LOCK_FLAGS;
}

/// Contains all the traps within a guest.
#[pin_data]
#[repr(C)]
pub struct TrapMap {
    #[pin]
    lock: KSpinlock<TrapMapLockClass>,
    #[pin]
    mem_traps: KCell<TrapTree, TrapMapLockClass>,
    #[cfg(target_arch = "x86_64")]
    #[pin]
    io_traps: KCell<TrapTree, TrapMapLockClass>,
}

static_assert!(offset_of!(TrapMap, lock) == 0);
static_assert!(offset_of!(TrapMap, mem_traps) == size_of::<TrackedSpinlock>().next_multiple_of(8));
#[cfg(target_arch = "x86_64")]
static_assert!(
    offset_of!(TrapMap, io_traps) == size_of::<TrackedSpinlock>().next_multiple_of(8) + 24
);
#[cfg(target_arch = "x86_64")]
static_assert!(size_of::<TrapMap>() == size_of::<TrackedSpinlock>().next_multiple_of(8) + 48);
#[cfg(not(target_arch = "x86_64"))]
static_assert!(size_of::<TrapMap>() == size_of::<TrackedSpinlock>().next_multiple_of(8) + 24);
static_assert!(align_of::<TrapMap>() == 8);

impl TrapMap {
    /// Creates an in-place pinned initializer for `TrapMap`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KSpinlock::init(),
            mem_traps <- KCell::pin_init(TrapTree::new()),
            #[cfg(target_arch = "x86_64")]
            io_traps <- KCell::pin_init(TrapTree::new()),
        })
    }

    /// Inserts a new trap range into the map.
    pub fn insert_trap(
        &self,
        kind: u32,
        addr: zx_gpaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> Result<(), Status> {
        if !valid_range(kind, addr, len) {
            return Err(Status::OUT_OF_RANGE);
        }
        let traps = self.tree_of(kind).ok_or(Status::INVALID_ARGS)?;

        let uninit = kalloc::Box::<Trap>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;
        let ptr = kalloc::Box::into_raw(uninit).cast::<Trap>();
        // SAFETY: `ptr` is the non-null, aligned, exclusively writable root heap allocation
        // pointer from `kalloc::Box::into_raw`, which remains pinned until dropped in place via
        // `Trap::recycle` (avoiding `kalloc::Box::try_pin_init`'s intermediate `DerefMut` reborrow
        // so `self_ptr` and `UniquePtr` share identical root allocation provenance).
        let Ok(()) = unsafe { Trap::new(kind, addr, len, port, key).__pinned_init(ptr) };
        // SAFETY: `ptr` was just fully initialized above, is uniquely owned, and was allocated via
        // `kalloc::Box` matching `Trap::recycle` (`UniquePtr::deref_mut` is never called on
        // `range` or by `WavlTree`).
        let range = unsafe { UniquePtr::from_raw(ptr) };

        ksync::lock!(let mut guard = self.lock.lock());
        // SAFETY: `guard` holds `self.lock`, which guards `traps`, and we hold exclusive mutable
        // access to its lock token. `traps` is a `#[pin]` field of `TrapMap`, which is only ever
        // constructed in place via `TrapMap::new()` (`rust_trap_map_new`) and never moved, so the
        // tree stays pinned.
        let mut traps = unsafe { traps.get_pinned_mut_locked(guard.as_mut().token_mut()) };
        let mut iter = traps.upper_bound(&addr);
        // If `upper_bound()` does not return `end()`, check if the range intersects.
        if let Some(trap) = iter.get()
            && intersects_offset_len(addr as u64, len as u64, trap.addr() as u64, trap.len() as u64)
        {
            return Err(Status::ALREADY_EXISTS);
        }
        // Decrement the iterator, and check if the next range intersects.
        iter.move_prev();
        if let Some(trap) = iter.get()
            && intersects_offset_len(addr as u64, len as u64, trap.addr() as u64, trap.len() as u64)
        {
            return Err(Status::ALREADY_EXISTS);
        }
        traps.as_mut().insert(range);
        Ok(())
    }

    /// Finds the trap containing `addr` for the given `kind`.
    pub fn find_trap(&self, kind: u32, addr: zx_gpaddr_t) -> Result<&Trap, Status> {
        let Some(traps) = self.tree_of(kind) else {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        };

        let found_ptr = {
            ksync::lock!(let guard = self.lock.lock());
            // SAFETY: `guard` holds `self.lock`, which guards `traps`.
            let traps = unsafe { traps.get(guard.token()) };
            let mut iter = traps.upper_bound(&addr);
            iter.move_prev();
            let Some(trap) = iter.get() else {
                core::hint::cold_path();
                return Err(Status::NOT_FOUND);
            };
            trap.self_ptr
        };
        // SAFETY: By `Trap::self_ptr`'s safety invariant, `found_ptr` is the pinned root heap
        // allocation pointer created in `Trap::new` (not derived from the lock-guarded `&TrapTree`
        // reference, which has already been dropped). Traps are never removed from
        // `TrapMap` until `TrapMap` itself is dropped, and `WavlTree` only mutates the intrusive
        // `wavl_node` (which has interior mutability via `UnsafeCell`), so forming `&Trap` with
        // the lifetime of `&self` is sound.
        let found = unsafe { found_ptr.as_ref() };
        if !found.contains(addr) {
            core::hint::cold_path();
            return Err(Status::NOT_FOUND);
        }
        Ok(found)
    }

    fn tree_of(&self, kind: u32) -> Option<&KCell<TrapTree, TrapMapLockClass>> {
        match kind {
            ZX_GUEST_TRAP_BELL | ZX_GUEST_TRAP_MEM => Some(&self.mem_traps),
            #[cfg(target_arch = "x86_64")]
            ZX_GUEST_TRAP_IO => Some(&self.io_traps),
            _ => None,
        }
    }
}

/// Unit tests for `TrapMap`.
#[cfg(ktest)]
#[unittest::suite(name = "trap_map_rust")]
mod tests {
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_err, expect_ok, expect_true, unwrap_ok, unwrap_some};
    use zx_types::zx_packet_type_t;

    /// Verifies that freeing all `MAX_PACKETS_PER_RANGE` packets retains the cache's slabs.
    #[test]
    fn blocking_port_allocator_retains_slabs() {
        stack_pin_init!(let allocator = BlockingPortAllocator::new());
        let mut packets =
            unwrap_ok!(kalloc::Box::try_new([None::<NonNull<PortPacket>>; MAX_PACKETS_PER_RANGE]));
        for slot in packets.iter_mut() {
            let packet = unwrap_some!(allocator.alloc_blocking());
            // SAFETY: `packet` was just allocated and is exclusively owned by this test.
            let packet_ref = unsafe { packet.as_ref() };
            expect_true!(packet_ref.is_ephemeral());
            expect_eq!(packet_ref.handle(), allocator.as_handle_ptr().as_ptr().cast_const());
            *slot = Some(packet);
        }
        let slab_count = allocator.cache.slab_count();
        expect_eq!(PACKET_CACHE_RESERVE_SLABS, slab_count);

        for packet in packets.iter_mut().filter_map(Option::take) {
            // SAFETY: `packet` was allocated by `allocator` above, is unlinked and exclusively
            // owned by this test, and is freed exactly once.
            unsafe {
                allocator.free(packet);
            }
        }
        // All slabs are retained, so reallocating up to the high-water mark cannot fail with
        // `Status::NO_MEMORY`.
        expect_eq!(slab_count, allocator.cache.slab_count());
    }

    /// Verifies that dropping a `Trap` cancels and frees the packets it queued on its port.
    #[test]
    fn trap_drop_cancels_queued_packets() {
        const KEY: u64 = 0x7a;
        let (handle, _) = unwrap_ok!(PortDispatcher::create(0));
        let port = handle.dispatcher();
        {
            stack_pin_init!(let trap_map = TrapMap::new());
            expect_ok!(trap_map.insert_trap(
                ZX_GUEST_TRAP_BELL,
                0x1000,
                0x1000,
                Some(port.clone()),
                KEY
            ));
            let trap = unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_BELL, 0x1800));
            let packet = zx_port_packet_t {
                key: KEY,
                packet_type: zx_packet_type_t::ZX_PKT_TYPE_GUEST_BELL,
                status: 0,
                union: [0; 32],
            };
            for _ in 0..3 {
                expect_ok!(trap.queue(&packet, None));
            }
        }
        // Dropping `trap_map` recycled the `Trap`, which canceled its queued packets and freed
        // them back to its cache before the cache itself was dropped.
        expect_err!(port.cancel_key(KEY), Status::NOT_FOUND);
    }

    /// Verifies that `TrapMap::insert_trap` rejects intersecting ranges.
    #[test]
    fn trap_map_insert_trap_intersecting() {
        stack_pin_init!(let trap_map = TrapMap::new());
        // Add traps:
        // 1. [10, 19]
        // 2. [20, 29]
        // 3. [35, 5]
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 10, 10, None, 0));
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 20, 10, None, 0));
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 35, 5, None, 0));
        // Trap at [0, 10] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 11, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [10, 19] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 10, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [11, 18] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 11, 8, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [15, 24] intersects with trap 1 and trap 2.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 15, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [30, 39] intersects with trap 3.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 30, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [36, 40] intersects with trap 3.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 36, 5, None, 0),
            Status::ALREADY_EXISTS
        );

        // Add a trap at the beginning.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 10, None, 0));
        // In the gap.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 30, 5, None, 0));
        // And at the end.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 40, 10, None, 0));
    }

    /// Verifies that `TrapMap::insert_trap` rejects out-of-range trap intervals.
    #[test]
    fn trap_map_insert_trap_out_of_range() {
        stack_pin_init!(let trap_map = TrapMap::new());
        expect_err!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 0, None, 0), Status::OUT_OF_RANGE);
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, u32::MAX as usize, usize::MAX, None, 0),
            Status::OUT_OF_RANGE
        );
        #[cfg(target_arch = "x86_64")]
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_IO, 0, u32::MAX as usize, None, 0),
            Status::OUT_OF_RANGE
        );
    }

    /// Verifies `TrapMap::find_trap` range hits and misses across trap kinds.
    #[test]
    fn trap_map_find_trap() {
        stack_pin_init!(let trap_map = TrapMap::new());
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0x1000, 0x1000, None, 11));
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_BELL, 0x3000, 0x1000, None, 22));

        // Misses below first trap, in gap, past end, and invalid kind.
        expect_err!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x0fff), Status::NOT_FOUND);
        expect_err!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x2000), Status::NOT_FOUND);
        expect_err!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x4000), Status::NOT_FOUND);
        expect_err!(trap_map.find_trap(u32::MAX, 0x1000), Status::INVALID_ARGS);

        // Hits at start, interior, and last byte (MEM and BELL share `mem_traps`).
        let trap1 = unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x1000));
        expect_eq!(trap1.kind(), ZX_GUEST_TRAP_MEM);
        expect_eq!(trap1.key(), 11);
        expect_eq!(unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x1800)).key(), 11);
        expect_eq!(unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x1fff)).key(), 11);

        let trap2 = unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_BELL, 0x3000));
        expect_eq!(trap2.kind(), ZX_GUEST_TRAP_BELL);
        expect_eq!(trap2.key(), 22);
        expect_eq!(unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_MEM, 0x3fff)).key(), 22);

        #[cfg(target_arch = "x86_64")]
        {
            expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_IO, 0x3f8, 8, None, 33));
            expect_err!(trap_map.find_trap(ZX_GUEST_TRAP_IO, 0x3f7), Status::NOT_FOUND);
            expect_err!(trap_map.find_trap(ZX_GUEST_TRAP_IO, 0x400), Status::NOT_FOUND);
            let io_trap = unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_IO, 0x3f8));
            expect_eq!(io_trap.kind(), ZX_GUEST_TRAP_IO);
            expect_eq!(io_trap.key(), 33);
            expect_eq!(unwrap_ok!(trap_map.find_trap(ZX_GUEST_TRAP_IO, 0x3ff)).key(), 33);
        }
    }
}
