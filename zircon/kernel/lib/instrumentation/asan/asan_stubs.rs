// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Compiler-generated KASAN runtime ABI entrypoints and stubs.
//!
//! LLVM provides no documentation on the ABI between the compiler and
//! the runtime.  The set of function signatures here was culled from
//! the LLVM sources for the compiler instrumentation and the runtime
//! (see llvm/lib/Transforms/Instrumentation/AddressSanitizer.cpp and
//! compiler-rt/lib/asan/*).

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use super::ASAN_GLOBAL_REDZONE_MAGIC;
use super::asan_internal::{AccessType, AsanGlobal, EarlyBootCell};
use super::asan_poisoning::{asan_check, asan_check_memory_overlap, asan_poison_shadow};

unsafe extern "C" {
    #[link_name = "llvm.returnaddress"]
    fn return_address(level: i32) -> *mut c_void;

    fn __unsanitized_memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn __unsanitized_memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn __unsanitized_memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void;
}

const ASAN_MAX_GLOBALS_REGIONS: usize = 500;

#[derive(Copy, Clone)]
struct GlobalsRegion {
    ptr: *const AsanGlobal,
    len: usize,
}

impl GlobalsRegion {
    const EMPTY: Self = Self { ptr: ptr::null(), len: 0 };
}

static GLOBALS_REGIONS: EarlyBootCell<[GlobalsRegion; ASAN_MAX_GLOBALS_REGIONS]> =
    EarlyBootCell::new([GlobalsRegion::EMPTY; ASAN_MAX_GLOBALS_REGIONS]);
static TOTAL_GLOBALS: EarlyBootCell<usize> = EarlyBootCell::new(0);

/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte copy.
#[sanitize(address = "off")]
#[inline]
unsafe fn asan_memcpy_impl(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
    caller: *mut c_void,
) -> *mut c_void {
    if n == 0 {
        return dst;
    }
    let dstptr = dst.addr();
    let srcptr = src.addr();

    // When src and dst are equal, skip the overlap check because LLVM requires
    // that memcpy handle the case where src and dest are equal (and the kernel's
    // implementations do).  See https://bugs.llvm.org/show_bug.cgi?id=11763 and
    // https://reviews.llvm.org/D86993.
    if dstptr != srcptr {
        asan_check_memory_overlap(dstptr, n, srcptr, n);
    }

    // SAFETY: Caller guarantees `src` and `dst` are valid kernel addresses.
    unsafe {
        asan_check(srcptr, n, AccessType::Read, caller);
        asan_check(dstptr, n, AccessType::Write, caller);
        __unsanitized_memcpy(dst, src, n)
    }
}

/// # Safety
///
/// `dst` must be valid for `n`-byte write.
#[sanitize(address = "off")]
#[inline]
unsafe fn asan_memset_impl(
    dst: *mut c_void,
    c: c_int,
    n: usize,
    caller: *mut c_void,
) -> *mut c_void {
    if n == 0 {
        return dst;
    }
    // SAFETY: Caller guarantees `dst` is a valid kernel address.
    unsafe {
        asan_check(dst.addr(), n, AccessType::Write, caller);
        __unsanitized_memset(dst, c, n)
    }
}

/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte move.
#[sanitize(address = "off")]
#[inline]
unsafe fn asan_memmove_impl(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
    caller: *mut c_void,
) -> *mut c_void {
    if n == 0 {
        return dst;
    }
    // SAFETY: Caller guarantees `src` and `dst` are valid kernel addresses.
    unsafe {
        asan_check(src.addr(), n, AccessType::Read, caller);
        asan_check(dst.addr(), n, AccessType::Write, caller);
        __unsanitized_memmove(dst, src, n)
    }
}

/// ASAN-checked `memcpy` implementation (`__asan_memcpy`).
///
/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte copy.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_memcpy(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `src` and `dst` are valid kernel addresses.
    unsafe { asan_memcpy_impl(dst, src, n, return_address(0)) }
}

/// ASAN-checked `memset` implementation (`__asan_memset`).
///
/// # Safety
///
/// `dst` must be valid for `n`-byte write.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `dst` is a valid kernel address.
    unsafe { asan_memset_impl(dst, c, n, return_address(0)) }
}

