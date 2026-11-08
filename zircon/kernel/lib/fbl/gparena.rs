// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::ksync_kernel::TrackedCriticalMutex;
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::pmm;
use crate::vm::vm_address_region::{VmAddressRegion, flag};
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use fbl::RefPtr;
use kprint::kprintln;
use ksync::{KMutex, LockClass};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;

/// Lock class for [`GPArena`]'s internal mapping mutex.
#[derive(Debug, Default, Copy, Clone)]
struct GPArenaLockClass;

ksync::declare_interned_string!(GPARENA_LOCK_STRING_REG, "GPArena::mapping_lock");

// SAFETY: The linker script defines the `rust_lock_classes` section for
// `LockClassRegistration` entries.
#[unsafe(link_section = "rust_lock_classes")]
#[used]
static GPARENA_LOCK_REGISTRATION: ksync::LockClassRegistration =
    ksync::LockClassRegistration::with_flags(
        GPARENA_LOCK_STRING_REG,
        <GPArenaLockClass as LockClass>::FLAGS,
    );

impl LockClass for GPArenaLockClass {
    const ID: *mut c_void = GPARENA_LOCK_REGISTRATION.get();
    const FLAGS: ksync::LockFlags = TrackedCriticalMutex::LOCK_FLAGS;
}

/// Reads a 32-bit value from `ptr` using a single architecture-level load instruction without
/// creating a Rust reference or assuming absence of concurrent non-atomic stores.
///
/// # Safety
///
/// `ptr` must be non-null, 4-byte aligned, and point to 4 bytes of permanently mapped, readable
/// kernel memory with valid or exposed provenance.
#[inline]
#[rustfmt::skip]
unsafe fn speculative_load_u32(ptr: *const AtomicU32) -> u32 {
    let out: u32;
    cfg_select! {
        target_arch = "x86_64" => {
            // SAFETY: Caller guarantees `ptr` points to 4 bytes of permanently mapped, 4-byte
            // aligned readable arena memory.
            unsafe {
                core::arch::asm!(
                    "mov {out:e}, [{ptr}]",
                    ptr = in(reg) ptr,
                    out = lateout(reg) out,
                    options(readonly, nostack, preserves_flags),
                );
            }
        }
        target_arch = "aarch64" => {
            // SAFETY: Caller guarantees `ptr` points to 4 bytes of permanently mapped, 4-byte
            // aligned readable arena memory.
            unsafe {
                core::arch::asm!(
                    "ldr {out:w}, [{ptr}]",
                    ptr = in(reg) ptr,
                    out = lateout(reg) out,
                    options(readonly, nostack, preserves_flags),
                );
            }
        }
        target_arch = "riscv64" => {
            // SAFETY: Caller guarantees `ptr` points to 4 bytes of permanently mapped, 4-byte
            // aligned readable arena memory.
            unsafe {
                core::arch::asm!(
                    "lw {out}, 0({ptr})",
                    ptr = in(reg) ptr,
                    out = lateout(reg) out,
                    options(readonly, nostack, preserves_flags),
                );
            }
        }
        _ => {
            compile_error!("Provide speculative_load_u32 implementation for your ARCH here");
            out = 0;
        }
    }
    out
}

/// Free-list node layout overlaid on unallocated [`GPArena`] slots.
#[repr(C)]
struct FreeNode<const PERSIST_SIZE: usize> {
    _data: [u8; PERSIST_SIZE],
    /// While this node is in the free list, `next` holds the index of the next node in the list
    /// (see [`LockFreeList`]). This struct is explicitly not packed to allow for the `next` field
    /// to be naturally aligned, as `LockFreeList` accesses it atomically. As a result we *may*
    /// preserve more than `PERSIST_SIZE`, but that is fine.
    next: AtomicU32,
}

/// Node stored in a [`LockFreeList`] containing a naturally aligned atomic next-slot index.
///
/// # Safety
///
/// Implementors must ensure `next_ptr` returns a valid pointer to a naturally aligned `AtomicU32`
/// field within `Self` without materializing references to `Self` or any of its fields (including
/// `next`).
unsafe trait LockFreeListNode: Sized {
    /// Returns a raw pointer to `node`'s `next` field without materializing a struct reference.
    ///
    /// # Safety
    ///
    /// `node` must point to mapped memory in the arena whose `next` field is naturally aligned.
    unsafe fn next_ptr(node: NonNull<Self>) -> *const AtomicU32;
}

// SAFETY: `FreeNode<PERSIST_SIZE>` is `#[repr(C)]` with a naturally aligned `AtomicU32` `next`
// field accessible via raw field projection.
unsafe impl<const PERSIST_SIZE: usize> LockFreeListNode for FreeNode<PERSIST_SIZE> {
    /// Returns a raw pointer to `node`'s `next` field without materializing a struct reference.
    ///
    /// # Safety
    ///
    /// `node` must point to mapped memory in the arena whose `next` field is naturally aligned.
    unsafe fn next_ptr(node: NonNull<Self>) -> *const AtomicU32 {
        // SAFETY: Projecting directly to `next` avoids materializing a reference to the whole
        // `FreeNode` (whose `_data` bytes may be uninitialized or concurrently modified).
        unsafe { ptr::addr_of!((*node.as_ptr()).next) }
    }
}

/// A simple lock-free list for use by [`GPArena`].
///
/// Rather than by address, the list refers to nodes by their 1-based index in an array of
/// `SLOT_SIZE`-byte slots that starts at the address passed to `init`, with index 0 meaning "no
/// node". This makes the head of the list, which pairs the index of the first node with a
/// generation count, small enough to fit in 64 bits, so that it can be updated with 8-byte atomic
/// operations. Unlike 16-byte atomic operations, these are lock-free on all supported
/// architectures.
///
/// Nodes have a naturally aligned `AtomicU32` `next` member, which holds the index of the next
/// node in the list while the node is in the list, and which the list accesses atomically.
struct LockFreeList<Node, const SLOT_SIZE: usize> {
    /// Stores the index of the current head node and a generation count. The generation count
    /// prevents races where one thread is modifying the list whilst another thread rapidly adds
    /// and removes. Every time the head is modified the generation count is incremented to
    /// generate a unique value.
    ///
    /// It is important that the count not wrap past an existing value that is still in use. That
    /// could only happen if a thread stalled in `pop`, between loading the head and the
    /// `compare_exchange`, while the head was modified an exact multiple of
    /// `2^GENERATION_BITS` times, leaving the same node at the head of the list. Every
    /// modification is a successful `compare_exchange` of `head`, and these are serialized, so
    /// even at a sustained rate of 10^8 modifications per second, which is faster than any real
    /// workload, 2^40 modifications take over 3 hours. No thread could plausibly be stalled for
    /// that long (by comparison, 2^32 modifications could take less than a minute).
    head: AtomicU64,

    /// The address of the slot with index 1.
    base: *mut u8,

    _marker: PhantomData<fn(Node) -> Node>,
}

zr::static_assert!(LockFreeList::<FreeNode<0>, 8>::INDEX_BITS <= 32);

impl<Node: LockFreeListNode, const SLOT_SIZE: usize> LockFreeList<Node, SLOT_SIZE> {
    /// The head of the list holds the index of the first node in its low `INDEX_BITS` bits, and
    /// the generation count in its high `GENERATION_BITS` bits. See `head` for why
    /// `GENERATION_BITS` is enough.
    const GENERATION_BITS: u32 = 40;
    const INDEX_BITS: u32 = 64 - Self::GENERATION_BITS;

    /// The maximum number of slots, and therefore nodes, that the list can refer to.
    const MAX_NODES: usize = ((1u64 << Self::INDEX_BITS) - 1) as usize;

    const INDEX_MASK: u64 = (1u64 << Self::INDEX_BITS) - 1;

    const fn new() -> Self {
        Self { head: AtomicU64::new(0), base: ptr::null_mut(), _marker: PhantomData }
    }

