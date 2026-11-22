// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared test fakes for `fuchsia.power.broker` FIDL protocols.
//!
//! This crate provides in-process servers for testing components that interact with Fuchsia Power
//! Broker without spinning up the full Power Broker component:
//!
//! - [`BasicElementRunner`]: serves `ElementRunner::SetLevel` and records level transitions.
//! - [`FakeLessor`] and [`FakeLeaseControl`]: serve `Lessor` and hanging-get `LeaseControl`
//!   channels.
//!
//! # Example
//!
//! ```no_run
//! use fidl_fuchsia_power_broker as fpb;
//! use power_broker_testing::FakeLessor;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let (lessor, proxy) = FakeLessor::spawn(fpb::LeaseStatus::Pending);
//! let lease_client = proxy.lease(1).await?.map_err(|err| format!("{err:?}"))?;
//! let lease = lessor.next_lease().await;
//!
//! let lease_proxy = lease_client.into_proxy();
//! assert_eq!(
//!     lease_proxy.watch_status(fpb::LeaseStatus::Unknown).await?,
//!     fpb::LeaseStatus::Pending,
//! );
//!
//! lease.set_status(fpb::LeaseStatus::Satisfied);
//! assert_eq!(
//!     lease_proxy.watch_status(fpb::LeaseStatus::Pending).await?,
//!     fpb::LeaseStatus::Satisfied,
//! );
//!
//! drop(lease_proxy);
//! lease.wait_until_closed().await;
//! # Ok(())
//! # }
//! ```

#![deny(clippy::await_holding_lock)]

mod lessor;
mod runner;

pub use lessor::{FakeLeaseControl, FakeLessor};
pub use runner::BasicElementRunner;
