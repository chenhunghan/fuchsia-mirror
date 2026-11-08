// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel heap memory profiler buffer and hash map.

// WARNING: This defines a private and unstable binary interface for a user-space consumer.
//
// The Rust-based consumer relies on the exact memory layout, and access patterns defined here.
// Because this layout is duplicated manually (not generated), the compiler provides no safety
// against ABI mismatches. Changes to this contract must be carefully synchronized.

// This is subject to change in every kernel version and is not meant to be any kind of stable ABI
// between the kernel and userland. The expectation is that this layout will be used only by a
// single privileged service that is tightly-coupled with the kernel, i.e. always built from source
// when building the kernel.

// Memory Layout and Visibility Guarantees
// ---------------------------------------

// The heap profile consists of a sequence of variable-size records stored
// consecutively in a fixed-size buffer.
//
// Buffer Layout:
//   - Header (8 bytes)
//   - 0 or more variable-size records
//   - Remaining space is zeroed
//
// Record Layout:
//   - Counters (32 bytes)
//   - Backtrace Size N (8 bytes)
//   - Backtrace elements (N * 8 bytes)
//
// The kernel updates this buffer. To read a consistent snapshot, a client can depend on the
// following memory ordering guarantees:
//
// 1. The `Header` is initialized before the buffer is shared with userspace.
//    `Header.version` should be 1.
//    `Header.event_dropped` should be read first with `memory_order_relaxed`.
// 2. For each record, the backtrace elements are written before the `backtrace_size`.
//    `backtrace_size` atomic must be read with `memory_order_acquire` to ensure visibility on
//    backtrace elements.
// 3. The `Counters` are incremented and decremented atomically without ordering guarantees.
//
// A client should first read the Header atomics, and the iterate through records reading
// `backtrace_size` first, until it finds a size of 0, which marks the end of the profile data.

use core::cell::{Cell, UnsafeCell};
use core::mem::{align_of, size_of};
use core::slice;
use core::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};

/// Header struct placed at the beginning of the heap profile buffer.
/// This is exposed to the userspace.
#[repr(C)]
pub struct Header {
    /// Memory profile format version. Nonzero.
    pub version: i32,
    /// >0 when events where dropped because of an overflow.
    pub event_dropped: AtomicI32,
}

zr::static_assert!(size_of::<Header>() == 8);
zr::static_assert!(align_of::<Header>() == 4);

/// Aggregated statistics for a given allocation backtrace.
/// This is exposed to the userspace.
///
/// Counters are updated without ordering or visibility guarantees.
#[repr(C)]
pub struct Counters {
    /// Number of currently live allocations for this backtrace.
    pub live_count: AtomicU64,
    /// Total bytes of currently live allocations for this backtrace.
    pub live_bytes: AtomicU64,
    /// Cumulative number of allocations for this backtrace.
    pub total_count: AtomicU64,
    /// Cumulative bytes allocated for this backtrace.
    pub total_bytes: AtomicU64,
}

zr::static_assert!(size_of::<Counters>() == 32);
zr::static_assert!(align_of::<Counters>() == 8);

