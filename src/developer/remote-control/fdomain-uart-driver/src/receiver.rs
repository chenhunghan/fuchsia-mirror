// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Target-side ResendSP frame receiver task and session validation.

use crate::error::{Result, TargetDriverError};
use crate::serial::{SerialRead, SerialReader};
use futures::channel::mpsc;
use log::{info, warn};
use uart_fpl::{
    AckTracker, CONTROL_CHANNEL_ID, FrameStatus, FrameType, HandshakeResponse, HandshakeStatus,
    ProtocolId, ResendReceiver, encode_frame,
};

/// In-order serial channel events delivered from [`receiver_task`] to the coordinator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerialEvent {
    /// Incoming payload bytes for a multiplexed FDomain channel.
    SerialData { channel_id: u16, data: Vec<u8> },
    /// Remote close notification for a multiplexed FDomain channel.
    SerialClose { channel_id: u16 },
}

const NEGOTIATE_RESP_SEQ: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCheck {
    Valid,
    Discard,
}

fn validate_frame_session(
    frame: &uart_fpl::Frame,
    current_session_id: u32,
) -> Result<SessionCheck> {
    if frame.session_id == 0 {
        warn!("Received {:?} frame with invalid session_id 0; discarding", frame.frame_type);
        return Ok(SessionCheck::Discard);
    }
    if frame.session_id == current_session_id {
        if frame.frame_type == FrameType::Reset
            || (frame.frame_type == FrameType::Close && frame.channel_id == CONTROL_CHANNEL_ID)
        {
            info!(
                "Received {:?} frame on control channel from host. Restarting bridge...",
                frame.frame_type
            );
            return Err(TargetDriverError::ResetRequested);
        }
        return Ok(SessionCheck::Valid);
    }
    if frame.frame_type == FrameType::NegotiateReq && frame.channel_id == CONTROL_CHANNEL_ID {
        info!("Session ID changed from {} to {}, restarting", current_session_id, frame.session_id);
        return Err(TargetDriverError::SessionIdChanged);
    }
    warn!(
        "Received frame with mismatched session ID (got {}, expected {}) type {:?} channel {}; discarding",
        frame.session_id, current_session_id, frame.frame_type, frame.channel_id
    );
    Ok(SessionCheck::Discard)
}

fn handle_duplicate_negotiate_req(rx_session_id: u32, data_tx: &mut mpsc::Sender<Vec<u8>>) {
    info!(
        "Received duplicate NegotiateReq for active session {}, re-transmitting NegotiateResp",
        rx_session_id
    );
    let resp = HandshakeResponse {
        status: HandshakeStatus::Success,
        selected: Some(ProtocolId::ResendSP),
    };
    if let Ok(serialized) = resp.serialize()
        && let Ok(resp_frame) = encode_frame(
            rx_session_id,
            CONTROL_CHANNEL_ID,
            NEGOTIATE_RESP_SEQ,
            FrameType::NegotiateResp,
            &serialized,
        )
        && let Err(e) = data_tx.try_send(resp_frame)
    {
        warn!("Failed to send duplicate NegotiateResp: {:?}", e);
    }
}

/// Validates the Go-Back-N sequence number of a `DATA` or `CLOSE` frame, forwards
/// in-order events to the coordinator without blocking, and updates `ack_tracker`.
fn handle_sequenced_serial_event(
    receiver: &mut ResendReceiver,
    rx_session_id: u32,
    seq: u8,
    event: SerialEvent,
    serial_tx: &mut mpsc::Sender<SerialEvent>,
    ack_tracker: &AckTracker,
) -> Result<()> {
    // Inspect `seq` before advancing `receiver` so we only commit sequence progress
    // if `serial_tx` has capacity to accept the event.
    match receiver.inspect(seq) {
        FrameStatus::InOrder { .. } => match serial_tx.try_send(event) {
            Ok(()) => ack_tracker.set_ack(rx_session_id, receiver.advance_in_order(seq)?),
            Err(e) if e.is_full() => {
                // Downstream coordinator queue is full: drop the frame without advancing
                // `receiver` and re-assert the last cumulative ACK. This applies Go-Back-N
                // backpressure (the host will retransmit `seq`) while keeping `receiver_task`
                // non-blocking so it can still process incoming `Ack` and `Reset` frames.
                if let Some(ack_seq) = receiver.current_ack_seq() {
                    ack_tracker.set_ack(rx_session_id, ack_seq);
                }
            }
            Err(e) => {
                let msg = format!("Failed to send event to coordinator: {e:?}");
                return Err(TargetDriverError::ChannelSend(msg));
            }
        },
        FrameStatus::OutOfOrder { ack_seq, .. } => {
            // Discard out-of-order or duplicate frames and re-emit the last contiguous
            // cumulative ACK so the host knows which sequence number the target expects.
            if let Some(ack_seq) = ack_seq {
                ack_tracker.set_ack(rx_session_id, ack_seq);
            }
        }
    }
    Ok(())
}

