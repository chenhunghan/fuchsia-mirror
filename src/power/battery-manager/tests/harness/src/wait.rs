// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers that wait for battery-manager's `BatteryInfo` updates and Inspect data.

use anyhow::{Result, anyhow};
use diagnostics_assertions::TreeAssertion;
use diagnostics_reader::ArchiveReader;
use fidl_fuchsia_power_battery as fpower;
use fuchsia_async as fasync;
use futures::StreamExt as _;

const INSPECT_POLL_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);
/// Polls between logs of the latest Inspect mismatch, so that a hung test shows why.
const INSPECT_POLLS_PER_LOG: usize = 50;

/// Waits for the first valid (non-`NotAvailable`) `BatteryInfo` update on `watcher_stream`.
pub async fn wait_for_battery_info(
    mut watcher_stream: fpower::BatteryInfoWatcherRequestStream,
) -> Result<(fpower::BatteryInfo, fpower::BatteryInfoWatcherRequestStream)> {
    let (info, _) = wait_for_matching_battery_info(&mut watcher_stream, |info| {
        info.level_percent.is_some() && info.status != Some(fpower::BatteryStatus::NotAvailable)
    })
    .await?;
    Ok((info, watcher_stream))
}

/// Waits on `watcher_stream` until `predicate` matches an incoming `BatteryInfo` update, and
/// returns it with its wake lease.
pub async fn wait_for_matching_battery_info(
    watcher_stream: &mut fpower::BatteryInfoWatcherRequestStream,
    mut predicate: impl FnMut(&fpower::BatteryInfo) -> bool,
) -> Result<(fpower::BatteryInfo, Option<zx::EventPair>)> {
    while let Some(Ok(fpower::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
        info,
        wake_lease,
        responder,
    })) = watcher_stream.next().await
    {
        responder.send()?;
        if predicate(&info) {
            return Ok((info, wake_lease));
        }
    }
    Err(anyhow!("Watcher stream ended before predicate matched"))
}

/// Polls the Inspect hierarchy of the component at `moniker` until it matches `assertion`.
///
/// There is no timeout: if it never matches, the test hangs until the test runner times it out,
/// with the latest mismatch in the logs.
pub async fn wait_for_inspect_match(moniker: &str, assertion: &TreeAssertion) -> Result<()> {
    let mut reader = ArchiveReader::inspect();
    reader.select_all_for_component(moniker.to_string()).with_minimum_schema_count(1);
    let mut polls = 0;
    loop {
        let payload = reader.snapshot().await?.into_iter().next().and_then(|data| data.payload);
        let mismatch = match payload {
            Some(payload) => match assertion.run(&payload) {
                Ok(()) => return Ok(()),
                Err(e) => format!("{e:?}; last observed: {payload:#?}"),
            },
            None => "no Inspect data".to_string(),
        };
        polls += 1;
        if polls % INSPECT_POLLS_PER_LOG == 0 {
            log::info!("Inspect for {moniker} does not match yet: {mismatch}");
        }
        // Inspect has no change notification, so poll.
        let next_poll = fasync::MonotonicInstant::after(INSPECT_POLL_INTERVAL);
        fasync::Timer::new(next_poll).await; // allow-test-timer
    }
}
