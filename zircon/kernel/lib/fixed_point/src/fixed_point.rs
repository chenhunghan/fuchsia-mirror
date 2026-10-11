// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! 32.64 unsigned fixed point arithmetic, mirroring `<lib/fixed_point.h>`.

use core::ptr::NonNull;

use crate::fixed_point_debug::{
    debug_mul_u32_u32, debug_u32_mul_u64_fp32_64, debug_u64_mul_u32_fp32_64,
    debug_u64_mul_u64_fp32_64,
};

/// A 32.64 unsigned fixed point number: 32 integer bits followed by 64
/// fractional bits, stored as three little-endian-by-significance limbs.
///
/// Layout-compatible with the C `struct fp_32_64`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fp3264 {
    /// unshifted value
    pub l0: u32,
    /// value shifted left 32 bits (or bit -1 to -32)
    pub l32: u32,
    /// value shifted left 64 bits (or bit -33 to -64)
    pub l64: u32,
}

zr::static_assert_size_and_align!(Fp3264, 12, 4);

/// Computes `dividend / divisor` as a 32.64 fixed point value, truncating
/// any fraction below the 64th fractional bit, and stores the result into `result`.
///
/// # Panics
///
/// Panics if `divisor` is zero (undefined behaviour in the C original).
///
/// # Safety
///
/// `result` must be valid for writes and properly aligned.
#[inline]
pub unsafe fn fp_32_64_div_32_32(result: NonNull<Fp3264>, dividend: u32, divisor: u32) {
    let mut tmp = (u64::from(dividend) << 32) / u64::from(divisor);
    let rem = ((u64::from(dividend) << 32) % u64::from(divisor)) as u32;
    // SAFETY: Caller guarantees `result` is properly aligned and valid for writes.
    unsafe {
        let res = result.as_ptr();
        (*res).l0 = (tmp >> 32) as u32;
        (*res).l32 = tmp as u32;
        tmp = (u64::from(rem) << 32) / u64::from(divisor);
        (*res).l64 = tmp as u32;
    }
}

/// Multiplies two 32-bit operands into a 64-bit product. `a_shift` and
/// `b_shift` describe the bit position of each operand relative to the binary
/// point and are used purely for debug tracing.
#[inline]
pub(crate) fn mul_u32_u32(a: u32, b: u32, a_shift: i32, b_shift: i32) -> u64 {
    let ret = u64::from(a) * u64::from(b);
    debug_mul_u32_u32(a, b, a_shift, b_shift, ret);
    ret
}

/// Multiplies a 32-bit integer `a` by the 32.64 fixed point value `b`,
/// returning the product rounded to the nearest 64-bit integer.
#[inline]
pub fn u64_mul_u32_fp32_64(a: u32, b: Fp3264) -> u64 {
    let mut res_0 = mul_u32_u32(a, b.l0, 0, 0);
    let tmp = mul_u32_u32(a, b.l32, 0, -32);
    res_0 = res_0.wrapping_add(tmp >> 32);
    let mut res_l32 = u64::from(tmp as u32);
    res_l32 = res_l32.wrapping_add(mul_u32_u32(a, b.l64, 0, -64) >> 32); // Improve rounding accuracy
    res_0 = res_0.wrapping_add(res_l32 >> 32);
    let res_l32_32 = res_l32 as u32;
    let ret = res_0.wrapping_add(u64::from(res_l32_32 >> 31)); // Round to nearest integer

    debug_u64_mul_u32_fp32_64(a, b, res_0, res_l32_32, ret);

    ret
}

/// Multiplies a 64-bit integer `a` by the 32.64 fixed point value `b`,
/// returning the low 32 bits of the product rounded to the nearest integer.
#[inline]
pub fn u32_mul_u64_fp32_64(a: u64, b: Fp3264) -> u32 {
    let a_r32 = (a >> 32) as u32;
    let a_0 = a as u32;

    // mul_u32_u32(a_r32, b.l0, 32, 0) does not affect result
    let mut res_l32 = mul_u32_u32(a_0, b.l0, 0, 0) << 32;
    res_l32 = res_l32.wrapping_add(mul_u32_u32(a_r32, b.l32, 32, -32) << 32);
    res_l32 = res_l32.wrapping_add(mul_u32_u32(a_0, b.l32, 0, -32));
    res_l32 = res_l32.wrapping_add(mul_u32_u32(a_r32, b.l64, 32, -64));
    res_l32 = res_l32.wrapping_add(mul_u32_u32(a_0, b.l64, 0, -64) >> 32); // Improve rounding accuracy
    let ret = (res_l32 >> 32).wrapping_add(u64::from((res_l32 as u32) >> 31)) as u32; // Round to nearest integer

    debug_u32_mul_u64_fp32_64(a, b, res_l32, ret);

    ret
}

