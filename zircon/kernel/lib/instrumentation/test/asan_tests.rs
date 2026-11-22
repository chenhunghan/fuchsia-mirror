// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel Address Sanitizer (`kasan`) unit tests.

#![cfg(ktest)]

/// Kernel Address Sanitizer (`kasan`) unit tests.
#[cfg(sanitize = "address")]
#[unittest::suite(name = "kasan")]
mod tests {
    use core::ffi::c_void;
    use core::mem::size_of;
    use core::ptr;

    use fbl::Array;
    use unittest::{assert_eq, assert_ne, expect_eq, expect_true, unwrap_ok};
    #[cfg(not(target_arch = "x86_64"))]
    use unittest::{assert_ok, unwrap_some};

    #[cfg(not(target_arch = "x86_64"))]
    use crate::arch_rs::KERNEL_ASPACE_BASE;
    #[cfg(target_arch = "x86_64")]
    use crate::instrumentation::asan::asan_internal::ASAN_SHADOW_SIZE;
    use crate::instrumentation::asan::asan_internal::{ASAN_GRANULARITY, ASAN_SHIFT};
    #[cfg(not(target_arch = "x86_64"))]
    use crate::instrumentation::asan::asan_map_shadow_for;
    use crate::instrumentation::asan::{
        ASAN_HEAP_LEFT_REDZONE_MAGIC, KASAN_SHADOW_OFFSET, QUARANTINE_ELEMENTS, Quarantine,
        asan_address_is_poisoned, asan_entire_region_is_poisoned, asan_poison_shadow,
        asan_region_is_poisoned, asan_unpoison_shadow,
    };
    #[cfg(not(target_arch = "x86_64"))]
    use crate::vm::vm_address_region::flag as vmar_flag;
    #[cfg(not(target_arch = "x86_64"))]
    use crate::vm::vm_aspace::VmAspace;

