// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::cmp::Ordering;

#[inline]
const fn min_u64(a: u64, b: u64) -> u64 {
    if a <= b { a } else { b }
}

#[inline]
const fn max_u64(a: u64, b: u64) -> u64 {
    if a >= b { a } else { b }
}

#[inline]
const fn min_u32(a: u32, b: u32) -> u32 {
    if a <= b { a } else { b }
}

#[inline]
const fn max_u32(a: u32, b: u32) -> u32 {
    if a >= b { a } else { b }
}

macro_rules! make_scalar_value_data {
    (
        $uint:ident,
        $sint:ident,
        $range:ident,
        $scalar:ident,
        bits = $bits:expr,
        bytes = $bytes:expr,
        min_fn = $min_fn:ident,
        max_fn = $max_fn:ident,
        from_types = [$($from_ty:ty),*],
    ) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $range {
            pub min: $uint,
            pub max: $uint,
        }

        impl $range {
            pub const fn new(min: $uint, max: $uint) -> Self {
                Self { min, max }
            }

            pub const fn max() -> Self {
                Self::new(0, $uint::MAX)
            }

            pub const fn from_scalar(value: $uint) -> Self {
                Self::new(value, value)
            }

            const fn mask_low_bits(value: $uint, bit_count: u32) -> $uint {
                if bit_count == 0 {
                    0
                } else if bit_count >= $bits {
                    value
                } else {
                    value & (((1 as $uint) << bit_count) - 1)
                }
            }

            pub const fn extract_slices(
                value: $uint,
                offset: usize,
                byte_count: usize,
            ) -> ($uint, $uint, $uint) {
                let clamped_offset = if offset > $bytes { $bytes } else { offset };
                let max_count = $bytes - clamped_offset;
                let clamped_count = if byte_count > max_count { max_count } else { byte_count };
                let low_bits = (clamped_offset * 8) as u32;
                let mid_bits = (clamped_count * 8) as u32;
                let high_bits = low_bits + mid_bits;

                let s1 = Self::mask_low_bits(value, low_bits);
                let shifted_mid = if low_bits >= $bits { 0 } else { value >> low_bits };
                let s2 = Self::mask_low_bits(shifted_mid, mid_bits);
                let s3 = if high_bits >= $bits { 0 } else { value >> high_bits };
                (s1, s2, s3)
            }

            pub const fn assemble_slices(
                values: ($uint, $uint, $uint),
                offset: usize,
                byte_count: usize,
            ) -> $uint {
                let (v1, v2, v3) = values;
                let clamped_offset = if offset > $bytes { $bytes } else { offset };
                let max_count = $bytes - clamped_offset;
                let clamped_count = if byte_count > max_count { max_count } else { byte_count };
                let low_bits = (clamped_offset * 8) as u32;
                let mid_bits = (clamped_count * 8) as u32;
                let high_bits = low_bits + mid_bits;

                let p1 = Self::mask_low_bits(v1, low_bits);
                let p2 = if low_bits >= $bits {
                    0
                } else {
                    Self::mask_low_bits(v2, mid_bits) << low_bits
                };
                let p3 = if high_bits >= $bits { 0 } else { v3 << high_bits };
                p1 | p2 | p3
            }

            /// Given target and source values, each in the `target` and `source` ranges, computes
            /// the range of the result where `byte_count` bytes from `source` at `source_offset`
            /// replace `byte_count` bytes from `target` at `target_offset`.
            pub const fn compute_range_for_bytes_swap(
                target: $range,
                source: $range,
                target_offset: usize,
                source_offset: usize,
                byte_count: usize,
            ) -> $range {
                let (target_umin1, target_umin2, target_umin3) =
                    Self::extract_slices(target.min, target_offset, byte_count);
                let (target_umax1, target_umax2, target_umax3) =
                    Self::extract_slices(target.max, target_offset, byte_count);
                let (_, source_umin2, source_umin3) =
                    Self::extract_slices(source.min, source_offset, byte_count);
                let (_, source_umax2, source_umax3) =
                    Self::extract_slices(source.max, source_offset, byte_count);

                let (final_umin3, final_umax3) = (target_umin3, target_umax3);
                let (final_umin2, final_umax2) = if source_umax3 > source_umin3 {
                    (0, $uint::MAX)
                } else {
                    (source_umin2, source_umax2)
                };
                let (final_umin1, final_umax1) =
                    if target_umax3 > target_umin3 || target_umax2 > target_umin2 {
                        (0, $uint::MAX)
                    } else {
                        (target_umin1, target_umax1)
                    };

                let final_min = Self::assemble_slices(
                    (final_umin1, final_umin2, final_umin3),
                    target_offset,
                    byte_count,
                );
                let final_max = Self::assemble_slices(
                    (final_umax1, final_umax2, final_umax3),
                    target_offset,
                    byte_count,
                );

                $range::new(final_min, final_max)
            }

            pub fn add_range(self, rhs: Self) -> Self {
                let (min, min_overflowed) = self.min.overflowing_add(rhs.min);
                let (max, max_overflowed) = self.max.overflowing_add(rhs.max);
                if min_overflowed != max_overflowed { Self::max() } else { Self { min, max } }
            }

            pub fn sub_range(self, rhs: Self) -> Self {
                let (min, min_overflowed) = self.min.overflowing_sub(rhs.max);
                let (max, max_overflowed) = self.max.overflowing_sub(rhs.min);
                if min_overflowed != max_overflowed { Self::max() } else { Self { min, max } }
            }

            pub fn mul_range(self, rhs: Self) -> Self {
                let Some(min) = self.min.checked_mul(rhs.min) else {
                    return Self::max();
                };
                let Some(max) = self.max.checked_mul(rhs.max) else {
                    return Self::max();
                };
                Self { min, max }
            }

            pub fn div_range(self, rhs: Self) -> Self {
                if rhs.max == 0 {
                    return Self::new(0, 0);
                }
                let min = if rhs.min == 0 { 0 } else { self.min / rhs.max };
                let divisor_min = $max_fn(1, rhs.min);
                let max = self.max / divisor_min;
                Self { min, max }
            }

            pub fn rem_range(self, rhs: Self) -> Self {
                if rhs.max == 0 {
                    return self;
                }
                let min = 0;
                let max = if rhs.min == 0 { self.max } else { $min_fn(self.max, rhs.max - 1) };
                Self { min, max }
            }

            pub fn neg_range(self) -> Self {
                if self.max == 0 {
                    return self;
                }
                if self.min == 0 {
                    return Self::max();
                }
                Self::new((0 as $uint).wrapping_sub(self.max), (0 as $uint).wrapping_sub(self.min))
            }

            #[allow(unused_comparisons)]
            pub fn shl_range(self, rhs: Self) -> Self {
                if rhs.max > (u32::MAX as $uint) || rhs.min > (u32::MAX as $uint) {
                    return Self::max();
                }
                let rhs_max = rhs.max as u32;
                let rhs_min = rhs.min as u32;
                if self.max == 0 {
                    return self;
                }
                if rhs_max > self.max.leading_zeros() {
                    return Self::max();
                }
                let min = self.min.wrapping_shl(rhs_min);
                let max = self.max.wrapping_shl(rhs_max);
                Self { min, max }
            }

            pub fn shr_range(self, rhs: Self) -> Self {
                // eBPF masks shift operations to the bit width; if `rhs.max` reaches the bit width,
                // only `<= self.max` holds.
                if rhs.max >= ($bits as $uint) {
                    return Self { min: 0, max: self.max };
                }
                let rhs_max = rhs.max as u32;
                let rhs_min = rhs.min as u32;
                let min = self.min.wrapping_shr(rhs_max);
                let max = self.max.wrapping_shr(rhs_min);
                Self { min, max }
            }

            pub fn checked_add(self, rhs: Self) -> Option<Self> {
                let Some(min) = self.min.checked_add(rhs.min) else {
                    return None;
                };
                let Some(max) = self.max.checked_add(rhs.max) else {
                    return None;
                };
                Some(Self { min, max })
            }

            pub fn checked_sub(self, rhs: Self) -> Option<Self> {
                let Some(min) = self.min.checked_sub(rhs.max) else {
                    return None;
                };
                let Some(max) = self.max.checked_sub(rhs.min) else {
                    return None;
                };
                Some(Self { min, max })
            }

            /// Returns whether `[self.min, self.max]` contains at least one integer matching the
            /// known bits `(value & !unknown_mask, unknown_mask)`.
            pub const fn contains_masked_value(self, value: $uint, unknown_mask: $uint) -> bool {
                let value = value & !unknown_mask;
                if self.min > self.max {
                    return false;
                }
                let diff = self.min ^ self.max;
                if diff == 0 {
                    return ((self.min ^ value) & !unknown_mask) == 0;
                }
                let b = ($bits - 1) - diff.leading_zeros();
                let common_mask: $uint =
                    if b == ($bits - 1) { 0 } else { (!(0 as $uint)).wrapping_shl(b + 1) };
                if (((self.min ^ value) & common_mask) & !unknown_mask) != 0 {
                    return false;
                }
                let value = (value & !common_mask) | (self.min & common_mask);
                let unknown_mask = unknown_mask & !common_mask;
                let bit_b: $uint = (1 as $uint).wrapping_shl(b);
                if (unknown_mask & bit_b) != 0 {
                    (value | (unknown_mask & !bit_b)) >= self.min || (value | bit_b) <= self.max
                } else {
                    value <= self.max && self.min <= (value | unknown_mask)
                }
            }
        }

        impl PartialOrd for $range {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                if *self == *other {
                    return Some(Ordering::Equal);
                }
                if self.min <= other.min && self.max >= other.max {
                    return Some(Ordering::Greater);
                }
                if self.min >= other.min && self.max <= other.max {
                    return Some(Ordering::Less);
                }
                None
            }
        }

        impl From<$uint> for $range {
            fn from(value: $uint) -> Self {
                Self::from_scalar(value)
            }
        }
        $(
            impl From<$from_ty> for $range {
                fn from(value: $from_ty) -> Self {
                    Self::from_scalar(value as $uint)
                }
            }
        )*

        impl core::ops::Add<$range> for $range {
            type Output = Self;
            fn add(self, rhs: $range) -> Self {
                self.add_range(rhs)
            }
        }
        impl core::ops::Sub<$range> for $range {
            type Output = Self;
            fn sub(self, rhs: $range) -> Self {
                self.sub_range(rhs)
            }
        }
        impl core::ops::Mul<$range> for $range {
            type Output = Self;
            fn mul(self, rhs: $range) -> Self {
                self.mul_range(rhs)
            }
        }
        impl core::ops::Div<$range> for $range {
            type Output = Self;
            fn div(self, rhs: $range) -> Self {
                self.div_range(rhs)
            }
        }
        impl core::ops::Rem<$range> for $range {
            type Output = Self;
            fn rem(self, rhs: $range) -> Self {
                self.rem_range(rhs)
            }
        }
        impl core::ops::Neg for $range {
            type Output = Self;
            fn neg(self) -> Self {
                self.neg_range()
            }
        }
        impl core::ops::Shl<$range> for $range {
            type Output = Self;
            fn shl(self, rhs: $range) -> Self {
                self.shl_range(rhs)
            }
        }
        impl core::ops::Shr<$range> for $range {
            type Output = Self;
            fn shr(self, rhs: $range) -> Self {
                self.shr_range(rhs)
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $scalar {
            /// The value. Its interpretation depends on `unknown_mask` and `unwritten_mask`.
            pub value: $uint,
            /// A bit mask of unknown bits. A bit in `value` is valid (and can be used by the
            /// verifier) if the equivalent mask in `unknown_mask` is 0.
            pub unknown_mask: $uint,
            /// A bit mask of unwritten bits. A bit in `value` is written (and can be sent back to
            /// userspace) if the equivalent mask in `unwritten_mask` is 0. `unwritten_mask` must
            /// always be a subset of `unknown_mask`.
            pub unwritten_mask: $uint,
            /// The range of possible unsigned values of this scalar.
            pub urange: $range,
            /// Prevent instantiation without using `new`.
            _guard: (),
        }

        /// Defines a partial ordering on `ScalarValueData` instances, capturing the notion of how
        /// "broad" a scalar value is in terms of the set of potential values it represents.
        ///
        /// The ordering is defined such that `s1 > s2` if a proof that an eBPF program terminates
        /// in a state where a register or memory location has a scalar value `s1` is also a proof
        /// that the program terminates in a state where that location has scalar value `s2`.
        ///
        /// In other words, a "broader" value represents a larger set of possible values, and
        /// proving termination with a broader type implies termination with any narrower value.
        ///
        /// Examples:
        /// * `ScalarValueData { unknown_mask: 0, .. }` (a known scalar value) is less than
        ///   `ScalarValueData { unknown_mask: u64::MAX, .. }` (an unknown scalar value).
        impl PartialOrd for $scalar {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                if *self == *other {
                    return Some(Ordering::Equal);
                }
                if (self.unwritten_mask & other.unwritten_mask) == other.unwritten_mask
                    && (self.unknown_mask & other.unknown_mask) == other.unknown_mask
                    && (self.value & !self.unknown_mask) == (other.value & !self.unknown_mask)
                    && matches!(
                        self.urange.partial_cmp(&other.urange),
                        Some(Ordering::Equal | Ordering::Greater)
                    )
                {
                    return Some(Ordering::Greater);
                }
                if (other.unwritten_mask & self.unwritten_mask) == self.unwritten_mask
                    && (other.unknown_mask & self.unknown_mask) == self.unknown_mask
                    && (self.value & !other.unknown_mask) == (other.value & !other.unknown_mask)
                    && matches!(
                        self.urange.partial_cmp(&other.urange),
                        Some(Ordering::Equal | Ordering::Less)
                    )
                {
                    return Some(Ordering::Less);
                }
                None
            }
        }

        impl $scalar {
            pub const UNINITIALIZED: Self = Self::new(0, $uint::MAX, $uint::MAX, $range::max());
            pub const UNKNOWN_WRITTEN: Self = Self::new(0, $uint::MAX, 0, $range::max());

            pub const fn new(
                value: $uint,
                unknown_mask: $uint,
                unwritten_mask: $uint,
                urange: $range,
            ) -> Self {
                let diff = urange.min ^ urange.max;
                let shift = $bits - diff.leading_zeros();
                let common_mask = if shift == $bits { 0 } else { (!(0 as $uint)).wrapping_shl(shift) };

                let unknown_mask = unknown_mask & !common_mask;
                let unwritten_mask = unwritten_mask & unknown_mask;
                let value = (value & !unknown_mask & !common_mask) | (urange.min & common_mask);

                let urange = if unknown_mask == 0 {
                    $range::new(value, value)
                } else {
                    let min = $max_fn(value, urange.min);
                    let max = $min_fn(value | unknown_mask, urange.max);
                    $range::new(min, max)
                };
                Self { value, unknown_mask, unwritten_mask, urange, _guard: () }
            }

            pub const fn from_known(value: $uint) -> Self {
                Self::new(value, 0, 0, $range::from_scalar(value))
            }

            /// Constructs a `ScalarValueData` if `urange` contains at least one value matching the
            /// known bits `(value, unknown_mask)`, or returns `None` if no value in `urange`
            /// matches.
            pub const fn try_new(
                value: $uint,
                unknown_mask: $uint,
                unwritten_mask: $uint,
                urange: $range,
            ) -> Option<Self> {
                if urange.contains_masked_value(value, unknown_mask) {
                    Some(Self::new(value, unknown_mask, unwritten_mask, urange))
                } else {
                    None
                }
            }

            pub const fn is_known(&self) -> bool {
                self.unknown_mask == 0
            }

            pub const fn is_fully_initialized(&self) -> bool {
                self.unwritten_mask == 0
            }

            pub const fn is_uninitialized(&self) -> bool {
                self.unwritten_mask == $uint::MAX
            }

            pub const fn is_zero(&self) -> bool {
                self.is_known() && self.value == 0
            }

            pub const fn min(&self) -> $uint {
                self.urange.min
            }

            pub const fn max(&self) -> $uint {
                self.urange.max
            }

            pub fn update_range(self, urange: $range) -> Self {
                Self::new(self.value, self.unknown_mask, self.unwritten_mask, urange)
            }

            /// Returns whether there exists at least one concrete value belonging to both `self`
            /// and `other`.
            pub fn can_be_equal(self, other: Self) -> bool {
                if ((self.value ^ other.value) & !(self.unknown_mask | other.unknown_mask)) != 0 {
                    return false;
                }
                let range =
                    $range::new($max_fn(self.min(), other.min()), $min_fn(self.max(), other.max()));
                let value = self.value | other.value;
                let unknown_mask = self.unknown_mask & other.unknown_mask;
                range.contains_masked_value(value, unknown_mask)
            }

            fn base(&self, rhs: Self) -> Self {
                if !self.is_fully_initialized() || !rhs.is_fully_initialized() {
                    Self::UNINITIALIZED
                } else {
                    Self::UNKNOWN_WRITTEN
                }
            }

            pub fn add_scalar(self, rhs: Self) -> Self {
                if self.is_known() && rhs.is_known() {
                    return Self::from_known(self.value.wrapping_add(rhs.value));
                }
                self.base(rhs).update_range(self.urange.add_range(rhs.urange))
            }

            pub fn sub_scalar(self, rhs: Self) -> Self {
                if self.is_known() && rhs.is_known() {
                    return Self::from_known(self.value.wrapping_sub(rhs.value));
                }
                self.base(rhs).update_range(self.urange.sub_range(rhs.urange))
            }

            pub fn mul_scalar(self, rhs: Self) -> Self {
                if self.is_known() && rhs.is_known() {
                    return Self::from_known(self.value.wrapping_mul(rhs.value));
                }
                self.base(rhs).update_range(self.urange.mul_range(rhs.urange))
            }

            pub fn div_scalar(self, rhs: Self) -> Self {
                if self.is_known() && rhs.is_known() {
                    if rhs.value == 0 {
                        return Self::from_known(0);
                    } else {
                        return Self::from_known(self.value / rhs.value);
                    }
                }
                self.base(rhs).update_range(self.urange.div_range(rhs.urange))
            }

            pub fn rem_scalar(self, rhs: Self) -> Self {
                if self.is_known() && rhs.is_known() {
                    if rhs.value == 0 {
                        return self;
                    } else {
                        return Self::from_known(self.value % rhs.value);
                    }
                }
                self.base(rhs).update_range(self.urange.rem_range(rhs.urange))
            }

            pub fn neg_scalar(self) -> Self {
                if self.is_known() {
                    return Self::from_known((0 as $uint).wrapping_sub(self.value));
                }
                self.base(self).update_range(self.urange.neg_range())
            }

            pub fn bitand_scalar(self, rhs: Self) -> Self {
                let urange = $range::new(0, $min_fn(self.max(), rhs.max()));
                // A bit can be 1 only if it is not known to be 0 in either operand.
                let not_known_zero =
                    (self.value | self.unknown_mask) & (rhs.value | rhs.unknown_mask);
                let unknown_mask = (self.unknown_mask | rhs.unknown_mask) & not_known_zero;
                let unwritten_mask = (self.unwritten_mask | rhs.unwritten_mask) & not_known_zero;
                let value = self.value & rhs.value;
                Self::new(value, unknown_mask, unwritten_mask, urange)
            }

            pub fn bitor_scalar(self, rhs: Self) -> Self {
                let urange = $range::new($max_fn(self.min(), rhs.min()), $uint::MAX);
                // Any bit known to be 1 in either operand is known to be 1 in the result.
                let value = self.value | rhs.value;
                let unknown_mask = (self.unknown_mask | rhs.unknown_mask) & !value;
                let unwritten_mask = (self.unwritten_mask | rhs.unwritten_mask) & !value;
                Self::new(value, unknown_mask, unwritten_mask, urange)
            }

            pub fn bitxor_scalar(self, rhs: Self) -> Self {
                let urange = $range::max();
                let unknown_mask = self.unknown_mask | rhs.unknown_mask;
                let unwritten_mask = self.unwritten_mask | rhs.unwritten_mask;
                let value = (self.value ^ rhs.value) & !unknown_mask;
                Self::new(value, unknown_mask, unwritten_mask, urange)
            }

            pub fn shl_scalar(self, rhs: Self) -> Self {
                let urange = self.urange.shl_range(rhs.urange);
                if rhs.is_known() {
                    let y = rhs.value as u32;
                    let value = self.value.wrapping_shl(y);
                    let unknown_mask = self.unknown_mask.wrapping_shl(y);
                    let unwritten_mask = self.unwritten_mask.wrapping_shl(y);
                    return Self::new(value, unknown_mask, unwritten_mask, urange);
                }
                self.base(rhs).update_range(urange)
            }

            pub fn shr_scalar(self, rhs: Self) -> Self {
                let urange = self.urange.shr_range(rhs.urange);
                if rhs.is_known() {
                    let y = rhs.value as u32;
                    let value = self.value.wrapping_shr(y);
                    let unknown_mask = self.unknown_mask.wrapping_shr(y);
                    let unwritten_mask = self.unwritten_mask.wrapping_shr(y);
                    return Self::new(value, unknown_mask, unwritten_mask, urange);
                }
                self.base(rhs).update_range(urange)
            }

            /// Arithmetic right shift.
            pub fn ashr(self, rhs: Self) -> Self {
                fn ashr_raw(x: $uint, y: $uint) -> $uint {
                    ((x as $sint).wrapping_shr(y as u32)) as $uint
                }
                if !rhs.is_known() {
                    return self.base(rhs);
                }
                if self.is_known() {
                    return Self::from_known(ashr_raw(self.value, rhs.value));
                }
                // Arithmetic right shift propagates the sign bit. If the sign bit is unknown,
                // the shifted-in bits must also be marked unknown. We must therefore shift
                // the masks arithmetically to preserve this sign-extended uncertainty.
                // Using logical shift would incorrectly clear the top bits of the masks,
                // causing the verifier to underestimate the maximum possible value.
                let unknown_mask = ashr_raw(self.unknown_mask, rhs.value);
                let unwritten_mask = ashr_raw(self.unwritten_mask, rhs.value);
                let value = ashr_raw(self.value, rhs.value) & !unknown_mask;
                let urange = $range::max();
                Self::new(value, unknown_mask, unwritten_mask, urange)
            }

            pub fn checked_add_scalar(self, rhs: Self) -> Option<Self> {
                if self.is_known() && rhs.is_known() {
                    return match self.value.checked_add(rhs.value) {
                        Some(v) => Some(Self::from_known(v)),
                        None => None,
                    };
                }
                let Some(urange) = self.urange.checked_add(rhs.urange) else {
                    return None;
                };
                Some(self.base(rhs).update_range(urange))
            }

            pub fn checked_sub_scalar(self, rhs: Self) -> Option<Self> {
                if self.is_known() && rhs.is_known() {
                    return match self.value.checked_sub(rhs.value) {
                        Some(v) => Some(Self::from_known(v)),
                        None => None,
                    };
                }
                let Some(urange) = self.urange.checked_sub(rhs.urange) else {
                    return None;
                };
                Some(self.base(rhs).update_range(urange))
            }

            pub fn checked_add(self, rhs: Self) -> Option<Self> {
                self.checked_add_scalar(rhs)
            }

            pub fn checked_sub(self, rhs: Self) -> Option<Self> {
                self.checked_sub_scalar(rhs)
            }
        }

        impl From<&$scalar> for $scalar {
            fn from(value: &Self) -> Self {
                *value
            }
        }
        impl From<$uint> for $scalar {
            fn from(value: $uint) -> Self {
                Self::from_known(value)
            }
        }
        $(
            impl From<$from_ty> for $scalar {
                fn from(value: $from_ty) -> Self {
                    Self::from_known(value as $uint)
                }
            }
        )*

        impl core::ops::Add<$scalar> for $scalar {
            type Output = Self;
            fn add(self, rhs: $scalar) -> Self {
                self.add_scalar(rhs)
            }
        }
        impl core::ops::Sub<$scalar> for $scalar {
            type Output = Self;
            fn sub(self, rhs: $scalar) -> Self {
                self.sub_scalar(rhs)
            }
        }
        impl core::ops::Mul<$scalar> for $scalar {
            type Output = Self;
            fn mul(self, rhs: $scalar) -> Self {
                self.mul_scalar(rhs)
            }
        }
        impl core::ops::Div<$scalar> for $scalar {
            type Output = Self;
            fn div(self, rhs: $scalar) -> Self {
                self.div_scalar(rhs)
            }
        }
        impl core::ops::Rem<$scalar> for $scalar {
            type Output = Self;
            fn rem(self, rhs: $scalar) -> Self {
                self.rem_scalar(rhs)
            }
        }
        impl core::ops::Neg for $scalar {
            type Output = Self;
            fn neg(self) -> Self {
                self.neg_scalar()
            }
        }
        impl core::ops::BitAnd<$scalar> for $scalar {
            type Output = Self;
            fn bitand(self, rhs: $scalar) -> Self {
                self.bitand_scalar(rhs)
            }
        }
        impl core::ops::BitOr<$scalar> for $scalar {
            type Output = Self;
            fn bitor(self, rhs: $scalar) -> Self {
                self.bitor_scalar(rhs)
            }
        }
        impl core::ops::BitXor<$scalar> for $scalar {
            type Output = Self;
            fn bitxor(self, rhs: $scalar) -> Self {
                self.bitxor_scalar(rhs)
            }
        }
        impl core::ops::Shl<$scalar> for $scalar {
            type Output = Self;
            fn shl(self, rhs: $scalar) -> Self {
                self.shl_scalar(rhs)
            }
        }
        impl core::ops::Shr<$scalar> for $scalar {
            type Output = Self;
            fn shr(self, rhs: $scalar) -> Self {
                self.shr_scalar(rhs)
            }
        }
    };
}

