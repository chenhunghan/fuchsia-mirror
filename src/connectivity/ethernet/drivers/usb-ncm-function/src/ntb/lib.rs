// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

//! 16-bit USB CDC-NCM Transfer Block (NTB-16) buffer framing (CDC NCM 1.0 Chapter 3).
//!
//! Provides zero-allocation in-place single-pass NTB-16 encoding ([`Ntb16Builder`]) and
//! chained-`NDP16` datagram slice iteration ([`parse_ntb16`]).

#[cfg(test)]
extern crate std;

mod builder;
mod error;
mod parser;
mod wire;

pub use crate::builder::{Ntb16BuildResult, Ntb16Builder, NtbSequence};
pub use crate::error::NtbError;
pub use crate::parser::{MAX_NDP_CHAIN_STEPS, Ntb16Iter, parse_ntb16};
pub use crate::wire::{
    DPE16_LENGTH, NDP16_HEADER_LENGTH, NDP16_MIN_LENGTH, NDP16_SIGNATURE_NO_CRC, NTB_ALIGNMENT,
    NTH16_HEADER_LENGTH, NTH16_SIGNATURE, NcmDpe16, NcmNdp16Header, NcmNth16,
};