/// ASAN-checked `memmove` implementation (`__asan_memmove`).
///
/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte move.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_memmove(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `src` and `dst` are valid kernel addresses.
    unsafe { asan_memmove_impl(dst, src, n, return_address(0)) }
}

/// Strong `memcpy` definition overriding the weak libc symbol under KASAN.
///
/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte copy.
#[sanitize(address = "off")]
#[unsafe(export_name = "memcpy")]
pub unsafe extern "C" fn asan_memcpy_alias(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `src` and `dst` are valid kernel addresses.
    unsafe { asan_memcpy_impl(dst, src, n, return_address(0)) }
}

/// Strong `memmove` definition overriding the weak libc symbol under KASAN.
///
/// # Safety
///
/// `dst` and `src` must be valid for `n`-byte move.
#[sanitize(address = "off")]
#[unsafe(export_name = "memmove")]
pub unsafe extern "C" fn asan_memmove_alias(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `src` and `dst` are valid kernel addresses.
    unsafe { asan_memmove_impl(dst, src, n, return_address(0)) }
}

/// Strong `memset` definition overriding the weak libc symbol under KASAN.
///
/// # Safety
///
/// `dst` must be valid for `n`-byte write.
#[sanitize(address = "off")]
#[unsafe(export_name = "memset")]
pub unsafe extern "C" fn asan_memset_alias(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    // SAFETY: `return_address(0)` reads the current frame's return address, and caller guarantees
    // `dst` is a valid kernel address.
    unsafe { asan_memset_impl(dst, c, n, return_address(0)) }
}

/// This is referenced by generated code to decide whether to call
/// `__asan_stack_malloc_*` instead of doing normal stack allocation.
/// Never use stack malloc before the real runtime library is loaded.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static __asan_option_detect_stack_use_after_return: c_int = 0;

// This is the one set of things we define for real just as the
// sanitizer runtime does.  Generated code calls these.  In practice,
// almost certainly nothing in the startup path needs them, but
// defining them properly is barely more than defining trap stubs.
macro_rules! asan_set_shadow_xx {
    ($name:ident, $val:expr) => {
        /// Fills `[addr, addr + size)` in the shadow map with `$val`.
        ///
        /// # Safety
        ///
        /// `[addr, addr + size)` must be a valid writable shadow range.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(addr: usize, size: usize) {
            // SAFETY: Caller guarantees `[addr, addr + size)` is a valid writable shadow range.
            unsafe {
                __unsanitized_memset(ptr::with_exposed_provenance_mut::<c_void>(addr), $val, size);
            }
        }
    };
}

asan_set_shadow_xx!(__asan_set_shadow_00, 0x00);
asan_set_shadow_xx!(__asan_set_shadow_f1, 0xf1);
asan_set_shadow_xx!(__asan_set_shadow_f2, 0xf2);
asan_set_shadow_xx!(__asan_set_shadow_f3, 0xf3);
asan_set_shadow_xx!(__asan_set_shadow_f5, 0xf5);
asan_set_shadow_xx!(__asan_set_shadow_f8, 0xf8);

// Everything else is stubs that panic.  They should never be called.

// These are only called when a bug is found.  So unless there's
// an actual bug in code that's on the dynamic linker startup path,
// they'll never be called.

// This is the same macro used in compiler-rt/lib/asan/asan_rtl.cc,
// where it makes use of the is_write argument.  The list of invocations
// of this macro below is taken verbatim from that file.
macro_rules! asan_report_error {
    ($report:ident, $report_exp:ident, $report_noabort:ident, $access_type:expr, $size:expr) => {
        /// Reports a KASAN access error of fixed size.
        ///
        /// # Safety
        ///
        /// `addr` must lie within the kernel address space covered by KASAN shadow memory.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $report(addr: usize) {
            // SAFETY: `return_address(0)` reads the current frame's return address.
            unsafe {
                let caller = return_address(0);
                asan_check(addr, $size, $access_type, caller);
            }
        }

        /// Reports a KASAN access error of fixed size with experiment mask.
        ///
        /// # Safety
        ///
        /// `addr` must lie within the kernel address space covered by KASAN shadow memory.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $report_exp(addr: usize, _exp: u32) {
            // SAFETY: `return_address(0)` reads the current frame's return address.
            unsafe {
                let caller = return_address(0);
                asan_check(addr, $size, $access_type, caller);
            }
        }

        /// Reports a KASAN access error of fixed size (noabort variant).
        ///
        /// # Safety
        ///
        /// `addr` must lie within the kernel address space covered by KASAN shadow memory.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $report_noabort(addr: usize) {
            // SAFETY: `return_address(0)` reads the current frame's return address.
            unsafe {
                let caller = return_address(0);
                asan_check(addr, $size, $access_type, caller);
            }
        }
    };
}

