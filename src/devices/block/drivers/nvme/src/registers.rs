// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! NVMe controller memory-mapped register definitions and helpers
//! (NVM Express Base Specification 2.0, section 3.1.3 "Controller Properties").
//!
//! Interrupt mask registers (`INTMS` and `INTMC`) and admin queue base address registers (`ASQ`
//! and `ACQ`) are declared write-only (`mode = WO`) so read-modify-write sequences cannot compile.

use mmio::{register, register_block};
use std::num::NonZeroU32;

/// Offset of `CAP` (Controller Capabilities, 64-bit RO).
pub const NVME_REG_CAP: usize = 0x00;

/// Offset of `VS` (Version, 32-bit RO).
pub const NVME_REG_VS: usize = 0x08;

/// Offset of `INTMS` (Interrupt Mask Set, 32-bit WO from host perspective).
pub const NVME_REG_INTMS: usize = 0x0c;

/// Offset of `INTMC` (Interrupt Mask Clear, 32-bit WO from host perspective).
pub const NVME_REG_INTMC: usize = 0x10;

/// Offset of `CC` (Controller Configuration, 32-bit RW).
pub const NVME_REG_CC: usize = 0x14;

/// Offset of `CSTS` (Controller Status, 32-bit RO).
pub const NVME_REG_CSTS: usize = 0x1c;

/// Offset of `AQA` (Admin Queue Attributes, 32-bit RW).
pub const NVME_REG_AQA: usize = 0x24;

/// Offset of `ASQ` (Admin Submission Queue Base Address, 64-bit RW, host writes full value).
pub const NVME_REG_ASQ: usize = 0x28;

/// Offset of `ACQ` (Admin Completion Queue Base Address, 64-bit RW, host writes full value).
pub const NVME_REG_ACQ: usize = 0x30;

/// Base offset of Submission/Completion Queue Doorbell registers in BAR 0.
pub const NVME_REG_DOORBELL_BASE: usize = 0x1000;

/// Default controller ready/reset poll interval for periodic diagnostic logging when `CAP.TO == 0`.
const DEFAULT_RESET_WAIT_MS: u64 = 5_000;

/// Minimum clamped controller ready/reset diagnostic timeout in milliseconds.
const MIN_RESET_WAIT_MS: u64 = 500;

/// Maximum clamped controller ready/reset diagnostic timeout in milliseconds.
const MAX_RESET_WAIT_MS: u64 = 30_000;

/// Mask of page-aligned bits `[63:12]` for `ASQ` and `ACQ` base addresses.
const ADMIN_QUEUE_ADDR_MASK: u64 = !0xfff;

