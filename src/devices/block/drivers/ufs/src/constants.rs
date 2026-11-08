// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Protocol, register, descriptor, and driver constants for UFS 3.1/4.0 and UFSHCI 3.0/4.0.

// --- Driver bring-up, bus, and scheduling constants ---

/// Idle time, in microseconds, after which the host autonomously enters link Hibernate; written
/// to `AHIT` by `configure_auto_hibernate` (UFSHCI 3.0 section 5.2.5).
pub const AUTO_HIBERNATE_TIMER_US: u32 = 2000;
/// Tag identifying the UFS bandwidth vote on the `fuchsia.hardware.interconnect` path.
pub const INTERCONNECT_TAG_UFS: u32 = u32::from_be_bytes(*b"UFS ");
/// Average bandwidth voted on the interconnect path while the device is `Active`.
pub const INTERCONNECT_AVG_BANDWIDTH_BPS: u64 = 2_000_000_000;
/// Peak bandwidth voted on the interconnect path while the device is `Active`: two lanes at
/// HS-G5 Rate B after 8b/10b line coding (UFS 4.0 section 5).
pub const INTERCONNECT_PEAK_BANDWIDTH_BPS: u64 = 4_640_000_000;
/// Smallest MMIO window accepted for the host controller: the standard UFSHCI register map
/// (host capabilities through the vendor-specific block) spans offsets `0x00..0xFF` (UFSHCI 4.0
/// section 5.1).
pub const MIN_MMIO_SIZE: u64 = 0x100;
/// Composite parent instance name of the platform device service.
pub const PDEV_INSTANCE: &str = "pdev";
/// Composite parent instance name of the `fuchsia.hardware.ufs.phy` service.
pub const PHY_INSTANCE: &str = "phy";
/// Composite parent instance name of the `fuchsia.hardware.interconnect` path service.
pub const INTERCONNECT_INSTANCE: &str = "ufs-interconnect";
/// Composite parent instance name of the PCI device service.
pub const PCI_INSTANCE: &str = "pci";
/// Number of `TEST UNIT READY` attempts per LU during bring-up. The first command after a reset
/// or power-on fails with `UNIT ATTENTION`, which the retry clears (UFS 3.1/4.0 section 11.3.1).
pub const TEST_UNIT_READY_ATTEMPTS: u32 = 3;
/// Scheduler role applied to the driver executor and block session threads.
pub const WORKER_SCHEDULER_ROLE: &str = "fuchsia.devices.storage.ufs.worker";
/// Dedicated scheduler role applied to the interrupt worker threads so completion notifications
/// are not starved by block worker threads.
pub const IRQ_SCHEDULER_ROLE: &str = "fuchsia.devices.storage.ufs.irq";

// --- UFSHCI register, UTRD, UCD, and transfer processor constants (UFSHCI 3.0/4.0 sec 5 - 7) ---