    /// Set the address of the first slot. Must be called before any node is pushed.
    #[inline]
    fn init(&mut self, base: *mut u8) {
        self.base = base;
    }

    /// Push `node` onto the list.
    ///
    /// # Safety
    ///
    /// `node` must point to a valid, uniquely owned `Node` slot within this list's backing arena
    /// starting at `self.base`.
    #[inline]
    unsafe fn push(&self, node: NonNull<Node>) {
        let index = self.to_index(node);
        // SAFETY: `push`'s `# Safety` contract guarantees `self.base` is initialized and `node`
        // lies within the arena starting at `self.base`, so `index` is a valid 1-based slot index
        // within arena bounds and the offset computation in `to_node` cannot wrap to zero.
        debug_assert!(
            node.as_ptr().addr() >= self.base.addr()
                && (index - 1) < Self::MAX_NODES as u64
                && unsafe { self.to_node(index) } == node
        );
        // Take a local copy/snapshot of the current head.
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // Every time the compare_exchange below fails head becomes the current value and so
            // we need to reset our intended next index every iteration.
            // SAFETY: `node` points to a valid arena slot whose `next` field is naturally aligned,
            // and `node` is uniquely owned by the caller until the `Release` `compare_exchange` on
            // `self.head` below succeeds, so no concurrent writes race with `&AtomicU32`.
            unsafe {
                self.next(node).store(Self::head_index(head) as u32, Ordering::Relaxed);
            }
            // Build our candidate next head.
            let next_head = Self::next_generation(head) | index;
            // Use release semantics so that any writes to the persist area, and our write to
            // `node.next`, are visible before the node can be seen in the free list and reused.
            match self.head.compare_exchange(head, next_head, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(current) => head = current,
            }
        }
    }

    /// Pop a node from the list. Returns `None` if empty.
    #[inline]
    fn pop(&self) -> Option<NonNull<Node>> {
        // Take a local copy/snapshot of the current head.
        // Use an acquire to match with the release in `push`.
        let mut head = self.head.load(Ordering::Acquire);
        while Self::head_index(head) != 0 {
            // SAFETY: `head_index(head) != 0` ensures `self.base` was initialized before a node
            // was pushed, and every non-zero index stored in `self.head` was pushed from a valid
            // 1-based slot within the mapped arena (whose VMAR mapping has exposed provenance), so
            // the offset computation cannot wrap to zero.
            let node = unsafe { self.to_node(Self::head_index(head)) };
            // This read is speculative: if another thread pops `node` before we do, it may then
            // overwrite `node.next`, so this may read garbage. If so, the generation count will
            // have changed, so the compare_exchange below will fail and the garbage will be
            // discarded. Mask the value regardless, so that garbage can never alter the generation
            // count in `next_head`, which must only ever advance by one.
            //
            // WARNING: If another thread popped `node`, this read formally races with its new
            // owner's plain stores, and no store or fence before handing out nodes can order the
            // two. This is benign: the arena's memory stays mapped for the life of the arena, so
            // the read can't fault, and as described above, whatever it returns is masked and then
            // discarded.
            //
            // SAFETY: `node` points to mapped, pinned arena memory that remains mapped for the
            // lifetime of the arena, and `node.next` is naturally aligned for `u32`. Reading via
            // `speculative_load_u32` avoids materializing a Rust reference when a racing thread
            // pops `node` and overwrites the slot with non-atomic stores.
            let next = unsafe { speculative_load_u32(Node::next_ptr(node)) } as u64;
            let next_head = Self::next_generation(head) | (next & Self::INDEX_MASK);
            // Acquire on both success and failure pairs with `push`'s Release store: on success it
            // ensures `push`'s write to `node.next` and any writes to the persist area
            // happen-before the slot is reused, and on failure (`head = current`) it synchronizes
            // with any concurrent `push` before the next iteration reads `current`'s `next` field.
            match self.head.compare_exchange(head, next_head, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => return Some(node),
                Err(current) => {
                    core::hint::cold_path();
                    head = current;
                }
            }
            // There is no pause here as we don't need to wait for anyone before trying again,
            // rather the sooner we retry the *more* likely we are to succeed given that we just
            // received the most up to date copy of head.
        }
        None
    }

    #[inline]
    const fn head_index(head: u64) -> u64 {
        head & Self::INDEX_MASK
    }

    /// Returns `head` with its index cleared and its generation count incremented. The generation
    /// count wraps around to 0 when it overflows.
    #[inline]
    const fn next_generation(head: u64) -> u64 {
        (head | Self::INDEX_MASK).wrapping_add(1)
    }

    #[inline]
    fn to_index(&self, node: NonNull<Node>) -> u64 {
        (node.as_ptr().addr().wrapping_sub(self.base.addr()) / SLOT_SIZE + 1) as u64
    }

    /// Converts a 1-based slot `index` back into a non-null node pointer.
    ///
    /// # Safety
    ///
    /// `self.base` must have been initialized to a non-null page-aligned virtual address and
    /// `index` must be a valid 1-based slot index within the arena so the offset computation
    /// cannot wrap to zero.
    #[inline]
    unsafe fn to_node(&self, index: u64) -> NonNull<Node> {
        let ptr = self.base.wrapping_add((index as usize - 1) * SLOT_SIZE).cast::<Node>();
        // SAFETY: Caller guarantees `self.base` is a non-null page-aligned virtual address and
        // `index >= 1` within arena bounds, so `ptr` is non-null.
        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// Returns a reference for atomically accessing `node`'s next index when pushing `node` onto
    /// the free list. `pop` instead uses `speculative_load_u32` because it reads the head node's
    /// next index without first taking ownership of the slot.
    ///
    /// # Safety
    ///
    /// `node` must point to mapped memory in the arena whose `next` field is naturally aligned,
    /// and no concurrent non-atomic writes may race with `node`'s `next` field for the duration
    /// of the returned reference.
    #[inline]
    unsafe fn next(&self, node: NonNull<Node>) -> &AtomicU32 {
        // SAFETY: Caller guarantees `node` points to mapped memory in the arena with a naturally
        // aligned `next` field and no racing non-atomic writes while `&AtomicU32` is live.
        unsafe { &*Node::next_ptr(node) }
    }
}

/// Growable Persistent Arena (`GPArena`) is an arena that allows for fast allocation and
/// deallocation of a single kind of object. Compared to other arena style allocators it
/// additionally guarantees that a portion of the object's memory will be preserved between calls
/// to [`GPArena::free`] and [`GPArena::alloc`].
///
/// Uses `#[repr(C, align(64))]` to enforce a two-cache-line layout: read-mostly bounds and VM
/// handles (`start`, `top`, `committed`, `end`, `vmar`, `mapping`, `vmo`) occupy cache line 0
/// (`+0x00..+0x3f`), isolating lock-free hot-path lookups from `free_list` and `count` mutations
/// in cache line 1 (`+0x40..+0x7f`).
#[pin_data(PinnedDrop)]
#[repr(C, align(64))]
pub struct GPArena<const PERSIST_SIZE: usize, const OBJECT_SIZE: usize> {
    start: *mut u8,
    /// `top` is the address of the next object to be allocated from the arena.
    top: AtomicUsize,
    /// `start..committed` represents the committed, pinned and mapped portion of the arena.
    committed: AtomicUsize,
    /// `start..end` represents the total virtual address reservation for the arena, and
    /// `committed` may not grow past `end`.
    end: usize,
    vmar: Option<RefPtr<VmAddressRegion>>,
    mapping: Option<RefPtr<VmMapping>>,
    vmo: Option<RefPtr<VmObject>>,
    _pad: [u8; 8],

    free_list: LockFreeList<FreeNode<PERSIST_SIZE>, OBJECT_SIZE>,
    /// Monotonic/diagnostic live object count; never gates memory accesses, so updated with
    /// `Ordering::Relaxed`.
    count: AtomicUsize,
    #[pin]
    mapping_lock: KMutex<GPArenaLockClass, TrackedCriticalMutex>,
    #[pin]
    _pin: PhantomData<core::marker::PhantomPinned>,
}

