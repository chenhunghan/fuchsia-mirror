// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod page_manager;

use core::cell::RefCell;
use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard};
use std::vec::Vec;

use rand::distr::Uniform;
use rand::rngs::StdRng;
use rand::seq::SliceRandom as _;
use rand::{RngExt as _, SeedableRng as _};

use self::page_manager::PageManager;
use crate::{
    HEAP_MAX_ALLOC_SIZE, cmpct_alloc, cmpct_free, cmpct_get_cookie, cmpct_get_info, cmpct_init,
    cmpct_memalign, cmpct_set_cookie, cmpct_sized_free,
};

/// The underlying `cmpctmalloc` allocator defaults to 8 byte alignment.
const HEAP_DEFAULT_ALIGNMENT: usize = 8;

const RANDOM_SEED: u32 = 101;

/// In the tests below, we wish to allocate until a certain threshold is met.
/// Expressing this threshold in terms of the number of allocations made is not
/// particularly meaningful, especially as the allocation sizes are random and
/// are sensitive to the above seed. Instead, we express this in terms of the
/// number of times that we see the heap grow.
///
/// The current value is picked due to its roundness and the fact that an order
/// more in magnitude would make for a test too slow for automation.
const HEAP_GROWTH_COUNT: usize = 10;

#[derive(Copy, Clone)]
enum FreeOrder {
    Chronological = 0,
    ReverseChronological = 1,
    Random = 2,
}

/// A convenience struct that allows us to allocate memory of random sizes, and
/// then free that memory in various orders.
struct RandomAllocator<'a> {
    _test: &'a CmpctmallocTest,
    generator: StdRng,
    sizes: Uniform<usize>,
    alignment_exponents: Uniform<usize>,
    allocated: Vec<*mut c_void>,
}

impl<'a> RandomAllocator<'a> {
    /// Filling allocated buffers with this value helps ensure that `cmpctmalloc`
    /// is indeed giving us a buffer large enough. For example, were it to give us
    /// anything with overlap with its internal data structures, this fill would
    /// stomp on that and likely result in a large crash.
    const ALLOC_FILL: u8 = 0x51;
    /// `memalign` is only required to accept alignment specifications that are
    /// powers of two and multiples of `size_of::<*mut c_void>()` (guaranteed itself to be a
    /// power of 2).
    const MIN_ALIGNMENT_EXPONENT: usize = size_of::<*mut c_void>().ilog2() as usize;
    const MAX_ALIGNMENT_EXPONENT: usize = page::SHIFT;

    fn new(test: &'a CmpctmallocTest) -> Self {
        Self {
            _test: test,
            generator: StdRng::seed_from_u64(u64::from(RANDOM_SEED)),
            sizes: Uniform::new_inclusive(1, HEAP_MAX_ALLOC_SIZE).unwrap(),
            alignment_exponents: Uniform::new_inclusive(
                Self::MIN_ALIGNMENT_EXPONENT,
                Self::MAX_ALIGNMENT_EXPONENT,
            )
            .unwrap(),
            allocated: Vec::new(),
        }
    }

    fn allocate(&mut self) {
        let size = self.generator.sample(self.sizes);
        // SAFETY: `self._test` guarantees `CmpctmallocTest` has initialized the heap and holds
        // `TEST_LOCK` for the lifetime of `self`.
        let p = unsafe { cmpct_alloc(size) };
        assert!(!p.is_null());
        assert_eq!(p.addr() % HEAP_DEFAULT_ALIGNMENT, 0);
        // SAFETY: `p` is a valid non-null allocation of at least `size` bytes.
        unsafe {
            ptr::write_bytes(p.cast::<u8>(), Self::ALLOC_FILL, size);
        }
        self.allocated.push(p);
    }

    fn allocate_aligned(&mut self) {
        let size = self.generator.sample(self.sizes);
        let exponent = self.generator.sample(self.alignment_exponents);
        let alignment = 1usize << exponent;
        // SAFETY: `alignment` is a power of two, and `self._test` guarantees `CmpctmallocTest` has
        // initialized the heap and holds `TEST_LOCK` for the lifetime of `self`.
        let p = unsafe { cmpct_memalign(alignment, size) };
        assert!(!p.is_null());
        assert_eq!(p.addr() % alignment, 0);
        // SAFETY: `p` is a valid non-null allocation of at least `size` bytes.
        unsafe {
            ptr::write_bytes(p.cast::<u8>(), Self::ALLOC_FILL, size);
        }
        self.allocated.push(p);
    }

    fn free(&mut self, order: FreeOrder) {
        match order {
            FreeOrder::Chronological => {}
            FreeOrder::ReverseChronological => {
                self.allocated.reverse();
            }
            FreeOrder::Random => {
                self.allocated.shuffle(&mut self.generator);
            }
        }
        for &p in &self.allocated {
            // SAFETY: `self._test` holds `TEST_LOCK`, and `p` was allocated from the active heap by
            // `cmpct_alloc` or `cmpct_memalign` and has not yet been freed.
            unsafe {
                cmpct_free(p);
            }
        }
        self.allocated.clear();
    }
}