register! {
    #[register(offset = NVME_REG_CAP, mode = RO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.1: `CAP` - Controller Capabilities.
    pub struct CapabilityReg(u64) {
        pub controller_ready_independent_media_supported, _: 60;
        pub controller_ready_with_media_supported, _: 59;
        pub subsystem_shutdown_supported, _: 58;
        pub controller_memory_buffer_supported, _: 57;
        pub persistent_memory_region_supported, _: 56;
        pub u8, memory_page_size_max, _: 55, 52;
        pub u8, memory_page_size_min, _: 51, 48;
        pub u8, controller_power_scope, _: 47, 46;
        pub boot_partition_support, _: 45;
        pub no_io_command_set_support, _: 44;
        pub identify_io_command_set_support, _: 43;
        pub nvm_command_set_support, _: 37;
        pub nvm_subsystem_reset_supported, _: 36;
        pub u8, doorbell_stride, _: 35, 32;
        pub u8, timeout, _: 31, 24;
        pub vendor_specific_arbitration_supported, _: 18;
        pub weighted_round_robin_arbitration_supported, _: 17;
        pub contiguous_queues_required, _: 16;
        pub u16, max_queue_entries_raw, _: 15, 0;
    }

    #[register(offset = NVME_REG_VS, mode = RO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.2: `VS` - Version.
    #[derive(PartialOrd, Ord)]
    pub struct VersionReg(u32) {
        pub u16, major, _: 31, 16;
        pub u8, minor, _: 15, 8;
        pub u8, tertiary, _: 7, 0;
    }

    #[register(offset = NVME_REG_INTMS, mode = WO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.3: `INTMS` - Interrupt Mask Set.
    ///
    /// Writing 1s masks the corresponding interrupt vectors; 0s have no effect. Must not be
    /// accessed when MSI-X is active.
    pub struct InterruptMaskSetReg(u32) {
        pub interrupts, set_interrupts: 31, 0;
    }

    #[register(offset = NVME_REG_INTMC, mode = WO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.4: `INTMC` - Interrupt Mask Clear.
    ///
    /// Writing 1s unmasks the corresponding interrupt vectors; 0s have no effect. Must not be
    /// accessed when MSI-X is active.
    pub struct InterruptMaskClearReg(u32) {
        pub interrupts, set_interrupts: 31, 0;
    }

    #[register(offset = NVME_REG_CC, mode = RW)]
    /// NVM Express Base Specification 2.0, section 3.1.3.5: `CC` - Controller Configuration.
    pub struct ControllerConfigReg(u32) {
        pub controller_ready_independent_of_media, set_controller_ready_independent_of_media: 24;
        pub u8, io_completion_queue_entry_size, set_io_completion_queue_entry_size: 23, 20;
        pub u8, io_submission_queue_entry_size, set_io_submission_queue_entry_size: 19, 16;
        pub u8, from into ShutdownNotification, shutdown_notification, set_shutdown_notification: 15, 14;
        pub u8, from into ArbitrationMechanism, arbitration_mechanism, set_arbitration_mechanism: 13, 11;
        pub u8, memory_page_size, set_memory_page_size: 10, 7;
        pub u8, from into IoCommandSet, io_command_set, set_io_command_set: 6, 4;
        pub enabled, set_enabled: 0;
    }

    #[register(offset = NVME_REG_CSTS, mode = RO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.6: `CSTS` - Controller Status.
    pub struct ControllerStatusReg(u32) {
        pub shutdown_type, _: 6;
        pub processing_paused, _: 5;
        pub subsystem_reset_occurred, _: 4;
        pub u8, from into ShutdownStatus, shutdown_status, _: 3, 2;
        pub controller_fatal_status, _: 1;
        pub ready, _: 0;
    }

    #[register(offset = NVME_REG_AQA, mode = RW)]
    /// NVM Express Base Specification 2.0, section 3.1.3.8: `AQA` - Admin Queue Attributes.
    pub struct AdminQueueAttributesReg(u32) {
        pub u16, completion_queue_size, set_completion_queue_size: 27, 16;
        pub u16, submission_queue_size, set_submission_queue_size: 11, 0;
    }

    #[register(offset = NVME_REG_ASQ, mode = WO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.9: `ASQ` - Admin Submission Queue Base
    /// Address. Declared write-only so host software always writes a fresh 64-bit address without
    /// reading stale hardware bits.
    pub struct AdminSubmissionQueueBaseReg(u64);

    #[register(offset = NVME_REG_ACQ, mode = WO)]
    /// NVM Express Base Specification 2.0, section 3.1.3.10: `ACQ` - Admin Completion Queue Base
    /// Address. Declared write-only so host software always writes a fresh 64-bit address without
    /// reading stale hardware bits.
    pub struct AdminCompletionQueueBaseReg(u64);
}

register_block! {
    /// NVMe controller property register block in BAR 0 (`0x0000..0x1000`).
    pub struct NvmeRegisterBlock<M> {
        pub cap: CapabilityReg,
        pub vs: VersionReg,
        pub intms: InterruptMaskSetReg,
        pub intmc: InterruptMaskClearReg,
        pub cc: ControllerConfigReg,
        pub csts: ControllerStatusReg,
        pub aqa: AdminQueueAttributesReg,
        pub asq: AdminSubmissionQueueBaseReg,
        pub acq: AdminCompletionQueueBaseReg,
    }
}

/// Shutdown Notification values in `CC.SHN` (NVMe Base Spec 2.0, section 3.1.3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShutdownNotification {
    None = 0,
    Normal = 1,
    Abrupt = 2,
    Reserved = 3,
}

impl From<u8> for ShutdownNotification {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Normal,
            2 => Self::Abrupt,
            _ => Self::Reserved,
        }
    }
}

impl From<ShutdownNotification> for u8 {
    fn from(value: ShutdownNotification) -> Self {
        value as u8
    }
}

/// Arbitration Mechanism Selected values in `CC.AMS` (NVMe Base Spec 2.0, section 3.1.3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbitrationMechanism {
    RoundRobin,
    WeightedRoundRobinUrgent,
    VendorSpecific,
    Reserved(u8),
}

impl From<u8> for ArbitrationMechanism {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::RoundRobin,
            1 => Self::WeightedRoundRobinUrgent,
            7 => Self::VendorSpecific,
            other => Self::Reserved(other),
        }
    }
}

impl From<ArbitrationMechanism> for u8 {
    fn from(value: ArbitrationMechanism) -> Self {
        match value {
            ArbitrationMechanism::RoundRobin => 0,
            ArbitrationMechanism::WeightedRoundRobinUrgent => 1,
            ArbitrationMechanism::VendorSpecific => 7,
            ArbitrationMechanism::Reserved(other) => other,
        }
    }
}

