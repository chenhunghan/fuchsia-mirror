// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel heap allocation wrappers, statistics, and page backing integration.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr::{self, NonNull};

use crate::arch_rs::blocking_disallowed as arch_blocking_disallowed;
#[cfg(kernel_memory_profiler)]
use crate::kernel::thread::current_get_backtrace;
use crate::kernel::thread::current_memory_allocation_state_is_enabled;
use crate::virtual_alloc::VirtualAlloc;
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::page_state::VmPageState;
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm::{self, AllocFailure, AllocFailureType, free_list as pmm_free};
use crate::vm::vm::{
    vaddr_to_paddr, vm_get_kernel_heap_base, vm_get_kernel_heap_size, vm_using_virtual_heap,
};
use core::pin::Pin;
use debug::ltracef;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode};
use kprint::kprintln;
#[cfg(kernel_memory_profiler)]
use ksync::LockToken;
use ksync::{KCell, lock};
use lazy_init::LazyInit;
use pin_init::stack_pin_init;

#[cfg(kernel_memory_profiler)]
use super::cmpctmalloc::cmpct_get_size;
use super::cmpctmalloc::{
    HEAP_MAX_ALLOC_SIZE, cmpct_alloc, cmpct_free, cmpct_get_cookie, cmpct_get_info, cmpct_init,
    cmpct_memalign, cmpct_set_cookie, cmpct_set_fill_on_alloc_threshold, cmpct_sized_free,
};
#[cfg(kernel_memory_profiler)]
use super::profile::HeapProfileMap;

const LOCAL_TRACE: u32 = 0;

const HEAP_PANIC_ON_ALLOC_FAIL: bool = cfg!(lk_debuglevel_gt2);

/// Whether per-callsite heap statistics collection is enabled at build time.
pub const HEAP_COLLECT_STATS: bool = false;

const KERNEL_MEMORY_PROFILER: bool = cfg!(kernel_memory_profiler);

// Kernel memory profiler and heap collect statistics cannot be enabled at the same time.
zr::static_assert!(!(KERNEL_MEMORY_PROFILER && HEAP_COLLECT_STATS));

/// Align the heap to 2MiB to optionally support large page mappings in it.
const ARCH_HEAP_ALIGNMENT_BITS: usize = 21;

#[cfg(sanitize = "address")]
use crate::instrumentation::asan::{ASAN_INTERNAL_HEAP_MAGIC, asan_poison_shadow};

crate::ksync_kernel::declare_singleton_spinlock!(StatLock);

/// Keep a list of unique caller:size sites in a list.
#[cfg(kernel_memory_profiler)]
type KernelHeapProfileMap = HeapProfileMap<{ 1024 * page::SIZE }, { 4096 * 4 }, 4096>;

#[cfg(kernel_memory_profiler)]
#[repr(transparent)]
struct AllocMapCell(KCell<KernelHeapProfileMap, StatLock>);

#[cfg(kernel_memory_profiler)]
zr::static_assert!(core::mem::align_of::<AllocMapCell>() == page::SIZE);

#[cfg(kernel_memory_profiler)]
impl AllocMapCell {
    #[inline]
    fn get<'a>(&'static self, token: &'a mut LockToken<'_, StatLock>) -> &'a KernelHeapProfileMap {
        // SAFETY: `StatLock` is a singleton spinlock and `self.0` is the singleton guarded by it.
        // We obtain a shared `&KernelHeapProfileMap` (while requiring `&mut LockToken` for
        // exclusive lock ownership) so we never create a `&mut` reference covering the
        // concurrently-read profile buffer inside `UnsafeCell`.
        unsafe { self.0.get(token) }
    }
}

#[cfg(kernel_memory_profiler)]
static ALLOC_MAP: AllocMapCell = AllocMapCell(KCell::new(KernelHeapProfileMap::new_zeroed()));

#[derive(DoublyLinkedListContainable)]
struct AllocStat {
    caller: usize,
    size: usize,
    alloc_count: u64,
    free_count: u64,
    index: u32,
    #[dll_node]
    node: DoublyLinkedListNode<AllocStat>,
}

impl AllocStat {
    const fn new() -> Self {
        Self {
            caller: 0,
            size: 0,
            alloc_count: 0,
            free_count: 0,
            index: 0,
            node: DoublyLinkedListNode::new(),
        }
    }
}

