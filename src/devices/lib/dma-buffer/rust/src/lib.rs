// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrappers around pinned and mapped DMA buffers for Fuchsia drivers.
//!
//! This crate separates DMA buffers by sharing role:
//!
//! - [`Buffer`], [`ContiguousBuffer`], and [`PagedBuffer`] model **exclusive / turn-based** DMA
//!   buffers where CPU mutations take `&mut self` and CPU reads take `&self`. Cached mappings use
//!   direct byte copies (paired with userspace `zx_cache_flush` calls under
//!   [`CacheOptions::Enabled`], or elided under [`CacheOptions::Coherent`]), while uncached
//!   (`Device-nGnRnE`) mappings use naturally aligned volatile loads and stores.
//! - [`SharedBuffer`], [`SharedContiguousBuffer`], and [`SharedPagedBuffer`] (obtained via
//!   [`ContiguousBuffer::into_shared`] / [`PagedBuffer::into_shared`]) model **concurrent /
//!   live-DMA** buffers shared across threads or active hardware rings using a single fixed atomic
//!   word granularity (`W`: [`DmaU32`] or [`DmaU64`]) that works on cached, coherent, and
//!   `Device-nGnRnE` mappings without runtime panics.

#![deny(missing_docs)]

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::ptr::{NonNull, with_exposed_provenance_mut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use core::{fmt, slice};
use fuchsia_runtime::vmar_root_self;
use mapped_vmo::Mapping;
use shared_buffer::SharedBuffer as RawSharedBuffer;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx::sys::{ZX_CACHE_FLUSH_DATA, ZX_CACHE_FLUSH_INVALIDATE, zx_cache_flush, zx_paddr_t};
use zx::{
    Bti, BtiOptions, CachePolicy, Pmt, Rights, Status, Vmar, VmarFlags, Vmo, system_get_page_size,
};

mod phys_iter;
pub use phys_iter::PhysIter;

const MAX_DMA_ALIGN: usize = 8;
const UNCACHEABLE_COPY_CHUNK: usize = 256;

const _: () = assert!(align_of::<u64>() <= MAX_DMA_ALIGN);
const _: () = assert!(align_of::<AtomicU32>() == size_of::<u32>());
const _: () = assert!(align_of::<AtomicU64>() == size_of::<u64>());
const _: () = assert!(align_of::<AtomicU64>() <= MAX_DMA_ALIGN);

/// Cache options for DMA buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheOptions {
    /// Disable CPU caching for the DMA buffer (`ZX_CACHE_POLICY_UNCACHED_DEVICE`).
    ///
    /// Use for live DMA rings or shared structures on busses without hardware cache coherency where
    /// sub-cacheline entries share CPU cache lines.
    Disabled,
    /// Enable CPU caching (`ZX_CACHE_POLICY_CACHED`) with userspace `zx_cache_flush` maintenance on
    /// [`Buffer`] accesses and explicit [`Buffer::cache_flush`] / [`SharedBuffer::cache_flush`]
    /// calls.
    #[default]
    Enabled,
    /// Enable CPU caching (`ZX_CACHE_POLICY_CACHED`) for hardware cache-coherent DMA (such as PCIe
    /// on x86_64 or coherent hypervisor virtual devices), eliding userspace `zx_cache_flush`
    /// (`clflush` / `dc civac`) operations while keeping fast cached memory copies.
    Coherent,
}

/// Emits an acquire memory barrier ordering preceding [`DmaU32`] / [`DmaU64`] loads before
/// subsequent memory reads and writes across both CPU threads and non-coherent / `Device-nGnRnE`
/// hardware DMA.
#[inline]
pub fn device_read_barrier() {
    #[cfg(any(not(any(target_arch = "aarch64", target_arch = "riscv64")), miri))]
    core::sync::atomic::fence(Ordering::Acquire);
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    // SAFETY: `dsb ld` is an unprivileged ARMv8-A data synchronization load barrier (and a full
    // compiler memory fence without `nomem`/`readonly`) that does not access registers, the stack,
    // or condition flags; `fence(Ordering::Acquire)` only emits `dmb ishld` (Inner Shareable),
    // whereas `dsb ld` orders preceding loads before subsequent reads and writes across the full
    // system domain (including `Device-nGnRnE` DMA) without emitting a redundant `dmb ishld`.
    unsafe {
        core::arch::asm!("dsb ld", options(nostack, preserves_flags));
    }
    #[cfg(all(target_arch = "riscv64", not(miri)))]
    // SAFETY: `fence ir,iorw` is an unprivileged RISC-V barrier (and a full compiler memory fence
    // without `nomem`/`readonly`) that does not access registers, the stack, or flags; unlike
    // `fence r,rw`, `fence ir,iorw` also orders Svpbmt `IO` (`CacheOptions::Disabled`) loads.
    unsafe {
        core::arch::asm!("fence ir,iorw", options(nostack, preserves_flags));
    }
}

/// Emits a release memory barrier ordering preceding memory reads and writes before subsequent
/// [`DmaU32`] / [`DmaU64`] stores (or MMIO doorbell writes) across both CPU threads and
/// non-coherent / `Device-nGnRnE` hardware DMA.
#[inline]
pub fn device_write_barrier() {
    #[cfg(any(not(any(target_arch = "aarch64", target_arch = "riscv64")), miri))]
    core::sync::atomic::fence(Ordering::Release);
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    // SAFETY: `dsb sy` is an unprivileged ARMv8-A data synchronization barrier (and a full compiler
    // memory fence without `nomem`/`readonly`) that does not access registers, the stack, or
    // condition flags; `fence(Ordering::Release)` only emits `dmb ish` (Inner Shareable) and
    // `dsb st` only orders `W -> W`, whereas `dsb sy` orders both preceding reads and preceding
    // writes (`R,W -> W`) across the full system domain without emitting a redundant `dmb ish`.
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
    #[cfg(all(target_arch = "riscv64", not(miri)))]
    // SAFETY: `fence iorw,ow` is an unprivileged RISC-V barrier (and a full compiler memory fence
    // without `nomem`/`readonly`) that does not access registers, the stack, or flags; unlike
    // `fence rw,w`, `fence iorw,ow` also orders Svpbmt `IO` (`CacheOptions::Disabled`) accesses.
    unsafe {
        core::arch::asm!("fence iorw,ow", options(nostack, preserves_flags));
    }
}

macro_rules! impl_dma_word {
    ($(#[$meta:meta])* $Name:ident, $Atomic:ty, $Prim:ty, $WIDTH:literal) => {
        $(#[$meta])*
        #[repr(transparent)]
        pub struct $Name($Atomic);

        const _: () = assert!(
            size_of::<$Name>() == size_of::<$Prim>() && align_of::<$Name>() == size_of::<$Prim>()
        );

        impl fmt::Debug for $Name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($Name)).field(&self.load_relaxed()).finish()
            }
        }

        impl $Name {
            /// Loads the value with [`Ordering::Relaxed`].
            #[inline]
            pub fn load_relaxed(&self) -> $Prim {
                self.0.load(Ordering::Relaxed)
            }

            /// Loads the value and orders it before subsequent memory accesses via
            /// [`device_read_barrier`].
            #[inline]
            pub fn load_acquire(&self) -> $Prim {
                let val = self.load_relaxed();
                device_read_barrier();
                val
            }

            /// Stores `val` with [`Ordering::Relaxed`].
            #[inline]
            pub fn store_relaxed(&self, val: $Prim) {
                self.0.store(val, Ordering::Relaxed);
            }

            /// Orders preceding memory accesses via [`device_write_barrier`] and stores `val`.
            #[inline]
            pub fn store_release(&self, val: $Prim) {
                device_write_barrier();
                self.store_relaxed(val);
            }
        }

        impl sealed::Word for $Name {
            const ALIGN_OK: () =
                assert!(size_of::<Self>() == $WIDTH && align_of::<Self>() <= MAX_DMA_ALIGN);

            fn load_to_uninit(cells: &[Self], dst: &mut [MaybeUninit<u8>]) {
                let (chunks, rem) = dst.as_chunks_mut::<$WIDTH>();
                debug_assert!(rem.is_empty() && chunks.len() == cells.len());
                for (chunk, cell) in chunks.iter_mut().zip(cells) {
                    *chunk = cell.load_relaxed().to_ne_bytes().map(MaybeUninit::new);
                }
            }

            fn store_from_bytes(cells: &[Self], src: &[u8]) {
                let (chunks, rem) = src.as_chunks::<$WIDTH>();
                debug_assert!(rem.is_empty() && chunks.len() == cells.len());
                for (cell, chunk) in cells.iter().zip(chunks) {
                    cell.store_relaxed(<$Prim>::from_ne_bytes(*chunk));
                }
            }
        }

        impl DmaWord for $Name {}
    };
}