impl Counters {
    /// Records an allocation of `size` bytes.
    #[inline]
    pub fn allocate(&self, size: u64) {
        self.total_bytes.fetch_add(size, Ordering::Relaxed);
        self.total_count.fetch_add(1, Ordering::Relaxed);
        self.live_bytes.fetch_add(size, Ordering::Relaxed);
        self.live_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a deallocation of `size` bytes.
    #[inline]
    pub fn deallocate(&self, size: u64) {
        self.live_bytes.fetch_sub(size, Ordering::Relaxed);
        self.live_count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Hashtable value, stored in the heap profile buffer.
/// This is exposed to the userspace.
#[repr(C)]
pub struct Value {
    /// Aggregated allocation counters for this record.
    pub counters: Counters,
    /// Number of backtrace frames following this `Value` in the buffer.
    pub backtrace_size: AtomicI64,
}

zr::static_assert!(size_of::<Value>() == 40);
zr::static_assert!(align_of::<Value>() == 8);

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for zero-initializable, 8-byte-multiple record types stored in [`Buffer`].
///
/// # Safety
///
/// Implementing types must be valid for all-zero bit patterns, have no uninitialized padding, and
/// satisfy `size_of::<Self>() % 8 == 0` and `8 % align_of::<Self>() == 0`.
pub unsafe trait BufferElement: sealed::Sealed + Sized {}

impl sealed::Sealed for Header {}
// SAFETY: `Header` is `repr(C)` of two `i32` / `AtomicI32` fields (size 8, align 4), valid when
// zero-initialized.
unsafe impl BufferElement for Header {}

impl sealed::Sealed for Value {}
// SAFETY: `Value` is `repr(C)` of `AtomicU64` / `AtomicI64` fields (size 40, align 8), valid when
// zero-initialized.
unsafe impl BufferElement for Value {}

impl sealed::Sealed for u64 {}
// SAFETY: `u64` has size 8 and align 8 and is valid when zero-initialized.
unsafe impl BufferElement for u64 {}

impl sealed::Sealed for usize {}
// SAFETY: `usize` has size 8 and align 8 on 64-bit targets and is valid when zero-initialized.
unsafe impl BufferElement for usize {}

/// Page-aligned bump allocator with allocation, and offset-based data access.
#[repr(C, align(4096))]
pub struct Buffer<const LENGTH: usize> {
    data: UnsafeCell<[u8; LENGTH]>,
    size: Cell<usize>,
}

zr::static_assert!(align_of::<Buffer<0>>() == page::SIZE);

impl<const LENGTH: usize> Buffer<LENGTH> {
    /// Creates a new `Buffer`.
    /// The content of the memory is initialized to zero.
    pub const fn new() -> Self {
        Self { data: UnsafeCell::new([0; LENGTH]), size: Cell::new(0) }
    }

    /// Returns the number of bytes currently allocated in the buffer.
    #[inline]
    pub fn size(&self) -> usize {
        self.size.get()
    }

    /// Returns a raw pointer to the beginning of the buffer and its byte length.
    #[inline]
    pub const fn data_ptr(&self) -> (*const u8, usize) {
        (self.data.get().cast::<u8>(), LENGTH)
    }

    /// Returns a slice pointing at the whole buffer.
    #[cfg(test)]
    pub fn data(&self) -> &[u8] {
        // SAFETY: `self.data` is valid for `LENGTH` bytes for the lifetime of `self`, and in unit
        // tests no concurrent atomic writes occur while the returned slice is inspected.
        unsafe { slice::from_raw_parts(self.data.get().cast::<u8>(), LENGTH) }
    }

    /// Allocates a block of memory large enough to hold `count` objects of type `T`, returning a
    /// raw pointer to the first element.
    /// The memory is initialized to zero. Returns `None` when there is not enough space available.
    #[inline]
    pub fn allocate_raw<T: BufferElement>(&self, count: usize) -> Option<*mut T> {
        const {
            // Ensure no padding is ever required.
            assert!(size_of::<T>().is_multiple_of(size_of::<u64>()));
            assert!(size_of::<u64>().is_multiple_of(align_of::<T>()));
        }
        let size = self.size.get();
        if size.saturating_add(count.saturating_mul(size_of::<T>())) > LENGTH {
            return None;
        }
        if !size.is_multiple_of(align_of::<T>()) {
            // This should never happen because all structures size are multiple of 8 bytes.
            return None;
        }
        self.size.set(size + count * size_of::<T>());
        // SAFETY: The offset `size` is within `self.data` bounds and aligned for `T`.
        Some(unsafe { self.data.get().cast::<u8>().add(size).cast::<T>() })
    }

    /// Allocates a block of memory large enough to hold `count` objects of type `T`.
    /// The memory is initialized to zero. Returns `None` when there is not enough space available.
    #[inline]
    pub fn allocate<T: BufferElement>(&mut self, count: usize) -> Option<&mut [T]> {
        let ptr = self.allocate_raw::<T>(count)?;
        // SAFETY: `ptr` points to `count` zero-initialized elements of `T: BufferElement` within
        // `self.data`, and `&mut self` guarantees exclusive access when no concurrent reader
        // accesses this region.
        Some(unsafe { slice::from_raw_parts_mut(ptr, count) })
    }

    /// Extends the used part of the buffer with the given slice of objects, and returns a raw
    /// pointer to the copy. Returns `None` when there is not enough space available.
    #[inline]
    pub fn append_raw<T: BufferElement + Copy>(&self, value: &[T]) -> Option<*mut [T]> {
        let dst = self.allocate_raw::<T>(value.len())?;
        // SAFETY: `dst` points to `value.len()` freshly allocated elements in `self.data` at a
        // previously unused offset, which cannot overlap `value`.
        unsafe {
            core::ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
        }
        Some(core::ptr::slice_from_raw_parts_mut(dst, value.len()))
    }

    /// Extends the used part of the buffer with the given slice of objects, and returns a slice
    /// pointing at the copy. Returns `None` when there is not enough space available.
    #[inline]
    pub fn append<T: BufferElement + Copy>(&mut self, value: &[T]) -> Option<&mut [T]> {
        let dst = self.append_raw(value)?;
        // SAFETY: `dst` is a valid initialized slice within `self.data` and `&mut self`
        // guarantees exclusive access when no concurrent reader accesses this region.
        Some(unsafe { &mut *dst })
    }

    /// Returns the offset of a given pointer relative to the beginning of the buffer.
    #[inline]
    fn offset_for<T: ?Sized>(&self, value: *const T) -> usize {
        value.addr() - self.data.get().addr()
    }

    /// Returns a raw pointer to `count` objects of type `T` placed at the specified offset, or
    /// `None` if the slice is not located inside the used buffer.
    #[inline]
    #[cfg(test)]
    fn raw_at_offset<T: BufferElement>(&self, offset: usize, count: usize) -> Option<*mut T> {
        if !offset.is_multiple_of(align_of::<T>()) {
            // This should never happen because all structures size are multiple of 8 bytes,
            // and the caller is not supposed to pass a non-aligned offset.
            return None;
        }
        let size = self.size.get();
        if offset >= size || offset.saturating_add(size_of::<T>().saturating_mul(count)) > size {
            return None;
        }
        // SAFETY: `offset < size <= LENGTH`, so `self.data.get().cast::<u8>().add(offset)` is
        // in-bounds of `self.data` and aligned to `align_of::<T>()`.
        Some(unsafe { self.data.get().cast::<u8>().add(offset).cast::<T>() })
    }

    /// Returns the slice of `count` objects of type `T` placed at the specified offset, or `None`
    /// if the slice is not located inside the used buffer.
    #[cfg(test)]
    fn at_offset<T: BufferElement>(&mut self, offset: usize, count: usize) -> Option<&mut [T]> {
        let ptr = self.raw_at_offset::<T>(offset, count)?;
        // SAFETY: `self.data` is 4096-byte aligned, `offset` is aligned to `align_of::<T>()`,
        // `[offset, offset + count * size_of::<T>())` lies within the initialized zeroed buffer,
        // `T: BufferElement` is valid for all-zero bytes, and `&mut self` guarantees exclusive
        // access.
        Some(unsafe { slice::from_raw_parts_mut(ptr, count) })
    }
}

#[derive(Copy, Clone)]
struct Entry {
    /// Byte offset of the associated [`Value`] in the owning `HeapProfileMap`'s buffer.
    ///
    /// Safety invariant: When stored in `HeapProfileState::entries[..entries_size]`, `value_offset`
    /// is a valid aligned byte offset of an allocated [`Value`].
    value_offset: usize,
    /// Byte offset of the stored backtrace slice in the owning `HeapProfileMap`'s buffer.
    ///
    /// Safety invariant: When stored in `HeapProfileState::entries[..entries_size]`,
    /// `backtrace_offset` and `backtrace_len` describe a valid aligned, initialized `[usize]` slice
    /// that is never mutated after initialization.
    backtrace_offset: usize,
    /// Number of frames in the stored backtrace.
    backtrace_len: usize,
    /// 1-based index of the next entry in the hash collision chain, or 0 if end of chain.
    next: usize,
}

impl Entry {
    const EMPTY: Self = Self { value_offset: 0, backtrace_offset: 0, backtrace_len: 0, next: 0 };

    /// Trait implementation for hashtable lookup.
    ///
    /// # Safety
    ///
    /// `self` must be an initialized entry belonging to `buffer` such that `self.backtrace_offset`
    /// and `self.backtrace_len` denote an in-bounds, properly aligned, immutable `[usize]` slice
    /// within `buffer.data`.
    #[inline]
    unsafe fn get_key<'a, const BUFFER_LENGTH: usize>(
        &self,
        buffer: &'a Buffer<BUFFER_LENGTH>,
    ) -> &'a [usize] {
        // SAFETY: Caller guarantees `self.backtrace_offset` and `self.backtrace_len` were
        // allocated in `buffer` via `append_raw` + `offset_for` for a `[usize]` slice that remains
        // immutable once written.
        unsafe {
            let ptr = buffer.data.get().cast::<u8>().add(self.backtrace_offset).cast::<usize>();
            slice::from_raw_parts(ptr, self.backtrace_len)
        }
    }

    #[inline]
    fn equal_to(a: &[usize], b: &[usize]) -> bool {
        a == b
    }

    #[inline]
    fn get_hash(data: &[usize]) -> usize {
        let mut result: usize = 0;
        for &value in data {
            result ^=
                value.wrapping_add(0x9e3779b9).wrapping_add(result << 6).wrapping_add(result >> 2);
        }
        result
    }
}

#[repr(C)]
struct HeapProfileState<const CAPACITY: usize, const BUCKET_COUNT: usize> {
    /// Entries of the hashtable.
    ///
    /// Safety invariant: For all `i < self.entries_size`, `self.entries[i]` was initialized by
    /// `try_get_entry` with a `value_offset` returned by `buffer.allocate_raw::<Value>(1)` +
    /// `offset_for` and `(backtrace_offset, backtrace_len)` returned by
    /// `buffer.append_raw(backtrace)` + `offset_for` on the owning `HeapProfileMap`'s `buffer`.
    entries: [Entry; CAPACITY],
    /// Current number of elements in `entries`.
    entries_size: usize,