fn handle_ack_frame(seq: u8, ack_tx: &mut mpsc::Sender<u8>) -> Result<()> {
    if let Err(e) = ack_tx.try_send(seq) {
        if e.is_full() {
            warn!("ack_tx full, dropping ACK event seq={}", seq);
        } else {
            return Err(TargetDriverError::ChannelSend(format!(
                "Failed to enqueue ACK event: {:?}",
                e
            )));
        }
    }
    Ok(())
}

fn dispatch_receiver_frame(
    frame: uart_fpl::Frame,
    receiver: &mut ResendReceiver,
    serial_tx: &mut mpsc::Sender<SerialEvent>,
    ack_tracker: &AckTracker,
    ack_tx: &mut mpsc::Sender<u8>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let (session_id, channel_id, seq, payload) =
        (frame.session_id, frame.channel_id, frame.seq, frame.payload);
    match frame.frame_type {
        FrameType::NegotiateReq if channel_id == CONTROL_CHANNEL_ID => {
            handle_duplicate_negotiate_req(session_id, data_tx);
        }
        FrameType::NegotiateReq => {
            warn!(
                "Received unexpected NegotiateReq on non-control channel {}; discarding",
                channel_id
            );
        }
        FrameType::Data if channel_id == CONTROL_CHANNEL_ID => {
            warn!("Received unexpected DATA frame on CONTROL_CHANNEL_ID; discarding");
        }
        FrameType::Data => {
            let event = SerialEvent::SerialData { channel_id, data: payload };
            handle_sequenced_serial_event(
                receiver,
                session_id,
                seq,
                event,
                serial_tx,
                ack_tracker,
            )?;
        }
        FrameType::Ack => handle_ack_frame(seq, ack_tx)?,
        FrameType::Close => {
            let event = SerialEvent::SerialClose { channel_id };
            handle_sequenced_serial_event(
                receiver,
                session_id,
                seq,
                event,
                serial_tx,
                ack_tracker,
            )?;
        }
        other => warn!("Unknown frame type: {:?}", other),
    }
    Ok(())
}

