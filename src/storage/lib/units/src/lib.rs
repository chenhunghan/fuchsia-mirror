// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Types for representing and performing efficient power-of-two block size arithmetic and
//! alignments.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt::{self, Debug};
use std::hash::Hash;
use std::ops::{
    Add, AddAssign, Div, DivAssign, Mul, MulAssign, Range, Rem, RemAssign, Sub, SubAssign,
};

#[cfg(target_os = "fuchsia")]
mod page;
#[cfg(target_os = "fuchsia")]
pub use crate::page::*;

mod private {
    pub trait Sealed {}
}

/// Sealed supertrait implemented by all [`BlockSize`] tags, including both domain-specific
/// [`BlockSizeTag`] types and [`UniversalBlockSizeTag`].
pub trait AnyBlockSizeTag:
    private::Sealed + Copy + Clone + Debug + PartialEq + Eq + PartialOrd + Ord + Hash
{
}

/// Trait for domain-specific [`BlockSize`] tags.
///
/// Implementing `BlockSizeTag` automatically implements [`AnyBlockSizeTag`] and enables converting
/// `BlockSize<T>` into `BlockSize<UniversalBlockSizeTag>` via [`From`]. Tag types are typically
/// zero-sized types (ZSTs), or small `Copy` identifiers (such as a device or volume ID) when
/// disambiguating multiple instances of a domain at runtime.
pub trait BlockSizeTag: AnyBlockSizeTag {}

impl<T: BlockSizeTag> private::Sealed for T {}
impl<T: BlockSizeTag> AnyBlockSizeTag for T {}

/// Represents a block size that is a power of 2, parameterized by an [`AnyBlockSizeTag`].
///
/// Provides zero-cost bitwise operations for alignments, conversions, and arithmetic with
/// integers. Two `BlockSize<T>` values with the same tag type `T` are ordered first by their byte
/// size and then by `tag`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockSize<T = UniversalBlockSizeTag> {
    // Field order matters for derived `PartialOrd` and `Ord`: `mask_shift` is compared first
    // (which orders by block size), followed by `tag`.
    mask_shift: u64,
    tag: T,
}

impl<T: AnyBlockSizeTag> BlockSize<T> {
    /// Constructs a `BlockSize` from a byte count and tag if `block_size` is a power of 2 and at
    /// most 2 GiB (`1 << 31`).
    #[inline(always)]
    pub const fn new_with_tag(block_size: u64, tag: T) -> Option<Self> {
        if block_size.is_power_of_two() && block_size <= (1 << 31) {
            let mask_shift = make_mask_shift(block_size);
            Some(Self { mask_shift, tag })
        } else {
            None
        }
    }

    /// Constructs a `BlockSize` with the same byte size as `other` and the given `tag`.
    #[inline(always)]
    pub const fn new_from<U: AnyBlockSizeTag>(other: BlockSize<U>, tag: T) -> Self {
        Self { mask_shift: other.mask_shift, tag }
    }

    /// Returns the block size in bytes.
    #[inline(always)]
    pub const fn get(self) -> u64 {
        let x = self.mask() + 1;
        // SAFETY: `BlockSize` only stores `mask_shift` values produced by `make_mask_shift` for
        // power-of-two block sizes in `1..=(1 << 31)`, so `self.mask() + 1` is always a power of
        // two. This hint allows the compiler to optimize downstream arithmetic on the returned
        // size.
        unsafe { std::hint::assert_unchecked(x.is_power_of_two()) };
        x
    }

    /// Returns the alignment mask (`size - 1`).
    #[inline(always)]
    pub const fn mask(self) -> u64 {
        let mask = self.mask_shift >> 32;
        // SAFETY: `BlockSize` only stores `mask_shift` values produced by `make_mask_shift` for
        // power-of-two block sizes in `1..=(1 << 31)`, so `mask` is always at most `i32::MAX`.
        // This hint allows the compiler to prove `get()` fits in `u32` and is non-zero as `u32`.
        unsafe { std::hint::assert_unchecked(mask <= i32::MAX as u64) };
        mask
    }

    /// Returns the power-of-two bit shift (e.g. `12` for 4096-byte blocks).
    #[inline(always)]
    pub const fn shift(self) -> u32 {
        let shift = self.mask_shift as u32;
        // SAFETY: `BlockSize` only stores `mask_shift` values produced by `make_mask_shift` for
        // power-of-two block sizes in `1..=(1 << 31)`, so `shift` is in `0..=31` (and thus
        // `<= 32`).
        //
        // Intentionally assert `<= 32` rather than `<= 31`: LLVM rewrites `<= 31` to a bitmask
        // test that proves bit 5 is zero, causing it to narrow 64-bit shift masks from `& 63` to
        // `& 31`. Because x86_64 and AArch64 64-bit shift instructions mask by 63 (not 31), the
        // backend can no longer elide the `& 31` and emits an extra `and` instruction before every
        // 64-bit shift. Asserting `<= 32` still lets LLVM eliminate overflow checks when
        // multiplying a `u32` block count while preserving zero-instruction shift unpacking.
        unsafe { std::hint::assert_unchecked(shift <= 32) };
        shift
    }

