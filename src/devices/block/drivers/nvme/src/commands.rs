// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! NVMe submission/completion queue entries, Admin commands, NVM I/O commands, and Identify data
//! structures (NVM Express Base Specification 2.0, sections 3.3.3 and 5; NVM Command Set
//! Specification 1.0c, sections 3.2 and 4.1.5).

use bitfield::bitfield;
use bitflags::bitflags;
use std::fmt;
use std::mem::{offset_of, size_of};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

// NVMe wire structures are little-endian on the wire; compiling only for little-endian targets
// allows direct native integer fields without byte-swapping wrappers.
const _: () = assert!(cfg!(target_endian = "little"));

/// Number of LBA Format descriptors in [`IdentifyNamespace`].
pub const NUM_LBA_FORMATS: usize = 64;

/// Number of NSID slots in [`IdentifyActiveNamespaces`].
const NUM_ACTIVE_NAMESPACE_IDS: usize = 1024;

/// Admin Command Set opcodes (NVM Express Base Specification 2.0c, section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AdminCommandOpcode {
    DeleteIoSubmissionQueue = 0x00,
    CreateIoSubmissionQueue = 0x01,
    DeleteIoCompletionQueue = 0x04,
    CreateIoCompletionQueue = 0x05,
    Identify = 0x06,
    SetFeatures = 0x09,
    GetFeatures = 0x0a,
}

/// NVM Command Set opcodes (NVM Command Set Specification 1.0c, section 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IoCommandOpcode {
    Flush = 0x00,
    Write = 0x01,
    Read = 0x02,
}

/// Feature Identifiers for `Set Features` and `Get Features` (NVMe Base Spec 2.0, section 5.27).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FeatureId {
    VolatileWriteCache = 0x06,
    NumberOfQueues = 0x07,
}

/// Controller or Namespace Structure (`CNS`) values for `Identify` (NVMe Base Spec 2.0,
/// section 5.17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IdentifyCns {
    IdentifyNamespace = 0x00,
    IdentifyController = 0x01,
    ActiveNamespaceList = 0x02,
}

/// Completion Status Code Type (`SCT`, NVMe Base Spec 2.0, section 3.3.3.2.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCodeType {
    Generic,
    CommandSpecific,
    IntegrityErrors,
    PathRelated,
    VendorSpecific,
    Reserved(u8),
}

impl From<u8> for StatusCodeType {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Generic,
            1 => Self::CommandSpecific,
            2 => Self::IntegrityErrors,
            3 => Self::PathRelated,
            7 => Self::VendorSpecific,
            other => Self::Reserved(other),
        }
    }
}

impl From<StatusCodeType> for u8 {
    fn from(value: StatusCodeType) -> Self {
        match value {
            StatusCodeType::Generic => 0,
            StatusCodeType::CommandSpecific => 1,
            StatusCodeType::IntegrityErrors => 2,
            StatusCodeType::PathRelated => 3,
            StatusCodeType::VendorSpecific => 7,
            StatusCodeType::Reserved(other) => other,
        }
    }
}

/// Generic Command Status codes (`SC` when `SCT == Generic`, NVMe Base Spec 2.0, Figure 99).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GenericStatus {
    Success = 0x00,
    InvalidOpcode = 0x01,
    InvalidField = 0x02,
    InvalidNamespaceOrFormat = 0x0b,
}

bitfield! {
    /// Command Dword 0 bitfields in [`Submission`].
    #[repr(transparent)]
    #[derive(
        Clone,
        Copy,
        Default,
        PartialEq,
        Eq,
        FromBytes,
        IntoBytes,
        Immutable,
        KnownLayout,
    )]
    pub struct CommandDword0(pub u32);
    impl Debug;
    pub u16, cid, set_cid: 31, 16;
    pub u8, data_transfer_mode, set_data_transfer_mode: 15, 14;
    pub u8, fused, set_fused: 9, 8;
    pub u8, opcode, set_opcode: 7, 0;
}

impl CommandDword0 {
    /// Creates a [`CommandDword0`] with `OPC` initialized to `opcode`.
    pub fn with_opcode(opcode: u8) -> Self {
        let mut dw0 = Self(0);
        dw0.set_opcode(opcode);
        dw0
    }
}

