// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! KASAN shadow memory poisoning, unpoisoning, checking, and error reporting.

use core::ffi::{c_int, c_void};
use core::ptr;

use kprint::{kprint, kprintln};

use crate::platform_rs::power::platform_panic_start;
use debug::dprintf;

use super::asan_internal::{
    ASAN_GRANULARITY_MASK, ASAN_SHIFT, ASAN_SMALLEST_POISONED_VALUE, AccessType,
    HEAP_RIGHT_REDZONE_SIZE, addr2shadow, is_asan_initialized,
};
use super::{KASAN_SHADOW_OFFSET, QUARANTINE_ELEMENTS, Quarantine};

unsafe extern "C" {
    fn __unsanitized_memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void;
}

/// Checks if an entire memory region `[ptr, ptr + size)` is all zeroes.
///
/// # Safety
///
/// `[ptr, ptr + size)` must be a valid readable shadow memory range.
#[sanitize(address = "off")]
#[inline]
unsafe fn is_mem_zero(ptr: *const u8, size: usize) -> bool {
    let mut i = 0;
    while i < size {
        // SAFETY: Caller guarantees `[ptr, ptr + size)` is readable shadow memory.
        if unsafe { *ptr.wrapping_add(i) } != 0 {
            return false;
        }
        i += 1;
    }
    true
}

/// When KASAN has detected an invalid access, print information about the access and the
/// corresponding parts of the shadow map. Also print PMM page state.
///
/// Example:
/// (Shadow address)        (shadow map contents)
///
/// KASAN detected a WRITE error: ptr={{{data:0xffffff8043251830}}}, size=0x4, caller:
/// {{{pc:0xffffffff001d9371}}} Shadow memory state around the buggy address 0xffffffe00864a306:
/// 0xffffffe00864a2f0: 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa
/// 0xffffffe00864a2f8: 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa
/// 0xffffffe00864a300: 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa
///                                                    ^^
/// 0xffffffe00864a308: 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa
/// 0xffffffe00864a310: 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa 0xfa
/// page 0xffffff807f475f30: address 0x43251000 state heap flags 0
///
/// # Safety
///
/// `address` and `poisoned_addr` must lie within the kernel virtual address space covered by the
/// KASAN shadow map.
#[sanitize(address = "off")]
unsafe fn print_error_shadow(
    address: usize,
    bytes: usize,
    access_type: AccessType,
    caller: *mut c_void,
    poisoned_addr: usize,
) {
    let shadow = addr2shadow(address).addr();
    let poisoned_shadow = addr2shadow(poisoned_addr).addr();

    let access_type_str = match access_type {
        AccessType::Assumption => "ASSUME",
        AccessType::Read => "READ",
        AccessType::Write => "WRITE",
    };

    dprintf!(
        CRITICAL,
        "\nKASAN detected a {} error: ptr={{{{{{data:{:#x}}}}}}}, size={:#x}, \
         caller: {{{{{{pc:{:p}}}}}}}\n",
        access_type_str,
        address,
        bytes,
        caller
    );

    // TODO(https://fxbug.dev/42104852): Decode the shadow value into 'use-after-free'/redzone/page
    // free/etc.
    kprintln!("Shadow memory state around the buggy address {:#x}:", shadow);
    // Print at least 16 bytes of the shadow map before and after the invalid access.
    let mut start_addr = (shadow & !0x07).saturating_sub(0x10);
    if start_addr < KASAN_SHADOW_OFFSET {
        start_addr = KASAN_SHADOW_OFFSET;
    }
    // Print the shadow map memory state and look for the location to print a caret.
    let mut caret = false;
    let mut caret_ind = 0;
    let mut row = 0;
    while row < 5 {
        // TODO(https://fxbug.dev/42128332): When kernel printf properly supports #, switch.
        kprint!("0x{:016x}:", start_addr);
        let mut j = 0;
        while j < 8 {
            // SAFETY: `start_addr + j` lies within the mapped KASAN shadow region.
            let byte = unsafe { *ptr::with_exposed_provenance::<u8>(start_addr + j) };
            kprint!(" 0x{:02x}", byte);
            if !caret && (start_addr + j) == poisoned_shadow {
                caret = true;
                caret_ind = j;
            }
            j += 1;
        }
        kprint!("\n");
        if caret {
            // The address takes 16 characters; add in space for ':', and "0x".
            kprint!("{:19s}", "");
            // Print either a caret or spaces under the line containing the invalid access.
            let mut k = 0;
            while k < 8 {
                kprint!("  {:>2s} ", if k == caret_ind { "^^" } else { "" });
                k += 1;
            }
            kprint!("\n");
            caret = false;
        }
        start_addr += 8;
        row += 1;
    }
    // Dump additional VM Page state - this is useful to debug use-after-state-change bugs.
    // TODO(https://fxbug.dev/42104852): This is disabled because we could panic while holding one
    // of the aspace locks, causing an error. Figure out how to dump vm_page_t info from here.
    // paddr_to_vm_page(vaddr_to_paddr(reinterpret_cast<void*>(address)))->dump();
}