zr::static_assert!(core::mem::align_of::<GPArena<0, 8>>() == crate::arch_rs::MAX_CACHE_LINE);
zr::static_assert!(core::mem::offset_of!(GPArena<0, 8>, free_list) == 64);
zr::static_assert!(core::mem::size_of::<GPArena<0, 8>>() == 128);

// SAFETY: `VmObject` wraps `Opaque<bindings::VmObject>` (`UnsafeCell`) and raw pointers `start`
// and `free_list.base` do not auto-implement `Send`, but `VmObject` is a thread-safe
// reference-counted kernel object and the arena's backing mapping can safely move across threads.
unsafe impl<const PERSIST_SIZE: usize, const OBJECT_SIZE: usize> Send
    for GPArena<PERSIST_SIZE, OBJECT_SIZE>
{
}

// SAFETY: `VmObject` (`Opaque<bindings::VmObject>`) and raw pointers `start` and `free_list.base`
// do not auto-implement `Sync`. Concurrent shared (`&self`) operations on `GPArena` synchronize
// `free_list`, `top`, `committed`, and `count` via atomics and serialize all `vmo`/`mapping`/`vmar`
// accesses under `mapping_lock`; `start`, `end`, `vmar`, `mapping`, and `vmo` are only mutated with
// exclusive `Pin<&mut Self>` access in `init` and `drop`.
unsafe impl<const PERSIST_SIZE: usize, const OBJECT_SIZE: usize> Sync
    for GPArena<PERSIST_SIZE, OBJECT_SIZE>
{
}

#[pinned_drop]
impl<const PERSIST_SIZE: usize, const OBJECT_SIZE: usize> PinnedDrop
    for GPArena<PERSIST_SIZE, OBJECT_SIZE>
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        debug_assert!(*this.count.get_mut() == 0);
        if let Some(vmar) = this.vmar.take() {
            // Unmap all of our memory and free our resources.
            let committed_len = *this.committed.get_mut() - (*this.start).addr();
            if committed_len > 0
                && let Some(ref vmo) = *this.vmo
            {
                vmo.unpin(0, committed_len as u64);
            }
            let _ = vmar.destroy();
        }
        *this.vmo = None;
    }
}

impl<const PERSIST_SIZE: usize, const OBJECT_SIZE: usize> GPArena<PERSIST_SIZE, OBJECT_SIZE> {
    const _LAYOUT_CHECK: () = {
        assert!(
            core::mem::size_of::<FreeNode<PERSIST_SIZE>>() <= OBJECT_SIZE,
            "Not enough free space in object"
        );
        assert!(
            OBJECT_SIZE.is_multiple_of(core::mem::align_of::<FreeNode<PERSIST_SIZE>>()),
            "OBJECT_SIZE must be common alignment multiple"
        );
    };

    /// The maximum number of objects that an arena can hold.
    #[must_use]
    pub const fn max_count() -> usize {
        let () = Self::_LAYOUT_CHECK;
        LockFreeList::<FreeNode<PERSIST_SIZE>, OBJECT_SIZE>::MAX_NODES
    }

    /// Creates an uninitialized `GPArena`.
    pub fn new() -> impl PinInit<Self> {
        let () = Self::_LAYOUT_CHECK;
        pin_init!(Self {
            start: ptr::null_mut(),
            top: AtomicUsize::new(0),
            committed: AtomicUsize::new(0),
            end: 0,
            vmar: None,
            mapping: None,
            vmo: None,
            _pad: [0; 8],
            free_list: LockFreeList::new(),
            count: AtomicUsize::new(0),
            mapping_lock <- KMutex::init(),
            _pin: PhantomData,
        })
    }

