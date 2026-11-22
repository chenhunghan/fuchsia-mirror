// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel Address Sanitizer (KASAN) is a tool to detect use-after-free, use-out-of-bounds, and
//! other common memory errors in kernel.
//! See: //zircon/kernel/lib/instrumentation/asan/README.md for more context.

#[cfg(sanitize = "address")]
cfg_select! {
    target_arch = "x86_64" => {
        #[path = "asan_init_x86.rs"]
        mod arch;
    }
    target_arch = "aarch64" => {
        #[path = "asan_init_arm64.rs"]
        mod arch;
    }
    target_arch = "riscv64" => {
        compile_error!("KASAN is not yet supported on riscv64");
    }
}

#[cfg(sanitize = "address")]
mod asan_init;
#[cfg(sanitize = "address")]
pub(crate) mod asan_internal;
#[cfg(sanitize = "address")]
mod asan_poisoning;
#[cfg(sanitize = "address")]
mod asan_stubs;

use core::mem::{align_of, size_of};
use core::ptr;

use crate::arch_rs::KERNEL_ASPACE_BASE;

#[cfg(sanitize = "address")]
pub use arch::asan_map_shadow_for;
#[cfg(sanitize = "address")]
pub use asan_poisoning::{
    asan_address_is_poisoned, asan_entire_region_is_poisoned, asan_heap_redzone_size,
    asan_poison_shadow, asan_region_is_poisoned, asan_unpoison_shadow,
};

// LINT.IfChange
/// 1 shadow byte covers `2^3` bytes (`ASAN_MAPPING_SCALE`).
pub const ASAN_MAPPING_SCALE: usize = 3;

/// Base offset used by the compiler to compute the shadow address of `addr` as
/// `ASAN_MAPPING_OFFSET + (addr >> ASAN_MAPPING_SCALE)`.
pub const ASAN_MAPPING_OFFSET: usize = 0xdfff_fff0_0000_0000;
// LINT.ThenChange(BUILD.gn)

/// Start virtual address of the KASAN shadow map covering
/// `[KERNEL_ASPACE_BASE, +KERNEL_ASPACE_SIZE)`.
pub const KASAN_SHADOW_OFFSET: usize =
    ASAN_MAPPING_OFFSET.wrapping_add(KERNEL_ASPACE_BASE >> ASAN_MAPPING_SCALE);

/// Number of 1 GiB PDP entries reserved for the x86-64 KASAN shadow map.
#[cfg(target_arch = "x86_64")]
pub const X86_KERNEL_KASAN_PDP_ENTRIES: usize = 64;

// Distinguished kasan poison values.
// LLVM defines userspace equivalents of these in compiler-rt/lib/asan/asan_internal.h
// There are some differences - kernel ASAN has distinguished states for Pmm free, for example.

// These constants are reserved by the compiler for stack poisoning.
/// KASAN poison byte for stack left redzones (`kAsanStackLeftRedzoneMagic`).
pub const ASAN_STACK_LEFT_REDZONE_MAGIC: u8 = 0xf1;
/// KASAN poison byte for stack middle redzones (`kAsanStackMidRedzoneMagic`).
pub const ASAN_STACK_MID_REDZONE_MAGIC: u8 = 0xf2;
/// KASAN poison byte for stack right redzones (`kAsanStackRightRedzoneMagic`).
pub const ASAN_STACK_RIGHT_REDZONE_MAGIC: u8 = 0xf3;
/// KASAN poison byte for stack use-after-return (`kAsanStackAfterReturnMagic`).
pub const ASAN_STACK_AFTER_RETURN_MAGIC: u8 = 0xf5;
/// KASAN poison byte for stack use-after-scope (`kAsanStackUseAfterScopeMagic`).
pub const ASAN_STACK_USE_AFTER_SCOPE_MAGIC: u8 = 0xf8;

// These constants are only known to the asan runtime.
/// KASAN poison byte for C++ `operator new[]` array cookies (`kAsanArrayCookie`).
pub const ASAN_ARRAY_COOKIE: u8 = 0xac;
/// KASAN poison byte for internal heap bookkeeping metadata (`kAsanInternalHeapMagic`).
pub const ASAN_INTERNAL_HEAP_MAGIC: u8 = 0xf0;
/// KASAN poison byte for global variable redzones (`kAsanGlobalRedzoneMagic`).
pub const ASAN_GLOBAL_REDZONE_MAGIC: u8 = 0xf9;
/// KASAN poison byte for heap allocation headers and redzones (`kAsanHeapLeftRedzoneMagic`).
pub const ASAN_HEAP_LEFT_REDZONE_MAGIC: u8 = 0xfa;
/// KASAN poison byte for free PMM pages (`kAsanPmmFreeMagic`).
pub const ASAN_PMM_FREE_MAGIC: u8 = 0xfb;
/// KASAN poison byte for quarantined objects (`kAsanQuarantineMagic`).
pub const ASAN_QUARANTINE_MAGIC: u8 = 0xfc;
/// KASAN poison byte for freed heap payloads (`kAsanHeapFreeMagic`).
pub const ASAN_HEAP_FREE_MAGIC: u8 = 0xfd;
/// KASAN poison byte for allocation headers (`kAsanAllocHeader`).
pub const ASAN_ALLOC_HEADER: u8 = 0xff;

/// Fixed capacity of [`Quarantine`] (`asan::Quarantine::kQuarantineElements`).
pub const QUARANTINE_ELEMENTS: usize = 65536;

/// Fixed-size FIFO ring buffer delaying reuse of freed heap allocations under KASAN.
#[repr(C)]
pub struct Quarantine<T> {
    pub(super) queue: [*mut T; QUARANTINE_ELEMENTS],
    pub(super) pos: usize,
}

zr::static_assert!(size_of::<Quarantine<u8>>() == 524296);
zr::static_assert!(align_of::<Quarantine<u8>>() == 8);

impl<T> Quarantine<T> {
    /// Creates an empty `Quarantine`.
    ///
    /// Note: `Quarantine` is 512 KiB and must only be constructed in `static` initializers, never
    /// on an 8 KiB kernel thread stack.
    pub const fn new() -> Self {
        Self { queue: [ptr::null_mut(); QUARANTINE_ELEMENTS], pos: 0 }
    }
}