make_scalar_value_data!(
    u32,
    i32,
    U32Range,
    U32ScalarValueData,
    bits = 32,
    bytes = 4,
    min_fn = min_u32,
    max_fn = max_u32,
    from_types = [i32, u16, i16, u8, i8],
);

make_scalar_value_data!(
    u64,
    i64,
    U64Range,
    U64ScalarValueData,
    bits = 64,
    bytes = 8,
    min_fn = min_u64,
    max_fn = max_u64,
    from_types = [i64, u32, i32, u16, i16, u8, i8, usize],
);

pub type ScalarValueData = U64ScalarValueData;

impl U64Range {
    pub fn add_i16(self, rhs: i16) -> Self {
        self.add_range(Self::from_scalar(rhs as u64))
    }
}

impl core::ops::Add<u64> for U64Range {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        self.add_range(Self::from_scalar(rhs))
    }
}

impl core::ops::Add<i16> for U64Range {
    type Output = Self;
    fn add(self, rhs: i16) -> Self {
        self.add_i16(rhs)
    }
}

impl U64ScalarValueData {
    pub fn add_u64(self, rhs: u64) -> Self {
        self.add_scalar(Self::from_known(rhs))
    }

    pub fn add_i16(self, rhs: i16) -> Self {
        self.add_scalar(Self::from_known(rhs as u64))
    }