/// Multiplies a 64-bit integer `a` by the 32.64 fixed point value `b`,
/// returning the low 64 bits of the product rounded to the nearest integer.
#[inline]
pub fn u64_mul_u64_fp32_64(a: u64, b: Fp3264) -> u64 {
    let a_r32 = (a >> 32) as u32;
    let a_0 = a as u32;

    let mut tmp = mul_u32_u32(a_r32, b.l0, 32, 0);
    let mut res_0 = tmp << 32;
    tmp = mul_u32_u32(a_0, b.l0, 0, 0);
    res_0 = res_0.wrapping_add(tmp);
    tmp = mul_u32_u32(a_r32, b.l32, 32, -32);
    res_0 = res_0.wrapping_add(tmp);
    tmp = mul_u32_u32(a_0, b.l32, 0, -32);
    res_0 = res_0.wrapping_add(tmp >> 32);
    let mut res_l32 = u64::from(tmp as u32);
    tmp = mul_u32_u32(a_r32, b.l64, 32, -64);
    res_0 = res_0.wrapping_add(tmp >> 32);
    res_l32 = res_l32.wrapping_add(u64::from(tmp as u32));
    tmp = mul_u32_u32(a_0, b.l64, 0, -64); // Improve rounding accuracy
    res_l32 = res_l32.wrapping_add(tmp >> 32);
    res_0 = res_0.wrapping_add(res_l32 >> 32);
    let res_l32_32 = res_l32 as u32;
    let ret = res_0.wrapping_add(u64::from(res_l32_32 >> 31)); // Round to nearest integer

    debug_u64_mul_u64_fp32_64(a, b, res_0, res_l32_32, ret);

    ret
}

/// FFI trampoline for C++ `fp_32_64_div_32_32`.
///
/// Called from C via `<lib/fixed_point.h>`; the C caller must pass a valid,
/// aligned, non-null pointer to a `struct fp_32_64`.
///
/// # Safety
///
/// `result` must be valid for writes and properly aligned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_fixed_point_fp_32_64_div_32_32(
    result: NonNull<Fp3264>,
    dividend: u32,
    divisor: u32,
) {
    // SAFETY: Forwarding caller's safety contract to `fp_32_64_div_32_32`.
    unsafe { fp_32_64_div_32_32(result, dividend, divisor) }
}

/// FFI trampoline for C++ `u64_mul_u32_fp32_64`.
///
/// Called from C via `<lib/fixed_point.h>`; `b` is passed by value matching C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_fixed_point_u64_mul_u32_fp32_64(a: u32, b: Fp3264) -> u64 {
    u64_mul_u32_fp32_64(a, b)
}

/// FFI trampoline for C++ `u32_mul_u64_fp32_64`.
///
/// Called from C via `<lib/fixed_point.h>`; `b` is passed by value matching C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_fixed_point_u32_mul_u64_fp32_64(a: u64, b: Fp3264) -> u32 {
    u32_mul_u64_fp32_64(a, b)
}

