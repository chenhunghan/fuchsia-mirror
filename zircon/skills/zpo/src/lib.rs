// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! zpo, the Zircon Profile Optimizer.
//!
//! zpo measures a task on a Fuchsia device, explains why it is slow, and only
//! when the answer is "the scheduler" derives deadline-profile parameters for
//! the board's `.profiles` file, then verifies them on the device.

pub mod model;
pub mod proc;
pub mod tp;

/// The version reported by `zpo --version`.
pub const VERSION: &str = "0.1.0";
