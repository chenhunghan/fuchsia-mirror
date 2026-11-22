// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! zpo, the Zircon Profile Optimizer.
//!
//! zpo measures a task on a Fuchsia device, explains why it is slow, and only
//! when the answer is "the scheduler" derives deadline-profile parameters for
//! the board's `.profiles` file, then verifies them on the device.

pub mod agent;
pub mod analysis;
pub mod attribution;
pub mod checkout;
pub mod cpu_profile;
pub mod derive;
pub mod device;
pub mod extensions;
pub mod llm;
pub mod model;
pub mod proc;
pub mod profiles_file;
pub mod report;
pub mod report_html;
pub mod roles;
pub mod runner;
pub mod server;
pub mod stats;
pub mod summary;
pub mod tasks;
pub mod topology;
pub mod tp;
pub mod trace_context;
pub mod trace_db;
pub mod ui_daemon;
pub mod verdict;

/// The version reported by `zpo --version`.
pub const VERSION: &str = "0.1.0";