impl_dma_word!(
    /// A 32-bit cell in shared DMA memory supporting tearing-free atomic loads and stores on
    /// cached, coherent, and [`CacheOptions::Disabled`] (`Device-nGnRnE`) mappings.
    ///
    /// Unlike [`AtomicU32`], `DmaU32` omits read-modify-write operations and `ldar`/`stlr`
    /// instructions that fault on ARM64 `Device-nGnRnE` mappings, implementing acquire/release
    /// ordering via relaxed atomic accesses paired with [`device_read_barrier`] /
    /// [`device_write_barrier`].
    DmaU32,
    AtomicU32,
    u32,
    4
);

impl_dma_word!(
    /// A 64-bit cell in shared DMA memory supporting tearing-free atomic loads and stores on
    /// cached, coherent, and [`CacheOptions::Disabled`] (`Device-nGnRnE`) mappings.
    ///
    /// Unlike [`AtomicU64`], `DmaU64` omits read-modify-write operations and `ldar`/`stlr`
    /// instructions that fault on ARM64 `Device-nGnRnE` mappings, implementing acquire/release
    /// ordering via relaxed atomic accesses paired with [`device_read_barrier`] /
    /// [`device_write_barrier`].
    DmaU64,
    AtomicU64,
    u64,
    8
);

mod sealed {
    use core::mem::MaybeUninit;
    use zx::Status;

    pub trait Word: Sized {
        const ALIGN_OK: ();
        fn load_to_uninit(cells: &[Self], dst: &mut [MaybeUninit<u8>]);
        fn store_from_bytes(cells: &[Self], src: &[u8]);
    }

    pub trait BufferRole {
        /// # Safety
        ///
        /// Hardware DMA to the pinned memory must have ceased.
        unsafe fn unpin_role(self) -> Result<(), Status>;
    }
}

/// Marker trait for fixed-width atomic DMA cells ([`DmaU32`] and [`DmaU64`]) backing a
/// [`SharedBuffer`].
///
/// Fixing a single atomic cell granularity `W` per [`SharedBuffer<W>`] statically prevents
/// concurrent mixed-width atomic accesses on overlapping bytes across threads.
pub trait DmaWord: sealed::Word + Send + Sync {}

struct AssertWordMultiple<T, W>(PhantomData<(T, W)>);

impl<T, W> AssertWordMultiple<T, W> {
    const OK: () = assert!(
        size_of::<T>().is_multiple_of(size_of::<W>()),
        "type size must be a multiple of SharedBuffer word size"
    );
}

/// Writes `len` bytes from `src` (or zeros when `src` is `None`) into exclusively owned uncached
/// DMA memory at `dst` using naturally aligned volatile stores (`1 -> 2 -> 4 -> 8` head, `8` bulk,
/// `4 -> 2 -> 1` tail) so 2-byte/4-byte fields never tear and ARM64 `Device-nGnRnE` mappings never
/// fault on unaligned stores or `dc zva`.
///
/// # Safety
///
/// - `dst` must carry read/write provenance over `dst..dst.add(len)` within a live
///   `PERM_READ | PERM_WRITE` mapping (`len <= isize::MAX as usize`, no `dst.addr() + len`
///   overflow), with no concurrent reads or writes overlapping `dst..dst.add(len)`.
/// - If `src` is `Some(p)`, `p..p.add(len)` must be valid for reads of initialized bytes, disjoint
///   from `dst..dst.add(len)`, with no `p.addr() + len` overflow.
unsafe fn volatile_write_raw(dst: *mut u8, src: Option<*const u8>, len: usize) {
    let mut pos = 0;
    macro_rules! step {
        ($T:ty, $cond:expr) => {
            if len - pos >= size_of::<$T>() && $cond {
                let p_dst = dst.add(pos);
                debug_assert_eq!(p_dst.addr() & (align_of::<$T>() - 1), 0);
                let v = src.map_or(0, |p| p.add(pos).cast::<$T>().read_unaligned());
                p_dst.cast::<$T>().write_volatile(v);
                pos += size_of::<$T>();
            }
        };
    }
    // SAFETY: Each step checks `len - pos >= W` (`pos + W <= len <= isize::MAX`), prior head steps
    // establish `((dst.addr() + pos) & (W - 1)) == 0`, the caller guarantees exclusive access to
    // `dst..dst.add(len)`, and `read_unaligned` allows arbitrary alignment on `src`.
    unsafe {
        step!(u8, ((dst.addr() + pos) & 1) != 0);
        step!(u16, ((dst.addr() + pos) & 2) != 0);
        step!(u32, ((dst.addr() + pos) & 4) != 0);
        while len - pos >= 8 {
            step!(u64, true);
        }
        step!(u32, true);
        step!(u16, true);
        if pos < len {
            let v = src.map_or(0, |p| p.add(pos).read());
            dst.add(pos).write_volatile(v);
        }
    }
}

/// Reads `len` bytes from uncached DMA memory at `src` into normal memory (potentially
/// uninitialized) at `dst` using naturally aligned volatile loads (`1 -> 2 -> 4 -> 8` head, `8`
/// bulk, `4 -> 2 -> 1` tail).
///
/// # Safety
///
/// - `src` must carry read provenance over `src..src.add(len)` within a live `PERM_READ` mapping of
///   initialized bytes (`len <= isize::MAX as usize`, no `src.addr() + len` overflow), with no
///   concurrent writes overlapping `src..src.add(len)`.
/// - `dst..dst.add(len)` must be valid for writes of `len` bytes, disjoint from
///   `src..src.add(len)`, with no `dst.addr() + len` overflow.
unsafe fn volatile_read_raw(src: *const u8, dst: *mut u8, len: usize) {
    let mut pos = 0;
    macro_rules! step {
        ($T:ty, $cond:expr) => {
            if len - pos >= size_of::<$T>() && $cond {
                let p_src = src.add(pos);
                debug_assert_eq!(p_src.addr() & (align_of::<$T>() - 1), 0);
                dst.add(pos).cast::<$T>().write_unaligned(p_src.cast::<$T>().read_volatile());
                pos += size_of::<$T>();
            }
        };
    }
    // SAFETY: Each step checks `len - pos >= W` (`pos + W <= len <= isize::MAX`), prior head steps
    // establish `((src.addr() + pos) & (W - 1)) == 0`, the caller guarantees no concurrent writes
    // to `src..src.add(len)`, and `write_unaligned` allows arbitrary alignment and uninitialized
    // destination bytes on `dst`.
    unsafe {
        step!(u8, ((src.addr() + pos) & 1) != 0);
        step!(u16, ((src.addr() + pos) & 2) != 0);
        step!(u32, ((src.addr() + pos) & 4) != 0);
        while len - pos >= 8 {
            step!(u64, true);
        }
        step!(u32, true);
        step!(u16, true);
        if pos < len {
            dst.add(pos).write(src.add(pos).read_volatile());
        }
    }
}