bitfield! {
    /// Dword 2 bitfields in [`Completion`].
    #[repr(transparent)]
    #[derive(
        Clone,
        Copy,
        Default,
        PartialEq,
        Eq,
        FromBytes,
        IntoBytes,
        Immutable,
        KnownLayout,
    )]
    pub struct CompletionDword2(pub u32);
    impl Debug;
    pub u16, sq_id, set_sq_id: 31, 16;
    pub u16, sq_head, set_sq_head: 15, 0;
}

bitfield! {
    /// Dword 3 (Status Field + Phase Tag + CID) bitfields in [`Completion`].
    #[repr(transparent)]
    #[derive(
        Clone,
        Copy,
        Default,
        PartialEq,
        Eq,
        FromBytes,
        IntoBytes,
        Immutable,
        KnownLayout,
    )]
    pub struct CompletionDword3(pub u32);
    impl Debug;
    pub u8, from into StatusCodeType, status_code_type, set_status_code_type: 27, 25;
    pub u8, status_code, set_status_code: 24, 17;
    pub phase, set_phase: 16;
    pub u16, command_id, set_command_id: 15, 0;
}

/// 64-byte NVMe Common Command Format Submission Queue Entry
/// (NVM Express Base Specification 2.0, section 3.3.3.1).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Submission {
    pub cdw0: CommandDword0,
    pub namespace_id: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub metadata_pointer: u64,
    pub data_pointer: [u64; 2],
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

const _: () = assert!(size_of::<Submission>() == 64);

impl Submission {
    /// Creates a zeroed [`Submission`] with `CDW0.OPC` set to `opcode`.
    pub fn new(opcode: u8) -> Self {
        Self { cdw0: CommandDword0::with_opcode(opcode), ..Default::default() }
    }