/// Byte granularity of `MCQCAP.QCFGPTR`: the per-queue configuration block starts at
/// `QCFGPTR * 0x200` (UFSHCI 4.0 sections 5.2.2 and 5.2.9).
pub const MCQ_QUEUE_CONFIG_PTR_UNIT: usize = 0x200;
/// Time allowed for `HCE` to toggle on reset/enable and for a UIC command issued during link
/// startup to complete (UFSHCI 3.0/4.0 sections 5.3.3, 5.3.4, and 7.1.1).
pub const HOST_CONTROLLER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Number of `DME_LINKSTARTUP` attempts before bring-up or recovery gives up (UFSHCI 3.0/4.0
/// section 7.1.1 allows the first attempt to fail while the device is still booting).
pub const LINK_STARTUP_ATTEMPTS: u32 = 3;
/// Delay between [`LINK_STARTUP_ATTEMPTS`] so the device can finish its own link startup.
pub const LINK_STARTUP_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(10);
/// Largest `AHIT.AH8ITV` (10-bit idle timer value, UFSHCI 3.0 section 5.2.5).
pub const AUTO_HIBERNATE_MAX_TIMER_VALUE: u32 = 0x3FF;
/// Largest `AHIT.TS` timer scale (`5` selects 100 ms steps, UFSHCI 3.0 section 5.2.5).
pub const AUTO_HIBERNATE_MAX_TIMER_SCALE: u32 = 5;
/// `UECPA.EC` error code for `PA_LINE_RESET`, which is reported but not fatal (UFSHCI 3.0/4.0
/// section 5.3.5).
pub const UIC_PHY_ADAPTER_LAYER_LINE_RESET: u32 = 0x10;

/// Largest transfer request list the driver tracks; bounds the per-slot state arrays (`UTRLBA`
/// list, UFSHCI 3.0/4.0 section 5.4.3).
pub const MAX_REQUEST_LIST_SIZE: u8 = 64;
/// Transfer slots in single-doorbell mode: one per `UTRLDBR` bit (UFSHCI 3.0/4.0 section 5.4.3).
pub const SDB_SLOTS: usize = 32;
/// Slot reserved for Admin/Query UPIUs so block I/O cannot starve control commands.
pub const ADMIN_COMMAND_SLOT_NUMBER: u8 = (SDB_SLOTS - 1) as u8;
/// `UTRD.CT` command type for UFS storage commands (UFSHCI 3.0/4.0 section 6.1.1).
pub const COMMAND_TYPE_UFS_STORAGE: u8 = 0x1;
/// Byte offset of dword 2 (`OCS`) inside a UTRD, polled directly after a posted completion
/// (UFSHCI 3.0/4.0 section 6.1.1).
pub const OCS_OFFSET: usize = 8;
/// `UTRD.I` interrupt bit: request a completion interrupt for this slot (UFSHCI 3.0/4.0 section
/// 6.1.1).
pub const UTRD_INTERRUPT_BIT: u32 = 1 << 24;
/// Bit position of `UTRD.DD` (data direction) in dword 0 (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_DATA_DIRECTION_SHIFT: u32 = 25;
/// Width mask of the 2-bit `UTRD.DD` field (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_DATA_DIRECTION_MASK: u32 = 0x3;
/// Bit position of `UTRD.CT` (command type) in dword 0 (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_COMMAND_TYPE_SHIFT: u32 = 28;
/// Width mask of the 4-bit `UTRD.CT` field (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_COMMAND_TYPE_MASK: u32 = 0xF;
/// Mask of the 8-bit `UTRD.OCS` (overall command status) field in dword 2 (UFSHCI 3.0/4.0
/// section 6.1.1).
pub const UTRD_OCS_MASK: u32 = 0xFF;
/// Size of one UTP Command Descriptor: command UPIU, response UPIU, and PRDT (UFSHCI 3.0/4.0
/// section 6.1.2). 8 KiB leaves room for [`MAX_PRDT_ENTRIES`] plus the inline data tail.
pub const UTP_COMMAND_DESCRIPTOR_SIZE: usize = 8192;
/// Size of the inline data tail at the end of each UCD, used to DMA small `HostToDevice`
/// payloads (such as `UNMAP` parameter lists) without pinning a client VMO.
pub const INLINE_DATA_SIZE: usize = 64;
/// Offset of the inline data tail inside a UCD.
pub const INLINE_DATA_OFFSET: usize = UTP_COMMAND_DESCRIPTOR_SIZE - INLINE_DATA_SIZE;
/// Largest PRDT the driver builds per command; with 4 KiB pages this bounds a transfer to 1 MiB
/// (UFSHCI 3.0/4.0 section 6.1.2, `PRDTL`).
pub const MAX_PRDT_ENTRIES: usize = 256;
/// Largest byte count of one PRDT entry: the 18-bit `DBC` field holds `bytes - 1` (UFSHCI 3.0/4.0
/// section 6.1.2).
pub const MAX_PRDT_ENTRY_BYTES: u32 = 256 * 1024;
/// Mask of the 18-bit PRDT `DBC` (data byte count minus one) field (UFSHCI 3.0/4.0 section
/// 6.1.2).
pub const PRDT_DBC_MASK: u32 = MAX_PRDT_ENTRY_BYTES - 1;
/// Low bits that must be clear in PRDT addresses and byte counts, which are dword granular
/// (UFSHCI 3.0/4.0 section 6.1.2).
pub const DWORD_ALIGN_MASK: u64 = 0x3;

/// Time a UTP transfer request may stay outstanding before the stall watchdog logs an error.
pub const COMMAND_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Spin re-reads of `UTRD.OCS` after a completion is signalled, covering the window in which the
/// controller's posted write of the UTRD has not yet landed in memory.
pub const OCS_RETRIES: u32 = 100;
/// Retries of a SCSI command that completes with `UNIT ATTENTION`, `NOT READY`, `BUSY` or
/// `TASK SET FULL` (SAM-5 section 5.3).
pub const SCSI_RETRIES: u32 = 3;
/// Delay before each [`SCSI_RETRIES`] attempt.
pub const SCSI_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(5);
/// Largest transfer, in pages, whose PRDT and physical address list are built on the stack
/// instead of in a heap allocation.
pub const SMALL_TRANSFER_PAGES: usize = 32;

// --- MIPI UniPro / UIC DME constants (UniPro v1.8/v2.0, UFSHCI 3.0/4.0 section 5.6) ---

/// `PA_AvailTxDataLanes`: number of TX lanes physically available (MIPI UniPro v2.0 section
/// 5.8.1, PHY Adapter M-TX attributes).
pub const PA_AVAILTXDATALANES: u16 = 0x1520;
/// `PA_AvailRxDataLanes`: number of RX lanes physically available (MIPI UniPro v2.0 section
/// 5.8.2, PHY Adapter M-RX attributes).
pub const PA_AVAILRXDATALANES: u16 = 0x1540;
/// `PA_ActiveTxDataLanes`: TX lanes to use after the next power mode change (MIPI UniPro v2.0
/// section 5.8.1).
pub const PA_ACTIVETXDATALANES: u16 = 0x1560;
/// `PA_TxGear`: TX gear requested for the next power mode change (MIPI UniPro v2.0 section
/// 5.8.1).
pub const PA_TXGEAR: u16 = 0x1568;
/// `PA_TxTermination`: enable TX line termination for HS mode (MIPI UniPro v2.0 section 5.8.1).
pub const PA_TXTERMINATION: u16 = 0x1569;
/// `PA_HSSeries`: HS rate series (A or B) for the next power mode change (MIPI UniPro v2.0
/// section 5.8.3, PHY Adapter common attributes).
pub const PA_HSSERIES: u16 = 0x156A;
/// `PA_PWRMode`: writing it triggers the power mode change for both directions (MIPI UniPro v2.0
/// section 5.8.3).
pub const PA_PWRMODE: u16 = 0x1571;
/// `PA_ActiveRxDataLanes`: RX lanes to use after the next power mode change (MIPI UniPro v2.0
/// section 5.8.2).
pub const PA_ACTIVERXDATALANES: u16 = 0x1580;
/// `PA_RxGear`: RX gear requested for the next power mode change (MIPI UniPro v2.0 section
/// 5.8.2).
pub const PA_RXGEAR: u16 = 0x1583;
/// `PA_RxTermination`: enable RX line termination for HS mode (MIPI UniPro v2.0 section 5.8.2).
pub const PA_RXTERMINATION: u16 = 0x1584;
/// `PA_MaxRxHSGear`: highest HS gear the local (or, via `DME_PEER_GET`, the peer) receiver
/// supports (MIPI UniPro v2.0 section 5.8.2).
pub const PA_MAXRXHSGEAR: u16 = 0x1587;
/// `PA_RemoteVerInfo`: UniPro version of the peer (MIPI UniPro v2.0 section 5.8.3, PHY Adapter
/// common attributes).
pub const PA_REMOTEVERINFO: u16 = 0x15A0;
/// `PA_TActivate`: time the peer needs to leave the low-power state, in `PA_Granularity` units
/// (MIPI UniPro v2.0 section 5.8.3).
pub const PA_TACTIVATE: u16 = 0x15A8;
/// `PA_LocalVerInfo`: UniPro version of the local link end (MIPI UniPro v2.0 section 5.8.3).
pub const PA_LOCALVERINFO: u16 = 0x15A9;
/// `PA_Granularity`: time unit of `PA_TActivate` and related timers (MIPI UniPro v2.0 section
/// 5.8.3).
pub const PA_GRANULARITY: u16 = 0x15AA;
/// `PA_PWRModeUserData0`: Data Link `FC0ProtectionTimeOutVal` applied by the power mode change
/// (MIPI UniPro v2.0 section 5.8.3, UFS 4.0 section 7.4).
pub const PA_PWRMODEUSERDATA0: u16 = 0x15B0;
/// `PA_PWRModeUserData1`: Data Link `TC0ReplayTimeOutVal` applied by the power mode change.
pub const PA_PWRMODEUSERDATA1: u16 = 0x15B1;
/// `PA_PWRModeUserData2`: Data Link `AFC0ReqTimeOutVal` applied by the power mode change.
pub const PA_PWRMODEUSERDATA2: u16 = 0x15B2;
/// `PA_PWRModeUserData3`: Data Link `FC1ProtectionTimeOutVal` applied by the power mode change.
pub const PA_PWRMODEUSERDATA3: u16 = 0x15B3;
/// `PA_PWRModeUserData4`: Data Link `TC1ReplayTimeOutVal` applied by the power mode change.
pub const PA_PWRMODEUSERDATA4: u16 = 0x15B4;
/// `PA_PWRModeUserData5`: Data Link `AFC1ReqTimeOutVal` applied by the power mode change.
pub const PA_PWRMODEUSERDATA5: u16 = 0x15B5;
/// `PA_TxHsAdaptType`: equalizer adaptation to run before an HS-G4 or faster gear change (MIPI
/// UniPro v2.0 section 5.8.3).
pub const PA_TXHSADAPTTYPE: u16 = 0x15D4;

/// Host-local shadow of `DL_FC0ProtectionTimeOutVal`, written alongside `PA_PWRModeUserData0`
/// so the host's Data Link timers match the negotiated values (UFS 4.0 section 7.4 / MIPI UniPro
/// v2.0 section 5.9).
pub const DME_LOCAL_FC0_PROTECTION_TIMEOUT_VAL: u16 = 0xD041;
/// Host-local shadow of `DL_TC0ReplayTimeOutVal` (UFS 4.0 section 7.4 / MIPI UniPro v2.0 section
/// 5.9).
pub const DME_LOCAL_TC0_REPLAY_TIMEOUT_VAL: u16 = 0xD042;
/// Host-local shadow of `DL_AFC0ReqTimeOutVal` (UFS 4.0 section 7.4 / MIPI UniPro v2.0 section
/// 5.9).
pub const DME_LOCAL_AFC0_REQ_TIMEOUT_VAL: u16 = 0xD043;
/// Default `DL_FC0ProtectionTimeOutVal`: largest 13-bit value, so flow control never times out
/// prematurely in HS mode (UFS 4.0 section 7.4).
pub const DL_FC0_PROTECTION_TIMEOUT_VAL: u32 = 8191;
/// Default `DL_TC0ReplayTimeOutVal`: largest 16-bit value (UFS 4.0 section 7.4).
pub const DL_TC0_REPLAY_TIMEOUT_VAL: u32 = 65535;
/// Default `DL_AFC0ReqTimeOutVal`: largest 15-bit value (UFS 4.0 section 7.4).
pub const DL_AFC0_REQ_TIMEOUT_VAL: u32 = 32767;
/// Sleep between register polls (`HCE`, `UCRDY`, UIC completion, doorbells) once the spin
/// budget is exhausted.
pub const REGISTER_POLL_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);
/// Time a register poll spins before it starts sleeping [`REGISTER_POLL_INTERVAL`] between
/// reads, so fast completions are not delayed by a timer.
pub const SPIN_POLL_BUDGET: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);
/// `PA_TxHsAdaptType` value requesting the full initial adaptation sequence (MIPI UniPro v2.0
/// section 5.8.3).
pub const PA_INITIAL_ADAPT: u32 = 0x01;
/// `PA_PWRMode` nibble selecting HS `Fast_Mode` for one direction (MIPI UniPro v2.0 section
/// 5.8.3).
const FAST_MODE: u32 = 1;
/// Bit position of the RX mode nibble in `PA_PWRMode`; bits 3:0 hold the TX mode (MIPI UniPro
/// v2.0 section 5.8.3).
const PWR_MODE_RX_SHIFT: u32 = 4;
/// `PA_PWRMode` value for HS `Fast_Mode` in both directions.
pub const FAST_PWR_MODE: u32 = (FAST_MODE << PWR_MODE_RX_SHIFT) | FAST_MODE;
/// `PA_TxTermination`/`PA_RxTermination` value enabling line termination, required for HS gears
/// (MIPI UniPro v2.0 sections 5.8.1 and 5.8.2).
pub const TERMINATION_ON: u32 = 1;
/// `PA_HSSeries` value for Rate B, the higher of the two HS rate series (MIPI UniPro v2.0
/// section 5.8.3).
pub const HS_SERIES_B: u32 = 2;
/// Lowest HS gear that requires `PA_TxHsAdaptType` to be set before the gear change (MIPI UniPro
/// v2.0 section 5.8.3, HS-G4 and above).
pub const HS_GEAR_ADAPT_MIN: u32 = 4;

