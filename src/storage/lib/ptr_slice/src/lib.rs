// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrappers for raw pointer byte slices.
//!
//! This crate provides [`PtrByteSlice`] and [`MutPtrByteSlice`], which are designed for use in
//! scenarios involving cross-process shared memory (e.g., communication with driver processes or
//! other untrusted components).
//!
//! ### Rationale
//!
//! In a multi-process system like Fuchsia, processes often share memory via VMOs (Virtual Memory
//! Objects). If a process shares a memory region with another process, that other process (which
//! may be compromised or untrusted) can modify the memory concurrently at any time.
//!
//! In Rust and LLVM's memory model, two distinct pitfalls arise when interacting with
//! concurrently-modifiable shared memory:
//! 1. Creating a standard Rust reference (`&[u8]` or `&mut [u8]`) over memory that can be modified
//!    concurrently by another party is **Undefined Behavior (UB)**. The Rust compiler assumes that
//!    the data behind a shared reference (`&T`) is immutable and cannot change unexpectedly,
//!    allowing optimizations that assume stability.
//! 2. Even with raw pointers, plain `std::ptr::copy_nonoverlapping` or `std::ptr::read_unaligned`
//!    allows LLVM optimization passes (`MemCpyOpt`, `SROA`, dead-store elimination, or load
//!    rematerialization) to elide a staging copy and re-fetch directly from the shared region after
//!    validation (a compiler-introduced Time-of-Check to Time-of-Use bug), or to propagate `undef`
//!    assumptions past the copy.
//!
//! [`PtrByteSlice`] and [`MutPtrByteSlice`] wrap raw pointers and provide a safe API tailored to
//! each access pattern:
//! 1. **Bulk copies and fills** (`copy_to_slice`, `to_vec`, `append_to`, `copy_from_slice`,
//!    `copy_from_ptr_slice`, `fill`): perform `std::ptr::copy_nonoverlapping` or
//!    `std::ptr::write_bytes` (preserving platform-optimized `memcpy`/`memset` throughput) followed
//!    by a zero-instruction inline-assembly compiler barrier on the destination pointer. Because
//!    the `asm!` block omits `nomem` and `readonly`, LLVM treats the destination buffer as both
//!    read and modified by opaque assembly, preventing `MemCpyOpt`/`SROA` forwarding back to the
//!    shared source, dead-store elimination, and `undef` propagation.
//! 2. **Single typed accesses** (`read`, `write`): perform `std::ptr::read_volatile` and
//!    `std::ptr::write_volatile` through a `#[repr(C, packed)]` wrapper, emitting single unaligned
//!    loads/stores without byte-tearing and preventing load rematerialization or store elimination.
//!    Reads require `T: Copy + FromBytes` so any byte pattern is valid; writes require
//!    `T: Copy + IntoBytes + Immutable` so uninitialized padding bytes are never exposed.
//! 3. **In-place typed iterators** (`iter_as`, `iter_as_mut`, `ElemMut`): perform plain aligned
//!    `std::ptr::read` / `std::ptr::write` for single-pass in-place processing (such as checksums
//!    or encryption) so hot loops retain full auto-vectorization and register allocation. They do
//!    not freeze values against concurrent modification; callers needing TOCTOU protection should
//!    copy data into private memory first or use [`PtrByteSlice::read`].
//!
//! Callers remain responsible for higher-level synchronization and TOCTOU validation.

use std::marker::PhantomData;
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Wrapper with 1-byte alignment so `std::ptr::read_volatile` and `std::ptr::write_volatile` can
/// access arbitrarily aligned `T` values as a single volatile load or store.
#[repr(C, packed)]
struct Unaligned<T>(T);

