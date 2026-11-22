// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;

use zx_types::zx_status_t;

use super::feature::{self, CpuidLeaf};

// TODO(https://fxbug.dev/540178114): Remove these FFI shims once all C++ callers are ported.

/// Queries CPUID leaf `num` and `subleaf`, writing the result into `*leaf` if supported
/// (`x86_get_cpuid_subleaf`).
///
/// Takes `num: u32` across `extern "C"` because C/C++ `enum x86_cpuid_leaf_num` can hold values
/// outside Rust's `#[repr(u32)] enum X86CpuidLeafNum`. Before `feature::early_init_percpu` runs,
/// `max_cpuid`, `max_hyp_cpuid`, and `max_ext_cpuid` are `0`, so leaves above `0` return `false`.
///
/// # Safety
///
/// The caller must ensure that `leaf` is non-null, properly aligned for [`CpuidLeaf`], valid for
/// writes of `size_of::<CpuidLeaf>()` bytes, and not accessed through any other pointer for the
/// duration of the call (as required to form `&mut MaybeUninit<CpuidLeaf>`).
// `get_cpuid_subleaf_raw` is `#[inline(always)]` and `x86_get_cpuid_subleaf` in `feature.h` marks
// the call `[[clang::always_inline]]` so ThinLTO inlines this across FFI (`rustc` rejects
// `#[inline]` on `#[no_mangle]` functions).
// SAFETY: Unique symbol name and C ABI signature matches `arch/x86/feature_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_get_cpuid_subleaf(
    num: u32,
    subleaf: u32,
    leaf: &mut MaybeUninit<CpuidLeaf>,
) -> bool {
    if let Some(res) = feature::get_cpuid_subleaf_raw(num, subleaf) {
        leaf.write(res);
        true
    } else {
        false
    }
}

/// Flushes the L1D cache on `ZX_ERR_INVALID_ARGS` or `ZX_ERR_BAD_HANDLE` syscall returns when
/// speculative execution mitigations are enabled (`x86_cpu_maybe_l1d_flush`, called from
/// `syscall.S`).
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0.
// SAFETY: Unique symbol name and C ABI signature matches `arch/x86/feature.h` and `syscall.S`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_cpu_maybe_l1d_flush(syscall_return: zx_status_t) {
    // SAFETY: The caller guarantees execution at CPL 0.
    unsafe { feature::cpu_maybe_l1d_flush(syscall_return) };
}