asan_report_error!(
    __asan_report_load1,
    __asan_report_exp_load1,
    __asan_report_load1_noabort,
    AccessType::Read,
    1
);
asan_report_error!(
    __asan_report_load2,
    __asan_report_exp_load2,
    __asan_report_load2_noabort,
    AccessType::Read,
    2
);
asan_report_error!(
    __asan_report_load4,
    __asan_report_exp_load4,
    __asan_report_load4_noabort,
    AccessType::Read,
    4
);
asan_report_error!(
    __asan_report_load8,
    __asan_report_exp_load8,
    __asan_report_load8_noabort,
    AccessType::Read,
    8
);
asan_report_error!(
    __asan_report_load16,
    __asan_report_exp_load16,
    __asan_report_load16_noabort,
    AccessType::Read,
    16
);
asan_report_error!(
    __asan_report_store1,
    __asan_report_exp_store1,
    __asan_report_store1_noabort,
    AccessType::Write,
    1
);
asan_report_error!(
    __asan_report_store2,
    __asan_report_exp_store2,
    __asan_report_store2_noabort,
    AccessType::Write,
    2
);
asan_report_error!(
    __asan_report_store4,
    __asan_report_exp_store4,
    __asan_report_store4_noabort,
    AccessType::Write,
    4
);
asan_report_error!(
    __asan_report_store8,
    __asan_report_exp_store8,
    __asan_report_store8_noabort,
    AccessType::Write,
    8
);
asan_report_error!(
    __asan_report_store16,
    __asan_report_exp_store16,
    __asan_report_store16_noabort,
    AccessType::Write,
    16
);

/// Reports a variable-length KASAN load error.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_load_n(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Read, return_address(0));
    }
}

/// Reports a variable-length KASAN load error (noabort variant).
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_load_n_noabort(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Read, return_address(0));
    }
}

/// Reports a variable-length KASAN load error with experiment mask.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_exp_load_n(addr: usize, size: usize, _exp: u32) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Read, return_address(0));
    }
}

/// Reports a variable-length KASAN store error.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_store_n(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Write, return_address(0));
    }
}

/// Reports a variable-length KASAN store error (noabort variant).
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_store_n_noabort(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Write, return_address(0));
    }
}

/// Reports a variable-length KASAN store error with experiment mask.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_exp_store_n(addr: usize, size: usize, _exp: u32) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Write, return_address(0));
    }
}

// These are called when not using the inline instrumentation that calls the
// ASAN_REPORT_ERROR functions for poisoned accesses.  Instead, calls to these
// functions are generated unconditionally before an access to perform the
// poison check.

/// Checks a variable-length load of `size` bytes at `addr`.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[allow(non_snake_case)]
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_loadN(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Read, return_address(0));
    }
}

/// Checks a variable-length store of `size` bytes at `addr`.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[allow(non_snake_case)]
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_storeN(addr: usize, size: usize) {
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Write, return_address(0));
    }
}

/// Emitted unconditionally for large Rust references under
/// `-Cllvm-args=-asan-instrument-assume-dereferenceable=1` (e.g., `&PerCpu`).
/// Despite the `__asan_report_*` naming prefix, LLVM invokes these helpers directly on the normal
/// execution path when `size > 64` expecting a normal return when unpoisoned, so they must call
/// `asan_check(...)` rather than panicking unconditionally.
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_assume_dereferenceable(addr: usize, size: usize) {
    if size == 0 {
        return;
    }
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Assumption, return_address(0));
    }
}