const NUM_STATS: usize = if HEAP_COLLECT_STATS { 1024 } else { 1 };

#[pin_init::pin_data]
struct StatState {
    next_unused_stat: u32,
    stats: [AllocStat; NUM_STATS],
    #[pin]
    stat_list: DoublyLinkedList<NonNull<AllocStat>>,
}

// SAFETY: StatState is only accessed via StatLock, and the underlying data doesn't require
// thread affinity.
unsafe impl Send for StatState {}
// SAFETY: StatState is only accessed via StatLock.
unsafe impl Sync for StatState {}

static STAT_STATE: LazyInit<KCell<StatState, StatLock>> = LazyInit::uninit();

struct SyncVirtualAlloc(UnsafeCell<VirtualAlloc>);

// SAFETY: `VIRTUAL_ALLOC` is initialized during single-threaded `heap_init()` before `cmpct_init()`
// and accessed in `heap_page_alloc` / `heap_page_free` while `TheHeapLock` is held.
unsafe impl Sync for SyncVirtualAlloc {}

impl SyncVirtualAlloc {
    const fn new(state: VmPageState) -> Self {
        Self(UnsafeCell::new(VirtualAlloc::new(state)))
    }

    /// # Safety
    ///
    /// Caller must ensure exclusive access (either single-threaded `heap_init` or while holding
    /// `TheHeapLock`).
    #[sanitize(address = "off")]
    #[allow(clippy::mut_from_ref)]
    #[inline]
    unsafe fn get_mut(&self) -> &mut VirtualAlloc {
        // SAFETY: Caller guarantees exclusive access.
        unsafe { &mut *self.0.get() }
    }
}

static VIRTUAL_ALLOC: SyncVirtualAlloc = SyncVirtualAlloc::new(VmPageState::HEAP);

/// Increments the `alloc_count` for the given `caller`+`size` reference and associates it with the
/// given `heap_ptr`, which should be the result from a `cmpct_alloc` call.
///
/// # Safety
///
/// `heap_ptr` must be null or a valid live allocation pointer returned by `cmpct_alloc` or
/// `cmpct_memalign`.
#[inline]
unsafe fn add_alloc_stat(caller: *mut c_void, size: usize, heap_ptr: *mut c_void) {
    if !HEAP_COLLECT_STATS && !KERNEL_MEMORY_PROFILER {
        return;
    }
    if heap_ptr.is_null() {
        return;
    }

    #[cfg(kernel_memory_profiler)]
    {
        let _ = (caller, size);
        // Get rounded up size.
        // SAFETY: `heap_ptr` is a valid non-null allocation returned by `cmpct_alloc` or
        // `cmpct_memalign`.
        let act_size = unsafe { cmpct_get_size(heap_ptr) };
        let handle = profile_track_alloc(act_size as usize);
        // SAFETY: `heap_ptr` is a valid non-null allocation returned by `cmpct_alloc` or
        // `cmpct_memalign`.
        unsafe { cmpct_set_cookie(heap_ptr, handle) };
        // Not compatible with HEAP_COLLECT_STATS because both use the cookie.
    }

    #[cfg(not(kernel_memory_profiler))]
    {
        lock!(let mut guard = StatLock::get().lock());
        // SAFETY: StatLock is held, so mutating STAT_STATE is safe.
        let state = unsafe { STAT_STATE.get().get_mut(guard.token_mut()) };

        // look for an existing stat, bump the count and move to head if found
        let mut found = None;
        // SAFETY: state.stat_list is structurally pinned.
        for s in unsafe { Pin::new_unchecked(&mut state.stat_list) }.iter_mut() {
            // SAFETY: s is never pinned, we just need the reference.
            let s = unsafe { core::pin::Pin::into_inner_unchecked(s) };
            if s.caller == caller.addr() && s.size == size {
                s.alloc_count += 1;
                let index = s.index;
                // SAFETY: `heap_ptr` is a valid non-null allocation returned by `cmpct_alloc` or
                // `cmpct_memalign`.
                unsafe { cmpct_set_cookie(heap_ptr, index) };
                found = Some(index);
                break;
            }
        }

        if let Some(index) = found {
            let node = NonNull::from(&mut state.stats[index as usize]);
            // SAFETY: `node` is in the list, and we are pushing it back to the front.
            unsafe {
                Pin::new_unchecked(&mut state.stat_list).erase(node.as_ref());
                Pin::new_unchecked(&mut state.stat_list).push_front_raw(node);
            }
            return;
        }

        // allocate a new one and add it to the list
        if (state.next_unused_stat as usize) >= NUM_STATS {
            core::hint::cold_path();
            // Set an out of range cookie so add_free_stat will skip later.
            // SAFETY: `heap_ptr` is a valid non-null allocation returned by `cmpct_alloc` or
            // `cmpct_memalign`.
            unsafe { cmpct_set_cookie(heap_ptr, NUM_STATS as u32) };
            return;
        }
        let idx = state.next_unused_stat;
        // SAFETY: `heap_ptr` is a valid non-null allocation returned by `cmpct_alloc` or
        // `cmpct_memalign`.
        unsafe { cmpct_set_cookie(heap_ptr, idx) };

        let s = &mut state.stats[idx as usize];
        s.caller = caller.addr();
        s.size = size;
        s.alloc_count = 1;
        s.free_count = 0;
        s.index = idx;

        let node = NonNull::from(&mut state.stats[idx as usize]);
        // SAFETY: `node` is unlinked and being added to the front.
        unsafe { Pin::new_unchecked(&mut state.stat_list).push_front_raw(node) };
        state.next_unused_stat += 1;
    }
}

