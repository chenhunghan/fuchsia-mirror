// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// "NCMH" in little-endian (`NCM_NTH16` signature, CDC NCM 1.0 Table 3-1).
pub const NTH16_SIGNATURE: u32 = 0x484D_434E;

/// "NCM0" in little-endian (`NCM_NDP16` signature without CRC, CDC NCM 1.0 Table 3-3).
pub const NDP16_SIGNATURE_NO_CRC: u32 = 0x304D_434E;

/// Byte length of `NCM_NTH16` (CDC NCM 1.0 Table 3-1).
pub const NTH16_HEADER_LENGTH: u16 = 12;

/// Byte length of the fixed `NCM_NDP16` header before `DPE16` entries (CDC NCM 1.0 Table 3-3).
pub const NDP16_HEADER_LENGTH: usize = 8;

/// Minimum `NCM_NDP16` length: 8-byte header + at least two 4-byte `DPE16` entries (CDC NCM 1.0
/// Table 3-3).
pub const NDP16_MIN_LENGTH: u16 = 16;

/// Byte length of a single 16-bit Datagram Pointer Entry (`NCM_DPE16`).
pub const DPE16_LENGTH: usize = 4;

/// Byte alignment and divisor for TX datagrams and `NDP16` tables (`<= 3` pad bytes per frame).
///
/// CDC NCM 1.0 Table 6-3 requires power-of-two alignment `>= 4` so 32-bit `NDP16` fields stay
/// word-aligned; using 4 for the divisor too satisfies Windows `UsbNcm.sys` and matches Linux
/// `f_ncm`. RX (`parse_ntb16`) slices by `DPE16` offset/length and accepts any host byte alignment.
pub const NTB_ALIGNMENT: usize = 4;

/// 16-bit NCM Transfer Header (`NCM_NTH16`, CDC NCM 1.0 Table 3-1).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
)]
pub struct NcmNth16 {
    /// 32-bit signature ("NCMH").
    pub signature: U32,
    /// 16-bit header length in bytes (must be 12).
    pub header_length: U16,
    /// 16-bit sequence number, incremented per transmitted NTB.
    pub sequence: U16,
    /// 16-bit total NTB block length in bytes (0 means transfer length).
    pub block_length: U16,
    /// 16-bit byte offset to the first `NDP16` header.
    pub ndp_index: U16,
}

/// 16-bit NCM Datagram Pointer Header (`NCM_NDP16`, CDC NCM 1.0 Table 3-3).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
)]
pub struct NcmNdp16Header {
    /// 32-bit signature ("NCM0" without CRC).
    pub signature: U32,
    /// 16-bit total length of this `NDP16` table in bytes.
    pub length: U16,
    /// 16-bit byte offset to the next `NDP16` header, or 0 if this is the last table.
    pub next_ndp_index: U16,
}

/// 16-bit NCM Datagram Pointer Entry (`NCM_DPE16`, CDC NCM 1.0 Table 3-3).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
)]
pub struct NcmDpe16 {
    /// 16-bit byte offset of the datagram within the NTB (0 terminates the table).
    pub datagram_index: U16,
    /// 16-bit byte length of the datagram (0 terminates the table).
    pub datagram_length: U16,
}