/// Noabort variant of [`__asan_report_assume_dereferenceable`].
///
/// # Safety
///
/// `[addr, addr + size)` must lie within the kernel address space covered by KASAN shadow memory.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_report_assume_dereferenceable_noabort(addr: usize, size: usize) {
    if size == 0 {
        return;
    }
    // SAFETY: `return_address(0)` reads the current frame's return address.
    unsafe {
        asan_check(addr, size, AccessType::Assumption, return_address(0));
    }
}

// This is the same macro used in compiler-rt/lib/asan/asan_rtl.cc,
// where it makes use of the is_write argument.  The list of invocations
// of this macro below is taken verbatim from that file.
macro_rules! asan_memory_access_callback {
    ($cb:ident, $cb_exp:ident, $access_type:expr, $size:expr) => {
        /// Checks a fixed-size memory access at `addr`.
        ///
        /// # Safety
        ///
        /// `addr` must lie within the kernel address space covered by KASAN shadow memory.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $cb(addr: usize) {
            // SAFETY: `return_address(0)` reads the current frame's return address.
            unsafe {
                asan_check(addr, $size, $access_type, return_address(0));
            }
        }

        /// Checks a fixed-size memory access at `addr` with experiment mask.
        ///
        /// # Safety
        ///
        /// `addr` must lie within the kernel address space covered by KASAN shadow memory.
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $cb_exp(addr: usize, _exp: u32) {
            // SAFETY: `return_address(0)` reads the current frame's return address.
            unsafe {
                asan_check(addr, $size, $access_type, return_address(0));
            }
        }
    };
}

asan_memory_access_callback!(__asan_load1, __asan_exp_load1, AccessType::Read, 1);
asan_memory_access_callback!(__asan_load2, __asan_exp_load2, AccessType::Read, 2);
asan_memory_access_callback!(__asan_load4, __asan_exp_load4, AccessType::Read, 4);
asan_memory_access_callback!(__asan_load8, __asan_exp_load8, AccessType::Read, 8);
asan_memory_access_callback!(__asan_load16, __asan_exp_load16, AccessType::Read, 16);
asan_memory_access_callback!(__asan_store1, __asan_exp_store1, AccessType::Write, 1);
asan_memory_access_callback!(__asan_store2, __asan_exp_store2, AccessType::Write, 2);
asan_memory_access_callback!(__asan_store4, __asan_exp_store4, AccessType::Write, 4);
asan_memory_access_callback!(__asan_store8, __asan_exp_store8, AccessType::Write, 8);
asan_memory_access_callback!(__asan_store16, __asan_exp_store16, AccessType::Write, 16);

/// This is called before calling any `[[noreturn]]` function.  In the userland
/// runtime, it's used to clean up per-thread "fake stack" allocations.  In the
/// kernel, all per-thread cleanup is done explicitly.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_handle_no_return() {}

/// Called in normal operation when poisoning a C++ `operator new[]` array cookie.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_poison_cxx_array_cookie(_p: usize) {}

/// Called in normal operation when loading a C++ `operator delete[]` array cookie.
///
/// # Safety
///
/// `p` must be a valid readable pointer to a `usize` array cookie.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_load_cxx_array_cookie(p: *const usize) -> usize {
    // SAFETY: Caller guarantees `p` is a valid pointer.
    unsafe { *p }
}

// These are sometimes called in normal operation.  But they're never
// called by any of the code on the startup path, so we can get away
// with making them trap stubs.

macro_rules! define_stack_malloc_free_with_class_id {
    ($malloc:ident, $free:ident) => {
        /// Unimplemented stack-malloc stub (`-asan-stack=0`).
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub extern "C" fn $malloc(_size: usize) -> usize {
            panic!("address sanitizer failure ({})", stringify!($malloc));
        }

        /// Unimplemented stack-free stub (`-asan-stack=0`).
        #[sanitize(address = "off")]
        #[unsafe(no_mangle)]
        pub extern "C" fn $free(_ptr: usize, _size: usize) {
            panic!("address sanitizer failure ({})", stringify!($free));
        }
    };
}

define_stack_malloc_free_with_class_id!(__asan_stack_malloc_0, __asan_stack_free_0);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_1, __asan_stack_free_1);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_2, __asan_stack_free_2);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_3, __asan_stack_free_3);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_4, __asan_stack_free_4);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_5, __asan_stack_free_5);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_6, __asan_stack_free_6);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_7, __asan_stack_free_7);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_8, __asan_stack_free_8);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_9, __asan_stack_free_9);
define_stack_malloc_free_with_class_id!(__asan_stack_malloc_10, __asan_stack_free_10);