/// Increments the `free_count` stat associated with the given heap pointer. This is the pointer
/// returned by `cmpct_alloc` and should have previously been given to `add_alloc_stat`.
/// `add_free_stat` must be called *before* passing the ptr to `cmpct_free`.
///
/// # Safety
///
/// `heap_ptr` must be null or a valid live allocation pointer returned by `cmpct_alloc` or
/// `cmpct_memalign`.
#[inline]
unsafe fn add_free_stat(heap_ptr: *mut c_void) {
    if !HEAP_COLLECT_STATS && !KERNEL_MEMORY_PROFILER {
        return;
    }
    if heap_ptr.is_null() {
        return;
    }

    #[cfg(kernel_memory_profiler)]
    {
        // SAFETY: `heap_ptr` is a valid non-null allocation being freed before `cmpct_free`.
        let (handle, size) = unsafe { (cmpct_get_cookie(heap_ptr), cmpct_get_size(heap_ptr)) };
        profile_track_free(handle, size as usize);
        // Not compatible with HEAP_COLLECT_STATS.
    }

    #[cfg(not(kernel_memory_profiler))]
    {
        // SAFETY: `heap_ptr` is a valid non-null allocation being freed before `cmpct_free`.
        let index = unsafe { cmpct_get_cookie(heap_ptr) };
        if (index as usize) >= NUM_STATS {
            return;
        }
        lock!(let mut guard = StatLock::get().lock());
        // SAFETY: StatLock is held, so mutating STAT_STATE is safe.
        let state = unsafe { STAT_STATE.get().get_mut(guard.token_mut()) };
        state.stats[index as usize].free_count += 1;
    }
}

#[inline]
fn is_size_in_range(size: usize) -> bool {
    size > 0 && size <= HEAP_MAX_ALLOC_SIZE
}