/// Role-neutral VMAR mapping, VMO, pinned memory token, and cache-maintenance state shared between
/// [`Buffer`] and [`SharedBuffer`].
#[derive(Debug)]
struct RawBuffer {
    vmar: Vmar,
    // Safety invariant: `ptr` is non-null, aligned to at least `MAX_DMA_ALIGN` (8 bytes), and
    // points with exposed read/write provenance to the base of a live, non-resizable
    // `PERM_READ | PERM_WRITE` mapping of `size` zero-initialized bytes
    // (`0 < size <= isize::MAX as usize`, `ptr.addr() + size <= usize::MAX`) in `vmar` until
    // `Drop::drop` runs.
    ptr: NonNull<u8>,
    size: usize,
    vmo: Vmo,
    pmt: Option<Pmt>,
    cache_options: CacheOptions,
}

// SAFETY: `Vmar`, `Vmo`, `Option<Pmt>`, `usize`, and `CacheOptions` are all `Send`, and the VMAR
// mapping at `[ptr, ptr + size)` resides in the process-wide address space so ownership (including
// unmapping in `Drop` and unpinning via `unpin`) can safely transfer across threads.
unsafe impl Send for RawBuffer {}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        // SAFETY: `[self.ptr, self.ptr + self.size)` is the live mapping created in
        // `RawBuffer::new`, `&mut self` guarantees all borrows from `self` have ended, and nothing
        // accesses `[self.ptr, self.ptr + self.size)` after `drop`.
        unsafe {
            let _ = self.vmar.unmap(self.ptr.as_ptr().addr(), self.size);
        }
    }
}

impl RawBuffer {
    fn round_up_size(size: usize) -> Result<(usize, usize), Status> {
        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }
        let page_size = system_get_page_size() as usize;
        let mapped_size = size
            .checked_next_multiple_of(page_size)
            .filter(|&s| s <= isize::MAX as usize)
            .ok_or(Status::OUT_OF_RANGE)?;
        Ok((mapped_size, page_size))
    }

    fn new(
        vmar: &Vmar,
        vmo: Vmo,
        mapped_size: usize,
        bti: &Bti,
        bti_options: BtiOptions,
        phys: &mut [zx_paddr_t],
        cache_options: CacheOptions,
    ) -> Result<Self, Status> {
        debug_assert!(mapped_size > 0 && mapped_size <= isize::MAX as usize);
        if cache_options == CacheOptions::Disabled {
            vmo.set_cache_policy(CachePolicy::UnCachedDevice)?;
        }
        let owned_vmar = vmar.duplicate_handle(Rights::SAME_RIGHTS)?;
        let addr = owned_vmar.map(
            0,
            &vmo,
            0,
            mapped_size,
            VmarFlags::PERM_READ
                | VmarFlags::PERM_WRITE
                | VmarFlags::MAP_RANGE
                | VmarFlags::REQUIRE_NON_RESIZABLE,
        )?;
        let Some(ptr) = NonNull::new(with_exposed_provenance_mut::<u8>(addr)).filter(|_| {
            addr.is_multiple_of(MAX_DMA_ALIGN) && addr.checked_add(mapped_size).is_some()
        }) else {
            // SAFETY: `owned_vmar.map` just created `[addr, addr + mapped_size)` with no aliases.
            unsafe {
                let _ = owned_vmar.unmap(addr, mapped_size);
            }
            return Err(Status::INTERNAL);
        };
        let mut raw =
            Self { vmar: owned_vmar, ptr, size: mapped_size, vmo, pmt: None, cache_options };
        // If `bti.pin` fails, dropping `raw` automatically unmaps `[ptr, ptr + size)`.
        raw.pmt = Some(bti.pin(bti_options, &raw.vmo, 0, mapped_size as u64, phys)?);
        Ok(raw)
    }

    /// # Safety
    ///
    /// Hardware DMA to the pinned memory must have ceased.
    unsafe fn unpin(mut self) -> Result<(), Status> {
        let pmt = self.pmt.take().expect("RawBuffer holds a pinned memory token until unpin");
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { pmt.unpin() }
    }

    fn cache_op(&self, offset: usize, length: usize, options: u32) -> Result<(), Status> {
        if !offset.checked_add(length).is_some_and(|end| end <= self.size) {
            return Err(Status::OUT_OF_RANGE);
        }
        if length == 0 || self.cache_options != CacheOptions::Enabled {
            return Ok(());
        }
        // SAFETY: `offset + length <= self.size <= isize::MAX as usize` was checked above over a
        // live readable/writable mapping; cleaning before invalidating preserves shared lines.
        Status::ok(unsafe {
            zx_cache_flush(self.ptr.as_ptr().add(offset).cast_const(), length, options)
        })
    }
}

/// Exclusive / turn-based DMA buffer providing userspace cache maintenance, pinned memory token
/// lifecycle, `&mut self` CPU writes, and `&self` CPU reads over a non-resizable [`Vmar`] mapping.
///
/// For live DMA rings or descriptor tables accessed concurrently across threads or while hardware
/// DMA is actively mutating the buffer, convert [`ContiguousBuffer`] or [`PagedBuffer`] into
/// [`SharedContiguousBuffer`] or [`SharedPagedBuffer`] via [`ContiguousBuffer::into_shared`] /
/// [`PagedBuffer::into_shared`].
#[derive(Debug)]
pub struct Buffer {
    // Safety invariant: `Buffer` hands out no `&` or `&mut` references into
    // `[inner.ptr, inner.ptr + inner.size)` and mutates it only via `&mut self`.
    inner: RawBuffer,
}

// SAFETY: `RawBuffer` fields (`Vmar`, `Vmo`, `Option<Pmt>`, `usize`, `CacheOptions`) are `Sync`.
// Every `&self` method on `Buffer` only performs raw-pointer reads or cache maintenance on
// `[inner.ptr, inner.ptr + inner.size)` without creating Rust references into the mapping, while
// all CPU writes and unmapping require `&mut self`.
unsafe impl Sync for Buffer {}

impl sealed::BufferRole for Buffer {
    unsafe fn unpin_role(self) -> Result<(), Status> {
        // SAFETY: Forwarded from `BufferRole::unpin_role` safety contract.
        unsafe { self.unpin() }
    }
}

impl Buffer {
    /// Returns the size of the mapped DMA buffer in bytes.
    pub fn size(&self) -> usize {
        self.inner.size
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &Vmo {
        &self.inner.vmo
    }

    /// Returns the [`CacheOptions`] configured for this DMA buffer.
    pub fn cache_options(&self) -> CacheOptions {
        self.inner.cache_options
    }

    /// Unpins the DMA buffer's physical memory token, consuming the buffer.
    ///
    /// Note that `Drop` does not automatically unpin the buffer (instead allowing the kernel to
    /// quarantine the pages on the BTI if dropped while still pinned). Call this method after
    /// ensuring hardware DMA has ceased.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.inner.unpin() }
    }

    /// Flushes CPU data cache lines (clean to memory) for `[offset, offset + length)` via the
    /// userspace `zx_cache_flush` vDSO call (`DMA_TO_DEVICE`) when [`CacheOptions::Enabled`].
    pub fn cache_flush(&self, offset: usize, length: usize) -> Result<(), Status> {
        self.inner.cache_op(offset, length, ZX_CACHE_FLUSH_DATA)
    }

    /// Flushes and invalidates CPU data cache lines for `[offset, offset + length)` via the
    /// userspace `zx_cache_flush` vDSO call (`DMA_FROM_DEVICE`) when [`CacheOptions::Enabled`].
    pub fn cache_flush_invalidate(&self, offset: usize, length: usize) -> Result<(), Status> {
        self.inner.cache_op(offset, length, ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE)
    }