    /// Returns `true` if `value` is aligned to this block size.
    #[inline(always)]
    pub fn is_aligned(self, value: impl IsAligned<T>) -> bool {
        value.is_aligned(self)
    }

    /// Aligns `bytes` up to the nearest block boundary, returning `None` on overflow.
    #[inline(always)]
    pub const fn align_up(self, bytes: u64) -> Option<u64> {
        match bytes.checked_add(self.mask()) {
            Some(sum) => {
                let aligned = self.align_down(sum);
                // SAFETY: When `bytes + mask` does not overflow, `(bytes + mask) & !mask` is
                // `(bytes + mask) - ((bytes + mask) & mask) >= bytes`. This hint lets the compiler
                // eliminate underflow checks in downstream calculations like `aligned - bytes`.
                unsafe { std::hint::assert_unchecked(aligned >= bytes) };
                Some(aligned)
            }
            None => None,
        }
    }

    /// Aligns `bytes` down to the nearest block boundary.
    #[inline(always)]
    pub const fn align_down(self, bytes: u64) -> u64 {
        bytes & !self.mask()
    }

    /// Aligns `bytes` up to the nearest block boundary and returns the total number of blocks.
    #[inline(always)]
    pub const fn align_up_to_blocks(self, bytes: u64) -> u64 {
        (bytes >> self.shift()) + (((bytes & self.mask()) != 0) as u64)
    }

    /// Aligns the range outwards to block boundaries (`start` aligned down, `end` aligned up).
    ///
    /// Returns `None` if aligning `end` overflows `u64`.
    #[inline(always)]
    pub fn align_range_outwards(self, range: impl Borrow<Range<u64>>) -> Option<Range<u64>> {
        let range = range.borrow();
        let start = self.align_down(range.start);
        let end = self.align_up(range.end)?;
        Some(start..end)
    }

    /// Multiplies `blocks` by the block size, returning `None` on overflow.
    #[inline(always)]
    pub const fn checked_mul(self, blocks: u64) -> Option<u64> {
        let shift = self.shift();
        if blocks.leading_zeros() >= shift { Some(blocks << shift) } else { None }
    }

    /// Multiplies `blocks` by the block size, saturating at `u64::MAX` on overflow.
    #[inline(always)]
    pub const fn saturating_mul(self, blocks: u64) -> u64 {
        match self.checked_mul(blocks) {
            Some(bytes) => bytes,
            None => u64::MAX,
        }
    }

    /// Returns the tag associated with this block size.
    #[inline(always)]
    pub const fn tag(self) -> T {
        self.tag
    }
}

/// The default tag for [`BlockSize`], representing a block size that is not tied to a specific
/// alignment domain.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UniversalBlockSizeTag;
impl private::Sealed for UniversalBlockSizeTag {}
impl AnyBlockSizeTag for UniversalBlockSizeTag {}

impl BlockSize<UniversalBlockSizeTag> {
    pub const SIZE_1B: Self = Self::new(1 << 0).unwrap();
    pub const SIZE_2B: Self = Self::new(1 << 1).unwrap();
    pub const SIZE_4B: Self = Self::new(1 << 2).unwrap();
    pub const SIZE_8B: Self = Self::new(1 << 3).unwrap();
    pub const SIZE_16B: Self = Self::new(1 << 4).unwrap();
    pub const SIZE_32B: Self = Self::new(1 << 5).unwrap();
    pub const SIZE_64B: Self = Self::new(1 << 6).unwrap();
    pub const SIZE_128B: Self = Self::new(1 << 7).unwrap();
    pub const SIZE_256B: Self = Self::new(1 << 8).unwrap();
    pub const SIZE_512B: Self = Self::new(1 << 9).unwrap();
    pub const SIZE_1KIB: Self = Self::new(1 << 10).unwrap();
    pub const SIZE_2KIB: Self = Self::new(1 << 11).unwrap();
    pub const SIZE_4KIB: Self = Self::new(1 << 12).unwrap();
    pub const SIZE_8KIB: Self = Self::new(1 << 13).unwrap();
    pub const SIZE_16KIB: Self = Self::new(1 << 14).unwrap();
    pub const SIZE_32KIB: Self = Self::new(1 << 15).unwrap();
    pub const SIZE_64KIB: Self = Self::new(1 << 16).unwrap();
    pub const SIZE_128KIB: Self = Self::new(1 << 17).unwrap();
    pub const SIZE_256KIB: Self = Self::new(1 << 18).unwrap();
    pub const SIZE_512KIB: Self = Self::new(1 << 19).unwrap();
    pub const SIZE_1MIB: Self = Self::new(1 << 20).unwrap();