/// Initializes the kernel heap and its backing virtual allocator (when enabled).
///
/// # Safety
///
/// The caller must ensure this is called once during early boot before concurrent heap operations.
#[inline(always)]
pub(crate) unsafe fn rust_heap_init() {
    #[cfg(not(kernel_memory_profiler))]
    {
        // Initialize the LazyInit static since DoublyLinkedList::new() returns impl PinInit.
        // SAFETY: rust_heap_init is called once during early boot.
        unsafe {
            core::pin::Pin::static_ref(&STAT_STATE)
                .init_pin(KCell::pin_init(pin_init::pin_init!(StatState {
                    next_unused_stat: 0,
                    stats: [const { AllocStat::new() }; NUM_STATS],
                    stat_list <- DoublyLinkedList::new(),
                })))
                .unwrap();
        }
    }
    #[cfg(kernel_memory_profiler)]
    {
        lock!(let mut guard = StatLock::get().lock());
        ALLOC_MAP.get(guard.token_mut()).init();
    }

    // Note: VIRTUAL_ALLOC is already const-initialized with VmPageState::HEAP at compile time
    // (`static VIRTUAL_ALLOC: SyncVirtualAlloc = SyncVirtualAlloc::new(VmPageState::HEAP);`),
    // matching `virtual_alloc.Initialize(vm_page_state::HEAP)` in C++.
    // SAFETY: `vm_init_preheap` runs before `heap_init` during early boot.
    if vm_using_virtual_heap() {
        // SAFETY: `heap_init` runs once during single-threaded early boot before `cmpct_init`.
        let va = unsafe { VIRTUAL_ALLOC.get_mut() };
        // SAFETY: `vm_init_preheap` has completed and `cpp_vm_using_virtual_heap()` returned true,
        // so the kernel heap VMAR is initialized.
        let (heap_base, heap_size) = (vm_get_kernel_heap_base(), vm_get_kernel_heap_size());
        if let Err(status) = va.init(heap_base, heap_size, 1, ARCH_HEAP_ALIGNMENT_BITS) {
            {
                core::hint::cold_path();
                panic!("Failed to initialized heap backing allocator: {}", status.into_raw())
            }
        }

        let bitmap_pages = va.debug_bitmap_pages();
        kprintln!(
            "Kernel heap [{:x}, {:x}) using {} pages ({} KiB) for tracking bitmap",
            heap_base,
            heap_base + heap_size,
            bitmap_pages,
            bitmap_pages * (page::SIZE / 1024)
        );
    }

    // SAFETY: Caller guarantees `heap_init` is called once during boot before concurrent heap ops.
    unsafe { cmpct_init() };
}

unsafe extern "C" {
    #[link_name = "llvm.returnaddress"]
    fn return_address(level: i32) -> *mut core::ffi::c_void;
}

#[inline(always)]
fn get_caller() -> *mut c_void {
    // SAFETY: Calling LLVM intrinsic return_address(0) is safe.
    unsafe { return_address(0).cast() }
}

/// Allocates `size` bytes from the kernel heap.
///
/// # Safety
///
/// The caller must ensure that the kernel heap has been initialized via [`heap_init`] and blocking
/// / memory allocation is permitted on the current thread.
#[inline(always)]
pub(crate) unsafe fn malloc(size: usize) -> *mut c_void {
    debug_assert!(!arch_blocking_disallowed());
    debug_assert!(current_memory_allocation_state_is_enabled());

    ltracef!("size {}\n", size);

    // SAFETY: Caller guarantees the heap is initialized and `TheHeapLock` is not held.
    let ptr = unsafe { cmpct_alloc(size) };
    // SAFETY: `ptr` is null or a valid live allocation just returned by `cmpct_alloc`.
    unsafe { add_alloc_stat(get_caller(), size, ptr) };

    if console::trace_enabled() {
        core::hint::cold_path();
        let mut tr_caller = get_caller();
        if tr_caller.is_null() {
            tr_caller = get_caller();
        }
        kprintln!("caller {:p} malloc {} -> {:p}", tr_caller, size, ptr);
    }

    if HEAP_PANIC_ON_ALLOC_FAIL && ptr.is_null() && is_size_in_range(size) {
        {
            core::hint::cold_path();
            panic!("malloc of size {} failed", size)
        };
    }

    ptr
}

/// Allocates `size` bytes aligned to `alignment` bytes from the kernel heap.
///
/// # Safety
///
/// The caller must ensure that `alignment` is a power of two (or `< 8`), the kernel heap has been
/// initialized via [`heap_init`], and blocking / memory allocation is permitted on the current
/// thread.
#[inline(always)]
pub(crate) unsafe fn memalign(alignment: usize, size: usize) -> *mut c_void {
    debug_assert!(!arch_blocking_disallowed());
    debug_assert!(current_memory_allocation_state_is_enabled());

    ltracef!("alignment {}, size {}\n", alignment, size);

    // SAFETY: Caller guarantees `alignment` is valid, the heap is initialized, and `TheHeapLock`
    // is not held.
    let ptr = unsafe { cmpct_memalign(alignment, size) };
    // SAFETY: `ptr` is null or a valid live allocation just returned by `cmpct_memalign`.
    unsafe { add_alloc_stat(get_caller(), size, ptr) };
    if console::trace_enabled() {
        core::hint::cold_path();
        let mut tr_caller = get_caller();
        if tr_caller.is_null() {
            tr_caller = get_caller();
        }
        kprintln!("caller {:p} memalign {}, {} -> {:p}", tr_caller, alignment, size, ptr);
    }

    if HEAP_PANIC_ON_ALLOC_FAIL && ptr.is_null() && is_size_in_range(size) {
        {
            core::hint::cold_path();
            panic!("memalign of size {} align {} failed", size, alignment)
        };
    }

    ptr
}

