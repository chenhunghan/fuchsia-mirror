// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Returns a bitmask of `x` low bits set, matching C++'s `BIT_MASK(x)` from `<bits.h>`.
#[inline(always)]
pub const fn bit_mask_u64(x: usize) -> u64 {
    if x >= u64::BITS as usize { !0 } else { (1u64 << x) - 1 }
}

/// Returns a 32-bit bitmask of `x` low bits set, matching C++'s `BIT_MASK32(x)` from `<bits.h>`.
#[inline(always)]
pub const fn bit_mask_u32(x: usize) -> u32 {
    if x >= u32::BITS as usize { !0 } else { (1u32 << x) - 1 }
}

#[inline(always)]
pub const fn bit(x: u64, bit: usize) -> u64 {
    x & (1u64 << bit)
}

#[inline(always)]
pub const fn bit_set(x: u64, bit: usize) -> bool {
    (x & (1u64 << bit)) != 0
}

#[inline(always)]
pub const fn bit_shift(x: u64, bit: usize) -> u64 {
    (x >> bit) & 1
}

#[inline(always)]
pub const fn bits(x: u64, high: usize, low: usize) -> u64 {
    x & (bit_mask_u64(high + 1) & !bit_mask_u64(low))
}

#[inline(always)]
pub const fn bits_shift(x: u64, high: usize, low: usize) -> u64 {
    (x >> low) & bit_mask_u64(high - low + 1)
}