    pub fn checked_add_u64(self, rhs: u64) -> Option<Self> {
        self.checked_add_scalar(Self::from_known(rhs))
    }
}

impl core::ops::Add<u64> for U64ScalarValueData {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        self.add_u64(rhs)
    }
}
impl core::ops::Add<i32> for U64ScalarValueData {
    type Output = Self;
    fn add(self, rhs: i32) -> Self {
        self.add_scalar(Self::from_known(rhs as u64))
    }
}
impl core::ops::Add<i16> for U64ScalarValueData {
    type Output = Self;
    fn add(self, rhs: i16) -> Self {
        self.add_i16(rhs)
    }
}
impl core::ops::Add<usize> for U64ScalarValueData {
    type Output = Self;
    fn add(self, rhs: usize) -> Self {
        self.add_u64(rhs as u64)
    }
}
impl core::ops::Sub<u64> for U64ScalarValueData {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        self.sub_scalar(Self::from_known(rhs))
    }
}
impl core::ops::Sub<i32> for U64ScalarValueData {
    type Output = Self;
    fn sub(self, rhs: i32) -> Self {
        self.sub_scalar(Self::from_known(rhs as u64))
    }
}
impl core::ops::Sub<i16> for U64ScalarValueData {
    type Output = Self;
    fn sub(self, rhs: i16) -> Self {
        self.sub_scalar(Self::from_known(rhs as u64))
    }
}

impl U32ScalarValueData {
    pub fn from_u64_scalar(v: U64ScalarValueData) -> Self {
        let urange = if (v.max() >> 32) == (v.min() >> 32) {
            U32Range::new(v.min() as u32, v.max() as u32)
        } else {
            U32Range::max()
        };
        Self::new(v.value as u32, v.unknown_mask as u32, v.unwritten_mask as u32, urange)
    }

    pub fn into_u64_scalar(self) -> U64ScalarValueData {
        U64ScalarValueData::new(
            self.value as u64,
            self.unknown_mask as u64,
            self.unwritten_mask as u64,
            U64Range::new(self.min() as u64, self.max() as u64),
        )
    }
}

impl From<U64ScalarValueData> for U32ScalarValueData {
    fn from(v: U64ScalarValueData) -> Self {
        Self::from_u64_scalar(v)
    }
}

impl From<U32ScalarValueData> for U64ScalarValueData {
    fn from(v: U32ScalarValueData) -> Self {
        v.into_u64_scalar()
    }
}