/// Allocates and zero-initializes `count * size` bytes from the kernel heap.
///
/// # Safety
///
/// The caller must ensure that the kernel heap has been initialized via [`heap_init`] and blocking
/// / memory allocation is permitted on the current thread.
#[inline(always)]
pub(crate) unsafe fn calloc(count: usize, size: usize) -> *mut c_void {
    debug_assert!(!arch_blocking_disallowed());
    debug_assert!(current_memory_allocation_state_is_enabled());

    ltracef!("count {}, size {}\n", count, size);

    let realsize = match count.checked_mul(size) {
        Some(r) if r <= HEAP_MAX_ALLOC_SIZE => r,
        _ => {
            core::hint::cold_path();
            return core::ptr::null_mut();
        }
    };

    // SAFETY: Caller guarantees the heap is initialized and `TheHeapLock` is not held.
    let ptr = unsafe { cmpct_alloc(realsize) };
    // SAFETY: `ptr` is null or a valid live allocation just returned by `cmpct_alloc`.
    unsafe { add_alloc_stat(get_caller(), size, ptr) };
    if !ptr.is_null() {
        // SAFETY: `ptr` is non-null and points to a newly allocated, uniquely-owned buffer of at
        // least `realsize` bytes from `cmpct_alloc(realsize)`.
        unsafe { ptr::write_bytes(ptr.cast::<u8>(), 0, realsize) };
    }
    if console::trace_enabled() {
        core::hint::cold_path();
        let mut tr_caller = get_caller();
        if tr_caller.is_null() {
            tr_caller = get_caller();
        }
        kprintln!("caller {:p} calloc {}, {} -> {:p}", tr_caller, count, size, ptr);
    }
    ptr
}

/// Frees `ptr` previously returned by [`malloc`], [`memalign`], or [`calloc`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is null or a live allocation returned by [`malloc`],
/// [`memalign`], or [`calloc`], and blocking is permitted on the current thread.
#[inline(always)]
pub(crate) unsafe fn free(ptr: *mut c_void) {
    debug_assert!(!arch_blocking_disallowed());

    ltracef!("ptr {:p}\n", ptr);
    if console::trace_enabled() {
        core::hint::cold_path();
        let mut tr_caller = get_caller();
        if tr_caller.is_null() {
            tr_caller = get_caller();
        }
        kprintln!("caller {:p} free {:p}", tr_caller, ptr);
    }
    // SAFETY: Caller guarantees `ptr` is null or a valid live allocation pointer not yet freed.
    unsafe { add_free_stat(ptr) };

    // SAFETY: Caller guarantees `ptr` is null or a valid live allocation pointer, and
    // `TheHeapLock` is not held.
    unsafe { cmpct_free(ptr) };
}

/// Frees `ptr` of expected size `s` previously returned by [`malloc`], [`memalign`], or [`calloc`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is null or a live allocation of at least `s` bytes returned
/// by [`malloc`], [`memalign`], or [`calloc`], and blocking is permitted on the current thread.
#[inline(always)]
pub(crate) unsafe fn sized_free(ptr: *mut c_void, s: usize) {
    debug_assert!(!arch_blocking_disallowed());

    ltracef!("ptr {:p} size {}\n", ptr, s);
    if console::trace_enabled() {
        core::hint::cold_path();
        let mut tr_caller = get_caller();
        if tr_caller.is_null() {
            tr_caller = get_caller();
        }
        kprintln!("caller {:p} free {:p} size {}", tr_caller, ptr, s);
    }
    // SAFETY: Caller guarantees `ptr` is null or a valid live allocation pointer not yet freed.
    unsafe { add_free_stat(ptr) };
    // SAFETY: Caller guarantees `ptr` is null or a valid live allocation pointer of at least `s`
    // bytes, and `TheHeapLock` is not held.
    unsafe { cmpct_sized_free(ptr, s) };
}

