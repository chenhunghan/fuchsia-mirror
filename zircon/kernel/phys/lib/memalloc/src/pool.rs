// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Pool is a container of contiguous physical memory ranges that tracks physical memory and
//! allocates available regions of RAM among them.

use core::marker;
use core::mem::MaybeUninit;
use core::pin::Pin;
use pin_init::{PinInit, pin_data, pin_init_from_closure, pinned_drop};

use crate::range::*;

// Symbols defined in the C++ memalloc static library which wrap the C++ pool interface.
unsafe extern "C" {
    #[cfg(test)]
    fn cpp_new_test_pool(pool: *mut bindings::memalloc_Pool);
    fn cpp_new_pool(pool: *mut bindings::memalloc_Pool);
    fn cpp_destroy_pool(pool: *mut bindings::memalloc_Pool);
    fn cpp_new_range_iteration_context(
        ranges: *mut Range,
        size: usize,
        out: *mut bindings::memalloc_internal_RangeIterationContext,
    );
    fn cpp_pool_init(
        pool: *mut bindings::memalloc_Pool,
        state: *mut bindings::memalloc_internal_RangeIterationContext,
        state_count: usize,
        default_min_addr: u64,
        default_max_addr: u64,
    ) -> bool;
    fn cpp_pool_empty(pool: *const bindings::memalloc_Pool) -> bool;
    fn cpp_pool_size(pool: *const bindings::memalloc_Pool) -> usize;
    fn cpp_pool_begin(
        pool: *const bindings::memalloc_Pool,
        out: *mut bindings::memalloc_PoolIteratorState,
    );
    fn cpp_pool_iter_next(
        pool: *const bindings::memalloc_Pool,
        state: *mut bindings::memalloc_PoolIteratorState,
    ) -> *const Range;
    fn cpp_pool_allocate(
        pool: *mut bindings::memalloc_Pool,
        ty: Type,
        size: u64,
        alignment: u64,
        min_addr: *const u64,
        max_addr: *const u64,
        out_addr: *mut u64,
    ) -> bool;
    fn cpp_pool_free(pool: *mut bindings::memalloc_Pool, addr: u64, size: u64) -> bool;
    fn cpp_pool_resize(
        pool: *mut bindings::memalloc_Pool,
        original: *const Range,
        new_size: u64,
        min_alignment: u64,
        out_addr: *mut u64,
    ) -> bool;
    fn cpp_pool_find_containing_range(
        pool: *const bindings::memalloc_Pool,
        addr: u64,
    ) -> *const Range;
    fn cpp_pool_update_ram_subranges(
        pool: *mut bindings::memalloc_Pool,
        ty: Type,
        addr: u64,
        size: u64,
    ) -> bool;
    fn cpp_pool_truncate_total_ram(
        pool: *mut bindings::memalloc_Pool,
        new_capacity_bytes: u64,
    ) -> bool;
    fn cpp_pool_mark_as_peripheral(pool: *mut bindings::memalloc_Pool, range: *const Range)
    -> bool;
    fn cpp_pool_coalesce_peripherals(
        pool: *mut bindings::memalloc_Pool,
        alignments: *const usize,
        count: usize,
    ) -> bool;
}

/// The memory pool.
///
/// For now, the pool is directly managed by C++ logic, and so we wrap a bindgen binding `inner`
/// to the C++ object.  All methods on `Pool` from Rust will be passed `inner` so that it may be
/// forwarded back to C++.
///
/// `inner` is a self-referential struct on account of its use of doubly-linked lists.  Since
/// bindgen cant capture that invariant, we manually make Pool !Unpin by giving it the `_pin`
/// field.
#[pin_data(PinnedDrop)]
pub struct Pool {
    inner: bindings::memalloc_Pool,
    #[pin]
    _pin: marker::PhantomPinned,
}

impl Pool {
    pub const BOOKKEEPING_CHUNK_SIZE: u64 = bindings::memalloc_Pool_kBookkeepingChunkSize;
    pub const DEFAULT_MIN_ADDR: u64 = bindings::memalloc_Pool_kDefaultMinAddr;
    pub const DEFAULT_MAX_ADDR: u64 = bindings::memalloc_Pool_kDefaultMaxAddr;

    /// Construct an empty, uninitialized pool.
    pub fn uninit() -> impl PinInit<Self> {
        unsafe {
            pin_init_from_closure(|slot: *mut Self| {
                cpp_new_pool(&raw mut (*slot).inner);
                Ok(())
            })
        }
    }

    /// Initialize the pool given a memory map.
    ///
    /// The memory map is supplied in two pieces: first `zbi_ranges`, and second
    /// `preallocated_ranges`.  `zbi_ranges` is supplied by a ZBI or synthesized from a legacy
    /// booting protocol, while `preallocated_ranges` could consist of other auxiliary reserved
    /// areas known to be in use at the time of initialization (e.g., a phys program's own load
    /// image and the data ZBI itself).
    ///
    /// Reserved ranges merely exist for the convenience of bootloaders: they represent holes to be
    /// punched out of free RAM ranges in the initialization.  So, pool does not explicitly track
    /// reserved ranges.
    ///
    /// `default_min_addr` and `default_max_addr` set the default lower and upper address bounds on
    /// memory the pool may allocate, including where `init` places the pool's initial internal
    /// bookkeeping.  When `None`, they default to [`Self::DEFAULT_MIN_ADDR`] and
    /// [`Self::DEFAULT_MAX_ADDR`], respectively.  Tracked free RAM outside these bounds is still
    /// retained by the pool.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient free RAM within the default bounds to use for the
    /// pool's initial bookkeeping.
    ///
    /// # Panics
    ///
    /// Panics if:
    ///
    ///  * The lower bound, specified or implicit, is greater than the upper bound, specified or
    ///    implicit.
    ///  * Any range in `preallocated_ranges` does not have an allocated type.
    ///  * Any range in `preallocated_ranges` overlaps with another range in `preallocated_ranges`
    ///    of a different allocated type, or with a reserved or peripheral range in `zbi_ranges`.
    ///
    pub fn init(
        self: Pin<&mut Self>,
        zbi_ranges: &mut [zbi::MemRange],
        preallocated_ranges: &mut [Range],
        default_min_addr: Option<u64>,
        default_max_addr: Option<u64>,
    ) -> Result<(), PoolError> {
        let min_addr = default_min_addr.unwrap_or(Self::DEFAULT_MIN_ADDR);
        let max_addr = default_max_addr.unwrap_or(Self::DEFAULT_MAX_ADDR);
        assert!(min_addr <= max_addr);
        assert!(preallocated_ranges.iter().all(|r| r.ty().is_allocated()));

        let ranges = [Range::slice_from_zbi_mem_ranges(zbi_ranges), preallocated_ranges];
        let mut state = ranges.map(|slice| {
            let mut ctx =
                MaybeUninit::<bindings::memalloc_internal_RangeIterationContext>::uninit();
            unsafe {
                cpp_new_range_iteration_context(slice.as_mut_ptr(), slice.len(), ctx.as_mut_ptr());
                ctx.assume_init()
            }
        });
        let ok = unsafe {
            cpp_pool_init(
                &mut self.get_unchecked_mut().inner,
                state.as_mut_ptr(),
                state.len(),
                min_addr,
                max_addr,
            )
        };
        if ok { Ok(()) } else { Err(PoolError) }
    }