impl Drop for RandomAllocator<'_> {
    fn drop(&mut self) {
        assert!(self.allocated.is_empty());
    }
}

fn heap_used_bytes() -> usize {
    cmpct_get_info().used_bytes
}

fn heap_free_bytes() -> usize {
    cmpct_get_info().free_bytes
}

fn heap_cached_bytes() -> usize {
    cmpct_get_info().cached_bytes
}

/// Serializes test execution because `cmpctmalloc` operates on a global `THE_HEAP` instance.
static TEST_LOCK: Mutex<()> = Mutex::new(());
static PAGE_MANAGER: Mutex<Option<PageManager>> = Mutex::new(None);

//
// Heap implementation.
//

/// Allocates `pages` contiguous pages for the heap from the active test's [`PageManager`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `pages > 0`.
pub unsafe fn heap_page_alloc(pages: usize) -> *mut c_void {
    let mut pm = PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
    pm.as_mut().unwrap().allocate_pages(pages)
}

/// Frees `pages` contiguous pages starting at `ptr` back to the active test's [`PageManager`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `ptr` is a page-aligned pointer previously
/// returned by [`heap_page_alloc`], and `pages > 0`.
pub unsafe fn heap_page_free(ptr: *mut c_void, pages: usize) {
    let mut pm = PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
    pm.as_mut().unwrap().free_pages(ptr, pages);
}

/// Reports a heap allocation failure of `size` bytes to the active test's [`PageManager`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held on the current thread.
pub unsafe fn heap_report_alloc_failure(_size: usize) {
    let mut pm = PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
    pm.as_mut().unwrap().inc_failures_reported();
}

struct CmpctmallocTest {
    _lock: MutexGuard<'static, ()>,
}

impl CmpctmallocTest {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner()) = Some(PageManager::default());
        // SAFETY: `TEST_LOCK` serializes all tests so no concurrent heap operations are in flight,
        // and `PAGE_MANAGER` is installed prior to `cmpct_init`.
        unsafe {
            cmpct_init();
        }
        Self { _lock: lock }
    }

    fn fail_next(&self, count: usize) {
        let mut pm = PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
        pm.as_mut().unwrap().fail_next(count);
    }

    fn get_failures_reported(&self) -> usize {
        let pm = PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner());
        pm.as_ref().unwrap().get_failures_reported()
    }
}