    /// Builds an `Identify Controller` (`CNS = 0x01`) admin command.
    pub fn identify_controller() -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::Identify as u8),
            cdw10: u32::from(IdentifyCns::IdentifyController as u8),
            ..Default::default()
        }
    }

    /// Builds an `Identify Active Namespace ID List` (`CNS = 0x02`) admin command.
    pub fn identify_active_namespaces() -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::Identify as u8),
            cdw10: u32::from(IdentifyCns::ActiveNamespaceList as u8),
            ..Default::default()
        }
    }

    /// Builds an `Identify Namespace` (`CNS = 0x00`) admin command for `namespace_id`.
    pub fn identify_namespace(namespace_id: u32) -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::Identify as u8),
            namespace_id,
            cdw10: u32::from(IdentifyCns::IdentifyNamespace as u8),
            ..Default::default()
        }
    }

    /// Builds a `Get Features` admin command for `Volatile Write Cache` (`FID = 0x06`).
    pub fn get_volatile_write_cache() -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::GetFeatures as u8),
            cdw10: u32::from(FeatureId::VolatileWriteCache as u8),
            ..Default::default()
        }
    }

    /// Builds a `Set Features` admin command for `Number of Queues` (`FID = 0x07`).
    ///
    /// Both `num_submission_queues` and `num_completion_queues` must be non-zero (encoded 0-based).
    pub fn set_io_queue_count(num_submission_queues: u16, num_completion_queues: u16) -> Self {
        assert!(num_submission_queues > 0 && num_completion_queues > 0);
        let cdw11 =
            (u32::from(num_completion_queues - 1) << 16) | u32::from(num_submission_queues - 1);
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::SetFeatures as u8),
            cdw10: u32::from(FeatureId::NumberOfQueues as u8),
            cdw11,
            ..Default::default()
        }
    }

    /// Builds a `Create I/O Completion Queue` (`0x05`) admin command for a physically contiguous CQ
    /// with interrupts enabled on `interrupt_vector`.
    ///
    /// Per NVMe Base Specification 2.0 sections 3.3.1 and 5.4, `entry_count` must be in
    /// `2..=65536` (encoded 0-based in `CDW10[31:16]`).
    pub fn create_io_completion_queue(
        queue_id: u16,
        entry_count: u32,
        device_addr: u64,
        interrupt_vector: u16,
    ) -> Self {
        assert!(
            (2..=65536).contains(&entry_count),
            "I/O completion queue entry_count {entry_count} must be in 2..=65536"
        );

        // Bit 0 = PC (Physically Contiguous = 1), Bit 1 = IEN (Interrupts Enabled = 1),
        // Bits 31:16 = IV (Interrupt Vector).
        let cdw11 = (u32::from(interrupt_vector) << 16) | 0b11;
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::CreateIoCompletionQueue as u8),
            data_pointer: [device_addr, 0],
            cdw10: ((entry_count - 1) << 16) | u32::from(queue_id),
            cdw11,
            ..Default::default()
        }
    }

    /// Builds a `Create I/O Submission Queue` (`0x01`) admin command for a physically contiguous SQ
    /// paired with `completion_queue_id`.
    ///
    /// Per NVMe Base Specification 2.0 sections 3.3.1 and 5.5, `entry_count` must be in
    /// `2..=65536` (encoded 0-based in `CDW10[31:16]`).
    pub fn create_io_submission_queue(
        queue_id: u16,
        entry_count: u32,
        device_addr: u64,
        completion_queue_id: u16,
    ) -> Self {
        assert!(
            (2..=65536).contains(&entry_count),
            "I/O submission queue entry_count {entry_count} must be in 2..=65536"
        );

        // Bit 0 = PC (Physically Contiguous = 1), Bits 31:16 = CQID.
        let cdw11 = (u32::from(completion_queue_id) << 16) | 0b01;
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::CreateIoSubmissionQueue as u8),
            data_pointer: [device_addr, 0],
            cdw10: ((entry_count - 1) << 16) | u32::from(queue_id),
            cdw11,
            ..Default::default()
        }
    }

    /// Builds a `Delete I/O Completion Queue` (`0x04`) admin command.
    pub fn delete_io_completion_queue(queue_id: u16) -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::DeleteIoCompletionQueue as u8),
            cdw10: u32::from(queue_id),
            ..Default::default()
        }
    }

    /// Builds a `Delete I/O Submission Queue` (`0x00`) admin command.
    pub fn delete_io_submission_queue(queue_id: u16) -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(AdminCommandOpcode::DeleteIoSubmissionQueue as u8),
            cdw10: u32::from(queue_id),
            ..Default::default()
        }
    }

    /// Builds an NVM `Read` (`0x02`) command for `block_count` blocks (`1..=65536`) starting at
    /// `start_lba`.
    pub fn nvm_read(namespace_id: u32, start_lba: u64, block_count: u32) -> Self {
        assert!((1..=65536).contains(&block_count));
        Self {
            cdw0: CommandDword0::with_opcode(IoCommandOpcode::Read as u8),
            namespace_id,
            cdw10: start_lba as u32,
            cdw11: (start_lba >> 32) as u32,
            cdw12: (block_count - 1) & 0xffff,
            ..Default::default()
        }
    }

    /// Builds an NVM `Write` (`0x01`) command for `block_count` blocks (`1..=65536`) starting at
    /// `start_lba`, setting Force Unit Access (`CDW12` bit 30) when `force_unit_access` is true.
    pub fn nvm_write(
        namespace_id: u32,
        start_lba: u64,
        block_count: u32,
        force_unit_access: bool,
    ) -> Self {
        assert!((1..=65536).contains(&block_count));
        let fua_bit = if force_unit_access { 1u32 << 30 } else { 0 };
        Self {
            cdw0: CommandDword0::with_opcode(IoCommandOpcode::Write as u8),
            namespace_id,
            cdw10: start_lba as u32,
            cdw11: (start_lba >> 32) as u32,
            cdw12: ((block_count - 1) & 0xffff) | fua_bit,
            ..Default::default()
        }
    }

    /// Builds an NVM `Flush` (`0x00`) command for `namespace_id`.
    pub fn nvm_flush(namespace_id: u32) -> Self {
        Self {
            cdw0: CommandDword0::with_opcode(IoCommandOpcode::Flush as u8),
            namespace_id,
            ..Default::default()
        }
    }
}

/// 16-byte NVMe Common Completion Queue Entry
/// (NVM Express Base Specification 2.0, section 3.3.3.2).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Completion {
    pub command: [u32; 2],
    pub dw2: CompletionDword2,
    pub dw3: CompletionDword3,
}