    /// Yields true if there are no ranges tracked by the pool.
    pub fn is_empty(self: Pin<&Self>) -> bool {
        unsafe { cpp_pool_empty(&self.inner) }
    }

    /// The number of ranges tracked by the pool.
    pub fn len(self: Pin<&Self>) -> usize {
        unsafe { cpp_pool_size(&self.inner) }
    }

    /// Borrow the pool as an iterator to enable searching through the pool's ranges.
    pub fn iter(self: Pin<&Self>) -> PoolIterator<'_> {
        PoolIterator::new(self)
    }

    /// Attempt to allocate physical memory.
    ///
    /// The caller supplies the type, size, and alignment of the range of physical memory they wish
    /// to allocate.  They may optionally supply maximum and minimum bounds beyond which no
    /// allocation should be made.  These supplied bounds override the default bounds of the pool,
    /// otherwise the default bounds are assumed.  Any returned address is garunteed to be nonzero.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient free RAM to to track any new ranges, or there is no
    /// free RAM that meets the given constraints.
    ///
    /// # Panics
    ///
    /// Panics if `ty` is not an allocated type, `size == 0`, `alignment` is not a power of two, or
    /// if both `min` and `max` are supplied where `min > max`.
    pub fn allocate(
        self: Pin<&mut Self>,
        ty: Type,
        size: u64,
        alignment: u64,
        min_addr: Option<u64>,
        max_addr: Option<u64>,
    ) -> Result<u64, PoolError> {
        assert!(ty.is_allocated());
        assert!(size > 0);
        assert!(alignment.is_power_of_two());
        if let (Some(min), Some(max)) = (min_addr, max_addr) {
            assert!(min <= max);
        }
        let min_ptr = min_addr.as_ref().map_or(core::ptr::null(), core::ptr::from_ref);
        let max_ptr = max_addr.as_ref().map_or(core::ptr::null(), core::ptr::from_ref);
        let mut addr = 0;
        let ok = unsafe {
            cpp_pool_allocate(
                &mut self.get_unchecked_mut().inner,
                ty,
                size,
                alignment,
                min_ptr,
                max_ptr,
                &mut addr,
            )
        };
        if ok { Ok(addr) } else { Err(PoolError) }
    }

    /// Attempt to free a subrange of a previously allocated range.
    ///
    /// The subrange, if found, is updated to have type free RAM.  Freeing a range already tracked
    /// as free RAM is a no-op.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient memory to track the new (subdivided) ranges of memory
    /// that would result from freeing.
    ///
    /// # Panics
    ///
    /// Panics if `addr + size > u64::MAX`.
    pub fn free(self: Pin<&mut Self>, addr: u64, size: u64) -> Result<(), PoolError> {
        assert!(addr.checked_add(size).is_some());
        let ok = unsafe { cpp_pool_free(&mut self.get_unchecked_mut().inner, addr, size) };
        if ok { Ok(()) } else { Err(PoolError) }
    }

    /// Attempt to resize an existing allocated range of memory.
    ///
    /// The resizing occurs only at the level of bookkeeping; the start of the resized region is
    /// returned and, if it differs from the one originally supplied, it is the responsibility of
    /// the caller to move the old contents into the new region before another allocation is
    /// performed.  If the new size is smaller than the original, then this method effectively just
    /// frees the range's tail.
    ///
    /// The original range - as opposed to just its root address - is required to be passed as
    /// testimony; since the after original allocation or initialization, the pool may have
    /// coalesced neighbouring regions into it or freed subregions of it, this method asserts that
    /// the associated tracked range does at least still have `original` as a subrange with the
    /// appropriate type.
    ///
    /// No failure mode will leave the original range free'd.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient memory to track the new (subdivided) ranges of memory
    /// that would result from resizing, or `original` is not tracked in the pool.
    ///
    /// # Panics
    ///
    /// Panics if `new_size == 0`, `original.ty()` is not an allocated type, `min_alignment` is not
    /// a power of two, or if `original.addr()` is not a multiple of `min_alignment`.
    pub fn resize(
        self: Pin<&mut Self>,
        original: &Range,
        new_size: u64,
        min_alignment: u64,
    ) -> Result<u64, PoolError> {
        assert!(new_size > 0);
        assert!(original.ty().is_allocated());
        assert!(min_alignment.is_power_of_two());
        assert!(original.addr().is_multiple_of(min_alignment));
        let mut addr = 0;
        let ok = unsafe {
            cpp_pool_resize(
                &mut self.get_unchecked_mut().inner,
                original,
                new_size,
                min_alignment,
                &mut addr,
            )
        };
        if ok { Ok(addr) } else { Err(PoolError) }
    }

    /// Yield the range in the pool containing `addr`, or None if there is no such range.
    pub fn find_containing_range(self: Pin<&Self>, addr: u64) -> Option<Range> {
        let ptr = unsafe { cpp_pool_find_containing_range(&self.inner, addr) };
        if ptr.is_null() { None } else { Some(unsafe { *ptr }) }
    }

    /// Destructively reallocate all RAM types within the given range to `ty`.
    ///
    /// The utility of this method lies in situations where there is a special range that we
    /// ultimately want to reserve for another booting context that we are handing pool bookkeeping
    /// off to.  For example when loading a fixed-address kernel image, we would want to prevent
    /// page tables-- which persist across the boot-- from being allocated out of that load range.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient bookkeeping to track any new ranges of memory.
    ///
    /// # Panics
    ///
    /// Panics if `addr + size > u64::MAX` or if `ty` is not an allocated type.
    pub fn update_ram_subranges(
        self: Pin<&mut Self>,
        ty: Type,
        addr: u64,
        size: u64,
    ) -> Result<(), PoolError> {
        assert!(ty.is_allocated());
        assert!(addr.checked_add(size).is_some());
        let ok = unsafe {
            cpp_pool_update_ram_subranges(&mut self.get_unchecked_mut().inner, ty, addr, size)
        };
        if ok { Ok(()) } else { Err(PoolError) }
    }

    /// Truncate all of RAM (free or allocated).
    ///
    /// This operation will reallocate all RAM beyond the `new_capacity_bytes` as a range of the
    /// truncated type.  This is used to simulate physical memory constraints of smaller devices.
    /// Any allocations (including pool bookkeeping) past the cut-off aredestructively reallocated
    /// with [`update_ram_subranges`].
    ///
    /// Nvram is excluded from this truncation, as such ranges are special and are of diagnostic
    /// value even when simulating physical memory constraints.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There is insufficient bookkeeping to track any new range of memory that
    /// would result from truncating a range.
    pub fn truncate_total_ram(
        self: Pin<&mut Self>,
        new_capacity_bytes: u64,
    ) -> Result<(), PoolError> {
        let ok = unsafe {
            cpp_pool_truncate_total_ram(&mut self.get_unchecked_mut().inner, new_capacity_bytes)
        };
        if ok { Ok(()) } else { Err(PoolError) }
    }

    /// Mark the provided range as peripheral, if it exists within the pool.
    ///
    /// # Errors
    ///
    /// [`PoolError`] - There are ranges in the pool overlapping with `range`.
    pub fn mark_as_peripheral(self: Pin<&mut Self>, range: &Range) -> Result<(), PoolError> {
        let ok = unsafe { cpp_pool_mark_as_peripheral(&mut self.get_unchecked_mut().inner, range) };
        if ok { Ok(()) } else { Err(PoolError) }
    }

    /// Make one peripheral range for every run of peripheral ranges in the pool.
    ///
    /// A "run of ranges" in this context means that there are ranges which are consecutive, but
    /// separated by a hole in memory (otherwise it would violate the normalized property of the
    /// pool).
    ///
    /// The created peripheral range, if successful, will be aligned to one of the provided
    /// alignments.  Each of the provided alignments is tried, and the first one not to generate
    /// invalid ranges, such as overlapping with free RAM ranges, is selected.
    ///
    /// `alignments` is a collection of exponents of powers of two which are used to get the actual
    /// alignment.  I.e., the mask for the alignment corresponding to the element at index n of
    /// `alignments` is `( << alignments[n]) - 1).
    ///
    /// # Panics
    ///
    /// Panics if any element in `alignments` is greater than or equal to 64.
    ///
    pub fn coalesce_peripherals(self: Pin<&mut Self>, alignments: &[usize]) {
        assert!(alignments.iter().all(|&a| a < 64));
        let ok = unsafe {
            cpp_pool_coalesce_peripherals(
                &mut self.get_unchecked_mut().inner,
                alignments.as_ptr(),
                alignments.len(),
            )
        };
        assert!(ok);
    }
}