/// Zero-instruction compiler barrier over memory reachable through `ptr` (emits no CPU fence
/// instructions and constrains only the compiler).
///
/// Omitting `nomem` and `readonly` forces LLVM to treat memory reachable via `ptr` as both read
/// and potentially modified (`ModRef`), preventing dead-store elimination of the preceding copy or
/// fill, preventing `MemCpyOpt`/`SROA` from forwarding later reads back to an untrusted shared
/// source, and acting as a memory-level `freeze` on the destination buffer.
#[inline(always)]
fn compiler_barrier(ptr: *mut u8) {
    // In normal (LLVM) builds, `std::sync::atomic::compiler_fence` is insufficient because LLVM's
    // `BasicAliasAnalysis` treats a `fence` as `NoModRef` on non-escaped local allocations (like a
    // local staging buffer or freshly allocated `Vec`), allowing `MemCpyOpt` and `SROA` to optimize
    // right past it; passing `ptr` as an operand to `asm!` forces LLVM to treat the destination
    // buffer itself as `ModRef`.
    #[cfg(not(miri))]
    {
        // SAFETY: The inline assembly contains only a comment and executes no instructions, so it
        // never dereferences `ptr` and preserves all registers and flags.
        unsafe {
            std::arch::asm!("/* {0} */", in(reg) ptr, options(nostack, preserves_flags));
        }
    }
    // Under `#[cfg(miri)]`, Miri rejects inline `asm!` as unsupported, and because Miri interprets
    // unoptimized MIR directly without running LLVM passes (`MemCpyOpt`, `SROA`, DSE), the `asm!`
    // clobber isn't needed.
    #[cfg(miri)]
    {
        let _ = ptr;
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// A read-only view of a raw pointer byte slice, providing a safe API.
#[derive(Debug, Copy, Clone)]
pub struct PtrByteSlice<'a> {
    slice: *const [u8],
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> PtrByteSlice<'a> {
    /// Creates a new `PtrByteSlice` from a raw pointer to a byte slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure that for the lifetime `'a`:
    /// - `slice` is valid for reads.
    /// - `slice` does not alias or overlap with any active [`MutPtrByteSlice`] (other than one
    ///   immutably borrowed for `'a`, as in [`MutPtrByteSlice::as_ptr_slice`]).
    pub unsafe fn new(slice: *const [u8]) -> Self {
        Self { slice, _marker: PhantomData }
    }

    /// Returns the length of the slice in bytes.
    pub fn len(&self) -> usize {
        self.slice.len()
    }

    /// Returns `true` if the slice has a length of 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads a copy of a value of type `T` from the start of the slice.
    ///
    /// The read is performed as an unaligned volatile load, so the slice does not need to be
    /// aligned to `T` and the compiler will not elide or rematerialize the load from shared memory.
    #[inline]
    pub fn read<T: Copy + FromBytes>(&self) -> Option<T> {
        let size = std::mem::size_of::<T>();
        if size > self.len() {
            return None;
        }
        let ptr = self.as_ptr().cast::<Unaligned<T>>();
        // SAFETY:
        // - `self.as_ptr()` is valid for reads of `size` (`<= self.len()`) bytes (guaranteed by
        //   `Self::new` and the bounds check above).
        // - `Unaligned<T>` is `#[repr(C, packed)]`, so its alignment is 1 and
        //   `size_of::<Unaligned<T>>() == size_of::<T>()`, making `read_volatile` valid at any
        //   alignment.
        // - `T: FromBytes` guarantees that any byte pattern is a valid `T`.
        unsafe { Some(std::ptr::read_volatile(ptr).0) }
    }

    /// Copies the contents of this slice into a safe Rust mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if `dest` is smaller than `self.len()`.
    #[inline]
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        let len = self.len();
        assert!(dest.len() >= len);
        if len == 0 {
            return;
        }
        // SAFETY:
        // - `self.as_ptr()` is valid for reads of `len` bytes (guaranteed by `Self::new`).
        // - `dest` is valid for writes of `len` bytes (ensured by the assert).
        // - The memory regions do not overlap because `dest` is an exclusive Rust reference.
        unsafe {
            std::ptr::copy_nonoverlapping(self.as_ptr(), dest.as_mut_ptr(), len);
        }
        compiler_barrier(dest.as_mut_ptr());
    }

    /// Copies the contents of this slice into `dest` starting at byte offset `vmo_offset` within
    /// `dest`.
    #[cfg(target_os = "fuchsia")]
    pub fn copy_to_vmo(&self, dest: &zx::Vmo, vmo_offset: u64) -> Result<(), zx::Status> {
        // SAFETY: `self.slice` is valid for reads of `self.len()` bytes (guaranteed by `Self::new`
        // safety contract), and `zx_vmo_write` copies directly from the raw pointer in the kernel
        // without creating a Rust slice reference.
        let status = unsafe {
            zx::sys::zx_vmo_write(dest.raw_handle(), self.as_ptr(), vmo_offset, self.len())
        };
        zx::ok(status)
    }

    /// Returns a subslice of this pointer slice.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn subslice(&self, range: std::ops::Range<usize>) -> Self {
        assert!(range.start <= range.end);
        assert!(range.end <= self.len());
        // SAFETY:
        // - `range` is within the bounds of `self.slice` (ensured by asserts).
        // - The original `self.slice` is valid for reads for `'a`, so any subslice of it
        //   is also valid for reads for `'a`.
        unsafe {
            let new_ptr = (self.slice as *const u8).add(range.start);
            let new_slice = std::ptr::slice_from_raw_parts(new_ptr, range.end - range.start);
            Self::new(new_slice)
        }
    }

    /// Splits the slice into two at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `mid` is out of bounds.
    pub fn split_at(self, mid: usize) -> (Self, Self) {
        assert!(mid <= self.len());
        // SAFETY:
        // - `mid` is within the bounds of `self.slice` (ensured by assert).
        // - The two subslices are valid for reads for `'a` as they are parts of the original
        //   valid slice.
        unsafe {
            let ptr = self.slice as *const u8;
            (
                Self::new(std::ptr::slice_from_raw_parts(ptr, mid)),
                Self::new(std::ptr::slice_from_raw_parts(ptr.add(mid), self.len() - mid)),
            )
        }
    }

    /// Returns the raw pointer to the slice.
    pub fn as_raw_slice_ptr(&self) -> *const [u8] {
        self.slice
    }

    /// Returns a raw pointer to the start of the slice.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice as *const u8
    }

    /// Allocates a new heap Vector and copies the contents into it.
    /// Bypasses zero-initialization using raw pointer copies.
    #[inline]
    pub fn to_vec(&self) -> Vec<u8> {
        let len = self.len();
        let mut vec = Vec::with_capacity(len);
        if len > 0 {
            // SAFETY:
            // - `self.as_ptr()` is valid for reads of `len` bytes (guaranteed by `Self::new`).
            // - `vec` has capacity `>= len`, does not overlap `self`, and `copy_nonoverlapping`
            //   initializes `0..len`, satisfying `Vec::set_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(self.as_ptr(), vec.as_mut_ptr(), len);
                compiler_barrier(vec.as_mut_ptr());
                vec.set_len(len);
            }
        }
        vec
    }

    /// Appends the contents of this slice to the given vector, expanding its capacity if needed.
    /// Bypasses zero-initialization using raw pointer copies.
    #[inline]
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let old_len = vec.len();
        let new_len = old_len + len;
        vec.reserve(len);
        // SAFETY:
        // - `vec.reserve(len)` ensures at least `len` bytes of spare capacity, so `dest_ptr` is in
        //   bounds of `vec`'s allocation and valid for writes of `len` bytes.
        // - `self.as_ptr()` is valid for reads of `len` bytes (`Self::new`).
        // - Source and destination do not overlap because `vec` owns its heap allocation
        //   exclusively.
        // - `copy_nonoverlapping` initializes `old_len..new_len`, satisfying `Vec::set_len`.
        unsafe {
            let dest_ptr = vec.as_mut_ptr().add(old_len);
            std::ptr::copy_nonoverlapping(self.as_ptr(), dest_ptr, len);
            compiler_barrier(dest_ptr);
            vec.set_len(new_len);
        }
    }

    /// Returns an iterator over read-only typed elements `T`.
    ///
    /// Performs plain aligned reads intended for single-pass in-place processing (such as checksums
    /// or encryption) rather than freezing values against concurrent mutation.
    ///
    /// # Panics
    ///
    /// Panics if the slice is not aligned to `T` or if its length in bytes is not a multiple of
    /// `size_of::<T>()`.
    pub fn iter_as<T: Copy + FromBytes>(&self) -> IterAs<'_, T> {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        assert!(size > 0, "Chunk size must be greater than 0");
        assert_eq!(self.slice as *const u8 as usize % align, 0, "Slice is not aligned to T");
        assert_eq!(self.len() % size, 0, "Slice length is not a multiple of T size");

        // SAFETY:
        // - `self.slice` is aligned to `T` (ensured by assert).
        // - The end pointer is calculated within the bounds of the original slice.
        // - Pointer arithmetic within the same allocated object is safe.
        let end = unsafe { (self.slice as *const T).add(self.len() / size) };
        IterAs { ptr: self.slice as *const T, end, _marker: PhantomData }
    }

    /// Returns an iterator over byte chunks of up to `chunk_size` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
    pub fn chunks(&self, chunk_size: usize) -> Chunks<'a> {
        assert!(chunk_size > 0, "chunk_size must be > 0");
        Chunks { slice: *self, chunk_size, offset: 0 }
    }
}