const _: () = assert!(size_of::<Completion>() == 16);

impl Completion {
    /// Constructs a [`Completion`] from four raw little-endian 32-bit dwords (`DW0..=DW3`).
    pub const fn from_dwords(dwords: [u32; 4]) -> Self {
        Self {
            command: [dwords[0], dwords[1]],
            dw2: CompletionDword2(dwords[2]),
            dw3: CompletionDword3(dwords[3]),
        }
    }

    /// Extracts the Phase Tag (`DW3` bit 16) from a raw `DW3` value.
    pub fn phase_from_dword3(dw3: u32) -> bool {
        CompletionDword3(dw3).phase()
    }

    /// Returns `true` if `SCT == Generic` and `SC == Success` (`0`).
    pub fn is_success(&self) -> bool {
        self.dw3.status_code_type() == StatusCodeType::Generic
            && self.dw3.status_code() == GenericStatus::Success as u8
    }

    /// Decodes `Get Features - Volatile Write Cache` (`FID = 0x06`) completion bit `DW0[0]`.
    pub fn volatile_write_cache_enabled(&self) -> bool {
        (self.command[0] & 1) != 0
    }

    /// Decodes 1-based allocated I/O submission queue count (`(DW0 & 0xffff) + 1`) from
    /// `Set Features - Number of Queues` (`FID = 0x07`).
    pub fn num_submission_queues(&self) -> u32 {
        (self.command[0] & 0xffff) + 1
    }

    /// Decodes 1-based allocated I/O completion queue count (`(DW0 >> 16) + 1`) from
    /// `Set Features - Number of Queues` (`FID = 0x07`).
    pub fn num_completion_queues(&self) -> u32 {
        (self.command[0] >> 16) + 1
    }
}

bitflags! {
    /// Optional Admin Command Support (`OACS`, NVMe Base Spec 2.0, Figure 275, bytes 257:256).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Oacs: u16 {
        const SECURITY = 1 << 0;
        const FORMAT_NVM = 1 << 1;
        const FIRMWARE = 1 << 2;
        const NAMESPACE_MANAGEMENT = 1 << 3;
        const DEVICE_SELF_TEST = 1 << 4;
        const DIRECTIVES = 1 << 5;
        const NVME_MI = 1 << 6;
        const VIRTUALIZATION_MANAGEMENT = 1 << 7;
        const DOORBELL_BUFFER_CONFIG = 1 << 8;
        const GET_LBA_STATUS = 1 << 9;
        const COMMAND_AND_FEATURE_LOCKDOWN = 1 << 10;
    }

    /// Optional NVM Command Support (`ONCS`, NVMe Base Spec 2.0, Figure 275, bytes 521:520).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Oncs: u16 {
        const COMPARE = 1 << 0;
        const WRITE_UNCORRECTABLE = 1 << 1;
        const DATASET_MANAGEMENT = 1 << 2;
        const WRITE_ZEROES = 1 << 3;
        const SAVE_SELECT_NONZERO = 1 << 4;
        const RESERVATIONS = 1 << 5;
        const TIMESTAMP = 1 << 6;
        const VERIFY = 1 << 7;
        const COPY = 1 << 8;
    }
}