/// Returns the address and size of the heap profile buffer if it exists, and `None` otherwise.
///
/// The buffer is wired kernel memory that lives for the lifetime of the kernel and is written
/// concurrently by the heap profiler, so it must not be read through a Rust reference.
#[inline]
pub fn get_heap_profile() -> Option<(NonNull<u8>, usize)> {
    #[cfg(kernel_memory_profiler)]
    {
        lock!(let mut guard = StatLock::get().lock());
        let (buf_ptr, buf_size) = ALLOC_MAP.get(guard.token_mut()).data_ptr();
        NonNull::new(buf_ptr.cast_mut()).map(|ptr| (ptr, buf_size))
    }
    #[cfg(not(kernel_memory_profiler))]
    {
        None
    }
}

/// Tracks an allocation of the given `size` in the heap profiler. This can be used to make visible
/// allocations that, while not from the general kernel heap, are notionally heap-like or from
/// separate slab / other non-general heap allocators.
/// The return value is a cookie that, along with `size` can be passed to `profile_track_free` to
/// indicate deallocation.
#[inline(always)]
pub(crate) fn profile_track_alloc(size: usize) -> u32 {
    #[cfg(kernel_memory_profiler)]
    {
        let mut bt = crate::backtrace::Backtrace::new();
        current_get_backtrace(&mut bt);
        lock!(let mut guard = StatLock::get().lock());
        let alloc_map = ALLOC_MAP.get(guard.token_mut());
        let (counters, handle) = alloc_map.try_get(bt.as_slice());
        if let Some(counters) = counters {
            counters.allocate(size as u64);
            return handle as u32;
        }
        core::hint::cold_path();
        alloc_map.event_dropped();
    }
    #[cfg(not(kernel_memory_profiler))]
    {
        let _ = size;
    }
    0
}

/// Tracks deallocation of `size` bytes associated with `handle` in the heap profiler.
#[inline(always)]
pub(crate) fn profile_track_free(handle: u32, size: usize) {
    #[cfg(kernel_memory_profiler)]
    {
        lock!(let mut guard = StatLock::get().lock());
        let alloc_map = ALLOC_MAP.get(guard.token_mut());
        if let Some(counters) = alloc_map.try_by_handle(handle as usize) {
            counters.deallocate(size as u64);
        } else {
            core::hint::cold_path();
            alloc_map.event_dropped();
        }
    }
    #[cfg(not(kernel_memory_profiler))]
    {
        let _ = (handle, size);
    }
}

/// Gets stats about the heap.
/// Returns `(total_bytes, free_bytes)`, where `total_bytes` is the total size of the heap (the sum
/// of all pages allocated from the PMM) and `free_bytes` is the free portion.
#[inline]
pub fn heap_get_info() -> (usize, usize) {
    let info = cmpct_get_info();
    (info.used_bytes + info.cached_bytes, info.free_bytes)
}