    /// Zeros `[offset, offset + length)` in place and flushes the CPU cache when
    /// [`CacheOptions::Enabled`].
    pub fn zero(&mut self, offset: usize, length: usize) -> Result<(), Status> {
        if !offset.checked_add(length).is_some_and(|end| end <= self.inner.size) {
            return Err(Status::OUT_OF_RANGE);
        }
        if length == 0 {
            return Ok(());
        }
        if self.inner.cache_options != CacheOptions::Disabled {
            // SAFETY: `offset + length <= self.inner.size <= isize::MAX as usize` over a live
            // cached R/W mapping with exclusive `&mut self` access.
            unsafe {
                core::ptr::write_bytes(self.inner.ptr.as_ptr().add(offset), 0, length);
            }
            self.cache_flush(offset, length)
        } else {
            // SAFETY: `offset + length <= self.inner.size <= isize::MAX as usize` over a live R/W
            // mapping with exclusive `&mut self` access.
            unsafe {
                volatile_write_raw(self.inner.ptr.as_ptr().add(offset), None, length);
            }
            Ok(())
        }
    }

    /// Writes `src` into the DMA buffer at byte `offset` and flushes the CPU cache when
    /// [`CacheOptions::Enabled`].
    pub fn write(&mut self, offset: usize, src: &[u8]) -> Result<(), Status> {
        if !offset.checked_add(src.len()).is_some_and(|end| end <= self.inner.size) {
            return Err(Status::OUT_OF_RANGE);
        }
        if src.is_empty() {
            return Ok(());
        }
        if self.inner.cache_options != CacheOptions::Disabled {
            // SAFETY: `offset + src.len() <= self.inner.size <= isize::MAX as usize` over a live
            // cached R/W mapping with exclusive `&mut self` access, disjoint from the normal-memory
            // slice `src` (since `Buffer` never yields references into its mapping).
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    self.inner.ptr.as_ptr().add(offset),
                    src.len(),
                );
            }
            self.cache_flush(offset, src.len())
        } else {
            // SAFETY: `offset + src.len() <= self.inner.size <= isize::MAX as usize` over a live
            // R/W mapping with exclusive `&mut self` access, disjoint from `src`.
            unsafe {
                volatile_write_raw(
                    self.inner.ptr.as_ptr().add(offset),
                    Some(src.as_ptr()),
                    src.len(),
                );
            }
            Ok(())
        }
    }

    /// Reads `len` bytes at `offset` into `dst`, invalidating the CPU cache first when
    /// [`CacheOptions::Enabled`].
    ///
    /// # Safety
    ///
    /// `dst` must be valid for writes of `len` bytes (`len == 0` requires no dereferenceable
    /// range), disjoint from `[self.inner.ptr, self.inner.ptr + self.inner.size)`, with no
    /// `dst.addr() + len` overflow.
    unsafe fn read_raw(&self, offset: usize, dst: *mut u8, len: usize) -> Result<(), Status> {
        if !offset.checked_add(len).is_some_and(|end| end <= self.inner.size) {
            return Err(Status::OUT_OF_RANGE);
        }
        if len == 0 {
            return Ok(());
        }
        if self.inner.cache_options != CacheOptions::Disabled {
            self.cache_flush_invalidate(offset, len)?;
            // SAFETY: `offset + len <= self.inner.size <= isize::MAX as usize` over a live cached
            // R/W mapping of initialized bytes with no concurrent Rust writes (`&self` on
            // `Buffer`), and the caller guarantees `dst..dst.add(len)` is valid and disjoint.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.inner.ptr.as_ptr().add(offset).cast_const(),
                    dst,
                    len,
                );
            }
        } else {
            // SAFETY: `offset + len <= self.inner.size <= isize::MAX as usize` over a live R/W
            // mapping of initialized bytes with no concurrent Rust writes (`&self` on `Buffer`),
            // and the caller guarantees `dst..dst.add(len)` is valid and disjoint.
            unsafe {
                volatile_read_raw(self.inner.ptr.as_ptr().add(offset).cast_const(), dst, len);
            }
        }
        Ok(())
    }

    /// Invalidates the CPU cache when [`CacheOptions::Enabled`] and reads from the DMA buffer at
    /// byte `offset` into `dst`.
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> Result<(), Status> {
        // SAFETY: `dst` is an exclusive normal-memory slice valid for `dst.len()` writes and
        // disjoint from `[self.inner.ptr, self.inner.ptr + self.inner.size)` because `Buffer` hands
        // out no references into its mapping.
        unsafe { self.read_raw(offset, dst.as_mut_ptr(), dst.len()) }
    }

    /// Writes `val` at byte `offset` and flushes the CPU cache when [`CacheOptions::Enabled`].
    pub fn write_struct<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        val: &T,
    ) -> Result<(), Status> {
        self.write(offset, val.as_bytes())
    }

    /// Writes `vals` contiguously starting at byte `offset` and flushes the CPU cache once when
    /// [`CacheOptions::Enabled`].
    pub fn write_slice<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        vals: &[T],
    ) -> Result<(), Status> {
        self.write(offset, vals.as_bytes())
    }

    /// Reads a value of type `T` from byte `offset`, invalidating the CPU cache first when
    /// [`CacheOptions::Enabled`].
    pub fn read_struct<T: FromBytes>(&self, offset: usize) -> Result<T, Status> {
        let mut val = MaybeUninit::<T>::uninit();
        // SAFETY: `val` is a stack-local `MaybeUninit<T>` valid for `size_of::<T>()` writes and
        // disjoint from `[self.inner.ptr, self.inner.ptr + self.inner.size)`.
        unsafe { self.read_raw(offset, val.as_mut_ptr().cast::<u8>(), size_of::<T>())? };
        // SAFETY: `read_raw` succeeded, so either `size_of::<T>() == 0` or all `size_of::<T>()`
        // bytes of `val` were initialized from initialized VMO memory, and `T: FromBytes` accepts
        // every initialized byte pattern.
        Ok(unsafe { val.assume_init() })
    }

    /// Writes `val` as the `index`-th element of type `T` (at byte offset `index * size_of::<T>()`)
    /// and flushes the CPU cache when [`CacheOptions::Enabled`].
    pub fn write_at<T: IntoBytes + Immutable>(
        &mut self,
        index: usize,
        val: &T,
    ) -> Result<(), Status> {
        let offset = index.checked_mul(size_of::<T>()).ok_or(Status::OUT_OF_RANGE)?;
        self.write_struct(offset, val)
    }

    /// Reads the `index`-th element of type `T` (at byte offset `index * size_of::<T>()`) with
    /// automatic cache invalidation when [`CacheOptions::Enabled`].
    pub fn read_at<T: FromBytes>(&self, index: usize) -> Result<T, Status> {
        let offset = index.checked_mul(size_of::<T>()).ok_or(Status::OUT_OF_RANGE)?;
        self.read_struct(offset)
    }

    /// Copies `len` bytes from `[src_offset, src_offset + len)` in this DMA buffer into `dst` at
    /// `dst_offset`, invalidating the CPU cache first when [`CacheOptions::Enabled`].
    ///
    /// For cached mappings ([`CacheOptions::Enabled`] and [`CacheOptions::Coherent`]), performs a
    /// direct raw-pointer copy via [`RawSharedBuffer`]. For [`CacheOptions::Disabled`], falls back
    /// to aligned volatile reads for `Device-nGnRnE` safety.
    pub fn copy_to_mapping(
        &self,
        src_offset: usize,
        dst: &mut Mapping,
        dst_offset: usize,
        len: usize,
    ) -> Result<(), Status> {
        if !src_offset.checked_add(len).is_some_and(|end| end <= self.inner.size)
            || !dst_offset.checked_add(len).is_some_and(|end| end <= dst.len())
        {
            return Err(Status::OUT_OF_RANGE);
        }
        if len == 0 {
            return Ok(());
        }
        if self.inner.cache_options != CacheOptions::Disabled {
            self.cache_flush_invalidate(src_offset, len)?;
            // SAFETY: `src_offset + len <= self.inner.size <= isize::MAX as usize` over a live R/W
            // VMAR mapping; `&self` on `Buffer` precludes any Rust references or concurrent Rust
            // writes to `[self.inner.ptr, self.inner.ptr + self.inner.size)`, and `RawSharedBuffer`
            // does not unmap on drop.
            let src_shared =
                unsafe { RawSharedBuffer::new(self.inner.ptr.as_ptr().add(src_offset), len) };
            let copied = dst.copy_from_at(dst_offset, &src_shared, 0, len);
            debug_assert_eq!(copied, len);
        } else {
            let mut buf = [0u8; UNCACHEABLE_COPY_CHUNK];
            for off in (0..len).step_by(UNCACHEABLE_COPY_CHUNK) {
                let step = (len - off).min(UNCACHEABLE_COPY_CHUNK);
                self.read(src_offset + off, &mut buf[..step])?;
                let written = dst.write_at(dst_offset + off, &buf[..step]);
                debug_assert_eq!(written, step);
            }
        }
        Ok(())
    }

    /// Copies `len` bytes from `src` at `src_offset` into `[dst_offset, dst_offset + len)` in this
    /// DMA buffer, flushing the CPU cache afterward when [`CacheOptions::Enabled`].
    ///
    /// For cached mappings ([`CacheOptions::Enabled`] and [`CacheOptions::Coherent`]), performs a
    /// direct raw-pointer copy via [`RawSharedBuffer`]. For [`CacheOptions::Disabled`], falls back
    /// to aligned volatile writes for `Device-nGnRnE` safety.
    pub fn copy_from_mapping(
        &mut self,
        dst_offset: usize,
        src: &Mapping,
        src_offset: usize,
        len: usize,
    ) -> Result<(), Status> {
        if !dst_offset.checked_add(len).is_some_and(|end| end <= self.inner.size)
            || !src_offset.checked_add(len).is_some_and(|end| end <= src.len())
        {
            return Err(Status::OUT_OF_RANGE);
        }
        if len == 0 {
            return Ok(());
        }
        if self.inner.cache_options != CacheOptions::Disabled {
            // SAFETY: `dst_offset + len <= self.inner.size <= isize::MAX as usize` over a live R/W
            // VMAR mapping; `&mut self` precludes any Rust aliases, and `RawSharedBuffer` does not
            // unmap on drop.
            let dst_shared =
                unsafe { RawSharedBuffer::new(self.inner.ptr.as_ptr().add(dst_offset), len) };
            let copied = dst_shared.copy_from_at(0, &**src, src_offset, len);
            debug_assert_eq!(copied, len);
            drop(dst_shared);
            self.cache_flush(dst_offset, len)
        } else {
            let mut buf = [0u8; UNCACHEABLE_COPY_CHUNK];
            for off in (0..len).step_by(UNCACHEABLE_COPY_CHUNK) {
                let step = (len - off).min(UNCACHEABLE_COPY_CHUNK);
                let read = src.read_at(src_offset + off, &mut buf[..step]);
                debug_assert_eq!(read, step);
                self.write(dst_offset + off, &buf[..step])?;
            }
            Ok(())
        }
    }
}