/// 4096-byte `Identify Controller` data structure
/// (NVM Express Base Specification 2.0, section 5.17.2.1, Figure 275).
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct IdentifyController {
    pub pci_vid: u16,
    pub pci_did: u16,
    pub serial_number: [u8; 20],
    pub model_number: [u8; 40],
    pub firmware_rev: [u8; 8],
    pub recommended_arbitration_burst: u8,
    pub oui: [u8; 3],
    pub cmic: u8,
    pub max_data_transfer: u8,
    pub controller_id: u16,
    pub version: u32,
    pub rtd3_resume_latency: u32,
    pub rtd3_entry_latency: u32,
    pub oaes: u32,
    pub ctratt: u32,
    pub rrls: u16,
    pub reserved0: [u8; 9],
    pub controller_type: u8,
    pub fru_guid: [u8; 16],
    pub crdt1: u16,
    pub crdt2: u16,
    pub crdt3: u16,
    pub reserved1: [u8; 119],
    pub nvmsr: u8,
    pub vwci: u8,
    pub mec: u8,

    // Offset 0x100
    pub oacs: u16,
    pub acl: u8,
    pub aerl: u8,
    pub frmw: u8,
    pub lpa: u8,
    pub elpe: u8,
    pub npss: u8,
    pub avscc: u8,
    pub apsta: u8,
    pub wctemp: u16,
    pub cctemp: u16,
    pub mtfa: u16,
    pub hmpre: u32,
    pub hmmin: u32,
    pub tnvmcap: [u64; 2],
    pub unvmcap: [u64; 2],
    pub rpmb_support: u32,
    pub edstt: u16,
    pub dsto: u8,
    pub fwug: u8,
    pub kas: u16,
    pub hctma: u16,
    pub mntmt: u16,
    pub mxtmt: u16,
    pub sanicap: u32,
    pub hmminds: u32,
    pub hmmaxd: u16,
    pub nsetid_max: u16,
    pub endgid_max: u16,
    pub ana_tt: u8,
    pub ana_cap: u8,
    pub ana_grp_max: u32,
    pub n_ana_grp_id: u32,
    pub pels: u32,
    pub domain_id: u16,
    pub reserved2: [u8; 10],
    pub max_egcap: [u64; 2],
    pub reserved3: [u8; 128],

    // Offset 0x200
    pub sqes: u8,
    pub cqes: u8,
    pub max_cmd: u16,
    pub num_namespaces: u32,
    pub oncs: u16,
    pub fuses: u16,
    pub fna: u8,
    pub vwc: u8,
    pub atomic_write_unit_normal: u16,
    pub atomic_write_unit_power_fail: u16,
    pub icsvscc: u8,
    pub nwpc: u8,
    pub acwu: u16,
    pub copy_formats_supported: u16,
    pub sgl_support: u32,
    pub max_allowed_namespaces: u32,
    pub max_dna: [u64; 2],
    pub max_cna: u32,
    pub reserved4: [u8; 204],

    // Offset 0x300
    pub nvme_qualified_name: [u8; 256],

    // Offset 0x400..0x700
    pub reserved5: [u8; 768],

    // Offset 0x700
    pub io_cc_size: u32,
    pub io_rc_size: u32,
    pub icdoff: u16,
    pub fcatt: u8,
    pub msdbd: u8,
    pub ofcs: u16,
    pub reserved6: [u8; 242],

    // Offset 0x800
    pub power_states: [[u32; 8]; 32],

    // Offset 0xc00..0x1000
    pub vendor_data: [u8; 1024],
}

const _: () = assert!(offset_of!(IdentifyController, oacs) == 0x100);
const _: () = assert!(offset_of!(IdentifyController, sqes) == 0x200);
const _: () = assert!(offset_of!(IdentifyController, nvme_qualified_name) == 0x300);
const _: () = assert!(offset_of!(IdentifyController, io_cc_size) == 0x700);
const _: () = assert!(offset_of!(IdentifyController, power_states) == 0x800);
const _: () = assert!(offset_of!(IdentifyController, vendor_data) == 0xc00);
const _: () = assert!(size_of::<IdentifyController>() == 0x1000);

impl Default for IdentifyController {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl fmt::Debug for IdentifyController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentifyController")
            .field("pci_vid", &self.pci_vid)
            .field("pci_did", &self.pci_did)
            .field("controller_id", &self.controller_id)
            .field("version", &self.version)
            .field("max_data_transfer", &self.max_data_transfer)
            .field("num_namespaces", &self.num_namespaces)
            .field("oacs", &self.oacs_flags())
            .field("oncs", &self.oncs_flags())
            .field("vwc", &self.vwc)
            .finish_non_exhaustive()
    }
}

impl IdentifyController {
    /// Maximum Submission Queue Entry Size in bytes (`2^(SQES[7:4])`).
    pub fn maximum_sq_entry_size(&self) -> usize {
        1usize << (self.sqes >> 4)
    }

    /// Minimum Submission Queue Entry Size in bytes (`2^(SQES[3:0])`).
    pub fn minimum_sq_entry_size(&self) -> usize {
        1usize << (self.sqes & 0x0f)
    }

