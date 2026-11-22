// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use flex_fuchsia_memory_heapdump_client::{
    SnapshotElement, SnapshotReceiverProxy, SnapshotReceiverProxyInterface,
};
use std::collections::VecDeque;

use crate::Error;

// Maximum number of batches that can be sent without waiting for their acknowledgment.
const MAX_IN_FLIGHT_BATCHES: usize = 16;

type BatchResponseFut = <SnapshotReceiverProxy as SnapshotReceiverProxyInterface>::BatchResponseFut;

/// Wraps a SnapshotReceiver channel so that batches can be sent without waiting for the previous
/// ones to be acknowledged, up to MAX_IN_FLIGHT_BATCHES at the same time.
pub struct SnapshotReceiverPipelinedProxy<'a> {
    dest: &'a mut SnapshotReceiverProxy,
    in_flight: VecDeque<BatchResponseFut>,
}

impl<'a> SnapshotReceiverPipelinedProxy<'a> {
    /// Prepares to send batches over the given channel.
    pub fn new(dest: &'a mut SnapshotReceiverProxy) -> SnapshotReceiverPipelinedProxy<'a> {
        SnapshotReceiverPipelinedProxy {
            dest,
            in_flight: VecDeque::with_capacity(MAX_IN_FLIGHT_BATCHES),
        }
    }

    /// Sends the given `batch`, without waiting for it to be acknowledged.
    pub async fn batch(&mut self, batch: &[SnapshotElement]) -> Result<(), Error> {
        // Wait for the oldest batch to be acknowledged if too many are already in flight.
        if self.in_flight.len() >= MAX_IN_FLIGHT_BATCHES
            && let Some(fut) = self.in_flight.pop_front()
        {
            fut.await?;
        }

        // Send the new batch.
        let fut = self.dest.batch(batch);
        self.in_flight.push_back(fut);
        Ok(())
    }

    /// Waits until all the batches that have been sent are acknowledged.
    pub async fn finish(self) -> Result<(), Error> {
        for fut in self.in_flight {
            fut.await?;
        }
        Ok(())
    }
}
