// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor guest trap map and blocking port packet allocator.

use crate::ksync_kernel::{KSpinlock, TrackedSpinlock};
use core::convert::Infallible;
use core::ffi::{CStr, c_void};
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::ptr::NonNull;
use fbl::{
    Recyclable, RefPtr, UninitRecyclable, UniquePtr, WavlTree, WavlTreeContainable,
    WavlTreeKeyable, WavlTreeNode,
};
use ksync::{KCell, LockClass};
use object_constants_rs as object_constants;
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

use super::ktrace::{VcpuBlockOp, VcpuMeta, ktrace_vcpu};
use super::state_invalidator::StateInvalidator;
use super::trap_map_ffi::{
    cpp_port_packet_arena_alloc, cpp_port_packet_arena_destroy, cpp_port_packet_arena_free,
    cpp_port_packet_arena_init, cpp_port_packet_arena_init_pool,
};

const MAX_PACKETS_PER_RANGE: usize = 256;

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

/// Facade for C++ `hypervisor::PortPacketArena`.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub(super) struct PortPacketArena {
    _storage: zr::OpaqueBytes<{ object_constants::kPortPacketArenaSize }>,
    #[pin]
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,
}

zr::static_assert_size_and_align!(
    PortPacketArena,
    object_constants::kPortPacketArenaSize,
    object_constants::kPortPacketArenaAlign,
);

zr::unsafe_pinned_drop_ffi!(PortPacketArena, cpp_port_packet_arena_destroy);

impl PortPacketArena {
    fn new() -> impl PinInit<Self, Infallible> {
        zr::pin_init_ffi!(cpp_port_packet_arena_init)
    }

    fn init(&self, name: &'static CStr, max_count: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid initialized `PortPacketArena` and `name` is a valid C string.
        let status = unsafe {
            cpp_port_packet_arena_init_pool(
                core::ptr::from_ref(self).cast_mut(),
                name.as_ptr(),
                size_of::<PortPacket>(),
                max_count,
            )
        };
        Status::ok(status)
    }

    /// Allocates and constructs a `PortPacket` in this arena with `handle` and `allocator`.
    ///
    /// # Safety
    ///
    /// `handle` and `allocator` must remain valid for the lifetime of the allocated `PortPacket`
    /// while it is queued on any `PortDispatcher`, and `allocator` must point to the
    /// `PortAllocator` subobject at offset 0 of a live `BlockingPortAllocator` with provenance over
    /// the full `BlockingPortAllocator`.
    unsafe fn new_packet(
        &self,
        handle: NonNull<c_void>,
        allocator: NonNull<PortAllocator>,
    ) -> Option<NonNull<PortPacket>> {
        // SAFETY: `self` is a valid initialized `PortPacketArena` (synchronized internally by its
        // embedded `Mutex`).
        let raw = unsafe {
            cpp_port_packet_arena_alloc(core::ptr::from_ref(self).cast_mut()).cast::<PortPacket>()
        };
        let pkt = NonNull::new(raw)?;
        // SAFETY: `pkt` points to a freshly allocated, uninitialized `PortPacket` slot from the
        // arena, and the caller guarantees `handle` and `allocator` are valid.
        unsafe {
            pkt.write(PortPacket::new(handle.as_ptr(), allocator.as_ptr()));
        }
        Some(pkt)
    }

    /// Destroys and frees `port_packet` back to this arena.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been allocated from this `PortPacketArena` and not yet freed.
    unsafe fn delete(&self, port_packet: NonNull<PortPacket>) {
        // SAFETY: Guaranteed by caller; `PortPacketArena::Free` is synchronized internally by
        // its embedded `Mutex`.
        unsafe {
            core::ptr::drop_in_place(port_packet.as_ptr());
            cpp_port_packet_arena_free(
                core::ptr::from_ref(self).cast_mut(),
                port_packet.as_ptr().cast(),
            );
        }
    }
}