    /// Maximum Completion Queue Entry Size in bytes (`2^(CQES[7:4])`).
    pub fn maximum_cq_entry_size(&self) -> usize {
        1usize << (self.cqes >> 4)
    }

    /// Minimum Completion Queue Entry Size in bytes (`2^(CQES[3:0])`).
    pub fn minimum_cq_entry_size(&self) -> usize {
        1usize << (self.cqes & 0x0f)
    }

    /// Decodes Optional Admin Command Support (`OACS`) flags.
    pub fn oacs_flags(&self) -> Oacs {
        Oacs::from_bits_retain(self.oacs)
    }

    /// Decodes Optional NVM Command Support (`ONCS`) flags.
    pub fn oncs_flags(&self) -> Oncs {
        Oncs::from_bits_retain(self.oncs)
    }
}

/// 4096-byte `Active Namespace ID list` data structure
/// (NVM Express Base Specification 2.0, section 5.17.2.2).
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct IdentifyActiveNamespaces {
    pub nsid: [u32; NUM_ACTIVE_NAMESPACE_IDS],
}

const _: () = assert!(size_of::<IdentifyActiveNamespaces>() == 0x1000);

impl Default for IdentifyActiveNamespaces {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl fmt::Debug for IdentifyActiveNamespaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active_count = self.nsid.iter().take_while(|&&id| id != 0).count();
        f.debug_struct("IdentifyActiveNamespaces")
            .field("active_nsids", &&self.nsid[..active_count])
            .finish_non_exhaustive()
    }
}

bitfield! {
    /// 4-byte LBA Format Data Structure (`LBAF`, NVM Command Set Specification 1.0c, Figure 98).
    #[repr(transparent)]
    #[derive(
        Clone,
        Copy,
        Default,
        PartialEq,
        Eq,
        FromBytes,
        IntoBytes,
        Immutable,
        KnownLayout,
    )]
    pub struct LbaFormat(pub u32);
    impl Debug;
    pub u8, relative_performance, set_relative_performance: 25, 24;
    pub u8, lba_data_size_log2, set_lba_data_size_log2: 23, 16;
    pub u16, metadata_size_bytes, set_metadata_size_bytes: 15, 0;
}

const _: () = assert!(size_of::<LbaFormat>() == 4);

impl LbaFormat {
    /// Constructs an [`LbaFormat`] from `lba_data_size_log2` (`LBADS`), `metadata_size_bytes`
    /// (`MS`), and `relative_performance` (`RP`, `0..=3`).
    pub fn from_parts(
        lba_data_size_log2: u8,
        metadata_size_bytes: u16,
        relative_performance: u8,
    ) -> Self {
        let mut fmt = Self(0);
        fmt.set_lba_data_size_log2(lba_data_size_log2);
        fmt.set_metadata_size_bytes(metadata_size_bytes);
        fmt.set_relative_performance(relative_performance);
        fmt
    }

    /// Returns LBA Data Size in bytes (`2^LBADS`), or `None` if `LBADS < 9` (including unused
    /// `LBADS == 0` descriptor slots, as NVMe requires `LBADS >= 9` for 512B+) or `LBADS >= 32`.
    pub fn lba_data_size_bytes(&self) -> Option<u32> {
        let lbads = self.lba_data_size_log2();
        if !(9..32).contains(&lbads) {
            return None;
        }
        Some(1u32 << lbads)
    }
}

/// 4096-byte NVM Command Set `Identify Namespace` data structure
/// (NVM Command Set Specification 1.0c, section 4.1.5.1, Figure 97).
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct IdentifyNamespace {
    pub nsze: u64,
    pub ncap: u64,
    pub nuse: u64,
    pub nsfeat: u8,
    pub nlbaf: u8,
    pub flbas: u8,
    pub mc: u8,
    pub dpc: u8,
    pub dps: u8,
    pub nmic: u8,
    pub rescap: u8,
    pub fpi: u8,
    pub dlfeat: u8,
    pub nawun: u16,
    pub nawupf: u16,
    pub nacwu: u16,
    pub nabsn: u16,
    pub nabo: u16,
    pub nabspf: u16,
    pub noiob: u16,
    pub nvmcap: [u64; 2],
    pub npwg: u16,
    pub npwa: u16,
    pub npdg: u16,
    pub npda: u16,
    pub nows: u16,
    pub mssrl: u16,
    pub mcl: u32,
    pub msrc: u8,
    pub reserved0: [u8; 11],
    pub anagrpid: u32,
    pub reserved1: [u8; 3],
    pub nsattr: u8,
    pub nvmsetid: u16,
    pub endgid: u16,
    pub nguid: [u8; 16],
    pub eui64: [u8; 8],
    pub lba_formats: [LbaFormat; NUM_LBA_FORMATS],
    pub vendor_specific: [u8; 3712],
}

