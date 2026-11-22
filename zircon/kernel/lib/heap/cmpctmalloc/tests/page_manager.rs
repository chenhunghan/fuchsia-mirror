// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::ptr::{self, NonNull};
use std::alloc::{Layout, alloc, dealloc};
use std::collections::BTreeMap;

/// `PageManager` backs our fake heap implementation, managing blocks of
/// pages allocated by the OS.
///
/// Its implementation is complicated by `cmpct_trim()`, which allows for the
/// freeing of strict heads and tails of the blocks; otherwise, we could just
/// implement `heap_page_alloc()` and `heap_page_free()` as thin wrappers around
/// `alloc()` and `dealloc()`, respectively.
#[derive(Default)]
pub struct PageManager {
    /// Maps the left-most pointer in an OS-allocated block of pages to
    /// information about that block.
    blocks: BTreeMap<*mut u8, Block>,

    /// The number of [`Self::allocate_pages`] calls that should fail.
    num_calls_to_fail: usize,
    /// The number of observed failures as reported via [`Self::inc_failures_reported`].
    num_failures_reported: usize,
}

// SAFETY: `PageManager` uniquely owns all page-aligned buffers tracked in `blocks`, which may be
// transferred across threads under `PAGE_MANAGER`'s mutex.
unsafe impl Send for PageManager {}

impl PageManager {
    pub fn allocate_pages(&mut self, num_pages: usize) -> *mut c_void {
        if self.num_calls_to_fail > 0 {
            self.num_calls_to_fail -= 1;
            return ptr::null_mut();
        }

        let block = Block::new(num_pages);
        // SAFETY: `block.contents` is valid for `block.size_bytes` bytes.
        unsafe {
            assert!(Block::range_is_clean_filled(
                block.contents.as_ptr(),
                block.contents.as_ptr().add(block.size_bytes),
            ));
        }
        let ptr = block.contents.as_ptr();
        assert!(page::is_page_rounded(ptr.addr()));
        self.blocks.insert(ptr, block);
        ptr.cast::<c_void>()
    }

    pub fn free_pages(&mut self, p: *mut c_void, num_pages: usize) {
        if num_pages == 0 {
            return;
        }

        let ptr = p.cast::<u8>();
        assert!(page::is_page_rounded(ptr.addr()), "address {:p} is not page aligned", ptr);

        // The greatest key <= `ptr` points to the block in which `ptr` lives.
        let Some((&block_key, block)) = self.blocks.range_mut(..=ptr).next_back() else {
            panic!("could not find a block containing {:p}", ptr);
        };

        assert!(
            ptr >= block.available_start && ptr < block.available_end(),
            "address {:p} is outside available subregion of block",
            ptr
        );

        let size_to_free = num_pages * page::SIZE;
        let available_from_ptr = block.available_end().addr() - ptr.addr();
        assert!(
            size_to_free <= available_from_ptr,
            "cannot free {} bytes from address {:p}; only {} bytes available",
            size_to_free,
            ptr,
            available_from_ptr
        );

        let freeing_head = ptr == block.available_start;
        let freeing_tail = available_from_ptr == size_to_free;
        assert!(
            freeing_head || freeing_tail,
            "we only free heads or tails of available pages at any given time."
        );
        if freeing_head {
            block.free_head(num_pages);
        } else if freeing_tail {
            block.free_tail(num_pages);
        }
        // If we were to free both a head and a tail, then we freed the whole
        // available subregion, at which point all pages in the block have been
        // freed and we can stop tracking the block altogether.
        if freeing_head && freeing_tail {
            self.blocks.remove(&block_key);
        }
    }

    /// Tells the `PageManager` to fail the next `count` [`Self::allocate_pages`] calls.
    pub fn fail_next(&mut self, count: usize) {
        self.num_calls_to_fail = count;
    }

    /// Called by `cmpctmalloc` via `heap_report_alloc_failure`.
    pub fn inc_failures_reported(&mut self) {
        self.num_failures_reported += 1;
    }

    /// Returns the number of times [`Self::inc_failures_reported`] was called.
    pub fn get_failures_reported(&self) -> usize {
        self.num_failures_reported
    }
}