impl Drop for CmpctmallocTest {
    fn drop(&mut self) {
        *PAGE_MANAGER.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[test]
fn zero_alloc_is_null() {
    let test = CmpctmallocTest::new();
    // SAFETY: `test` initialized the heap.
    assert!(unsafe { cmpct_alloc(0) }.is_null());
    assert_eq!(0, test.get_failures_reported());
}

#[test]
fn null_can_be_freed() {
    let _test = CmpctmallocTest::new();
    // SAFETY: Passing null to `cmpct_free` is a safe no-op.
    unsafe { cmpct_free(ptr::null_mut()) };
}

#[test]
fn heap_is_properly_initialized() {
    let _test = CmpctmallocTest::new();
    // Assumes that we have called `cmpct_init`, which was done in the test case
    // set-up.

    // The heap should have space.
    assert!(heap_used_bytes() > 0);
    assert!(heap_free_bytes() > 0);
    // Nothing should have been cached at this point.
    assert_eq!(heap_cached_bytes(), 0);
}

#[test]
fn can_alloc_and_free() {
    let test = CmpctmallocTest::new();
    let orders = [FreeOrder::Chronological, FreeOrder::ReverseChronological, FreeOrder::Random];
    for order in orders {
        let mut ra = RandomAllocator::new(&test);
        // Allocate until we grow the heap a sufficient number of times.
        let mut times_grown = 0;
        while times_grown < HEAP_GROWTH_COUNT {
            let before = heap_used_bytes();
            ra.allocate();
            let after = heap_used_bytes();
            times_grown += usize::from(after > before);
        }
        ra.free(order);
    }
}

#[test]
fn can_memalign_and_free() {
    let test = CmpctmallocTest::new();
    let orders = [FreeOrder::Chronological, FreeOrder::ReverseChronological, FreeOrder::Random];
    for order in orders {
        let mut ra = RandomAllocator::new(&test);
        // Allocate until we grow the heap a sufficient number of times.
        let mut times_grown = 0;
        while times_grown < HEAP_GROWTH_COUNT {
            let before = heap_used_bytes();
            ra.allocate_aligned();
            let after = heap_used_bytes();
            times_grown += usize::from(after > before);
        }
        ra.free(order);
    }
}

#[test]
fn cookie_test() {
    let _test = CmpctmallocTest::new();
    // SAFETY: `p1`, `p2`, and `p3` are valid non-null allocations from the initialized heap.
    unsafe {
        let p1 = cmpct_alloc(64);
        let p2 = cmpct_alloc(64);
        cmpct_set_cookie(p1, 42);
        cmpct_set_cookie(p2, 43);
        assert_eq!(cmpct_get_cookie(p1), 42);
        assert_eq!(cmpct_get_cookie(p2), 43);
        cmpct_free(p2);
        let p3 = cmpct_alloc(64);
        cmpct_set_cookie(p3, 44);
        assert_eq!(cmpct_get_cookie(p1), 42);
        assert_eq!(cmpct_get_cookie(p3), 44);
    }
}

#[test]
fn sized_free() {
    let _test = CmpctmallocTest::new();
    const ALLOC_SIZE: usize = 1000;
    // SAFETY: `p` is allocated with `ALLOC_SIZE` from the initialized heap and freed with
    // `ALLOC_SIZE`.
    unsafe {
        let p = cmpct_alloc(ALLOC_SIZE);
        assert!(!p.is_null());
        cmpct_sized_free(p, ALLOC_SIZE);
    }
}

/// Regression test for https://fxbug.dev/386377108.
#[test]
fn sized_free_from_memalign() {
    let _test = CmpctmallocTest::new();
    const ALLOC_SIZE: usize = 1;
    const ALIGNMENT: usize = 128;
    // SAFETY: `p` is allocated with `ALIGNMENT` and `ALLOC_SIZE` from the initialized heap and
    // freed with `ALLOC_SIZE`.
    unsafe {
        let p = cmpct_memalign(ALIGNMENT, ALLOC_SIZE);
        assert!(!p.is_null());
        cmpct_sized_free(p, ALLOC_SIZE);
    }
}

#[test]
fn large_allocs_are_null() {
    let test = CmpctmallocTest::new();
    // SAFETY: `test` initialized the heap, and `p` is freed before testing
    // `HEAP_MAX_ALLOC_SIZE + 1`.
    unsafe {
        let mut p = cmpct_alloc(HEAP_MAX_ALLOC_SIZE);
        assert!(!p.is_null());
        cmpct_free(p);
        p = cmpct_alloc(HEAP_MAX_ALLOC_SIZE + 1);
        assert!(p.is_null());
    }
    assert_eq!(0, test.get_failures_reported());
}

#[test]
fn cached_allocation_is_efficiently_used() {
    let _test = CmpctmallocTest::new();
    const ALLOC_SIZE: usize = 1000;
    let mut allocations = Vec::new();
    let mut grown = false;
    while !grown {
        let before = heap_used_bytes();
        // SAFETY: `_test` initialized the heap.
        allocations.push(unsafe { cmpct_alloc(ALLOC_SIZE) });
        let after = heap_used_bytes();
        grown = after > before;
    }

    // As we alternatingly allocate and free across the threshold at which we
    // saw a request to the heap for more pages, we expect to only be using
    // our cached allocation.
    for _ in 0..1000 {
        let p = allocations.pop().unwrap();
        // SAFETY: `p` is a live allocation returned by `cmpct_alloc`.
        unsafe { cmpct_free(p) };
        assert!(heap_cached_bytes() > 0);

        // SAFETY: `_test` initialized the heap.
        allocations.push(unsafe { cmpct_alloc(ALLOC_SIZE) });
        assert_eq!(0, heap_cached_bytes());
    }

    // Ditto if we now free everything.
    while let Some(p) = allocations.pop() {
        // SAFETY: `p` is a live allocation returned by `cmpct_alloc`.
        unsafe { cmpct_free(p) };
    }
    assert!(heap_cached_bytes() > 0);
}

#[test]
fn retry_on_heap_alloc_bytes_ultimately_succeeds() {
    let test = CmpctmallocTest::new();
    // Allocate until we successfully grow the heap (despite needing to retry).
    test.fail_next(3);
    const ALLOC_SIZE: usize = 1000;
    let allocations = RefCell::new(Vec::new());
    let _cleanup = zr::defer(|| {
        while let Some(p) = allocations.borrow_mut().pop() {
            // SAFETY: `p` is a live allocation returned by `cmpct_alloc`.
            unsafe { cmpct_free(p) };
        }
    });

    let mut grown = false;
    while !grown {
        let before = heap_used_bytes();
        // SAFETY: `test` initialized the heap.
        allocations.borrow_mut().push(unsafe { cmpct_alloc(ALLOC_SIZE) });
        let after = heap_used_bytes();
        grown = after > before;
    }

    // See that the heap never reported failures.
    assert_eq!(0, test.get_failures_reported());
}

#[test]
fn retry_on_heap_alloc_bytes_ultimately_fails() {
    let test = CmpctmallocTest::new();
    test.fail_next(usize::MAX);
    const ALLOC_SIZE: usize = 1000;
    let allocations = RefCell::new(Vec::new());
    let _cleanup = zr::defer(|| {
        while let Some(p) = allocations.borrow_mut().pop() {
            // SAFETY: `p` is a live allocation returned by `cmpct_alloc`.
            unsafe { cmpct_free(p) };
        }
    });

    // SAFETY: `test` initialized the heap.
    while let Some(p) = NonNull::new(unsafe { cmpct_alloc(ALLOC_SIZE) }) {
        allocations.borrow_mut().push(p.as_ptr());
    }

    assert_eq!(1, test.get_failures_reported());
}