/// FFI trampoline for C++ `u64_mul_u64_fp32_64`.
///
/// Called from C via `<lib/fixed_point.h>`; `b` is passed by value matching C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_fixed_point_u64_mul_u64_fp32_64(a: u64, b: Fp3264) -> u64 {
    u64_mul_u64_fp32_64(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: Fp3264 = Fp3264 { l0: 1, l32: 0, l64: 0 };
    const HALF: Fp3264 = Fp3264 { l0: 0, l32: 0x8000_0000, l64: 0 };
    const MAX: Fp3264 = Fp3264 { l0: u32::MAX, l32: u32::MAX, l64: u32::MAX };

    /// The 96-bit integer `l0:l32:l64` (i.e. the fixed point value scaled by 2^64).
    fn raw(b: Fp3264) -> u128 {
        (u128::from(b.l0) << 64) | (u128::from(b.l32) << 32) | u128::from(b.l64)
    }

    /// Reference for `fp_32_64_div_32_32`: floor(dividend * 2^64 / divisor) split into limbs.
    fn div_reference(dividend: u32, divisor: u32) -> Fp3264 {
        let q = (u128::from(dividend) << 64) / u128::from(divisor);
        Fp3264 { l0: (q >> 64) as u32, l32: (q >> 32) as u32, l64: q as u32 }
    }

    /// Bit-exact reference model of the multiply routines.
    ///
    /// All three multiply helpers compute `a * b` exactly except that the low 32 bits of the
    /// `a_0 * b.l64` partial product are discarded before rounding ("Improve rounding accuracy"
    /// keeps only the high half). The intermediate `X` therefore equals the exact product scaled
    /// by 2^-32 minus that discarded term, and the result is `X` rounded to nearest at bit 32.
    /// Wrapping arithmetic mirrors the C unsigned semantics for inputs that overflow.
    fn mul_reference(a: u64, b: Fp3264) -> u128 {
        let a_0 = a as u32;
        let discarded = (u64::from(a_0) * u64::from(b.l64)) & 0xffff_ffff;
        let x = (u128::from(a).wrapping_mul(raw(b)).wrapping_sub(u128::from(discarded))) >> 32;
        x.wrapping_add(1 << 31) >> 32
    }

    /// Mathematically exact `a * b` rounded to nearest integer.
    ///
    /// The full product is up to 160 bits wide, so this is computed modulo 2^128; that still
    /// determines the rounded value modulo 2^64, which is all [`within_one_mod_u64`] needs.
    fn exact_rounded(a: u64, b: Fp3264) -> u128 {
        u128::from(a).wrapping_mul(raw(b)).wrapping_add(1u128 << 63) >> 64
    }

    /// Returns true if `got` is `exact` or `exact - 1`, compared modulo 2^64 so that the check
    /// stays meaningful when the true product does not fit in the 64-bit result.
    fn within_one_mod_u64(exact: u128, got: u64) -> bool {
        (exact.wrapping_sub(u128::from(got)) & u128::from(u64::MAX)) <= 1
    }

    /// Deterministic pseudo-random sequence (64-bit LCG) for cross-check vectors.
    fn next_random(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state ^ (*state >> 29)
    }

    #[test]
    fn div_known_values() {
        let mut result = Fp3264::default();

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1, 1) };
        assert_eq!(result, ONE);

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1, 2) };
        assert_eq!(result, HALF);

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 3, 2) };
        assert_eq!(result, Fp3264 { l0: 1, l32: 0x8000_0000, l64: 0 });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1, 3) };
        assert_eq!(result, Fp3264 { l0: 0, l32: 0x5555_5555, l64: 0x5555_5555 });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 2, 3) };
        assert_eq!(result, Fp3264 { l0: 0, l32: 0xaaaa_aaaa, l64: 0xaaaa_aaaa });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1, 4) };
        assert_eq!(result, Fp3264 { l0: 0, l32: 0x4000_0000, l64: 0 });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 0, 7) };
        assert_eq!(result, Fp3264::default());

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), u32::MAX, 1) };
        assert_eq!(result, Fp3264 { l0: u32::MAX, l32: 0, l64: 0 });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1, u32::MAX) };
        assert_eq!(result, Fp3264 { l0: 0, l32: 1, l64: 1 });

        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), u32::MAX, u32::MAX) };
        assert_eq!(result, ONE);

        // 1_000_000 / 3_000_000 == 1/3: the "ns per TSC tick" ratio for a 3GHz TSC.
        let mut expected = Fp3264::default();
        // SAFETY: `expected` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut expected), 1, 3) };
        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), 1_000_000, 3_000_000) };
        assert_eq!(result, expected);
    }

    #[test]
    fn div_matches_u128_reference() {
        let mut state = 0x1234_5678_9abc_def0;
        let mut result = Fp3264::default();
        for _ in 0..10_000 {
            let dividend = next_random(&mut state) as u32;
            let divisor = (next_random(&mut state) as u32).max(1);
            // SAFETY: `result` is a valid, aligned mutable reference.
            unsafe { fp_32_64_div_32_32(NonNull::from(&mut result), dividend, divisor) };
            assert_eq!(result, div_reference(dividend, divisor), "{dividend} / {divisor}");
        }
    }

    #[test]
    fn mul_u32_u32_products() {
        assert_eq!(mul_u32_u32(0, u32::MAX, 0, 0), 0);
        assert_eq!(mul_u32_u32(1, 1, 0, 0), 1);
        assert_eq!(mul_u32_u32(0x1_0000, 0x1_0000, 0, -32), 1 << 32);
        assert_eq!(mul_u32_u32(u32::MAX, u32::MAX, 32, -64), 0xffff_fffe_0000_0001);
    }

    #[test]
    fn mul_by_zero() {
        let zero = Fp3264::default();
        assert_eq!(u64_mul_u32_fp32_64(u32::MAX, zero), 0);
        assert_eq!(u32_mul_u64_fp32_64(u64::MAX, zero), 0);
        assert_eq!(u64_mul_u64_fp32_64(u64::MAX, zero), 0);
        assert_eq!(u64_mul_u32_fp32_64(0, MAX), 0);
        assert_eq!(u32_mul_u64_fp32_64(0, MAX), 0);
        assert_eq!(u64_mul_u64_fp32_64(0, MAX), 0);
    }

    #[test]
    fn mul_by_one_is_identity() {
        for a in [0u64, 1, 2, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x1_0000_0000, u64::MAX] {
            assert_eq!(u64_mul_u64_fp32_64(a, ONE), a);
            // The 32-bit result keeps the low 32 bits of the product.
            assert_eq!(u32_mul_u64_fp32_64(a, ONE), a as u32);
            if let Ok(a32) = u32::try_from(a) {
                assert_eq!(u64_mul_u32_fp32_64(a32, ONE), a);
            }
        }
    }

    #[test]
    fn mul_rounds_to_nearest() {
        // 0.5 * n rounds half up: 1*0.5 -> 1, 2*0.5 -> 1, 3*0.5 -> 2.
        assert_eq!(u64_mul_u32_fp32_64(1, HALF), 1);
        assert_eq!(u64_mul_u32_fp32_64(2, HALF), 1);
        assert_eq!(u64_mul_u32_fp32_64(3, HALF), 2);
        assert_eq!(u32_mul_u64_fp32_64(1, HALF), 1);
        assert_eq!(u32_mul_u64_fp32_64(2, HALF), 1);
        assert_eq!(u32_mul_u64_fp32_64(3, HALF), 2);
        assert_eq!(u64_mul_u64_fp32_64(1, HALF), 1);
        assert_eq!(u64_mul_u64_fp32_64(2, HALF), 1);
        assert_eq!(u64_mul_u64_fp32_64(3, HALF), 2);

        // Just under one half (0.5 - 2^-64) rounds down for a == 1...
        let just_under_half = Fp3264 { l0: 0, l32: 0x7fff_ffff, l64: u32::MAX };
        assert_eq!(u64_mul_u32_fp32_64(1, just_under_half), 0);
        assert_eq!(u32_mul_u64_fp32_64(1, just_under_half), 0);
        assert_eq!(u64_mul_u64_fp32_64(1, just_under_half), 0);
        // ...and 3 * (0.5 - 2^-64) = 1.4999... rounds to 1.
        assert_eq!(u64_mul_u32_fp32_64(3, just_under_half), 1);
        assert_eq!(u32_mul_u64_fp32_64(3, just_under_half), 1);
        assert_eq!(u64_mul_u64_fp32_64(3, just_under_half), 1);

        // 1/3 scaled by 3_000_000_000 ticks (one second at 3GHz) is 1e9 ns.
        let mut ns_per_tsc = Fp3264::default();
        // SAFETY: `ns_per_tsc` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut ns_per_tsc), 1_000_000, 3_000_000) };
        assert_eq!(u64_mul_u64_fp32_64(3_000_000_000, ns_per_tsc), 1_000_000_000);
        assert_eq!(u32_mul_u64_fp32_64(3_000_000_000, ns_per_tsc), 1_000_000_000);
        assert_eq!(u64_mul_u32_fp32_64(3_000_000_000, ns_per_tsc), 1_000_000_000);
        assert_eq!(u64_mul_u64_fp32_64(1, ns_per_tsc), 0);
        assert_eq!(u64_mul_u64_fp32_64(2, ns_per_tsc), 1);
    }

    #[test]
    fn mul_max_values() {
        // u32::MAX * (2^32 - 2^-64) = 2^64 - 2^32 - (2^32 - 1) * 2^-64, which rounds to
        // 2^64 - 2^32 and comfortably fits in a u64.
        assert_eq!(u64_mul_u32_fp32_64(u32::MAX, MAX), 0xffff_ffff_0000_0000);
        assert_eq!(
            u64_mul_u32_fp32_64(u32::MAX, MAX),
            mul_reference(u64::from(u32::MAX), MAX) as u64
        );

        // u64::MAX * (2^32 - 2^-64) overflows; the C routines wrap modulo 2^64 / 2^32.
        assert_eq!(u64_mul_u64_fp32_64(u64::MAX, MAX), mul_reference(u64::MAX, MAX) as u64);
        assert_eq!(u32_mul_u64_fp32_64(u64::MAX, MAX), mul_reference(u64::MAX, MAX) as u32);
        assert!(within_one_mod_u64(
            exact_rounded(u64::MAX, MAX),
            u64_mul_u64_fp32_64(u64::MAX, MAX)
        ));

        // Largest non-overflowing 64-bit case: 2^32 * (2^32 - 1) = 2^64 - 2^32.
        let int_max = Fp3264 { l0: u32::MAX, l32: 0, l64: 0 };
        assert_eq!(u64_mul_u64_fp32_64(1 << 32, int_max), 0xffff_ffff_0000_0000);
    }

    #[test]
    fn mul_matches_u128_reference() {
        let mut state = 0xdead_beef_cafe_f00d;
        for i in 0..20_000 {
            let a = next_random(&mut state);
            let b = Fp3264 {
                l0: next_random(&mut state) as u32,
                l32: next_random(&mut state) as u32,
                l64: next_random(&mut state) as u32,
            };
            // Mix in some small operands so that the products do not always overflow.
            let (a, b) = match i % 4 {
                0 => (a, b),
                1 => (a >> 40, b),
                2 => (a, Fp3264 { l0: b.l0 >> 24, ..b }),
                _ => (a >> 48, Fp3264 { l0: 0, ..b }),
            };
            let a32 = a as u32;

            assert_eq!(u64_mul_u64_fp32_64(a, b), mul_reference(a, b) as u64, "{a:#x} * {b:x?}");
            assert_eq!(u32_mul_u64_fp32_64(a, b), mul_reference(a, b) as u32, "{a:#x} * {b:x?}");
            assert_eq!(
                u64_mul_u32_fp32_64(a32, b),
                mul_reference(u64::from(a32), b) as u64,
                "{a32:#x} * {b:x?}"
            );

            // Independently of the bit-exact model, the truncated `a_0 * l64` term can perturb the
            // result by at most one unit below the exactly rounded product.
            assert!(
                within_one_mod_u64(exact_rounded(a, b), u64_mul_u64_fp32_64(a, b)),
                "{a:#x} * {b:x?}"
            );
            assert!(
                within_one_mod_u64(exact_rounded(u64::from(a32), b), u64_mul_u32_fp32_64(a32, b)),
                "{a32:#x} * {b:x?}"
            );
        }
    }

    #[test]
    fn ffi_trampolines() {
        let mut result = Fp3264::default();
        // SAFETY: `result` is a valid, aligned mutable reference.
        unsafe { rust_fixed_point_fp_32_64_div_32_32(NonNull::from(&mut result), 1, 3) };
        let mut expected = Fp3264::default();
        // SAFETY: `expected` is a valid, aligned mutable reference.
        unsafe { fp_32_64_div_32_32(NonNull::from(&mut expected), 1, 3) };
        assert_eq!(result, expected);
        assert_eq!(rust_fixed_point_u64_mul_u32_fp32_64(3, result), u64_mul_u32_fp32_64(3, result));
        assert_eq!(
            rust_fixed_point_u32_mul_u64_fp32_64(3_000_000_000, result),
            u32_mul_u64_fp32_64(3_000_000_000, result)
        );
        assert_eq!(
            rust_fixed_point_u64_mul_u64_fp32_64(3_000_000_000, result),
            u64_mul_u64_fp32_64(3_000_000_000, result)
        );
        assert_eq!(rust_fixed_point_u64_mul_u64_fp32_64(3_000_000_000, result), 1_000_000_000);
    }
}