#[pinned_drop]
impl PinnedDrop for Pool {
    fn drop(self: Pin<&mut Self>) {
        unsafe { cpp_destroy_pool(&mut self.get_unchecked_mut().inner) };
    }
}

// TODO(https://fxbug.dev/570652165): After the port of Pool into Rust, revisit the need to
// introduce new PoolError variants to capture all the failure modes of every pool operation--
// rather than having just one generic failure signal.
/// Error returned when an operation on a [`Pool`] fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolError;

/// The pool iterator.
///
/// All ranges tracked by the pool are normalized with respect to each other-- which means there
/// are no overlapping ranges, they are lexicographically sorted, and they are maximally
/// contiguous.
pub struct PoolIterator<'a> {
    pool: Pin<&'a Pool>,
    state: bindings::memalloc_PoolIteratorState,
}

impl<'a> PoolIterator<'a> {
    fn new(pool: Pin<&'a Pool>) -> Self {
        let mut state = MaybeUninit::<bindings::memalloc_PoolIteratorState>::uninit();
        let state = unsafe {
            cpp_pool_begin(&pool.inner, state.as_mut_ptr());
            state.assume_init()
        };
        Self { pool, state }
    }
}

impl Iterator for PoolIterator<'_> {
    type Item = Range;

    fn next(&mut self) -> Option<Range> {
        let ptr = unsafe { cpp_pool_iter_next(&self.pool.inner, &mut self.state) };
        if ptr.is_null() {
            None
        } else {
            // Safety: The Rust Range type is ABI compatible with the C++ Range type.
            Some(unsafe { *ptr })
        }
    }
}