const _: () = assert!(offset_of!(IdentifyNamespace, nlbaf) == 25);
const _: () = assert!(offset_of!(IdentifyNamespace, nguid) == 104);
const _: () = assert!(offset_of!(IdentifyNamespace, eui64) == 120);
const _: () = assert!(offset_of!(IdentifyNamespace, lba_formats) == 128);
const _: () = assert!(offset_of!(IdentifyNamespace, vendor_specific) == 0x180);
const _: () = assert!(size_of::<IdentifyNamespace>() == 0x1000);

impl Default for IdentifyNamespace {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

impl fmt::Debug for IdentifyNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentifyNamespace")
            .field("nsze", &self.nsze)
            .field("ncap", &self.ncap)
            .field("nuse", &self.nuse)
            .field("nlbaf", &self.nlbaf)
            .field("flbas", &self.flbas)
            .field("active_lba_format", &self.active_lba_format())
            .finish_non_exhaustive()
    }
}

impl IdentifyNamespace {
    /// Returns the active LBA format index decoded from `FLBAS` and `NLBAF`.
    pub fn lba_format_index(&self) -> usize {
        let lo = self.flbas & 0x0f;
        if self.nlbaf < 16 {
            usize::from(lo)
        } else {
            let hi = (self.flbas >> 5) & 0x03;
            usize::from((hi << 4) | lo)
        }
    }

    /// Returns the active [`LbaFormat`] selected by [`Self::lba_format_index`].
    pub fn active_lba_format(&self) -> LbaFormat {
        self.lba_formats[self.lba_format_index()]
    }