/// Concurrent / live-DMA buffer backed by fixed-width atomic DMA cells (`W`: [`DmaU32`] by
/// default, or [`DmaU64`]).
///
/// Every CPU load and store on `SharedBuffer<W>` goes exclusively through `W`-aligned relaxed
/// atomic accesses (without implicit `zx_cache_flush`), preventing both atomic-vs-non-atomic data
/// races and mixed-width atomic UB across threads while remaining fault-free on cached, coherent,
/// and [`CacheOptions::Disabled`] (`Device-nGnRnE`) mappings. Callers on non-cache-coherent busses
/// using [`CacheOptions::Enabled`] must pair accesses with explicit [`Self::cache_flush`] or
/// [`Self::cache_flush_invalidate`] calls on cache-line-disjoint regions.
pub struct SharedBuffer<W = DmaU32> {
    // Safety invariant: all CPU memory accesses to `[inner.ptr, inner.ptr + inner.size)` for the
    // lifetime of `SharedBuffer<W>` occur exclusively via `W` atomic cells (and optional cache
    // maintenance).
    inner: RawBuffer,
    _word: PhantomData<W>,
}

// SAFETY: `RawBuffer` fields (`Vmar`, `Vmo`, `Option<Pmt>`, `usize`, `CacheOptions`) are `Sync`,
// `W: DmaWord` is `Send + Sync`, and every `&self` memory access on `SharedBuffer<W>` goes
// exclusively through fixed-width atomic cells `W` (`DmaU32` or `DmaU64`).
unsafe impl<W: DmaWord> Sync for SharedBuffer<W> {}

impl<W> fmt::Debug for SharedBuffer<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBuffer").field("inner", &self.inner).finish()
    }
}

impl<W: DmaWord> sealed::BufferRole for SharedBuffer<W> {
    unsafe fn unpin_role(self) -> Result<(), Status> {
        // SAFETY: Forwarded from `BufferRole::unpin_role` safety contract.
        unsafe { self.unpin() }
    }
}

impl<W: DmaWord> SharedBuffer<W> {
    fn from_raw(inner: RawBuffer) -> Self {
        let () = W::ALIGN_OK;
        Self { inner, _word: PhantomData }
    }

    /// Returns the size of the mapped DMA buffer in bytes.
    pub fn size(&self) -> usize {
        self.inner.size
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &Vmo {
        &self.inner.vmo
    }

    /// Returns the [`CacheOptions`] configured for this DMA buffer.
    pub fn cache_options(&self) -> CacheOptions {
        self.inner.cache_options
    }

    /// Unpins the DMA buffer's physical memory token, consuming the buffer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.inner.unpin() }
    }

    /// Flushes CPU data cache lines (clean to memory) for `[offset, offset + length)` via the
    /// userspace `zx_cache_flush` vDSO call (`DMA_TO_DEVICE`) when [`CacheOptions::Enabled`].
    pub fn cache_flush(&self, offset: usize, length: usize) -> Result<(), Status> {
        self.inner.cache_op(offset, length, ZX_CACHE_FLUSH_DATA)
    }

    /// Flushes and invalidates CPU data cache lines for `[offset, offset + length)` via the
    /// userspace `zx_cache_flush` vDSO call (`DMA_FROM_DEVICE`) when [`CacheOptions::Enabled`].
    pub fn cache_flush_invalidate(&self, offset: usize, length: usize) -> Result<(), Status> {
        self.inner.cache_op(offset, length, ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE)
    }

    /// Returns an in-place view of the mapped buffer as a slice of `W` DMA cells (`&[DmaU32]` or
    /// `&[DmaU64]`).
    ///
    /// Works on cached, coherent, and [`CacheOptions::Disabled`] mappings. Callers on
    /// non-cache-coherent busses using [`CacheOptions::Enabled`] must pair accesses with
    /// [`Self::cache_flush`] or [`Self::cache_flush_invalidate`].
    pub fn as_slice(&self) -> &[W] {
        let () = W::ALIGN_OK;
        // SAFETY: `self.inner.ptr` is non-null, aligned to `MAX_DMA_ALIGN >= align_of::<W>()`, and
        // backed by `self.inner.size <= isize::MAX as usize` live non-resizable zero-initialized
        // R/W VMO bytes; `W` is `#[repr(transparent)]` over `AtomicU32` or `AtomicU64`
        // (`UnsafeCell`-backed, valid for all initialized bit patterns), and `SharedBuffer<W>`
        // never creates `&mut` references or accesses the mapping at any other atomic width until
        // `Drop`.
        unsafe {
            slice::from_raw_parts(
                self.inner.ptr.as_ptr().cast::<W>(),
                self.inner.size / size_of::<W>(),
            )
        }
    }