// --- UPIU, Query, Descriptor, and Device Manager constants (UFS 3.1/4.0 section 10 - 14) ---

/// Mask of the 6-bit transaction code in UPIU byte 0; bits 7:6 are the HD and DD flags (UFS
/// 3.1/4.0 section 10.6.2).
pub const TRANSACTION_CODE_MASK: u8 = 0x3F;
/// COMMAND UPIU header flag `W`: the command has a Data-Out phase (UFS 3.1/4.0 section 10.7.1).
pub const FLAG_WRITE: u8 = 0x20;
/// COMMAND UPIU header flag `R`: the command has a Data-In phase (UFS 3.1/4.0 section 10.7.1).
pub const FLAG_READ: u8 = 0x40;
/// RESPONSE UPIU header flag `O`: residual overflow (UFS 3.1/4.0 section 10.7.2).
pub const FLAG_OVERFLOW: u8 = 0x40;
/// RESPONSE UPIU header flag `U`: residual underflow (UFS 3.1/4.0 section 10.7.2).
pub const FLAG_UNDERFLOW: u8 = 0x20;
/// Size of the Query UPIU header plus transaction-specific fields, excluding the data segment
/// (UFS 3.1/4.0 section 10.7.8).
pub const QUERY_HEADER_SIZE: usize = 32;
/// Size of the Query UPIU data segment, which must hold the largest descriptor (UFS 3.1/4.0
/// section 10.7.8).
pub const QUERY_DATA_SEGMENT_SIZE: usize = 256;
/// Largest descriptor length a client may request through `ReadDescriptor`; bounded by the
/// Query UPIU data segment (UFS 3.1/4.0 section 14.1).
pub const MAX_DESCRIPTOR_LENGTH: u16 = 256;
/// Size of the `bLength` + `bDescriptorIDN` header that starts every descriptor (UFS 3.1/4.0
/// section 14.1); a returned descriptor shorter than this cannot be validated.
pub const DESCRIPTOR_HEADER_SIZE: usize = 2;