    /// Returns `NSFEAT` bit 1 (`NSABP` - Namespace Atomic Boundary & Write Unit Parameters).
    pub fn ns_atomics(&self) -> bool {
        (self.nsfeat & (1 << 1)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn nvm_read_masks_max_block_count_in_cdw12() {
        let sub = Submission::nvm_read(1, 0x20, 65536);
        assert_eq!(sub.cdw0.opcode(), IoCommandOpcode::Read as u8);
        assert_eq!(sub.cdw12, 0xffff);
    }

    #[fuchsia::test]
    #[should_panic]
    fn nvm_read_rejects_zero_blocks() {
        let _ = Submission::nvm_read(1, 0, 0);
    }

    #[fuchsia::test]
    #[should_panic]
    fn nvm_read_rejects_excessive_blocks() {
        let _ = Submission::nvm_read(1, 0, 65537);
    }

    #[fuchsia::test]
    #[should_panic]
    fn set_io_queue_count_rejects_zero() {
        let _ = Submission::set_io_queue_count(0, 1);
    }

    #[fuchsia::test]
    fn create_io_queues_encode_min_and_max_entry_counts() {
        let min_cq = Submission::create_io_completion_queue(1, 2, 0x1000, 0);
        assert_eq!(min_cq.cdw10, (1 << 16) | 1);

        let max_cq = Submission::create_io_completion_queue(1, 65536, 0x1000, 3);
        assert_eq!(max_cq.cdw10, (0xffff << 16) | 1);
        assert_eq!(max_cq.cdw11, (3 << 16) | 0b11);

        let min_sq = Submission::create_io_submission_queue(2, 2, 0x2000, 1);
        assert_eq!(min_sq.cdw10, (1 << 16) | 2);

        let max_sq = Submission::create_io_submission_queue(2, 65536, 0x2000, 1);
        assert_eq!(max_sq.cdw10, (0xffff << 16) | 2);
        assert_eq!(max_sq.cdw11, (1 << 16) | 0b01);
    }

    #[fuchsia::test]
    #[should_panic(expected = "must be in 2..=65536")]
    fn create_io_completion_queue_rejects_one_entry() {
        let _ = Submission::create_io_completion_queue(1, 1, 0x1000, 0);
    }

    #[fuchsia::test]
    #[should_panic(expected = "must be in 2..=65536")]
    fn create_io_completion_queue_rejects_excessive_entries() {
        let _ = Submission::create_io_completion_queue(1, 65537, 0x1000, 0);
    }

    #[fuchsia::test]
    #[should_panic(expected = "must be in 2..=65536")]
    fn create_io_submission_queue_rejects_one_entry() {
        let _ = Submission::create_io_submission_queue(1, 1, 0x1000, 1);
    }

    #[fuchsia::test]
    #[should_panic(expected = "must be in 2..=65536")]
    fn create_io_submission_queue_rejects_excessive_entries() {
        let _ = Submission::create_io_submission_queue(1, 65537, 0x1000, 1);
    }

    #[fuchsia::test]
    fn lba_format_data_size_validates_bounds() {
        assert_eq!(LbaFormat::from_parts(0, 0, 0).lba_data_size_bytes(), None);
        assert_eq!(LbaFormat::from_parts(8, 0, 0).lba_data_size_bytes(), None);
        assert_eq!(LbaFormat::from_parts(9, 0, 0).lba_data_size_bytes(), Some(512));
        assert_eq!(LbaFormat::from_parts(12, 0, 0).lba_data_size_bytes(), Some(4096));
        assert_eq!(LbaFormat::from_parts(31, 0, 0).lba_data_size_bytes(), Some(1u32 << 31));
        assert_eq!(LbaFormat::from_parts(32, 0, 0).lba_data_size_bytes(), None);
    }

    #[fuchsia::test]
    fn identify_namespace_lba_format_indexing() {
        let mut ns = IdentifyNamespace { nsze: 2048, ..Default::default() };

        // NLBAF = 0 means 1 format (LBAF0); FLBAS[6:5] are reserved/ignored.
        ns.nlbaf = 0;
        ns.flbas = 0b11 << 5;
        ns.lba_formats[0] = LbaFormat::from_parts(9, 0, 2);
        assert_eq!(ns.lba_format_index(), 0);
        assert_eq!(ns.active_lba_format().lba_data_size_bytes(), Some(512));
        assert_eq!(ns.active_lba_format().metadata_size_bytes(), 0);
        assert_eq!(ns.active_lba_format().relative_performance(), 2);

        // NLBAF = 15 means 16 formats (LBAF0..=LBAF15); FLBAS[6:5] are still ignored.
        ns.nlbaf = 15;
        ns.flbas = (0b11 << 5) | 15;
        ns.lba_formats[15] = LbaFormat::from_parts(12, 0, 1);
        assert_eq!(ns.lba_format_index(), 15);
        assert_eq!(ns.active_lba_format().lba_data_size_bytes(), Some(4096));

        // NLBAF = 16 means 17 formats (LBAF0..=LBAF16); FLBAS[6:5] MUST be included so LBAF16 is
        // reachable when FLBAS[6:5] = 0b01 and FLBAS[3:0] = 0.
        ns.nlbaf = 16;
        ns.flbas = 0b01 << 5;
        ns.lba_formats[16] = LbaFormat::from_parts(13, 8, 0);
        assert_eq!(ns.lba_format_index(), 16);
        assert_eq!(ns.active_lba_format().lba_data_size_bytes(), Some(8192));
        assert_eq!(ns.active_lba_format().metadata_size_bytes(), 8);

        // With NLBAF > 16, bits 6:5 supply bits 5:4 of the 6-bit format index.
        ns.nlbaf = 32;
        ns.flbas = (0b01 << 5) | 3;
        ns.lba_formats[19] = LbaFormat::from_parts(12, 8, 0);
        assert_eq!(ns.lba_format_index(), 19);
        assert_eq!(ns.active_lba_format().lba_data_size_bytes(), Some(4096));
        assert_eq!(ns.active_lba_format().metadata_size_bytes(), 8);
    }
}
