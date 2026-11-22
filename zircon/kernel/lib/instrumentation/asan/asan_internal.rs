// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Internal KASAN shadow constants, address translation, and compiler ABI types.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void};
use core::mem::offset_of;
use core::ptr;

use crate::arch_rs::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};

#[cfg(target_arch = "x86_64")]
use super::X86_KERNEL_KASAN_PDP_ENTRIES;
use super::{ASAN_MAPPING_SCALE, KASAN_SHADOW_OFFSET};

/// Shift factor mapping kernel virtual addresses to KASAN shadow bytes (`kAsanShift`).
pub const ASAN_SHIFT: usize = ASAN_MAPPING_SCALE;

/// Total size in bytes of the KASAN shadow region (`kAsanShadowSize`).
pub const ASAN_SHADOW_SIZE: usize = KERNEL_ASPACE_SIZE >> ASAN_SHIFT;

#[cfg(target_arch = "x86_64")]
zr::static_assert!(X86_KERNEL_KASAN_PDP_ENTRIES * 1024 * 1024 * 1024 == ASAN_SHADOW_SIZE);

/// Number of kernel bytes represented by a single shadow byte (`kAsanGranularity`).
pub const ASAN_GRANULARITY: usize = 1 << ASAN_SHIFT;

/// Bitmask for offsets within a single KASAN granule (`kAsanGranularityMask`).
pub const ASAN_GRANULARITY_MASK: usize = ASAN_GRANULARITY - 1;

/// Cell for KASAN runtime state mutated exclusively on `BOOT_CPU_ID` during single-threaded early
/// boot before secondary CPUs are brought online, and read-only thereafter.
///
/// Computes raw pointers via `cell.cast::<T>().cast_mut()` rather than `UnsafeCell::get` or
/// `AtomicBool::as_ptr` so `#[sanitize(address = "off")]` callers never risk emitting an
/// out-of-line call into KASAN-instrumented `libcore` methods.
#[repr(transparent)]
pub(super) struct EarlyBootCell<T>(UnsafeCell<T>);

// SAFETY: `EarlyBootCell` exposes no safe methods to access its inner `UnsafeCell`; all callers
// must uphold the single-threaded early-boot write / read-only-afterward protocol in their
// `unsafe` accessors.
unsafe impl<T> Sync for EarlyBootCell<T> {}

impl<T> EarlyBootCell<T> {
    /// Creates a new `EarlyBootCell` containing `value`.
    pub(super) const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// Returns a raw mutable pointer to the inner value without invoking `UnsafeCell::get`.
    ///
    /// Takes `*const Self` rather than `&self` so large `T` (such as `GLOBALS_REGIONS`) never
    /// materialize `&Self` across a call boundary under
    /// `-asan-instrument-assume-dereferenceable=1`.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) const fn as_ptr(cell: *const Self) -> *mut T {
        cell.cast::<T>().cast_mut()
    }
}

impl EarlyBootCell<bool> {
    /// Reads the early-boot boolean flag without calling KASAN-instrumented `libcore` helpers and
    /// without a reader-side compiler memory clobber.
    ///
    /// # Safety
    ///
    /// Must not race with [`Self::set`].
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn get(&self) -> bool {
        // SAFETY: `self` is a valid static `EarlyBootCell<bool>` and caller guarantees no
        // concurrent write.
        unsafe { *Self::as_ptr(&raw const *self) }
    }

    /// Writes the early-boot boolean flag, bracketing the store with compiler barriers so
    /// surrounding KASAN initialization cannot be reordered across the transition.
    ///
    /// # Safety
    ///
    /// Must only be called during single-threaded early boot before secondary CPUs start.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn set(&self, value: bool) {
        // SAFETY: Caller guarantees single-threaded early boot before secondary CPUs start.
        unsafe {
            core::arch::asm!("", options(nostack, preserves_flags));
            *Self::as_ptr(&raw const *self) = value;
            core::arch::asm!("", options(nostack, preserves_flags));
        }
    }
}

/// Tracks whether the KASAN shadow map has been initialized (`g_asan_initialized`).
static ASAN_INITIALIZED: EarlyBootCell<bool> = EarlyBootCell::new(false);

/// Returns whether the KASAN shadow map has been initialized (`g_asan_initialized`).
#[sanitize(address = "off")]
#[inline]
pub(super) fn is_asan_initialized() -> bool {
    // SAFETY: `ASAN_INITIALIZED` is written once during single-threaded early boot
    // (`arch_asan_early_init`) before secondary CPUs are brought online.
    unsafe { ASAN_INITIALIZED.get() }
}