/// Unit Descriptor `bLUEnable` value for an enabled LU (UFS 3.1/4.0 section 14.1.4.5).
pub const LU_ENABLED: u8 = 1;
/// Unit Descriptor `bProvisioningType` for thin provisioning with `TPRZ = 0` (`02h`); `03h`
/// sets `TPRZ = 1`, so the TRIM capability is derived from bit 1 (UFS 3.1/4.0 section 14.1.4.5).
pub const PROVISIONING_TYPE_THIN: u8 = 0x02;
/// Unit Descriptor `bProvisioningType` for thin provisioning that reads unmapped blocks as zero
/// (`TPRZ = 1`, UFS 3.1/4.0 section 14.1.4.5).
pub const PROVISIONING_TYPE_THIN_READ_ZERO: u8 = 0x03;
/// Unit Descriptor `bLUWriteProtect`: permanently write protected (UFS 3.1/4.0 section 14.1.4.5).
pub const LU_WRITE_PROTECT_PERMANENT: u8 = 0x02;
/// PCI vendor ID of Intel host controllers, which need the 19.2 MHz reference clock and the
/// link-timing workarounds in `Quirks`.
pub const INTEL_VENDOR_ID: u16 = 0x8086;
/// PCI vendor ID used by QEMU's emulated UFS controller.
pub const REDHAT_VENDOR_ID: u16 = 0x1B36;
/// PCI device ID of QEMU's emulated UFS controller, which skips UniPro and power management.
pub const QEMU_UFS_DEVICE_ID: u16 = 0x0013;
/// Time allowed for the device to clear `fDeviceInit` after it is set (UFS 3.1/4.0 section
/// 14.2).
pub const DEVICE_INIT_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Interval between `fDeviceInit` reads while waiting for the device to clear it.
pub const DEVICE_INIT_POLL_INTERVAL: zx::MonotonicDuration =
    zx::MonotonicDuration::from_micros(500);