/// A mutable view of a raw pointer byte slice, providing a safe API.
#[derive(Debug)]
pub struct MutPtrByteSlice<'a> {
    slice: *mut [u8],
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> MutPtrByteSlice<'a> {
    /// Creates a new `MutPtrByteSlice` from a raw mutable pointer to a byte slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure that for the lifetime `'a`:
    /// - `slice` is valid for reads and writes.
    /// - `slice` does not alias or overlap with any other active `MutPtrByteSlice` or
    ///   [`PtrByteSlice`].
    pub unsafe fn new(slice: *mut [u8]) -> Self {
        Self { slice, _marker: PhantomData }
    }

    /// Returns the length of the slice in bytes.
    pub fn len(&self) -> usize {
        self.slice.len()
    }

    /// Returns `true` if the slice has a length of 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads a copy of a value of type `T` from the start of the slice.
    ///
    /// The read is performed as an unaligned volatile load, so the slice does not need to be
    /// aligned to `T` and the compiler will not elide or rematerialize the load from shared memory.
    #[inline]
    pub fn read<T: Copy + FromBytes>(&self) -> Option<T> {
        self.as_ptr_slice().read()
    }

    /// Writes a value of type `T` to the start of the slice.
    ///
    /// The write is performed as an unaligned volatile store, so the slice does not need to be
    /// aligned to `T` and the compiler will not eliminate the write to shared memory.
    #[inline]
    pub fn write<T: Copy + IntoBytes + Immutable>(&mut self, val: T) -> Option<()> {
        let size = std::mem::size_of::<T>();
        if size > self.len() {
            return None;
        }
        let ptr = self.as_mut_ptr().cast::<Unaligned<T>>();
        // SAFETY:
        // - `self.as_mut_ptr()` is valid for writes of `size` (`<= self.len()`) bytes (guaranteed
        //   by `Self::new` and the bounds check above).
        // - `Unaligned<T>` is `#[repr(C, packed)]`, so its alignment is 1 and
        //   `size_of::<Unaligned<T>>() == size_of::<T>()`, making `write_volatile` valid at any
        //   alignment.
        // - `T: IntoBytes + Immutable` guarantees `val` has no uninitialized padding bytes or
        //   interior mutability.
        unsafe {
            std::ptr::write_volatile(ptr, Unaligned(val));
        }
        Some(())
    }

    /// Copies the contents of this slice into a safe Rust mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if `dest` is smaller than `self.len()`.
    #[inline]
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        self.as_ptr_slice().copy_to_slice(dest);
    }

    /// Copies the contents of another read-only pointer slice into this mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if the lengths of the slices do not match.
    #[inline]
    pub fn copy_from_ptr_slice(&mut self, src: PtrByteSlice<'_>) {
        assert_eq!(self.len(), src.len());
        let len = self.len();
        if len == 0 {
            return;
        }
        // SAFETY:
        // - `self.as_mut_ptr()` is valid for writes of `len` bytes (`Self::new`).
        // - `src.as_ptr()` is valid for reads of `len` (`== src.len()`) bytes (`PtrByteSlice::new`).
        // - The memory regions do not overlap because `Self::new` and `PtrByteSlice::new` require
        //   that an active `MutPtrByteSlice` does not overlap with any other active `PtrByteSlice`
        //   or `MutPtrByteSlice`, and `&mut self` prevents `src` from being borrowed from `self`.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.as_mut_ptr(), len);
        }
        compiler_barrier(self.as_mut_ptr());
    }

    /// Copies the contents of a standard safe slice into this mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if the lengths of the slices do not match.
    #[inline]
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        assert_eq!(self.len(), src.len());
        let len = self.len();
        if len == 0 {
            return;
        }
        // SAFETY:
        // - `self.as_mut_ptr()` is valid for writes of `len` bytes (`Self::new`).
        // - `src` is valid for reads of `len` bytes and cannot overlap `self` (`src` is an
        //   immutable Rust reference while `self` is mutated).
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.as_mut_ptr(), len);
        }
        compiler_barrier(self.as_mut_ptr());
    }

    /// Copies the contents of this slice into `dest` starting at byte offset `vmo_offset` within
    /// `dest`.
    #[cfg(target_os = "fuchsia")]
    pub fn copy_to_vmo(&self, dest: &zx::Vmo, vmo_offset: u64) -> Result<(), zx::Status> {
        self.as_ptr_slice().copy_to_vmo(dest, vmo_offset)
    }

    /// Copies `self.len()` bytes from `src` starting at byte offset `vmo_offset` within `src` into
    /// this mutable slice.
    #[cfg(target_os = "fuchsia")]
    pub fn copy_from_vmo(&mut self, src: &zx::Vmo, vmo_offset: u64) -> Result<(), zx::Status> {
        // SAFETY:
        // - `self.slice` is valid for writes of `self.len()` bytes (guaranteed by `Self::new`
        //   safety contract).
        // - `&mut self` guarantees exclusive access to the memory pointed to by `self.as_mut_ptr()`
        //   for the duration of the call.
        unsafe { src.read_raw(self.as_mut_ptr(), self.len(), vmo_offset) }
    }

    /// Fills the slice with the given byte value.
    #[inline]
    pub fn fill(&mut self, val: u8) {
        let len = self.len();
        if len == 0 {
            return;
        }
        // SAFETY: `self.as_mut_ptr()` is valid for writes of `len` bytes (`Self::new`).
        unsafe {
            std::ptr::write_bytes(self.as_mut_ptr(), val, len);
        }
        compiler_barrier(self.as_mut_ptr());
    }

    /// Returns a read-only view of this slice.
    pub fn as_ptr_slice(&self) -> PtrByteSlice<'_> {
        // SAFETY: `self.slice` is valid for reads for the borrow's lifetime (since it is valid for
        // writes for `'a`), `&self` prevents mutation through `self` for that lifetime, and
        // `Self::new` guarantees no other active `MutPtrByteSlice` overlaps `self.slice`.
        unsafe { PtrByteSlice::new(self.slice as *const [u8]) }
    }

    /// Returns a mutable subslice of this pointer slice.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn subslice_mut(&mut self, range: std::ops::Range<usize>) -> Self {
        assert!(range.start <= range.end);
        assert!(range.end <= self.len());
        // SAFETY:
        // - `range` is within the bounds of `self.slice` (ensured by asserts).
        // - The original `self.slice` is valid for reads and writes for `'a`, so any subslice of it
        //   is also valid for reads and writes for `'a`.
        unsafe {
            let new_ptr = (self.slice as *mut u8).add(range.start);
            let new_slice = std::ptr::slice_from_raw_parts_mut(new_ptr, range.end - range.start);
            Self::new(new_slice)
        }
    }

    /// Splits the slice into two at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `mid` is out of bounds.
    pub fn split_at_mut(self, mid: usize) -> (Self, Self) {
        assert!(mid <= self.len());
        // SAFETY:
        // - `mid` is within the bounds of `self.slice` (ensured by assert).
        // - The two subslices are valid for reads and writes for `'a` as they are parts of the
        //   original valid slice.
        // - They do not overlap.
        unsafe {
            let ptr = self.slice as *mut u8;
            (
                Self::new(std::ptr::slice_from_raw_parts_mut(ptr, mid)),
                Self::new(std::ptr::slice_from_raw_parts_mut(ptr.add(mid), self.len() - mid)),
            )
        }
    }

    /// Returns the raw mutable pointer to the slice.
    pub fn as_raw_mut_slice_ptr(&self) -> *mut [u8] {
        self.slice
    }

    /// Returns a raw pointer to the start of the slice.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice as *const u8
    }

    /// Returns a raw mutable pointer to the start of the slice.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.slice as *mut u8
    }

    /// Reborrows the mutable slice with a shorter lifetime.
    pub fn reborrow(&mut self) -> MutPtrByteSlice<'_> {
        MutPtrByteSlice { slice: self.slice, _marker: std::marker::PhantomData }
    }

    /// Allocates a new heap Vector and copies the contents into it.
    /// Bypasses zero-initialization using raw pointer copies.
    #[inline]
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_ptr_slice().to_vec()
    }

    /// Appends the contents of this slice to the given vector, expanding its capacity if needed.
    /// Bypasses zero-initialization using raw pointer copies.
    #[inline]
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        self.as_ptr_slice().append_to(vec);
    }

    /// Returns an iterator over mutable typed elements `T`.
    ///
    /// Performs plain aligned accesses intended for single-pass in-place processing (such as
    /// encryption) rather than freezing values against concurrent mutation.
    ///
    /// # Panics
    ///
    /// Panics if the slice is not aligned to `T` or if its length in bytes is not a multiple of
    /// `size_of::<T>()`.
    pub fn iter_as_mut<T>(&mut self) -> IterAsMut<'_, T> {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        assert!(size > 0, "Chunk size must be greater than 0");
        assert_eq!(self.slice as *mut u8 as usize % align, 0, "Slice is not aligned to T");
        assert_eq!(self.len() % size, 0, "Slice length is not a multiple of T size");

        // SAFETY:
        // - `self.slice` is aligned to `T` (ensured by assert).
        // - The end pointer is calculated within the bounds of the original slice.
        // - Pointer arithmetic within the same allocated object is safe.
        let end = unsafe { (self.slice as *mut T).add(self.len() / size) };
        IterAsMut { ptr: self.slice as *mut T, end, _marker: PhantomData }
    }

    /// Returns an iterator over mutable byte chunks of up to `chunk_size` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
    pub fn chunks_mut(&mut self, chunk_size: usize) -> ChunksMut<'_> {
        assert!(chunk_size > 0, "chunk_size must be > 0");
        ChunksMut { slice: self.reborrow(), chunk_size, offset: 0 }
    }

    /// Consumes this slice and returns an iterator over mutable byte chunks of up to
    /// `chunk_size` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
    pub fn into_chunks_mut(self, chunk_size: usize) -> ChunksMut<'a> {
        assert!(chunk_size > 0, "chunk_size must be > 0");
        ChunksMut { slice: self, chunk_size, offset: 0 }
    }

    /// Returns an `io::Write` adapter for this slice.
    pub fn writer(self) -> Writer<'a> {
        Writer::new(self)
    }
}

