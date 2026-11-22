// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(not(test), no_std)]
// TODO(https://fxbug.dev/581987928): Remove once compiler options can control atomic coverage
// counter updates.
#![feature(coverage_attribute)]

pub mod arm64;
pub mod riscv64;
pub mod x86;

//
// The conceptually similar routines and types defined for all supported
// architectures.
//

#[cfg(target_arch = "x86_64")]
use x86 as per_arch;

#[cfg(target_arch = "aarch64")]
use arm64 as per_arch;

#[cfg(target_arch = "riscv64")]
use riscv64 as per_arch;

pub use per_arch::{
    EarlyTicks, cycles, device_memory_barrier, serialize_instructions, store_memory_barrier,
    thread_memory_barrier, r#yield, yield_processor,
};