/// Blocks on allocation if the arena is empty.
#[pin_data]
#[repr(C)]
pub struct BlockingPortAllocator {
    allocator: PortAllocator,
    #[pin]
    semaphore: Semaphore,
    #[pin]
    arena: PortPacketArena,
}

static_assert!(offset_of!(BlockingPortAllocator, allocator) == 0);

impl BlockingPortAllocator {
    /// Creates an in-place pinned initializer for `BlockingPortAllocator`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            allocator: PortAllocator::new(Self::free_packet),
            semaphore <- Semaphore::new(MAX_PACKETS_PER_RANGE as i64),
            arena <- PortPacketArena::new(),
        })
    }

    /// Callback invoked by [`PortPacket::free`] when a packet allocated by this
    /// `BlockingPortAllocator` is released.
    ///
    /// # Safety
    ///
    /// `allocator` must point to the `allocator` field at offset 0 of a live
    /// `BlockingPortAllocator` with provenance over the entire `BlockingPortAllocator`, and
    /// `port_packet` must have been allocated by that `BlockingPortAllocator` and not yet freed.
    unsafe extern "C" fn free_packet(
        allocator: *const PortAllocator,
        port_packet: NonNull<PortPacket>,
    ) {
        // SAFETY: Guaranteed by caller.
        unsafe {
            (*allocator.cast::<Self>()).free(port_packet);
        }
    }

    /// Initializes the underlying packet arena.
    pub fn init(&self) -> Result<(), Status> {
        self.arena.init(c"hypervisor-packets", MAX_PACKETS_PER_RANGE)
    }

    /// Waits on the semaphore and allocates a `PortPacket` from the arena.
    pub fn alloc_blocking(&self) -> Option<NonNull<PortPacket>> {
        ktrace_vcpu(VcpuBlockOp::Block, VcpuMeta::Port);
        let result = self.semaphore.wait(&Deadline::infinite());
        ktrace_vcpu(VcpuBlockOp::Unblock, VcpuMeta::Port);
        if result.is_err() {
            return None;
        }
        self.alloc()
    }

    pub(super) fn alloc(&self) -> Option<NonNull<PortPacket>> {
        let this = NonNull::from(self);
        // SAFETY: `this` is a valid `BlockingPortAllocator` that cancels all queued packets in
        // `Trap::drop` before being destroyed. Deriving the `PortAllocator` pointer from
        // `NonNull::from(self)` (with `allocator` at offset 0) preserves provenance over the
        // entire `BlockingPortAllocator` when `BlockingPortAllocator::free_packet` casts
        // `allocator` back to `BlockingPortAllocator*`.
        unsafe { self.arena.new_packet(this.cast(), this.cast()) }
    }

    /// Frees `port_packet` back to the arena and posts to the semaphore.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been allocated by this `BlockingPortAllocator` and not yet freed.
    pub unsafe fn free(&self, port_packet: NonNull<PortPacket>) {
        // SAFETY: Guaranteed by caller.
        unsafe {
            self.arena.delete(port_packet);
        }
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
    #[wavl_node]
    wavl_node: WavlTreeNode<Trap>,
    kind: u32,
    addr: zx_gpaddr_t,
    len: usize,
    port: Option<RefPtr<PortDispatcher>>,
    key: u64, // Key for packets in this port range.
    #[pin]
    port_allocator: BlockingPortAllocator,
}

// SAFETY: `Trap` is synchronized internally (`PortDispatcher` is thread-safe and
// `BlockingPortAllocator` synchronizes via `Semaphore` and `PortPacketArena`'s `Mutex`).
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

    fn allocate(value: Self) -> Result<NonNull<Self>, kalloc::AllocError> {
        let boxed = kalloc::Box::try_new(value)?;
        let raw = kalloc::Box::into_raw(boxed);
        // SAFETY: `Box::into_raw` returns a valid, non-null pointer.
        unsafe { Ok(NonNull::new_unchecked(raw)) }
    }
}