/// An `io::Write` adapter for `MutPtrByteSlice`.
#[derive(Debug)]
pub struct Writer<'a> {
    slice: MutPtrByteSlice<'a>,
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Creates a new writer from a `MutPtrByteSlice`.
    pub fn new(slice: MutPtrByteSlice<'a>) -> Self {
        Self { slice, pos: 0 }
    }

    /// Returns the number of bytes written so far (the current position of the writer).
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Returns the remaining unwritten subslice of the buffer.
    pub fn remaining(&mut self) -> MutPtrByteSlice<'_> {
        let len = self.slice.len();
        self.slice.reborrow().subslice_mut(self.pos..len)
    }

    /// Consumes the writer and returns the subslice containing the data written so far.
    pub fn into_written(mut self) -> MutPtrByteSlice<'a> {
        let pos = self.pos;
        self.slice.subslice_mut(0..pos)
    }
}

impl std::io::Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let remaining = self.slice.len() - self.pos;
        if remaining == 0 {
            return Ok(0);
        }
        let to_write = std::cmp::min(remaining, buf.len());
        self.slice
            .reborrow()
            .subslice_mut(self.pos..self.pos + to_write)
            .copy_from_slice(&buf[..to_write]);
        self.pos += to_write;
        Ok(to_write)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if buf.len() > self.slice.len() - self.pos {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole buffer",
            ));
        }
        self.slice.reborrow().subslice_mut(self.pos..self.pos + buf.len()).copy_from_slice(buf);
        self.pos += buf.len();
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// SAFETY: `PtrByteSlice` is conceptually a read-only view of a byte slice (`&[u8]`).
// It does not allow mutation and does not own the underlying memory.
// It is safe to send it to another thread (`Send`) and share it among threads (`Sync`)
// because the underlying memory is guaranteed to be valid for the lifetime `'a`.
unsafe impl Send for PtrByteSlice<'_> {}
// SAFETY: See comment above.
unsafe impl Sync for PtrByteSlice<'_> {}
// SAFETY: `MutPtrByteSlice` is conceptually a mutable view of a byte slice (`&mut [u8]`).
// It enforces exclusive access because it does not implement `Clone` or `Copy`,
// and all mutating methods require `&mut self` or ownership.
// It is safe to send it to another thread (`Send`) because only one thread can possess it
// at a time.
unsafe impl Send for MutPtrByteSlice<'_> {}
// SAFETY: `MutPtrByteSlice` is safe to share among threads (`Sync`) because it does not
// permit safe concurrent mutation through a shared reference (`&self`).
unsafe impl Sync for MutPtrByteSlice<'_> {}