/// Unimplemented `alloca` poison stub.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_alloca_poison(_addr: usize, _size: usize) {
    panic!("address sanitizer failure (__asan_alloca_poison)");
}

/// Unimplemented `alloca` unpoison stub.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_allocas_unpoison(_top: usize, _bottom: usize) {
    panic!("address sanitizer failure (__asan_allocas_unpoison)");
}

/// Records an array of instrumented globals registered by a module's static constructor.
///
/// # Safety
///
/// Must only be called during single-threaded early boot static constructor execution, with
/// `globals` pointing to `size` valid `'static` `AsanGlobal` entries. Every registered
/// `AsanGlobal` must satisfy `size_with_redzone > size` and have its redzone
/// `[begin + size, begin + size_with_redzone)` lying within writable KASAN shadow memory once
/// KASAN shadow initialization completes.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __asan_register_globals(globals: *const AsanGlobal, size: usize) {
    // SAFETY: Called only during single-threaded early boot constructor execution, so exclusive
    // access to `TOTAL_GLOBALS` and `GLOBALS_REGIONS` is guaranteed.
    unsafe {
        let total_ptr = EarlyBootCell::as_ptr(&raw const TOTAL_GLOBALS);
        let i = *total_ptr;
        *total_ptr = i + 1;
        if i < ASAN_MAX_GLOBALS_REGIONS {
            *EarlyBootCell::as_ptr(&raw const GLOBALS_REGIONS)
                .cast::<GlobalsRegion>()
                .wrapping_add(i) = GlobalsRegion { ptr: globals, len: size };
        }
    }
}

/// Unregistering globals is unreachable in the kernel.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_unregister_globals(_globals: *const AsanGlobal, _size: usize) {
    panic!("__asan_unregister_globals should be unreachable code");
}

/// Poisons the redzones of all globals registered during early boot once KASAN shadow memory is
/// writable.
///
/// # Safety
///
/// Must be called once during single-threaded boot (`LK_INIT_LEVEL_VM`) after writable KASAN
/// shadow memory has been initialized.
#[sanitize(address = "off")]
pub(super) unsafe fn asan_register_globals_late() {
    // SAFETY: Called once during single-threaded `LK_INIT_LEVEL_VM` after all early boot
    // `__asan_register_globals` calls have completed. Each registered `GlobalsRegion` in
    // `0..total` holds a valid `'static` slice of `region.len` `AsanGlobal` descriptors whose
    // redzones (`[begin + size, begin + size_with_redzone)`) are non-empty and lie within
    // writable KASAN shadow memory.
    unsafe {
        let total = *EarlyBootCell::as_ptr(&raw const TOTAL_GLOBALS);
        assert!(total <= ASAN_MAX_GLOBALS_REGIONS, "{} > {}", total, ASAN_MAX_GLOBALS_REGIONS);
        let regions_ptr = EarlyBootCell::as_ptr(&raw const GLOBALS_REGIONS).cast::<GlobalsRegion>();
        let mut i = 0;
        while i < total {
            let region = *regions_ptr.wrapping_add(i);
            let mut j = 0;
            while j < region.len {
                let global = &*region.ptr.wrapping_add(j);
                asan_poison_shadow(
                    global.begin.addr() + global.size,
                    global.size_with_redzone - global.size,
                    ASAN_GLOBAL_REDZONE_MAGIC,
                );
                j += 1;
            }
            i += 1;
        }
    }
}

// TODO(https://fxbug.dev/42104852): Figure out what dynamic_init is doing.
/// Stub called before dynamic initialization of globals in `module`.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_before_dynamic_init(_module: *const c_char) {}

/// Stub called after dynamic initialization of globals.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_after_dynamic_init() {}

// These are called by static constructor code to initialize the sanitizer
// runtime.  There's no need for those calls in the kernel, since the
// initialization is all done explicitly.
/// No-op static constructor entrypoint (`__asan_init`).
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_init() {}

/// No-op compiler ABI version check entrypoint (`__asan_version_mismatch_check_v8`).
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub extern "C" fn __asan_version_mismatch_check_v8() {}