/// Marks the KASAN shadow map as initialized (`g_asan_initialized = true`).
///
/// # Safety
///
/// Must only be called during single-threaded early boot (`arch_asan_early_init`).
#[sanitize(address = "off")]
#[inline]
pub(super) unsafe fn set_asan_initialized() {
    // SAFETY: Caller guarantees single-threaded early boot before secondary CPUs start.
    unsafe { ASAN_INITIALIZED.set(true) }
}

/// The redzone is an area of poisoned bytes added at the end of memory allocations. This allows
/// detecting out-of-bounds accesses.
///
/// Increasing this size allows detecting out-of-bounds access that are further beyond the end of
/// the allocation, but each allocation would take more space.
///
/// The kernel's implementation uses a fixed redzone plus a small variable block for alignment.
/// In LLVM (compiler-rt)'s implementation of the asan runtime, the redzone is adaptive depending
/// on the size of the allocation.
pub(super) const HEAP_RIGHT_REDZONE_SIZE: usize = 16;

/// Any value in the shadow equal to or above this value is poisoned (`kAsanSmallestPoisonedValue`).
pub(super) const ASAN_SMALLEST_POISONED_VALUE: u8 = 0x08;

/// Returns the address of the shadow byte corresponding to `address`.
#[allow(clippy::absurd_extreme_comparisons)]
#[sanitize(address = "off")]
#[inline]
pub(super) fn addr2shadow(address: usize) -> *mut u8 {
    debug_assert!(address >= KERNEL_ASPACE_BASE, "address: 0x{:016x}", address);
    debug_assert!(
        address <= KERNEL_ASPACE_BASE + (KERNEL_ASPACE_SIZE - 1),
        "address: 0x{:016x}",
        address
    );

    let kasan_shadow_map = ptr::with_exposed_provenance_mut::<u8>(KASAN_SHADOW_OFFSET);
    kasan_shadow_map.wrapping_add((address - KERNEL_ASPACE_BASE) >> ASAN_SHIFT)
}

/// Kind of memory access being checked by KASAN.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum AccessType {
    Read = 0,
    Write = 1,
    Assumption = 2,
}

/// Structure shared between the compiler and ASAN runtime describing the location (in source code)
/// where a particular global is defined.
///
/// See LLVM compiler-rt/lib/asan/asan_interface_internal.h
#[repr(C)]
pub struct AsanGlobalSourceLocation {
    pub filename: *const c_char,
    pub line_no: c_int,
    pub column_no: c_int,
}

zr::static_assert_size_and_align!(AsanGlobalSourceLocation, 16, 8);
zr::static_assert!(offset_of!(AsanGlobalSourceLocation, filename) == 0);
zr::static_assert!(offset_of!(AsanGlobalSourceLocation, line_no) == 8);
zr::static_assert!(offset_of!(AsanGlobalSourceLocation, column_no) == 12);

/// Structure shared between the compiler and ASAN runtime describing a global variable that is
/// instrumented. Describes the virtual address, source location, size and redzone, and other
/// metadata.
///
/// See LLVM compiler-rt/lib/asan/asan_interface_internal.h
#[repr(C)]
pub struct AsanGlobal {
    pub begin: *const c_void,
    pub size: usize,
    pub size_with_redzone: usize,
    pub name: *const c_char,
    pub module_name: *const c_char,
    pub dynamic_init: usize,
    pub asan_global_source_location: *mut AsanGlobalSourceLocation,
    pub odr_indicator: usize,
}

zr::static_assert_size_and_align!(AsanGlobal, 64, 8);
zr::static_assert!(offset_of!(AsanGlobal, begin) == 0);
zr::static_assert!(offset_of!(AsanGlobal, size) == 8);
zr::static_assert!(offset_of!(AsanGlobal, size_with_redzone) == 16);
zr::static_assert!(offset_of!(AsanGlobal, name) == 24);
zr::static_assert!(offset_of!(AsanGlobal, module_name) == 32);
zr::static_assert!(offset_of!(AsanGlobal, dynamic_init) == 40);
zr::static_assert!(offset_of!(AsanGlobal, asan_global_source_location) == 48);
zr::static_assert!(offset_of!(AsanGlobal, odr_indicator) == 56);