#[sanitize(address = "off")]
#[inline]
fn ranges_overlap(offset1: usize, len1: usize, offset2: usize, len2: usize) -> bool {
    !((offset1 + len1 <= offset2) || (offset2 + len2 <= offset1))
}

impl<T> Quarantine<T> {
    /// Push `allocation` that was going to be freed into a 'quarantine', increasing reuse
    /// distance.
    /// Returns nullptr if the quarantine is not full or the oldest pushed allocation if it is
    /// full.
    /// If this returns an allocation, it is the responsibility of the caller to free it.
    #[sanitize(address = "off")]
    #[inline]
    pub fn push(&mut self, allocation: *mut T) -> *mut T {
        let idx = self.pos % QUARANTINE_ELEMENTS;
        self.pos = self.pos.wrapping_add(1);
        // Avoid calling `core::mem::replace`, which carries `sanitize_address` when the crate is
        // compiled with `-Zsanitizer=address` and will not inline into
        // `#[sanitize(address = "off")]`.
        // SAFETY: `idx < QUARANTINE_ELEMENTS`.
        unsafe {
            let slot = self.queue.as_mut_ptr().wrapping_add(idx);
            let prev = *slot;
            *slot = allocation;
            prev
        }
    }
}

/// Checks whether a memory `address` is poisoned.
///
/// ASAN tracks address poison status at byte granularity in a shadow map.
/// `ASAN_GRANULARITY` bytes are represented by one byte in the shadow map.
///
/// If the value in the shadow map is 0, accesses to address are allowed.
/// If the value is in `[1, ASAN_GRANULARITY)`, accesses to the corresponding
/// addresses less than the value are allowed.
/// All other values disallow access to the entire aligned region.
///
/// # Safety
///
/// `address` must lie within the kernel virtual address space covered by the KASAN shadow map.
#[sanitize(address = "off")]
#[inline]
pub unsafe fn asan_address_is_poisoned(address: usize) -> bool {
    // SAFETY: Caller guarantees `address` is backed by KASAN shadow memory.
    let shadow_val = unsafe { *addr2shadow(address) };
    // Zero values in the shadow map mean that all 8 bytes are valid.
    if shadow_val == 0 {
        return false;
    }
    if shadow_val >= ASAN_SMALLEST_POISONED_VALUE {
        core::hint::cold_path();
        return true;
    }
    // Part of this region is poisoned. Check whether address is below the last valid byte.
    let offset = address & ASAN_GRANULARITY_MASK;
    (shadow_val as usize) <= offset
}