    /// Constructs a `BlockSize` from a byte count if `block_size` is a power of 2 and at most
    /// 2 GiB (`1 << 31`).
    #[inline(always)]
    pub const fn new(block_size: u64) -> Option<Self> {
        Self::new_with_tag(block_size, UniversalBlockSizeTag)
    }
}

macro_rules! impl_binary_op {
    ($trait:ident, $method:ident, $lhs:ty, $rhs:ty, |$a:ident, $b:ident| $expr:expr) => {
        impl<T: AnyBlockSizeTag> $trait<$rhs> for $lhs {
            type Output = u64;
            #[inline(always)]
            fn $method(self, other: $rhs) -> u64 {
                let $a = self;
                let $b = other;
                $expr
            }
        }
    };
}

// Add: BlockSize + u64 and u64 + BlockSize (and reference variants)
impl_binary_op!(Add, add, BlockSize<T>, u64, |bs, val| bs.get() + val);
impl_binary_op!(Add, add, BlockSize<T>, &u64, |bs, val| bs.get() + *val);
impl_binary_op!(Add, add, &BlockSize<T>, u64, |bs, val| bs.get() + val);
impl_binary_op!(Add, add, &BlockSize<T>, &u64, |bs, val| bs.get() + *val);

impl_binary_op!(Add, add, u64, BlockSize<T>, |val, bs| val + bs.get());
impl_binary_op!(Add, add, u64, &BlockSize<T>, |val, bs| val + bs.get());
impl_binary_op!(Add, add, &u64, BlockSize<T>, |val, bs| *val + bs.get());
impl_binary_op!(Add, add, &u64, &BlockSize<T>, |val, bs| *val + bs.get());

// Sub: BlockSize - u64 and u64 - BlockSize (and reference variants)
impl_binary_op!(Sub, sub, BlockSize<T>, u64, |bs, val| bs.get() - val);
impl_binary_op!(Sub, sub, BlockSize<T>, &u64, |bs, val| bs.get() - *val);
impl_binary_op!(Sub, sub, &BlockSize<T>, u64, |bs, val| bs.get() - val);
impl_binary_op!(Sub, sub, &BlockSize<T>, &u64, |bs, val| bs.get() - *val);

impl_binary_op!(Sub, sub, u64, BlockSize<T>, |val, bs| val - bs.get());
impl_binary_op!(Sub, sub, u64, &BlockSize<T>, |val, bs| val - bs.get());
impl_binary_op!(Sub, sub, &u64, BlockSize<T>, |val, bs| *val - bs.get());
impl_binary_op!(Sub, sub, &u64, &BlockSize<T>, |val, bs| *val - bs.get());

#[inline(always)]
fn mul_block_size<T: AnyBlockSizeTag>(val: u64, bs: BlockSize<T>) -> u64 {
    let shift = bs.shift();
    // Preserve the panic on overflow during multiplication in debug builds.
    debug_assert!(val.leading_zeros() >= shift, "attempt to multiply with overflow");
    val << shift
}

// Mul: BlockSize * u64 and u64 * BlockSize (and reference variants)
impl_binary_op!(Mul, mul, BlockSize<T>, u64, |bs, val| mul_block_size(val, bs));
impl_binary_op!(Mul, mul, BlockSize<T>, &u64, |bs, val| mul_block_size(*val, bs));
impl_binary_op!(Mul, mul, &BlockSize<T>, u64, |bs, val| mul_block_size(val, *bs));
impl_binary_op!(Mul, mul, &BlockSize<T>, &u64, |bs, val| mul_block_size(*val, *bs));

impl_binary_op!(Mul, mul, u64, BlockSize<T>, |val, bs| mul_block_size(val, bs));
impl_binary_op!(Mul, mul, u64, &BlockSize<T>, |val, bs| mul_block_size(val, *bs));
impl_binary_op!(Mul, mul, &u64, BlockSize<T>, |val, bs| mul_block_size(*val, bs));
impl_binary_op!(Mul, mul, &u64, &BlockSize<T>, |val, bs| mul_block_size(*val, *bs));

// Div: u64 / BlockSize (and reference variants)
impl_binary_op!(Div, div, u64, BlockSize<T>, |val, bs| val >> bs.shift());
impl_binary_op!(Div, div, u64, &BlockSize<T>, |val, bs| val >> bs.shift());
impl_binary_op!(Div, div, &u64, BlockSize<T>, |val, bs| *val >> bs.shift());
impl_binary_op!(Div, div, &u64, &BlockSize<T>, |val, bs| *val >> bs.shift());