/// Reads frames from `reader`, validates `session_id`, tracks ResendSP sequence
/// numbers via `ack_tracker`, and dispatches in-order [`SerialEvent`]s and ACKs.
///
/// # Errors
///
/// Returns [`TargetDriverError::ResetRequested`] if the host sends a reset frame,
/// [`TargetDriverError::SessionIdChanged`] if a new handshake arrives with a
/// different session ID, or propagates serial read and channel send errors.
pub async fn receiver_task<R: SerialRead>(
    reader: &mut SerialReader<R>,
    mut serial_tx: mpsc::Sender<SerialEvent>,
    ack_tracker: AckTracker,
    mut ack_tx: mpsc::Sender<u8>,
    session_id: u32,
    mut data_tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut receiver = ResendReceiver::new();
    loop {
        // `reader.next_frame().await` resolves synchronously without yielding to the
        // single-threaded executor whenever multiple frames are already buffered in
        // `FrameParser` from a single bulk `Device.Read`. Yield at the top of every
        // iteration (including after `SessionCheck::Discard`) so `coordinator_task`,
        // `sender_task`, and `writer_task` can make progress.
        fuchsia_async::yield_now().await;
        let frame = reader.next_frame().await?;
        match validate_frame_session(&frame, session_id) {
            Ok(SessionCheck::Valid) => {}
            Ok(SessionCheck::Discard) => continue,
            Err(TargetDriverError::SessionIdChanged) => {
                // `next_frame()` already consumed the new session's `NegotiateReq` from `reader`.
                // Put it back at the front of `reader` so `run_device_sessions` can pass it into
                // the next `negotiate_session` call via `take_unconsumed()`, avoiding a host
                // handshake timeout.
                reader.requeue_frame(&frame);
                return Err(TargetDriverError::SessionIdChanged);
            }
            Err(e) => return Err(e),
        }
        dispatch_receiver_frame(
            frame,
            &mut receiver,
            &mut serial_tx,
            &ack_tracker,
            &mut ack_tx,
            &mut data_tx,
        )?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::MockSerial;
    use futures::prelude::*;
    use uart_fpl::{FrameParser, HostHandshake};

    struct MockSerialPending {
        read_data: Vec<Vec<u8>>,
    }

    impl SerialRead for MockSerialPending {
        async fn serial_read(&mut self) -> Result<Vec<u8>> {
            if self.read_data.is_empty() {
                futures::future::pending().await
            } else {
                Ok(self.read_data.remove(0))
            }
        }
    }

    struct TestReceiverHarness {
        ack_tracker: AckTracker,
        serial_rx: mpsc::Receiver<SerialEvent>,
        data_rx: mpsc::Receiver<Vec<u8>>,
        _ack_rx: mpsc::Receiver<u8>,
        task: fuchsia_async::Task<Result<()>>,
    }

    fn spawn_test_receiver<R: SerialRead + 'static>(
        client: R,
        session_id: u32,
    ) -> TestReceiverHarness {
        let mut reader = SerialReader::new(client);
        let ack_tracker = AckTracker::new();
        let (ack_tx, _ack_rx) = mpsc::channel(10);
        let (serial_tx, serial_rx) = mpsc::channel(10);
        let (data_tx, data_rx) = mpsc::channel(10);
        let ack_clone = ack_tracker.clone();
        let task = fuchsia_async::Task::local(async move {
            receiver_task(&mut reader, serial_tx, ack_clone, ack_tx, session_id, data_tx).await
        });
        TestReceiverHarness { ack_tracker, serial_rx, data_rx, _ack_rx, task }
    }

    async fn run_receiver_once(frames: Vec<Vec<u8>>, session_id: u32) -> (Result<()>, Vec<u8>) {
        let mock_serial = MockSerial { read_data: frames, write_data: Vec::new() };
        let mut reader = SerialReader::new(mock_serial);
        let ack_tracker = AckTracker::new();
        let (ack_tx, _ack_rx) = mpsc::channel(10);
        let (serial_tx, _serial_rx) = mpsc::channel(10);
        let (data_tx, _data_rx) = mpsc::channel(10);
        let res =
            receiver_task(&mut reader, serial_tx, ack_tracker, ack_tx, session_id, data_tx).await;
        (res, reader.take_unconsumed())
    }

    #[fuchsia::test]
    async fn test_receiver_task_happy_path() {
        let payload = b"hello rcs";
        let (session_id, channel_id) = (12345u32, 42u16);
        let data_frame = encode_frame(session_id, channel_id, 0, FrameType::Data, payload).unwrap();
        let mock_serial = MockSerial { read_data: vec![data_frame], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, session_id);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(event, SerialEvent::SerialData { channel_id, data: payload.to_vec() });
        assert_eq!(harness.ack_tracker.take_ack(), Some((session_id, 0)));
    }

    #[fuchsia::test]
    async fn test_receiver_task_discards_zero_session_id() {
        let (valid_sid, channel_id) = (12345u32, 42u16);
        let zero_frame = encode_frame(0, channel_id, 0, FrameType::Data, b"bad").unwrap();
        let valid_frame = encode_frame(valid_sid, channel_id, 0, FrameType::Data, b"ok").unwrap();
        let mock_serial =
            MockSerial { read_data: vec![zero_frame, valid_frame], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, valid_sid);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(event, SerialEvent::SerialData { channel_id, data: b"ok".to_vec() });
    }

    #[fuchsia::test]
    async fn test_receiver_task_out_of_order_reacks() {
        let (session_id, channel_id) = (12345u32, 42u16);
        let frame_0 = encode_frame(session_id, channel_id, 0, FrameType::Data, b"first").unwrap();
        let frame_2 = encode_frame(session_id, channel_id, 2, FrameType::Data, b"third").unwrap();
        let frame_1 = encode_frame(session_id, channel_id, 1, FrameType::Data, b"second").unwrap();
        let mock_serial =
            MockSerial { read_data: vec![frame_0, frame_2, frame_1], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, session_id);
        let ev0 = harness.serial_rx.next().await.unwrap();
        assert_eq!(ev0, SerialEvent::SerialData { channel_id, data: b"first".to_vec() });
        let ev1 = harness.serial_rx.next().await.unwrap();
        assert_eq!(ev1, SerialEvent::SerialData { channel_id, data: b"second".to_vec() });
        assert_eq!(harness.ack_tracker.take_ack(), Some((session_id, 1)));
    }

    #[fuchsia::test]
    async fn test_receiver_task_session_change_discard() {
        let (session_id_1, session_id_2, channel_id) = (12345u32, 67890u32, 42u16);
        let frame_1 =
            encode_frame(session_id_2, channel_id, 0, FrameType::Data, b"discard me").unwrap();
        let frame_2 =
            encode_frame(session_id_1, channel_id, 0, FrameType::Data, b"accept me").unwrap();
        let mock_serial = MockSerial { read_data: vec![frame_1, frame_2], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, session_id_1);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(event, SerialEvent::SerialData { channel_id, data: b"accept me".to_vec() });
    }

    #[fuchsia::test]
    async fn test_receiver_task_session_change_negotiate_reboot() {
        let (session_id_1, session_id_2) = (12345u32, 67890u32);
        let frame_1 =
            encode_frame(session_id_2, CONTROL_CHANNEL_ID, 0, FrameType::NegotiateReq, &[1, 1])
                .unwrap();
        let (result, unconsumed) = run_receiver_once(vec![frame_1.clone()], session_id_1).await;
        assert!(matches!(result, Err(TargetDriverError::SessionIdChanged)));
        assert_eq!(unconsumed, frame_1);
    }

    #[fuchsia::test]
    async fn test_session_id_resiliency_adversarial() {
        let (sid_current, sid_forged) = (100u32, 200u32);
        let frame_bad = encode_frame(sid_forged, 42, 0, FrameType::Data, b"discard").unwrap();
        let frame_ok = encode_frame(sid_current, 42, 0, FrameType::Data, b"accept").unwrap();
        let mock_serial =
            MockSerial { read_data: vec![frame_bad, frame_ok], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, sid_current);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(event, SerialEvent::SerialData { channel_id: 42, data: b"accept".to_vec() });
    }

    #[fuchsia::test]
    async fn test_crc_noise_collision_resiliency() {
        let (sid_current, sid_noise) = (0xABCDEFu32, 0x99999999u32);
        let noise_frame = encode_frame(sid_noise, 1, 0, FrameType::Data, b"noisebytes").unwrap();
        let valid_frame = encode_frame(sid_current, 1, 0, FrameType::Data, b"validbytes").unwrap();
        let mock_serial = MockSerialPending { read_data: vec![noise_frame, valid_frame] };
        let mut harness = spawn_test_receiver(mock_serial, sid_current);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(event, SerialEvent::SerialData { channel_id: 1, data: b"validbytes".to_vec() });
        assert!(futures::poll!(&mut harness.task).is_pending());
    }

    #[fuchsia::test]
    async fn test_receiver_task_duplicate_negotiate_req_retransmits_resp() {
        let session_id = 99999u32;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let dup_req =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let mock_serial = MockSerial { read_data: vec![dup_req], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, session_id);
        let resp_bytes = harness.data_rx.next().await.expect("Expected NegotiateResp");
        let mut parser = FrameParser::new();
        parser.feed(&resp_bytes);
        let resp_frame = parser.next_frame().expect("Should parse NegotiateResp frame");
        assert_eq!(resp_frame.session_id, session_id);
        assert_eq!(resp_frame.channel_id, CONTROL_CHANNEL_ID);
        assert_eq!(resp_frame.frame_type, FrameType::NegotiateResp);
        let resp = HandshakeResponse::try_from(resp_frame.payload.as_slice()).unwrap();
        assert_eq!(resp.status, HandshakeStatus::Success);
        assert_eq!(resp.selected, Some(ProtocolId::ResendSP));
    }

    #[fuchsia::test]
    async fn test_receiver_task_handles_reset_frame() {
        let session_id = 12345u32;
        let zero_reset = encode_frame(0, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        let stale_reset =
            encode_frame(99999, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        let valid_reset =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        let (result, _) =
            run_receiver_once(vec![zero_reset, stale_reset, valid_reset], session_id).await;
        assert!(matches!(result, Err(TargetDriverError::ResetRequested)));
    }

    #[fuchsia::test]
    async fn test_receiver_task_discards_control_channel_data() {
        let session_id = 12345u32;
        let bad_frame = encode_frame(
            session_id,
            CONTROL_CHANNEL_ID,
            0,
            FrameType::Data,
            b"spurious control data",
        )
        .unwrap();
        let good_frame =
            encode_frame(session_id, 42, 0, FrameType::Data, b"valid user data").unwrap();
        let mock_serial =
            MockSerial { read_data: vec![bad_frame, good_frame], write_data: Vec::new() };
        let mut harness = spawn_test_receiver(mock_serial, session_id);
        let event = harness.serial_rx.next().await.unwrap();
        assert_eq!(
            event,
            SerialEvent::SerialData { channel_id: 42, data: b"valid user data".to_vec() }
        );
    }
}