/// Returns true if all bytes in `[address, address + size)` are poisoned.
///
/// # Safety
///
/// `[address, address + size)` must lie within the kernel virtual address space covered by the
/// KASAN shadow map.
#[sanitize(address = "off")]
pub unsafe fn asan_entire_region_is_poisoned(address: usize, size: usize) -> bool {
    let mut i = 0;
    while i < size {
        // SAFETY: Caller guarantees `[address, address + size)` is backed by KASAN shadow memory.
        if unsafe { !asan_address_is_poisoned(address.wrapping_add(i)) } {
            return false;
        }
        i += 1;
    }
    true
}

/// Returns the address of the first poisoned byte in `[address, address + size)`.
/// If no bytes are poisoned, returns 0.
///
/// # Safety
///
/// `[address, address + size)` must lie within the kernel virtual address space covered by the
/// KASAN shadow map.
#[sanitize(address = "off")]
pub unsafe fn asan_region_is_poisoned(address: usize, size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    // Compute shadow bounds as offsets from `addr2shadow(address)` so ranges reaching the top of
    // `KERNEL_ASPACE` (`2^64`) never wrap to `0` before `addr2shadow`.
    let last = address.wrapping_add(size - 1);
    let offset = address & ASAN_GRANULARITY_MASK;
    let shadow_base = addr2shadow(address).cast_const();
    let shadow_beg = shadow_base.wrapping_add(if offset != 0 { 1 } else { 0 });
    let shadow_end = shadow_base.wrapping_add((offset + size) >> ASAN_SHIFT);

    // SAFETY: Caller guarantees `[address, address + size)` is backed by KASAN shadow memory.
    unsafe {
        if !asan_address_is_poisoned(address)
            && !asan_address_is_poisoned(last)
            && (shadow_end <= shadow_beg
                || is_mem_zero(shadow_beg, shadow_end.addr() - shadow_beg.addr()))
        {
            return 0;
        }
    }

    core::hint::cold_path();
    let mut i = 0;
    while i < size {
        let addr = address.wrapping_add(i);
        // SAFETY: Caller guarantees `[address, address + size)` is backed by KASAN shadow memory.
        if unsafe { asan_address_is_poisoned(addr) } {
            return addr;
        }
        i += 1;
    }

    panic!("Unreachable code");
}

/// Checks the validity of an entire region. This function panics and prints an
/// error message if any part of `[address, address + bytes)` is poisoned.
///
/// # Safety
///
/// `[address, address + bytes)` must lie within the kernel virtual address space covered by the
/// KASAN shadow map.
#[sanitize(address = "off")]
#[inline(never)]
pub(super) unsafe fn asan_check(
    address: usize,
    bytes: usize,
    access_type: AccessType,
    caller: *mut c_void,
) {
    // TODO(https://fxbug.dev/42104852): Inline the fast path for constant-size checks.
    if !is_asan_initialized() {
        core::hint::cold_path();
        return;
    }

    // SAFETY: Caller guarantees `[address, address + bytes)` is backed by KASAN shadow memory.
    let poisoned_addr = unsafe { asan_region_is_poisoned(address, bytes) };
    if poisoned_addr == 0 {
        return;
    }

    core::hint::cold_path();
    platform_panic_start();
    // SAFETY: `address` and `poisoned_addr` lie within the KASAN shadow-mapped address space.
    unsafe {
        print_error_shadow(address, bytes, access_type, caller, poisoned_addr);
    }
    panic!("kasan");
}

/// Checks whether the two memory ranges defined by `[offset1, offset1 + len1)` and
/// `[offset2, offset2 + len2)` overlap. This function panics and prints an error message if
/// the two memory ranges overlap.
#[sanitize(address = "off")]
pub(super) fn asan_check_memory_overlap(offset1: usize, len1: usize, offset2: usize, len2: usize) {
    if !ranges_overlap(offset1, len1, offset2, len2) {
        return;
    }
    core::hint::cold_path();
    platform_panic_start();
    kprintln!("KASAN detected a memory range overlap error.");
    kprintln!(
        "ptr: 0x{:016x} size: {:#x} overlaps with ptr: 0x{:016x} size: {:#x}",
        offset1,
        len1,
        offset2,
        len2
    );
    panic!("kasan");
}

