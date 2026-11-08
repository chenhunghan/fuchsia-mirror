// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
mod tests {
    use ebpf_core::scalar_value::{U32Range, U32ScalarValueData, U64Range, U64ScalarValueData};

    #[test]
    fn test_range_checked_sub_interval_arithmetic() {
        // [8,8] - [0,3] = [8-3, 8-0] = [5,8]
        let a = U64Range::new(8, 8);
        let b = U64Range::new(0, 3);
        let result = a.checked_sub(b).unwrap();
        assert_eq!(result.min, 5);
        assert_eq!(result.max, 8);

        // [10,20] - [2,5] = [10-5, 20-2] = [5,18]
        let a = U64Range::new(10, 20);
        let b = U64Range::new(2, 5);
        let result = a.checked_sub(b).unwrap();
        assert_eq!(result.min, 5);
        assert_eq!(result.max, 18);

        // [3,3] - [0,5] = None (3-5 underflows)
        let a = U64Range::new(3, 3);
        let b = U64Range::new(0, 5);
        assert!(a.checked_sub(b).is_none());
    }

    // Regression test: Arithmetic right shift of a fully-unknown u64 must leave the
    // result unbounded. If the sign bit is unknown, all shifted-in bits are also unknown.
    // Before the fix, the masks were logically shifted, which cleared the top bits and
    // incorrectly bounded the range, leading to verifier bypasses.
    #[test]
    fn ashr_fully_unknown_u64_stays_unbounded() {
        let x = U64ScalarValueData::UNKNOWN_WRITTEN;
        assert!(!x.is_known());
        assert_eq!(x.unknown_mask, u64::MAX);
        let shifted = x.ashr(60u64.into());

        assert_eq!(
            shifted.max(),
            u64::MAX,
            "ashr of fully-unknown u64 must leave urange.max = u64::MAX"
        );
        assert_eq!(shifted.unknown_mask, u64::MAX);
    }

    #[test]
    fn ashr_fully_unknown_u32_stays_unbounded() {
        let x = U32ScalarValueData::UNKNOWN_WRITTEN;
        assert!(!x.is_known());
        assert_eq!(x.unknown_mask, u32::MAX);
        let shifted = x.ashr(16u32.into());
        assert_eq!(
            shifted.max(),
            u32::MAX,
            "ashr of fully-unknown u32 must leave urange.max = u32::MAX"
        );
        assert_eq!(shifted.unknown_mask, u32::MAX);
    }

    // Non-negative inputs (known zero sign bit) should still narrow correctly.
    #[test]
    fn ashr_partial_unknown_with_known_zero_msb_narrows_correctly() {
        // Input in [0, 0xFF]. After arsh 4, output should be in [0, 0xF].
        let x = U64ScalarValueData::new(
            /*value=*/ 0,
            /*unknown_mask=*/ 0x00FF,
            /*unwritten_mask=*/ 0,
            U64Range::new(0, 0xFF),
        );
        let shifted = x.ashr(4u64.into());
        assert_eq!(shifted.max(), 0xF);
        assert_eq!(shifted.min(), 0);
        assert_eq!(shifted.unknown_mask, 0xF);
    }

    // Negative inputs (known one sign bit) must sign-extend correctly.
    #[test]
    fn ashr_known_negative_msb_sign_extends() {
        // MSB is 1, bottom byte is unknown.
        let x = U64ScalarValueData::new(
            /*value=*/ 0x8000_0000_0000_0000,
            /*unknown_mask=*/ 0x00FF,
            /*unwritten_mask=*/ 0,
            U64Range::max(),
        );
        let shifted = x.ashr(4u64.into());
        // Result must be sign-extended and retain unknown bottom bits.
        assert_eq!(shifted.value & 0xFFFF_FFFF_FFFF_FF00, 0xF800_0000_0000_0000);
        assert_eq!(shifted.unknown_mask, 0x0F);
    }

