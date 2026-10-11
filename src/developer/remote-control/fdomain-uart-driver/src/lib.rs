// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Target-side FDomain UART driver daemon for Fuchsia devices.
//!
//! This component acts as the target-side transport bridge between a physical or
//! virtual serial device (`/dev/class/serial`) and the Remote Control Service
//! (`fuchsia.developer.remotecontrol.connector.Connector`).
//!
//! # Architecture
//!
//! 1. **Boot Argument Check**: Checks `fuchsia.boot.Arguments` for `dev.fdomain.uart`
//!    (defaults to disabled; opt-in via `dev.fdomain.uart=true`) and `dev.fdomain.uart.baud`
//!    (defaults to `1000000`).
//! 2. **Device Discovery**: Scans `/dev/class/serial`, preferring `Class::Generic`
//!    ports over `Class::Console` ports, and configures 8N1 at the target baud rate.
//! 3. **Handshake**: Runs `TargetHandshake` supporting `ProtocolId::ResendSP`
//!    (Go-Back-N sliding-window framing) and preserves any trailing bytes read
//!    past the handshake frame.
//! 4. **Bridge Tasks**: Spawns four concurrent tasks (`writer_task`, `receiver_task`,
//!    `sender_task`, and `coordinator_task`) that multiplex FDomain channels over
//!    UART frames and connect each channel to RCS via `fdomain_toolbox_socket`.

pub mod coordinator;
pub mod error;
pub mod receiver;
pub mod sender;
pub mod serial;

use crate::coordinator::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_CLIENT_DATA_CAPACITY, coordinator_task,
};
use crate::error::TargetDriverError;
use crate::receiver::receiver_task;
use crate::sender::sender_task;
use crate::serial::{
    SerialRead, SerialReader, SerialWrite, open_serial_device,
    run_target_handshake_with_initial_data, writer_task,
};
pub use error::Result;
use fidl_fuchsia_developer_remotecontrol_connector::{ConnectorMarker, ConnectorProxy};
use fidl_fuchsia_hardware_serial::DeviceProxy;
use fuchsia_component::client::connect_to_protocol;
use futures::channel::mpsc;
use log::{error, info, warn};
use std::time::Duration;
use uart_fpl::{AckTracker, CONTROL_CHANNEL_ID, FrameType, ProtocolId, encode_frame};

/// Boot argument key controlling whether the target FDomain UART driver is enabled.
const BOOT_ARG_ENABLED: &str = "dev.fdomain.uart";

/// Boot argument key specifying an optional UART baud rate override.
const BOOT_ARG_BAUD_RATE: &str = "dev.fdomain.uart.baud";

/// Default UART baud rate (1,000,000 bps) used when [`BOOT_ARG_BAUD_RATE`] is unset or invalid.
const DEFAULT_BAUD_RATE: u32 = 1_000_000;

/// Delay before attempting to reopen the serial device after an I/O error or disconnect.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Queries `fuchsia.boot.Arguments` to check whether the driver is enabled via
/// [`BOOT_ARG_ENABLED`], defaulting to `false` when unset or if the protocol is unavailable.
async fn is_enabled() -> bool {
    if let Ok(args) = connect_to_protocol::<fidl_fuchsia_boot::ArgumentsMarker>() {
        match args.get_bool(BOOT_ARG_ENABLED, false).await {
            Ok(val) => val,
            Err(e) => {
                warn!("Failed to get {} boot arg: {:?}", BOOT_ARG_ENABLED, e);
                false
            }
        }
    } else {
        warn!("Failed to connect to fuchsia.boot.Arguments");
        false
    }
}

/// Reads the configured UART baud rate from `fuchsia.boot.Arguments` ([`BOOT_ARG_BAUD_RATE`]),
/// falling back to [`DEFAULT_BAUD_RATE`] when unset, zero, or unparseable.
async fn read_baud_rate() -> u32 {
    let Ok(args) = connect_to_protocol::<fidl_fuchsia_boot::ArgumentsMarker>() else {
        warn!("Failed to connect to fuchsia.boot.Arguments to get baud rate");
        return DEFAULT_BAUD_RATE;
    };
    match args.get_string(BOOT_ARG_BAUD_RATE).await {
        Ok(Some(value)) => match value.parse::<u32>() {
            Ok(baud) if baud > 0 => baud,
            _ => {
                warn!("Failed to parse {} '{}', using default", BOOT_ARG_BAUD_RATE, value);
                DEFAULT_BAUD_RATE
            }
        },
        Ok(None) => DEFAULT_BAUD_RATE,
        Err(e) => {
            warn!("Failed to get {} boot arg: {:?}", BOOT_ARG_BAUD_RATE, e);
            DEFAULT_BAUD_RATE
        }
    }
}

