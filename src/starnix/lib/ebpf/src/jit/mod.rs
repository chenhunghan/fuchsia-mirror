// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{EbpfInstruction, HelperLinkage, JitError};

#[cfg(target_os = "fuchsia")]
mod fuchsia;

#[cfg(target_os = "fuchsia")]
pub use fuchsia::*;

/// Whether JIT compilation is supported on the current target architecture.
pub const IS_SUPPORTED: bool = false;

/// Entry point signature for JIT-compiled eBPF programs.
pub type JitFn =
    unsafe extern "C" fn(run_context: *mut (), r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64;

/// Compiles relocated eBPF instructions into target architecture machine code.
pub fn compile(
    code: &[EbpfInstruction],
    helpers: &[HelperLinkage],
    packet_load_fn_addr: Option<u64>,
) -> Result<Vec<u8>, JitError> {
    let _ = (code, helpers, packet_load_fn_addr);
    Err(JitError::UnsupportedArchitecture)
}