/// Time allowed for `HCS.UPMCRS` to report the outcome of a `PA_PWRMode` change (UFSHCI 3.0/4.0
/// sections 5.3.1, 5.3.2, and 5.3.3).
pub const POWER_MODE_CHANGE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);
/// Settle time after a power mode change on Intel hosts before the link is probed to clear
/// pending DME errors.
pub const INTEL_POST_POWER_MODE_DELAY: zx::MonotonicDuration =
    zx::MonotonicDuration::from_micros(1250);
/// Extra `PA_Granularity` units added to the host `PA_TActivate` when programming the peer on
/// Intel hosts, giving the device more time to wake the host's receiver.
pub const INTEL_PEER_T_ACTIVATE_INCREMENT: u32 = 2;
/// `bActiveICCLevel` for the highest current budget, which allows full performance (UFS 3.1/4.0
/// section 14.3).
pub const HIGHEST_ACTIVE_ICC_LEVEL: u32 = 0x0F;
/// Smallest logical block size exposed to the block stack (UFS 3.1/4.0 section 14.1.4.5
/// requires `bLogicalBlockSize >= 12`).
pub const MIN_LOGICAL_BLOCK_SIZE: u32 = 4096;
/// Largest single transfer the driver issues, matching [`MAX_PRDT_ENTRIES`] 4 KiB pages.
pub const MAX_TRANSFER_BYTES: u64 = 1024 * 1024;
/// Unit of Geometry Descriptor `bMinAddrBlockSize`, `bMaxInBufferSize` and `bMaxOutBufferSize`
/// (UFS 3.1/4.0 section 14.1.4.4).
pub const GEOMETRY_SIZE_UNIT: u32 = 512;
/// Unit of Geometry Descriptor `dSegmentSize` (UFS 3.1/4.0 section 14.1.4.4).
pub const SEGMENT_SIZE_UNIT: u64 = 512;
/// `dExtendedUFSFeaturesSupport` bit for WriteBooster (UFS 3.1/4.0 section 14.1.4.2).
pub const EXTENDED_FEATURE_WRITE_BOOSTER: u32 = 1 << 8;
/// `bWBBufferLifeTimeEst` value meaning the buffer has exceeded its lifetime and must not be
/// used (UFS 3.1/4.0 section 14.3).
pub const WRITE_BOOSTER_LIFETIME_EXCEEDED: u32 = 0x0B;
/// Geometry `bMaxNumberLU` value indicating up to 8 logical units (UFS 3.1/4.0 section 14.1.4.4).
pub const MAX_NUMBER_LU_8: u8 = 0;
/// Geometry `bMaxNumberLU` value indicating up to 32 logical units (UFS 3.1/4.0 section 14.1.4.4).
pub const MAX_NUMBER_LU_32: u8 = 1;
/// Number of logical units scanned when `bMaxNumberLU == 0` (UFS 3.1/4.0 section 14.1.4.4).
pub const LUN_SCAN_COUNT_8: u8 = 8;
/// Number of logical units scanned when `bMaxNumberLU == 1` (UFS 3.1/4.0 section 14.1.4.4).
pub const LUN_SCAN_COUNT_32: u8 = 32;
/// `PowerCondition` Inspect value recorded when the UFS device is in the active power condition
/// (UFS 3.1/4.0 section 11.3.10, `START STOP UNIT` `POWER CONDITION = 1h`).
pub const POWER_CONDITION_ACTIVE: u64 = 1;
/// `LinkState` Inspect value recorded when the UniPro link is in the active state.
pub const LINK_STATE_ACTIVE: u64 = 1;