// Rem: u64 % BlockSize (and reference variants)
impl_binary_op!(Rem, rem, u64, BlockSize<T>, |val, bs| val & bs.mask());
impl_binary_op!(Rem, rem, u64, &BlockSize<T>, |val, bs| val & bs.mask());
impl_binary_op!(Rem, rem, &u64, BlockSize<T>, |val, bs| *val & bs.mask());
impl_binary_op!(Rem, rem, &u64, &BlockSize<T>, |val, bs| *val & bs.mask());

macro_rules! impl_assign_op {
    ($trait:ident, $method:ident, $rhs:ty, |$a:ident, $b:ident| $expr:expr) => {
        impl<T: AnyBlockSizeTag> $trait<$rhs> for u64 {
            #[inline(always)]
            fn $method(&mut self, other: $rhs) {
                let $a = self;
                let $b = other;
                $expr
            }
        }
    };
}

// Assign operations: u64 <op>= BlockSize (and reference variants)
impl_assign_op!(AddAssign, add_assign, BlockSize<T>, |val, bs| *val += bs.get());
impl_assign_op!(AddAssign, add_assign, &BlockSize<T>, |val, bs| *val += bs.get());

impl_assign_op!(SubAssign, sub_assign, BlockSize<T>, |val, bs| *val -= bs.get());
impl_assign_op!(SubAssign, sub_assign, &BlockSize<T>, |val, bs| *val -= bs.get());

impl_assign_op!(MulAssign, mul_assign, BlockSize<T>, |val, bs| *val = mul_block_size(*val, bs));
impl_assign_op!(MulAssign, mul_assign, &BlockSize<T>, |val, bs| *val = mul_block_size(*val, *bs));

impl_assign_op!(DivAssign, div_assign, BlockSize<T>, |val, bs| *val >>= bs.shift());
impl_assign_op!(DivAssign, div_assign, &BlockSize<T>, |val, bs| *val >>= bs.shift());

impl_assign_op!(RemAssign, rem_assign, BlockSize<T>, |val, bs| *val &= bs.mask());
impl_assign_op!(RemAssign, rem_assign, &BlockSize<T>, |val, bs| *val &= bs.mask());

macro_rules! impl_partial_eq_ord {
    ($lhs:ty, $rhs:ty, |$a:ident, $b:ident| ($expr_a:expr, $expr_b:expr)) => {
        impl<T: AnyBlockSizeTag> PartialEq<$rhs> for $lhs {
            #[inline(always)]
            fn eq(&self, other: &$rhs) -> bool {
                let $a = self;
                let $b = other;
                $expr_a == $expr_b
            }
        }

        impl<T: AnyBlockSizeTag> PartialOrd<$rhs> for $lhs {
            #[inline(always)]
            fn partial_cmp(&self, other: &$rhs) -> Option<Ordering> {
                let $a = self;
                let $b = other;
                $expr_a.partial_cmp(&$expr_b)
            }
        }
    };
}

// Cross-type comparisons: PartialEq and PartialOrd between BlockSize and u64 (and reference variants)
impl_partial_eq_ord!(BlockSize<T>, u64, |bs, val| (bs.get(), *val));
impl_partial_eq_ord!(BlockSize<T>, &u64, |bs, val| (bs.get(), **val));
impl_partial_eq_ord!(&BlockSize<T>, u64, |bs, val| (bs.get(), *val));
impl_partial_eq_ord!(u64, BlockSize<T>, |val, bs| (*val, bs.get()));
impl_partial_eq_ord!(u64, &BlockSize<T>, |val, bs| (*val, bs.get()));
impl_partial_eq_ord!(&u64, BlockSize<T>, |val, bs| (**val, bs.get()));

macro_rules! impl_fmt {
    ($($trait:ident),*) => {
        $(
            impl<T: AnyBlockSizeTag> fmt::$trait for BlockSize<T> {
                #[inline(always)]
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    fmt::$trait::fmt(&self.get(), f)
                }
            }
        )*
    };
}

// Formatting traits: Display, Binary, LowerHex, UpperHex, Octal
impl_fmt!(Display, Binary, LowerHex, UpperHex, Octal);

/// A trait for checking if something is aligned to a block size.
pub trait IsAligned<T: AnyBlockSizeTag> {
    /// Returns `true` if `self` is aligned to `block_size`.
    fn is_aligned(self, block_size: BlockSize<T>) -> bool;
}

impl<T: AnyBlockSizeTag> IsAligned<T> for u64 {
    #[inline(always)]
    fn is_aligned(self, block_size: BlockSize<T>) -> bool {
        self % block_size == 0
    }
}

impl<T: AnyBlockSizeTag> IsAligned<T> for &u64 {
    #[inline(always)]
    fn is_aligned(self, block_size: BlockSize<T>) -> bool {
        block_size.is_aligned(*self)
    }
}

