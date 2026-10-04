// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Low-level serial I/O traits, device discovery, handshake, frame reader, and writer task.
//!
//! This module encapsulates all direct interaction with the serial byte stream
//! below the Go-Back-N `ResendSP` state machines:
//! - **RX (`SerialReader`)**: Rather than spawning a separate `reader_task`,
//!   incoming bytes are parsed by [`SerialReader`], which is borrowed directly
//!   by the receiver task (`reader.next_frame().await`). This allows the caller
//!   to recover any unconsumed trailing bytes via [`SerialReader::take_unconsumed`]
//!   when a session ends or renegotiates.
//! - **TX (`writer_task`)**: Because multiple producers emit frames concurrently
//!   (outgoing data/close frames, duplicate handshake replies, and priority
//!   cumulative ACKs via [`AckTracker`]), outgoing serial writes are handled by
//!   a dedicated [`writer_task`] that prioritizes ACKs and coalesces 4 KiB
//!   frame batches.

use crate::error::{Result, TargetDriverError};
use fidl_fuchsia_hardware_serial::{
    CharacterWidth, Class, Config, DeviceMarker, DeviceProxy, DeviceProxy_Marker, FlowControl,
    Parity, StopWidth,
};
use futures::channel::mpsc;
use futures::prelude::*;
use futures::stream::FusedStream;
use log::{error, info, warn};
use std::time::Duration;
use uart_fpl::{
    AckTracker, CONTROL_CHANNEL_ID, FrameParser, FrameType, ProtocolId, TargetHandshake,
    encode_frame,
};

const SERIAL_DEV_DIR: &str = "/dev/class/serial";
const MAX_WRITE_ATTEMPTS: usize = 5;
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(50);
const WRITE_BATCH_CAPACITY: usize = 4096;

/// Asynchronous byte reader abstraction over a target serial device.
#[allow(async_fn_in_trait)]
pub trait SerialRead {
    /// Reads the next chunk of raw bytes from the serial device.
    ///
    /// Returns an empty `Vec` on EOF.
    async fn serial_read(&mut self) -> Result<Vec<u8>>;
}

/// Asynchronous byte writer abstraction over a target serial device.
#[allow(async_fn_in_trait)]
pub trait SerialWrite {
    /// Writes the full slice of bytes to the serial device.
    async fn serial_write(&mut self, data: &[u8]) -> Result<()>;
}

impl SerialRead for DeviceProxy {
    async fn serial_read(&mut self) -> Result<Vec<u8>> {
        let res = self.read().await?;
        res.map_err(TargetDriverError::DriverStatus)
    }
}

impl SerialWrite for DeviceProxy {
    async fn serial_write(&mut self, data: &[u8]) -> Result<()> {
        let res = self.write(data).await?;
        res.map_err(TargetDriverError::DriverStatus)?;
        Ok(())
    }
}

async fn handle_handshake_frame<W: SerialWrite>(
    handshake: &TargetHandshake,
    frame: &uart_fpl::Frame,
    device: &mut W,
) -> Result<ProtocolId> {
    let (selected, resp_payload) = handshake.handle_request(&frame.payload)?;
    let resp_frame = encode_frame(
        frame.session_id,
        CONTROL_CHANNEL_ID,
        1,
        FrameType::NegotiateResp,
        &resp_payload,
    )?;
    write_frame_to_device(device, &resp_frame).await?;
    if let Some(proto) = selected {
        Ok(proto)
    } else {
        warn!("Handshake negotiation failed: no common protocol");
        Err(TargetDriverError::NoCommonProtocol)
    }
}

/// Runs the target-side framing protocol handshake over `device`.
///
/// Waits for a valid `NegotiateReq` frame on [`CONTROL_CHANNEL_ID`], sends a
/// `NegotiateResp`, and returns the negotiated [`ProtocolId`], session ID, and
/// any unconsumed bytes read past the handshake frame.
///
/// # Errors
///
/// Returns [`TargetDriverError::EmptyRead`] if the serial device returns EOF
/// before negotiation completes, or an I/O/framing error if reading or writing
/// to the serial device fails.
pub async fn run_target_handshake<S: SerialRead + SerialWrite>(
    device: &mut S,
) -> Result<(ProtocolId, u32, Vec<u8>)> {
    run_target_handshake_with_initial_data(device, &[]).await
}

