// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Target-side FDomain UART driver daemon for Fuchsia devices.

pub mod error;
pub mod receiver;
pub mod sender;
pub mod serial;

pub use error::Result;

/// Runs the target-side FDomain UART driver daemon.
pub async fn run_driver() -> Result<()> {
    Ok(())
}