    unsafe extern "C" {
        static g_asan_test_global_buffer: [u8; 8];

        fn malloc(size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
        #[cfg(not(target_arch = "x86_64"))]
        fn __unsanitized_memset(dst: *mut c_void, c: core::ffi::c_int, n: usize) -> *mut c_void;
    }

    // TODO(https://fxbug.dev/42104852): Enable for x86_64 once it can add shadow mappings after
    // early init.
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    fn test_addr2shadow(address: usize) -> *mut u8 {
        let kasan_shadow_map = ptr::with_exposed_provenance_mut::<u8>(KASAN_SHADOW_OFFSET);
        kasan_shadow_map.wrapping_add((address - KERNEL_ASPACE_BASE) >> ASAN_SHIFT)
    }

    /// Makes sure that a region returned by malloc is unpoisoned.
    #[test]
    fn malloc_unpoisons() {
        const SIZES: [usize; 5] = [1, 10, 32, 1023, 1024];

        for size in SIZES {
            // SAFETY: Allocates and frees `size` bytes on the kernel heap and checks its shadow.
            unsafe {
                let m = malloc(size);
                assert_ne!(m, ptr::null_mut());
                expect_eq!(0, asan_region_is_poisoned(m.addr(), size));
                free(m);
            }
        }
    }

    /// Makes sure that a region recently freed is poisoned.
    // TODO(https://fxbug.dev/42129397): Test is flaky. Fix and re-enable.
    #[test]
    #[ignore]
    fn free_poisons() {
        const SIZES: [usize; 5] = [1, 10, 32, 1023, 1024];

        for size in SIZES {
            // SAFETY: Allocates and frees `size` bytes on the kernel heap and checks its shadow.
            unsafe {
                let m = malloc(size);
                assert_ne!(m, ptr::null_mut());
                free(m);
                expect_true!(asan_entire_region_is_poisoned(m.addr(), size));
            }
        }
    }

    /// Makes sure that the surrounding parts of a buffer are poisoned.
    #[test]
    fn detects_buffer_overflows() {
        const SIZES: [usize; 11] = [1, 2, 3, 4, 5, 6, 7, 10, 32, 1023, 1024];

        for size in SIZES {
            // SAFETY: Allocates and frees `size` bytes on the kernel heap and checks adjacent
            // redzone bytes in the shadow map.
            unsafe {
                let m = malloc(size);
                assert_ne!(m, ptr::null_mut());
                let base = m.addr();
                expect_true!(asan_address_is_poisoned(base + size));
                expect_true!(asan_address_is_poisoned(base - 1));
                free(m);
            }
        }
    }

    /// Makes sure that a region from the heap can be poisoned.
    #[test]
    fn test_poisoning_heap() {
        const SIZES: [usize; 12] = [1, 2, 3, 5, 7, 8, 9, 11, 15, 16, 17, 19];

        const BUF_SZ: usize = 1024;
        let buf = unwrap_ok!(Array::<u8>::try_new(BUF_SZ));
        let bufptr = buf.as_ptr().addr();
        let _unpoison = zr::defer(|| {
            // SAFETY: `buf` outlives `_unpoison` and is backed by KASAN shadow memory.
            unsafe { asan_unpoison_shadow(bufptr, BUF_SZ) };
        });
        // SAFETY: `buf` is a live `BUF_SZ`-byte heap allocation backed by KASAN shadow memory.
        unsafe {
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));
            for size in SIZES {
                let poisoned_size = size & !((1 << ASAN_SHIFT) - 1);
                asan_poison_shadow(bufptr, size, ASAN_HEAP_LEFT_REDZONE_MAGIC);
                expect_true!(asan_entire_region_is_poisoned(bufptr, poisoned_size));
                asan_unpoison_shadow(bufptr, BUF_SZ);
                expect_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));
            }
        }
    }

    /// Make sure poison checks work in partially poisoned regions.
    #[test]
    fn test_poisoning_heap_partial() {
        const BUF_SZ: usize = 128;
        let buf = unwrap_ok!(Array::<u8>::try_new(BUF_SZ));
        let bufptr = buf.as_ptr().addr();
        let _unpoison = zr::defer(|| {
            // SAFETY: `buf` outlives `_unpoison` and is backed by KASAN shadow memory.
            unsafe { asan_unpoison_shadow(bufptr, BUF_SZ) };
        });
        // SAFETY: `buf` is a live `BUF_SZ`-byte heap allocation backed by KASAN shadow memory.
        unsafe {
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));

            // Leave the first and last two granules unpoisoned.
            let poison_start = bufptr + (2 << ASAN_SHIFT);
            let poison_size = BUF_SZ - (4 << ASAN_SHIFT);

            asan_poison_shadow(poison_start, poison_size, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            expect_eq!(poison_start, asan_region_is_poisoned(bufptr, BUF_SZ));

            asan_unpoison_shadow(poison_start, poison_size);
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));
        }
    }

    /// Make sure poisoning and unpoisoning at unaligned offsets works.
    #[test]
    fn unaligned_poison() {
        const BUF_SZ: usize = 128;
        let buf = unwrap_ok!(Array::<u8>::try_new(BUF_SZ));
        let bufptr = buf.as_ptr().addr();
        let _unpoison = zr::defer(|| {
            // SAFETY: `buf` outlives `_unpoison` and is backed by KASAN shadow memory.
            unsafe { asan_unpoison_shadow(bufptr, BUF_SZ) };
        });
        // SAFETY: `buf` is a live `BUF_SZ`-byte heap allocation backed by KASAN shadow memory.
        unsafe {
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));

            // Poison from byte 4 onwards.
            let poison_skip = 3;
            asan_poison_shadow(
                bufptr + poison_skip,
                BUF_SZ - poison_skip,
                ASAN_HEAP_LEFT_REDZONE_MAGIC,
            );
            expect_eq!(bufptr + poison_skip, asan_region_is_poisoned(bufptr, BUF_SZ));
            expect_true!(asan_entire_region_is_poisoned(
                bufptr + poison_skip,
                BUF_SZ - poison_skip
            ));

            // Unpoison the last chunk
            let unpoison_start = 2 * ASAN_GRANULARITY - 1;
            asan_unpoison_shadow(bufptr + unpoison_start, BUF_SZ - unpoison_start);
            expect_eq!(
                0,
                asan_region_is_poisoned(bufptr + unpoison_start, BUF_SZ - unpoison_start)
            );
            // It didn't unpoison the first asan granule.
            expect_true!(asan_entire_region_is_poisoned(
                bufptr + poison_skip,
                ASAN_GRANULARITY - poison_skip
            ));

            // Poisoning the third byte onwards should increase the left poison size.
            asan_poison_shadow(
                bufptr + poison_skip - 1,
                BUF_SZ - poison_skip + 1,
                ASAN_HEAP_LEFT_REDZONE_MAGIC,
            );
            expect_eq!(bufptr + poison_skip - 1, asan_region_is_poisoned(bufptr, BUF_SZ));
            expect_true!(asan_entire_region_is_poisoned(
                bufptr + poison_skip - 1,
                BUF_SZ - poison_skip + 1
            ));

            // Unpoison the fourth byte should unpoison bytes 0, 1, 2 and 3.
            asan_unpoison_shadow(bufptr + poison_skip, 1);
            expect_eq!(bufptr + poison_skip + 1, asan_region_is_poisoned(bufptr, BUF_SZ));
            expect_true!(asan_entire_region_is_poisoned(
                bufptr + poison_skip + 1,
                BUF_SZ - poison_skip
            ));
        }
    }

    /// Make sure poisoning less than an entire granule works.
    #[test]
    fn small_poison() {
        const BUF_SZ: usize = 128;
        let buf = unwrap_ok!(Array::<u8>::try_new(BUF_SZ));
        let bufptr = buf.as_ptr().addr();
        let _unpoison = zr::defer(|| {
            // SAFETY: `buf` outlives `_unpoison` and is backed by KASAN shadow memory.
            unsafe { asan_unpoison_shadow(bufptr, BUF_SZ) };
        });
        // SAFETY: `buf` is a live `BUF_SZ`-byte heap allocation backed by KASAN shadow memory.
        unsafe {
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));

            // If we try to poison the first ASAN_GRANULARITY - 1 bytes into an unpoisoned
            // region, it shouldn't do anything.
            asan_poison_shadow(bufptr, ASAN_GRANULARITY - 1, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));

            // Poison from byte 3 onwards.
            let poison_skip = 3;
            asan_poison_shadow(
                bufptr + poison_skip,
                BUF_SZ - poison_skip,
                ASAN_HEAP_LEFT_REDZONE_MAGIC,
            );
            assert_eq!(bufptr + poison_skip, asan_region_is_poisoned(bufptr, BUF_SZ));

            // If we poison the first 2 bytes, nothing should happen.
            asan_poison_shadow(bufptr, poison_skip - 1, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            expect_eq!(bufptr + poison_skip, asan_region_is_poisoned(bufptr, BUF_SZ));

            // Poisoning the third byte should increase the poisoned region.
            asan_poison_shadow(bufptr + poison_skip - 1, 1, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            expect_eq!(bufptr + poison_skip - 1, asan_region_is_poisoned(bufptr, BUF_SZ));

            // Poisoning from the start should make the whole range poisoned.
            asan_poison_shadow(bufptr, poison_skip, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            expect_true!(asan_entire_region_is_poisoned(bufptr, BUF_SZ));

            asan_unpoison_shadow(bufptr, BUF_SZ);
            assert_eq!(0, asan_region_is_poisoned(bufptr, BUF_SZ));
        }
    }

    /// Make sure `Quarantine::push` stores and evicts elements in FIFO order.
    #[test]
    fn test_quarantine() {
        const FIRST_PTR: usize = 0xA1FA1FA;
        // Allocate on the heap so `Quarantine<c_void>` (512 KiB) does not overflow the kernel
        // thread stack.
        let uninit = unwrap_ok!(kalloc::Box::<Quarantine<c_void>>::try_new_zeroed());
        // SAFETY: `Quarantine<c_void>` contains only `[*mut c_void; QUARANTINE_ELEMENTS]` and
        // `usize`, for which an all-zero bit pattern is valid.
        let mut test_quarantine = unsafe { uninit.assume_init() };

        for i in 0..QUARANTINE_ELEMENTS {
            let fake_pointer = ptr::without_provenance_mut::<c_void>(FIRST_PTR + i);
            let retrieved = test_quarantine.push(fake_pointer);
            expect_eq!(retrieved, ptr::null_mut());
        }
        for i in 0..QUARANTINE_ELEMENTS {
            let retrieved = test_quarantine.push(ptr::null_mut());
            expect_eq!(retrieved, ptr::without_provenance_mut::<c_void>(FIRST_PTR + i));
        }
        expect_eq!(ptr::null_mut(), test_quarantine.push(ptr::null_mut()));
    }

    /// Read one byte from every page of the KASAN shadow to check shadow page tables.
    #[test]
    fn test_walk_shadow() {
        // This test is only valid on x86-64, because it has the full shadow memory
        // mapped (with some parts as read-only).
        #[cfg(target_arch = "x86_64")]
        {
            let start = ptr::with_exposed_provenance::<u8>(KASAN_SHADOW_OFFSET);
            let end = ptr::with_exposed_provenance::<u8>(KASAN_SHADOW_OFFSET + ASAN_SHADOW_SIZE);
            let mut p = start;
            while p < end {
                // SAFETY: Every page in `[KASAN_SHADOW_OFFSET, +ASAN_SHADOW_SIZE)` is mapped
                // readable on x86-64.
                unsafe {
                    let _ = ptr::read_volatile(p); // Read one byte from each page of the shadow.
                }
                p = p.wrapping_add(page::SIZE);
            }
        }
    }

    /// Test that `asan_map_shadow_for` makes a writable shadow region for address ranges.
    #[sanitize(address = "off")]
    #[test]
    fn test_asan_map_shadow_for() {
        // asan_remap_shadow is normally only allowed to be called in early boot; this test is safe
        // to use it, however, because it only runs on one CPU.
        // TODO(https://fxbug.dev/42104852): Enable for x86_64 once it can add shadow mappings
        // after early init.
        #[cfg(not(target_arch = "x86_64"))]
        {
            let kernel_vmar = unwrap_some!(VmAspace::kernel_aspace().root_vmar());
            let test_vmar = unwrap_ok!(kernel_vmar.create_sub_vmar(
                /*offset=*/ 0,
                /*size=*/ (2 * page::SIZE) << ASAN_SHIFT,
                /*align_pow2=*/ ASAN_SHIFT as u8,
                /*vmar_flags=*/ vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                b"kasan_test_remap_shadow",
            ));

            let test_vmar_shadow_start = test_addr2shadow(test_vmar.base().0);
            let test_vmar_shadow_end = test_addr2shadow(test_vmar.base().0 + test_vmar.size());

            // SAFETY: `[test_vmar.base(), +test_vmar.size())` is a freshly reserved kernel VMAR.
            unsafe {
                asan_map_shadow_for(test_vmar.base().0, test_vmar.size());

                // Walk the shadow after asan_map_shadow_for and write to every page. The write
                // should succeed and write to newly-allocated shadow pages.
                let mut p = test_vmar_shadow_start;
                while p < test_vmar_shadow_end {
                    ptr::write_volatile(p, 1);
                    p = p.wrapping_add(page::SIZE);
                }
                let shadow_len = test_vmar_shadow_end.addr() - test_vmar_shadow_start.addr();
                __unsanitized_memset(test_vmar_shadow_start.cast::<c_void>(), 0, shadow_len);
            }

            assert_ok!(test_vmar.destroy());
        }
    }

    /// Make sure global buffer redzones are poisoned while the global itself is unpoisoned.
    #[test]
    fn test_globals() {
        let buf = (&raw const g_asan_test_global_buffer).addr();
        let buf_size = size_of::<[u8; 8]>();
        // SAFETY: `g_asan_test_global_buffer` and its trailing redzone lie in the kernel image
        // backed by KASAN shadow memory.
        unsafe {
            expect_true!(asan_address_is_poisoned(buf + buf_size));
            expect_eq!(0, asan_region_is_poisoned(buf, buf_size));
        }
    }

    /// Verify that `memcpy` succeeds when `dst` equals `src` (https://fxbug.dev/42143915).
    #[test]
    fn memcpy_dst_src_equal() {
        let mut buffer = [0u8; 256];
        let ptr = buffer.as_mut_ptr().cast::<c_void>();
        // SAFETY: `buffer` is a valid 256-byte stack buffer, and `memcpy` permits `dst == src`.
        unsafe {
            memcpy(ptr, ptr, buffer.len());
        }
    }
}