// --- SCSI command set, sense, and RPMB constants (SPC-4 / SBC-3 / UFS 3.1/4.0 sec 11 - 12) ---

/// Mask of the sense data `RESPONSE CODE` in byte 0; bit 7 is `VALID` (SPC-4 section 4.5).
pub const SENSE_RESPONSE_CODE_MASK: u8 = 0x7F;
/// Sense `RESPONSE CODE` for descriptor-format current errors, whose sense key is in byte 1
/// rather than byte 2 (SPC-4 section 4.5.2).
pub const SENSE_DESCRIPTOR_FORMAT_CURRENT: u8 = 0x72;
/// Sense `RESPONSE CODE` for descriptor-format deferred errors (SPC-4 section 4.5.2).
pub const SENSE_DESCRIPTOR_FORMAT_DEFERRED: u8 = 0x73;
/// SCSI LUN prefix of the well-known logical unit addressing method (SAM-5 section 4.7.8).
pub const SCSI_WELL_KNOWN_LUN_ID: u16 = 0xC100;
/// Mask selecting the addressing method byte of a 16-bit SCSI LUN (SAM-5 section 4.7.8).
pub const SCSI_WELL_KNOWN_LUN_MASK: u16 = 0xFF00;
/// Bit set in the UPIU LUN field to address a well-known LU (UFS 3.1/4.0 section 10.8.5).
pub const UFS_WELL_KNOWN_LUN_ID: u8 = 0x80;
/// Highest regular LUN a UFS device can expose (UFS 3.1/4.0 section 10.8.5 allows 32 LUs).
pub const MAX_LUN_INDEX: u16 = 31;
/// `SERVICE ACTION IN (16)` service action selecting `READ CAPACITY (16)` (SBC-3 section 5.16).
pub const SERVICE_ACTION_READ_CAPACITY_16: u8 = 0x10;
/// `FUA` bit in byte 1 of `READ`/`WRITE (10)/(16)` CDBs (SBC-3 sections 5.11 and 5.33).
pub const CDB_FUA_BIT: u8 = 0x08;
/// `SECURITY PROTOCOL` value for UFS RPMB in `SECURITY PROTOCOL IN/OUT` (UFS 3.1/4.0 section
/// 12.2).
pub const UFS_SECURITY_PROTOCOL: u8 = 0xEC;
/// `SECURITY PROTOCOL SPECIFIC` value selecting RPMB region 0 (UFS 3.1/4.0 section 12.2).
pub const RPMB_SECURITY_PROTOCOL_SPECIFIC: u16 = 0x0001;

