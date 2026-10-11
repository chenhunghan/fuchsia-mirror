// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::packet::PacketType;
use crate::serial::SerialConnection;
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc;
use log::{error, warn};

/// Maximum number of packets buffered in [`SendQueue`].
///
/// `fuchsia.hardware.bluetooth` recommends at most 10 pending `Send` calls per protocol; since
/// `HciTransport` and `ScoConnection` share the outbound UART queue, 20 accommodates both while
/// bounding memory if the serial bus stalls.
pub const SEND_QUEUE_LIMIT: usize = 20;

/// Bounded asynchronous queue that prepends the 1-byte H4 packet indicator and writes packets
/// to [`SerialConnection`].
#[derive(Debug)]
pub struct SendQueue {
    sender: mpsc::Sender<(PacketType, Vec<u8>)>,
}

impl SendQueue {
    pub fn new(serial: SerialConnection, scope: &fasync::Scope) -> Self {
        let (sender, receiver) = mpsc::channel::<(PacketType, Vec<u8>)>(SEND_QUEUE_LIMIT);

        scope.spawn(Self::write_task(receiver, serial));

        Self { sender }
    }

    async fn write_task(
        mut receiver: mpsc::Receiver<(PacketType, Vec<u8>)>,
        serial: SerialConnection,
    ) {
        while let Some((packet_type, payload)) = receiver.next().await {
            let mut data = Vec::with_capacity(payload.len() + 1);
            data.push(u8::from(packet_type));
            data.extend_from_slice(&payload);

            if let Err(status) = serial.write(&data).await {
                error!("Serial write failed with status {status}. Dropping packet.");
            }
        }
    }

    /// Queues an HCI packet (`payload` without the leading H4 indicator byte) for sending over
    /// serial.
    ///
    /// Returns `Err(zx::Status::NO_MEMORY)` if the queue is full or `Err(zx::Status::PEER_CLOSED)`
    /// if the queue receiver has been dropped.
    pub fn queue_packet(
        &mut self,
        packet_type: PacketType,
        payload: Vec<u8>,
    ) -> Result<(), zx::Status> {
        self.sender.try_send((packet_type, payload)).map_err(|send_error| {
            if send_error.is_full() {
                warn!("Send queue is full! Dropping packet.");
                zx::Status::NO_MEMORY
            } else {
                warn!("Send queue receiver dropped! Cannot send packet.");
                zx::Status::PEER_CLOSED
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::fake_serial::FakeSerialState;
    use crate::tests::setup_test_harness;
    use fuchsia_sync::Mutex;
    use std::sync::Arc;

    #[fuchsia::test]
    async fn test_send_queue_dispatches_packets_with_indicators() {
        let (write_sender, mut write_receiver) = mpsc::unbounded();
        let state = Arc::new(Mutex::new(FakeSerialState {
            write_sender: Some(write_sender),
            ..Default::default()
        }));
        let mut harness = setup_test_harness(state);
        let started_driver = harness.start_driver().await.expect("driver should start");
        let driver = started_driver.get_driver().expect("driver instance");

        let mut queue = SendQueue::new(driver.serial().clone(), driver.scope());

        assert_eq!(queue.queue_packet(PacketType::COMMAND, vec![0x03, 0x0c, 0x00]), Ok(()));
        assert_eq!(
            queue.queue_packet(PacketType::ACL_DATA, vec![0x01, 0x00, 0x01, 0x00, 0xff]),
            Ok(())
        );
        assert_eq!(queue.queue_packet(PacketType::SYNC_DATA, vec![0x02, 0x00, 0x01, 0xaa]), Ok(()));
        assert_eq!(
            queue.queue_packet(PacketType::ISO_DATA, vec![0x03, 0x00, 0x01, 0x00, 0xbb]),
            Ok(())
        );

        assert_eq!(
            write_receiver.next().await,
            Some(vec![u8::from(PacketType::COMMAND), 0x03, 0x0c, 0x00])
        );
        assert_eq!(
            write_receiver.next().await,
            Some(vec![u8::from(PacketType::ACL_DATA), 0x01, 0x00, 0x01, 0x00, 0xff])
        );
        assert_eq!(
            write_receiver.next().await,
            Some(vec![u8::from(PacketType::SYNC_DATA), 0x02, 0x00, 0x01, 0xaa])
        );
        assert_eq!(
            write_receiver.next().await,
            Some(vec![u8::from(PacketType::ISO_DATA), 0x03, 0x00, 0x01, 0x00, 0xbb])
        );

        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_send_queue_continues_after_write_error() {
        let (write_sender, mut write_receiver) = mpsc::unbounded();
        let state = Arc::new(Mutex::new(FakeSerialState {
            write_sender: Some(write_sender),
            ..Default::default()
        }));
        state.lock().write_results.push_back(Err(zx::Status::IO));

        let mut harness = setup_test_harness(state);
        let started_driver = harness.start_driver().await.expect("driver should start");
        let driver = started_driver.get_driver().expect("driver instance");

        let mut queue = SendQueue::new(driver.serial().clone(), driver.scope());

        // First packet write will fail with ZX_ERR_IO and be dropped.
        assert_eq!(queue.queue_packet(PacketType::COMMAND, vec![0x01]), Ok(()));
        // Second packet write should still be processed and succeed.
        assert_eq!(queue.queue_packet(PacketType::ACL_DATA, vec![0x02]), Ok(()));

        assert_eq!(write_receiver.next().await, Some(vec![u8::from(PacketType::ACL_DATA), 0x02]));

        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_send_queue_full_returns_no_memory() {
        let state = Arc::new(Mutex::new(FakeSerialState::default()));
        let mut harness = setup_test_harness(state);
        let started_driver = harness.start_driver().await.expect("driver should start");
        let driver = started_driver.get_driver().expect("driver instance");

        let scope = fasync::Scope::new();
        let mut queue = SendQueue::new(driver.serial().clone(), &scope);

        // futures::channel::mpsc::channel(SEND_QUEUE_LIMIT) has capacity SEND_QUEUE_LIMIT + 1
        // (one extra slot for the single sender).
        for _ in 0..=SEND_QUEUE_LIMIT {
            assert_eq!(queue.queue_packet(PacketType::COMMAND, vec![0x01]), Ok(()));
        }

        assert_eq!(queue.queue_packet(PacketType::COMMAND, vec![0x02]), Err(zx::Status::NO_MEMORY));

        scope.cancel().await;
        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_send_queue_closed_returns_peer_closed() {
        let state = Arc::new(Mutex::new(FakeSerialState::default()));
        let mut harness = setup_test_harness(state);
        let started_driver = harness.start_driver().await.expect("driver should start");
        let driver = started_driver.get_driver().expect("driver instance");

        let scope = fasync::Scope::new();
        let mut queue = SendQueue::new(driver.serial().clone(), &scope);

        scope.cancel().await;

        assert_eq!(
            queue.queue_packet(PacketType::COMMAND, vec![0x01]),
            Err(zx::Status::PEER_CLOSED)
        );

        started_driver.stop_driver().await;
    }
}
