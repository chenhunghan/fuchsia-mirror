// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ZBI interrupt descriptors and the parsed [`IrqDef`] shared by the SMMUv2 and SMMUv3 drivers.

use crate::arm_smmu::common::{InterruptPolarity, InterruptTriggerMode};

/// ZBI ARM SMMU IRQ flag indicating rising-edge trigger (`ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE: u32 = 1 << 0;

/// ZBI ARM SMMU IRQ flag indicating falling-edge trigger (`ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE: u32 = 1 << 1;

/// ZBI ARM SMMU IRQ flag indicating active-high level polarity
/// (`ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH: u32 = 1 << 2;

/// ZBI ARM SMMU IRQ flag indicating active-low level polarity
/// (`ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW: u32 = 1 << 3;

/// ZBI IRQ descriptor for an ARM SMMU interrupt line.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZbiDcfgArmSmmuIrq {
    /// Interrupt number (GIC SPI/PPI).
    pub num: u32,
    /// Interrupt configuration flags (`ZBI_ARM_SMMU_IRQ_FLAGS_*`).
    pub flags: u32,
}

zr::static_assert!(core::mem::size_of::<ZbiDcfgArmSmmuIrq>() == 8);
zr::static_assert!(core::mem::align_of::<ZbiDcfgArmSmmuIrq>() == 4);

/// Parsed IRQ definition for an SMMU interrupt line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqDef {
    /// Interrupt vector number.
    pub num: u32,
    /// Interrupt flags (`ZBI_ARM_SMMU_IRQ_FLAGS_*`).
    pub flags: u32,
}

impl IrqDef {
    /// Constructs an [`IrqDef`] from a [`ZbiDcfgArmSmmuIrq`].
    #[inline]
    pub const fn from_zbi(irq: ZbiDcfgArmSmmuIrq) -> Self {
        Self { num: irq.num, flags: irq.flags }
    }

    /// Returns `true` if exactly one valid trigger/polarity bit is set in `self.flags`.
    #[inline]
    pub const fn valid(&self) -> bool {
        const VALID_BITS: u32 = ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE
            | ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE
            | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH
            | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW;
        (self.flags & VALID_BITS).count_ones() == 1
    }

    /// Returns the [`InterruptTriggerMode`] for this IRQ.
    #[inline]
    pub const fn trigger_mode(&self) -> InterruptTriggerMode {
        if (self.flags & (ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE | ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE))
            != 0
        {
            InterruptTriggerMode::Edge
        } else {
            InterruptTriggerMode::Level
        }
    }

    /// Returns the [`InterruptPolarity`] for this IRQ.
    #[inline]
    pub const fn polarity(&self) -> InterruptPolarity {
        if (self.flags & (ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH))
            != 0
        {
            InterruptPolarity::High
        } else {
            InterruptPolarity::Low
        }
    }
}