// Safety: `Pool` maintains its ranges in normalized order at all times.
unsafe impl NormalizedRangeIterator for PoolIterator<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use pin_init::stack_pin_init;

    // Range type abbreviations used in test diagrams:
    //
    //   FR  - FreeRam
    //   P   - Peripheral
    //   R   - Reserved
    //   PB  - PoolBookkeeping
    //   PTP - PoolTestPayload
    //   DZ  - DataZbi
    //   TR  - TruncatedRam
    //   N   - Nvram

    const DEFAULT_MIN_ADDR: u64 = 0;

    // A constructor for a zbi memory range.
    const fn new_zbi_range(paddr: u64, length: u64, ty: zbi::MemType) -> zbi::MemRange {
        zbi::MemRange { paddr, length, r#type: ty, reserved: 0 }
    }

    // A wrapper around the `cpp_new_test_pool` binding.
    fn uninit_test_pool() -> impl PinInit<Pool> {
        unsafe {
            pin_init_from_closure(|slot: *mut Pool| {
                cpp_new_test_pool(&raw mut (*slot).inner);
                Ok(())
            })
        }
    }

    // A newly constructed pool is empty.
    #[test]
    fn uninit_pool_is_empty() {
        stack_pin_init!(let pool = Pool::uninit());
        assert!(pool.as_ref().is_empty());
        assert_eq!(pool.as_ref().len(), 0);
        assert_eq!(pool.as_ref().iter().next(), None);
    }

    // Initializing a pool with an empty memory map results in an error, and an empty pool.
    #[test]
    fn no_input_memory() {
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_err());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), None);
    }

    // Initializing a pool with no free RAM results in an error and an empty pool.
    //
    // Input:  [ R | P ]
    // Output: Err
    #[test]
    fn no_ram() {
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0, 0x1000, zbi::MemType::RESERVED),
            new_zbi_range(0x1000, 0x1000, zbi::MemType::PERIPHERAL),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_err());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), None);
    }

    // Initializing a pool where the input memory map doesn't have enough free RAM for its own
    // bookkeeping results in an error and an empty pool.
    #[test]
    fn too_little_ram() {
        //         |------|------|
        // Input:  [ FR  ]
        // Output: Err
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0xfff, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_err());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), None);
        }

        //         |---------------|---------------|
        // Input:  [       R       |   FR  ]
        //                 [ R |FR]
        //
        // Output: Err
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RESERVED),
                new_zbi_range(0x1000, 0x800, zbi::MemType::RAM),
                new_zbi_range(0x800, 0x400, zbi::MemType::RESERVED),
                new_zbi_range(0xc00, 0x200, zbi::MemType::RAM),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_err());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), None);
        }
    }

    // Bookkeeping placement tests.
    #[test]
    fn bookkeeping() {
        // Bookkeeping placement for one page of free RAM.
        //
        //         |-----|
        // Input:  [ FR  ]
        // Output: [ PB  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x1000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), None);
        }

        // Bookkeeping prefers the first pages available in the selected free RAM it is placed.
        //
        //         |-----|-----|
        // Input:  [    FR     ]
        // Output: [ PB  | FR  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        // Bookkeeping placement can only occur within free RAM pages.
        //
        //         |-----|-----|
        // Input:  [  P  | FR  ]
        // Output: [  P  | PB  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::PERIPHERAL),
                new_zbi_range(0x1000, 0x1000, zbi::MemType::RAM),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::Peripheral)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), None);
        }

        // Bookkeeping is page aligned.
        //
        //         |-------|-------|
        // Input:      [     FR    ]
        // Output:     [FR |  PB   ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x800, 0x1800, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x800, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), None);
        }

        // Bookkeeping placement is resolved after the input memory ranges have been normalized.
        //
        //         |-------|-------|-------|
        // Input:  [      FR       |  FR   ]
        //             [   P   ]
        //
        // Output: [FR |   P   |FR |  PB   ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x2000, zbi::MemType::RAM),
                new_zbi_range(0x800, 0x1000, zbi::MemType::PERIPHERAL),
                new_zbi_range(0x2000, 0x1000, zbi::MemType::RAM),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x800, 0x1000, Type::Peripheral)));
            assert_eq!(iter.next(), Some(Range::new(0x1800, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), None);
        }
    }

    // Reserved ranges in the input beget implicit holes in the ranges stored by the pool.
    #[test]
    fn reserved_ranges_are_not_explicitly_tracked() {
        //         |-----|-----|-----|
        // Input:  [  R  ]
        //         [       FR        ]
        //
        // Output:       [ PB  | FR  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RESERVED),
                new_zbi_range(0, 0x3000, zbi::MemType::RAM),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        //         |-----|-----|-----|
        // Input:  [       FR        ]
        //               [  R  ]
        //
        // Output: [ PB  ]     [ FR  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x3000, zbi::MemType::RAM),
                new_zbi_range(0x1000, 0x1000, zbi::MemType::RESERVED),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        //         |-----|-----|-----|
        // Input:  [       FR        ]
        //                     [  R  ]
        //
        // Output: [ PB  | FR  ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x3000, zbi::MemType::RAM),
                new_zbi_range(0x2000, 0x1000, zbi::MemType::RESERVED),
            ];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }
    }

    // Default allocation bounds restrict bookkeeping placement and default `allocate` calls,
    // without discarding tracked RAM outside those bounds.
    #[test]
    fn default_allocation_bounds() {
        // A minimum address at the end of free RAM leaves no room inside the bounds for
        // bookkeeping.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x64000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(0x64000), None).is_err());
        }

        // A maximum address that leaves less than a page of free RAM within the bounds leaves no
        // room for bookkeeping.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x64000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(0), Some(0x800)).is_err());
        }

        // 100 pages total.
        //              |--|--|--|--|--|--|--|--|--|--|--|--|--| ... |--|--| ... |--|--|
        //                                            <-------------------->
        // Memory Map:  [                              FR                              ]
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x64000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(0xa000), Some(0x32000)).is_ok());

            // Bookkeeping is placed at the first free RAM page within the default bounds.
            //
            // Pool:    [             FR              |PB|              FR             ]
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0xa000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0xa000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0xb000, 0x59000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            // 89 pages are free after bookkeeping, but only 39 pages lie below the default upper
            // bound so allocating 40 pages fails.
            assert!(
                pool.as_mut().allocate(Type::PoolTestPayload, 0x28000, 0x10, None, None).is_err()
            );
            // Once the upper bound is overridden, then its okay.
            assert!(
                pool.as_mut()
                    .allocate(Type::PoolTestPayload, 0x59000, 0x10, None, Some(0x64000))
                    .is_ok()
            );

            // 10 pages are free before bookkeeping, but they lie below the default lower bound,
            // so allocating them fails.
            assert!(
                pool.as_mut().allocate(Type::PoolTestPayload, 0xa000, 0x10, None, None).is_err()
            );
            // Once the lower bounds is overridden, then its okay.
            assert!(
                pool.as_mut().allocate(Type::PoolTestPayload, 0xa000, 0x10, Some(0), None).is_ok()
            );
        }
    }

    // Exercise all the failure modes of a request to allocate memory.
    #[test]
    fn no_resources_allocation() {
        //  |-----|-----|-----|
        //        [    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //        [ PB  | FR  ]
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        // Fail to allocate two pages since there is just one page of free RAM.
        assert!(pool.as_mut().allocate(Type::PoolTestPayload, 0x2000, 0x10, None, None).is_err());
        // Fail to allocate one page at a four-page alignment since the only free page is at a
        // two-page alignment.
        assert!(pool.as_mut().allocate(Type::PoolTestPayload, 0x1000, 0x4000, None, None).is_err());
        //               <----------
        //        [ PB  | FR  ]
        assert!(
            pool.as_mut()
                .allocate(Type::PoolTestPayload, 0x1000, 0x10, Some(0x2001), None)
                .is_err()
        );
        //                    <-----
        //        [ PB  | FR  ]
        assert!(
            pool.as_mut()
                .allocate(Type::PoolTestPayload, 0x1000, 0x10, Some(0x3000), None)
                .is_err()
        );
        //  ---------------->
        //        [ PB  | FR  ]
        assert!(
            pool.as_mut()
                .allocate(Type::PoolTestPayload, 0x1000, 0x10, None, Some(0x2ffe))
                .is_err()
        );
        //  ------------>
        //        [ PB  | FR  ]
        assert!(
            pool.as_mut()
                .allocate(Type::PoolTestPayload, 0x1000, 0x10, None, Some(0x2000))
                .is_err()
        );

        // Sanity check that the state of the pool remains unchanged after all the failed
        // allocation attempts.
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Edge case when both bounds are equal, both in the initialization of the pool, and in making
    // allocations.
    #[test]
    fn allocation_with_equal_bounds() {
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(0x1000), Some(0x1000)).is_err());
        }

        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            assert!(
                pool.as_mut()
                    .allocate(Type::PoolTestPayload, 2, 0x10, Some(0x2000), Some(0x2000))
                    .is_err()
            );
            // You can allocate one byte when the bounds are equal (and contained in free RAM).
            assert!(
                pool.as_mut()
                    .allocate(Type::PoolTestPayload, 1, 0x10, Some(0x2000), Some(0x2000))
                    .is_ok()
            );
        }
    }

    // Allocate the same amount of total space, but with varying amounts of calls to allocate.
    //
    // Each test has the following initial conditions before allocation:
    //
    //              |-----|-----|-----|
    // Memory map:        [    FR     ]
    // Pool:              [ PB  | FR  ]
    //
    // And then after allocating, in all four tests, we expect the pool to look like:
    //
    //                    [ PB  | PTP ]
    #[test]
    fn exhaustive_allocation() {
        // Allocate once, exactly one page.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            assert!(
                pool.as_mut().allocate(Type::PoolTestPayload, 0x1000, 0x10, None, None).is_ok()
            );

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        // Allocate two halves of a page.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            for _ in 0..2 {
                assert!(
                    pool.as_mut().allocate(Type::PoolTestPayload, 0x800, 0x10, None, None).is_ok()
                );
            }

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        // Allocate four quarters of a page.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            for _ in 0..4 {
                assert!(
                    pool.as_mut().allocate(Type::PoolTestPayload, 0x400, 0x10, None, None).is_ok()
                );
            }

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        // Allocate one byte 4096 times.
        {
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0x1000, 0x2000, zbi::MemType::RAM)];
            assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);

            for _ in 0..0x1000 {
                assert!(pool.as_mut().allocate(Type::PoolTestPayload, 1, 1, None, None).is_ok());
            }

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }
    }

    // Exercise basic free behavior.
    #[test]
    fn freeing() {
        //  |-------|-------|-------|
        //  [      FR       |  DZ   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
        let preallocated_ranges = &mut [Range::new(0x2000, 0x1000, Type::DataZbi)];
        assert!(
            pool.as_mut()
                .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                .is_ok()
        );

        //  [  PB   |  FR   |  DZ   ]
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::DataZbi)));
        assert_eq!(iter.next(), None);

        //                  <--->
        //  [  PB   |  FR   |  DZ   ]
        assert!(pool.as_mut().free(0x2000, 0x800).is_ok());
        //                      <--->
        //  [  PB   |    FR     |DZ ]
        assert!(pool.as_mut().free(0x2800, 0x800).is_ok());

        //  [  PB   |      FR       ]
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        //          <------->
        //  [  PB   |      FR       ]
        assert!(pool.as_mut().free(0x1000, 0x1000).is_ok());

        // No change.
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        //          <--->
        //  [  PB   |      FR       ]
        assert!(pool.as_mut().free(0x1000, 0x800).is_ok());

        // No change.
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        //              <--->
        //  [  PB   |      FR       ]
        assert!(pool.as_mut().free(0x1800, 0x800).is_ok());

        // No change.
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        //                  <------->
        //  [  PB   |      FR       ]
        assert!(pool.as_mut().free(0x2000, 0x1000).is_ok());

        // No change.
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Make allocations of various sizes and immediately free them before making the next
    // allocation.
    #[test]
    fn freed_allocations() {
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        for size in [1, 2, 4, 8, 16, 0x800, 0x1000] {
            let addr = pool.as_mut().allocate(Type::PoolTestPayload, size, 1, None, None).unwrap();
            assert!(pool.as_mut().free(addr, size).is_ok());

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }
    }

    // Basic resizing behavior.
    #[test]
    fn resizing() {
        {
            //              |-----|-----|
            // Original:          <----->
            // Resized:           <----->
            // Pool:        [ PB  | PTP ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x1000, zbi::MemType::RAM)];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Resizing the payload to its current size is a no-op.
            assert_eq!(pool.as_mut().resize(&original, 0x1000, 0x1000).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);

            // Resizing the payload to its current size with a `min_alignment` that is a divisor of
            // its current alignment is also a no-op.
            assert_eq!(pool.as_mut().resize(&original, 0x1000, 0x800).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-------|-------|
            // Pool:        [  PB   |  PTP  ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x1000, zbi::MemType::RAM)];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Successful resize of payload to half of its former size.
            //
            // Original:            <------->
            // Resized:             <--->
            // Before:      [  PB   |  PTP  ]
            // After:       [  PB   |PTP|FR ]
            assert_eq!(pool.as_mut().resize(&original, 0x800, 0x1000).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x800, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x1800, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-----|-----|-----|
            // Pool:        [ PB  | PTP | FR  ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0x2000, 0x1000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Successful resize of payload to twice its former size.
            //
            // Original:          <----->
            // Resized:           <----------->
            // Before:      [ PB  | PTP | FR  ]
            // After:       [ PB  |    PTP    ]
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-----|-----|-----|-----|
            // Pool:        [ PB  |    PTP    | FR  ]
            let original = Range::new(0x2000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0x3000, 0x1000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [Range::new(0x1000, 0x2000, Type::PoolTestPayload)];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // An attempt to resize a subrange of an existing range into existing free RAM will
            // succeed and grow the whole range, but only the address of the beginning of the
            // subrange is returned.
            //
            // Original:                <----->
            // Resized:                 <----------->
            // Before:      [ PB  |    PTP    | FR  ]
            // After:       [ PB  |       PTP       ]
            //                          ^
            //                           Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0x2000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x3000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |----|----|----|----|----|----|----|----|----|----|----|----|
            // Pool:        [ PB |PTP ]                                       [   FR    ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // If a range must move locations in order to be resized, then it is.
            //
            // Original:         <---->
            // Resized:          <--------->
            // Before:      [ PB |PTP ]                                       [   FR    ]
            // After:       [ PB | FR ]                                       [   PTP   ]
            //                                                                ^
            //                                                                 Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0xa000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |----|----|----|----|----|----|----|----|----|----|----|----|
            // Pool:        [ PB |   PTP   ]                                  [   FR    ]
            let original = Range::new(0x2000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [Range::new(0x1000, 0x2000, Type::PoolTestPayload)];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // If a SUB-range must be moved in order to be resized, then it is moved, and free RAM
            // takes the place of the subrange.
            //
            // Original:              <---->
            // Resized:               <--------->
            // Before:      [ PB |   PTP   ]                                  [   FR    ]
            // After:       [ PB |PTP | FR ]                                  [   PTP   ]
            //                                                                ^
            //                                                                 Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0xa000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |----|----|----|----|----|----|----|----|----|----|----|----|
            // Pool:        [ PB |   PTP   ]                                  [   FR    ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [Range::new(0x1000, 0x2000, Type::PoolTestPayload)];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Even if the subrange provided as `original` starts at the same address as the super
            // range, it is treated as a different range if they have different sizes.
            //
            // Original:         <---->
            // Resized:          <--------->
            // Before:      [ PB |   PTP   ]                                  [   FR    ]
            // After:       [ PB | FR |PTP ]                                  [   PTP   ]
            //                                                                ^
            //                                                                 Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0xa000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |----|----|----|----|----|----|----|----|----|----|----|----|
            // Pool:        [ PB |     PTP      ]                             [   FR    ]
            let original = Range::new(0x2000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [Range::new(0x1000, 0x3000, Type::PoolTestPayload)];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // The subrange provided as `original` is removed from the original range when it is
            // moved to a new location and resized-- replaced by free RAM.
            //
            // Original:              <---->
            // Resized:               <--------->
            // Before:      [ PB |     PTP      ]                             [   FR    ]
            // After:       [ PB |PTP | FR |PTP ]                             [   PTP   ]
            //                                                                ^
            //                                                                 Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x2000, 0x1000).ok(), Some(0xa000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-------|-------|-------|
            // Pool:        [  PB   |  FR   |  PTP  ]
            let original = Range::new(0x2000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Ranges will move backwards if that's where there is spare space.
            //
            // Original:                    <------->
            // Resized:                     <----------->
            // Before:      [  PB   |  FR   |  PTP  ]
            // After:       [  PB   |    PTP    |FR ]
            //                      ^
            //                       Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x1800, 0x1000).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1800, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x2800, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-------|-------|-------|-------|
            // Pool:        [  PB   |  FR   |      PTP      ]
            let original = Range::new(0x2000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
            let preallocated_ranges = &mut [Range::new(0x2000, 0x2000, Type::PoolTestPayload)];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // A subrange at the start of a larger coalesced range can also move backwards into
            // preceding free RAM, freeing any unused tail while leaving the rest of the original
            // range intact.
            //
            // Original:                    <------->
            // Resized:                     <----------->
            // Before:      [  PB   |  FR   |      PTP      ]
            // After:       [  PB   |    PTP    |FR |  PTP  ]
            //                      ^
            //                       Returned address
            assert_eq!(pool.as_mut().resize(&original, 0x1800, 0x1000).ok(), Some(0x1000));
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1800, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x2800, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        {
            //              |-------|-------|-------|
            // Pool:        [  PB   |  PTP  |FR ]
            let original = Range::new(0x1000, 0x1000, Type::PoolTestPayload);
            stack_pin_init!(let pool = uninit_test_pool());
            let zbi_ranges = &mut [
                new_zbi_range(0, 0x1000, zbi::MemType::RAM),
                new_zbi_range(0x2000, 0x800, zbi::MemType::RAM),
            ];
            let preallocated_ranges = &mut [original];
            assert!(
                pool.as_mut()
                    .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                    .is_ok()
            );

            // Fail to resize when there is not enough contiguous free RAM to meet the
            // constraints.
            //
            // Original:            <------->
            // Resized:             <--------------->
            // Before:      [  PB   |  PTP  |FR ]
            // After:       Err
            assert!(pool.as_mut().resize(&original, 0x2000, 0x1000).is_err());
        }
    }

    // Basic tests for find containing range.
    #[test]
    fn find_containing_range() {
        //         |-----|-----|-----|
        //         [       FR        ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0, 0x3000, zbi::MemType::RAM)];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //         [ PB  |    FR     ]
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        //         v
        // Input:  [ PB  |    FR     ]
        // Output: [ PB  ]
        assert_eq!(
            pool.as_ref().find_containing_range(DEFAULT_MIN_ADDR),
            Some(Range::new(0, 0x1000, Type::PoolBookkeeping))
        );

        //              v
        // Input:  [ PB  |    FR     ]
        // Output: [ PB  ]
        assert_eq!(
            pool.as_ref().find_containing_range(0xfff),
            Some(Range::new(0, 0x1000, Type::PoolBookkeeping))
        );

        //               v
        // Input:  [ PB  |    FR     ]
        // Output:       [    FR     ]
        assert_eq!(
            pool.as_ref().find_containing_range(0x1000),
            Some(Range::new(0x1000, 0x2000, Type::FreeRam))
        );

        //                     v
        // Input:  [ PB  |    FR     ]
        // Output:       [    FR     ]
        assert_eq!(
            pool.as_ref().find_containing_range(0x2000),
            Some(Range::new(0x1000, 0x2000, Type::FreeRam))
        );

        //                          v
        // Input:  [ PB  |    FR     ]
        // Output:       [    FR     ]
        assert_eq!(
            pool.as_ref().find_containing_range(0x2fff),
            Some(Range::new(0x1000, 0x2000, Type::FreeRam))
        );

        //                           v
        // Input:  [ PB  |    FR     ]
        // Output: None
        assert_eq!(pool.as_ref().find_containing_range(0x3000), None);
    }

    // Basic behavior of `update_ram_subranges()`.
    #[test]
    fn ram_subrange_updates() {
        //              |-------|-------|-------|-------|-------|-------|-------|
        // Memory map:  [          FR           |  DZ   |  FR   ]       [  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0, 0x3000, zbi::MemType::RAM),
            new_zbi_range(0x4000, 0x1000, zbi::MemType::RAM),
            new_zbi_range(0x6000, 0x1000, zbi::MemType::RAM),
        ];
        let preallocated_ranges = &mut [Range::new(0x3000, 0x1000, Type::DataZbi)];
        assert!(
            pool.as_mut()
                .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                .is_ok()
        );

        // Pool:        [  PB   |      FR       |  DZ   |  FR   ]       [  FR   ]
        {
            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x2000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::DataZbi)));
            assert_eq!(iter.next(), Some(Range::new(0x4000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x6000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        //                          <--------------->
        // Before:      [  PB   |      FR       |  DZ   |  FR   ]       [  FR   ]
        // After:       [  PB   |FR |      PTP      |DZ |  FR   ]       [  FR   ]
        {
            assert!(
                pool.as_mut().update_ram_subranges(Type::PoolTestPayload, 0x1800, 0x2000).is_ok()
            );

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x800, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x1800, 0x2000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x3800, 0x800, Type::DataZbi)));
            assert_eq!(iter.next(), Some(Range::new(0x4000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x6000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        //                      <----------------------->
        // Before:      [  PB   |FR |      PTP      |DZ |  FR   ]       [  FR   ]
        // After:       [  PB   |          PTP          |  FR   ]       [  FR   ]
        {
            assert!(
                pool.as_mut().update_ram_subranges(Type::PoolTestPayload, 0x1000, 0x3000).is_ok()
            );

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x3000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x4000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), Some(Range::new(0x6000, 0x1000, Type::FreeRam)));
            assert_eq!(iter.next(), None);
        }

        //                                              <----------------------->
        // Before:      [  PB   |          PTP          |  FR   ]       [  FR   ]
        // After:       [  PB   |              PTP              ]       [  PTP  ]
        {
            assert!(
                pool.as_mut().update_ram_subranges(Type::PoolTestPayload, 0x4000, 0x3000).is_ok()
            );

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
            assert_eq!(iter.next(), Some(Range::new(0x1000, 0x4000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x6000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }

        // Bookkeeping ranges are not safe from being updated.
        //
        //              <------------------------------------------------------->
        // Before:      [  PB   |              PTP              ]       [  PTP  ]
        // After:       [                  PTP                  ]       [  PTP  ]
        {
            assert!(
                pool.as_mut()
                    .update_ram_subranges(Type::PoolTestPayload, DEFAULT_MIN_ADDR, 0x7000)
                    .is_ok()
            );

            let mut iter = pool.as_ref().iter();
            assert_eq!(iter.next(), Some(Range::new(0, 0x5000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), Some(Range::new(0x6000, 0x1000, Type::PoolTestPayload)));
            assert_eq!(iter.next(), None);
        }
    }

    // Basic behavior of `truncate_total_ram()`.
    #[test]
    fn total_ram_truncation() {
        //              |-------|-------|
        // Pool:        [  PB   |  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        // New max capacity: 2 pages
        // Before:      [  PB   |  FR   ]
        // After:       [  PB   |  FR   ]
        assert!(pool.as_mut().truncate_total_ram(0x2000).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), None);

        // New max capacity: 1.5 pages
        // Before:      [  PB   |  FR   ]
        // After:       [  PB   |FR |TR ]
        assert!(pool.as_mut().truncate_total_ram(0x1800).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x800, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x1800, 0x800, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);

        assert!(pool.as_mut().allocate(Type::PoolTestPayload, 0x800, 0x800, None, None).is_ok());
        // New max capacity: 1 page
        // Before:      [  PB   |PTP|TR ]
        // After:       [  PB   |  TR   ]
        assert!(pool.as_mut().truncate_total_ram(0x1000).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);

        // New max capacity: 0.5 pages
        // Before:      [  PB   |  TR   ]
        // After:       [PB |    TR     ]
        assert!(pool.as_mut().truncate_total_ram(0x800).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x800, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x800, 0x1800, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);

        // New max capacity: 0 pages
        // Before:      [PB |    TR     ]
        // After:       [      TR       ]
        assert!(pool.as_mut().truncate_total_ram(0).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x2000, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);

        // There is no free ram left to be allocated (or bookkeeping to track it)
        assert!(pool.as_mut().allocate(Type::PhysScratch, 0x1000, 0x1000, None, None).is_err());
    }

    // Behavior of ram truncation when Nvram is involved.
    #[test]
    fn total_ram_truncation_with_peripheral_and_nvram_ranges() {
        //              |---|---|---| ... |---|---|---| ... |---| ... |---|
        // Pool:        [PB |FR | P ]     [  PTP  |FR ]     [ N ]     [FR ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x2000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0xc000, 0x1000, zbi::MemType::RAM),
            new_zbi_range(0x1e000, 0x1000, zbi::MemType::RAM),
        ];
        let preallocated_ranges = &mut [
            Range::new(0xa000, 0x2000, Type::PoolTestPayload),
            Range::new(0x14000, 0x1000, Type::Nvram),
        ];
        assert!(
            pool.as_mut()
                .init(zbi_ranges, preallocated_ranges, Some(DEFAULT_MIN_ADDR), None)
                .is_ok()
        );

        // New max capacity: 3 pages
        // Before:      [PB |FR | P ]     [  PTP  |FR ]     [ N ]     [FR ]
        // After:       [PB |FR | P ]     [PTP|  TR   ]     [ N ]     [TR ]
        assert!(pool.as_mut().truncate_total_ram(0x3000).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x1000, Type::PoolTestPayload)));
        assert_eq!(iter.next(), Some(Range::new(0xb000, 0x2000, Type::TruncatedRam)));
        assert_eq!(iter.next(), Some(Range::new(0x14000, 0x1000, Type::Nvram)));
        assert_eq!(iter.next(), Some(Range::new(0x1e000, 0x1000, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);

        // New max capacity: 0 pages
        // Before:      [PB |FR | P ]     [PTP|  TR   ]     [ N ]     [TR ]
        // After:       [  TR   | P ]     [    TR     ]     [ N ]     [TR ]
        assert!(pool.as_mut().truncate_total_ram(0).is_ok());
        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x2000, Type::TruncatedRam)));
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x3000, Type::TruncatedRam)));
        assert_eq!(iter.next(), Some(Range::new(0x14000, 0x1000, Type::Nvram)));
        assert_eq!(iter.next(), Some(Range::new(0x1e000, 0x1000, Type::TruncatedRam)));
        assert_eq!(iter.next(), None);
    }

    // Test behavior of operations on pool when there is no bookkeeping space.
    #[test]
    fn out_of_memory() {
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [new_zbi_range(0, 0x2000, zbi::MemType::RAM)];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        // Allocate two byte ranges of different types until we cannot allocate anymore due to lack
        // of bookkeeping capacity.
        let types = [Type::PoolTestPayload, Type::ZbiTestPayload];
        let mut idx = 0;
        while pool.as_mut().allocate(types[idx % 2], 2, 1, None, None).is_ok() {
            idx += 1;
        }
        assert!(idx > 0);
        assert!(pool.as_mut().allocate(Type::PoolTestPayload, 1, 1, None, None).is_err());

        let allocated = pool
            .as_ref()
            .iter()
            .find(|r| r.ty() != Type::PoolBookkeeping && r.ty() != Type::FreeRam && r.size() > 1)
            .unwrap();
        // There isn't enough bookkeeping to track the new free range introduced by this call to
        // free.
        assert!(pool.as_mut().free(allocated.addr(), 1).is_err());

        let free_ram =
            pool.as_ref().iter().find(|r| r.ty() == Type::FreeRam && r.size() > 1).unwrap();
        // There is not enough bookkeeping to track the new test payload range introduced by the
        // call to update_ram_subranges.
        assert!(
            pool.as_mut().update_ram_subranges(Type::PoolTestPayload, free_ram.addr(), 1).is_err()
        );
    }

    // Marking a range before the first range in the pool as peripheral adds it to the start.
    #[test]
    fn mark_as_peripheral_before_first_range() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                                                [PB |    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0xc000, 0x2000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                              <------->
        // Before:                                              [PB |    FR     ]
        // After:                                       [   P   |PB |    FR     ]
        let peripheral = Range::new(0x8000, 0x2000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(peripheral));
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xb000, 0x3000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a range after the last range in the pool as peripheral appends it to the end.
    #[test]
    fn mark_as_peripheral_after_last_range() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                                                [PB |    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0xc000, 0x2000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                                                      <------->
        // Before:                                              [PB |    FR     ]
        // After:                                               [PB |    FR     |   P   ]
        let peripheral = Range::new(0xe000, 0x2000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xb000, 0x3000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(peripheral));
        assert_eq!(iter.next(), None);
    }

    // Marking an unmapped gap between two RAM ranges as peripheral inserts it between them.
    #[test]
    fn mark_as_peripheral_between_ranges() {
        //              |---|---|---|---|---|---|---|---|---|---|
        // Pool:                        [PB |FR ]       [  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x4000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x8000, 0x2000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                      <------->
        // Before:                      [PB |FR ]       [  FR   ]
        // After:                       [PB |FR |   P   |  FR   ]
        let peripheral = Range::new(0x6000, 0x2000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x4000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x5000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(peripheral));
        assert_eq!(iter.next(), Some(Range::new(0x8000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a peripheral range that overlaps and abuts existing peripheral ranges merges them.
    #[test]
    fn mark_as_peripheral_between_ranges_merging() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                [PB |FR |   P   ]   [     P     |  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x2000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x4000, 0x2000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x7000, 0x3000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                  <------->
        // Before:              [PB |FR |   P   ]   [     P     |  FR   ]
        // After:               [PB |FR |           P           |  FR   ]
        let peripheral = Range::new(0x5000, 0x2000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x4000, 0x6000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a peripheral range separated from existing peripheral ranges by unmapped gaps does
    // not merge them.
    #[test]
    fn mark_as_peripheral_between_ranges_no_merging() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                [PB |FR | P ]           [ P ]   [  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x2000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x4000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x8000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0xa000, 0x2000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                      <--->
        // Before:              [PB |FR | P ]           [ P ]   [  FR   ]
        // After:               [PB |FR | P ]   [ P ]   [ P ]   [  FR   ]
        let peripheral = Range::new(0x6000, 0x1000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x4000, 0x1000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(peripheral));
        assert_eq!(iter.next(), Some(Range::new(0x8000, 0x1000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xa000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a peripheral range starting before the first range and spanning existing peripheral
    // ranges merges them.
    #[test]
    fn mark_as_peripheral_before_ranges_with_merging() {
        //              |---|---|---|---|---|---|---|---|
        // Pool:                [ P ]   [ P |PB |  FR   ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x2000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x4000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x5000, 0x3000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                  <--------------->
        // Before:              [ P ]   [ P |PB |  FR   ]
        // After:           [       P       |PB |  FR   ]
        let peripheral = Range::new(0x1000, 0x4000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x1000, 0x4000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x5000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x6000, 0x2000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a peripheral range directly adjacent to two peripheral ranges coalesces all three.
    #[test]
    fn mark_as_peripheral_adjacent_bounds() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                [PB |FR | P ]           [ P |    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x2000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x4000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x8000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x9000, 0x3000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                  <----------->
        // Before:              [PB |FR | P ]           [ P |    FR     ]
        // After:               [PB |FR |         P         |    FR     ]
        let peripheral = Range::new(0x5000, 0x3000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x4000, 0x5000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x9000, 0x3000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a peripheral range that encompasses an interior peripheral range and abuts two outer
    // peripheral ranges coalesces all of them.
    #[test]
    fn mark_as_peripheral_adjacent_bounds_with_intersecting_range() {
        //              |---|---|---|---|---|---|---|---|---|---|---|---|
        // Pool:                [PB |FR | P ]   [ P ]   [ P |    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0x2000, 0x2000, zbi::MemType::RAM),
            new_zbi_range(0x4000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x6000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x8000, 0x1000, zbi::MemType::PERIPHERAL),
            new_zbi_range(0x9000, 0x3000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //                                  <----------->
        // Before:              [PB |FR | P ]   [ P ]   [ P |    FR     ]
        // After:               [PB |FR |         P         |    FR     ]
        let peripheral = Range::new(0x5000, 0x3000, Type::Peripheral);
        assert!(pool.as_mut().mark_as_peripheral(&peripheral).is_ok());

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0x2000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0x3000, 0x1000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x4000, 0x5000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x9000, 0x3000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // Marking a range as peripheral fails if it overlaps any non-peripheral range.
    #[test]
    fn mark_as_peripheral_overlaps_with_non_peripheral() {
        //              |---|---|---|---|---|---|---|---|---|
        // Pool:        [PB |    FR     ]   [      FR       ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0, 0x4000, zbi::MemType::RAM),
            new_zbi_range(0x5000, 0x4000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        //              <--->
        //              [PB |    FR     ]   [      FR       ]
        assert!(
            pool.as_mut().mark_as_peripheral(&Range::new(0, 0x1000, Type::Peripheral)).is_err()
        );
        //                                  <--->
        //              [PB |    FR     ]   [      FR       ]
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x5000, 0x1000, Type::Peripheral))
                .is_err()
        );
    }

    // A peripheral run before the first RAM range is coalesced and expanded to 1 GiB alignment.
    #[test]
    fn coalesce_peripheral_single_run_at_start() {
        // Each tick is 0x40000 pages.
        //              |-------|-------|-------|-------|-------|-------|-------|-------|-------|
        // Pool:                                [PB|                     FR                     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xc0000000, 0x80000000, zbi::MemType::RAM),
            new_zbi_range(0x140000000, 0x100000000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x10000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x2f400000, 0x400, Type::Peripheral))
                .is_ok()
        );

        // Before:       [P] [P]                [PB|                     FR                     ]
        // After:       [   P   ]               [PB|                     FR                     ]
        pool.as_mut().coalesce_peripherals(&[30, 21, 12]);

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0x40000000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xc0000000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xc0001000, 0x17ffff000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // When 1 GiB alignment would overlap adjacent RAM, `coalesce_peripherals()` falls back to the
    // next alignment (2 MiB).
    #[test]
    fn coalesce_peripheral_worse_alignment() {
        // Each tick is 0x40000 pages.
        //              |-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|
        // Pool:                          [PB|     FR    ]        [          FR           ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xc0000000, 0xa0000000, zbi::MemType::RAM),
            new_zbi_range(0x1a0000000, 0x100000000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x160500000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x19ff00000, 0x100000, Type::Peripheral))
                .is_ok()
        );

        //                                            <----------->
        // Before:                        [PB|     FR    ] [P]  [P|          FR           ]
        // After:                         [PB|     FR    ] [   P  |          FR           ]
        pool.as_mut().coalesce_peripherals(&[30, 21, 12]);

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0xc0000000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xc0001000, 0x9ffff000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x160400000, 0x3fc00000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x1a0000000, 0x100000000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // A peripheral run between two RAM ranges is coalesced and expanded to 1 GiB alignment.
    #[test]
    fn coalesce_peripheral_single_run_at_middle() {
        // Each tick is 0x40000 pages.
        //              |-----|-----|-----|-----|-----|-----|-----|-----|-----|
        // Pool:                          [PB|   FR   ]           [    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xc0000000, 0x80000000, zbi::MemType::RAM),
            new_zbi_range(0x1c0000000, 0x80000000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x150000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x1af400000, 0x400, Type::Peripheral))
                .is_ok()
        );

        // Before:                        [PB|   FR   ] [P]   [P] [    FR     ]
        // After:                         [PB|   FR   |     P     |    FR     ]
        pool.as_mut().coalesce_peripherals(&[30, 21, 12]);

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0xc0000000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xc0001000, 0x7ffff000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x140000000, 0x80000000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x1c0000000, 0x80000000, Type::FreeRam)));
        assert_eq!(iter.next(), None);
    }

    // A peripheral run after the last RAM range is coalesced and expanded to 1 GiB alignment.
    #[test]
    fn coalesce_peripheral_single_run_at_end() {
        // Each tick is 0x40000 pages.
        //         |----|----|----|----|----|----|----|----|----|----|----|----|----|----|----|----|
        // Pool:                  [PB|  FR  ]         [   FR    ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xc0000000, 0x80000000, zbi::MemType::RAM),
            new_zbi_range(0x1c0000000, 0x80000000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x250000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x3ef400000, 0x400, Type::Peripheral))
                .is_ok()
        );

        // Before:                [PB|  FR  ]         [   FR    ] [P]                          [P]
        // After:                 [PB|  FR  ]         [   FR    |                P                 ]
        pool.as_mut().coalesce_peripherals(&[30, 21, 12]);

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0xc0000000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xc0001000, 0x7ffff000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x1c0000000, 0x80000000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x240000000, 0x1c0000000, Type::Peripheral)));
        assert_eq!(iter.next(), None);
    }

    // Multiple peripheral runs separated by RAM ranges are each coalesced independently.
    #[test]
    fn coalesce_peripheral_multiple_runs() {
        // Each tick is 0x40000 pages.
        //          |-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|
        // Pool:                      [PB|   FR   ]           [    FR     ]
        stack_pin_init!(let pool = uninit_test_pool());
        let zbi_ranges = &mut [
            new_zbi_range(0xc0000000, 0x80000000, zbi::MemType::RAM),
            new_zbi_range(0x1c0000000, 0x80000000, zbi::MemType::RAM),
        ];
        assert!(pool.as_mut().init(zbi_ranges, &mut [], Some(DEFAULT_MIN_ADDR), None).is_ok());

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x10000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x60000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x90000000, 0x40000, Type::Peripheral))
                .is_ok()
        );

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x150000000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x180000000, 0x4800, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x1af400000, 0x3c00, Type::Peripheral))
                .is_ok()
        );

        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x2f8400000, 0x40000, Type::Peripheral))
                .is_ok()
        );
        assert!(
            pool.as_mut()
                .mark_as_peripheral(&Range::new(0x300200000, 0x400, Type::Peripheral))
                .is_ok()
        );

        // Before:   [P]    [P]   [P] [PB|   FR   ] [P][P][P] [    FR     ]              [P] [P]
        // After:   [        P        |PB|   FR   |     P     |    FR     ]           [     P     ]
        pool.as_mut().coalesce_peripherals(&[30, 21, 12]);

        let mut iter = pool.as_ref().iter();
        assert_eq!(iter.next(), Some(Range::new(0, 0xc0000000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0xc0000000, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(iter.next(), Some(Range::new(0xc0001000, 0x7ffff000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x140000000, 0x80000000, Type::Peripheral)));
        assert_eq!(iter.next(), Some(Range::new(0x1c0000000, 0x80000000, Type::FreeRam)));
        assert_eq!(iter.next(), Some(Range::new(0x2c0000000, 0x80000000, Type::Peripheral)));
        assert_eq!(iter.next(), None);
    }
}