    fn word_slot<T>(&self, index: usize) -> Result<&[W], Status> {
        let () = AssertWordMultiple::<T, W>::OK;
        let words_per_elem = size_of::<T>() / size_of::<W>();
        let start = index.checked_mul(words_per_elem).ok_or(Status::OUT_OF_RANGE)?;
        let end = start.checked_add(words_per_elem).ok_or(Status::OUT_OF_RANGE)?;
        self.as_slice().get(start..end).ok_or(Status::OUT_OF_RANGE)
    }

    /// Writes `val` as the `index`-th element of type `T` (at byte offset `index * size_of::<T>()`)
    /// using `W`-granular relaxed atomic stores without an implicit `zx_cache_flush`.
    ///
    /// Callers on non-cache-coherent busses using [`CacheOptions::Enabled`] must call
    /// [`Self::cache_flush`] explicitly when publishing descriptors to hardware. Fails to compile
    /// if `size_of::<T>()` is not a multiple of `size_of::<W>()`.
    pub fn write_at<T: IntoBytes + Immutable>(&self, index: usize, val: &T) -> Result<(), Status> {
        let cells = self.word_slot::<T>(index)?;
        W::store_from_bytes(cells, val.as_bytes());
        Ok(())
    }

    /// Reads the `index`-th element of type `T` (at byte offset `index * size_of::<T>()`) using
    /// `W`-granular relaxed atomic loads without an implicit `zx_cache_flush`.
    ///
    /// Callers on non-cache-coherent busses using [`CacheOptions::Enabled`] must call
    /// [`Self::cache_flush_invalidate`] explicitly before reading device-written bytes. Fails to
    /// compile if `size_of::<T>()` is not a multiple of `size_of::<W>()`.
    pub fn read_at<T: FromBytes>(&self, index: usize) -> Result<T, Status> {
        let cells = self.word_slot::<T>(index)?;
        let mut val = MaybeUninit::<T>::uninit();
        if !cells.is_empty() {
            // SAFETY: `val` is a valid local `MaybeUninit<T>` of `size_of::<T>()` writable bytes.
            let dst = unsafe {
                slice::from_raw_parts_mut(
                    val.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    size_of::<T>(),
                )
            };
            W::load_to_uninit(cells, dst);
        }
        // SAFETY: Either `size_of::<T>() == 0` or `load_to_uninit` initialized all `size_of::<T>()`
        // bytes of `val`, and `T: FromBytes` accepts every initialized byte pattern.
        Ok(unsafe { val.assume_init() })
    }
}

/// A physically contiguous DMA buffer pinned to a [`Bti`] and mapped into a [`Vmar`].
///
/// Defaults to the exclusive [`Buffer`] role; convert via [`ContiguousBuffer::into_shared`] (see
/// [`SharedContiguousBuffer`]) for concurrent or live-DMA access.
#[derive(Debug)]
pub struct ContiguousBuffer<B = Buffer> {
    buffer: B,
    phys: zx_paddr_t,
}

/// A physically contiguous shared DMA buffer pinned to a [`Bti`] and mapped into a [`Vmar`] for
/// concurrent or live-DMA access.
pub type SharedContiguousBuffer<W = DmaU32> = ContiguousBuffer<SharedBuffer<W>>;

impl<B> Deref for ContiguousBuffer<B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for ContiguousBuffer<Buffer> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl ContiguousBuffer<Buffer> {
    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in the root VMAR with default alignment and [`CacheOptions::Enabled`].
    pub fn new(bti: &Bti, size: usize) -> Result<Self, Status> {
        Self::new_with_vmar(bti, &vmar_root_self(), size)
    }

    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in `vmar` with default alignment and [`CacheOptions::Enabled`].
    pub fn new_with_vmar(bti: &Bti, vmar: &Vmar, size: usize) -> Result<Self, Status> {
        Self::new_with_options(bti, vmar, size, 0, CacheOptions::Enabled)
    }

    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in `vmar` with the specified `alignment_log2` and `cache_options`.
    pub fn new_with_options(
        bti: &Bti,
        vmar: &Vmar,
        size: usize,
        alignment_log2: u32,
        cache_options: CacheOptions,
    ) -> Result<Self, Status> {
        let (mapped_size, _) = RawBuffer::round_up_size(size)?;
        let vmo = Vmo::create_contiguous(bti, mapped_size, alignment_log2)?;
        let mut phys = [0 as zx_paddr_t; 1];
        let inner = RawBuffer::new(
            vmar,
            vmo,
            mapped_size,
            bti,
            BtiOptions::PERM_READ | BtiOptions::PERM_WRITE | BtiOptions::CONTIGUOUS,
            &mut phys,
            cache_options,
        )?;
        Ok(Self { buffer: Buffer { inner }, phys: phys[0] })
    }

    /// Converts this exclusive buffer into a [`SharedContiguousBuffer`] for concurrent or live-DMA
    /// access with atomic word granularity `W`.
    pub fn into_shared<W: DmaWord>(self) -> SharedContiguousBuffer<W> {
        ContiguousBuffer { buffer: SharedBuffer::from_raw(self.buffer.inner), phys: self.phys }
    }
}

impl<B> ContiguousBuffer<B> {
    /// Returns the base physical address of the contiguous DMA buffer.
    pub fn phys(&self) -> zx_paddr_t {
        self.phys
    }
}

impl<B: sealed::BufferRole> ContiguousBuffer<B> {
    /// Unpins the DMA buffer's physical memory token, consuming the buffer so its physical
    /// address and mapping cannot be accessed after unpinning.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.buffer.unpin_role() }
    }
}

/// A paged DMA buffer consisting of one or more pinned pages that are contiguous in virtual memory,
/// but may be discontiguous in physical memory.
///
/// Defaults to the exclusive [`Buffer`] role; convert via [`PagedBuffer::into_shared`] (see
/// [`SharedPagedBuffer`]) for concurrent or live-DMA access.
#[derive(Debug)]
pub struct PagedBuffer<B = Buffer> {
    buffer: B,
    phys: Vec<zx_paddr_t>,
}

/// A paged shared DMA buffer consisting of one or more pinned pages mapped contiguously in virtual
/// memory for concurrent or live-DMA access.
pub type SharedPagedBuffer<W = DmaU32> = PagedBuffer<SharedBuffer<W>>;

impl<B> Deref for PagedBuffer<B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for PagedBuffer<Buffer> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl PagedBuffer<Buffer> {
    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in the root VMAR with [`CacheOptions::Enabled`].
    pub fn new(bti: &Bti, size: usize) -> Result<Self, Status> {
        Self::new_with_vmar(bti, &vmar_root_self(), size)
    }

    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in `vmar` with [`CacheOptions::Enabled`].
    pub fn new_with_vmar(bti: &Bti, vmar: &Vmar, size: usize) -> Result<Self, Status> {
        Self::new_with_options(bti, vmar, size, CacheOptions::Enabled)
    }

    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in `vmar` with the specified `cache_options`.
    pub fn new_with_options(
        bti: &Bti,
        vmar: &Vmar,
        size: usize,
        cache_options: CacheOptions,
    ) -> Result<Self, Status> {
        let (mapped_size, page_size) = RawBuffer::round_up_size(size)?;
        let vmo = Vmo::create(mapped_size as u64)?;
        let mut phys = vec![0 as zx_paddr_t; mapped_size / page_size];
        let inner = RawBuffer::new(
            vmar,
            vmo,
            mapped_size,
            bti,
            BtiOptions::PERM_READ | BtiOptions::PERM_WRITE,
            &mut phys,
            cache_options,
        )?;
        Ok(Self { buffer: Buffer { inner }, phys })
    }

    /// Converts this exclusive buffer into a [`SharedPagedBuffer`] for concurrent or live-DMA
    /// access with atomic word granularity `W`.
    pub fn into_shared<W: DmaWord>(self) -> SharedPagedBuffer<W> {
        PagedBuffer { buffer: SharedBuffer::from_raw(self.buffer.inner), phys: self.phys }
    }
}

