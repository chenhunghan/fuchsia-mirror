// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure-logic core for the driver-lab driver: target-ceiling and
//! session-allowlist policy evaluation and the bounded audit ring.
//!
//! This crate deliberately has no Fuchsia dependencies so policy and audit
//! behavior are testable on the build host without a component realm or an
//! emulator.

pub mod access_policy;
pub mod audit_ring;
pub mod config;
pub mod digest;
pub mod executor;
pub mod hardware_backend;
pub mod interrupt;
pub mod protocol_resource_adapter;
pub mod provider;
pub mod session;
pub mod state_bank;
pub mod target_policy;