    /// Creates and initializes a `GPArena` in-place.
    pub fn new_init(name: &[u8], max_count: usize) -> impl PinInit<Self, Status> + '_ {
        // SAFETY: Delegates directly to `Self::new().__pinned_init(slot)`.
        unsafe {
            pin_init::pin_init_from_closure(|slot| match Self::new().__pinned_init(slot) {
                Ok(()) => Ok(()),
            })
        }
        .pin_chain(move |this| this.init(name, max_count))
    }

    /// Reserves space for at least `max_count` objects, or more if they fit in the space left over
    /// from rounding up to a whole number of pages. Fails with `Status::OUT_OF_RANGE` if that
    /// would be more than [`GPArena::max_count`] objects, or if `max_count * OBJECT_SIZE`, page
    /// rounding, or the virtual address range end overflows `usize`.
    #[cold]
    #[inline(never)]
    pub fn init(self: Pin<&mut Self>, name: &[u8], max_count: usize) -> Result<(), Status> {
        let () = Self::_LAYOUT_CHECK;
        if max_count == 0 {
            return Err(Status::INVALID_ARGS);
        }

        let this = self.project();
        if this.vmar.is_some() {
            // Already initialized.
            return Err(Status::BAD_STATE);
        }

        // Check this before using max_count to compute mem_sz, and guard against integer overflow
        // for arbitrarily large `OBJECT_SIZE`.
        if max_count > Self::max_count() {
            return Err(Status::OUT_OF_RANGE);
        }
        let Some(raw_sz) = max_count.checked_mul(OBJECT_SIZE) else {
            return Err(Status::OUT_OF_RANGE);
        };
        if raw_sz > usize::MAX - (page::SIZE - 1) {
            return Err(Status::OUT_OF_RANGE);
        }

        // Carve out some memory from the kernel root VMAR.
        let mem_sz = page::round_up(raw_sz);

        // `alloc` hands out every object that fits in `mem_sz`, which may be more than
        // `max_count`, and the free list must be able to refer to all of them.
        if mem_sz / OBJECT_SIZE > Self::max_count() {
            return Err(Status::OUT_OF_RANGE);
        }

        let vmo_paged = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, mem_sz as u64)?;
        let vmo = VmObjectPaged::into_vm_object(vmo_paged);

        let kspace = VmAspace::kernel_aspace();
        let root_vmar = kspace.root_vmar().expect("kernel root_vmar must exist");

        let mut vname = [0u8; 32];
        let prefix = b"gparena:";
        vname[..prefix.len()].copy_from_slice(prefix);
        let name_end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let copy_len = name_end.min(vname.len() - 1 - prefix.len());
        vname[prefix.len()..prefix.len() + copy_len].copy_from_slice(&name[..copy_len]);
        let _ = vmo.set_name(&vname[..prefix.len() + copy_len]);

        let vmar = root_vmar
            .create_sub_vmar(
                0, // offset (ignored)
                mem_sz,
                0, // align_pow2
                flag::CAN_MAP_READ | flag::CAN_MAP_WRITE | flag::CAN_MAP_SPECIFIC,
                &vname[..prefix.len() + copy_len],
            )
            .map_err(|_| Status::NO_MEMORY)?;

        // The VMAR's parent holds a ref, so it won't be destroyed automatically when we return.
        let vmar_for_cleanup = vmar.clone();
        let mut destroy_vmar = zr::defer(move || {
            let _ = vmar_for_cleanup.destroy();
        });

        let map_result = vmar.create_vm_mapping(
            0, // mapping_offset
            mem_sz,
            0, // align_pow2
            flag::SPECIFIC | flag::DEBUG_DYNAMIC_KERNEL_MAPPING,
            vmo.clone(),
            0, // vmo_offset
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            b"gparena",
        )?;

        let Some(end) = map_result.base.checked_add(mem_sz) else {
            return Err(Status::OUT_OF_RANGE);
        };

        let base_ptr = ptr::with_exposed_provenance_mut::<u8>(map_result.base);
        *this.vmar = Some(vmar);
        *this.mapping = Some(map_result.mapping);
        *this.vmo = Some(vmo);
        *this.start = base_ptr;
        *this.top.get_mut() = map_result.base;
        *this.committed.get_mut() = map_result.base;
        *this.end = end;
        this.free_list.init(base_ptr);

        debug_assert!(page::is_page_rounded(map_result.base));
        debug_assert!(page::is_page_rounded(*this.end));

        destroy_vmar.cancel();

        Ok(())
    }

    /// Returns a non-null pointer to raw slot storage rather than a typed reference so that the
    /// preserved memory can be inspected prior to construction taking place.
    ///
    /// Fast-path allocation from the free list or already-committed memory is lock-free. Slow-path
    /// page growth acquires `mapping_lock` and commits VMO pages, so `alloc` must only be called
    /// in thread context where blocking is permitted.
    ///
    /// Because [`LockFreeList::pop`] speculatively reads the 4-byte `FreeNode<PERSIST_SIZE>::next`
    /// region via `speculative_load_u32` without owning the slot, a racing `alloc` call may read
    /// those 4 bytes after another thread has popped and begun reusing the slot. Multi-threaded
    /// callers should avoid holding an exclusive `&mut T` (without `UnsafeCell`) covering the
    /// `FreeNode<PERSIST_SIZE>::next` bytes while concurrent `alloc` calls may execute.
    #[inline]
    #[must_use]
    pub fn alloc(&self) -> Option<NonNull<u8>> {
        if let Some(node) = self.free_list.pop() {
            // `count` is a diagnostic counter that does not synchronize memory accesses.
            self.count.fetch_add(1, Ordering::Relaxed);
            return Some(node.cast::<u8>());
        }

        // Nothing in the free list, we need to grow.
        let mut top = self.top.load(Ordering::Relaxed);
        loop {
            // Every time the compare_exchange below fails top becomes the current value and so
            // we recalculate our potential next_top every iteration from it.
            let next_top = top.checked_add(OBJECT_SIZE)?;
            // Acquire load pairs with the Release store in `grow` so that newly mapped pages are
            // visible on weak-memory SMP architectures before the slot is initialized.
            if next_top > self.committed.load(Ordering::Acquire) && !self.grow(next_top) {
                return None;
            }
            // Release ordering ensures prior page commitment in `grow` is ordered before `top`
            // advances for readers calling `committed()`, `confine()`, and `lookup_confined()`;
            // `Relaxed` suffices on failure because the next iteration reloads `self.committed`
            // with `Acquire`.
            match self.top.compare_exchange(top, next_top, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => {
                    core::hint::cold_path();
                    top = current;
                }
            }
        }
        // `count` is a diagnostic counter that does not synchronize memory accesses.
        self.count.fetch_add(1, Ordering::Relaxed);
        let offset = top.wrapping_sub(self.start.addr());
        // SAFETY: `self.start` was initialized to a non-null page-aligned VMAR base and `top` lies
        // within `[self.start, self.end)`, so `self.start.wrapping_add(offset)` is non-null.
        Some(unsafe { NonNull::new_unchecked(self.start.wrapping_add(offset)) })
    }

    /// Returns an allocated slot to the free list; the caller must have already run any destructor
    /// for the object stored in `node`.
    ///
    /// # Safety
    ///
    /// `node` must be a currently allocated slot returned by [`GPArena::alloc`] from this arena
    /// that has not already been freed. Any value in the non-preserved portion of the slot must
    /// already have been dropped, and no live `&mut` references (nor any shared `&` references
    /// outside interior-mutable atomic fields in `[0..PERSIST_SIZE)`) may remain to any part of
    /// `[0..OBJECT_SIZE)` when calling `free`.
    #[inline]
    pub unsafe fn free(&self, node: NonNull<u8>) {
        // SAFETY: Caller guarantees `node` is a valid allocated slot from this arena with no
        // conflicting live references.
        unsafe {
            self.free_list.push(node.cast::<FreeNode<PERSIST_SIZE>>());
        }
        // `count` is a diagnostic counter that does not synchronize memory accesses.
        self.count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Returns the number of currently allocated objects in the arena.
    #[inline]
    #[must_use]
    pub fn diagnostic_count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Returns whether `node` lies within the arena's allocated range `[start, top)`.
    #[inline]
    #[must_use]
    pub fn committed(&self, node: *const u8) -> bool {
        let n = node.addr();
        // Acquire pairs with the Release `compare_exchange` on `self.top` in `alloc`.
        n >= self.start.addr() && n < self.top.load(Ordering::Acquire)
    }

    /// Return `address` if it is within the valid range of the arena or the base of the arena if
    /// not. Hardened against Spectre V1 / bounds check bypass speculation attacks - it always
    /// returns a safe value, even under speculation.
    #[inline]
    #[must_use]
    pub fn confine(&self, address: usize) -> usize {
        let start = self.start.addr();
        // Acquire pairs with the Release `compare_exchange` on `self.top` in `alloc`.
        let size = self.top.load(Ordering::Acquire) - start;
        let offset = address.wrapping_sub(start);
        let offset = fbl::confine_array_index(offset, size);
        start + offset
    }

    /// Looks up the slot at byte `offset` from the base of the arena if it lies within the
    /// committed range `[0, top - start)`, returning a Spectre-V1 confined pointer derived from
    /// the arena's base provenance.
    #[inline]
    #[must_use]
    pub fn lookup_confined(&self, offset: usize) -> Option<NonNull<u8>> {
        // Acquire pairs with the Release `compare_exchange` on `self.top` in `alloc`.
        let size = self.top.load(Ordering::Acquire) - self.start.addr();
        if offset >= size {
            return None;
        }
        let offset = fbl::confine_array_index(offset, size);
        // SAFETY: `offset < size` implies `size > 0`, so the arena is initialized with a non-null
        // `self.start` and `self.start.wrapping_add(offset)` lies within the mapped arena.
        Some(unsafe { NonNull::new_unchecked(self.start.wrapping_add(offset)) })
    }

    /// Returns the base address of the arena as a pointer.
    #[inline]
    #[must_use]
    pub fn base(&self) -> *mut u8 {
        self.start
    }

    /// Dumps diagnostic information about the arena and its VMAR mappings.
    pub fn dump(&self) {
        // Take the mapping lock so we can safely dump the vmar without mappings being done in
        // parallel.
        ksync::lock!(let _guard = self.mapping_lock.lock());

        let vmar = self.vmar.as_ref().expect("GPArena::dump called on uninitialized arena");
        kprintln!("GPArena<{PERSIST_SIZE:#x},{OBJECT_SIZE:#x}> {:s} mappings:", vmar.name());
        {
            ksync::lock!(let region_guard = vmar.region_lock().lock());
            ksync::lock!(let vmar_guard = vmar.lock().lock());
            vmar.dump_locked(region_guard.token(), vmar_guard.token(), 1, true);
        }

        // `top`, `committed`, and `count` are all atomic variables that can change out from under
        // this method so it's not possible to guarantee a consistent snapshot. Instead, we aim
        // for providing output that's not "obviously wrong".
        //
        // Load (with sequential consistency) all three values into local variables to minimize the
        // chance of getting inconsistent data.
        let start = self.start.addr();
        let top = self.top.load(Ordering::SeqCst);
        let committed = self.committed.load(Ordering::SeqCst);
        let count = self.count.load(Ordering::SeqCst);

        let nslots = (top - start) / OBJECT_SIZE;
        let np = (committed - start) / page::SIZE;
        let npmax = (self.end - start) / page::SIZE;
        let tslots = (self.end - start) / OBJECT_SIZE;

        // If we didn't get a consistent read, at least make sure we don't underflow.
        let free_list = nslots.saturating_sub(count);

        kprintln!(" start {start:#x}");
        kprintln!(" top {top:#x} ({nslots} slots allocated)");
        kprintln!(" committed {committed:#x} ({np}/{npmax} pages)");
        kprintln!(" end {:#x} ({tslots} slots total)", self.end);
        kprintln!(" free list length {free_list}");
    }

    /// Attempts to grow the committed memory range such that `next_top` is included in the range.
    #[cold]
    #[inline(never)]
    #[must_use]
    fn grow(&self, next_top: usize) -> bool {
        if next_top > self.end || next_top > usize::MAX - (page::SIZE - 1) {
            return false;
        }
        // Take the mapping lock.
        ksync::lock!(let _guard = self.mapping_lock.lock());
        // Cache `committed` as only we can change it while holding `mapping_lock`.
        let committed = self.committed.load(Ordering::Relaxed);
        // Now that we have the lock, double check we need to proceed. Avoid early `return` while
        // `_guard` is in scope so the `KMutexGuard` unlock epilogue is emitted only once.
        if next_top <= committed {
            true
        } else {
            let nc = committed
                .saturating_add(4 * page::SIZE)
                .max(page::round_up(next_top))
                .min(self.end);
            if nc == committed || nc < next_top {
                // If we aren't going to commit enough memory to include `next_top`, then we have
                // completely filled the arena.
                false
            } else {
                let offset = committed - self.start.addr();
                let len = nc - committed;
                let vmo = self.vmo.as_ref().expect("vmo must be initialized");
                let mapping = self.mapping.as_ref().expect("mapping must be initialized");
                if vmo.commit_range_pinned(offset as u64, len as u64, true).is_err() {
                    false
                } else if mapping.map_range(offset, len, true, false).is_err() {
                    // Unpin the range.
                    vmo.unpin(offset as u64, len as u64);
                    // Try to clean up any committed pages, but don't require that it succeeds.
                    let _ = mapping.decommit_range(offset, len);
                    false
                } else {
                    // Release pairs with the Acquire load of `self.committed` in `alloc`.
                    self.committed.store(nc, Ordering::Release);
                    true
                }
            }
        }
    }
}

