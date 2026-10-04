// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Target-side ResendSP Go-Back-N sender task and retransmission management.

use crate::error::{Result, TargetDriverError};
use futures::channel::mpsc;
use futures::prelude::*;
use std::pin::Pin;
use uart_fpl::{AckOutcome, DEFAULT_RETRANSMISSION_TIMEOUT, FrameType, ResendSender};

/// Outgoing channel messages queued by the coordinator for [`sender_task`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SenderMessage {
    /// Outgoing payload bytes for a multiplexed FDomain channel.
    Data { channel_id: u16, payload: Vec<u8> },
    /// Local close notification for a multiplexed FDomain channel.
    Close { channel_id: u16 },
}

/// Processes a cumulative ACK from `receiver_task` and updates the retransmission timer.
///
/// Clears `active_timer` if all in-flight frames are now acknowledged, or resets
/// the timer deadline if the sliding window advanced with frames still in flight.
fn handle_sender_ack(
    sender: &mut ResendSender,
    ack_event: Option<u8>,
    active_timer: &mut Option<Pin<Box<fuchsia_async::Timer>>>,
) -> Result<()> {
    let Some(ack_seq) = ack_event else {
        return Err(TargetDriverError::AckChannelClosed);
    };
    if let AckOutcome::Advanced { all_acked, .. } = sender.handle_ack(ack_seq) {
        if all_acked {
            *active_timer = None;
        } else {
            *active_timer =
                Some(Box::pin(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT)));
        }
    }
    Ok(())
}

/// Sends an encoded frame to `data_tx` while concurrently draining `ack_rx`.
///
/// Polling `ack_rx` alongside `data_tx.send(frame)` prevents `sender_task` from
/// ignoring incoming cumulative ACKs when `data_tx` is temporarily full.
async fn send_data_frame(
    frame: Vec<u8>,
    sender: &mut ResendSender,
    ack_rx: &mut mpsc::Receiver<u8>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    active_timer: &mut Option<Pin<Box<fuchsia_async::Timer>>>,
) -> Result<()> {
    let mut send_fut = std::pin::pin!(data_tx.send(frame).fuse());
    loop {
        futures::select_biased! {
            ack_event = ack_rx.next().fuse() => {
                handle_sender_ack(sender, ack_event, active_timer)?;
            }
            res = send_fut => {
                return res.map_err(|e| {
                    TargetDriverError::ChannelSend(format!(
                        "Failed to send frame to writer: {e:?}"
                    ))
                });
            }
        }
    }
}

/// Retransmits all unacknowledged frames in `[base, next_seq)` upon timer expiry
/// and rearms `active_timer`.
async fn retransmit_window(
    sender: &mut ResendSender,
    ack_rx: &mut mpsc::Receiver<u8>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    active_timer: &mut Option<Pin<Box<fuchsia_async::Timer>>>,
) -> Result<()> {
    let retransmit_frames = sender.handle_timeout()?;
    for frame in retransmit_frames {
        if sender.is_idle() {
            break;
        }
        send_data_frame(frame, sender, ack_rx, data_tx, active_timer).await?;
    }
    if !sender.is_idle() {
        *active_timer = Some(Box::pin(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT)));
    }
    Ok(())
}

/// Encodes and transmits `in_flight_msg` if the Go-Back-N sliding window has capacity.
///
/// If the window is full (`!sender.can_send()`), `in_flight_msg` remains staged
/// until a subsequent ACK frees a sequence slot. Arms `active_timer` when the
/// first unacknowledged frame enters the window.
async fn send_next_in_flight(
    sender: &mut ResendSender,
    session_id: u32,
    in_flight_msg: &mut Option<SenderMessage>,
    ack_rx: &mut mpsc::Receiver<u8>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    active_timer: &mut Option<Pin<Box<fuchsia_async::Timer>>>,
) -> Result<()> {
    if !sender.can_send() {
        return Ok(());
    }
    let Some(msg) = in_flight_msg.take() else {
        return Ok(());
    };
    let (frame_type, channel_id, payload) = match msg {
        SenderMessage::Data { channel_id, payload } => (FrameType::Data, channel_id, payload),
        SenderMessage::Close { channel_id } => (FrameType::Close, channel_id, Vec::new()),
    };
    let (_seq, frame) = sender.enqueue_frame(session_id, channel_id, frame_type, &payload)?;
    if sender.in_flight() == 1 {
        *active_timer = Some(Box::pin(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT)));
    }
    send_data_frame(frame, sender, ack_rx, data_tx, active_timer).await
}