/// Performs the Channel 0 protocol negotiation handshake on `device`, seeded with any
/// `initial_data` carried over from a previous session.
///
/// On the first invocation after daemon startup (`*is_initial_start == true`), emits a
/// cold-start `Reset(0)` frame before listening for the host's `NegotiateReq`.
async fn negotiate_session<S: SerialRead + SerialWrite + Clone>(
    device: &S,
    is_initial_start: &mut bool,
    initial_data: &[u8],
) -> Result<(ProtocolId, u32, Vec<u8>)> {
    if *is_initial_start {
        // On cold start, emit an initial Reset frame with sentinel session_id = 0 to
        // instruct any listening host to flush stale in-flight state. Once written
        // successfully, `is_initial_start` is cleared for the lifetime of the daemon
        // to avoid spurious unsolicited resets on subsequent device reopens.
        let reset_frame = encode_frame(0, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[])?;
        let mut device_writer = device.clone();
        if let Err(e) = device_writer.serial_write(&reset_frame).await {
            warn!("Failed to send reset frame: {:?}", e);
        } else {
            *is_initial_start = false;
        }
    }
    let mut device_reader = device.clone();
    run_target_handshake_with_initial_data(&mut device_reader, initial_data).await
}

/// Spawns and drives the four concurrent UART bridge tasks ([`writer_task`],
/// [`receiver_task`], [`sender_task`], and [`coordinator_task`]) for an active `session_id`.
///
/// Returns the joint task completion result along with any unconsumed trailing bytes
/// buffered in [`SerialReader`] when the session terminates (for example, bytes from a
/// coalesced `NegotiateReq` immediately following a `Reset` frame).
async fn run_bridge_session<S: SerialRead + SerialWrite + Clone>(
    device: S,
    session_id: u32,
    unconsumed: &[u8],
    rcs_connector: ConnectorProxy,
) -> (Result<((), (), (), ())>, Vec<u8>) {
    let ack_tracker = AckTracker::new();
    let (data_tx, data_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
    let (ack_tx, ack_rx) = mpsc::channel(DEFAULT_CLIENT_DATA_CAPACITY);
    let (sender_tx, sender_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
    let (serial_tx, serial_rx) = mpsc::channel(DEFAULT_CLIENT_DATA_CAPACITY);
    let mut reader = SerialReader::with_initial_data(device.clone(), unconsumed);
    let res = {
        let writer = writer_task(device, ack_tracker.clone(), data_rx);
        let receiver =
            receiver_task(&mut reader, serial_tx, ack_tracker, ack_tx, session_id, data_tx.clone());
        let sender = sender_task(sender_rx, data_tx, ack_rx, session_id);
        let coordinator = coordinator_task(serial_rx, sender_tx, rcs_connector);
        info!("Bridge tasks started");
        futures::try_join!(writer, receiver, sender, coordinator)
    };
    (res, reader.take_unconsumed())
}

/// Runs consecutive protocol handshakes and FDomain bridge sessions over an open `device`.
///
/// In-band session transitions ([`TargetDriverError::SessionIdChanged`] or
/// [`TargetDriverError::ResetRequested`]) renegotiate immediately on the open `device`
/// while preserving trailing bytes in `carry_over`. Transport-level errors or EOF
/// explicitly drop `device` before waiting [`RETRY_DELAY`] so the underlying serial
/// driver can complete asynchronous teardown before the port is reopened.
async fn run_device_sessions<S: SerialRead + SerialWrite + Clone>(
    device: S,
    rcs_connector: &ConnectorProxy,
    is_initial_start: &mut bool,
) {
    let mut carry_over = Vec::new();
    loop {
        let (proto, session_id, unconsumed) =
            match negotiate_session(&device, is_initial_start, &carry_over).await {
                Ok(negotiated) => negotiated,
                Err(e) => {
                    error!("Handshake failed ({:?}); reopening serial device in 1s...", e);
                    drop(device);
                    fuchsia_async::Timer::new(RETRY_DELAY).await;
                    return;
                }
            };
        info!("Handshake succeeded. Negotiated protocol: {:?}", proto);
        let (res, remaining) =
            run_bridge_session(device.clone(), session_id, &unconsumed, rcs_connector.clone())
                .await;
        match res {
            Err(TargetDriverError::SessionIdChanged) => {
                info!("Session ID changed; renegotiating on existing serial device");
                carry_over = remaining;
            }
            Err(TargetDriverError::ResetRequested) => {
                info!("Session reset requested; renegotiating on existing serial device");
                carry_over = remaining;
            }
            Err(e) => {
                warn!(
                    "Bridge session ended with error ({:?}); reopening serial device in 1s...",
                    e
                );
                drop(device);
                fuchsia_async::Timer::new(RETRY_DELAY).await;
                return;
            }
            Ok(_) => {
                drop(device);
                fuchsia_async::Timer::new(RETRY_DELAY).await;
                return;
            }
        }
    }
}

/// Executes a single device-discovery and session lifecycle iteration.
///
/// Attempts to open and configure a target UART device at `baud_rate` and run
/// [`run_device_sessions`], or backs off for [`RETRY_DELAY`] if no serial port is
/// ready yet.
async fn run_driver_iteration(
    baud_rate: u32,
    rcs_connector: &ConnectorProxy,
    is_initial_start: &mut bool,
) -> Result<()> {
    let device: DeviceProxy = match open_serial_device(baud_rate).await {
        Ok(device) => device,
        // Silence NoSerialPorts and early devfs Io errors to prevent Archivist log
        // buffer flooding on targets without populated UART controllers or during
        // asynchronous driver discovery.
        Err(TargetDriverError::NoSerialPorts | TargetDriverError::Io(_)) => {
            fuchsia_async::Timer::new(RETRY_DELAY).await;
            return Ok(());
        }
        Err(e) => {
            warn!("Failed to open serial device: {:?}. Retrying in 1s...", e);
            fuchsia_async::Timer::new(RETRY_DELAY).await;
            return Ok(());
        }
    };
    run_device_sessions(device, rcs_connector, is_initial_start).await;
    Ok(())
}

/// Runs the target-side FDomain UART driver daemon.
///
/// # Errors
///
/// Returns [`TargetDriverError::RcsConnect`] if connecting to the
/// `fuchsia.developer.remotecontrol.connector.Connector` protocol fails.
pub async fn run_driver() -> Result<()> {
    info!("Starting FDomain UART Driver");
    if !is_enabled().await {
        info!(
            "FDomain UART Driver is disabled by boot argument '{}'. Sleeping forever.",
            BOOT_ARG_ENABLED
        );
        futures::future::pending::<()>().await;
    }
    let baud_rate = read_baud_rate().await;
    let rcs_connector = connect_to_protocol::<ConnectorMarker>()
        .map_err(|e| TargetDriverError::RcsConnect(format!("{e:#}")))?;
    let mut is_initial_start = true;
    loop {
        run_driver_iteration(baud_rate, &rcs_connector, &mut is_initial_start).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::MockSerial;
    use std::cell::RefCell;
    use std::rc::Rc;
    use uart_fpl::{FrameParser, HostHandshake};

    #[derive(Clone)]
    struct SharedMockSerial(Rc<RefCell<MockSerial>>);

    impl SerialRead for SharedMockSerial {
        async fn serial_read(&mut self) -> Result<Vec<u8>> {
            let mut inner = self.0.borrow_mut();
            if inner.read_data.is_empty() {
                return Ok(Vec::new());
            }
            Ok(inner.read_data.remove(0))
        }
    }

    impl SerialWrite for SharedMockSerial {
        async fn serial_write(&mut self, data: &[u8]) -> Result<()> {
            self.0.borrow_mut().write_data.push(data.to_vec());
            Ok(())
        }
    }

    #[fuchsia::test]
    async fn test_negotiate_session_initial_start_sends_reset() {
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let req_1 = encode_frame(111, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let req_2 = encode_frame(222, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let mock = SharedMockSerial(Rc::new(RefCell::new(MockSerial {
            read_data: vec![req_1],
            write_data: Vec::new(),
        })));
        let mut is_initial_start = true;
        let (proto_1, sid_1, _) =
            negotiate_session(&mock, &mut is_initial_start, &[]).await.unwrap();
        assert!(!is_initial_start);
        assert_eq!(proto_1, ProtocolId::ResendSP);
        assert_eq!(sid_1, 111);
        let mut parser = FrameParser::new();
        for chunk in &mock.0.borrow().write_data {
            parser.feed(chunk);
        }
        let first_written = parser.next_frame().unwrap();
        assert_eq!(first_written.frame_type, FrameType::Reset);
        let second_written = parser.next_frame().unwrap();
        assert_eq!(second_written.frame_type, FrameType::NegotiateResp);

        mock.0.borrow_mut().write_data.clear();
        let (proto_2, sid_2, _) =
            negotiate_session(&mock, &mut is_initial_start, &req_2).await.unwrap();
        assert_eq!(proto_2, ProtocolId::ResendSP);
        assert_eq!(sid_2, 222);
        let mut parser_2 = FrameParser::new();
        for chunk in &mock.0.borrow().write_data {
            parser_2.feed(chunk);
        }
        let only_written = parser_2.next_frame().unwrap();
        assert_eq!(only_written.frame_type, FrameType::NegotiateResp);
        assert!(parser_2.next_frame().is_none());
    }

    #[fuchsia::test]
    async fn test_run_bridge_session_preserves_remaining_bytes_on_reset() {
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let session_1 = 111u32;
        let session_2 = 222u32;
        let reset_1 =
            encode_frame(session_1, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        let req_2 = encode_frame(session_2, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        // Coalesce `Reset` for session 1 and `NegotiateReq` for session 2 into a single read chunk.
        let coalesced = [reset_1, req_2.clone()].concat();
        let mock = SharedMockSerial(Rc::new(RefCell::new(MockSerial {
            read_data: vec![coalesced],
            write_data: Vec::new(),
        })));
        let (connector_proxy, _connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        let (res, remaining) =
            run_bridge_session(mock.clone(), session_1, &[], connector_proxy).await;
        assert!(matches!(res, Err(TargetDriverError::ResetRequested)));
        assert_eq!(remaining, req_2);

        // Passing `remaining` as `carry_over` into `negotiate_session` immediately negotiates session 2.
        let mut is_initial_start = false;
        let (proto, sid, _) =
            negotiate_session(&mock, &mut is_initial_start, &remaining).await.unwrap();
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(sid, session_2);
    }
}