/// Unit tests for [`GPArena`].
#[cfg(ktest)]
#[unittest::suite(name = "gparena_tests")]
mod tests {
    use super::GPArena;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
    use core::ffi::c_void;
    use core::ptr::{self, NonNull};
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use pin_init::{pin_data, pin_init, stack_pin_init, stack_try_pin_init};
    use unittest::{
        assert_ok, assert_true, expect_eq, expect_false, expect_true, subtest, unwrap_ok,
        unwrap_some,
    };
    use zx_status::Status;

    /// Verifies that small object sizes with and without preserved regions compile cleanly.
    #[test]
    fn can_declare_small_objectsize() {
        // This is just exercising the compile-time layout assertions in `GPArena` that ensure we
        // can have fairly small object sizes even in the presence of preservation.
        stack_pin_init!(let _smallest = GPArena::<0, 8>::new());
        stack_pin_init!(let _smallest_with_preserve = GPArena::<4, 16>::new());
    }

    /// Verifies last-in-first-out reuse ordering when freeing and reallocating objects.
    #[test]
    fn basic_lifo() {
        stack_pin_init!(let arena = GPArena::<0, 8>::new());
        assert_ok!(arena.as_mut().init(b"test", 4));

        let first = arena.alloc();
        assert_true!(first.is_some());
        let first = first.unwrap();
        let second = arena.alloc();
        assert_true!(second.is_some());
        let second = second.unwrap();

        // `alloc` should always return the last `free`.
        // SAFETY: `second` was just allocated from `arena`.
        unsafe { arena.free(second) };
        expect_true!(arena.alloc() == Some(second));

        // If we free multiple we should get them back in last-in-first-out order.
        // SAFETY: `second` and `first` are currently allocated from `arena`.
        unsafe {
            arena.free(second);
            arena.free(first);
        }
        expect_true!(arena.alloc() == Some(first));
        expect_true!(arena.alloc() == Some(second));

        // Cleanup.
        // SAFETY: `second` and `first` are currently allocated from `arena`.
        unsafe {
            arena.free(second);
            arena.free(first);
        }
    }