impl<B> PagedBuffer<B> {
    /// Returns the slice of pinned page physical addresses backing the buffer.
    pub fn phys(&self) -> &[zx_paddr_t] {
        &self.phys
    }
}

impl<B: sealed::BufferRole> PagedBuffer<B> {
    /// Unpins the DMA buffer's physical memory token, consuming the buffer so its physical
    /// addresses and mapping cannot be accessed after unpinning.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.buffer.unpin_role() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_bti::FakeBti;

    // Intentionally contains 3 implicit padding bytes between `tag` and `value` so it implements
    // `FromBytes` without `IntoBytes`.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes)]
    struct PaddedHeader {
        tag: u8,
        value: u32,
    }

    #[fuchsia::test]
    fn test_contiguous_cache_options() {
        let bti = FakeBti::create().unwrap();
        let page_size = system_get_page_size() as usize;
        let size = page_size * 4;

        for (opts, expected_policy) in [
            (CacheOptions::Enabled, CachePolicy::Cached),
            (CacheOptions::Coherent, CachePolicy::Cached),
            (CacheOptions::Disabled, CachePolicy::UnCachedDevice),
        ] {
            let buffer =
                ContiguousBuffer::new_with_options(&bti, &vmar_root_self(), size, 12, opts)
                    .unwrap();

            assert_eq!(buffer.size(), size);
            assert_eq!(buffer.phys(), FakeBti::phys_addr());
            assert_eq!(buffer.cache_options(), opts);
            assert_eq!(buffer.vmo().info().unwrap().cache_policy(), expected_policy);
            assert_eq!(bti.info().unwrap().pmo_count, 1);

            // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
            unsafe { buffer.unpin().unwrap() };
            assert_eq!(bti.info().unwrap().pmo_count, 0);
        }
    }

    #[fuchsia::test]
    fn test_paged_buffer() {
        let bti = FakeBti::create().unwrap();
        bti.set_paddrs(&[0x1000, 0x3000, 0x5000]);
        let page_size = system_get_page_size() as usize;
        // Request unaligned size (2 pages + 100 bytes) to verify rounding up to 3 pages.
        let buffer = PagedBuffer::new(&bti, page_size * 2 + 100).unwrap();

        assert_eq!(buffer.size(), page_size * 3);
        assert_eq!(buffer.phys(), &[0x1000, 0x3000, 0x5000]);
        assert_eq!(buffer.cache_options(), CacheOptions::Enabled);
        assert_eq!(bti.info().unwrap().pmo_count, 1);

        // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
        unsafe { buffer.unpin().unwrap() };
        assert_eq!(bti.info().unwrap().pmo_count, 0);
    }

    #[fuchsia::test]
    fn test_drop_unmaps_vmar_region() {
        let bti = FakeBti::create().unwrap();
        let page_size = system_get_page_size() as usize;
        let (child_vmar, _base) = vmar_root_self()
            .allocate(0, page_size * 4, VmarFlags::CAN_MAP_READ | VmarFlags::CAN_MAP_WRITE)
            .unwrap();

        let buffer = ContiguousBuffer::new_with_vmar(&bti, &child_vmar, page_size).unwrap();
        let koid = buffer.vmo().koid().unwrap();
        assert!(
            child_vmar
                .maps_vec()
                .unwrap()
                .iter()
                .any(|m| m.details().as_mapping().is_some_and(|d| d.vmo_koid == koid))
        );

        // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
        unsafe { buffer.unpin().unwrap() };

        assert!(
            !child_vmar
                .maps_vec()
                .unwrap()
                .iter()
                .any(|m| m.details().as_mapping().is_some_and(|d| d.vmo_koid == koid))
        );
    }

    #[fuchsia::test]
    fn test_exclusive_read_write_slice_and_cache_operations() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;