/// Awaits `active_timer` if armed, or suspends indefinitely when no frames are in flight.
async fn wait_active_timer(active_timer: &mut Option<Pin<Box<fuchsia_async::Timer>>>) {
    if let Some(timer) = active_timer {
        timer.as_mut().await;
    } else {
        futures::future::pending::<()>().await;
    }
}

/// Polls `sender_rx` for the next coordinator message only when `enabled` is true,
/// suspending when a message is already staged in `in_flight_msg` or `sender_rx` has closed.
async fn next_sender_msg(
    sender_rx: &mut mpsc::Receiver<SenderMessage>,
    enabled: bool,
) -> Option<SenderMessage> {
    if enabled { sender_rx.next().await } else { futures::future::pending().await }
}

/// Runs the Go-Back-N sender loop, encoding [`SenderMessage`]s with `session_id`,
/// tracking unacknowledged frames via [`ResendSender`], and retransmitting on timeout.
///
/// # Errors
///
/// Returns [`TargetDriverError::AckChannelClosed`] if `ack_rx` closes,
/// [`TargetDriverError::RetransmissionLimit`] if retransmission attempts are
/// exhausted, or [`TargetDriverError::ChannelSend`] if sending to `data_tx` fails.
pub async fn sender_task(
    mut sender_rx: mpsc::Receiver<SenderMessage>,
    mut data_tx: mpsc::Sender<Vec<u8>>,
    mut ack_rx: mpsc::Receiver<u8>,
    session_id: u32,
) -> Result<()> {
    let mut sender = ResendSender::default();
    let mut active_timer: Option<Pin<Box<fuchsia_async::Timer>>> = None;
    let mut in_flight_msg: Option<SenderMessage> = None;
    let mut sender_rx_closed = false;
    loop {
        // Stop pulling from `sender_rx` while the Go-Back-N window is full or a
        // message is already staged, propagating backpressure to the coordinator.
        let can_recv = sender.can_send() && in_flight_msg.is_none() && !sender_rx_closed;
        // Prioritize incoming ACKs ahead of timeout retransmissions and new messages
        // so an ACK that arrived just as the timer fired advances the window first.
        futures::select_biased! {
            ack_event = ack_rx.next().fuse() => {
                handle_sender_ack(&mut sender, ack_event, &mut active_timer)?;
            }
            _ = wait_active_timer(&mut active_timer).fuse() => {
                retransmit_window(&mut sender, &mut ack_rx, &mut data_tx, &mut active_timer).await?;
            }
            msg = next_sender_msg(&mut sender_rx, can_recv).fuse() => match msg {
                Some(message) => in_flight_msg = Some(message),
                None => sender_rx_closed = true,
            },
        }
        // Only exit once the coordinator has closed `sender_rx`, any staged message has
        // been sent, and all in-flight frames have been acknowledged.
        if in_flight_msg.is_none() && sender_rx_closed && sender.is_idle() {
            return Ok(());
        }
        send_next_in_flight(
            &mut sender,
            session_id,
            &mut in_flight_msg,
            &mut ack_rx,
            &mut data_tx,
            &mut active_timer,
        )
        .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uart_fpl::encode_frame;

    #[fuchsia::test]
    async fn test_sender_task_happy_path() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(10);
        let (mut ack_tx, ack_rx) = mpsc::channel(10);
        let (mut sender_tx, sender_rx) = mpsc::channel(10);
        let session_id = 12345u32;
        let tx_handle = fuchsia_async::Task::spawn(async move {
            sender_task(sender_rx, outgoing_tx, ack_rx, session_id).await
        });
        let payload = b"data to serial";
        let channel_id = 42u16;
        sender_tx
            .send(SenderMessage::Data { channel_id, payload: payload.to_vec() })
            .await
            .unwrap();
        let frame = outgoing_rx.next().await.unwrap();
        let expected_frame =
            encode_frame(session_id, channel_id, 0, FrameType::Data, payload).unwrap();
        assert_eq!(frame, expected_frame);
        ack_tx.send(0).await.unwrap();
        drop(sender_tx);
        let res = tx_handle.await;
        assert!(res.is_ok());
    }

    #[fuchsia::test]
    async fn test_sender_task_close_message() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(10);
        let (mut ack_tx, ack_rx) = mpsc::channel(10);
        let (mut sender_tx, sender_rx) = mpsc::channel(10);
        let session_id = 67890u32;
        let tx_handle = fuchsia_async::Task::spawn(async move {
            sender_task(sender_rx, outgoing_tx, ack_rx, session_id).await
        });
        sender_tx.send(SenderMessage::Close { channel_id: 7 }).await.unwrap();
        let frame = outgoing_rx.next().await.unwrap();
        let expected_frame = encode_frame(session_id, 7, 0, FrameType::Close, &[]).unwrap();
        assert_eq!(frame, expected_frame);
        ack_tx.send(0).await.unwrap();
        drop(sender_tx);
        assert!(tx_handle.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_sender_task_ack_channel_closed_error() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(10);
        let (ack_tx, ack_rx) = mpsc::channel(10);
        let (_sender_tx, sender_rx) = mpsc::channel(10);
        drop(ack_tx);
        let res = sender_task(sender_rx, outgoing_tx, ack_rx, 12345).await;
        assert!(matches!(res, Err(TargetDriverError::AckChannelClosed)));
    }

    #[fuchsia::test]
    async fn test_sender_task_retransmits_on_timeout() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(10);
        let (mut ack_tx, ack_rx) = mpsc::channel(10);
        let (mut sender_tx, sender_rx) = mpsc::channel(10);
        let tx_handle = fuchsia_async::Task::spawn(async move {
            sender_task(sender_rx, outgoing_tx, ack_rx, 12345).await
        });
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"retransmit".to_vec() })
            .await
            .unwrap();
        let first_frame = outgoing_rx.next().await.unwrap();
        let retransmitted_frame = outgoing_rx.next().await.unwrap();
        assert_eq!(first_frame, retransmitted_frame);
        ack_tx.send(0).await.unwrap();
        drop(sender_tx);
        assert!(tx_handle.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_sender_task_drains_acks_while_data_tx_backpressured() {
        // `SinkExt::send` calls `start_send` followed by `poll_flush`, and `mpsc::Sender`'s
        // `poll_flush` waits for `poll_ready` (an available slot for the *next* message).
        // Using `mpsc::channel(1)` (1 shared slot + 1 per-sender slot = 2 total slots) allows
        // `frame0` to complete `send` while `frame1` fills the second slot and suspends inside
        // `send_data_frame` with both `frame0` (seq 0) and `frame1` (seq 1) in the GBN window.
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
        let (mut ack_tx, ack_rx) = mpsc::channel(10);
        let (mut sender_tx, sender_rx) = mpsc::channel(10);
        let session_id = 12345u32;
        let tx_handle = fuchsia_async::Task::spawn(async move {
            sender_task(sender_rx, outgoing_tx, ack_rx, session_id).await
        });
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"frame0".to_vec() })
            .await
            .unwrap();
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"frame1".to_vec() })
            .await
            .unwrap();
        drop(sender_tx);
        // Because `sender_tx` has buffer capacity, the `.send(...).await` calls above
        // resolve synchronously without yielding to the single-threaded test executor.
        // We MUST yield here before sending `ACK 1`: otherwise `sender_task`'s outer
        // `select_biased!` (which polls `ack_rx` before `sender_rx`) would consume and
        // ignore `ACK 1` on its very first poll while the Go-Back-N window is still
        // empty, before `frame0` and `frame1` have even been read from `sender_rx`.
        fuchsia_async::yield_now().await;
        // Send cumulative ACK for both frames while `sender_task` is suspended inside
        // `send_data_frame` waiting on `outgoing_tx.send` for Frame 1.
        ack_tx.send(1).await.unwrap();
        // Yield again while `outgoing_tx` is still full so `send_data_frame`'s inner
        // `select_biased!` drains `ack_rx` during backpressure (before `outgoing_rx` is read).
        fuchsia_async::yield_now().await;
        // Now drain `outgoing_rx`; because ACK 1 was already processed during `send_data_frame`,
        // `sender_task` should exit cleanly immediately after `frame1` is read.
        let _f0 = outgoing_rx.next().await.unwrap();
        let _f1 = outgoing_rx.next().await.unwrap();
        assert!(tx_handle.await.is_ok());
    }
}