/// `asan_poison_shadow()` marks the memory region denoted by
/// `[address, round_down(address + size, ASAN_GRANULARITY))` as invalid. If the
/// byte located at `address + size` is already poisoned, the entire region
/// `[address, address + size)` is marked as invalid.
/// Memory accesses to that region will fail asan checks.
///
/// `value` annotates the 'type' of poison and must be one of the values in the
/// 'distinguished kasan values' section.
///
/// # Safety
///
/// The caller must ensure that `size > 0`, `[address, address + size)` lies within a region
/// backed by writable KASAN shadow memory, and `value` is a valid KASAN poison magic byte.
#[sanitize(address = "off")]
pub unsafe fn asan_poison_shadow(address: usize, size: usize, value: u8) {
    // pmm_alloc_page is called before the kasan shadow map has been remapped r/w.
    // Do not attempt to poison memory in that case.
    if !is_asan_initialized() {
        core::hint::cold_path();
        return;
    }
    debug_assert!(size > 0);
    debug_assert!(value >= ASAN_SMALLEST_POISONED_VALUE); // only used for poisoning.

    let start_offset = address & ASAN_GRANULARITY_MASK;
    let mut shadow_addr_beg = addr2shadow(address);
    let shadow_addr_end = shadow_addr_beg.wrapping_add((start_offset + size) >> ASAN_SHIFT);

    let offset = start_offset as u8;
    let end_offset = ((start_offset + size) & ASAN_GRANULARITY_MASK) as u8;

    // We want to poison less than one shadow byte. This shadow byte
    // could be: Unpoisoned, Poisoned or Partially Poisoned.
    // For the poisoned case, we avoid repoisoning.
    // For the unpoisoned and partially poisoned cases, we can't leave gaps:
    // only poison if the last byte (end_offset) is already poisoned. In that
    // case, this might end up poisoning the entire byte, or could extend the
    // current partially poisoned area.
    if ptr::eq(shadow_addr_beg, shadow_addr_end) {
        // SAFETY: Caller guarantees `shadow_addr_beg` lies in writable KASAN shadow memory.
        let cur = unsafe { *shadow_addr_beg };
        // If the shadow byte is already poisoned, do nothing.
        if cur >= ASAN_SMALLEST_POISONED_VALUE {
            return;
        }
        // If the shadow byte is unpoisoned, do nothing.
        if cur == 0 {
            return;
        }
        // If the byte at address+size is not poisoned, do nothing: otherwise
        // there would be an unpoisoned gap.
        if cur > end_offset {
            return;
        }

        if offset != 0 {
            // Partially poison the shadow byte. Only override if we are expanding
            // the poisoned area.
            // SAFETY: `shadow_addr_beg` is writable shadow memory.
            unsafe {
                *shadow_addr_beg = if cur < offset { cur } else { offset };
            }
        } else {
            // Poison the entire byte.
            // SAFETY: `shadow_addr_beg` is writable shadow memory.
            unsafe {
                *shadow_addr_beg = value;
            }
        }
        return;
    }

    // We need to check whether we are partially poisoning the first shadow
    // byte, but only if it is not already poisoned.
    if offset != 0 {
        // SAFETY: `shadow_addr_beg` is writable shadow memory.
        let cur = unsafe { *shadow_addr_beg };
        if cur == 0 {
            // SAFETY: `shadow_addr_beg` is writable shadow memory.
            unsafe {
                *shadow_addr_beg = offset;
            }
        } else if cur < ASAN_SMALLEST_POISONED_VALUE {
            // value is partially poisoned, only override if we are poisoning more.
            // SAFETY: `shadow_addr_beg` is writable shadow memory.
            unsafe {
                *shadow_addr_beg = if cur < offset { cur } else { offset };
            }
        }
        shadow_addr_beg = shadow_addr_beg.wrapping_add(1);
    }

    // SAFETY: `shadow_addr_beg <= shadow_addr_end` both lie within writable KASAN shadow memory.
    unsafe {
        let count = shadow_addr_end.addr() - shadow_addr_beg.addr();
        __unsanitized_memset(shadow_addr_beg.cast::<c_void>(), c_int::from(value), count);
    }

    // If the last shadow byte (shadow_addr_end) is partially poisoned, we might
    // be completing the whole shadow byte, otherwise, don't touch it.
    // For example, if the last byte has 2 bytes unpoisoned, but our end_offset is
    // 3, it means that we can safely poison the entire shadow byte.
    if end_offset != 0 {
        // SAFETY: `shadow_addr_end` is writable shadow memory.
        let cur = unsafe { *shadow_addr_end };
        if cur > 0 && cur <= end_offset {
            // SAFETY: `shadow_addr_end` is writable shadow memory.
            unsafe {
                *shadow_addr_end = value;
            }
        }
    }
}