unsafe impl UninitRecyclable for Trap {
    unsafe fn recycle_uninit(ptr: NonNull<MaybeUninit<Self>>) {
        // SAFETY: `ptr` was allocated by `allocate_uninit` (`kalloc::Box`).
        unsafe {
            let _ = kalloc::Box::from_non_null(ptr);
        }
    }

    fn allocate_uninit() -> Result<NonNull<MaybeUninit<Self>>, kalloc::AllocError> {
        let boxed = kalloc::Box::try_new_uninit()?;
        let raw = kalloc::Box::into_raw(boxed);
        // SAFETY: `Box::into_raw` returns a valid, non-null pointer.
        unsafe { Ok(NonNull::new_unchecked(raw)) }
    }
}

#[allow(clippy::len_without_is_empty)]
impl Trap {
    /// Creates an in-place pinned initializer for `Trap`.
    pub fn new(
        kind: u32,
        addr: zx_gpaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            wavl_node: WavlTreeNode::new(),
            kind,
            addr,
            len,
            port,
            key,
            port_allocator <- BlockingPortAllocator::new(),
        })
    }

    /// Initializes the trap's port allocator.
    pub fn init(&self) -> Result<(), Status> {
        self.port_allocator.init()
    }

    /// Queues `packet` on the trap's port, optionally invalidating `invalidator` first.
    pub fn queue(
        &self,
        packet: &zx_port_packet_t,
        invalidator: Option<&mut StateInvalidator>,
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
static_assert!(offset_of!(TrapMap, mem_traps) == size_of::<TrackedSpinlock>());
#[cfg(target_arch = "x86_64")]
static_assert!(offset_of!(TrapMap, io_traps) == size_of::<TrackedSpinlock>() + 24);
#[cfg(target_arch = "x86_64")]
static_assert!(size_of::<TrapMap>() == size_of::<TrackedSpinlock>() + 48);
#[cfg(not(target_arch = "x86_64"))]
static_assert!(size_of::<TrapMap>() == size_of::<TrackedSpinlock>() + 24);
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
        // SAFETY: `ptr` is a valid, uninitialized, properly aligned heap allocation for `Trap`
        // that remains pinned until `UniquePtr` drops it in place via `Recyclable::recycle`, and
        // `UniquePtr::from_raw` adopts ownership of the initialized allocation.
        let range = unsafe {
            let _ = Trap::new(kind, addr, len, port, key).__pinned_init(ptr);
            UniquePtr::from_raw(ptr)
        };
        range.init()?;

        ksync::lock!(let mut guard = self.lock.lock());
        // SAFETY: `guard` holds `self.lock`, which guards `traps`, and we hold exclusive mutable
        // access to its lock token.
        let traps = unsafe { traps.get_mut(guard.as_mut().token_mut()) };
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
        traps.insert(range);
        Ok(())
    }

    /// Finds the trap containing `addr` for the given `kind`.
    pub fn find_trap(&self, kind: u32, addr: zx_gpaddr_t) -> Result<&Trap, Status> {
        let traps = self.tree_of(kind).ok_or(Status::INVALID_ARGS)?;

        let found = {
            ksync::lock!(let mut guard = self.lock.lock());
            // SAFETY: `guard` holds `self.lock`, which guards `traps`, and we hold exclusive
            // mutable access to its lock token.
            let traps = unsafe { traps.get_mut(guard.as_mut().token_mut()) };
            let mut iter = traps.upper_bound(&addr);
            iter.move_prev();
            let Some(trap) = iter.get() else {
                core::hint::cold_path();
                return Err(Status::NOT_FOUND);
            };
            // SAFETY: Traps are never removed from `TrapMap` until `TrapMap` itself is dropped,
            // and `Trap` is never mutated through `&mut Trap` after insertion into the tree, so
            // extending the shared reference lifetime to `&self` is sound.
            unsafe { &*core::ptr::from_ref(trap) }
        };
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
    use unittest::{expect_err, expect_ok};

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
}