/// Runs the target-side framing protocol handshake seeded with `initial_data`.
///
/// Feeds `initial_data` into the frame parser before reading additional bytes
/// from `device`, allowing carry-over bytes from a previous session to be
/// processed without loss. Invalid or unsupported `NegotiateReq` payloads are
/// rejected in-band while continuing to wait for a valid handshake on `device`.
///
/// # Errors
///
/// Returns [`TargetDriverError::EmptyRead`] if the serial device returns EOF
/// before negotiation completes, or an I/O/framing error if reading or writing
/// to the serial device fails.
pub async fn run_target_handshake_with_initial_data<S: SerialRead + SerialWrite>(
    device: &mut S,
    initial_data: &[u8],
) -> Result<(ProtocolId, u32, Vec<u8>)> {
    let handshake = TargetHandshake::new(vec![ProtocolId::ResendSP]);
    let mut parser = FrameParser::new();
    if !initial_data.is_empty() {
        parser.feed(initial_data);
    }
    loop {
        while let Some(frame) = parser.next_frame() {
            if frame.channel_id == CONTROL_CHANNEL_ID && frame.frame_type == FrameType::NegotiateReq
            {
                if frame.session_id == 0 {
                    warn!("Ignoring NegotiateReq with invalid session_id 0");
                    continue;
                }
                match handle_handshake_frame(&handshake, &frame, device).await {
                    Ok(proto) => return Ok((proto, frame.session_id, parser.take_unconsumed())),
                    Err(
                        e @ (TargetDriverError::NoCommonProtocol | TargetDriverError::Handshake(_)),
                    ) => {
                        warn!(
                            "Ignoring rejected NegotiateReq ({:?}); waiting for next handshake",
                            e
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let bytes = device.serial_read().await?;
        if bytes.is_empty() {
            return Err(TargetDriverError::EmptyRead);
        }
        parser.feed(&bytes);
    }
}

/// Buffered UART protocol frame reader wrapping a [`SerialRead`] implementation.
///
/// Serves as the RX counterpart to [`writer_task`]. It is structured as a
/// borrowed reader rather than a standalone background task so that unconsumed
/// trailing bytes in the [`FrameParser`] can be recovered via
/// [`Self::take_unconsumed`] when a session renegotiates.
#[derive(Debug)]
pub struct SerialReader<R> {
    client: R,
    parser: FrameParser,
}

impl<R: SerialRead> SerialReader<R> {
    /// Creates a new [`SerialReader`] with an empty parser buffer.
    pub fn new(client: R) -> Self {
        Self::with_initial_data(client, &[])
    }

    /// Creates a new [`SerialReader`] pre-populated with `initial_data`.
    pub fn with_initial_data(client: R, initial_data: &[u8]) -> Self {
        let mut parser = FrameParser::new();
        if !initial_data.is_empty() {
            parser.feed(initial_data);
        }
        Self { client, parser }
    }

    /// Drains and returns all unconsumed raw bytes currently buffered in the parser.
    pub fn take_unconsumed(&mut self) -> Vec<u8> {
        self.parser.take_unconsumed()
    }

    /// Re-encodes `frame` and prepends it ahead of any unconsumed bytes in the parser buffer.
    pub fn requeue_frame(&mut self, frame: &uart_fpl::Frame) {
        match encode_frame(
            frame.session_id,
            frame.channel_id,
            frame.seq,
            frame.frame_type,
            &frame.payload,
        ) {
            Ok(raw) => {
                let trailing = self.parser.take_unconsumed();
                self.parser.feed(&raw);
                if !trailing.is_empty() {
                    self.parser.feed(&trailing);
                }
            }
            Err(e) => {
                warn!("Failed to re-encode frame for requeue: {:?}", e);
            }
        }
    }

    /// Reads from the underlying serial device until a complete, valid [`uart_fpl::Frame`] is parsed.
    ///
    /// # Errors
    ///
    /// Returns [`TargetDriverError::Eof`] if the underlying reader returns an empty read,
    /// or propagates any error from [`SerialRead::serial_read`].
    pub async fn next_frame(&mut self) -> Result<uart_fpl::Frame> {
        loop {
            if let Some(frame) = self.parser.next_frame() {
                return Ok(frame);
            }
            let data = self.client.serial_read().await?;
            if data.is_empty() {
                return Err(TargetDriverError::Eof);
            }
            self.parser.feed(&data);
        }
    }
}

async fn probe_serial_path(path: &str) -> Option<(DeviceProxy, Class)> {
    // In `fuchsia.hardware.serial`, `/dev/class/serial` nodes serve the `@discoverable`
    // `DeviceProxy` connector protocol (generated by `fidlgen_rust` as `DeviceProxy_Marker`
    // to avoid colliding with `Device`'s `DeviceProxy` struct), which requires calling
    // `get_channel` to bind a `fuchsia.hardware.serial/Device` channel.
    let proxy_client =
        match fuchsia_component::client::connect_to_protocol_at_path::<DeviceProxy_Marker>(path) {
            Ok(proxy) => proxy,
            Err(e) => {
                warn!("Failed to connect to DeviceProxy at {}: {:?}", path, e);
                return None;
            }
        };
    let (client, server) = fidl::endpoints::create_proxy::<DeviceMarker>();
    if let Err(e) = proxy_client.get_channel(server) {
        warn!("Failed to call get_channel for serial device at {}: {:?}", path, e);
        return None;
    }
    match client.get_class().await {
        Ok(class) => Some((client, class)),
        Err(e) => {
            warn!("Failed to get_class for serial device at {}: {:?}", path, e);
            None
        }
    }
}

fn collect_sorted_serial_paths() -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(SERIAL_DEV_DIR)? {
        let path_buf = entry?.path();
        let path = path_buf.to_str().ok_or(TargetDriverError::NonUtf8Path)?.to_owned();
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

async fn discover_serial_port() -> Result<(DeviceProxy, String)> {
    let paths = collect_sorted_serial_paths()?;
    let mut console_port = None;
    for path in paths {
        if let Some((client, class)) = probe_serial_path(&path).await {
            // Prefer a dedicated generic UART over one attached to an interactive console, and
            // ignore internal board UARTs (BluetoothHci, KernelDebug, Mcu).
            // Note: In practice, the Class::Console branch may currently be a no-op because no
            // Fuchsia serial driver reports Class::Console, even when used as a console port.
            match class {
                Class::Generic => return Ok((client, path)),
                Class::Console => {
                    console_port.get_or_insert((client, path));
                }
                _ => {}
            }
        }
    }
    console_port.ok_or(TargetDriverError::NoSerialPorts)
}

/// Discovers a serial device under `/dev/class/serial` and configures it for 8N1 at `baud_rate`.
///
/// Prefers `Class::Generic` ports over `Class::Console` ports.
///
/// # Errors
///
/// Returns [`TargetDriverError::NoSerialPorts`] if no usable port is found, or
/// propagates filesystem and FIDL errors encountered during discovery.
pub async fn open_serial_device(baud_rate: u32) -> Result<DeviceProxy> {
    let (client, path) = discover_serial_port().await?;
    let config = Config {
        character_width: CharacterWidth::Bits8,
        stop_width: StopWidth::Bits1,
        parity: Parity::None,
        control_flow: FlowControl::None,
        baud_rate,
    };
    let status = client.set_config(&config).await?;
    if status != 0 {
        // Virtual serial drivers such as uart16550 on QEMU reject SetConfig while enabled or when
        // baud_rate exceeds 115200, even though the underlying channel operates normally.
        warn!("SetConfig on {} returned non-OK status: {}", path, status);
    }
    info!("Successfully opened and configured serial device at {}", path);
    Ok(client)
}

async fn write_frame_to_device<W: SerialWrite>(device: &mut W, frame: &[u8]) -> Result<()> {
    let mut attempts = 0;
    loop {
        match device.serial_write(frame).await {
            Ok(()) => return Ok(()),
            Err(e) => error!("Serial write error: {:?}, attempt={}", e, attempts),
        }
        attempts += 1;
        if attempts >= MAX_WRITE_ATTEMPTS {
            return Err(TargetDriverError::WriteRetriesExhausted);
        }
        fuchsia_async::Timer::new(WRITE_RETRY_DELAY).await;
    }
}

async fn write_ack_frame<W: SerialWrite>(device: &mut W, session_id: u32, seq: u8) -> Result<()> {
    let ack_frame = encode_frame(session_id, CONTROL_CHANNEL_ID, seq, FrameType::Ack, &[])?;
    write_frame_to_device(device, &ack_frame).await
}

async fn flush_frame_batch<W: SerialWrite>(
    device: &mut W,
    first_frame: &[u8],
    data_rx: &mut mpsc::Receiver<Vec<u8>>,
    batch_buf: &mut Vec<u8>,
) -> Result<()> {
    batch_buf.clear();
    batch_buf.extend_from_slice(first_frame);
    while let Ok(next_frame) = data_rx.try_recv() {
        batch_buf.extend_from_slice(&next_frame);
        if batch_buf.len() >= WRITE_BATCH_CAPACITY {
            break;
        }
    }
    write_frame_to_device(device, batch_buf).await
}

/// Runs the serial frame writer loop, prioritizing pending ACKs from `ack_tracker`
/// ahead of coalesced outgoing data frames from `data_rx`.
///
/// # Errors
///
/// Returns [`TargetDriverError::WriteRetriesExhausted`] if writing to `device`
/// fails repeatedly, or [`TargetDriverError::Frame`] if encoding an ACK frame fails.
pub async fn writer_task<W: SerialWrite>(
    mut device: W,
    ack_tracker: AckTracker,
    mut data_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let mut batch_buf = Vec::with_capacity(WRITE_BATCH_CAPACITY);
    loop {
        if let Some((session_id, seq)) = ack_tracker.take_ack() {
            write_ack_frame(&mut device, session_id, seq).await?;
            continue;
        }
        if data_rx.is_terminated() {
            break;
        }
        futures::select_biased! {
            (session_id, seq) = ack_tracker.wait_ack().fuse() => {
                write_ack_frame(&mut device, session_id, seq).await?;
            }
            frame = data_rx.next() => match frame {
                Some(first_frame) => {
                    flush_frame_batch(&mut device, &first_frame, &mut data_rx, &mut batch_buf)
                        .await?;
                }
                None => break,
            },
            complete => break,
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) struct MockSerial {
    pub(crate) read_data: Vec<Vec<u8>>,
    pub(crate) write_data: Vec<Vec<u8>>,
}

#[cfg(test)]
impl SerialRead for MockSerial {
    async fn serial_read(&mut self) -> Result<Vec<u8>> {
        if self.read_data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.read_data.remove(0))
    }
}

#[cfg(test)]
impl SerialWrite for MockSerial {
    async fn serial_write(&mut self, data: &[u8]) -> Result<()> {
        self.write_data.push(data.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use uart_fpl::{HandshakeResponse, HandshakeStatus, HostHandshake};

    struct SharedWriteData {
        data: RefCell<Vec<Vec<u8>>>,
    }

    #[derive(Clone)]
    struct MockSerialWriter {
        shared: Rc<SharedWriteData>,
    }

    impl SerialWrite for MockSerialWriter {
        async fn serial_write(&mut self, data: &[u8]) -> Result<()> {
            self.shared.data.borrow_mut().push(data.to_vec());
            Ok(())
        }
    }

    #[fuchsia::test]
    async fn test_writer_task_prioritizes_acks() {
        let shared = Rc::new(SharedWriteData { data: RefCell::new(Vec::new()) });
        let writer = MockSerialWriter { shared: shared.clone() };
        let ack_tracker = AckTracker::new();
        let (mut data_tx, data_rx) = mpsc::channel(10);
        let session_id = 12345u32;
        let data_frame_1 = encode_frame(session_id, 1, 0, FrameType::Data, &[1]).unwrap();
        let data_frame_2 = encode_frame(session_id, 1, 1, FrameType::Data, &[2]).unwrap();
        let ack_frame =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, FrameType::Ack, &[]).unwrap();
        data_tx.send(data_frame_1.clone()).await.unwrap();
        data_tx.send(data_frame_2.clone()).await.unwrap();
        ack_tracker.set_ack(session_id, 0);
        let writer_handle =
            fuchsia_async::Task::local(
                async move { writer_task(writer, ack_tracker, data_rx).await },
            );
        drop(data_tx);
        writer_handle.await.unwrap();
        let written = shared.data.borrow().clone();
        let expected_data = [data_frame_1, data_frame_2].concat();
        assert_eq!(written, vec![ack_frame, expected_data]);
    }

    #[fuchsia::test]
    async fn test_run_target_handshake_mock_serial() {
        let session_id = 0xCAFEBABE;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let req_frame =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let mut mock_serial = MockSerial { read_data: vec![req_frame], write_data: Vec::new() };
        let (proto, negotiated_sid, unconsumed) =
            run_target_handshake(&mut mock_serial).await.expect("handshake should succeed");
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(negotiated_sid, session_id);
        assert!(unconsumed.is_empty());
        assert_eq!(mock_serial.write_data.len(), 1);
        let mut parser = FrameParser::new();
        parser.feed(&mock_serial.write_data[0]);
        let resp_frame = parser.next_frame().expect("valid response frame");
        assert_eq!(resp_frame.session_id, session_id);
        assert_eq!(resp_frame.channel_id, CONTROL_CHANNEL_ID);
        assert_eq!(resp_frame.seq, 1);
        assert_eq!(resp_frame.frame_type, FrameType::NegotiateResp);
        let resp = HandshakeResponse::try_from(resp_frame.payload.as_slice()).unwrap();
        assert_eq!(resp.status, HandshakeStatus::Success);
        assert_eq!(resp.selected, Some(ProtocolId::ResendSP));
    }

    #[fuchsia::test]
    async fn test_run_target_handshake_ignores_zero_session_id() {
        let valid_sid = 0xCAFEBABE;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let zero_req = encode_frame(0, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let valid_req =
            encode_frame(valid_sid, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let mut mock_serial =
            MockSerial { read_data: vec![zero_req, valid_req], write_data: Vec::new() };
        let (proto, negotiated_sid, _) =
            run_target_handshake(&mut mock_serial).await.expect("handshake should succeed");
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(negotiated_sid, valid_sid);
        assert_eq!(mock_serial.write_data.len(), 1);
    }

    #[fuchsia::test]
    async fn test_serial_reader_with_initial_data() {
        let session_id = 12345u32;
        let frame_bytes =
            encode_frame(session_id, 42, 0, FrameType::Data, b"buffered_stream_data").unwrap();
        let mock_serial = MockSerial { read_data: Vec::new(), write_data: Vec::new() };
        let mut reader = SerialReader::with_initial_data(mock_serial, &frame_bytes);
        let frame = reader.next_frame().await.expect("Should parse frame from initial_data");
        assert_eq!(frame.session_id, session_id);
        assert_eq!(frame.channel_id, 42);
        assert_eq!(frame.payload, b"buffered_stream_data");
    }

    #[fuchsia::test]
    async fn test_serial_reader_requeue_frame_and_take_unconsumed() {
        let session_id = 12345u32;
        let frame1_bytes = encode_frame(session_id, 1, 0, FrameType::Data, b"first").unwrap();
        let frame2_bytes = encode_frame(session_id, 1, 1, FrameType::Data, b"second").unwrap();
        let mock_serial = MockSerial {
            read_data: vec![[frame1_bytes.clone(), frame2_bytes.clone()].concat()],
            write_data: Vec::new(),
        };
        let mut reader = SerialReader::new(mock_serial);
        let parsed_frame1 = reader.next_frame().await.unwrap();
        reader.requeue_frame(&parsed_frame1);
        let unconsumed = reader.take_unconsumed();
        assert_eq!(unconsumed, [frame1_bytes, frame2_bytes].concat());
    }

    #[fuchsia::test]
    async fn test_run_target_handshake_retains_unconsumed_data() {
        let session_id = 0x12345678;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let req_frame =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let trailing = encode_frame(session_id, 1, 0, FrameType::Data, b"initial payload").unwrap();
        let combined = [req_frame, trailing.clone()].concat();
        let mut mock_serial = MockSerial { read_data: vec![combined], write_data: Vec::new() };
        let (proto, negotiated_sid, unconsumed) =
            run_target_handshake(&mut mock_serial).await.expect("handshake should succeed");
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(negotiated_sid, session_id);
        assert_eq!(unconsumed, trailing);
        let mut reader = SerialReader::with_initial_data(mock_serial, &unconsumed);
        let parsed = reader.next_frame().await.expect("Should parse unconsumed frame");
        assert_eq!(parsed.channel_id, 1);
        assert_eq!(parsed.payload, b"initial payload");
    }

    #[fuchsia::test]
    async fn test_run_target_handshake_ignores_rejected_requests_and_retains_trailing() {
        let unsupported_req = encode_frame(
            0x22334455,
            CONTROL_CHANNEL_ID,
            0,
            FrameType::NegotiateReq,
            &[1, 0, 0, 0, 99],
        )
        .unwrap();
        let malformed_req =
            encode_frame(0x22334456, CONTROL_CHANNEL_ID, 0, FrameType::NegotiateReq, &[1]).unwrap();
        let valid_sid = 0x22334457;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let valid_req =
            encode_frame(valid_sid, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        // Coalesce unsupported, malformed, and valid requests into a single chunk to verify
        // that rejected NegotiateReq frames do not abort the loop or discard trailing bytes.
        let combined = [unsupported_req, malformed_req, valid_req].concat();
        let mut mock_serial = MockSerial { read_data: vec![combined], write_data: Vec::new() };
        let (proto, negotiated_sid, unconsumed) =
            run_target_handshake(&mut mock_serial).await.expect("handshake should succeed");
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(negotiated_sid, valid_sid);
        assert!(unconsumed.is_empty());
        // One NoCommonProtocol NegotiateResp + one Success NegotiateResp.
        assert_eq!(mock_serial.write_data.len(), 2);
    }

    #[fuchsia::test]
    async fn test_run_target_handshake_retries_transient_write_error() {
        struct FlakyWriteSerial {
            read_data: Vec<Vec<u8>>,
            write_data: Vec<Vec<u8>>,
            remaining_write_failures: usize,
        }

        impl SerialRead for FlakyWriteSerial {
            async fn serial_read(&mut self) -> Result<Vec<u8>> {
                if self.read_data.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(self.read_data.remove(0))
            }
        }

        impl SerialWrite for FlakyWriteSerial {
            async fn serial_write(&mut self, data: &[u8]) -> Result<()> {
                if self.remaining_write_failures > 0 {
                    self.remaining_write_failures -= 1;
                    return Err(TargetDriverError::DriverStatus(-27));
                }
                self.write_data.push(data.to_vec());
                Ok(())
            }
        }

        let session_id = 0x11223344;
        let host = HostHandshake::new(vec![ProtocolId::ResendSP]);
        let (frame_type, payload) = host.start().unwrap();
        let req_frame =
            encode_frame(session_id, CONTROL_CHANNEL_ID, 0, frame_type, &payload).unwrap();
        let mut flaky = FlakyWriteSerial {
            read_data: vec![req_frame],
            write_data: Vec::new(),
            remaining_write_failures: 2,
        };
        let (proto, negotiated_sid, _) =
            run_target_handshake(&mut flaky).await.expect("handshake should succeed after retry");
        assert_eq!(proto, ProtocolId::ResendSP);
        assert_eq!(negotiated_sid, session_id);
        assert_eq!(flaky.remaining_write_failures, 0);
        assert_eq!(flaky.write_data.len(), 1);
    }
}
