// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use thiserror::Error;

/// Errors encountered while parsing or building NTB-16 blocks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Error)]
pub enum NtbError {
    #[error("NTB buffer too short")]
    BufferTooShort,
    #[error("invalid NTH16 signature: {0:#x}")]
    InvalidNthSignature(u32),
    #[error("invalid NTH16 header length: {0}")]
    InvalidNthHeaderLength(u16),
    #[error("NTB block length {block_len} out of bounds for buffer length {buf_len}")]
    BlockLengthOutOfBounds { block_len: usize, buf_len: usize },
    #[error("NDP16 offset {ndp_offset} out of bounds for block length {block_len}")]
    NdpOffsetOutOfBounds { ndp_offset: usize, block_len: usize },
    #[error("invalid NDP16 signature: {0:#x}")]
    InvalidNdpSignature(u32),
    #[error("invalid NDP16 length: {0}")]
    InvalidNdpLength(u16),
    #[error("NDP16 chain exceeds maximum length")]
    NdpChainTooLong,
    #[error("NDP16 missing terminating null entry")]
    MissingNdpNullTerminator,
    #[error(
        "datagram ({dg_offset}..{}) out of bounds for block length {block_len}",
        dg_offset.saturating_add(*dg_len)
    )]
    DatagramOutOfBounds { dg_offset: usize, dg_len: usize, block_len: usize },
    #[error(
        "cumulative NDP16 and datagram bytes ({accounted_bytes}) exceed block length {block_len}"
    )]
    TotalPayloadExceedsBlock { accounted_bytes: usize, block_len: usize },
    #[error("no frames provided for NTB")]
    EmptyInput,
    #[error("buffer too small for NTB")]
    BufferTooSmall,
    #[error("arithmetic overflow while processing NTB")]
    ArithmeticOverflow,
}