/// Allocates `pages` contiguous pages from the virtual heap or PMM to back the compact heap.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `pages > 0`.
#[sanitize(address = "off")]
pub unsafe fn heap_page_alloc(pages: usize) -> *mut c_void {
    debug_assert!(pages > 0);

    // SAFETY: `heap_page_alloc` is only called after `vm_init_preheap` and `heap_init`.
    if vm_using_virtual_heap() {
        // SAFETY: `TheHeapLock` is held by the caller, granting exclusive access to
        // `VIRTUAL_ALLOC`.
        let va = unsafe { VIRTUAL_ALLOC.get_mut() };
        let vaddr = match va.alloc_pages(pages) {
            Ok(vaddr) => vaddr,
            Err(status) => {
                core::hint::cold_path();
                kprintln!("Failed to allocate {} pages for heap: {}", pages, status.into_raw());
                return ptr::null_mut();
            }
        };

        #[cfg(sanitize = "address")]
        {
            // SAFETY: `[vaddr, vaddr + pages * page::SIZE)` is a freshly mapped kernel heap range
            // backed by KASAN shadow memory, and `ASAN_INTERNAL_HEAP_MAGIC` is a valid poison
            // byte.
            unsafe { asan_poison_shadow(vaddr, pages * page::SIZE, ASAN_INTERNAL_HEAP_MAGIC) };
        }

        let ret = ptr::with_exposed_provenance_mut::<c_void>(vaddr);
        ltracef!("pages {}: va {:p}\n", pages, ret);
        ret
    } else {
        stack_pin_init!(let list = VmPageDoublyLinkedList::new());

        let Ok(pa) = pmm::alloc_contiguous(pages, 0, page::SHIFT as u8, list.as_mut()) else {
            core::hint::cold_path();
            return ptr::null_mut();
        };

        // mark all of the allocated page as HEAP
        while let Some(p) = list.as_mut().pop_front() {
            // SAFETY: `p` is a newly allocated PMM page exclusively owned by the caller.
            let page_ref = unsafe { p.as_ref() };
            // SAFETY: `page_ref` is a newly allocated PMM page exclusively owned by the caller.
            unsafe { page_ref.set_state(VmPageState::HEAP) };
            #[cfg(sanitize = "address")]
            {
                let vaddr = paddr_to_physmap(page_ref.paddr());
                // SAFETY: `[vaddr, vaddr + page::SIZE)` is a valid physmap page backed by KASAN
                // shadow memory, and `ASAN_INTERNAL_HEAP_MAGIC` is a valid poison byte.
                unsafe { asan_poison_shadow(vaddr.0, page::SIZE, ASAN_INTERNAL_HEAP_MAGIC) };
            }
        }

        ltracef!(
            "pages {}: pa {:#x}, va {:p}\n",
            pages,
            pa.0,
            ptr::with_exposed_provenance_mut::<c_void>(paddr_to_physmap(pa).0)
        );

        ptr::with_exposed_provenance_mut::<c_void>(paddr_to_physmap(pa).0)
    }
}

/// Frees `pages` contiguous pages starting at `ptr` back to the virtual heap or PMM.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `ptr` is a page-aligned pointer previously
/// returned by [`heap_page_alloc`], and `pages > 0`.
#[sanitize(address = "off")]
pub unsafe fn heap_page_free(ptr: *mut c_void, mut pages: usize) {
    debug_assert!(page::is_page_rounded(ptr.addr()));
    debug_assert!(pages > 0);

    ltracef!("ptr {:p}, pages {}\n", ptr, pages);

    // SAFETY: `heap_page_free` is only called after `vm_init_preheap` and `heap_init`.
    if vm_using_virtual_heap() {
        let vaddr = ptr.addr();
        // SAFETY: `TheHeapLock` is held by the caller, granting exclusive access to
        // `VIRTUAL_ALLOC`.
        unsafe { VIRTUAL_ALLOC.get_mut() }.free_pages(vaddr, pages);
    } else {
        let mut cur = ptr.cast::<u8>();

        stack_pin_init!(let list = VmPageDoublyLinkedList::new());

        while pages > 0 {
            if let Some(p) = pmm::paddr_to_vm_page(vaddr_to_paddr(cur.cast())) {
                debug_assert!(p.state() == VmPageState::HEAP);
                // SAFETY: `p` is a valid `vm_page_t` in the PMM arena.
                debug_assert!(!unsafe { p.as_non_null().as_ref() }.get_node().in_container());
                // SAFETY: `p` is an allocated `HEAP` page not currently in any container, and it
                // remains valid until `pmm_free` consumes `list` below.
                unsafe { list.as_mut().push_back_raw(p.as_non_null()) };
            }

            cur = cur.wrapping_add(page::SIZE);
            pages -= 1;
        }

        // SAFETY: `list` contains valid allocated `HEAP` pages ready to be returned to the PMM.
        unsafe { pmm_free(list.as_mut()) };
    }
}

/// Reports a heap allocation failure of `size` bytes to the PMM.
#[sanitize(address = "off")]
#[inline]
///
/// # Safety
///
/// Caller must ensure that `TheHeapLock` is not held.
pub unsafe fn heap_report_alloc_failure(size: usize) {
    pmm::node().report_alloc_failure(AllocFailure {
        r#type: AllocFailureType::Heap,
        size,
        free_count: 0,
    });
}

/// FFI entry point for [`cmpct_set_fill_on_alloc_threshold`].
// SAFETY: The symbol name is unique and matches the C FFI declaration in cmpctmalloc.h.
#[inline(always)]
pub(crate) fn rust_cmpct_set_fill_on_alloc_threshold(size: usize) {
    cmpct_set_fill_on_alloc_threshold(size);
}