/// I/O Command Set Selected values in `CC.CSS` (NVMe Base Spec 2.0, section 3.1.3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoCommandSet {
    Nvm,
    AllSupportedIo,
    AdminOnly,
    Reserved(u8),
}

impl From<u8> for IoCommandSet {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Nvm,
            6 => Self::AllSupportedIo,
            7 => Self::AdminOnly,
            other => Self::Reserved(other),
        }
    }
}

impl From<IoCommandSet> for u8 {
    fn from(value: IoCommandSet) -> Self {
        match value {
            IoCommandSet::Nvm => 0,
            IoCommandSet::AllSupportedIo => 6,
            IoCommandSet::AdminOnly => 7,
            IoCommandSet::Reserved(other) => other,
        }
    }
}

/// Shutdown Status values in `CSTS.SHST` (NVMe Base Spec 2.0, section 3.1.3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShutdownStatus {
    NoShutdown = 0,
    Occurring = 1,
    Complete = 2,
    Reserved = 3,
}

impl From<u8> for ShutdownStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::NoShutdown,
            1 => Self::Occurring,
            2 => Self::Complete,
            _ => Self::Reserved,
        }
    }
}

impl From<ShutdownStatus> for u8 {
    fn from(value: ShutdownStatus) -> Self {
        value as u8
    }
}

impl CapabilityReg {
    /// Maximum memory page size supported by the controller in bytes (`2^(12 + MPSMAX)`).
    pub fn memory_page_size_max_bytes(&self) -> u32 {
        // `memory_page_size_max()` is a 4-bit field (`0..=15`), so `12 + mpsmax <= 27 < 32`.
        1u32 << (12 + self.memory_page_size_max())
    }

    /// Minimum memory page size supported by the controller in bytes (`2^(12 + MPSMIN)`).
    pub fn memory_page_size_min_bytes(&self) -> u32 {
        // `memory_page_size_min()` is a 4-bit field (`0..=15`), so `12 + mpsmin <= 27 < 32`.
        1u32 << (12 + self.memory_page_size_min())
    }

    /// Doorbell stride in bytes (`4 << CAP.DSTRD` = `2^(2 + DSTRD)`).
    pub fn doorbell_stride_bytes(&self) -> usize {
        4usize << self.doorbell_stride()
    }

    /// Total doorbell MMIO span in bytes for a single SQ + CQ pair (`2 * doorbell_stride_bytes`).
    pub fn queue_pair_doorbell_bytes(&self) -> usize {
        2 * self.doorbell_stride_bytes()
    }

    /// Worst-case ready transition timeout reported by `CAP.TO` in milliseconds (`500ms * TO`).
    pub fn timeout_ms(&self) -> u32 {
        u32::from(self.timeout()) * 500
    }

    /// Controller ready/shutdown diagnostic interval in milliseconds, clamped to
    /// `[MIN_RESET_WAIT_MS, MAX_RESET_WAIT_MS]` and defaulting to `DEFAULT_RESET_WAIT_MS` when
    /// `CAP.TO == 0`.
    pub fn controller_timeout_ms(&self) -> u64 {
        let raw_ms = u64::from(self.timeout_ms());
        if raw_ms == 0 {
            DEFAULT_RESET_WAIT_MS
        } else {
            raw_ms.clamp(MIN_RESET_WAIT_MS, MAX_RESET_WAIT_MS)
        }
    }

    /// Maximum individual queue size supported by the controller (`CAP.MQES + 1`).
    pub fn max_queue_entries(&self) -> u32 {
        u32::from(self.max_queue_entries_raw()) + 1
    }

    /// Computes the controller's maximum data transfer size in bytes from `IdentifyController.MDTS`
    /// and `CAP.MPSMIN` (`2^(12 + MPSMIN + MDTS)`), returning `None` if `mdts == 0` (no controller
    /// limit) or if `2^(12 + MPSMIN + MDTS)` exceeds `u32::MAX`.
    pub fn max_data_transfer_bytes(&self, mdts: u8) -> Option<NonZeroU32> {
        if mdts == 0 {
            return None;
        }
        let shift = 12u32 + u32::from(self.memory_page_size_min()) + u32::from(mdts);
        1u32.checked_shl(shift).and_then(NonZeroU32::new)
    }
}

impl VersionReg {
    /// Constructs a [`VersionReg`] from `(major, minor, tertiary)` version components.
    pub const fn from_parts(major: u16, minor: u8, tertiary: u8) -> Self {
        Self(((major as u32) << 16) | ((minor as u32) << 8) | (tertiary as u32))
    }
}