/// UPIU LUN of the RPMB well-known LU (UFS 3.1/4.0 section 10.8.5).
pub const RPMB_LUN: u8 = 0xC4;
/// Size of one RPMB data frame (UFS 3.1/4.0 section 12.2).
pub const RPMB_FRAME_SIZE: u64 = 512;
/// Largest `fuchsia.hardware.rpmb/Request` buffer accepted: 16 frames.
pub const MAX_RPMB_TRANSFER_SIZE: u64 = 8192;
/// Upper bound on frames sent per `SECURITY PROTOCOL OUT` command before reading the Geometry
/// Descriptor (`bRPMB_ReadWriteSize`). Kept at 2 frames (1 KiB) matching the C++ driver (`rpmb.h`)
/// because `trusty_storage_proxy` concatenates up to 2 `Authenticated Data Write` frames
/// (`reliable_write_size`) and 1 `Result Read Request` frame (`write_size`) into a single 3-frame
/// `tx_frames` buffer, which UFS 3.1/4.0 section 12.4.4.1 requires to be sent as two separate
/// `SECURITY PROTOCOL OUT` commands.
pub const MAX_FRAMES_PER_TRANSFER: u64 = 2;
/// `rpmb_size` reported when the RPMB Unit Descriptor is unreadable: 4 x 128 KiB = 512 KiB.
pub const DEFAULT_RPMB_SIZE_UNITS: u8 = 4;
/// Unit of the `rpmb_size` reported in `fuchsia.hardware.rpmb/DeviceInfo` (128 KiB, matching
/// eMMC `RPMB_SIZE_MULT`).
pub const RPMB_SIZE_UNIT: u64 = 128 * 1024;
/// RPMB logical block size assumed when the Unit Descriptor `bLogicalBlockSize` is out of range:
/// `1 << 8` bytes (UFS 3.1/4.0 section 14.1.4.6).
pub const RPMB_DEFAULT_BLOCK_SIZE: u64 = 256;
/// Smallest valid RPMB Unit Descriptor `bLogicalBlockSize` exponent (`1 << 8` = 256 B, UFS 3.1/4.0
/// section 14.1.4.6).
pub const MIN_RPMB_LOGICAL_BLOCK_SIZE_SHIFT: u8 = 8;
/// Largest valid RPMB Unit Descriptor `bLogicalBlockSize` exponent (`1 << 16` = 64 KiB, UFS 3.1/4.0
/// section 14.1.4.6).
pub const MAX_RPMB_LOGICAL_BLOCK_SIZE_SHIFT: u8 = 16;
/// Largest `reliable_write_sector_count` advertised in `fuchsia.hardware.rpmb/DeviceInfo`, bounded
/// by [`MAX_RPMB_TRANSFER_SIZE`] (16 frames of [`RPMB_FRAME_SIZE`] bytes).
pub const MAX_RPMB_RELIABLE_WRITE_SECTORS: u8 = (MAX_RPMB_TRANSFER_SIZE / RPMB_FRAME_SIZE) as u8;
/// Size of the VMO through which RPMB frames are staged for DMA.
pub const STAGING_VMO_SIZE: u64 = MAX_RPMB_TRANSFER_SIZE;
/// Stack buffer size used when copying frames between client and staging VMOs.
pub const COPY_CHUNK_SIZE: usize = 1024;