        for opts in [CacheOptions::Enabled, CacheOptions::Coherent] {
            let mut buffer =
                ContiguousBuffer::new_with_options(&bti, &vmar_root_self(), size, 0, opts).unwrap();

            let write_data = [0xDE, 0xAD, 0xBE, 0xEF];
            let mut read_data = [0u8; 4];
            buffer.write(16, &write_data).unwrap();
            buffer.read(16, &mut read_data).unwrap();
            assert_eq!(write_data, read_data);

            #[repr(C)]
            #[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable)]
            struct TestPacket {
                magic: u32,
                length: u16,
                flags: u8,
                pad: u8,
            }

            let out_pkt = TestPacket { magic: 0x12345678, length: 64, flags: 0xFF, pad: 0 };
            buffer.write_struct(64, &out_pkt).unwrap();
            let in_pkt: TestPacket = buffer.read_struct(64).unwrap();
            assert_eq!(out_pkt, in_pkt);

            buffer.write_at(3, &out_pkt).unwrap();
            assert_eq!(buffer.read_at::<TestPacket>(3).unwrap(), out_pkt);

            let batch = [0x1111_2222u32, 0x3333_4444, 0x5555_6666];
            buffer.write_slice(128, &batch).unwrap();
            assert_eq!(buffer.read_struct::<[u32; 3]>(128).unwrap(), batch);

            // Odd-indexed 4-byte element (`offset = 4`) and `FromBytes`-only struct with padding.
            buffer.write_at(1, &0x5566_7788u32).unwrap();
            assert_eq!(buffer.read_at::<u32>(1).unwrap(), 0x5566_7788);
            buffer.write(32, &[0x5A, 0xFF, 0xFF, 0xFF, 0x78, 0x56, 0x34, 0x12]).unwrap();
            let hdr = PaddedHeader { tag: 0x5A, value: u32::from_le(0x1234_5678) };
            assert_eq!(buffer.read_struct::<PaddedHeader>(32).unwrap(), hdr);
            assert_eq!(buffer.read_at::<PaddedHeader>(4).unwrap(), hdr);

            assert_eq!(buffer.write(size - 2, &write_data), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.read(size - 2, &mut read_data), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.write(size + 1, &[]), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.read(size + 1, &mut []), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.write_at(usize::MAX, &out_pkt), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.read_at::<TestPacket>(usize::MAX), Err(Status::OUT_OF_RANGE));

            assert_eq!(buffer.cache_flush(0, 0), Ok(()));
            assert_eq!(buffer.cache_flush(0, 32), Ok(()));
            assert_eq!(buffer.cache_flush_invalidate(0, 0), Ok(()));
            assert_eq!(buffer.cache_flush_invalidate(0, 32), Ok(()));
            assert_eq!(buffer.cache_flush(size + 1, 0), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.cache_flush_invalidate(size + 1, 0), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.cache_flush(size - 10, 20), Err(Status::OUT_OF_RANGE));
            assert_eq!(buffer.cache_flush_invalidate(size - 10, 20), Err(Status::OUT_OF_RANGE));

            // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
            unsafe { buffer.unpin().unwrap() };
        }
    }

    #[fuchsia::test]
    fn test_uncached_volatile_helper_all_cases() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;
        let mut buffer = ContiguousBuffer::new_with_options(
            &bti,
            &vmar_root_self(),
            size,
            0,
            CacheOptions::Disabled,
        )
        .unwrap();
        assert_eq!(buffer.cache_options(), CacheOptions::Disabled);

        // 1. Zero-length transfer.
        let mut empty = [];
        assert_eq!(buffer.write(0, &empty), Ok(()));
        assert_eq!(buffer.read(0, &mut empty), Ok(()));
        assert_eq!(buffer.zero(0, 0), Ok(()));

        // 2. Background pattern verification helper.
        let bg_pattern = vec![0xAAu8; size];
        buffer.write(0, &bg_pattern).unwrap();

        let verify_slice = |buf: &ContiguousBuffer, offset: usize, length: usize, val: u8| {
            let mut read_back = vec![0u8; size];
            buf.read(0, &mut read_back).unwrap();
            for (i, &b) in read_back.iter().enumerate() {
                if i >= offset && i < offset + length {
                    assert_eq!(b, val, "mismatch at modified index {i}");
                } else {
                    assert_eq!(b, 0xAA, "corruption at background index {i}");
                }
            }
        };

        // 3. Small unaligned transfer (< 8 bytes, at offset 1).
        buffer.write(1, &[0x11; 3]).unwrap();
        verify_slice(&buffer, 1, 3, 0x11);
        buffer.zero(1, 3).unwrap();
        verify_slice(&buffer, 1, 3, 0x00);
        buffer.write(0, &bg_pattern).unwrap();

        // 4. Unaligned head alignment (offset 3, length 24).
        buffer.write(3, &[0x22; 24]).unwrap();
        verify_slice(&buffer, 3, 24, 0x22);
        buffer.zero(3, 24).unwrap();
        verify_slice(&buffer, 3, 24, 0x00);
        buffer.write(0, &bg_pattern).unwrap();

        // 5. Unaligned tail alignment (offset 8, length 13).
        buffer.write(8, &[0x33; 13]).unwrap();
        verify_slice(&buffer, 8, 13, 0x33);
        buffer.write(0, &bg_pattern).unwrap();

        // 6. Both unaligned head and unaligned tail (offset 5, length 19).
        buffer.write(5, &[0x44; 19]).unwrap();
        verify_slice(&buffer, 5, 19, 0x44);
        buffer.zero(5, 19).unwrap();
        verify_slice(&buffer, 5, 19, 0x00);
        buffer.write(0, &bg_pattern).unwrap();

        // 7. Exact word boundary transfer (offset 16, length 32) and OOB zero check.
        buffer.write(16, &[0x66; 32]).unwrap();
        verify_slice(&buffer, 16, 32, 0x66);
        assert_eq!(buffer.zero(size - 2, 4), Err(Status::OUT_OF_RANGE));
        assert_eq!(buffer.zero(size + 1, 0), Err(Status::OUT_OF_RANGE));

        // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
        unsafe { buffer.unpin().unwrap() };
    }

    #[fuchsia::test]
    fn test_copy_to_and_from_mapping() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;

        for cache_options in [CacheOptions::Enabled, CacheOptions::Coherent, CacheOptions::Disabled]
        {
            let mut buffer =
                PagedBuffer::new_with_options(&bti, &vmar_root_self(), size, cache_options)
                    .unwrap();
            let (mapping, _vmo) = Mapping::allocate(size).unwrap();

            let len = 600;
            let src_data: Vec<u8> = (0..len).map(|i| ((i * 29 + 3) & 0xFF) as u8).collect();
            assert_eq!(mapping.write_at(13, &src_data), len);

            buffer.copy_from_mapping(7, &mapping, 13, len).unwrap();
            let shared_ref: &PagedBuffer = &buffer;
            let mut check = vec![0u8; len];
            shared_ref.read(7, &mut check).unwrap();
            assert_eq!(check, src_data);

            // `copy_to_mapping` only requires `&self`.
            let (mut out_mapping, _out_vmo) = Mapping::allocate(size).unwrap();
            shared_ref.copy_to_mapping(7, &mut out_mapping, 19, len).unwrap();
            let mut mapped_out = vec![0u8; len];
            assert_eq!(out_mapping.read_at(19, &mut mapped_out), len);
            assert_eq!(mapped_out, src_data);

            assert_eq!(shared_ref.copy_to_mapping(size, &mut out_mapping, size, 0), Ok(()));
            assert_eq!(
                shared_ref.copy_to_mapping(size + 1, &mut out_mapping, 0, 0),
                Err(Status::OUT_OF_RANGE)
            );
            assert_eq!(buffer.copy_from_mapping(size, &mapping, size, 0), Ok(()));
            assert_eq!(
                buffer.copy_from_mapping(0, &mapping, size - 4, 8),
                Err(Status::OUT_OF_RANGE)
            );

            // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
            unsafe { buffer.unpin().unwrap() };
        }
    }

    #[fuchsia::test]
    fn test_shared_buffer_u32_cached_coherent_and_uncached() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;

        for cache_options in [CacheOptions::Enabled, CacheOptions::Coherent, CacheOptions::Disabled]
        {
            let shared: SharedContiguousBuffer<DmaU32> =
                ContiguousBuffer::new_with_options(&bti, &vmar_root_self(), size, 0, cache_options)
                    .unwrap()
                    .into_shared();

            assert_eq!(shared.cache_options(), cache_options);
            let u32s = shared.as_slice();
            assert_eq!(u32s.len(), size / size_of::<u32>());

            u32s[0].store_relaxed(0xCAFE_BABE);
            u32s[3].store_release(0x8001_0002);
            shared.cache_flush(0, 16).unwrap();

            assert_eq!(u32s[3].load_acquire(), 0x8001_0002);
            assert_eq!(shared.read_at::<u32>(0).unwrap(), 0xCAFE_BABE);

            shared.write_at(4, &[0x1122_3344u32, 0x5566_7788]).unwrap();
            assert_eq!(shared.read_at::<[u32; 2]>(4).unwrap(), [0x1122_3344, 0x5566_7788]);

            // `FromBytes`-only struct with implicit padding (`size_of::<PaddedHeader>() == 8`).
            shared
                .write_at(
                    8,
                    &[u32::from_ne_bytes([0x5A, 0xFF, 0xFF, 0xFF]), u32::from_le(0x1234_5678)],
                )
                .unwrap();
            let hdr = PaddedHeader { tag: 0x5A, value: u32::from_le(0x1234_5678) };
            assert_eq!(shared.read_at::<PaddedHeader>(8).unwrap(), hdr);

            assert_eq!(shared.write_at(usize::MAX, &1u32), Err(Status::OUT_OF_RANGE));
            assert_eq!(shared.read_at::<u32>(usize::MAX), Err(Status::OUT_OF_RANGE));
            assert_eq!(shared.read_at::<u32>(size / size_of::<u32>()), Err(Status::OUT_OF_RANGE));
            assert_eq!(shared.cache_flush_invalidate(0, 16), Ok(()));
            assert_eq!(shared.cache_flush(size + 1, 0), Err(Status::OUT_OF_RANGE));

            // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
            unsafe { shared.unpin().unwrap() };
        }
    }

    #[fuchsia::test]
    fn test_shared_paged_buffer_u64_cached_coherent_and_uncached() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;

        for cache_options in [CacheOptions::Enabled, CacheOptions::Coherent, CacheOptions::Disabled]
        {
            bti.set_paddrs(&[0x2000, 0x4000]);
            let shared: SharedPagedBuffer<DmaU64> =
                PagedBuffer::new_with_options(&bti, &vmar_root_self(), size * 2, cache_options)
                    .unwrap()
                    .into_shared();

            assert_eq!(shared.phys(), &[0x2000, 0x4000]);
            let u64s = shared.as_slice();
            assert_eq!(u64s.len(), (size * 2) / size_of::<u64>());

            u64s[0].store_relaxed(0x0123_4567_89AB_CDEF);
            u64s[7].store_release(0xFEDC_BA98_7654_3210);
            shared.cache_flush(0, 64).unwrap();

            assert_eq!(u64s[7].load_acquire(), 0xFEDC_BA98_7654_3210);
            assert_eq!(shared.read_at::<u64>(0).unwrap(), 0x0123_4567_89AB_CDEF);

            shared.write_at(3, &[10u64, 20, 30]).unwrap();
            assert_eq!(shared.read_at::<[u64; 3]>(3).unwrap(), [10, 20, 30]);
            assert_eq!(shared.read_at::<u64>(usize::MAX), Err(Status::OUT_OF_RANGE));

            // SAFETY: No hardware DMA was submitted for this FakeBti buffer.
            unsafe { shared.unpin().unwrap() };
        }
    }
}