impl InterruptMaskSetReg {
    /// Constructs an `INTMS` write value that masks only `vector` (`1 << vector`).
    pub fn for_vector(vector: u32) -> Self {
        assert!(vector < u32::BITS, "interrupt vector {vector} out of 32-bit INTMS range");
        Self(1u32 << vector)
    }
}

impl InterruptMaskClearReg {
    /// Constructs an `INTMC` write value that unmasks only `vector` (`1 << vector`).
    pub fn for_vector(vector: u32) -> Self {
        assert!(vector < u32::BITS, "interrupt vector {vector} out of 32-bit INTMC range");
        Self(1u32 << vector)
    }
}

impl ControllerStatusReg {
    /// Returns `true` when `CSTS` reads all-ones (`0xffff_ffff`), indicating PCIe link loss or
    /// surprise device removal.
    pub const fn is_removed(self) -> bool {
        self.0 == u32::MAX
    }
}

impl AdminSubmissionQueueBaseReg {
    /// Constructs an `ASQ` register value from a 4 KiB-aligned physical address.
    pub fn from_phys_addr(paddr: u64) -> Self {
        assert_eq!(paddr & !ADMIN_QUEUE_ADDR_MASK, 0, "ASQ address must be 4 KiB aligned");
        Self(paddr)
    }
}

impl AdminCompletionQueueBaseReg {
    /// Constructs an `ACQ` register value from a 4 KiB-aligned physical address.
    pub fn from_phys_addr(paddr: u64) -> Self {
        assert_eq!(paddr & !ADMIN_QUEUE_ADDR_MASK, 0, "ACQ address must be 4 KiB aligned");
        Self(paddr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::Register;

    #[fuchsia::test]
    fn capability_reg_computes_limits_and_timeouts() {
        // MQES=511 (512 entries), TO=20 (10_000 ms), DSTRD=1 (8B stride), MPSMIN=0 (4 KiB),
        // MPSMAX=2 (16 KiB).
        let raw = (2u64 << 52) | (1u64 << 32) | (20u64 << 24) | 511u64;
        let cap = CapabilityReg::from_raw(raw);
        assert_eq!(cap.max_queue_entries(), 512);
        assert_eq!(cap.timeout_ms(), 10_000);
        assert_eq!(cap.controller_timeout_ms(), 10_000);
        assert_eq!(cap.doorbell_stride_bytes(), 8);
        assert_eq!(cap.queue_pair_doorbell_bytes(), 16);
        assert_eq!(cap.memory_page_size_min_bytes(), 4096);
        assert_eq!(cap.memory_page_size_max_bytes(), 16384);

        // MDTS = 0 means unbounded; MDTS = 5 -> 4 KiB * 32 = 128 KiB; MDTS = 31 overflows u32 and
        // returns None (unbounded within u32).
        assert_eq!(cap.max_data_transfer_bytes(0), None);
        assert_eq!(cap.max_data_transfer_bytes(5).map(NonZeroU32::get), Some(131_072));
        assert_eq!(cap.max_data_transfer_bytes(19).map(NonZeroU32::get), Some(1u32 << 31));
        assert_eq!(cap.max_data_transfer_bytes(20), None);
        assert_eq!(cap.max_data_transfer_bytes(31), None);

        // CAP.TO == 0 defaults to 5_000 ms; CAP.TO == 255 (127_500 ms) clamps to 30_000 ms.
        assert_eq!(CapabilityReg::from_raw(0).controller_timeout_ms(), DEFAULT_RESET_WAIT_MS);
        assert_eq!(
            CapabilityReg::from_raw(255u64 << 24).controller_timeout_ms(),
            MAX_RESET_WAIT_MS
        );
    }

    #[fuchsia::test]
    fn version_reg_ordering_and_decoding() {
        let v1_3 = VersionReg::from_parts(1, 3, 0);
        let v1_4 = VersionReg::from_parts(1, 4, 0);
        let v2_0 = VersionReg::from_parts(2, 0, 1);
        assert_eq!(v2_0.major(), 2);
        assert_eq!(v2_0.minor(), 0);
        assert_eq!(v2_0.tertiary(), 1);
        assert!(v1_3 < v1_4);
        assert!(v1_4 >= VersionReg::from_parts(1, 4, 0));
        assert!(v2_0 > v1_4);
    }

    #[fuchsia::test]
    #[should_panic(expected = "ASQ address must be 4 KiB aligned")]
    fn admin_sq_base_rejects_misaligned_address() {
        let _ = AdminSubmissionQueueBaseReg::from_phys_addr(0x1004);
    }

    #[fuchsia::test]
    #[should_panic(expected = "ACQ address must be 4 KiB aligned")]
    fn admin_cq_base_rejects_misaligned_address() {
        let _ = AdminCompletionQueueBaseReg::from_phys_addr(0x1800);
    }
}