impl<'a> From<&'a [u8]> for PtrByteSlice<'a> {
    fn from(slice: &'a [u8]) -> Self {
        // SAFETY: A standard Rust reference is guaranteed to be valid for reads.
        unsafe { Self::new(slice as *const [u8]) }
    }
}

impl<'a> From<&'a Vec<u8>> for PtrByteSlice<'a> {
    fn from(vec: &'a Vec<u8>) -> Self {
        Self::from(vec.as_slice())
    }
}

impl<'a> From<MutPtrByteSlice<'a>> for PtrByteSlice<'a> {
    fn from(slice: MutPtrByteSlice<'a>) -> Self {
        // SAFETY: MutPtrByteSlice guarantees the memory is valid for 'a.
        // Since we consume the MutPtrByteSlice, we can safely return a PtrByteSlice with the same
        // lifetime.
        unsafe { Self::new(slice.slice as *const [u8]) }
    }
}

impl<'a> From<&'a mut [u8]> for MutPtrByteSlice<'a> {
    fn from(slice: &'a mut [u8]) -> Self {
        // SAFETY: A standard Rust mutable reference is guaranteed to be valid and exclusive.
        unsafe { Self::new(slice as *mut [u8]) }
    }
}

impl<'a> From<&'a mut Vec<u8>> for MutPtrByteSlice<'a> {
    fn from(vec: &'a mut Vec<u8>) -> Self {
        Self::from(vec.as_mut_slice())
    }
}