/// Represents an OS-allocated block of pages that tracks the contiguous
/// subset of pages of it still available for use.
///
/// Newly constructed objects or newly freed subregions within it are
/// expected to have their contents filled with [`Block::CLEAN_FILL`].
struct Block {
    /// The total size of the block, in bytes.
    // Safety invariant: `size_bytes > 0` is a non-zero multiple of `page::SIZE` with a valid
    // `Layout::from_size_align(size_bytes, page::SIZE)`.
    size_bytes: usize,
    /// The size of the available subregion, in bytes.
    // Safety invariant: `(available_start.addr() - contents.addr()) + available_bytes <=
    // size_bytes`.
    available_bytes: usize,
    /// The contents of the block.
    // Safety invariant: `contents` points to a live allocation of `size_bytes` initialized bytes
    // allocated by `std::alloc::alloc` with `Layout::from_size_align(size_bytes, page::SIZE)`.
    contents: NonNull<u8>,
    /// The start of the available region.
    // Safety invariant: `available_start` is derived from `contents` and lies within
    // `[contents, contents + size_bytes - available_bytes]`, with `[contents, available_start)`
    // and `[available_start + available_bytes, contents + size_bytes)` remaining initialized.
    available_start: *mut u8,
}

impl Block {
    /// See [`Block`] documentation.
    const CLEAN_FILL: u8 = 0x41;

    fn new(num_pages: usize) -> Self {
        assert!(num_pages > 0);
        let size_bytes = num_pages * page::SIZE;
        let layout = Layout::from_size_align(size_bytes, page::SIZE).unwrap();
        // SAFETY: `assert!(num_pages > 0)` above guarantees `size_bytes > 0`, so `layout` has
        // non-zero size.
        let raw = unsafe { alloc(layout) };
        let contents = NonNull::new(raw).unwrap();
        // SAFETY: `contents` is a valid allocation of `size_bytes` bytes.
        unsafe {
            ptr::write_bytes(contents.as_ptr(), Self::CLEAN_FILL, size_bytes);
        }
        Self {
            size_bytes,
            available_bytes: size_bytes,
            contents,
            available_start: contents.as_ptr(),
        }
    }

    fn available_end(&self) -> *mut u8 {
        // SAFETY: `self.available_start + self.available_bytes <= self.contents + self.size_bytes`.
        unsafe { self.available_start.add(self.available_bytes) }
    }

    /// Returns true if all bytes in `[begin, end)` equal [`Self::CLEAN_FILL`].
    ///
    /// # Safety
    ///
    /// Whenever `begin < end`, `begin` and `end` must be derived from the same allocated object
    /// and `[begin, end)` must be valid for reads of initialized `u8` bytes.
    unsafe fn range_is_clean_filled(begin: *const u8, end: *const u8) -> bool {
        let mut it = begin;
        while it < end {
            // SAFETY: Caller guarantees `[begin, end)` is valid for reads when `it < end`.
            if unsafe { *it } != Self::CLEAN_FILL {
                return false;
            }
            // SAFETY: `it < end`, so advancing by 1 stays within `[begin, end]`.
            it = unsafe { it.add(1) };
        }
        true
    }

    /// Frees the given number of pages from the head of the available subregion.
    fn free_head(&mut self, num_pages: usize) {
        let size = num_pages * page::SIZE;
        assert!(size > 0 && size <= self.available_bytes);
        // SAFETY: `self.available_start` points to `self.available_bytes >= size` bytes inside
        // `self.contents`.
        unsafe {
            ptr::write_bytes(self.available_start, Self::CLEAN_FILL, size);
            self.available_start = self.available_start.add(size);
        }
        self.available_bytes -= size;
    }

    /// Frees the given number of pages from the tail of the available subregion.
    fn free_tail(&mut self, num_pages: usize) {
        let size = num_pages * page::SIZE;
        assert!(size > 0 && size <= self.available_bytes);
        // SAFETY: `self.available_end()` points to the end of `self.available_bytes >= size` bytes
        // inside `self.contents`.
        unsafe {
            ptr::write_bytes(self.available_end().sub(size), Self::CLEAN_FILL, size);
        }
        self.available_bytes -= size;
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // A block might be destroyed with a non-trivial available region still
        // in use. We can only make guarantees that its complement has remained
        // unallocated since being freed.
        // SAFETY: `[self.contents, self.available_start)` and
        // `[self.available_end(), self.contents + self.size_bytes)` are within
        // the `self.size_bytes` allocation at `self.contents`.
        unsafe {
            assert!(Self::range_is_clean_filled(self.contents.as_ptr(), self.available_start));
            assert!(Self::range_is_clean_filled(
                self.available_end(),
                self.contents.as_ptr().add(self.size_bytes),
            ));
        }
        let layout = Layout::from_size_align(self.size_bytes, page::SIZE).unwrap();
        // SAFETY: `self.contents` was allocated with `alloc(layout)` in `Block::new`.
        unsafe {
            dealloc(self.contents.as_ptr(), layout);
        }
    }
}
