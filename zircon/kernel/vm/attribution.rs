// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use attribution_bindings as bindings;

/// Structure to store fractional counts of bytes in fixed point with 63 bits of precision. The max
/// fractional value is thus `2-Fraction::Epsilon()`. Counts are strictly unsigned.
///
/// This structure supports accumulation with other fractional counts and the code will handle the
/// internal bookkeeping around moving whole bytes from the fractional sum to the integral sum.
///
/// The structure also supports addition, subtraction, and division with whole integers. If
/// overflow occurs in either direction, an assert is fired. Multiplication is not supported.
///
/// We always check and guarantee that any fractional sums >=1 have the excess byte stripped off
/// and rolled over to the integral value. Thus, since accumulation always operates on fractions
/// `<=1-Fraction::Epsilon()`, it always generates results `<= 2-Fraction::Epsilon()` and there is
/// never overflow in the fractional fields.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FractionalBytes {
    pub integral: usize,
    pub fractional: u64,
}

zr::static_assert!(
    core::mem::size_of::<FractionalBytes>() == core::mem::size_of::<bindings::vm_FractionalBytes>()
);
zr::static_assert!(
    core::mem::align_of::<FractionalBytes>()
        == core::mem::align_of::<bindings::vm_FractionalBytes>()
);

impl core::ops::AddAssign<u64> for FractionalBytes {
    fn add_assign(&mut self, other: u64) {
        let (res, overflow) = self.integral.overflowing_add(other as usize);
        debug_assert!(!overflow);
        self.integral = res;
    }
}

impl core::ops::Add<u64> for FractionalBytes {
    type Output = Self;
    fn add(mut self, other: u64) -> Self {
        self += other;
        self
    }
}

impl core::ops::SubAssign<u64> for FractionalBytes {
    fn sub_assign(&mut self, other: u64) {
        let (res, overflow) = self.integral.overflowing_sub(other as usize);
        debug_assert!(!overflow);
        self.integral = res;
    }
}

impl core::ops::Sub<u64> for FractionalBytes {
    type Output = Self;
    fn sub(mut self, other: u64) -> Self {
        self -= other;
        self
    }
}

impl core::ops::DivAssign<u64> for FractionalBytes {
    fn div_assign(&mut self, other: u64) {
        // Input fraction must always be <1 to guard against overflow.
        // If this is true, the sum of fractions must be <1:
        // The sum is:
        //  `(fractional / other) + (1 / other) * remainder`
        // which we can rewrite as
        //  `(fractional + remainder) / other`
        // We know that fractional < 1 and remainder < other, thus (fractional + remainder) < other and
        // the rewritten sum cannot be >=1.
        debug_assert!(self.fractional < Self::ONE_BYTE);
        let remainder = (self.integral as u64) % other;
        let scaled_remainder = (Self::ONE_BYTE / other) * remainder;
        self.fractional = (self.fractional / other) + scaled_remainder;
        debug_assert!(self.fractional < Self::ONE_BYTE);
        self.integral /= other as usize;
    }
}

impl core::ops::Div<u64> for FractionalBytes {
    type Output = Self;
    fn div(mut self, other: u64) -> Self {
        self /= other;
        self
    }
}

impl FractionalBytes {
    pub const ONE_BYTE: u64 = bindings::vm_FractionalBytes_kOneByteValue;

    pub const fn from_whole(whole_bytes: u64) -> Self {
        Self { integral: whole_bytes as usize, fractional: 0 }
    }

    pub const fn from_fraction(numerator: u64, denominator: u64) -> Self {
        let integral = (numerator / denominator) as usize;
        let fractional = (Self::ONE_BYTE / denominator) * (numerator % denominator);
        Self { integral, fractional }
    }

    pub fn add(&mut self, other: &Self) {
        // Input fractions must always be <1 to guard against overflow.
        // If the fractional sum is >=1, then roll that overflow byte into the integral part.
        debug_assert!(self.fractional < Self::ONE_BYTE);
        debug_assert!(other.fractional < Self::ONE_BYTE);
        let mut new_fractional = self.fractional + other.fractional;
        let mut rollover = 0usize;
        if new_fractional >= Self::ONE_BYTE {
            rollover = 1;
            new_fractional -= Self::ONE_BYTE;
        }
        let (new_integral, overflow) = self.integral.overflowing_add(other.integral);
        debug_assert!(!overflow);
        let (new_integral, overflow) = new_integral.overflowing_add(rollover);
        debug_assert!(!overflow);
        self.integral = new_integral;
        self.fractional = new_fractional;
    }
}

impl core::ops::Add for FractionalBytes {
    type Output = Self;

    fn add(mut self, other: Self) -> Self {
        FractionalBytes::add(&mut self, &other);
        self
    }
}

impl core::ops::AddAssign<&FractionalBytes> for FractionalBytes {
    fn add_assign(&mut self, other: &FractionalBytes) {
        self.add(other);
    }
}

impl core::ops::AddAssign<FractionalBytes> for FractionalBytes {
    fn add_assign(&mut self, other: FractionalBytes) {
        self.add(&other);
    }
}

/// Structure to store counts of memory attributed to VMOs or portions thereof.
///
/// These counts can be accumulated to support attributing memory across composite objects such as
/// address spaces or processes.
///
/// The `scaled_bytes` fields may contain a fractional number of bytes, and the structure stores the
/// fractional counts in fixed point with 63 bits of precision.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AttributionCounts {
    pub uncompressed_bytes: usize,
    pub compressed_bytes: usize,
    pub private_uncompressed_bytes: usize,
    pub private_compressed_bytes: usize,
    pub scaled_uncompressed_bytes: FractionalBytes,
    pub scaled_compressed_bytes: FractionalBytes,
}

zr::static_assert!(
    core::mem::size_of::<AttributionCounts>()
        == core::mem::size_of::<bindings::vm_AttributionCounts>()
);
zr::static_assert!(
    core::mem::align_of::<AttributionCounts>()
        == core::mem::align_of::<bindings::vm_AttributionCounts>()
);

impl AttributionCounts {
    pub const fn zero() -> Self {
        Self {
            uncompressed_bytes: 0,
            compressed_bytes: 0,
            private_uncompressed_bytes: 0,
            private_compressed_bytes: 0,
            scaled_uncompressed_bytes: FractionalBytes { integral: 0, fractional: 0 },
            scaled_compressed_bytes: FractionalBytes { integral: 0, fractional: 0 },
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.uncompressed_bytes += other.uncompressed_bytes;
        self.compressed_bytes += other.compressed_bytes;
        self.private_uncompressed_bytes += other.private_uncompressed_bytes;
        self.private_compressed_bytes += other.private_compressed_bytes;
        self.scaled_uncompressed_bytes += other.scaled_uncompressed_bytes;
        self.scaled_compressed_bytes += other.scaled_compressed_bytes;
    }

    pub fn total_bytes(&self) -> usize {
        self.uncompressed_bytes + self.compressed_bytes
    }

    pub fn total_private_bytes(&self) -> usize {
        self.private_uncompressed_bytes + self.private_compressed_bytes
    }

    pub fn total_scaled_bytes(&self) -> FractionalBytes {
        self.scaled_uncompressed_bytes + self.scaled_compressed_bytes
    }
}

impl core::ops::AddAssign<&AttributionCounts> for AttributionCounts {
    fn add_assign(&mut self, other: &AttributionCounts) {
        self.add(other);
    }
}

impl core::ops::AddAssign<AttributionCounts> for AttributionCounts {
    fn add_assign(&mut self, other: AttributionCounts) {
        self.add(&other);
    }
}