/// An iterator over read-only typed elements of a pointer slice.
pub struct IterAs<'a, T> {
    ptr: *const T,
    end: *const T,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: Copy + FromBytes> Iterator for IterAs<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ptr == self.end {
            None
        } else {
            let current = self.ptr;
            // SAFETY: `self.ptr` is less than `self.end` (checked), so adding 1 is within the
            // bounds of the allocation.
            self.ptr = unsafe { self.ptr.add(1) };
            // SAFETY: The pointer is guaranteed to be valid and aligned for `T` since alignment
            // was verified when the iterator was created, and `T: FromBytes` guarantees any byte
            // pattern is valid.
            Some(unsafe { std::ptr::read(current) })
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = unsafe { self.end.offset_from(self.ptr) as usize };
        (len, Some(len))
    }
}

impl<T: Copy + FromBytes> ExactSizeIterator for IterAs<'_, T> {}

/// An iterator over mutable typed elements of a pointer slice.
pub struct IterAsMut<'a, T> {
    ptr: *mut T,
    end: *mut T,
    _marker: PhantomData<&'a mut T>,
}

impl<'a, T> Iterator for IterAsMut<'a, T> {
    type Item = ElemMut<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ptr == self.end {
            None
        } else {
            let current = self.ptr;
            // SAFETY: `self.ptr` is less than `self.end` (checked), so adding 1 is within the
            // bounds of the allocation.
            self.ptr = unsafe { self.ptr.add(1) };
            Some(ElemMut { ptr: current, _marker: PhantomData })
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = unsafe { self.end.offset_from(self.ptr) as usize };
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for IterAsMut<'_, T> {}

/// A mutable typed element of a pointer slice.
pub struct ElemMut<'a, T> {
    ptr: *mut T,
    _marker: PhantomData<&'a mut T>,
}

impl<T> ElemMut<'_, T> {
    /// Returns the raw pointer to the element.
    pub fn as_ptr(&self) -> *mut T {
        self.ptr
    }
}

impl<T: Copy + FromBytes> ElemMut<'_, T> {
    /// Reads the value from the element.
    ///
    /// Since alignment and validity were verified once when the iterator was created,
    /// this access is safe and fast.
    pub fn read(&self) -> T {
        // SAFETY: The pointer is guaranteed to be valid and aligned, and `T: FromBytes` guarantees
        // any byte pattern is valid.
        unsafe { std::ptr::read(self.ptr) }
    }
}

impl<T: Copy + IntoBytes + Immutable> ElemMut<'_, T> {
    /// Writes a value to the element.
    ///
    /// Since alignment and validity were verified once when the iterator was created,
    /// this access is safe and fast.
    pub fn write(&self, val: T) {
        // SAFETY: The pointer is guaranteed to be valid and aligned, and `T: IntoBytes +
        // Immutable` guarantees `val` has no uninitialized padding bytes or interior mutability.
        unsafe { std::ptr::write(self.ptr, val) };
    }
}

/// An iterator over read-only byte chunks of a pointer slice.
#[derive(Debug)]
pub struct Chunks<'a> {
    slice: PtrByteSlice<'a>,
    chunk_size: usize,
    offset: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = PtrByteSlice<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.slice.len() {
            None
        } else {
            let len = std::cmp::min(self.chunk_size, self.slice.len() - self.offset);
            let chunk = self.slice.subslice(self.offset..self.offset + len);
            self.offset += len;
            Some(chunk)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for Chunks<'_> {
    fn len(&self) -> usize {
        let remaining = self.slice.len().saturating_sub(self.offset);
        remaining.div_ceil(self.chunk_size)
    }
}

impl std::iter::FusedIterator for Chunks<'_> {}

/// An iterator over mutable byte chunks of a pointer slice.
#[derive(Debug)]
pub struct ChunksMut<'a> {
    slice: MutPtrByteSlice<'a>,
    chunk_size: usize,
    offset: usize,
}