/// `asan_unpoison_shadow()` marks `[round_down(address, ASAN_GRANULARITY), address + size)` as
/// valid memory. Memory accesses to that region will not fail asan checks.
///
/// # Safety
///
/// The caller must ensure that `size > 0` and `[address, address + size)` lies within a region
/// backed by writable KASAN shadow memory.
#[sanitize(address = "off")]
pub unsafe fn asan_unpoison_shadow(address: usize, size: usize) {
    // pmm_alloc_page is called before the kasan shadow map has been remapped r/w.
    // Do not attempt to unpoison memory in that case.
    if !is_asan_initialized() {
        core::hint::cold_path();
        return;
    }
    debug_assert!(size > 0);
    let start_offset = address & ASAN_GRANULARITY_MASK;
    let shadow_addr_beg = addr2shadow(address);
    let shadow_addr_end = shadow_addr_beg.wrapping_add((start_offset + size) >> ASAN_SHIFT);

    // SAFETY: `shadow_addr_beg <= shadow_addr_end` both lie within writable KASAN shadow memory.
    unsafe {
        let count = shadow_addr_end.addr() - shadow_addr_beg.addr();
        __unsanitized_memset(shadow_addr_beg.cast::<c_void>(), 0, count);
    }

    // The last shadow byte should capture how many bytes
    // are unpoisoned at the end of the poisoned area.
    let end_offset = ((start_offset + size) & ASAN_GRANULARITY_MASK) as u8;
    if end_offset != 0 {
        // SAFETY: `shadow_addr_end` is writable shadow memory.
        let cur = unsafe { *shadow_addr_end };
        if cur >= ASAN_SMALLEST_POISONED_VALUE {
            // The entire byte is poisoned, unpoison up until end_offset.
            // SAFETY: `shadow_addr_end` is writable shadow memory.
            unsafe {
                *shadow_addr_end = end_offset;
            }
        } else if cur != 0 {
            // The byte is partially poisoned. See if we can increase the unpoisoned
            // region.
            // SAFETY: `shadow_addr_end` is writable shadow memory.
            unsafe {
                *shadow_addr_end = if cur > end_offset { cur } else { end_offset };
            }
        }
    }
}

/// Returns number of bytes to add to heap allocations of `size` for a redzone, to detect out-of
/// bounds accesses. (Rounds up the size to an ASAN granule)
#[sanitize(address = "off")]
pub const fn asan_heap_redzone_size(size: usize) -> usize {
    // The allocation end might not be aligned to an asan granule, so we add
    // the remaining part to the redzone size so that size + redzone_size
    // is aligned to an asan granule.
    let remaining = size.wrapping_neg() & ASAN_GRANULARITY_MASK;
    HEAP_RIGHT_REDZONE_SIZE + remaining
}