    #[test]
    fn test_min_max_known() {
        // Range min == max, should clear unknown mask completely
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(8, 8));
        assert_eq!(data.is_known(), true);
        assert_eq!(data.value, 8);
        assert_eq!(data.unknown_mask, 0);
    }

    #[test]
    fn test_min_max_leading_bits() {
        // Leading common bits: min=4 (0b0100), max=5 (0b0101)
        // Common leading mask: 0b1100
        // bit 2 is 1, bit 1 is 0
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(4, 5));
        assert_eq!(data.unknown_mask & 0b1100, 0);
        assert_eq!(data.value & 0b1100, 0b0100);
        assert_eq!(data.is_known(), false);
    }

    #[test]
    fn test_min_max_no_common_bits() {
        // No common bits: min=0, max=15
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(0, 15));
        assert_eq!(data.unknown_mask, 0xf);
        assert_eq!(data.value, 0);
    }

    #[test]
    fn test_min_max_retains_known_bits() {
        // Initially, bit 3 is known to be 0 (value=0, unknown_mask=0b0111).
        // Range is [4, 5] (0b0100 to 0b0101).
        // Common leading bits for [4, 5] are bits 1..63.
        // Bits 1 and 2 should become known (0 and 1 respectively).
        // Bit 3 should remain known (0).
        let data = U64ScalarValueData::new(0, 0b0111, 0, U64Range::new(4, 5));
        assert_eq!(data.unknown_mask, 0b0001);
        assert_eq!(data.value, 4);
    }

    #[test]
    fn test_unwritten_mask_narrowing() {
        // When urange narrows leading bits (e.g. [0, 1]), those bits become
        // known and written, while the remaining bits stay unknown and
        // unwritten.
        let data = U64ScalarValueData::new(0, u64::MAX, u64::MAX, U64Range::new(0, 1));
        assert!(!data.is_known());
        assert!(!data.is_fully_initialized());
        assert_eq!(data.unknown_mask, 1);
        assert_eq!(data.unwritten_mask, 1);
        assert_eq!(data.min(), 0);
        assert_eq!(data.max(), 1);

        // BitAnd with a non-contiguous mask clears unknown_mask and
        // unwritten_mask on all 0 bits of the mask (even non-leading ones).
        let masked = U64ScalarValueData::UNINITIALIZED & U64ScalarValueData::from(0x0f0fu64);
        assert_eq!(masked.value, 0);
        assert_eq!(masked.unknown_mask, 0x0f0f);
        assert_eq!(masked.unwritten_mask, 0x0f0f);

        // BitOr with a constant clears unknown_mask and unwritten_mask on all 1
        // bits of the constant and sets those bits in value.
        let ored = U64ScalarValueData::UNINITIALIZED | U64ScalarValueData::from(0x0f0fu64);
        assert_eq!(ored.value, 0x0f0f);
        assert_eq!(ored.unknown_mask, !0x0f0f);
        assert_eq!(ored.unwritten_mask, !0x0f0f);

        // Combining BitOr and BitAnd that together overwrite all bits yields a
        // known, initialized constant.
        let initialized = ored & U64ScalarValueData::from(0x0f0fu64);
        assert!(initialized.is_known());
        assert!(initialized.is_fully_initialized());
        assert_eq!(initialized.value, 0x0f0f);
        assert_eq!(initialized.unknown_mask, 0);
        assert_eq!(initialized.unwritten_mask, 0);

        // When urange is a single constant [0, 0] (e.g. `and %r0, 0`), all bits
        // are overwritten with 0, so both unknown_mask and unwritten_mask
        // become 0.
        let zeroed = data & U64ScalarValueData::from(0u64);
        assert!(zeroed.is_known());
        assert!(zeroed.is_fully_initialized());
        assert_eq!(zeroed.value, 0);
        assert_eq!(zeroed.unknown_mask, 0);
        assert_eq!(zeroed.unwritten_mask, 0);

        // Symmetrically, BitOr with all ones (e.g. `or %r0, -1`) overwrites all
        // bits with 1.
        let all_ones = data | U64ScalarValueData::from(u64::MAX);
        assert!(all_ones.is_known());
        assert!(all_ones.is_fully_initialized());
        assert_eq!(all_ones.value, u64::MAX);
        assert_eq!(all_ones.unknown_mask, 0);
        assert_eq!(all_ones.unwritten_mask, 0);

        // BitAnd of an unwritten masked value with an unknown-but-written
        // value: a bit can be 1 only where `masked` is not known to be 0, so
        // the masks shrink to `masked`'s, and the unwritten bits of `masked`
        // stay unwritten.
        let unknown = U64ScalarValueData::UNKNOWN_WRITTEN;
        let and_masked_unknown = masked & unknown;
        assert_eq!(and_masked_unknown.value, 0);
        assert_eq!(and_masked_unknown.unknown_mask, 0x0f0f);
        assert_eq!(and_masked_unknown.unwritten_mask, 0x0f0f);
        assert_eq!(and_masked_unknown.max(), 0x0f0f);

        // BitAnd of a value with known 1 bits (`ored`) with an
        // unknown-but-written value: the known 1 bits become unknown (as the
        // other side is unknown there), but written, as they only depend on the
        // written operand.
        let and_ored_unknown = ored & unknown;
        assert_eq!(and_ored_unknown.value, 0);
        assert_eq!(and_ored_unknown.unknown_mask, u64::MAX);
        assert_eq!(and_ored_unknown.unwritten_mask, !0x0f0f);

        // BitOr of an unwritten masked value with an unknown-but-written value:
        // no bit is known, and only the bits that may come from `masked` remain
        // unwritten.
        let or_masked_unknown = masked | unknown;
        assert_eq!(or_masked_unknown.value, 0);
        assert_eq!(or_masked_unknown.unknown_mask, u64::MAX);
        assert_eq!(or_masked_unknown.unwritten_mask, 0x0f0f);

        // BitOr of `ored` with an unknown-but-written value: the known 1 bits
        // of `ored` are preserved as known and written; everything else is
        // unknown and unwritten.
        let or_ored_unknown = ored | unknown;
        assert_eq!(or_ored_unknown.value, 0x0f0f);
        assert_eq!(or_ored_unknown.unknown_mask, !0x0f0f);
        assert_eq!(or_ored_unknown.unwritten_mask, !0x0f0f);
        assert_eq!(or_ored_unknown.min(), 0x0f0f);

        // Partially known and fully written: only the low byte is unknown.
        let partial = unknown & U64ScalarValueData::from(0xffu64);
        assert_eq!(partial.value, 0);
        assert_eq!(partial.unknown_mask, 0xff);
        assert_eq!(partial.unwritten_mask, 0);

        // BitAnd of two partially unknown values with overlapping masks: only
        // bits that are not known to be 0 on both sides (0xff & 0x0f0f) stay
        // unknown, and among them only the bits unwritten in `masked` stay
        // unwritten.
        let and_partial_masked = partial & masked;
        assert_eq!(and_partial_masked.value, 0);
        assert_eq!(and_partial_masked.unknown_mask, 0x0f);
        assert_eq!(and_partial_masked.unwritten_mask, 0x0f);
        assert_eq!(and_partial_masked.max(), 0x0f);

        // BitOr of the same two values: no bit is known to be 1, so the masks
        // are simply unioned.
        let or_partial_masked = partial | masked;
        assert_eq!(or_partial_masked.value, 0);
        assert_eq!(or_partial_masked.unknown_mask, 0x0fff);
        assert_eq!(or_partial_masked.unwritten_mask, 0x0f0f);

        // Known 1 bits in the low byte, unknown but written elsewhere.
        let known_low = unknown | U64ScalarValueData::from(0xffu64);
        assert_eq!(known_low.value, 0xff);
        assert_eq!(known_low.unknown_mask, !0xff);
        assert_eq!(known_low.unwritten_mask, 0);

        // BitOr with `masked`: the known 1 bits overwrite the unwritten low
        // bits of `masked`, leaving only its high nibble unwritten.
        let or_known_low_masked = known_low | masked;
        assert_eq!(or_known_low_masked.value, 0xff);
        assert_eq!(or_known_low_masked.unknown_mask, !0xff);
        assert_eq!(or_known_low_masked.unwritten_mask, 0x0f00);

        // BitAnd with `ored`: bits known to be 1 on both sides (0xff & 0x0f0f)
        // are known 1, all other bits are unknown, and only the bits unwritten
        // in `ored` are unwritten.
        let and_known_low_ored = known_low & ored;
        assert_eq!(and_known_low_ored.value, 0x0f);
        assert_eq!(and_known_low_ored.unknown_mask, !0x0f);
        assert_eq!(and_known_low_ored.unwritten_mask, !0x0f0f);
        assert_eq!(and_known_low_ored.min(), 0x0f);
    }

    #[test]
    fn test_u32_compute_range_for_bytes_swap() {
        let target = U32Range::new(0x1122_3344, 0x1122_3344);
        let source = U32Range::new(0x0000_aabb, 0x0000_aacc);
        let swapped = U32Range::compute_range_for_bytes_swap(target, source, 1, 0, 2);
        assert_eq!(swapped, U32Range::new(0x11aa_bb44, 0x11aa_cc44));
    }

    #[test]
    fn test_contains_masked_value_and_try_new_exhaustive_5bit_and_high_bits() {
        for min in 0u64..32 {
            for max in 0u64..32 {
                let range64 = U64Range::new(min, max);
                let range32 = U32Range::new(min as u32, max as u32);
                for unknown_mask in 0u64..32 {
                    for value in 0u64..32 {
                        if value & unknown_mask != 0 {
                            continue;
                        }
                        let expected =
                            min <= max && (min..=max).any(|x| (x & !unknown_mask) == value);
                        assert_eq!(
                            range64.contains_masked_value(value, unknown_mask),
                            expected,
                            "u64 min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            U64ScalarValueData::try_new(value, unknown_mask, 0, range64).is_some(),
                            expected,
                            "u64 try_new min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            range32.contains_masked_value(value as u32, unknown_mask as u32),
                            expected,
                            "u32 min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            U32ScalarValueData::try_new(
                                value as u32,
                                unknown_mask as u32,
                                0,
                                range32,
                            )
                            .is_some(),
                            expected,
                            "u32 try_new min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                    }
                }
            }
        }

        // Highest bit (`b == BITS - 1`) coverage for both `u64` and `u32`.
        let top64 = 1u64 << 63;
        let top_range64 = U64Range::new(top64 - 1, top64);
        assert!(top_range64.contains_masked_value(0, top64 | 2));
        assert!(U64ScalarValueData::try_new(0, top64 | 2, 0, top_range64).is_some());
        assert!(!top_range64.contains_masked_value(1, top64 | 2));
        assert!(U64ScalarValueData::try_new(1, top64 | 2, 0, top_range64).is_none());

        let top32 = 1u32 << 31;
        let top_range32 = U32Range::new(top32 - 1, top32);
        assert!(top_range32.contains_masked_value(0, top32 | 2));
        assert!(U32ScalarValueData::try_new(0, top32 | 2, 0, top_range32).is_some());
        assert!(!top_range32.contains_masked_value(1, top32 | 2));
        assert!(U32ScalarValueData::try_new(1, top32 | 2, 0, top_range32).is_none());
    }

    #[test]
    fn test_can_be_equal_common_prefix_and_msb_gap() {
        // Shared `[umin, umax] = [4, 5]` prefix forces bit 2 = 1, turning `val = 2` into `6 > 5`.
        let x = U64ScalarValueData::new(0, 7, 0, U64Range::new(4, 5));
        let y = U64ScalarValueData::new(2, 4, 0, U64Range::new(2, 6));
        assert!(!x.can_be_equal(y));
        assert!(!U32ScalarValueData::from(x).can_be_equal(U32ScalarValueData::from(y)));

        // Gap across the highest differing bit of `umin ^ umax`: `[umin, umax] = [2, 4]` vs
        // combined known bits `(value = 1, unknown_mask = 4)` (`{1, 5}`), where `1 <= 4` and
        // `2 <= 5` both hold yet `{1, 5}` misses `[2, 4]`.
        let a = U64ScalarValueData::new(1, 6, 0, U64Range::new(1, 4));
        let b = U64ScalarValueData::new(0, 5, 0, U64Range::new(2, 5));
        assert!(!a.can_be_equal(b));
        assert!(!U32ScalarValueData::from(a).can_be_equal(U32ScalarValueData::from(b)));
    }

    #[test]
    fn test_new_normalizes_mask_invariants() {
        let s = U64ScalarValueData::new(0b1111, 0b0110, 0b1010, U64Range::new(0, 31));
        assert_eq!(s.value & s.unknown_mask, 0);
        assert_eq!(s.unwritten_mask & !s.unknown_mask, 0);
        assert_eq!(s.value, 0b1001);
        assert_eq!(s.unknown_mask, 0b0110);
        assert_eq!(s.unwritten_mask, 0b0010);
        assert!(U64Range::new(8, 15).contains_masked_value(0b1111, 0b0110));
    }

    #[test]
    fn test_extract_and_assemble_slices_bitwise() {
        const CONST_CHECK: (u64, u64, u64) = U64Range::extract_slices(0x0102_0304_0506_0708, 2, 3);
        assert_eq!(CONST_CHECK, (0x0708, 0x040506, 0x010203));
        const CONST_ASSEMBLED: u64 = U64Range::assemble_slices(CONST_CHECK, 2, 3);
        assert_eq!(CONST_ASSEMBLED, 0x0102_0304_0506_0708);

        let sample64 = 0xfedc_ba98_7654_3210u64;
        let bytes64 = sample64.to_le_bytes();
        for offset in 0..=8 {
            for byte_count in 0..=(8 - offset) {
                let (s1, s2, s3) = U64Range::extract_slices(sample64, offset, byte_count);
                let mut b1 = [0u8; 8];
                b1[..offset].copy_from_slice(&bytes64[..offset]);
                let mut b2 = [0u8; 8];
                b2[..byte_count].copy_from_slice(&bytes64[offset..(offset + byte_count)]);
                let mut b3 = [0u8; 8];
                let rem = 8 - offset - byte_count;
                b3[..rem].copy_from_slice(&bytes64[(offset + byte_count)..]);
                assert_eq!(
                    (s1, s2, s3),
                    (u64::from_le_bytes(b1), u64::from_le_bytes(b2), u64::from_le_bytes(b3)),
                    "u64 extract offset={offset} count={byte_count}"
                );
                assert_eq!(
                    U64Range::assemble_slices((s1, s2, s3), offset, byte_count),
                    sample64,
                    "u64 roundtrip offset={offset} count={byte_count}"
                );
                assert_eq!(
                    U64Range::assemble_slices((u64::MAX, u64::MAX, u64::MAX), offset, byte_count),
                    u64::MAX,
                    "u64 max assemble offset={offset} count={byte_count}"
                );
            }
        }

        let sample32 = 0x89ab_cdefu32;
        let bytes32 = sample32.to_le_bytes();
        for offset in 0..=4 {
            for byte_count in 0..=(4 - offset) {
                let (s1, s2, s3) = U32Range::extract_slices(sample32, offset, byte_count);
                let mut b1 = [0u8; 4];
                b1[..offset].copy_from_slice(&bytes32[..offset]);
                let mut b2 = [0u8; 4];
                b2[..byte_count].copy_from_slice(&bytes32[offset..(offset + byte_count)]);
                let mut b3 = [0u8; 4];
                let rem = 4 - offset - byte_count;
                b3[..rem].copy_from_slice(&bytes32[(offset + byte_count)..]);
                assert_eq!(
                    (s1, s2, s3),
                    (u32::from_le_bytes(b1), u32::from_le_bytes(b2), u32::from_le_bytes(b3)),
                    "u32 extract offset={offset} count={byte_count}"
                );
                assert_eq!(
                    U32Range::assemble_slices((s1, s2, s3), offset, byte_count),
                    sample32,
                    "u32 roundtrip offset={offset} count={byte_count}"
                );
                assert_eq!(
                    U32Range::assemble_slices((u32::MAX, u32::MAX, u32::MAX), offset, byte_count),
                    u32::MAX,
                    "u32 max assemble offset={offset} count={byte_count}"
                );
            }
        }
    }
}