impl<'a> Iterator for ChunksMut<'a> {
    type Item = MutPtrByteSlice<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.slice.len() {
            None
        } else {
            let len = std::cmp::min(self.chunk_size, self.slice.len() - self.offset);
            let chunk = self.slice.subslice_mut(self.offset..self.offset + len);
            self.offset += len;
            Some(chunk)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for ChunksMut<'_> {
    fn len(&self) -> usize {
        let remaining = self.slice.len().saturating_sub(self.offset);
        if remaining == 0 { 0 } else { remaining.div_ceil(self.chunk_size) }
    }
}

impl std::iter::FusedIterator for ChunksMut<'_> {}

impl std::io::Read for PtrByteSlice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len();
        if remaining == 0 {
            return Ok(0);
        }
        let to_read = std::cmp::min(remaining, buf.len());
        let (a, b) = self.split_at(to_read);
        a.copy_to_slice(&mut buf[..to_read]);
        *self = b;
        Ok(to_read)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        if buf.len() > self.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            ));
        }
        let (a, b) = self.split_at(buf.len());
        a.copy_to_slice(buf);
        *self = b;
        Ok(())
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        let len = self.len();
        self.append_to(buf);
        *self = self.subslice(len..len);
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[derive(Copy, Clone, Debug, FromBytes, IntoBytes, Immutable)]
    #[repr(C, align(4))]
    struct Aligned4(u32);

    #[test]
    fn test_iter_as_success() {
        let bytes = [0u8; 16];
        let slice = PtrByteSlice::from(&bytes[..]);
        let elems = slice.iter_as::<Aligned4>();
        assert_eq!(elems.count(), 4);
    }

    #[test]
    #[should_panic(expected = "Slice is not aligned to T")]
    fn test_iter_as_unaligned_panic() {
        #[repr(C, align(4))]
        struct AligningBuffer {
            buffer: [u8; 17],
        }
        let aligned = AligningBuffer { buffer: [0u8; 17] };
        let slice = PtrByteSlice::from(&aligned.buffer[1..17]);
        let _ = slice.iter_as::<Aligned4>();
    }

    #[test]
    #[should_panic]
    fn test_iter_as_missized_panic() {
        let bytes = [0u8; 15];
        let slice = PtrByteSlice::from(&bytes[..]);
        let _ = slice.iter_as::<Aligned4>();
    }

    #[test]
    fn test_iter_as_mut_success() {
        let mut bytes = [0u8; 16];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        let elems = slice.iter_as_mut::<Aligned4>();
        assert_eq!(elems.count(), 4);
    }

    #[test]
    #[should_panic(expected = "Slice is not aligned to T")]
    fn test_iter_as_mut_unaligned_panic() {
        #[repr(C, align(4))]
        struct AligningBuffer {
            buffer: [u8; 17],
        }
        let mut aligned = AligningBuffer { buffer: [0u8; 17] };
        let mut slice = MutPtrByteSlice::from(&mut aligned.buffer[1..17]);
        let _ = slice.iter_as_mut::<Aligned4>();
    }

    #[test]
    #[should_panic]
    fn test_iter_as_mut_missized_panic() {
        let mut bytes = [0u8; 15];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        let _ = slice.iter_as_mut::<Aligned4>();
    }

    #[test]
    fn test_byte_chunks() {
        let bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let slice = PtrByteSlice::from(&bytes[..]);
        let chunks: Vec<_> = slice.chunks(4).map(|c| c.to_vec()).collect();
        assert_eq!(chunks, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8], vec![9, 10]]);
    }

    #[test]
    fn test_byte_chunks_mut() {
        let mut bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        for mut chunk in slice.chunks_mut(4) {
            chunk.fill(0);
        }
        assert_eq!(bytes, [0u8; 10]);
    }

    #[test]
    fn test_reader() {
        let bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut slice = PtrByteSlice::from(&bytes[..]);
        let mut buf = [0u8; 4];
        assert_eq!(Read::read(&mut slice, &mut buf).unwrap(), 4);
        assert_eq!(buf, [1, 2, 3, 4]);
        assert_eq!(slice.len(), 6);
        let mut rest = Vec::new();
        assert_eq!(slice.read_to_end(&mut rest).unwrap(), 6);
        assert_eq!(rest, [5, 6, 7, 8, 9, 10]);
        assert_eq!(slice.len(), 0);
    }

    #[test]
    fn test_read_success() {
        let bytes = [1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8, 8u8];
        let slice = PtrByteSlice::from(&bytes[..]);

        let val = slice.read::<Aligned4>().unwrap();
        // We use from_ne_bytes to be independent of endianness for the raw bytes comparison,
        // but Aligned4 is just a wrapper around u32.
        assert_eq!(val.0, u32::from_ne_bytes([1, 2, 3, 4]));

        // Unaligned read
        let sub = slice.subslice(1..8);
        let val_unaligned = sub.read::<Aligned4>().unwrap();
        assert_eq!(val_unaligned.0, u32::from_ne_bytes([2, 3, 4, 5]));
    }

    #[test]
    fn test_read_bounds_failure() {
        let bytes = [1u8, 2u8, 3u8];
        let slice = PtrByteSlice::from(&bytes[..]);
        assert!(slice.read::<Aligned4>().is_none());
    }

    #[test]
    fn test_mut_read_write_success() {
        let mut bytes = [0u8; 8];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);

        // Write aligned
        slice.write(Aligned4(0x12345678)).unwrap();
        assert_eq!(slice.read::<Aligned4>().unwrap().0, 0x12345678);

        // Write unaligned
        let mut sub = slice.subslice_mut(1..8);
        sub.write(Aligned4(0xabcdef01)).unwrap();
        assert_eq!(sub.read::<Aligned4>().unwrap().0, 0xabcdef01);

        // Test ElemMut read and write via iter_as_mut
        let mut elems = slice.iter_as_mut::<Aligned4>();
        let elem = elems.next().unwrap();
        elem.write(Aligned4(0x99887766));
        assert_eq!(elem.read().0, 0x99887766);
    }

    #[test]
    fn test_mut_write_bounds_failure() {
        let mut bytes = [0u8; 3];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        assert!(slice.write(Aligned4(0)).is_none());
    }

    #[test]
    fn test_writer() {
        let mut bytes = [0u8; 16];
        let slice = MutPtrByteSlice::from(&mut bytes[..]);
        let mut writer = slice.writer();
        assert_eq!(writer.write(&[1, 2, 3]).unwrap(), 3);
        assert_eq!(writer.position(), 3);
        writer.write_all(&[4, 5, 6, 7]).unwrap();
        assert_eq!(writer.position(), 7);
        assert_eq!(&bytes[..7], &[1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn test_writer_write_all_error() {
        let mut bytes = [0u8; 4];
        let slice = MutPtrByteSlice::from(&mut bytes[..]);
        let mut writer = slice.writer();
        let err = writer.write_all(&[1, 2, 3, 4, 5]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
        assert_eq!(writer.position(), 0);
    }

    #[test]
    fn test_writer_write_full_and_remaining() {
        let mut bytes = [0u8; 4];
        let slice = MutPtrByteSlice::from(&mut bytes[..]);
        let mut writer = slice.writer();
        assert_eq!(writer.write(&[1, 2, 3, 4, 5]).unwrap(), 4);
        assert_eq!(writer.position(), 4);
        assert_eq!(writer.write(&[6]).unwrap(), 0);
        assert_eq!(writer.remaining().len(), 0);
        writer.flush().unwrap();
        let written = writer.into_written();
        assert_eq!(written.len(), 4);
        assert_eq!(&bytes[..], &[1, 2, 3, 4]);
    }

    #[cfg(target_os = "fuchsia")]
    #[test]
    fn test_copy_to_and_from_vmo() {
        let vmo = zx::Vmo::create(64).unwrap();
        let src_bytes = [10u8, 20, 30, 40, 50, 60];
        let ptr_slice = PtrByteSlice::from(&src_bytes[..]);
        ptr_slice.copy_to_vmo(&vmo, 8).unwrap();

        let mut dst_bytes = [0u8; 6];
        let mut mut_ptr_slice = MutPtrByteSlice::from(&mut dst_bytes[..]);
        mut_ptr_slice.copy_from_vmo(&vmo, 8).unwrap();
        assert_eq!(dst_bytes, src_bytes);
    }

    #[test]
    fn test_unaligned_slice_copies_and_fills() {
        let mut backing1 = [0xA5_u8; 128];
        let mut backing2 = [0x5A_u8; 128];
        let src = [0x42_u8; 32];

        // SAFETY: Both backing arrays are 128-byte local arrays valid for 128 bytes.
        let mut s1 = unsafe {
            MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(backing1.as_mut_ptr(), 128))
        };
        let mut s2 = unsafe {
            MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(backing2.as_mut_ptr(), 128))
        };

        // Unaligned subslice fill
        s1.subslice_mut(3..35).fill(0x7F);
        assert_eq!(&backing1[3..35], &[0x7F_u8; 32]);
        assert_eq!(backing1[2], 0xA5);
        assert_eq!(backing1[35], 0xA5);

        // Unaligned copy_from_slice
        s1.subslice_mut(5..37).copy_from_slice(&src);
        assert_eq!(&backing1[5..37], &[0x42_u8; 32]);
        assert_eq!(backing1[4], 0x7F);

        // Unaligned copy_to_slice
        let mut dest = [0u8; 32];
        s1.as_ptr_slice().subslice(5..37).copy_to_slice(&mut dest);
        assert_eq!(&dest[..], &src[..]);

        // Unaligned copy_from_ptr_slice between distinct non-overlapping slices
        s2.subslice_mut(7..39).copy_from_ptr_slice(s1.as_ptr_slice().subslice(5..37));
        assert_eq!(&backing2[7..39], &[0x42_u8; 32]);
        assert_eq!(backing2[6], 0x5A);
        assert_eq!(backing2[39], 0x5A);
    }

    #[test]
    fn test_zst_and_empty_slices() {
        let mut slice = MutPtrByteSlice::from(&mut [][..]);
        assert_eq!(slice.read::<()>(), Some(()));
        assert_eq!(slice.write(()), Some(()));
        slice.fill(0);
        slice.copy_from_slice(&[]);
        slice.copy_from_ptr_slice(PtrByteSlice::from(&[][..]));
        let mut vec = Vec::new();
        slice.append_to(&mut vec);
        assert!(vec.is_empty());
        assert!(slice.to_vec().is_empty());
    }

    #[test]
    fn test_read_write_various_sizes_and_alignments() {
        fn check<T: Copy + PartialEq + std::fmt::Debug + FromBytes + IntoBytes + Immutable>(
            val: T,
        ) {
            const BUF_LEN: usize = 96;
            const GUARD: usize = 16;
            const CANARY: u8 = 0xD7;
            #[repr(C, align(16))]
            struct Buf([u8; BUF_LEN]);

            let size = std::mem::size_of::<T>();
            for off in 0..16 {
                let mut buf = Buf([CANARY; BUF_LEN]);
                let mut slice = MutPtrByteSlice::from(&mut buf.0[..]);
                let mut sub = slice.subslice_mut(GUARD + off..GUARD + off + size);
                assert_eq!(sub.write(val), Some(()));
                assert_eq!(sub.read::<T>(), Some(val), "mismatch at off={off}, size={size}");
                assert!(
                    buf.0[..GUARD + off].iter().all(|&b| b == CANARY),
                    "leading guard clobbered for off={off}, size={size}"
                );
                assert!(
                    buf.0[GUARD + off + size..].iter().all(|&b| b == CANARY),
                    "trailing guard clobbered for off={off}, size={size}"
                );
            }
        }

        check(());
        check(0xA5_u8);
        check(0x1234_u16);
        check([0x11_u8, 0x22, 0x33]);
        check(0x1234_5678_u32);
        check(0x0123_4567_89AB_CDEF_u64);
        check([0x77_u8; 12]);
        check(0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210_u128);
        check([0x3A_u8; 24]);
    }

    #[test]
    fn test_write_only_type() {
        #[derive(Copy, Clone, Debug, PartialEq, IntoBytes, Immutable)]
        #[repr(transparent)]
        struct WriteOnlyFlag(bool);

        let mut bytes = [0u8; 8];
        // SAFETY: `bytes` is an 8-byte local array, valid for a mutable slice of 8 bytes.
        let mut slice = unsafe {
            MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(bytes.as_mut_ptr(), 8))
        };
        assert_eq!(slice.write(WriteOnlyFlag(true)), Some(()));
        assert_eq!(bytes[0], 1);

        let mut elems = slice.iter_as_mut::<WriteOnlyFlag>();
        let elem = elems.next().unwrap();
        elem.write(WriteOnlyFlag(false));
        assert_eq!(bytes[0], 0);
    }
}