impl<T: AnyBlockSizeTag> IsAligned<T> for Range<u64> {
    #[inline(always)]
    fn is_aligned(self, block_size: BlockSize<T>) -> bool {
        block_size.is_aligned(&self)
    }
}

impl<T: AnyBlockSizeTag> IsAligned<T> for &Range<u64> {
    #[inline(always)]
    fn is_aligned(self, block_size: BlockSize<T>) -> bool {
        (self.start | self.end) % block_size == 0
    }
}

impl<T: BlockSizeTag> From<BlockSize<T>> for BlockSize<UniversalBlockSizeTag> {
    fn from(other: BlockSize<T>) -> Self {
        Self::new_from(other, UniversalBlockSizeTag)
    }
}

/// Converts a power of 2 `block_size` into the "mask shift" representation. This representation
/// packs the alignment mask in the upper 32 bits and the power-of-two bit shift in the lower 32
/// bits: `(mask << 32) | shift`.
///
/// ### Why this representation was chosen:
///
/// 1. **Zero Dynamic Bit-Scanning Latency:** Representations that only store the size or the mask
///    must dynamically compute `shift` via trailing-zero counting (`trailing_zeros` or
///    `trailing_ones`).
///    - On **ARM64**, computing trailing ones requires a dependent instruction chain: `mvn` +
///      `rbit` + `clz`. On in-order efficiency cores, this 3-instruction dependency stalls the
///      pipeline, making operations like `shift`, `div`, and `mul` more than 2x slower.
///
/// 2. **The 6-Bit Shift Rule (Zero-Instruction Unpacking for Shifts):** Variable shift instructions
///    on both AArch64 (`lsr xd, xn, xm` / `lsl xd, xn, xm`) and x86_64 (`shrx r64, r64, r64`) only
///    inspect the lowest 6 bits of the register operand (`shift_amount mod 64`). Because `shift`
///    (which is between 0 and 31) is placed in the low 32 bits (`[31..0]`), the lowest 6 bits of
///    the raw `u64` are *literally* the shift amount. Consequently, compilers do not need to mask,
///    clear, or move `shift` into a temporary register before shifting. The raw 64-bit register can
///    be passed directly as the shift operand, executing in a **single instruction and single
///    cycle** (`lsr x_val, x_val, x_bs`).
///
/// 3. **ARM64 Fused Barrel Shifter (`adds ..., lsr #32`):** ARM64 ALU instructions feature a
///    hardware barrel shifter that can shift an input operand at zero extra cycle cost. To add,
///    test, or clear the mask on an offset (such as in `align_up`, `align_down`, `is_aligned`, or
///    `%`), the compiler shifts out the low 32 bits and applies the high 32-bit mask in a **single
///    cycle**:
///    ```text
///    adds x_res, x_val, x_bs, lsr #32
///    bic  x_res, x_val, x_bs, lsr #32
///    and  x_res, x_val, x_bs, lsr #32
///    ```
///
/// 4. **Packed `u64` vs. Two Separate `u32` Fields:** Storing a single packed `u64` is superior to
///    a struct with two `u32`s (`{ shift: u32, mask: u32 }`):
///    - **Register Pressure & ABI:** Under Rust's internal calling convention, small multi-field
///      structs are scalarized across multiple registers when passed by value. A packed `u64`
///      always consumes a single argument register, avoiding register pressure and spills in
///      functions with many arguments.
///    - **Register Reuse on ARM64:** With a single `u64`, the exact same register can be fed
///      directly into both the barrel-shifted mask operation (`adds ..., x0, lsr #32`) and the
///      shift (`lsr ..., x0`). Separate fields force values into different registers and require
///      moving or zero-extending them.
const fn make_mask_shift(block_size: u64) -> u64 {
    assert!(block_size.is_power_of_two() && block_size <= (1 << 31));
    let mask = block_size - 1;
    let shift = mask.trailing_ones();
    (mask << 32) | (shift as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_new() {
        assert!(BlockSize::new(0).is_none());
        assert_eq!(BlockSize::new(1).unwrap().get(), 1);
        assert!(BlockSize::new(3).is_none());
        assert_eq!(BlockSize::new(512).unwrap().get(), 512);
        assert_eq!(BlockSize::new(4096).unwrap().get(), 4096);
        assert_eq!(BlockSize::new(1 << 31).unwrap().get(), 1 << 31);
        assert!(BlockSize::new((1 << 31) + 1).is_none());
        assert!(BlockSize::new(u32::MAX as u64).is_none());
        assert!(BlockSize::new(1 << 32).is_none());
        assert!(BlockSize::new(1 << 33).is_none());
        assert!(BlockSize::new(u64::MAX).is_none());
    }

    #[fuchsia::test]
    fn test_getters_and_constants() {
        let bs = BlockSize::SIZE_4KIB;
        assert_eq!(bs.get(), 4096);
        assert_eq!(bs.mask(), 4095);
        assert_eq!(bs.shift(), 12);
        assert_eq!(bs.tag(), UniversalBlockSizeTag);

        assert_eq!(BlockSize::SIZE_1B.get(), 1);
        assert_eq!(BlockSize::SIZE_2B.get(), 2);
        assert_eq!(BlockSize::SIZE_4B.get(), 4);
        assert_eq!(BlockSize::SIZE_8B.get(), 8);
        assert_eq!(BlockSize::SIZE_16B.get(), 16);
        assert_eq!(BlockSize::SIZE_32B.get(), 32);
        assert_eq!(BlockSize::SIZE_64B.get(), 64);
        assert_eq!(BlockSize::SIZE_128B.get(), 128);
        assert_eq!(BlockSize::SIZE_256B.get(), 256);
        assert_eq!(BlockSize::SIZE_512B.get(), 512);
        assert_eq!(BlockSize::SIZE_1KIB.get(), 1024);
        assert_eq!(BlockSize::SIZE_2KIB.get(), 2048);
        assert_eq!(BlockSize::SIZE_4KIB.get(), 4096);
        assert_eq!(BlockSize::SIZE_8KIB.get(), 8192);
        assert_eq!(BlockSize::SIZE_16KIB.get(), 16384);
        assert_eq!(BlockSize::SIZE_32KIB.get(), 32768);
        assert_eq!(BlockSize::SIZE_64KIB.get(), 65536);
        assert_eq!(BlockSize::SIZE_128KIB.get(), 131072);
        assert_eq!(BlockSize::SIZE_256KIB.get(), 262144);
        assert_eq!(BlockSize::SIZE_512KIB.get(), 524288);
        assert_eq!(BlockSize::SIZE_1MIB.get(), 1048576);
    }

    #[fuchsia::test]
    fn test_alignment_helpers() {
        let bs = BlockSize::SIZE_4KIB;

        assert!(bs.is_aligned(0u64));
        assert!(bs.is_aligned(4096u64));
        assert!(bs.is_aligned(&4096u64));
        assert!(!bs.is_aligned(4095u64));
        assert!(!bs.is_aligned(4097u64));

        assert!(bs.is_aligned(0..4096));
        assert!(bs.is_aligned(&(4096..8192)));
        assert!(!bs.is_aligned(0..4095));
        assert!(!bs.is_aligned(1..4096));
        assert!(!bs.is_aligned(1..4095));

        assert_eq!(bs.align_down(0), 0);
        assert_eq!(bs.align_down(4095), 0);
        assert_eq!(bs.align_down(4096), 4096);
        assert_eq!(bs.align_down(4097), 4096);

        assert_eq!(bs.align_up(0), Some(0));
        assert_eq!(bs.align_up(1), Some(4096));
        assert_eq!(bs.align_up(4095), Some(4096));
        assert_eq!(bs.align_up(4096), Some(4096));
        // Exact overflow boundary for 4KiB blocks:
        assert_eq!(bs.align_up(u64::MAX - 4095), Some(u64::MAX - 4095));
        assert_eq!(bs.align_up(u64::MAX - 4094), None);
        assert_eq!(bs.align_up(u64::MAX), None);

        assert_eq!(bs.align_up_to_blocks(0), 0);
        assert_eq!(bs.align_up_to_blocks(1), 1);
        assert_eq!(bs.align_up_to_blocks(4095), 1);
        assert_eq!(bs.align_up_to_blocks(4096), 1);
        assert_eq!(bs.align_up_to_blocks(4097), 2);
        assert_eq!(bs.align_up_to_blocks(u64::MAX), 1 << 52);

        assert_eq!(bs.align_range_outwards(1..4095), Some(0..4096));
        assert_eq!(bs.align_range_outwards(&(0..4096)), Some(0..4096));
        assert_eq!(bs.align_range_outwards(4096..4096), Some(4096..4096));
        assert_eq!(bs.align_range_outwards(10..10), Some(0..4096));
        assert_eq!(bs.align_range_outwards(0..u64::MAX), None);
    }

    #[fuchsia::test]
    fn test_arithmetic_ops() {
        let bs = BlockSize::SIZE_4KIB;
        let bs_ref = &bs;
        let val = 8192u64;
        let val_ref = &val;

        // Add
        assert_eq!(bs + val, 12288);
        assert_eq!(bs + val_ref, 12288);
        assert_eq!(bs_ref + val, 12288);
        assert_eq!(bs_ref + val_ref, 12288);
        assert_eq!(val + bs, 12288);
        assert_eq!(val + bs_ref, 12288);
        assert_eq!(val_ref + bs, 12288);
        assert_eq!(val_ref + bs_ref, 12288);

        // Sub
        assert_eq!(val - bs, 4096);
        assert_eq!(val - bs_ref, 4096);
        assert_eq!(val_ref - bs, 4096);
        assert_eq!(val_ref - bs_ref, 4096);
        assert_eq!(bs - 100u64, 3996);
        assert_eq!(bs - &100u64, 3996);
        assert_eq!(bs_ref - 100u64, 3996);
        assert_eq!(bs_ref - &100u64, 3996);

        // Mul
        assert_eq!(bs * 3u64, 12288);
        assert_eq!(bs * &3u64, 12288);
        assert_eq!(bs_ref * 3u64, 12288);
        assert_eq!(bs_ref * &3u64, 12288);
        assert_eq!(3u64 * bs, 12288);
        assert_eq!(3u64 * bs_ref, 12288);
        assert_eq!(&3u64 * bs, 12288);
        assert_eq!(&3u64 * bs_ref, 12288);

        assert_eq!(bs.checked_mul(0), Some(0));
        assert_eq!(bs.checked_mul(3), Some(12288));
        assert_eq!(bs.checked_mul((1 << 52) - 1), Some(u64::MAX - 4095));
        assert_eq!(bs.checked_mul(1 << 52), None);
        assert_eq!(bs.checked_mul(u64::MAX), None);

        assert_eq!(bs.saturating_mul(0), 0);
        assert_eq!(bs.saturating_mul(3), 12288);
        assert_eq!(bs.saturating_mul((1 << 52) - 1), u64::MAX - 4095);
        assert_eq!(bs.saturating_mul(1 << 52), u64::MAX);
        assert_eq!(bs.saturating_mul(u64::MAX), u64::MAX);

        // Div
        assert_eq!(val / bs, 2);
        assert_eq!(val / bs_ref, 2);
        assert_eq!(val_ref / bs, 2);
        assert_eq!(val_ref / bs_ref, 2);

        // Rem
        assert_eq!(5000u64 % bs, 904);
        assert_eq!(5000u64 % bs_ref, 904);
        assert_eq!(&5000u64 % bs, 904);
        assert_eq!(&5000u64 % bs_ref, 904);
    }

    #[fuchsia::test]
    fn test_assign_ops() {
        let bs = BlockSize::SIZE_4KIB;

        let mut v = 100u64;
        v += bs;
        assert_eq!(v, 4196);
        v += &bs;
        assert_eq!(v, 8292);

        v -= bs;
        assert_eq!(v, 4196);
        v -= &bs;
        assert_eq!(v, 100);

        let mut blocks = 3u64;
        blocks *= bs;
        assert_eq!(blocks, 12288);
        let mut blocks2 = 3u64;
        blocks2 *= &bs;
        assert_eq!(blocks2, 12288);

        let mut bytes = 12288u64;
        bytes /= bs;
        assert_eq!(bytes, 3);
        let mut bytes2 = 12288u64;
        bytes2 /= &bs;
        assert_eq!(bytes2, 3);

        let mut rem = 5000u64;
        rem %= bs;
        assert_eq!(rem, 904);
        let mut rem2 = 5000u64;
        rem2 %= &bs;
        assert_eq!(rem2, 904);
    }

    #[fuchsia::test]
    fn test_comparisons_and_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher;

        #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        struct CustomTag(u32);
        impl BlockSizeTag for CustomTag {}

        let bs1 = BlockSize::SIZE_512B;
        let bs2 = BlockSize::SIZE_4KIB;

        assert!(bs1 < bs2);
        assert!(bs2 > bs1);
        assert_eq!(bs1.min(bs2), bs1);
        assert_eq!(bs1.max(bs2), bs2);

        // Tag-aware comparisons and Hash consistency
        let custom_4k_a1 = BlockSize::new_with_tag(4096, CustomTag(1)).unwrap();
        let custom_4k_a2 = BlockSize::new_from(bs2, CustomTag(1));
        let custom_4k_b = BlockSize::new_with_tag(4096, CustomTag(2)).unwrap();
        let custom_512_b = BlockSize::new_with_tag(512, CustomTag(2)).unwrap();

        assert_eq!(BlockSize::from(custom_4k_a1), bs2);
        assert_eq!(custom_4k_a1, custom_4k_a2);
        assert_ne!(custom_4k_a1, custom_4k_b);
        assert!(custom_4k_a1 < custom_4k_b);
        assert!(custom_4k_b > custom_4k_a1);
        // Block size is ordered before tag: 512B with tag 2 < 4KiB with tag 1
        assert!(custom_512_b < custom_4k_a1);

        fn hash_val<H: Hash>(v: &H) -> u64 {
            let mut hasher = DefaultHasher::new();
            v.hash(&mut hasher);
            hasher.finish()
        }
        assert_eq!(hash_val(&custom_4k_a1), hash_val(&custom_4k_a2));
        assert_ne!(hash_val(&custom_4k_a1), hash_val(&custom_4k_b));
        assert_ne!(hash_val(&bs1), hash_val(&bs2));

        assert_eq!(bs2, 4096u64);
        assert_eq!(bs2, &4096u64);
        assert_eq!(&bs2, 4096u64);
        assert_eq!(&bs2, &4096u64);
        assert_eq!(4096u64, bs2);
        assert_eq!(4096u64, &bs2);
        assert_eq!(&4096u64, bs2);
        assert_eq!(&4096u64, &bs2);

        assert!(bs2 > 512u64);
        assert!(bs2 > &512u64);
        assert!(&bs2 > 512u64);
        assert!(&bs2 > &512u64);
        assert!(512u64 < bs2);
        assert!(512u64 < &bs2);
        assert!(&512u64 < bs2);
        assert!(&512u64 < &bs2);
    }

    #[cfg(debug_assertions)]
    #[fuchsia::test]
    #[should_panic(expected = "attempt to multiply with overflow")]
    fn test_mul_overflow() {
        let _ = BlockSize::SIZE_4KIB * (1u64 << 60);
    }

    #[cfg(debug_assertions)]
    #[fuchsia::test]
    #[should_panic(expected = "attempt to multiply with overflow")]
    fn test_mul_assign_overflow() {
        let mut v = 1u64 << 60;
        v *= BlockSize::SIZE_4KIB;
    }

    #[fuchsia::test]
    fn test_formatting() {
        let bs = BlockSize::SIZE_4KIB;
        assert_eq!(format!("{}", bs), "4096");
        assert_eq!(format!("{:x}", bs), "1000");
        assert_eq!(format!("{:X}", bs), "1000");
        assert_eq!(format!("{:b}", bs), "1000000000000");
        assert_eq!(format!("{:o}", bs), "10000");
    }

    #[fuchsia::test]
    fn test_block_size_one() {
        let block_size = BlockSize::new(1).unwrap();

        assert_eq!(block_size.align_up(0), Some(0));
        assert_eq!(block_size.align_up(20), Some(20));
        assert_eq!(block_size.align_up(u64::MAX), Some(u64::MAX));

        assert_eq!(block_size.align_down(0), 0);
        assert_eq!(block_size.align_down(20), 20);
        assert_eq!(block_size.align_down(u64::MAX), u64::MAX);

        assert_eq!(block_size * 0, 0);
        assert_eq!(block_size * 20, 20);
        assert_eq!(block_size * u64::MAX, u64::MAX);
        assert_eq!(block_size.checked_mul(0), Some(0));
        assert_eq!(block_size.checked_mul(20), Some(20));
        assert_eq!(block_size.checked_mul(u64::MAX), Some(u64::MAX));
        assert_eq!(block_size.saturating_mul(0), 0);
        assert_eq!(block_size.saturating_mul(20), 20);
        assert_eq!(block_size.saturating_mul(u64::MAX), u64::MAX);

        assert_eq!(0 / block_size, 0);
        assert_eq!(20 / block_size, 20);
        assert_eq!(u64::MAX / block_size, u64::MAX);

        assert_eq!(0 % block_size, 0);
        assert_eq!(20 % block_size, 0);
        assert_eq!(u64::MAX % block_size, 0);
    }

    #[fuchsia::test]
    fn test_block_size_max_2gib() {
        let bs = BlockSize::new(1 << 31).unwrap();
        assert_eq!(bs.get(), 1 << 31);
        assert_eq!(bs.mask(), i32::MAX as u64);
        assert_eq!(bs.shift(), 31);

        assert!(bs.is_aligned(0u64));
        assert!(bs.is_aligned(1u64 << 31));
        assert!(!bs.is_aligned((1u64 << 31) - 1));

        assert_eq!(bs.align_up(1), Some(1 << 31));
        assert_eq!(bs.align_down((1 << 31) + 123), 1 << 31);
        assert_eq!(bs * 3u64, 3 << 31);
        assert_eq!(bs.checked_mul(3), Some(3 << 31));
        assert_eq!(bs.checked_mul((1 << 33) - 1), Some(((1u64 << 33) - 1) << 31));
        assert_eq!(bs.checked_mul(1 << 33), None);
        assert_eq!(bs.saturating_mul(3), 3 << 31);
        assert_eq!(bs.saturating_mul((1 << 33) - 1), ((1u64 << 33) - 1) << 31);
        assert_eq!(bs.saturating_mul(1 << 33), u64::MAX);
        assert_eq!((3u64 << 31) / bs, 3);
        assert_eq!(((3u64 << 31) + 55) % bs, 55);
    }
}