    /// Verifies allocation exhaustion and recovery after returning objects to the free list.
    #[test]
    fn out_of_memory() {
        // Use large objects so we can store all the allocations in a stack array.
        const COUNT: usize = page::SIZE / 512;
        stack_pin_init!(let arena = GPArena::<0, 512>::new());
        assert_ok!(arena.as_mut().init(b"test", COUNT));
        let mut allocs = [None; COUNT];

        // Allocate all objects from the arena.
        for slot in &mut allocs {
            let p = arena.alloc();
            assert_true!(p.is_some());
            *slot = p;
        }

        // Unless we calculated wrong, allocations should now fail.
        expect_true!(arena.alloc().is_none());
        expect_true!(arena.alloc().is_none());

        // Should be able to put objects back and then successfully reallocate them.
        let last = allocs[COUNT - 1].unwrap();
        let second_last = allocs[COUNT - 2].unwrap();
        // SAFETY: Both slots are currently allocated from `arena`.
        unsafe {
            arena.free(last);
            arena.free(second_last);
        }
        expect_true!(arena.alloc() == Some(second_last));
        expect_true!(arena.alloc() == Some(last));

        // Once we reallocate the ones we put back, future allocations should be back to failing.
        expect_true!(arena.alloc().is_none());
        expect_true!(arena.alloc().is_none());

        // Cleanup.
        for slot in allocs {
            // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }
    }

    /// Verifies that the preserved prefix bytes remain intact across `free` and `alloc`.
    #[test]
    fn does_preserve() {
        const PRESERVE: usize = 8;
        const MAGIC: &[u8; PRESERVE] = b"preserve";
        const COUNT: usize = 4;

        stack_pin_init!(let arena = GPArena::<PRESERVE, 16>::new());
        assert_ok!(arena.as_mut().init(b"test", COUNT));
        let mut allocs = [None; COUNT];

        // Allocate all our objects, and initialize them with the magic data.
        for slot in &mut allocs {
            let p = arena.alloc();
            assert_true!(p.is_some());
            let p = p.unwrap();
            // SAFETY: `p` points to a uniquely owned 16-byte slot in `arena`.
            unsafe { ptr::copy_nonoverlapping(MAGIC.as_ptr(), p.as_ptr(), PRESERVE) };
            *slot = Some(p);
        }

        // Return the objects back to the allocator.
        for slot in allocs {
            // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }

        // Whilst unallocated the preserve region should be unchanged.
        for slot in allocs {
            let p = slot.unwrap();
            // SAFETY: `p` points to mapped, pinned arena memory within `PRESERVE` bytes.
            let slice = unsafe { core::slice::from_raw_parts(p.as_ptr(), PRESERVE) };
            expect_true!(slice == MAGIC);
        }

        // Reallocate the object and validate that allocation didn't destroy the preserve region.
        for slot in &mut allocs {
            let p = arena.alloc();
            assert_true!(p.is_some());
            *slot = p;
        }
        for slot in allocs {
            let p = slot.unwrap();
            // SAFETY: `p` points to an allocated slot in `arena` whose first `PRESERVE` bytes hold
            // `MAGIC`.
            let slice = unsafe { core::slice::from_raw_parts(p.as_ptr(), PRESERVE) };
            expect_true!(slice == MAGIC);
        }

        // Cleanup.
        for slot in allocs {
            // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }
    }

    /// Small helper to do offset arithmetic of an arena `base`, returning a `*const u8`.
    fn base_offset<const P: usize, const O: usize>(
        arena: &GPArena<P, O>,
        offset: usize,
    ) -> *const u8 {
        arena.base().wrapping_add(offset).cast_const()
    }

    /// Probes that a worker thread is still running until `deadline`. If the worker unexpectedly
    /// exits early (`Ok(_)`), clears `thread` to `None` so cleanup will not double-join it.
    fn probe_still_running(thread: &mut Option<ThreadPtr>, deadline: InstantMono) -> bool {
        let Some(ref t) = *thread else {
            return false;
        };
        // SAFETY: `t` has not previously completed a `join`.
        let res = unsafe { t.join(deadline) };
        if res.is_ok() {
            *thread = None;
        }
        res == Err(Status::TIMED_OUT)
    }

    /// Joins a worker thread during test cleanup and returns its exit code, or `None` if it had
    /// already exited earlier or timed out. If the 5-second timed join fails, blocks indefinitely
    /// so stack-pinned test fixtures are never dropped while a worker thread is still running.
    fn join_cleanup(thread: &mut Option<ThreadPtr>) -> Option<i32> {
        let t = thread.take()?;
        // SAFETY: `t` was taken from `thread` and has not previously completed a `join`.
        match unsafe { t.join(current_mono_time() + DurationMono::from_seconds(5)) } {
            Ok(retcode) => Some(retcode),
            Err(_) => {
                // SAFETY: Timed join failed, so `t` has still not completed a `join`.
                let _ = unsafe { t.join(InstantMono::INFINITE) };
                None
            }
        }
    }

    /// Allocates a heap-backed slice of `len` `Option<NonNull<u8>>` slots initialized to `None`.
    fn alloc_ptr_slice(len: usize) -> Result<kalloc::Box<[Option<NonNull<u8>>]>, Status> {
        let slice = kalloc::Box::<[Option<NonNull<u8>>]>::try_new_zeroed_uninit_slice(len)
            .map_err(|_| Status::NO_MEMORY)?;
        // SAFETY: `Option<NonNull<u8>>` is guaranteed by the null-pointer layout optimization to
        // represent `None` as all-zero bytes.
        Ok(unsafe { slice.assume_init() })
    }

    /// Verifies that `committed` grows monotonically as objects are allocated and never shrinks.
    #[test]
    fn committed_monotonic() {
        stack_pin_init!(let arena = GPArena::<0, 8>::new());
        assert_ok!(arena.as_mut().init(b"test", 4));

        // Initially `alloc` has not been called, and so `committed` can never be true.
        expect_false!(arena.committed(base_offset(&arena, 0)));
        expect_false!(arena.committed(base_offset(&arena, 8)));
        expect_false!(arena.committed(base_offset(&arena, 16)));

        // Perform an allocation and check `committed` is true for that value, but no other.
        expect_true!(
            arena.alloc().map(|p| p.as_ptr().cast_const()) == Some(base_offset(&arena, 0))
        );
        expect_true!(arena.committed(base_offset(&arena, 0)));
        expect_false!(arena.committed(base_offset(&arena, 8)));
        expect_false!(arena.committed(base_offset(&arena, 16)));

        // Perform another allocation; `committed` should be true for it and previous allocation.
        expect_true!(
            arena.alloc().map(|p| p.as_ptr().cast_const()) == Some(base_offset(&arena, 8))
        );
        expect_true!(arena.committed(base_offset(&arena, 0)));
        expect_true!(arena.committed(base_offset(&arena, 8)));
        expect_false!(arena.committed(base_offset(&arena, 16)));

        // Returning the allocated objects should have no impact on what `committed` returns.
        // SAFETY: Both slots at offsets 8 and 0 are currently allocated from `arena`.
        unsafe {
            arena.free(NonNull::new(base_offset(&arena, 8).cast_mut()).unwrap());
        }
        expect_true!(arena.committed(base_offset(&arena, 0)));
        expect_true!(arena.committed(base_offset(&arena, 8)));
        expect_false!(arena.committed(base_offset(&arena, 16)));

        // SAFETY: Slot at offset 0 is currently allocated from `arena`.
        unsafe {
            arena.free(NonNull::new(base_offset(&arena, 0).cast_mut()).unwrap());
        }
        expect_true!(arena.committed(base_offset(&arena, 0)));
        expect_true!(arena.committed(base_offset(&arena, 8)));
        expect_false!(arena.committed(base_offset(&arena, 16)));
    }

    /// Allocates every object in an arena, then frees them all and allocates them all again. This
    /// checks that the free list can refer to every object in the arena, including for object
    /// sizes that are not a power of two.
    fn alloc_all<const P: usize, const O: usize>() -> bool {
        // Use enough objects that the arena has to grow several times, and that fill a whole
        // number of pages, so that the arena holds exactly this many objects.
        const {
            let count = 16 * page::SIZE / O;
            assert!(page::round_up(count * O) / O == count);
        }
        let count = 16 * page::SIZE / O;

        let case = subtest!(|| {
            stack_pin_init!(let arena = GPArena::<P, O>::new());
            assert_ok!(arena.as_mut().init(b"test", count));

            let mut allocs = unwrap_ok!(alloc_ptr_slice(count));

            // Objects are initially allocated in address order.
            for (i, slot) in allocs.iter_mut().enumerate() {
                let p = unwrap_some!(arena.alloc());
                expect_true!(p.as_ptr().cast_const() == base_offset(&arena, i * O));
                *slot = Some(p);
            }
            expect_true!(arena.alloc().is_none());

            // Free the objects in an order other than address order: first the objects with an
            // even index, then those with an odd index.
            let evens = count.div_ceil(2);
            let free_order = |n: usize| if n < evens { 2 * n } else { 2 * (n - evens) + 1 };
            for n in 0..count {
                let p = unwrap_some!(allocs[free_order(n)]);
                // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
                unsafe {
                    arena.free(p);
                }
            }

            // As the free list is LIFO, the objects should be reallocated in the reverse of that
            // order.
            for n in (0..count).rev() {
                let expected = allocs[free_order(n)];
                expect_true!(arena.alloc() == expected);
            }
            expect_true!(arena.alloc().is_none());

            // Cleanup.
            for slot in &*allocs {
                let p = unwrap_some!(*slot);
                // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
                unsafe {
                    arena.free(p);
                }
            }
        });
        case()
    }

    /// Verifies full allocation, non-sequential free, and LIFO reallocation for 8-byte slots.
    #[test]
    fn alloc_all_0_8() {
        assert_true!(alloc_all::<0, 8>());
    }

    /// Verifies full allocation, non-sequential free, and LIFO reallocation for 40-byte slots.
    #[test]
    fn alloc_all_24_40() {
        assert_true!(alloc_all::<24, 40>());
    }

    /// Verifies `max_count` boundary checks during `init`.
    #[test]
    fn init_max_count() {
        type Arena = GPArena<0, 8>;
        const MAX_COUNT: usize = Arena::max_count();
        const OBJECTS_PER_PAGE: usize = page::SIZE / 8;
        zr::static_assert!(!MAX_COUNT.is_multiple_of(OBJECTS_PER_PAGE));

        // More objects than the free list can refer to.
        {
            stack_pin_init!(let arena = Arena::new());
            expect_true!(arena.as_mut().init(b"test", MAX_COUNT + 1) == Err(Status::OUT_OF_RANGE));
        }

        // Not more objects than the free list can refer to, but rounding up to a whole number of
        // pages would make room for more objects than that.
        {
            stack_pin_init!(let arena = Arena::new());
            expect_true!(arena.as_mut().init(b"test", MAX_COUNT) == Err(Status::OUT_OF_RANGE));
        }

        // The largest number of objects that both fills a whole number of pages and does not
        // exceed `MAX_COUNT`. Nothing is committed until objects are allocated, so this only
        // reserves address space.
        {
            stack_pin_init!(let arena = Arena::new());
            assert_ok!(
                arena.as_mut().init(b"test", MAX_COUNT / OBJECTS_PER_PAGE * OBJECTS_PER_PAGE)
            );
            let object = arena.alloc();
            assert_true!(object.is_some());
            let object = object.unwrap();
            expect_eq!(arena.base(), object.as_ptr());
            // SAFETY: `object` was just allocated from `arena`.
            unsafe { arena.free(object) };
        }
    }

    /// A helper that continuously allocates and frees a single object.
    #[pin_data]
    struct ArenaAllocFixture<const P: usize, const O: usize> {
        #[pin]
        arena: GPArena<P, O>,
        stop_requested: AtomicBool,
    }

    impl<const P: usize, const O: usize> ArenaAllocFixture<P, O> {
        fn new() -> impl PinInit<Self> {
            pin_init!(Self {
                arena <- GPArena::new(),
                stop_requested: AtomicBool::new(false),
            })
        }

        /// A thread_start_routine for use with `thread::create_with_priority`.
        extern "C" fn run_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a pinned `ArenaAllocFixture` that outlives all worker
            // threads joined before `run_entry` returns.
            let fixture = unsafe { &*arg.cast::<Self>() };
            fixture.run()
        }

        fn run(&self) -> i32 {
            let mut allocations = 0u32;
            loop {
                let Some(v) = self.arena.alloc() else {
                    // On any failure just return. That we terminated at all is the error signal
                    // to our parent.
                    return -1;
                };
                // SAFETY: `v` was just allocated from `self.arena`.
                unsafe { self.arena.free(v) };
                // Check every so often if someone is trying to stop us. We don't check every
                // iteration to maximize the chance of actually doing `alloc` and `free`
                // concurrently.
                allocations = allocations.wrapping_add(1);
                if allocations.is_multiple_of(100) && self.stop_requested.load(Ordering::Relaxed) {
                    return 0;
                }
            }
        }
    }

    /// Verifies concurrent `alloc` and `free` across parallel worker threads.
    #[test]
    fn parallel_alloc() {
        type Fixture = ArenaAllocFixture<0, 8>;
        stack_pin_init!(let fixture = Fixture::new());
        assert_ok!(fixture.as_mut().project().arena.init(b"test", 4));
        let fixture_ref = &*fixture;
        let arg = ptr::from_ref(fixture_ref).cast_mut().cast::<c_void>();

        // Spin up two instances of the allocation fixture that will run in parallel.
        // SAFETY: `fixture` stays pinned on the stack until both threads are joined below.
        let (mut t1, mut t2) = unsafe {
            let t1 = thread::create_with_priority(
                c"gparena worker1".as_ptr(),
                Fixture::run_entry,
                arg,
                DEFAULT_PRIORITY,
            )
            .expect("worker1 thread creation should succeed");
            let t2 = thread::create_with_priority(
                c"gparena worker2".as_ptr(),
                Fixture::run_entry,
                arg,
                DEFAULT_PRIORITY,
            )
            .expect("worker2 thread creation should succeed");
            t1.resume();
            t2.resume();
            (Some(t1), Some(t2))
        };

        // Attempt to join one of the threads, letting it run for a bit. If the join succeeds this
        // means the fixture terminated, which indicates it encountered an error.
        expect_true!(probe_still_running(
            &mut t1,
            current_mono_time() + DurationMono::from_millis(500)
        ));
        // Check that the other thread is still running as well.
        expect_true!(probe_still_running(&mut t2, current_mono_time()));

        // Cleanup.
        fixture_ref.stop_requested.store(true, Ordering::Relaxed);
        let ret1 = join_cleanup(&mut t1);
        expect_true!(ret1.is_some());
        if let Some(code) = ret1 {
            expect_eq!(code, 0);
        }
        let ret2 = join_cleanup(&mut t2);
        expect_true!(ret2.is_some());
        if let Some(code) = ret2 {
            expect_eq!(code, 0);
        }
    }

    /// Holding more than one object at a time allows other threads to rearrange the free list
    /// while a thread is popping from it, which is the race that the free list's generation count
    /// prevents.
    const OBJECTS_PER_THREAD: usize = 3;

    /// A helper that continuously allocates a few objects at a time, tags each object with a
    /// value unique to that allocation, and checks that the tags are unchanged before freeing the
    /// objects. If the arena ever hands out an object that is already allocated, such as due to
    /// an ABA race in the free list, a tag will be overwritten.
    #[pin_data]
    struct ArenaExclusiveFixture<const P: usize, const O: usize> {
        #[pin]
        arena: GPArena<P, O>,
        stop_requested: AtomicBool,
        thread_count: AtomicU64,
    }

    impl<const P: usize, const O: usize> ArenaExclusiveFixture<P, O> {
        const _ALIGN_CHECK: () = {
            assert!(
                O.is_multiple_of(core::mem::size_of::<u64>()),
                "Objects must be able to hold an aligned tag"
            );
        };

        fn new() -> impl PinInit<Self> {
            let () = Self::_ALIGN_CHECK;
            pin_init!(Self {
                arena <- GPArena::new(),
                stop_requested: AtomicBool::new(false),
                thread_count: AtomicU64::new(0),
            })
        }

        /// Tags are accessed atomically, as multiple threads may access the same object if the
        /// arena is broken.
        ///
        /// # Safety
        ///
        /// `object` must point to an 8-byte aligned slot of size at least 8 bytes in the arena.
        unsafe fn tag(&self, object: NonNull<u8>) -> &AtomicU64 {
            // SAFETY: Caller guarantees `object` is an 8-byte aligned slot of size `O >= 8` in
            // `self.arena`.
            unsafe { &*object.as_ptr().cast::<AtomicU64>() }
        }

        /// A thread_start_routine for use with `thread::create_with_priority`.
        extern "C" fn run_entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to a pinned `ArenaExclusiveFixture` that outlives all worker
            // threads joined before the test returns.
            let fixture = unsafe { &*arg.cast::<Self>() };
            fixture.run()
        }

        fn run(&self) -> i32 {
            // Start each thread's tags in a different range, so that tags are unique across
            // threads.
            let mut next_tag = (self.thread_count.fetch_add(1, Ordering::Relaxed) + 1) << 48;
            let mut iterations = 1u32;
            loop {
                let mut objects: [Option<NonNull<u8>>; OBJECTS_PER_THREAD] =
                    [None; OBJECTS_PER_THREAD];
                let mut tags = [0u64; OBJECTS_PER_THREAD];
                let mut result = 0;
                for i in 0..OBJECTS_PER_THREAD {
                    let Some(obj) = self.arena.alloc() else {
                        result = -1;
                        break;
                    };
                    objects[i] = Some(obj);
                    tags[i] = next_tag;
                    next_tag += 1;
                    // SAFETY: `obj` is an 8-byte aligned slot of size `O >= 8` in `self.arena`.
                    unsafe {
                        self.tag(obj).store(tags[i], Ordering::Relaxed);
                    }
                }
                // Free every object we allocated, even if there was an error, so the arena can be
                // destroyed.
                for i in 0..OBJECTS_PER_THREAD {
                    let Some(obj) = objects[i] else {
                        break;
                    };
                    // SAFETY: `obj` is currently allocated from `self.arena`.
                    unsafe {
                        let actual = self.tag(obj).load(Ordering::Relaxed);
                        if actual != tags[i] {
                            result = -2;
                        }
                        self.arena.free(obj);
                    }
                }
                if result != 0 {
                    return result;
                }
                // Check every so often if someone is trying to stop us.
                if iterations.is_multiple_of(100) && self.stop_requested.load(Ordering::Relaxed) {
                    return 0;
                }
                iterations = iterations.wrapping_add(1);
            }
        }
    }

    /// Verifies exclusive object ownership across 8 racing worker threads (ABA prevention).
    #[test]
    fn parallel_alloc_exclusive() {
        type Fixture = ArenaExclusiveFixture<0, 8>;
        // Use more threads than most test environments have CPUs, so that threads are sometimes
        // preempted part way through updating the free list, as well as racing with each other.
        const NUM_THREADS: usize = 8;

        stack_pin_init!(let fixture = Fixture::new());
        assert_ok!(
            fixture.as_mut().project().arena.init(b"test", NUM_THREADS * OBJECTS_PER_THREAD)
        );
        let fixture_ref = &*fixture;
        let arg = ptr::from_ref(fixture_ref).cast_mut().cast::<c_void>();

        let mut threads: [Option<ThreadPtr>; NUM_THREADS] = [None; NUM_THREADS];
        for slot in &mut threads {
            // SAFETY: `fixture` stays pinned on the stack until all threads are joined below.
            let t = unsafe {
                let t = thread::create_with_priority(
                    c"gparena worker".as_ptr(),
                    Fixture::run_entry,
                    arg,
                    DEFAULT_PRIORITY,
                )
                .expect("worker thread creation should succeed");
                t.resume();
                t
            };
            *slot = Some(t);
        }

        // Let the threads run for a bit, then stop them and check that none of them found an
        // error.
        let _ = thread::sleep_relative(DurationMono::from_millis(500));
        fixture_ref.stop_requested.store(true, Ordering::Relaxed);
        for slot in &mut threads {
            let ret = join_cleanup(slot);
            expect_true!(ret.is_some());
            if let Some(code) = ret {
                expect_eq!(code, 0);
            }
        }
    }

    /// Verifies concurrent allocation while growing backing memory across page boundaries.
    #[test]
    fn parallel_grow_memory() {
        const COUNT: usize = page::SIZE * 32 / 8;

        let mut allocs = unwrap_ok!(alloc_ptr_slice(COUNT));

        type Fixture = ArenaAllocFixture<0, 8>;
        stack_pin_init!(let fixture = Fixture::new());
        assert_ok!(fixture.as_mut().project().arena.init(b"test", COUNT));
        let fixture_ref = &*fixture;
        let arg = ptr::from_ref(fixture_ref).cast_mut().cast::<c_void>();

        // Spin up a worker that will perform allocations in parallel whilst we are causing the
        // arena to need to be grown.
        // SAFETY: `fixture` stays pinned on the stack until `t` is joined below.
        let mut t = unsafe {
            let t = thread::create_with_priority(
                c"gparena worker".as_ptr(),
                Fixture::run_entry,
                arg,
                DEFAULT_PRIORITY,
            )
            .expect("worker thread creation should succeed");
            t.resume();
            Some(t)
        };

        // Let the worker run for a bit to make sure it has started.
        expect_true!(probe_still_running(
            &mut t,
            current_mono_time() + DurationMono::from_millis(10)
        ));

        // Allocate all the rest of the objects causing arena to have to grow.
        for slot in &mut allocs[..COUNT - 1] {
            let p = fixture_ref.arena.alloc();
            expect_true!(p.is_some());
            *slot = p;
        }

        // Worker should still be running fine.
        expect_true!(probe_still_running(
            &mut t,
            current_mono_time() + DurationMono::from_millis(10)
        ));

        // Cleanup.
        fixture_ref.stop_requested.store(true, Ordering::Relaxed);
        let ret = join_cleanup(&mut t);
        expect_true!(ret.is_some());
        if let Some(code) = ret {
            expect_eq!(code, 0);
        }

        for slot in &allocs[..COUNT - 1] {
            let p = unwrap_some!(*slot);
            // SAFETY: Every slot in `allocs[..COUNT - 1]` is currently allocated from `arena`.
            unsafe {
                fixture_ref.arena.free(p);
            }
        }
    }

    /// Verifies `confine` and `lookup_confined` bounds-check bypass hardening.
    #[test]
    fn test_confine() {
        const MAX_ITEMS: usize = 32;
        const SIZE: usize = 64;

        stack_try_pin_init!(let arena = GPArena::<0, SIZE>::new_init(b"test", MAX_ITEMS));
        let arena = unwrap_ok!(arena);
        let mut allocs = [None; MAX_ITEMS];

        expect_true!(arena.lookup_confined(0).is_none());

        for slot in &mut allocs {
            *slot = arena.alloc();
        }

        let base = arena.base().addr();
        for i in 0..(2 * MAX_ITEMS) {
            let offset = i * SIZE;
            let expected_object = base + offset;
            if let Some(&slot) = allocs.get(i) {
                expect_eq!(expected_object, arena.confine(expected_object));
                expect_true!(arena.lookup_confined(offset) == slot);
            } else {
                expect_eq!(base, arena.confine(expected_object));
                expect_true!(arena.lookup_confined(offset).is_none());
            }
        }

        expect_eq!(base, arena.confine(usize::MAX));
        expect_eq!(base, arena.confine(base - 1));
        expect_eq!(base, arena.confine(base - SIZE - 1));
        expect_true!(arena.lookup_confined(usize::MAX).is_none());

        for slot in allocs {
            // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }
    }

    /// Verifies multi-page objects (`OBJECT_SIZE > 4 * page::SIZE`) and `init` overflow rejection.
    #[test]
    fn large_multi_page_objects_and_overflow() {
        // Overflow in `max_count * OBJECT_SIZE` must fail cleanly with `Status::OUT_OF_RANGE`.
        {
            stack_pin_init!(let arena = GPArena::<0, { 1 << 44 }>::new());
            expect_true!(arena.as_mut().init(b"overflow", 1 << 21) == Err(Status::OUT_OF_RANGE));
        }

        // Overflow when rounding `raw_sz` up to a page boundary must also fail with
        // `Status::OUT_OF_RANGE`.
        {
            stack_pin_init!(
                let arena = GPArena::<0, { usize::MAX & !(page::SIZE - 1) }>::new()
            );
            expect_true!(arena.as_mut().init(b"overflow_align", 1) == Err(Status::OUT_OF_RANGE));
        }

        // Multi-page objects (`5 * page::SIZE > 4 * page::SIZE`, and not a multiple of
        // `4 * page::SIZE`) must grow and preserve cleanly across every mapped page.
        const PRESERVE: usize = 16;
        const OBJ_SIZE: usize = 5 * page::SIZE;
        const COUNT: usize = 3;
        const MAGIC: &[u8; PRESERVE] = b"multipage-arena!";
        const TAIL_BYTE: u8 = 0xa5;

        stack_pin_init!(let arena = GPArena::<PRESERVE, OBJ_SIZE>::new());
        assert_ok!(arena.as_mut().init(b"multipage", COUNT));

        let mut allocs = [None; COUNT];
        for slot in &mut allocs {
            let p = arena.alloc();
            assert_true!(p.is_some());
            let p = p.unwrap();
            // SAFETY: `p` points to a uniquely owned `OBJ_SIZE`-byte slot in `arena`.
            unsafe {
                ptr::copy_nonoverlapping(MAGIC.as_ptr(), p.as_ptr(), PRESERVE);
                ptr::write(p.as_ptr().add(OBJ_SIZE - 1), TAIL_BYTE);
                expect_eq!(ptr::read(p.as_ptr().add(OBJ_SIZE - 1)), TAIL_BYTE);
            }
            *slot = Some(p);
        }
        expect_true!(arena.alloc().is_none());

        for slot in allocs {
            // SAFETY: Every slot in `allocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }

        // Reallocate all `COUNT` slots before freeing so every slot in the LIFO free list is
        // popped and verified.
        let mut reallocs = [None; COUNT];
        for slot in &mut reallocs {
            let p = arena.alloc();
            assert_true!(p.is_some());
            *slot = p;
        }
        expect_true!(arena.alloc().is_none());

        for slot in reallocs {
            let p = slot.unwrap();
            // SAFETY: `p` points to an allocated `OBJ_SIZE`-byte slot in `arena` whose first
            // `PRESERVE` bytes hold `MAGIC` and whose last byte is mapped and writable.
            unsafe {
                let slice = core::slice::from_raw_parts(p.as_ptr(), PRESERVE);
                expect_true!(slice == MAGIC);
                ptr::write(p.as_ptr().add(OBJ_SIZE - 1), TAIL_BYTE ^ 0xff);
                expect_eq!(ptr::read(p.as_ptr().add(OBJ_SIZE - 1)), TAIL_BYTE ^ 0xff);
            }
        }

        for slot in reallocs {
            // SAFETY: Every slot in `reallocs` is currently allocated from `arena`.
            unsafe { arena.free(slot.unwrap()) };
        }
    }
}