    /// Buckets of Hashtable from backtrace to counters.
    /// Either 0 when the bucket is empty, or 1-based index into `entries`.
    backtrace_to_values: [usize; BUCKET_COUNT],
}

impl<const CAPACITY: usize, const BUCKET_COUNT: usize> HeapProfileState<CAPACITY, BUCKET_COUNT> {
    const fn new_zeroed() -> Self {
        Self {
            entries: [Entry::EMPTY; CAPACITY],
            entries_size: 0,
            backtrace_to_values: [0; BUCKET_COUNT],
        }
    }
}

/// Fixed-size lookup table that links code path to its memory usage statistics.
///
/// Const generic arguments:
///  `BUFFER_LENGTH`: length of the buffer holding `Header`, `Value` and backtraces. Must be a
///    multiple of `page::SIZE`.
///  `CAPACITY`: the maximum number of `Entry` that can be stored in the hashmap.
///  `BUCKET_COUNT`: number of buckets for the hashmap. Must be smaller than `CAPACITY`.
#[repr(C)]
pub struct HeapProfileMap<
    const BUFFER_LENGTH: usize,
    const CAPACITY: usize,
    const BUCKET_COUNT: usize,
> {
    /// Buffers shared with userspace that holds the heap profile.
    /// It starts with one `Header` followed by a sequence of `(Value, backtrace)`.
    /// the backtrace being an array of `zx_vaddr_t` for size `Value::backtrace_size`.
    buffer: Buffer<BUFFER_LENGTH>,

    /// Mutable hashtable state, wrapped in `UnsafeCell` so that mutating hashtable entries never
    /// requires a `&mut HeapProfileMap` that would assert `noalias` over `buffer.data`.
    state: UnsafeCell<HeapProfileState<CAPACITY, BUCKET_COUNT>>,
}

impl<const BUFFER_LENGTH: usize, const CAPACITY: usize, const BUCKET_COUNT: usize>
    HeapProfileMap<BUFFER_LENGTH, CAPACITY, BUCKET_COUNT>
{
    /// Creates an all-zero `HeapProfileMap` suitable for `.bss` static storage.
    ///
    /// `ALLOC_MAP` is zero-initialized in `.bss` (to avoid placing a 4 MiB buffer with non-zero
    /// `Header.version = 1` into `.data` and inflating the kernel image by 4 MiB).
    /// [`Self::init`] is called during [`heap_init`] (with a lazy fallback in
    /// `try_get_entry` / `event_dropped` if called before [`heap_init`]) to initialize the
    /// [`Header`].
    pub const fn new_zeroed() -> Self {
        const {
            assert!(BUFFER_LENGTH >= size_of::<Header>());
        }
        Self { buffer: Buffer::new(), state: UnsafeCell::new(HeapProfileState::new_zeroed()) }
    }

    /// Allocates and initializes the [`Header`] (`version = 1`) at the start of `buffer`.
    ///
    /// This is called during [`heap_init`] (with a lazy fallback in `try_get_entry` /
    /// `event_dropped` if called before [`heap_init`]) so that `ALLOC_MAP` can remain
    /// zero-initialized in `.bss` rather than placing a 4 MiB buffer into `.data`.
    #[inline]
    pub fn init(&self) {
        if self.buffer.size() == 0 {
            self.init_cold();
        }
    }

    #[cold]
    #[inline(never)]
    fn init_cold(&self) {
        let header = self.buffer.allocate_raw::<Header>(1).unwrap();
        // SAFETY: `header` was freshly allocated at offset 0 of `self.buffer` before any
        // record is published.
        unsafe {
            (*header).version = 1;
        }
    }

    /// Creates a new `HeapProfileMap` with `Header { version: 1, event_dropped: 0 }` initialized
    /// at the start of `buffer`.
    #[cfg(test)]
    pub fn new() -> Self {
        let map = Self::new_zeroed();
        map.init();
        map
    }

    #[inline]
    fn header(&self) -> &Header {
        debug_assert!(self.buffer.size() >= size_of::<Header>());
        // SAFETY: `HeapProfileMap::init` allocates a `Header` at offset 0 of the 4096-aligned
        // buffer before `header()` is called, and `Header` is only mutated via its `AtomicI32`
        // `event_dropped` field after initialization.
        unsafe { &*self.buffer.data.get().cast::<Header>() }
    }

    /// Returns a shared reference to the [`Value`] at `value_offset` in `buffer`.
    ///
    /// # Safety
    ///
    /// `value_offset` must be a valid aligned byte offset of an allocated [`Value`] in
    /// `buffer` (obtained from `allocate_raw::<Value>(1)` and `offset_for`).
    #[inline]
    unsafe fn value_at(buffer: &Buffer<BUFFER_LENGTH>, value_offset: usize) -> &Value {
        // SAFETY: Caller guarantees `value_offset` was returned by `allocate_raw::<Value>(1)` +
        // `offset_for` on `buffer`, and all fields of `Value` (`Counters` and
        // `backtrace_size`) use atomic interior mutability.
        unsafe { &*buffer.data.get().cast::<u8>().add(value_offset).cast::<Value>() }
    }

    /// Gets or creates `Counters` for a given allocation backtrace.
    /// Must be called under the profile lock.
    /// Upon success, returns a reference to the `Counters` and the 1-based handle for that
    /// instance to be used with `try_by_handle` for future retrieval.
    /// When the capacity is reached, the creation fails and `(None, 0)` is returned.
    #[inline]
    pub fn try_get(&self, backtrace: &[usize]) -> (Option<&Counters>, usize) {
        let Some(handle) = self.try_get_entry(backtrace) else {
            return (None, 0);
        };
        // The handle is the index in `entries` + 1.
        // 0 is not a valid handle.
        // SAFETY: `HeapProfileMap` is `!Sync` (synchronized by `StatLock` via `KCell` in the
        // kernel) and `self.state` is not borrowed across any reentrant call.
        let value_offset = unsafe { (*self.state.get()).entries[handle - 1].value_offset };
        // SAFETY: `handle` was returned by `try_get_entry`, so `entries[handle - 1].value_offset`
        // is a valid aligned offset of an allocated `Value` in `self.buffer`.
        (Some(&unsafe { Self::value_at(&self.buffer, value_offset) }.counters), handle)
    }

    /// Returns a raw pointer and byte length to the live memory profile buffer exposed to
    /// userspace. The buffer is page aligned.
    #[inline]
    pub const fn data_ptr(&self) -> (*const u8, usize) {
        self.buffer.data_ptr()
    }

    /// Slice pointing to the buffer holding a live memory profile exposed to userspace.
    /// The buffer is page aligned.
    #[cfg(test)]
    pub fn data(&self) -> &[u8] {
        self.buffer.data()
    }

    /// Increases the count of event dropped.
    /// Must be called under the profile lock.
    #[cold]
    pub fn event_dropped(&self) {
        if self.buffer.size() == 0 {
            self.init();
        }
        // The increment is `memory_order_relaxed` because the reader does not require visibility
        // of other writes. It only needs to eventually see a non-zero value to fail early.
        self.header().event_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns a reference to the counters at the given handle, or `None` if the handle is invalid.
    /// Must be called under the profile lock.
    #[inline]
    pub fn try_by_handle(&self, handle: usize) -> Option<&Counters> {
        let idx = handle.wrapping_sub(1);
        // SAFETY: `HeapProfileMap` is `!Sync` (synchronized by `StatLock` via `KCell` in the
        // kernel) and `self.state` is not borrowed across any reentrant call.
        let state = unsafe { &*self.state.get() };
        let limit = state.entries_size.min(CAPACITY);
        if idx >= limit {
            core::hint::cold_path();
            return None;
        }
        let value_offset = state.entries[..limit][idx].value_offset;
        // SAFETY: `idx < state.entries_size`, so `state.entries[idx].value_offset` is a valid
        // aligned offset of an allocated `Value` in `self.buffer`.
        Some(&unsafe { Self::value_at(&self.buffer, value_offset) }.counters)
    }

    #[inline]
    fn find_in_bucket(
        buffer: &Buffer<BUFFER_LENGTH>,
        state: &HeapProfileState<CAPACITY, BUCKET_COUNT>,
        bucket: usize,
        backtrace: &[usize],
    ) -> Option<usize> {
        let mut curr = state.backtrace_to_values[bucket];
        while curr != 0 {
            let entry = &state.entries[curr - 1];
            // SAFETY: `curr` is a non-zero handle in `backtrace_to_values`, so `entry` is an
            // initialized entry in `state.entries[..state.entries_size]` belonging to
            // `buffer`.
            if Entry::equal_to(unsafe { entry.get_key(buffer) }, backtrace) {
                return Some(curr);
            }
            curr = entry.next;
        }
        None
    }

    #[inline]
    fn insert_in_bucket(
        state: &mut HeapProfileState<CAPACITY, BUCKET_COUNT>,
        bucket: usize,
        handle: usize,
    ) {
        state.entries[handle - 1].next = state.backtrace_to_values[bucket];
        state.backtrace_to_values[bucket] = handle;
    }

    #[inline]
    fn try_get_entry(&self, backtrace: &[usize]) -> Option<usize> {
        if backtrace.is_empty() {
            return None;
        }
        if self.buffer.size() == 0 {
            self.init();
        }
        // Split borrows across disjoint fields: borrow `self.buffer` and `self.state` separately
        // to avoid passing `&self` while `state: &mut HeapProfileState` is live.
        let buffer = &self.buffer;
        // SAFETY: `HeapProfileMap` is `!Sync` (synchronized by `StatLock` via `KCell` in the
        // kernel) and `state` does not escape this non-reentrant function.
        let state = unsafe { &mut *self.state.get() };
        let bucket = Entry::get_hash(backtrace) % BUCKET_COUNT;
        if let Some(handle) = Self::find_in_bucket(buffer, state, bucket, backtrace) {
            return Some(handle);
        }

        if state.entries_size >= CAPACITY {
            core::hint::cold_path();
            return None;
        }

        let required_bytes = size_of::<Value>() + core::mem::size_of_val(backtrace);
        let current_size = buffer.size.get();
        if current_size.saturating_add(required_bytes) > BUFFER_LENGTH {
            core::hint::cold_path();
            return None;
        }

        // Add a new Value and backtrace to the buffer.
        let Some(stored_value) = buffer.allocate_raw::<Value>(1) else {
            core::hint::cold_path();
            return None; // Value buffer is full.
        };
        let Some(stored_backtrace) = buffer.append_raw(backtrace) else {
            core::hint::cold_path();
            return None; // value buffer is full.
        };

        // `backtrace_size` is written with `memory_order_release` to ensure that the
        // backtrace elements are visible to any reader that acquires this value.
        let value_offset = buffer.offset_for(stored_value);
        // SAFETY: `value_offset` is the offset of `stored_value` just allocated via
        // `buffer.allocate_raw::<Value>(1)`.
        unsafe { Self::value_at(buffer, value_offset) }
            .backtrace_size
            .store(backtrace.len() as i64, Ordering::Release);

        // Add a new entry to the hashtable.
        if state.entries_size >= state.entries.len() {
            core::hint::cold_path();
            return None; // Entry array is full.
        }
        let handle = state.entries_size + 1;
        state.entries_size += 1;
        state.entries[handle - 1] = Entry {
            value_offset,
            backtrace_offset: buffer.offset_for(stored_backtrace),
            backtrace_len: backtrace.len(),
            next: 0,
        };
        Self::insert_in_bucket(state, bucket, handle);

        Some(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;

    fn equals<T: PartialEq>(span1: &[T], span2: &[T]) -> bool {
        span1 == span2
    }

    fn as_u8(values: &[u64]) -> &[u8] {
        values.as_bytes()
    }

    #[test]
    fn empty_backtrace_test() {
        let map = HeapProfileMap::<1024, 4096, 64>::new_zeroed();
        let (counters, size) = map.try_get(&[]);
        assert!(counters.is_none());
        assert_eq!(size, 0);
        // Also verify the buffer size doesn't increase, meaning we didn't corrupt the buffer.
        assert_eq!(map.buffer.size.get(), 0);
    }

    #[test]
    fn data_test() {
        const SIZE: usize = 42;
        let buf = Buffer::<SIZE>::new();
        assert_eq!(SIZE, buf.data().len());
    }

    #[test]
    fn allocate_and_append_test() {
        const SIZE: usize = size_of::<[u64; 5]>();
        let mut buf = Buffer::<SIZE>::new();

        let s1 = buf.allocate::<u64>(2).unwrap();
        assert!(equals(s1, &[0, 0]));
        assert!(equals(buf.data(), as_u8(&[0, 0, 0, 0, 0])));
        // Change the span, also changes the buffer.
        let s1 = buf.at_offset::<u64>(0, 2).unwrap();
        s1[0] = 1;
        s1[1] = 2;
        assert!(equals(buf.data(), as_u8(&[1, 2, 0, 0, 0])));

        let s2 = buf.append::<u64>(&[33, 44, 55]).unwrap();
        assert!(equals(s2, &[33, 44, 55]));
        assert!(equals(buf.data(), as_u8(&[1, 2, 33, 44, 55])));

        let s2 = buf.at_offset::<u64>(size_of::<[u64; 2]>(), 3).unwrap();
        s2[0] = 3;
        s2[1] = 4;
        s2[2] = 5;

        assert!(equals(buf.data(), as_u8(&[1, 2, 3, 4, 5])));
    }

    #[test]
    fn allocate_failed_test() {
        const SIZE: usize = size_of::<[u64; 3]>();
        let mut buf = Buffer::<SIZE>::new();
        assert!(buf.allocate::<u64>(2).is_some());
        assert!(buf.allocate::<u64>(2).is_none());
        assert!(buf.allocate::<u64>(1).is_some());
        assert!(buf.allocate::<u64>(1).is_none());
    }

    #[test]
    fn append_failed_test() {
        const SIZE: usize = size_of::<[u64; 3]>();
        let mut buf = Buffer::<SIZE>::new();
        assert!(buf.append::<u64>(&[1, 2]).is_some());
        assert!(buf.append::<u64>(&[3, 4]).is_none());
        assert!(buf.append::<u64>(&[5]).is_some());
        assert!(buf.append::<u64>(&[6]).is_none());
        assert!(equals(buf.data(), as_u8(&[1, 2, 5])));
    }

    #[test]
    fn more_offset_test() {
        const BUCKET_COUNT: usize = 2;
        let map = HeapProfileMap::<1024, 128, BUCKET_COUNT>::new();

        /// A number larger than the number of buckets, so that Entry::next is used.
        const INSERT_COUNT: usize = BUCKET_COUNT * 2;
        for i in 0..INSERT_COUNT {
            let (v, handle) = map.try_get(&[i, 2, 3]);
            assert!(v.is_some());
            let v = v.unwrap();
            assert_ne!(handle, 0);
            assert_eq!(0, v.live_count.load(Ordering::Relaxed));
            assert_eq!(0, v.live_bytes.load(Ordering::Relaxed));
            assert_eq!(0, v.total_count.load(Ordering::Relaxed));
            assert_eq!(0, v.total_bytes.load(Ordering::Relaxed));
            v.live_count.store(i as u64, Ordering::Relaxed);
        }

        // Getting the same values again.
        for i in 0..INSERT_COUNT {
            let (v, o) = map.try_get(&[i, 2, 3]);
            let v = v.unwrap();

            assert_eq!(i as u64, v.live_count.load(Ordering::Relaxed));
            assert_eq!(0, v.live_bytes.load(Ordering::Relaxed));
            assert_eq!(0, v.total_count.load(Ordering::Relaxed));
            assert_eq!(0, v.total_bytes.load(Ordering::Relaxed));

            let vv = map.try_by_handle(o);
            assert!(vv.is_some());
            assert_eq!(i as u64, vv.unwrap().live_count.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn map_test() {
        let map = HeapProfileMap::<{ size_of::<[u64; 8]>() }, 128, 128>::new();
        let (counters, _handle) = map.try_get(&[0x1122334455667788]);
        assert!(counters.is_some());
        counters.unwrap().allocate(42);

        // No event dropped.
        assert!(equals(&map.data()[..size_of::<Header>()], as_u8(&[0x0000000000000001])));
        // This is not dropping an element.
        assert!(map.try_get(&[0xaabbccdd]).0.is_none());
        // No event dropped.
        assert!(equals(&map.data()[..size_of::<Header>()], as_u8(&[0x0000000000000001])));
        map.event_dropped();
        assert!(equals(&map.data()[..size_of::<Header>()], as_u8(&[0x0000000100000001])));
        map.event_dropped();

        // Event dropped and buffer with trailing zeros.
        assert!(equals(
            map.data(),
            as_u8(&[
                /*header         */ 0x0000000200000001,
                /*live_count     */ 1,
                /*live_bytes     */ 42,
                /*total_count    */ 1,
                /*total_bytes    */ 42,
                /*backtrace_size */ 1,
                /*backtrace      */ 0x1122334455667788,
                /*zeros          */ 0x0000000000000000,
            ])
        ));
    }

    #[test]
    fn alignment_test() {
        let v1 = HeapProfileMap::<24, 2, 2>::new();
        // The buffer is at the same address and same alignment as the HeapProfileMap.
        assert_eq!(core::ptr::from_ref(&v1).cast::<u8>(), v1.data().as_ptr());
    }
}
