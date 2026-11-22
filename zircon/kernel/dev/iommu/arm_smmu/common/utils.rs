// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Small utility types shared by the SMMUv2 and SMMUv3 drivers: [`Bitmask128`] and [`BtiMode`].

/// Fixed-capacity 128-bit allocation and tracking bitmask for SMMUv2 Stream Match Register Groups
/// (up to 128 SMRGs) and Context Banks (up to 128 CBs), and for SMMUv3 context descriptor slots.
///
/// Uses native `u128` bit intrinsics (`count_ones`, `trailing_zeros`, `trailing_ones`) in a single
/// word because the SMMUv2 architectural maximum for both SMRGs and Context Banks is 128.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bitmask128(u128);

impl Bitmask128 {
    /// Total number of bits managed by [`Bitmask128`].
    pub const NUM_BITS: u32 = 128;

    /// Constructs a new [`Bitmask128`] with all bits cleared (`0`).
    #[inline]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Constructs a [`Bitmask128`] from a raw `u128` bit pattern.
    #[inline]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// Returns the raw `u128` bit pattern.
    #[inline]
    pub const fn raw(&self) -> u128 {
        self.0
    }

    /// Sets bit `bit` (`0 <= bit < 128`) to `1`.
    #[inline]
    pub fn set_bit(&mut self, bit: u32) {
        debug_assert!(bit < Self::NUM_BITS);
        self.0 |= 1u128 << bit;
    }

    /// Clears bit `bit` (`0 <= bit < 128`) to `0`.
    #[inline]
    pub fn clear_bit(&mut self, bit: u32) {
        debug_assert!(bit < Self::NUM_BITS);
        self.0 &= !(1u128 << bit);
    }

    /// Assigns bit `bit` (`0 <= bit < 128`) to `val`.
    #[inline]
    pub fn assign_bit(&mut self, bit: u32, val: bool) {
        if val {
            self.set_bit(bit);
        } else {
            self.clear_bit(bit);
        }
    }

    /// Returns `true` if bit `bit` (`0 <= bit < 128`) is set.
    #[inline]
    pub const fn test_bit(&self, bit: u32) -> bool {
        debug_assert!(bit < Self::NUM_BITS);
        (self.0 & (1u128 << bit)) != 0
    }

    /// Sets the lowest `n` bits (`0 <= n <= 128`) to `1` (ORing them into the existing mask).
    #[inline]
    pub fn set_lowest_n_bits(&mut self, n: u32) {
        debug_assert!(n <= Self::NUM_BITS);
        // `n == 128` is what an SMMU with the architectural maximum of 128 SMRGs or context
        // banks produces. It is handled explicitly, as `1u128 << 128` would overflow.
        if n == Self::NUM_BITS {
            self.0 = u128::MAX;
        } else if n > 0 {
            self.0 |= (1u128 << n) - 1;
        }
    }

    /// Sets all 128 bits to `1`.
    #[inline]
    pub fn set_all(&mut self) {
        self.0 = u128::MAX;
    }

    /// Clears all 128 bits to `0`.
    #[inline]
    pub fn clear_all(&mut self) {
        self.0 = 0;
    }

    /// Returns the number of bits currently set to `1`.
    #[inline]
    pub const fn num_set_bits(&self) -> u32 {
        self.0.count_ones()
    }

    /// Returns the index (`0..128`) of the least-significant set bit, or `None` if all bits are
    /// zero.
    #[inline]
    pub const fn find_first_set_bit(&self) -> Option<u32> {
        if self.0 == 0 { None } else { Some(self.0.trailing_zeros()) }
    }

    /// Returns the index (`0..128`) of the least-significant clear bit, or `None` if all bits are
    /// one.
    #[inline]
    pub const fn find_first_clr_bit(&self) -> Option<u32> {
        if self.0 == u128::MAX { None } else { Some(self.0.trailing_ones()) }
    }

    /// Returns an iterator yielding the bit indices (`0..128`) of all set bits in ascending order.
    #[inline]
    pub const fn iter_set_bits(&self) -> Bitmask128Iter {
        Bitmask128Iter { remaining: self.0 }
    }
}

/// Iterator over the set bit indices (`0..128`) of a [`Bitmask128`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bitmask128Iter {
    remaining: u128,
}

impl Iterator for Bitmask128Iter {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let bit = self.remaining.trailing_zeros();
        self.remaining &= self.remaining - 1;
        Some(bit)
    }
}