/// Console diagnostic helpers for the kernel heap.
#[cfg(console_enabled)]
pub mod console {
    use super::*;
    use crate::vm::heap::cmpctmalloc::{CmpctDumpOptions, cmpct_dump, cmpct_test};

    /// Heap allocation tracing flag.
    ///
    /// Accessed with `Ordering::Relaxed` throughout as an independent diagnostic flag with no
    /// dependent memory operations to synchronize.
    pub static HEAP_TRACE: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    #[inline(always)]
    /// Returns true if heap allocation tracing is enabled.
    pub fn trace_enabled() -> bool {
        HEAP_TRACE.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Dumps collected per-callsite heap statistics.
    pub fn dump_stats() {
        if !HEAP_COLLECT_STATS {
            return;
        }

        lock!(let mut guard = StatLock::get().lock());
        // SAFETY: StatLock is held, so mutating STAT_STATE is safe.
        let state = unsafe { STAT_STATE.get().get_mut(guard.token_mut()) };

        // remove all of them from the list
        // SAFETY: state.stat_list is structurally pinned.
        unsafe { Pin::new_unchecked(&mut state.stat_list) }.clear();

        // reinsert all of the entries, sorted by size
        for i in 0..state.next_unused_stat {
            let mut added = false;
            let item_size = state.stats[i as usize].size;
            let node_to_insert = NonNull::from(&mut state.stats[i as usize]);
            // SAFETY: state.stat_list is structurally pinned.
            let mut cursor = unsafe { Pin::new_unchecked(&mut state.stat_list) }.cursor_front_mut();
            while let Some(node) = cursor.get() {
                if item_size >= node.size {
                    // SAFETY: `node_to_insert` is a valid `AllocStat`.
                    unsafe { cursor.insert_before_raw(node_to_insert) };
                    added = true;
                    break;
                }
                cursor.move_next();
            }
            // fell off the end
            if !added {
                // SAFETY: `node_to_insert` is a valid unlinked `AllocStat`.
                unsafe { Pin::new_unchecked(&mut state.stat_list).push_back_raw(node_to_insert) };
            }
        }

        // dump the list of stats
        // SAFETY: state.stat_list is structurally pinned.
        for s in unsafe { Pin::new_unchecked(&state.stat_list) }.iter() {
            kprintln!(
                "size {:8} alloc_count {:8} free_count {:8} caller {:p}",
                s.size,
                s.alloc_count,
                s.free_count,
                ptr::without_provenance::<c_void>(s.caller)
            );
        }

        if (state.next_unused_stat as usize) >= NUM_STATS {
            kprintln!(
                "WARNING: max number of unique records hit, some statistics were likely lost"
            );
        }
    }

    /// Dumps heap diagnostics using `options`.
    ///
    /// # Safety
    ///
    /// If `options` includes `PANIC_TIME`, the caller must ensure that all other CPUs are halted
    /// in a kernel panic with no concurrent mutation of `THE_HEAP`. Otherwise, the caller must
    /// ensure that `TheHeapLock` is not held on the current thread.
    pub unsafe fn heap_dump(options: CmpctDumpOptions) {
        // SAFETY: The caller upholds the safety requirements of `cmpct_dump`.
        unsafe { cmpct_dump(options) };
    }

    /// Runs the internal heap self-test.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the heap has been initialized and `TheHeapLock` is not held on
    /// the current thread.
    pub unsafe fn heap_test() {
        // SAFETY: The caller upholds the safety requirements of `cmpct_test`.
        unsafe { cmpct_test() };
    }

    /// Toggles heap allocation tracing and returns the new state.
    pub fn toggle_heap_trace() -> bool {
        // `Ordering::Relaxed` is sufficient as `HEAP_TRACE` is an independent diagnostic flag with
        // no dependent memory operations to synchronize.
        let prev = HEAP_TRACE.fetch_xor(true, core::sync::atomic::Ordering::Relaxed);
        !prev
    }
}

/// Console diagnostic helpers for the kernel heap.
#[cfg(not(console_enabled))]
pub mod console {
    #[inline(always)]
    /// Returns true if heap allocation tracing is enabled.
    pub fn trace_enabled() -> bool {
        false
    }
}