impl IntoIterator for Bitmask128 {
    type Item = u32;
    type IntoIter = Bitmask128Iter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_set_bits()
    }
}

impl IntoIterator for &Bitmask128 {
    type Item = u32;
    type IntoIter = Bitmask128Iter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_set_bits()
    }
}

/// Operational mode for an `SmmuV2Bti` or `ContextBank`.
///
/// This enum is used in two places, at the BTI level, and at the context bank level for the
/// (single) context bank owned by a BTI. Note that when operating in fully enforced mode, the BTI
/// may be in the Fault state (preventing new pin operations) while its ContextBank remains in
/// Translation mode in order to continue to allow access to actively pinned PMTs, while
/// specifically denying access to leaked PMT regions.
///
/// In the context of a Context bank, the values used in this enum are not specific to any one
/// register value. Instead, they reflect the way that we choose to model the three modes of
/// operation using the CBAR (gr1), SCTLR (cb(N)), and TCR (cb(N)) registers.
///
/// Their definitions for each mode are as follows:
///
/// ```text
/// Mode        | CBAR.TYPE   | SCTLR.M | TCR.EPD0 | TCR.EPD1 | Notes
/// ------------+-------------+---------+----------+----------+---------------------------------
/// Translation | S1TS2Bypass |    1    |    0     |    1     | MMU enabled, TTBR0 enabled
/// Bypass      | S1TS2Bypass |    0    |    1     |    1     | MMU + TTBRs Disabled
/// Fault       | S1TS2Bypass |    1    |    0     |    0     | MMU enabled, TTBRs disabled
/// Adopted     |    ???      |   ???   |   ???    |   ???    | Register config at adoption.
/// Shutdown    | S1TS2Bypass |    1    |    0     |    0     | Same as fault, deny all access.
/// ```
///
/// Note that all of the modes (except adopted) use Stage 1 Translate, Stage 2 Bypass as their
/// CBAR type. Stage 2's configuration is frequently under the control of either the hypervisor or
/// the secure monitor, which may deny any attempt to put the system into a S1TS2Fault mode in
/// order to force faulting. Additionally, while we might specify either S2Bypass or S2Translate,
/// we typically have no control over Stage 2 behavior from EL1. We configure for S2Bypass,
/// assuming that there is no S2 translation going on, however it may be the case that the system
/// chooses to either change our written value to S1TS2Translate if it wants to perform
/// translation, or to lie to us by reporting that our CBAR.TYPE is S1TS2Bypass even though (under
/// the hood) it has actually configured for Stage 2 Translation.
///
/// Either way, we are still able to represent all three of the primary modes with S1TS2Bypass. For
/// full translation, we enable the MMU, and TTBR0 which is configured to point to our page tables.
/// TTBR1 is not currently used in any mode. For bypass mode, we disable the MMU, which means that
/// all transactions which reach this context bank are simply accepted and passed through as is
/// with no translation. Finally, when configured for Fault/Shutdown, the MMU is enabled, but both
/// TTBRs are disabled, meaning there are no valid/active translation table entries, ensuring that
/// "translation" in Fault mode always fails.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtiMode {
    /// Fault mode: A fault has occurred. Either a PMT has been leaked, or the HW attempted to
    /// access something it didn't have access to and we noticed. Access is restricted until
    /// user-mode takes control of their HW and signals that it has by calling
    /// `zx_bti_release_quarantine`.
    Fault = 0,
    /// Passthru mode: No translation is performed, all accesses are allowed.
    Bypass = 1,
    /// Enforced mode: Translation is performed, only pinned memory can be accessed.
    Translation = 2,
    /// Adopted mode: The configuration is whatever was passed to us by our bootloader.
    Adopted = 3,
    /// Shutdown: The BTI has been shutdown. HW should be in Fault mode, but cannot ever return to
    /// an operational mode. The BTI is about to be destroyed.
    Shutdown = 4,
    /// Uninitialized or invalid mode.
    Invalid = 5,
}

/// Returns the human-readable string representation of a [`BtiMode`].
#[inline]
pub const fn bti_mode_to_string(mode: BtiMode) -> &'static str {
    match mode {
        BtiMode::Fault => "Fault",
        BtiMode::Bypass => "Bypass",
        BtiMode::Translation => "Translation",
        BtiMode::Adopted => "Adopted",
        BtiMode::Shutdown => "Shutdown",
        BtiMode::Invalid => "Invalid",
    }
}
