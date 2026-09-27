// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Core daemon implementation for the Fuchsia UART host driver.
//!
//! Provides [`HostDriver`], the daemon execution run loop that supervises client
//! connections, manages background UART link lifecycle and reconnection, and resolves
//! target identity over FDomain Remote Control Service (RCS).
//!
//! Coordinates bidirectional multiplexing and reliable transport over the UART link
//! using four dedicated asynchronous session tasks:
//! * `coordinator_task`: Manages client connection lifecycle, channel allocations, and event routing.
//! * `sender_task`: Implements sliding-window Go-Back-N retransmission using [`uart_fpl::ResendSender`].
//! * `writer_task`: Transmits outgoing frames and cumulative ACKs to the UART device with batching and rate limiting.
//! * `receiver_task`: Demultiplexes incoming frames using [`uart_fpl::ResendReceiver`] and generates cumulative ACKs.

use ffx_tool_uart::DaemonMetrics;
use futures::channel::mpsc;
use futures::future::{FutureExt, poll_fn};
use futures::{SinkExt, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::os::unix::fs::FileTypeExt as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use uart_driver_api::{ConnectionError, ConnectionMetadata, ConnectionStatus, UartProtocol};
use uart_fpl::{
    AckOutcome, AckTracker, CONTROL_CHANNEL_ID, DEFAULT_RETRANSMISSION_TIMEOUT, Frame, FrameParser,
    FrameStatus, FrameType, ResendReceiver, ResendSender, encode_frame,
};

pub const DEFAULT_CLIENT_DATA_CAPACITY: usize = 64;
pub const DEFAULT_SERIAL_DATA_CAPACITY: usize = 64;
const UART_READ_BUFFER_SIZE: usize = 16 * 1024;
const TERMINATE_POLL_RETRIES: usize = 5;
const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLIENT_WRITER_CHANNEL_CAPACITY: usize = 256;
const MAX_UART_WRITE_BATCH_BYTES: usize = 4096;
const IDENTITY_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_IDENTITY_ATTEMPTS: usize = 5;
const IDENTITY_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const METADATA_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_RETRIES: usize = 3;
const SERIAL_8N1_BITS_PER_BYTE: u32 = 10;

pub fn is_char_device(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.file_type().is_char_device()).unwrap_or(false)
}

mod adapters;
use adapters::FDomainTransport;
use fdomain_client::fidl::DiscoverableProtocolMarker as _;
use fdomain_fuchsia_developer_remotecontrol as rcs_fdomain;
use fdomain_fuchsia_io as fio_fdomain;
pub use ffx_tool_uart::stream::{AsyncUart, UartStream, connect_uart_stream};

/// Errors that can occur when managing Unix domain socket lifecycle and stale socket cleanup.
#[derive(thiserror::Error, Debug)]
pub enum RemoveAndBindError {
    /// The target socket path already exists and an active daemon is actively listening.
    #[error("Socket {0} already exists and is in use")]
    InUse(PathBuf),
    /// A stale socket file was detected but could not be removed from the filesystem.
    #[error("Could not remove stale socket at {0}: {1}")]
    RemoveStale(PathBuf, #[source] std::io::Error),
    /// An unexpected I/O error occurred while probing whether a pre-existing socket is active.
    #[error("Unexpected error when checking for stale socket at {0}: {1}")]
    ConnectCheck(PathBuf, #[source] std::io::Error),
    /// Binding the Unix domain listener to the target path failed.
    #[error("Could not listen on socket at {0}: {1}")]
    Bind(PathBuf, #[source] std::io::Error),
}

/// Errors that can occur when reading frames from a UART byte stream.
#[derive(thiserror::Error, Debug)]
pub enum ReaderError {
    /// Reading from the underlying UART stream failed with an I/O error.
    #[error("Failed to read from UART: {0}")]
    Read(#[source] std::io::Error),
    /// Reached end-of-file unexpectedly while reading from UART.
    #[error("EOF from UART")]
    Eof,
}

/// Errors that can occur during [`writer_task`] execution.
#[derive(thiserror::Error, Debug)]
pub enum WriterTaskError {
    /// Writing frame bytes to the UART device failed.
    #[error("Failed to write to UART: {0}")]
    Write(#[source] std::io::Error),
    /// Flushing the UART device stream failed.
    #[error("Failed to flush UART: {0}")]
    Flush(#[source] std::io::Error),
    /// Encoding an outgoing cumulative ACK frame failed.
    #[error("Failed to encode ACK frame: {0}")]
    EncodeAck(#[source] uart_fpl::FrameError),
}

/// Errors that can occur during [`receiver_task`] execution.
#[derive(thiserror::Error, Debug)]
pub enum ReceiverTaskError {
    /// Reading a frame from the UART stream failed.
    #[error("Reader error: {0}")]
    Reader(#[from] ReaderError),
    /// The target sent a protocol reset frame.
    #[error("Target requested protocol reset")]
    TargetRequestedReset,
    /// Advancing the sliding-window in-order sequence failed.
    #[error("Failed to advance in-order sequence: {0}")]
    AdvanceSeq(#[source] uart_fpl::UnexpectedSeqError),
    /// Enqueuing an ACK event sequence to the sender task failed.
    #[error("Failed to enqueue ACK event: {0}")]
    EnqueueAck(#[source] mpsc::SendError),
    /// Routing a received event to the coordinator failed because the coordinator channel closed.
    #[error("Failed to send {0} to coordinator: {1}")]
    CoordinatorClosed(&'static str, String),
}

/// Errors that can occur during [`sender_task`] execution.
#[derive(thiserror::Error, Debug)]
pub enum SenderTaskError {
    /// The ACK notification channel was unexpectedly closed.
    #[error("ACK channel closed")]
    AckChannelClosed,
    /// Retransmission attempts exceeded the configured limit during timeout handling.
    #[error("Retransmission limit exceeded: {0}")]
    RetransmissionLimit(#[from] uart_fpl::RetransmissionLimitExceeded),
    /// Encoding an outgoing frame failed.
    #[error("Failed to encode frame: {0}")]
    EncodeFrame(#[source] uart_fpl::FrameError),
    /// Sending a frame to the writer task failed because the writer channel closed.
    #[error("Failed to send frame to writer")]
    WriterClosed,
}

/// Unified error type encompassing failures across all asynchronous UART driver session tasks.
#[derive(thiserror::Error, Debug)]
pub enum TaskError {
    /// An error occurred in the writer task.
    #[error(transparent)]
    Writer(#[from] WriterTaskError),
    /// An error occurred in the receiver task.
    #[error(transparent)]
    Receiver(#[from] ReceiverTaskError),
    /// An error occurred in the sender task.
    #[error(transparent)]
    Sender(#[from] SenderTaskError),
}

/// Errors that can occur when querying target identity over FDomain.
#[derive(thiserror::Error, Debug)]
pub enum IdentityError {
    /// Failed to connect to the Unix domain socket.
    #[error("Failed to connect to socket {0}: {1}")]
    Connect(PathBuf, #[source] std::io::Error),
    /// Failed to retrieve the FDomain namespace.
    #[error("Failed to get FDomain namespace: {0}")]
    Namespace(#[source] fdomain_client::Error),
    /// Failed to open the RemoteControlService protocol within the FDomain namespace.
    #[error("Failed to open RCS protocol: {0}")]
    OpenRcs(#[source] fidl::Error),
    /// The FIDL transport call to IdentifyHost failed.
    #[error("IdentifyHost FIDL call failed: {0}")]
    Fidl(#[source] fidl::Error),
    /// The IdentifyHost service returned an error status.
    #[error("IdentifyHost returned error: {0:?}")]
    Identify(rcs_fdomain::IdentifyHostError),
    /// The FDomain transport closed prematurely during negotiation.
    #[error("FDomain transport closed prematurely")]
    TransportClosed,
    /// Timed out waiting for the target to respond with host identity.
    #[error("Timed out waiting for IdentifyHost response")]
    Timeout,
}

async fn terminate_process(pid: u32) {
    if pid <= 1 || pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: libc::kill is a standard POSIX system call to send SIGTERM for graceful termination.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if !ffx_tool_uart::is_running(pid) {
        log::info!("Orphaned daemon {} exited immediately after SIGTERM.", pid);
        return;
    }
    for _ in 0..TERMINATE_POLL_RETRIES {
        tokio::time::sleep(TERMINATE_POLL_INTERVAL).await;
        if !ffx_tool_uart::is_running(pid) {
            log::info!("Orphaned daemon {} exited after SIGTERM.", pid);
            return;
        }
    }
    log::warn!("Orphaned daemon {} still running after SIGTERM. Sending SIGKILL...", pid);
    // SAFETY: libc::kill with SIGKILL forcefully terminates an unresponsive orphaned process.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

async fn check_and_kill_orphaned_daemon(socket_path: &std::path::Path, verify_daemon: bool) {
    let json_path = uart_driver_api::get_metadata_path(socket_path);
    if !json_path.exists() {
        return;
    }
    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let metadata: ConnectionMetadata = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return,
    };
    let old_pid = metadata.pid;
    if old_pid == 0 {
        return;
    }
    let should_kill = if verify_daemon {
        ffx_tool_uart::is_driver_running(old_pid)
    } else {
        ffx_tool_uart::is_running(old_pid)
    };
    if should_kill {
        log::warn!(
            "Detected orphaned daemon with PID {} for target '{}' (socket is dead). Killing it...",
            old_pid,
            metadata.target
        );
        terminate_process(old_pid).await;
    }
}

/// Binds a [`UnixListener`] to `socket_path`, safely detecting and cleaning up stale socket files.
///
/// Attempts to connect to `socket_path` to verify whether an active daemon is running:
/// * If connection succeeds, the socket is actively in use, returning [`RemoveAndBindError::InUse`].
/// * If connection is refused, the socket is stale. If an associated metadata file indicates
///   an orphaned process, terminates the orphan and unlinks the stale socket before binding.
///
/// # Arguments
///
/// * `socket_path` - Filesystem path where the UNIX domain socket should be bound.
/// * `verify_daemon` - If `true`, verifies that any process recorded in the metadata file is an
///   actual `ffx-uart-driver` process before terminating it.
///
/// # Returns
///
/// The newly bound [`UnixListener`].
///
/// # Errors
///
/// Returns [`RemoveAndBindError`] if the socket is in active use, if removing a stale socket fails,
/// or if binding the listener fails.
pub async fn remove_and_bind_socket(
    socket_path: PathBuf,
    verify_daemon: bool,
) -> Result<UnixListener, RemoveAndBindError> {
    match UnixStream::connect(&socket_path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            check_and_kill_orphaned_daemon(&socket_path, verify_daemon).await;
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            check_and_kill_orphaned_daemon(&socket_path, verify_daemon).await;
            if let Err(e) = std::fs::remove_file(&socket_path) {
                return Err(RemoveAndBindError::RemoveStale(socket_path, e));
            }
        }
        Ok(_) => {
            return Err(RemoveAndBindError::InUse(socket_path));
        }
        Err(e) => {
            return Err(RemoveAndBindError::ConnectCheck(socket_path, e));
        }
    }

    match UnixListener::bind(&socket_path) {
        Ok(s) => Ok(s),
        Err(e) => Err(RemoveAndBindError::Bind(socket_path, e)),
    }
}

/// Events emitted by client channel readers/writers or the UNIX domain listener.
pub enum ClientEvent {
    /// A new client connected on `channel_id`.
    NewChannel { channel_id: u16, stream: UnixStream },
    /// Incoming data read from a client socket on `channel_id`.
    ChannelData { channel_id: u16, data: Vec<u8> },
    /// A client socket closed or encountered an EOF/error on `channel_id`.
    ChannelClose { channel_id: u16 },
}

/// Events emitted by the UART session reader or supervisor loop.
#[derive(Debug)]
pub enum UartEvent {
    /// Decoded data payload received from the UART link for `channel_id`.
    UartData { channel_id: u16, data: Vec<u8> },
    /// Remote close frame received from the UART link for `channel_id`.
    UartClose { channel_id: u16 },
    /// A new UART session was established with an outgoing `sender_tx` queue.
    UpdateSender { sender_tx: mpsc::Sender<SenderMessage> },
    /// The active UART session disconnected or failed.
    UartDown,
}

impl PartialEq for UartEvent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::UartData { channel_id: c1, data: d1 },
                Self::UartData { channel_id: c2, data: d2 },
            ) => c1 == c2 && d1 == d2,
            (Self::UartClose { channel_id: c1 }, Self::UartClose { channel_id: c2 }) => c1 == c2,
            (Self::UartDown, Self::UartDown) => true,
            _ => false,
        }
    }
}

/// Outgoing messages queued for transmission by [`sender_task`].
#[derive(Clone, Debug)]
pub enum SenderMessage {
    /// Data payload to frame and transmit on `channel_id`.
    Data { channel_id: u16, payload: Vec<u8> },
    /// Close notification frame to transmit on `channel_id`.
    Close { channel_id: u16 },
}

struct ClientState {
    writer_tx: mpsc::Sender<Vec<u8>>,
    _reader_task: fuchsia_async::Task<()>,
    _writer_task: fuchsia_async::Task<()>,
}

async fn dispatch_client_chunks(
    channel_id: u16,
    data: &[u8],
    coordinator_tx: &mut mpsc::Sender<ClientEvent>,
) -> bool {
    for chunk in data.chunks(uart_fpl::MAX_PAYLOAD_SIZE) {
        if coordinator_tx
            .send(ClientEvent::ChannelData { channel_id, data: chunk.to_vec() })
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn client_reader_task(
    channel_id: u16,
    mut reader: OwnedReadHalf,
    mut coordinator_tx: mpsc::Sender<ClientEvent>,
) {
    let mut buf = [0; 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // Drop the socket read half before awaiting `coordinator_tx.send` so the OS
                // descriptor is released immediately even if `coordinator_tx` is temporarily
                // backpressured.
                drop(reader);
                let _ = coordinator_tx.send(ClientEvent::ChannelClose { channel_id }).await;
                break;
            }
            Ok(n) => {
                log::trace!("CLIENT READ: channel={}, len={}", channel_id, n);
                if !dispatch_client_chunks(channel_id, &buf[..n], &mut coordinator_tx).await {
                    break;
                }
            }
        }
    }
}

const MAX_CLIENT_WRITE_BATCH_BYTES: usize = 64 * 1024;

async fn client_writer_task(
    channel_id: u16,
    mut writer: OwnedWriteHalf,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    mut coordinator_tx: mpsc::Sender<ClientEvent>,
) {
    while let Some(data) = receiver.next().await {
        let batch = collect_data_batch(data, &mut receiver, MAX_CLIENT_WRITE_BATCH_BYTES);
        if let Err(e) = writer.write_all(&batch).await {
            log::error!("Client {} writer failed: {:?}", channel_id, e);
            break;
        }
        if let Err(e) = writer.flush().await {
            log::error!("Client {} writer flush failed: {:?}", channel_id, e);
            break;
        }
    }
    log::debug!("Client {} writer task exiting", channel_id);
    // Explicitly drop `receiver` (`writer_rx`) and `writer` BEFORE awaiting `coordinator_tx.send`.
    // If the client socket breaks while both `state.writer_tx` (coordinator -> client_writer_task)
    // and `coordinator_tx` (client_writer_task -> coordinator) are simultaneously full, dropping
    // `receiver` first causes `state.writer_tx.send(data)` in `coordinator_task` to immediately
    // wake with a disconnected error, breaking the two-task wait cycle.
    drop(receiver);
    drop(writer);
    let _ = coordinator_tx.send(ClientEvent::ChannelClose { channel_id }).await;
}

fn spawn_client_channel(
    channel_id: u16,
    stream: UnixStream,
    client_tx: &mpsc::Sender<ClientEvent>,
) -> ClientState {
    let (reader, writer) = stream.into_split();
    let reader_task =
        fuchsia_async::Task::local(client_reader_task(channel_id, reader, client_tx.clone()));
    let (writer_tx, writer_rx) = mpsc::channel(CLIENT_WRITER_CHANNEL_CAPACITY);
    let writer_task = fuchsia_async::Task::local(client_writer_task(
        channel_id,
        writer,
        writer_rx,
        client_tx.clone(),
    ));
    ClientState { writer_tx, _reader_task: reader_task, _writer_task: writer_task }
}

async fn handle_uart_event(
    event: UartEvent,
    active_channels: &mut HashMap<u16, ClientState>,
    outgoing_queue: &mut VecDeque<SenderMessage>,
    sender_tx: &mut Option<mpsc::Sender<SenderMessage>>,
) {
    match event {
        UartEvent::UartData { channel_id, data } => {
            if let Some(state) = active_channels.get_mut(&channel_id) {
                // Backpressure & Deadlock Design:
                // Fast path: try_send handles uncontested channels synchronously with zero future allocation.
                // When full (e.g. at 1,000,000 baud with a slow consumer), awaiting state.writer_tx.send
                // intentionally pauses coordinator_task from draining serial_rx so receiver_task withholds
                // sequence advancement and throttles the target via Go-Back-N without dropping the socket.
                // While waiting, we continue polling poll_send_outgoing so outgoing_queue keeps draining.
                match state.writer_tx.try_send(data) {
                    Ok(()) => {}
                    Err(e) if e.is_disconnected() => {
                        log::warn!("Client {} writer disconnected", channel_id);
                        active_channels.remove(&channel_id);
                        outgoing_queue.push_back(SenderMessage::Close { channel_id });
                    }
                    Err(e) => {
                        let mut send_fut =
                            std::pin::pin!(state.writer_tx.send(e.into_inner()).fuse());
                        loop {
                            let sender_ready_fut = futures::future::poll_fn(|cx| {
                                poll_send_outgoing(outgoing_queue, sender_tx, cx)
                            });
                            futures::select_biased! {
                                _ = sender_ready_fut.fuse() => {}
                                res = send_fut => {
                                    if let Err(e) = res {
                                        log::warn!("Client {} writer disconnected: {:?}", channel_id, e);
                                        active_channels.remove(&channel_id);
                                        outgoing_queue.push_back(SenderMessage::Close { channel_id });
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        UartEvent::UartClose { channel_id } => {
            active_channels.remove(&channel_id);
        }
        UartEvent::UpdateSender { sender_tx: new_tx } => {
            *sender_tx = Some(new_tx);
        }
        UartEvent::UartDown => {
            log::warn!("UART connection went down, disconnecting all clients");
            active_channels.clear();
            outgoing_queue.clear();
            *sender_tx = None;
        }
    }
}

fn handle_client_event(
    event: ClientEvent,
    active_channels: &mut HashMap<u16, ClientState>,
    outgoing_queue: &mut VecDeque<SenderMessage>,
    sender_tx: &Option<mpsc::Sender<SenderMessage>>,
    client_tx: &mpsc::Sender<ClientEvent>,
) {
    match event {
        ClientEvent::NewChannel { channel_id, stream } => {
            if sender_tx.is_none() {
                log::warn!("Rejecting new channel {} because serial is down", channel_id);
                return;
            }
            let state = spawn_client_channel(channel_id, stream, client_tx);
            active_channels.insert(channel_id, state);
        }
        ClientEvent::ChannelData { channel_id, data } => {
            log::trace!("COORDINATOR: Client data channel={}, len={}", channel_id, data.len());
            outgoing_queue.push_back(SenderMessage::Data { channel_id, payload: data });
        }
        ClientEvent::ChannelClose { channel_id } => {
            if active_channels.remove(&channel_id).is_some() {
                outgoing_queue.push_back(SenderMessage::Close { channel_id });
            }
        }
    }
}

fn poll_send_outgoing(
    outgoing_queue: &mut VecDeque<SenderMessage>,
    sender_tx: &mut Option<mpsc::Sender<SenderMessage>>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<()> {
    if outgoing_queue.is_empty() {
        return std::task::Poll::Pending;
    }
    if let Some(tx) = sender_tx.as_mut() {
        match futures::sink::Sink::poll_ready(Pin::new(tx), cx) {
            std::task::Poll::Ready(Ok(())) => {
                let msg = outgoing_queue.pop_front().unwrap();
                let _ = futures::sink::Sink::start_send(Pin::new(tx), msg);
                std::task::Poll::Ready(())
            }
            std::task::Poll::Ready(Err(_)) => {
                outgoing_queue.clear();
                *sender_tx = None;
                std::task::Poll::Ready(())
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    } else {
        std::task::Poll::Pending
    }
}

/// Coordinates client UNIX socket channels, backpressure, and event multiplexing with the active UART link.
pub async fn coordinator_task(
    mut client_rx: mpsc::Receiver<ClientEvent>,
    mut serial_rx: mpsc::Receiver<UartEvent>,
    client_tx: mpsc::Sender<ClientEvent>,
    mut sender_tx: Option<mpsc::Sender<SenderMessage>>,
    metrics: Arc<Mutex<DaemonMetrics>>,
) -> Result<(), TaskError> {
    let mut active_channels = HashMap::<u16, ClientState>::new();
    let mut outgoing_queue = VecDeque::<SenderMessage>::new();
    loop {
        let q_len = outgoing_queue.len();
        // Host-to-Target Backpressure: Stop polling `client_rx` once `outgoing_queue` reaches
        // `DEFAULT_CLIENT_DATA_CAPACITY` so `client_reader_task` instances suspend on
        // `coordinator_tx.send(...)` and apply OS socket backpressure to fast host clients
        // when the UART TX window is saturated.
        let mut client_rx_fut = std::pin::pin!(
            if q_len < DEFAULT_CLIENT_DATA_CAPACITY {
                futures::future::Either::Left(client_rx.next())
            } else {
                futures::future::Either::Right(futures::future::pending())
            }
            .fuse()
        );
        let sender_ready_fut = futures::future::poll_fn(|cx| {
            poll_send_outgoing(&mut outgoing_queue, &mut sender_tx, cx)
        });
        // Use fair `futures::select!` rather than `select_biased!` so continuous high-baudrate
        // incoming UART frames on `serial_rx` cannot starve `sender_ready_fut` or `client_rx_fut`.
        futures::select! {
            event = serial_rx.next().fuse() => {
                let Some(event) = event else { break };
                handle_uart_event(event, &mut active_channels, &mut outgoing_queue, &mut sender_tx).await;
            }
            _ = sender_ready_fut.fuse() => {}
            event = client_rx_fut => {
                let Some(event) = event else { break };
                handle_client_event(event, &mut active_channels, &mut outgoing_queue, &sender_tx, &client_tx);
            }
        }
        metrics.lock().unwrap().outgoing_queue_len = outgoing_queue.len() as u32;
    }
    Ok(())
}

fn current_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct DeviceWriter<W> {
    device: W,
    rate_limit: Option<NonZeroU32>,
    next_allowed_time: std::time::Instant,
    metrics: Arc<Mutex<DaemonMetrics>>,
}

impl<W: AsyncWrite + Unpin> DeviceWriter<W> {
    async fn write_frame(&mut self, frame: &[u8]) -> Result<(), WriterTaskError> {
        if let Some(limit) = self.rate_limit {
            let now = std::time::Instant::now();
            if self.next_allowed_time > now {
                fuchsia_async::Timer::new(self.next_allowed_time - now).await;
            }
            let frame_duration =
                std::time::Duration::from_secs_f64(frame.len() as f64 / limit.get() as f64);
            self.next_allowed_time = std::cmp::max(now, self.next_allowed_time) + frame_duration;
        }
        log::trace!("WRITER: writing frame len={}", frame.len());
        self.device.write_all(frame).await.map_err(WriterTaskError::Write)?;
        self.device.flush().await.map_err(WriterTaskError::Flush)?;
        self.metrics.lock().unwrap().last_write_timestamp_ms = current_epoch_ms();
        Ok(())
    }

    async fn send_ack(&mut self, sid: u32, seq: u8) -> Result<(), WriterTaskError> {
        let ack_frame = encode_frame(sid, CONTROL_CHANNEL_ID, seq, FrameType::Ack, &[])
            .map_err(WriterTaskError::EncodeAck)?;
        self.write_frame(&ack_frame).await
    }
}

fn collect_data_batch(
    first_frame: Vec<u8>,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    max_bytes: usize,
) -> Vec<u8> {
    let mut batch = first_frame;
    while let Ok(next_frame) = rx.try_recv() {
        batch.extend_from_slice(&next_frame);
        if batch.len() >= max_bytes {
            break;
        }
    }
    batch
}

/// Transmits outgoing data frames and cumulative ACKs to the UART device with batching and rate limiting.
pub async fn writer_task<W: AsyncWrite + Unpin>(
    device: W,
    ack_tracker: AckTracker,
    mut data_rx: mpsc::Receiver<Vec<u8>>,
    metrics: Arc<Mutex<DaemonMetrics>>,
    rate_limit: Option<NonZeroU32>,
) -> Result<(), TaskError> {
    let mut writer =
        DeviceWriter { device, rate_limit, next_allowed_time: std::time::Instant::now(), metrics };
    loop {
        // Prioritize cumulative ACKs ahead of outgoing data batches so the remote Go-Back-N
        // sender receives timely window advances (or duplicate-ACK gap notifications after a
        // data glitch) without queuing behind bulk data frames.
        if let Some((sid, seq)) = ack_tracker.take_ack() {
            writer.send_ack(sid, seq).await.map_err(TaskError::Writer)?;
            continue;
        }
        if futures::stream::FusedStream::is_terminated(&data_rx) {
            break;
        }
        futures::select_biased! {
            (sid, seq) = ack_tracker.wait_ack().fuse() => {
                writer.send_ack(sid, seq).await.map_err(TaskError::Writer)?;
            }
            frame = data_rx.next() => {
                let Some(frame) = frame else { break };
                let batch = collect_data_batch(frame, &mut data_rx, MAX_UART_WRITE_BATCH_BYTES);
                writer.write_frame(&batch).await.map_err(TaskError::Writer)?;
            }
        }
    }
    Ok(())
}

/// Buffered asynchronous stream reader that extracts framed [`Frame`] packets from a UART byte stream.
pub struct UartReader<R: AsyncRead + Unpin> {
    client: R,
    parser: FrameParser,
    buf: Box<[u8; UART_READ_BUFFER_SIZE]>,
}

impl<R: AsyncRead + Unpin> UartReader<R> {
    /// Creates a new [`UartReader`] wrapping `client` with an empty initial buffer.
    pub fn new(client: R) -> Self {
        Self::with_initial_data(client, Vec::new())
    }

    /// Creates a new [`UartReader`] seeded with `initial_data` remaining from a prior handshake.
    pub fn with_initial_data(client: R, initial_data: Vec<u8>) -> Self {
        let mut parser = FrameParser::new();
        if !initial_data.is_empty() {
            parser.feed(&initial_data);
        }
        Self { client, parser, buf: Box::new([0u8; UART_READ_BUFFER_SIZE]) }
    }

    async fn drain_available(&mut self) {
        poll_fn(|cx| {
            loop {
                let mut read_buf = tokio::io::ReadBuf::new(&mut self.buf[..]);
                match Pin::new(&mut self.client).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) if !read_buf.filled().is_empty() => {
                        let n = read_buf.filled().len();
                        self.parser.feed(&self.buf[..n]);
                    }
                    _ => return Poll::Ready(()),
                }
            }
        })
        .await;
    }

    /// Reads from the underlying stream until a complete, checksum-verified [`Frame`] is decoded.
    pub async fn next_frame(
        &mut self,
        metrics: &Arc<Mutex<DaemonMetrics>>,
    ) -> Result<Frame, ReaderError> {
        loop {
            self.drain_available().await;
            let frame = self.parser.next_frame();
            metrics.lock().unwrap().checksum_errors = self.parser.checksum_errors();
            if let Some(frame) = frame {
                return Ok(frame);
            }
            let n = self.client.read(&mut self.buf[..]).await.map_err(ReaderError::Read)?;
            if n == 0 {
                return Err(ReaderError::Eof);
            }
            metrics.lock().unwrap().last_read_timestamp_ms = current_epoch_ms();
            self.parser.feed(&self.buf[..n]);
        }
    }
}

fn dispatch_ordered_event(
    session_id: u32,
    channel_id: u16,
    seq: u8,
    event_name: &'static str,
    event: UartEvent,
    receiver: &mut ResendReceiver,
    ack_tracker: &AckTracker,
    serial_tx: &mut mpsc::Sender<UartEvent>,
) -> Result<(), ReceiverTaskError> {
    // Go-Back-N Data Glitch & Loss Recovery:
    // If a UART wire glitch corrupts a frame (discarded by `FrameParser` via Fletcher-16 checksum)
    // or drops bytes, subsequent frames in the remote sender's window arrive with `seq != expected_seq`
    // (`FrameStatus::OutOfOrder`). We discard out-of-order/duplicate frames without advancing
    // `ResendReceiver` and immediately re-assert `ack_tracker.set_ack(rx_session_id, ack_seq)` for
    // the highest contiguous sequence number received prior to the glitch. This ensures the remote
    // `ResendSender` advances its window up to the last good frame and Go-Back-N retransmits
    // starting at `expected_seq`.
    //
    // Non-blocking `try_send` is critical here for two reasons:
    // 1. Deadlock Prevention: `receiver_task` must never suspend while forwarding `DATA` or
    //    `CLOSE` events to `coordinator_task`, so it remains free to read incoming `FrameType::Ack`
    //    and `FrameType::Reset` control frames from the UART even when `coordinator_task` is
    //    backpressured on a slow client socket.
    // 2. End-to-End Go-Back-N Backpressure: When `serial_tx` is full (`e.is_full()`), we drop the
    //    frame *without* calling `receiver.advance_in_order(seq)` and re-advertise the last
    //    contiguous `current_ack_seq()`. The remote `ResendSender` sees that its window has stopped
    //    advancing, pauses new transmissions once its window fills, and cleanly retransmits from
    //    `expected_seq` once `coordinator_task` drains `serial_rx`.
    let ack_seq = match receiver.inspect(seq) {
        FrameStatus::InOrder { .. } => match serial_tx.try_send(event) {
            Ok(()) => Some(receiver.advance_in_order(seq).map_err(ReceiverTaskError::AdvanceSeq)?),
            Err(e) if e.is_full() => {
                log::warn!(
                    "Coordinator queue full for channel {}; dropping in-order {} seq={}",
                    channel_id,
                    event_name,
                    seq
                );
                receiver.current_ack_seq()
            }
            Err(e) => {
                return Err(ReceiverTaskError::CoordinatorClosed(event_name, format!("{e:?}")));
            }
        },
        FrameStatus::OutOfOrder { expected_seq, ack_seq, .. } => {
            log::trace!(
                "Duplicate/Out-of-order {} frame, seq={}, expected={}",
                event_name,
                seq,
                expected_seq
            );
            ack_seq
        }
    };
    if let Some(seq) = ack_seq {
        ack_tracker.set_ack(session_id, seq);
    }
    Ok(())
}

fn process_received_frame(
    frame: Frame,
    receiver: &mut ResendReceiver,
    ack_tracker: &AckTracker,
    serial_tx: &mut mpsc::Sender<UartEvent>,
    incoming_event_tx: &mut mpsc::Sender<u8>,
) -> Result<(), ReceiverTaskError> {
    match frame.frame_type {
        FrameType::Data => {
            log::trace!(
                "Received DATA frame, seq={}, channel={}, len={}",
                frame.seq,
                frame.channel_id,
                frame.payload.len()
            );
            let ev = UartEvent::UartData { channel_id: frame.channel_id, data: frame.payload };
            dispatch_ordered_event(
                frame.session_id,
                frame.channel_id,
                frame.seq,
                "DATA",
                ev,
                receiver,
                ack_tracker,
                serial_tx,
            )?;
        }
        FrameType::Ack => {
            log::trace!("Received ACK frame, seq={}", frame.seq);
            // Use non-blocking `try_send` so `process_received_frame` is purely synchronous and
            // can never suspend `receiver_task`. Because FPL ACKs are cumulative, if `incoming_event_tx`
            // is momentarily full, any subsequent ACK supersedes earlier ones.
            if let Err(e) = incoming_event_tx.try_send(frame.seq) {
                if e.is_full() {
                    log::warn!("Sender ACK queue full, dropping cumulative ACK seq={}", frame.seq);
                } else {
                    return Err(ReceiverTaskError::EnqueueAck(e.into_send_error()));
                }
            }
        }
        FrameType::Close => {
            log::trace!("Received CLOSE frame, seq={}, channel={}", frame.seq, frame.channel_id);
            let ev = UartEvent::UartClose { channel_id: frame.channel_id };
            dispatch_ordered_event(
                frame.session_id,
                frame.channel_id,
                frame.seq,
                "CLOSE",
                ev,
                receiver,
                ack_tracker,
                serial_tx,
            )?;
        }
        _ => log::warn!("Unknown frame type: {}", u8::from(frame.frame_type)),
    }
    Ok(())
}

async fn run_receiver_loop<R: AsyncRead + Unpin>(
    reader: &mut UartReader<R>,
    serial_tx: &mut mpsc::Sender<UartEvent>,
    ack_tracker: &AckTracker,
    incoming_event_tx: &mut mpsc::Sender<u8>,
    session_id: u32,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) -> Result<(), ReceiverTaskError> {
    let mut receiver = ResendReceiver::new();
    loop {
        let frame = reader.next_frame(metrics).await?;
        if frame.frame_type == FrameType::Reset
            || (frame.frame_type == FrameType::Close && frame.channel_id == CONTROL_CHANNEL_ID)
        {
            log::info!("Received RESET frame from target. Restarting UART driver...");
            return Err(ReceiverTaskError::TargetRequestedReset);
        }
        if frame.session_id != session_id {
            log::warn!(
                "Received frame with mismatched session ID (got {}, expected {}), discarding",
                frame.session_id,
                session_id
            );
            continue;
        }
        process_received_frame(frame, &mut receiver, ack_tracker, serial_tx, incoming_event_tx)?;
        // Yield to the executor to allow other tasks on the single-threaded runtime (such as
        // coordinator_task consuming serial_tx and writer_task transmitting cumulative ACKs)
        // to make progress when a single I/O read chunk delivers multiple parsed frames
        // without suspension.
        fuchsia_async::yield_now().await;
    }
}

/// Demultiplexes incoming UART frames using [`uart_fpl::ResendReceiver`] and generates cumulative ACKs.
pub async fn receiver_task<R: AsyncRead + Unpin>(
    mut reader: UartReader<R>,
    mut serial_tx: mpsc::Sender<UartEvent>,
    ack_tracker: AckTracker,
    mut incoming_event_tx: mpsc::Sender<u8>,
    session_id: u32,
    metrics: Arc<Mutex<DaemonMetrics>>,
) -> Result<(), TaskError> {
    run_receiver_loop(
        &mut reader,
        &mut serial_tx,
        &ack_tracker,
        &mut incoming_event_tx,
        session_id,
        &metrics,
    )
    .await
    .map_err(TaskError::Receiver)
}

fn handle_sender_ack(
    ack_seq: u8,
    sender: &mut ResendSender,
    active_timer: &mut Option<fuchsia_async::Timer>,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) {
    if let AckOutcome::Advanced { rtt, all_acked, .. } = sender.handle_ack(ack_seq) {
        if let Some(rtt) = rtt {
            let mut m = metrics.lock().unwrap();
            m.estimated_rtt_ms = rtt.as_millis() as u32;
        }
        if all_acked {
            *active_timer = None;
        } else {
            *active_timer = Some(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT));
        }
    }
}

async fn send_data_frame(
    frame: Vec<u8>,
    sender: &mut ResendSender,
    active_timer: &mut Option<fuchsia_async::Timer>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    incoming_event_rx: &mut mpsc::Receiver<u8>,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) -> Result<(), SenderTaskError> {
    // Bidirectional Deadlock Prevention:
    // When the UART writer path is slow or rate-limited (`data_tx` is full), `sender_task`
    // suspends while pushing outgoing or retransmitted frames into `data_tx`. We must continue
    // polling `incoming_event_rx` via `select_biased!` while `data_tx.send(frame)` is pending so
    // incoming cumulative ACKs are drained immediately, advancing the Go-Back-N window and
    // resetting/clearing `active_timer` rather than stalling behind `data_tx`.
    let mut send_fut = std::pin::pin!(data_tx.send(frame).fuse());
    loop {
        futures::select_biased! {
            ack_event = incoming_event_rx.next().fuse() => match ack_event {
                Some(seq) => handle_sender_ack(seq, sender, active_timer, metrics),
                None => return Err(SenderTaskError::AckChannelClosed),
            },
            res = send_fut => {
                return res.map_err(|_| SenderTaskError::WriterClosed);
            }
        }
    }
}

async fn handle_sender_timeout(
    sender: &mut ResendSender,
    active_timer: &mut Option<fuchsia_async::Timer>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    incoming_event_rx: &mut mpsc::Receiver<u8>,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) -> Result<(), SenderTaskError> {
    let retransmit_frames =
        sender.handle_timeout().map_err(SenderTaskError::RetransmissionLimit)?;
    {
        let mut m = metrics.lock().unwrap();
        m.retransmissions += 1;
    }
    log::trace!(
        "Go-Back-N timeout! Retransmitting window base={}, next_seq={}, attempts={}",
        sender.base(),
        sender.next_seq(),
        sender.retransmission_attempts()
    );

    for frame in retransmit_frames {
        // Because `send_data_frame` continues processing incoming ACKs while waiting on `data_tx`,
        // a cumulative ACK arriving mid-burst can acknowledge the entire in-flight window. Stop
        // retransmitting immediately if `sender.is_idle()` becomes true.
        if sender.is_idle() {
            break;
        }
        send_data_frame(frame, sender, active_timer, data_tx, incoming_event_rx, metrics).await?;
    }

    if !sender.is_idle() {
        *active_timer = Some(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT));
    }
    Ok(())
}

async fn send_next_message(
    msg: SenderMessage,
    sender: &mut ResendSender,
    active_timer: &mut Option<fuchsia_async::Timer>,
    data_tx: &mut mpsc::Sender<Vec<u8>>,
    incoming_event_rx: &mut mpsc::Receiver<u8>,
    sid: u32,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) -> Result<(), SenderTaskError> {
    let (frame_type, channel_id, payload) = match msg {
        SenderMessage::Data { channel_id, payload } => (FrameType::Data, channel_id, payload),
        SenderMessage::Close { channel_id } => (FrameType::Close, channel_id, Vec::new()),
    };

    let (_seq, frame) = sender
        .enqueue_frame(sid, channel_id, frame_type, &payload)
        .map_err(SenderTaskError::EncodeFrame)?;

    // Arm the retransmission timer before calling `send_data_frame` so that if an ACK for this
    // frame arrives while `send_data_frame` is waiting for `data_tx` to flush, `handle_sender_ack`
    // will cleanly clear `active_timer` back to `None`.
    if sender.in_flight() == 1 {
        *active_timer = Some(fuchsia_async::Timer::new(DEFAULT_RETRANSMISSION_TIMEOUT));
    }

    send_data_frame(frame, sender, active_timer, data_tx, incoming_event_rx, metrics).await?;
    Ok(())
}

async fn wait_active_timer(active_timer: &mut Option<fuchsia_async::Timer>) {
    if let Some(timer) = active_timer {
        timer.await;
    } else {
        futures::future::pending::<()>().await;
    }
}

async fn run_sender_loop(
    mut sender_rx: mpsc::Receiver<SenderMessage>,
    mut data_tx: mpsc::Sender<Vec<u8>>,
    mut incoming_event_rx: mpsc::Receiver<u8>,
    session_id: u32,
    metrics: &Arc<Mutex<DaemonMetrics>>,
) -> Result<(), SenderTaskError> {
    let mut sender = ResendSender::default();
    let mut active_timer: Option<fuchsia_async::Timer> = None;
    let mut sender_rx_closed = false;
    loop {
        let mut msg_rx_fut = std::pin::pin!(if sender.can_send() && !sender_rx_closed {
            futures::future::Either::Left(sender_rx.next())
        } else {
            futures::future::Either::Right(futures::future::pending())
        });
        futures::select_biased! {
            ack_event = incoming_event_rx.next().fuse() => match ack_event {
                Some(seq) => handle_sender_ack(seq, &mut sender, &mut active_timer, metrics),
                None => return Err(SenderTaskError::AckChannelClosed),
            },
            _ = wait_active_timer(&mut active_timer).fuse() => {
                handle_sender_timeout(
                    &mut sender,
                    &mut active_timer,
                    &mut data_tx,
                    &mut incoming_event_rx,
                    metrics,
                )
                .await?;
            }
            msg = msg_rx_fut => match msg {
                Some(m) => {
                    send_next_message(
                        m,
                        &mut sender,
                        &mut active_timer,
                        &mut data_tx,
                        &mut incoming_event_rx,
                        session_id,
                        metrics,
                    )
                    .await?
                }
                None => sender_rx_closed = true,
            },
        }
        if sender_rx_closed && sender.is_idle() {
            break;
        }
    }
    Ok(())
}

/// Packetizes outgoing messages and manages sliding-window Go-Back-N retransmission via [`uart_fpl::ResendSender`].
pub async fn sender_task(
    sender_rx: mpsc::Receiver<SenderMessage>,
    data_tx: mpsc::Sender<Vec<u8>>,
    incoming_event_rx: mpsc::Receiver<u8>,
    session_id: u32,
    metrics: Arc<Mutex<DaemonMetrics>>,
) -> Result<(), TaskError> {
    run_sender_loop(sender_rx, data_tx, incoming_event_rx, session_id, &metrics)
        .await
        .map_err(TaskError::Sender)
}

/// Host-side driver coordinator managing daemon background execution and hardware state.
///
/// Supervises client connections, performs handshake negotiation, queries target identity
/// over FDomain Remote Control Service (RCS), and synchronizes connection metadata for
/// tool discovery.
pub struct HostDriver;

impl HostDriver {
    fn update_status(meta_path: &Option<PathBuf>, status: ConnectionStatus) {
        let Some(path) = meta_path else { return };
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to read metadata at {}: {:?}", path.display(), e);
                return;
            }
        };
        let mut meta = match serde_json::from_str::<ConnectionMetadata>(&content) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to parse metadata at {}: {:?}", path.display(), e);
                return;
            }
        };
        meta.status = status;
        if meta.status != ConnectionStatus::Connected {
            meta.nodename = None;
            meta.serial = None;
        }
        if let Ok(new_content) = serde_json::to_string(&meta) {
            let temp_path = path.with_extension(format!("{}.tmp", meta.pid));
            if std::fs::write(&temp_path, new_content.as_bytes()).is_ok() {
                let _ = std::fs::rename(&temp_path, path);
            }
        }
    }

    async fn identify_host_over_client(
        client: &Arc<fdomain_client::Client>,
    ) -> Result<(Option<String>, Option<String>), IdentityError> {
        let (proxy, server_end) = client.create_proxy::<rcs_fdomain::RemoteControlMarker>();
        let ns = client.namespace().await.map_err(IdentityError::Namespace)?;
        let ns = fio_fdomain::DirectoryProxy::new(ns);
        ns.open(
            rcs_fdomain::RemoteControlMarker::PROTOCOL_NAME,
            fio_fdomain::Flags::PROTOCOL_SERVICE,
            &fio_fdomain::Options::default(),
            server_end.into_channel(),
        )
        .map_err(IdentityError::OpenRcs)?;

        let identify_res = proxy
            .identify_host()
            .await
            .map_err(IdentityError::Fidl)?
            .map_err(IdentityError::Identify)?;

        Ok((identify_res.nodename, identify_res.serial_number))
    }

    async fn query_identity_once(
        sock_path: &std::path::Path,
    ) -> Result<(Option<String>, Option<String>), IdentityError> {
        let stream = UnixStream::connect(sock_path)
            .await
            .map_err(|e| IdentityError::Connect(sock_path.to_path_buf(), e))?;
        let (read_half, write_half) = tokio::io::split(stream);
        let transport = FDomainTransport::new(
            Box::new(tokio::io::BufReader::new(read_half)),
            Box::new(write_half),
        );
        let (client, transport_fut) = fdomain_client::Client::new(transport);

        futures::select! {
            res = Self::identify_host_over_client(&client).fuse() => res,
            _ = transport_fut.fuse() => Err(IdentityError::TransportClosed),
            _ = fuchsia_async::Timer::new(IDENTITY_QUERY_TIMEOUT).fuse() => {
                Err(IdentityError::Timeout)
            }
        }
    }

    async fn query_and_update_identity(sock_path: &std::path::Path) {
        for attempt in 1..=MAX_IDENTITY_ATTEMPTS {
            log::info!(
                "Attempting to query target identity over FDomain (attempt {attempt}/{MAX_IDENTITY_ATTEMPTS})..."
            );
            match Self::query_identity_once(sock_path).await {
                Ok((nodename, serial)) => {
                    log::info!(
                        "Discovered target identity: nodename={nodename:?}, serial={serial:?}"
                    );
                    let _ = uart_driver_api::update_metadata_identity(sock_path, nodename, serial);
                    return;
                }
                Err(e) => {
                    log::debug!("Query identity attempt {attempt} failed: {:?}", e);
                    if attempt < MAX_IDENTITY_ATTEMPTS {
                        fuchsia_async::Timer::new(IDENTITY_RETRY_INTERVAL).await;
                    }
                }
            }
        }
        log::warn!("Could not determine target identity after {MAX_IDENTITY_ATTEMPTS} attempts");
    }

    fn watch_metadata_deletion(
        meta_path: &Option<PathBuf>,
        shutdown_tx: futures::channel::oneshot::Sender<()>,
    ) -> Option<fuchsia_async::Task<()>> {
        let path = meta_path.as_ref()?.clone();
        Some(fuchsia_async::Task::local(async move {
            loop {
                fuchsia_async::Timer::new(METADATA_CHECK_INTERVAL).await;
                if !path.exists() {
                    log::info!(
                        "Metadata file {} was deleted. Triggering shutdown.",
                        path.display()
                    );
                    let _ = shutdown_tx.send(());
                    break;
                }
            }
        }))
    }

    pub fn cleanup_driver_files(
        local_addr: Option<tokio::net::unix::SocketAddr>,
        control_socket_path: Option<&PathBuf>,
        meta_path: Option<&PathBuf>,
    ) {
        if let Some(path) = local_addr.as_ref().and_then(|a| a.as_pathname()) {
            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("Failed to remove socket file at {}: {:?}", path.display(), e);
            }
        }
        if let Some(path) = control_socket_path {
            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("Failed to remove control socket file at {}: {:?}", path.display(), e);
            }
        }
        if let Some(path) = meta_path {
            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("Failed to remove metadata file at {}: {:?}", path.display(), e);
            }
        }
    }

    async fn serve_control_listener(
        scope: &fuchsia_async::Scope,
        listener: UnixListener,
        metrics: Arc<Mutex<DaemonMetrics>>,
    ) {
        while let Ok((mut stream, _)) = listener.accept().await {
            let metrics = Arc::clone(&metrics);
            scope.spawn(async move {
                let json = {
                    let m = metrics.lock().unwrap_or_else(|e| e.into_inner());
                    serde_json::to_string(&*m).unwrap_or_default()
                };
                if let Err(e) = stream.write_all(json.as_bytes()).await {
                    log::debug!("Failed to write to control stream: {:?}", e);
                }
                let _ = stream.shutdown().await;
            });
        }
    }

    fn setup_control_socket(
        local_addr: Option<&tokio::net::unix::SocketAddr>,
        metrics: Arc<Mutex<DaemonMetrics>>,
    ) -> (Option<PathBuf>, Option<fuchsia_async::Task<()>>) {
        let Some(path) = local_addr.and_then(|a| a.as_pathname()) else {
            return (None, None);
        };
        let control_path = uart_driver_api::get_control_socket_path(path);
        if let Err(e) = std::fs::remove_file(&control_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "Failed to remove stale control socket at {}: {:?}",
                    control_path.display(),
                    e
                );
            }
        }
        match UnixListener::bind(&control_path) {
            Ok(listener) => {
                log::info!("Control socket listening on: {}", control_path.display());
                let task = fuchsia_async::Task::local(async move {
                    let scope = fuchsia_async::Scope::new();
                    Self::serve_control_listener(&scope, listener, metrics).await;
                });
                (Some(control_path), Some(task))
            }
            Err(e) => {
                log::error!("Failed to bind control socket at {}: {:?}", control_path.display(), e);
                (None, None)
            }
        }
    }
    fn spawn_listener_task(
        listener: UnixListener,
        listener_tx: mpsc::Sender<ClientEvent>,
    ) -> fuchsia_async::Task<()> {
        fuchsia_async::Task::local(async move {
            let mut channel_counter = 1u16;
            while let Ok((stream, _addr)) = listener.accept().await {
                let channel_id = channel_counter;
                channel_counter = channel_counter.wrapping_add(1).max(1);
                log::debug!("New client connection, assigning channel_id={}", channel_id);
                if let Err(e) =
                    listener_tx.clone().send(ClientEvent::NewChannel { channel_id, stream }).await
                {
                    log::error!("Failed to send NewChannel to coordinator: {:?}", e);
                    break;
                }
            }
        })
    }

    fn check_fatal_connect_error(
        e: &ConnectionError,
        target_path: &str,
        no_retry: bool,
        is_socket: bool,
        associated_peer_pid: Option<u32>,
    ) -> bool {
        if no_retry {
            log::error!("Connection failed and no-retry is set. Exiting driver: {e}");
            return true;
        }
        if let Some(pid) = associated_peer_pid {
            if !ffx_tool_uart::is_running(pid) {
                log::info!("Associated peer (PID {pid}) has exited. Exiting driver.");
                return true;
            }
        }
        if matches!(e, ConnectionError::PathNotFound { .. }) {
            if is_socket {
                log::info!(
                    "Virtual socket target path {target_path} does not exist. Exiting driver."
                );
                return true;
            }
            log::warn!("Target UART port {target_path} not found. Waiting for device...");
        }
        false
    }

    fn handle_handshake_success(
        proto: uart_fpl::ProtocolId,
        meta_path: &Option<PathBuf>,
        metrics: &Arc<Mutex<DaemonMetrics>>,
    ) {
        let now = current_epoch_ms();
        {
            let mut m = metrics.lock().unwrap();
            m.last_read_timestamp_ms = now;
            m.last_write_timestamp_ms = now;
            m.active_protocol = match proto {
                uart_fpl::ProtocolId::ResendSP => UartProtocol::ResendSP,
                uart_fpl::ProtocolId::Unknown(_) => UartProtocol::Unknown,
            };
        }
        log::info!("Handshake succeeded. Negotiated protocol: {proto:?}");
        Self::update_status(meta_path, ConnectionStatus::Connected);
    }

    fn spawn_coordinator_and_listener(
        listener: UnixListener,
        metrics: Arc<Mutex<DaemonMetrics>>,
    ) -> (mpsc::Sender<UartEvent>, fuchsia_async::Task<()>, fuchsia_async::Task<()>) {
        let (client_tx, client_rx) = mpsc::channel(DEFAULT_CLIENT_DATA_CAPACITY);
        let (serial_tx, serial_rx) = mpsc::channel(DEFAULT_SERIAL_DATA_CAPACITY);

        let coordinator_client_tx = client_tx.clone();
        let coordinator_task = fuchsia_async::Task::local(async move {
            if let Err(e) =
                coordinator_task(client_rx, serial_rx, coordinator_client_tx, None, metrics).await
            {
                log::error!("Coordinator failed: {:?}", e);
            }
        });
        let listener_task = Self::spawn_listener_task(listener, client_tx);
        (serial_tx, coordinator_task, listener_task)
    }

    async fn try_connect_uart(
        target_path: &str,
        baud: NonZeroU32,
        associated_peer_pid: &mut Option<u32>,
    ) -> Result<UartStream, ConnectionError> {
        let s = connect_uart_stream(target_path, baud).await?;
        if let UartStream::Socket(ref unix_stream) = s {
            if let Ok(pid) = ffx_tool_uart::get_peer_pid(unix_stream) {
                log::info!("Detected peer PID: {}", pid);
                *associated_peer_pid = Some(pid);
            }
        }
        Ok(s)
    }

    fn resolve_client_socket_path(
        local_addr: Option<&tokio::net::unix::SocketAddr>,
        meta_path: Option<&PathBuf>,
    ) -> Option<PathBuf> {
        local_addr
            .and_then(|a| a.as_pathname().map(|p| p.to_path_buf()))
            .or_else(|| meta_path.map(|p| uart_driver_api::get_client_socket_path(p)))
    }

    fn determine_target_properties(
        target_path: &str,
        baud: NonZeroU32,
    ) -> (bool, Option<NonZeroU32>) {
        let is_socket =
            std::fs::metadata(target_path).map(|m| m.file_type().is_socket()).unwrap_or(false);
        let is_pty = ffx_tool_uart::is_pty_target(target_path);
        let rate_limit = if !is_char_device(target_path) || is_pty {
            NonZeroU32::new(baud.get() / SERIAL_8N1_BITS_PER_BYTE)
        } else {
            None
        };
        log::info!("UART rate limit: {:?}", rate_limit);
        (is_socket, rate_limit)
    }

    /// Runs the main host driver supervision loop until cancelled or terminated.
    pub async fn run(
        listener: UnixListener,
        target_path: String,
        baud: NonZeroU32,
        meta_path: Option<PathBuf>,
        no_retry: bool,
    ) {
        let local_addr = listener.local_addr().ok();
        let client_sock_path =
            Self::resolve_client_socket_path(local_addr.as_ref(), meta_path.as_ref());
        let (is_socket, rate_limit) = Self::determine_target_properties(&target_path, baud);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));
        let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel::<()>();
        let _meta_watcher = Self::watch_metadata_deletion(&meta_path, shutdown_tx);
        let (control_sock, control_task) =
            Self::setup_control_socket(local_addr.as_ref(), metrics.clone());
        let ctx = DriverContext {
            target_path,
            baud,
            rate_limit,
            meta_path: meta_path.clone(),
            no_retry,
            is_socket,
            client_sock_path,
            metrics,
        };
        let main_loop_fut = ctx.run_reconnection_loop(listener).fuse();
        let shutdown_rx = shutdown_rx.fuse();
        futures::pin_mut!(main_loop_fut, shutdown_rx);
        futures::select! {
            _ = main_loop_fut => {},
            _ = shutdown_rx => log::info!("Driver shutdown triggered by metadata deletion."),
        }
        Self::cleanup_driver_files(local_addr, control_sock.as_ref(), meta_path.as_ref());
        drop(control_task);
    }
}

struct DriverContext {
    target_path: String,
    baud: NonZeroU32,
    rate_limit: Option<NonZeroU32>,
    meta_path: Option<PathBuf>,
    no_retry: bool,
    is_socket: bool,
    client_sock_path: Option<PathBuf>,
    metrics: Arc<Mutex<DaemonMetrics>>,
}

impl DriverContext {
    async fn record_failure_and_backoff(
        &self,
        err: ConnectionError,
        serial_tx: &mpsc::Sender<UartEvent>,
        is_handshake_fail: bool,
        is_drop: bool,
    ) {
        HostDriver::update_status(&self.meta_path, ConnectionStatus::Error(err));
        let _ = serial_tx.clone().send(UartEvent::UartDown).await;
        {
            let mut m = self.metrics.lock().unwrap();
            m.active_protocol = UartProtocol::Unknown;
            m.estimated_rtt_ms = 0;
            if is_handshake_fail {
                m.handshake_failures += 1;
            }
            if is_drop {
                m.connection_drops += 1;
            }
        }
        fuchsia_async::Timer::new(RECONNECT_BACKOFF_INTERVAL).await;
    }

    async fn perform_handshake(
        &self,
        stream: &mut UartStream,
        serial_tx: &mpsc::Sender<UartEvent>,
    ) -> Option<(uart_fpl::ProtocolId, u32, Vec<u8>)> {
        let proposed = vec![uart_fpl::ProtocolId::ResendSP];
        match ffx_tool_uart::stream::run_host_handshake(
            stream,
            proposed,
            HANDSHAKE_TIMEOUT,
            HANDSHAKE_RETRIES,
        )
        .await
        {
            Ok((proto, sid, unconsumed)) => {
                HostDriver::handle_handshake_success(proto, &self.meta_path, &self.metrics);
                Some((proto, sid, unconsumed))
            }
            Err(e) => {
                log::error!(
                    "Handshake failed: {e:?}. Retrying in {:?}...",
                    RECONNECT_BACKOFF_INTERVAL
                );
                let err = ConnectionError::raw(format!("Handshake failed: {e:?}"));
                self.record_failure_and_backoff(err, serial_tx, true, false).await;
                None
            }
        }
    }

    async fn run_session_tasks(
        &self,
        stream: UartStream,
        unconsumed: Vec<u8>,
        session_id: u32,
        serial_tx: &mpsc::Sender<UartEvent>,
    ) {
        let ack_tracker = AckTracker::new();
        let (data_tx, data_rx) = mpsc::channel(DEFAULT_SERIAL_DATA_CAPACITY);
        let (incoming_event_tx, incoming_event_rx) = mpsc::channel(DEFAULT_SERIAL_DATA_CAPACITY);
        let (sender_tx, sender_rx) = mpsc::channel(DEFAULT_SERIAL_DATA_CAPACITY);
        if let Err(e) = serial_tx.clone().send(UartEvent::UpdateSender { sender_tx }).await {
            return log::error!("Failed to send UpdateSender to coordinator: {:?}", e);
        }
        let (serial_read, serial_write) = tokio::io::split(stream);
        let writer = writer_task(
            serial_write,
            ack_tracker.clone(),
            data_rx,
            self.metrics.clone(),
            self.rate_limit,
        );
        let reader = UartReader::with_initial_data(serial_read, unconsumed);
        let receiver = receiver_task(
            reader,
            serial_tx.clone(),
            ack_tracker,
            incoming_event_tx,
            session_id,
            self.metrics.clone(),
        );
        let sender =
            sender_task(sender_rx, data_tx, incoming_event_rx, session_id, self.metrics.clone());
        log::info!("Starting UART tasks for session_id={session_id}");
        if let Err(e) = futures::try_join!(writer, receiver, sender) {
            log::error!("UART tasks failed: {:?}", e);
        }
    }

    async fn connect_uart(
        &self,
        serial_tx: &mpsc::Sender<UartEvent>,
        associated_peer_pid: &mut Option<u32>,
    ) -> Result<UartStream, bool> {
        log::info!("Connecting to target UART port at {}...", self.target_path);
        HostDriver::update_status(&self.meta_path, ConnectionStatus::Connecting);
        match HostDriver::try_connect_uart(&self.target_path, self.baud, associated_peer_pid).await
        {
            Ok(s) => Ok(s),
            Err(e) => {
                if HostDriver::check_fatal_connect_error(
                    &e,
                    &self.target_path,
                    self.no_retry,
                    self.is_socket,
                    *associated_peer_pid,
                ) {
                    HostDriver::update_status(&self.meta_path, ConnectionStatus::Error(e));
                    return Err(true);
                }
                log::error!(
                    "Failed to connect to UART port: {e}. Retrying in {:?}...",
                    RECONNECT_BACKOFF_INTERVAL
                );
                self.record_failure_and_backoff(e, serial_tx, false, false).await;
                Err(false)
            }
        }
    }

    async fn run_single_session(
        &self,
        serial_tx: &mpsc::Sender<UartEvent>,
        associated_peer_pid: &mut Option<u32>,
    ) -> bool {
        let mut stream = match self.connect_uart(serial_tx, associated_peer_pid).await {
            Ok(s) => s,
            Err(fatal) => return fatal,
        };
        if let Some((_, sid, unconsumed)) = self.perform_handshake(&mut stream, serial_tx).await {
            {
                let _identify = self.client_sock_path.as_deref().map(|s| {
                    let sock = s.to_path_buf();
                    fuchsia_async::Task::local(async move {
                        HostDriver::query_and_update_identity(&sock).await;
                    })
                });
                self.run_session_tasks(stream, unconsumed, sid, serial_tx).await;
            }
            log::info!("Re-initializing UART connection in {:?}...", RECONNECT_BACKOFF_INTERVAL);
            let err = ConnectionError::raw("UART connection lost");
            self.record_failure_and_backoff(err, serial_tx, false, true).await;
        }
        false
    }

    async fn run_reconnection_loop(self, listener: UnixListener) {
        let (serial_tx, _coord_task, _listener_task) =
            HostDriver::spawn_coordinator_and_listener(listener, self.metrics.clone());
        let mut associated_peer_pid = None;
        while !self.run_single_session(&serial_tx, &mut associated_peer_pid).await {}
    }
}

#[cfg(test)]
fn make_test_metadata(target: &str, status: ConnectionStatus) -> ConnectionMetadata {
    ConnectionMetadata {
        pid: std::process::id(),
        target: target.to_string(),
        status,
        id: Some("testid".to_string()),
        baud: NonZeroU32::new(115200),
        protocol: uart_driver_api::UartProtocol::ResendSP,
        log_level: None,
        nodename: None,
        serial: None,
    }
}

#[cfg(test)]
fn write_test_metadata(
    path: &std::path::Path,
    target: &str,
    pid: u32,
    status: ConnectionStatus,
) -> ConnectionMetadata {
    let mut meta = make_test_metadata(target, status);
    meta.pid = pid;
    std::fs::write(path, serde_json::to_string(&meta).unwrap()).unwrap();
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tempfile::tempdir;
    use tokio::io::ReadBuf;

    #[fuchsia::test]
    async fn test_uart_reader_drain_and_frame() {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = UartReader::new(server);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let frame_bytes = encode_frame(123, 1, 0, FrameType::Data, b"hello uart").unwrap();
        client.write_all(&frame_bytes).await.unwrap();

        let frame = reader.next_frame(&metrics).await.unwrap();
        assert_eq!(frame.session_id, 123);
        assert_eq!(frame.channel_id, 1);
        assert_eq!(frame.seq, 0);
        assert_eq!(frame.payload, b"hello uart");
    }

    #[fuchsia::test]
    async fn test_remove_and_bind_socket() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("test.sock");

        let listener = remove_and_bind_socket(sock_path.clone(), false).await.unwrap();
        drop(listener);
        // Yield to allow kernel socket teardown to complete and transition to ECONNREFUSED.
        fuchsia_async::Timer::new(Duration::from_millis(50)).await;

        // Bind again to verify stale socket removal
        let listener2 = remove_and_bind_socket(sock_path, false).await.unwrap();
        drop(listener2);
    }

    #[fuchsia::test]
    async fn test_sender_task_enqueue_and_ack() {
        let (mut sender_tx, sender_rx) = mpsc::channel(16);
        let (data_tx, mut data_rx) = mpsc::channel(16);
        let (mut incoming_event_tx, incoming_event_rx) = mpsc::channel(16);
        let session_id = 100;
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let task = fuchsia_async::Task::local(sender_task(
            sender_rx,
            data_tx,
            incoming_event_rx,
            session_id,
            metrics,
        ));

        // Send a data message
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"ping".to_vec() })
            .await
            .unwrap();

        // Check that data_rx receives an encoded frame
        let frame_bytes = data_rx.next().await.unwrap();
        let mut parser = FrameParser::new();
        parser.feed(&frame_bytes);
        let frame = parser.next_frame().unwrap();
        assert_eq!(frame.channel_id, 1);
        assert_eq!(frame.payload, b"ping");

        // Send ACK for seq
        incoming_event_tx.send(frame.seq).await.unwrap();

        // Close sender_tx while keeping incoming_event_tx alive
        drop(sender_tx);

        let res = task.await;
        assert!(res.is_ok(), "sender_task failed: {:?}", res);
    }

    #[fuchsia::test]
    async fn test_coordinator_task_client_data_and_close() {
        let (mut client_tx_out, client_rx) = mpsc::channel(16);
        let (_serial_tx, serial_rx) = mpsc::channel(16);
        let (client_tx, _client_rx_in) = mpsc::channel(16);
        let (sender_tx_ch, mut sender_rx_ch) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let coord_task = fuchsia_async::Task::local(coordinator_task(
            client_rx,
            serial_rx,
            client_tx,
            Some(sender_tx_ch),
            metrics,
        ));

        // Send channel data
        client_tx_out
            .send(ClientEvent::ChannelData { channel_id: 1, data: b"hello".to_vec() })
            .await
            .unwrap();

        // Expect sender_rx_ch receives SenderMessage::Data
        let msg = sender_rx_ch.next().await.unwrap();
        match msg {
            SenderMessage::Data { channel_id, payload } => {
                assert_eq!(channel_id, 1);
                assert_eq!(payload, b"hello");
            }
            _ => panic!("Expected SenderMessage::Data"),
        }

        // Close client_tx_out to terminate coordinator
        drop(client_tx_out);
        let res = coord_task.await;
        assert!(res.is_ok());
    }

    #[fuchsia::test]
    async fn test_sender_task_processes_acks_while_data_tx_blocked() {
        let (mut sender_tx, sender_rx) = mpsc::channel(16);
        // Create data_tx with capacity 1 (2 slots total with 1 sender) so f0 completes
        // poll_flush and f1 suspends inside send_data_frame with both seq 0 and seq 1 in flight.
        let (data_tx, mut data_rx) = mpsc::channel(1);
        let (mut incoming_event_tx, incoming_event_rx) = mpsc::channel(1);
        let session_id = 100;
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let task = fuchsia_async::Task::local(sender_task(
            sender_rx,
            data_tx,
            incoming_event_rx,
            session_id,
            metrics,
        ));

        // 1st frame occupies slot 1 in data_tx; 2nd frame fills slot 2 and suspends in poll_flush
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"f0".to_vec() })
            .await
            .unwrap();
        sender_tx
            .send(SenderMessage::Data { channel_id: 1, payload: b"f1".to_vec() })
            .await
            .unwrap();
        // Yield so sender_task dequeues f0 and f1 and suspends inside send_data_frame(f1)
        fuchsia_async::yield_now().await;
        fuchsia_async::yield_now().await;

        // While sender_task is blocked waiting for data_rx to drain, send ACKs for seq 0 and 1
        // and yield so sender_task processes them inside send_data_frame while data_tx is full.
        incoming_event_tx.send(0).await.unwrap();
        incoming_event_tx.send(1).await.unwrap();
        fuchsia_async::yield_now().await;

        // Now drain data_rx and close sender_tx
        let _ = data_rx.next().await.unwrap();
        let _ = data_rx.next().await.unwrap();
        drop(sender_tx);

        assert!(task.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_coordinator_congested_client_backpressure_and_disconnect() {
        let (mut client_tx_out, client_rx) = mpsc::channel(1);
        let (mut serial_tx, serial_rx) = mpsc::channel(16);
        let (sender_tx_ch, mut sender_rx_ch) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let coord_task = fuchsia_async::Task::local(coordinator_task(
            client_rx,
            serial_rx,
            client_tx_out.clone(),
            Some(sender_tx_ch),
            metrics,
        ));

        let (stalled_client, stalled_server) = UnixStream::pair().unwrap();
        let (mut healthy_client, healthy_server) = UnixStream::pair().unwrap();

        client_tx_out
            .send(ClientEvent::NewChannel { channel_id: 1, stream: stalled_server })
            .await
            .unwrap();
        client_tx_out
            .send(ClientEvent::NewChannel { channel_id: 2, stream: healthy_server })
            .await
            .unwrap();

        // Flood channel 1 with large payloads until `state.writer_tx` blocks `handle_uart_event`
        let big_chunk = vec![b'x'; 65536];
        for _ in 0..(CLIENT_WRITER_CHANNEL_CAPACITY + 64) {
            if serial_tx
                .try_send(UartEvent::UartData { channel_id: 1, data: big_chunk.clone() })
                .is_err()
            {
                break;
            }
            fuchsia_async::yield_now().await;
        }

        // Disconnect `stalled_client` while `coordinator_task` is backpressured on `writer_tx`.
        // Because `client_writer_task` drops `receiver` and `writer` before awaiting
        // `coordinator_tx.send(ChannelClose)`, `state.writer_tx.send` immediately unblocks.
        drop(stalled_client);

        // Verify channel 2 receives data cleanly after the stalled client disconnects
        serial_tx
            .send(UartEvent::UartData { channel_id: 2, data: b"healthy-ok".to_vec() })
            .await
            .unwrap();

        let mut buf = [0u8; 10];
        healthy_client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"healthy-ok");

        // Verify coordinator emitted a Close message for the disconnected channel 1
        let msg = sender_rx_ch.next().await.unwrap();
        assert!(matches!(msg, SenderMessage::Close { channel_id: 1 }));

        drop(healthy_client);
        drop(client_tx_out);
        drop(serial_tx);
        assert!(coord_task.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_writer_task_batches_and_acks() {
        let (client, mut server) = tokio::io::duplex(1024);
        let ack_tracker = AckTracker::new();
        let (mut data_tx, data_rx) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));
        let task = fuchsia_async::Task::local(writer_task(
            client,
            ack_tracker.clone(),
            data_rx,
            metrics,
            None,
        ));

        ack_tracker.set_ack(42, 7);
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).await.unwrap();
        let mut parser = FrameParser::new();
        parser.feed(&buf[..n]);
        let frame = parser.next_frame().unwrap();
        assert_eq!((frame.session_id, frame.seq, frame.frame_type), (42, 7, FrameType::Ack));

        let data_frame = encode_frame(42, 1, 0, FrameType::Data, b"out").unwrap();
        data_tx.send(data_frame).await.unwrap();
        drop(data_tx);

        let n2 = server.read(&mut buf).await.unwrap();
        parser.feed(&buf[..n2]);
        let frame2 = parser.next_frame().unwrap();
        assert_eq!((frame2.channel_id, frame2.payload.as_slice()), (1, &b"out"[..]));
        assert!(task.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_writer_task_rate_limiting() {
        let (client, mut server) = tokio::io::duplex(1024);
        let ack_tracker = AckTracker::new();
        let (mut data_tx, data_rx) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));
        let task = fuchsia_async::Task::local(writer_task(
            client,
            ack_tracker,
            data_rx,
            metrics,
            NonZeroU32::new(100_000),
        ));

        let data_frame = encode_frame(42, 1, 0, FrameType::Data, b"rate-limited").unwrap();
        data_tx.send(data_frame).await.unwrap();
        drop(data_tx);

        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).await.unwrap();
        let mut parser = FrameParser::new();
        parser.feed(&buf[..n]);
        let frame = parser.next_frame().unwrap();
        assert_eq!((frame.channel_id, frame.payload.as_slice()), (1, &b"rate-limited"[..]));
        assert!(task.await.is_ok());
    }

    #[fuchsia::test]
    async fn test_receiver_task_in_order_and_ack() {
        let (mut client, server) = tokio::io::duplex(1024);
        let reader = UartReader::new(server);
        let (serial_tx, mut serial_rx) = mpsc::channel(16);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let task = fuchsia_async::Task::local(receiver_task(
            reader,
            serial_tx,
            ack_tracker.clone(),
            incoming_event_tx,
            99,
            metrics,
        ));

        let data_frame = encode_frame(99, 2, 0, FrameType::Data, b"rx-payload").unwrap();
        client.write_all(&data_frame).await.unwrap();

        let event = serial_rx.next().await.unwrap();
        match event {
            UartEvent::UartData { channel_id, data } => {
                assert_eq!(channel_id, 2);
                assert_eq!(data, b"rx-payload");
            }
            _ => panic!("Expected UartEvent::UartData"),
        }
        assert_eq!(ack_tracker.take_ack(), Some((99, 0)));

        drop(client);
        let _ = task.await;
    }
    #[fuchsia::test]
    async fn test_receiver_task_reset_frame_error() {
        let (mut client, server) = tokio::io::duplex(1024);
        let reader = UartReader::new(server);
        let (serial_tx, _serial_rx) = mpsc::channel(16);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(16);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let task = fuchsia_async::Task::local(receiver_task(
            reader,
            serial_tx,
            ack_tracker,
            incoming_event_tx,
            99,
            metrics,
        ));

        let reset_frame = encode_frame(99, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        client.write_all(&reset_frame).await.unwrap();

        let res = task.await;
        assert!(matches!(res, Err(TaskError::Receiver(ReceiverTaskError::TargetRequestedReset))));
    }

    #[fuchsia::test]
    async fn test_sender_task_ack_channel_closed() {
        let (_sender_tx, sender_rx) = mpsc::channel(16);
        let (data_tx, _data_rx) = mpsc::channel(16);
        let (incoming_event_tx, incoming_event_rx) = mpsc::channel(16);
        let session_id = 100;
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let task = fuchsia_async::Task::local(sender_task(
            sender_rx,
            data_tx,
            incoming_event_rx,
            session_id,
            metrics,
        ));

        // Drop incoming_event_tx so the ACK channel closes
        drop(incoming_event_tx);

        let res = task.await;
        assert!(matches!(res, Err(TaskError::Sender(SenderTaskError::AckChannelClosed))));
    }

    #[fuchsia::test]
    async fn test_uart_reader_eof() {
        let (client, server) = tokio::io::duplex(1024);
        drop(client);
        let mut reader = UartReader::new(server);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let res = reader.next_frame(&metrics).await;
        assert!(matches!(res, Err(ReaderError::Eof)));
    }

    #[fuchsia::test]
    async fn test_host_driver_metadata_deletion_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("client.sock");
        let meta_path = temp.path().join("meta.json");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let meta = make_test_metadata("/dev/nonexistent_uart_test", ConnectionStatus::Connecting);
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        HostDriver::update_status(&Some(meta_path.clone()), ConnectionStatus::Connected);
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let loaded: ConnectionMetadata = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.status, ConnectionStatus::Connected);

        let meta_path_clone = meta_path.clone();
        let _cleaner = fuchsia_async::Task::local(async move {
            fuchsia_async::Timer::new(Duration::from_millis(50)).await;
            let _ = std::fs::remove_file(&meta_path_clone);
        });

        HostDriver::run(
            listener,
            "/dev/nonexistent_uart_test".to_string(),
            NonZeroU32::new(115200).unwrap(),
            Some(meta_path),
            true,
        )
        .await;
    }

    #[test]
    fn test_host_driver_identity_metadata_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("ffx_uart_test.sock");
        let meta_path = temp.path().join("ffx_uart_test.json");

        let meta = make_test_metadata("/dev/ttyUSB0", ConnectionStatus::Connected);
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        uart_driver_api::update_metadata_identity(
            &sock_path,
            Some("fuchsia-1234-test".to_string()),
            Some("SERIAL001".to_string()),
        )
        .unwrap();

        let content = std::fs::read_to_string(&meta_path).unwrap();
        let loaded: ConnectionMetadata = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.nodename.as_deref(), Some("fuchsia-1234-test"));
        assert_eq!(loaded.serial.as_deref(), Some("SERIAL001"));

        HostDriver::update_status(
            &Some(meta_path.clone()),
            ConnectionStatus::Error(ConnectionError::raw("UART connection lost")),
        );
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let loaded: ConnectionMetadata = serde_json::from_str(&content).unwrap();
        assert_eq!((loaded.nodename, loaded.serial), (None, None));
    }

    struct MockReader {
        data: Vec<u8>,
        read_ptr: usize,
    }

    impl MockReader {
        fn new(data: Vec<u8>) -> Self {
            Self { data, read_ptr: 0 }
        }
    }

    impl tokio::io::AsyncRead for MockReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.read_ptr >= self.data.len() {
                return Poll::Ready(Ok(()));
            }
            let amt = std::cmp::min(buf.remaining(), self.data.len() - self.read_ptr);
            buf.put_slice(&self.data[self.read_ptr..self.read_ptr + amt]);
            self.read_ptr += amt;
            Poll::Ready(Ok(()))
        }
    }

    #[fuchsia::test]
    async fn test_watch_metadata_deletion_unit() {
        let temp = tempfile::tempdir().unwrap();
        let meta_path = temp.path().join("meta.json");
        write_test_metadata(
            &meta_path,
            "test_target",
            std::process::id(),
            ConnectionStatus::Connected,
        );

        let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel();
        let _watcher = HostDriver::watch_metadata_deletion(&Some(meta_path.clone()), shutdown_tx);

        std::fs::remove_file(&meta_path).unwrap();
        let timeout_res = tokio::time::timeout(Duration::from_secs(3), shutdown_rx).await;
        assert!(timeout_res.is_ok(), "Shutdown signal not received within timeout");
    }

    #[fuchsia::test]
    async fn test_host_receiver_task_session_change_discard() {
        let (s1, s2, cid) = (12345u32, 67890u32, 42u16);
        let mut data = encode_frame(s2, cid, 0, FrameType::Data, b"discard").unwrap();
        data.extend_from_slice(&encode_frame(s1, cid, 0, FrameType::Data, b"accept").unwrap());

        let (serial_tx, mut serial_rx) = mpsc::channel(10);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(10);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let rx_handle = fuchsia_async::Task::local(receiver_task(
            UartReader::new(MockReader::new(data)),
            serial_tx,
            ack_tracker.clone(),
            incoming_event_tx,
            s1,
            metrics,
        ));

        let event = serial_rx.next().await.unwrap();
        assert_eq!(event, UartEvent::UartData { channel_id: cid, data: b"accept".to_vec() });
        assert_eq!(ack_tracker.take_ack(), Some((s1, 0)));
        drop(rx_handle);
    }

    #[fuchsia::test]
    async fn test_host_receiver_task_fletcher_collision_noise() {
        let (s_cur, s_noise) = (0xABCDEFu32, 0x99999999u32);
        let mut data = encode_frame(s_noise, 1, 0, FrameType::Data, b"noisebytes").unwrap();
        data.extend_from_slice(&encode_frame(s_cur, 1, 0, FrameType::Data, b"validbytes").unwrap());

        let (serial_tx, mut serial_rx) = mpsc::channel(10);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(10);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let rx_handle = fuchsia_async::Task::local(receiver_task(
            UartReader::new(MockReader::new(data)),
            serial_tx,
            ack_tracker,
            incoming_event_tx,
            s_cur,
            metrics,
        ));

        let event = serial_rx.next().await.unwrap();
        assert_eq!(event, UartEvent::UartData { channel_id: 1, data: b"validbytes".to_vec() });
        drop(rx_handle);
    }

    #[fuchsia::test]
    async fn test_remove_and_bind_socket_kills_stale_daemon() {
        let temp = tempdir().unwrap();
        let socket_path = temp.path().join("ffx_uart_test.sock");
        let json_path = socket_path.with_extension(uart_driver_api::METADATA_FILE_EXTENSION);

        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id().unwrap();
        write_test_metadata(&json_path, "mock-target", pid, ConnectionStatus::Connecting);

        let _listener = remove_and_bind_socket(socket_path.clone(), false).await.unwrap();
        let status = child.wait().await.unwrap();
        assert!(!status.success(), "Child process was expected to be terminated by signal");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(libc::SIGTERM));
        }
    }

    #[fuchsia::test]
    async fn test_control_socket_metrics_exposure() {
        let temp = tempdir().unwrap();
        let control_path = temp.path().join("ffx_uart_test.control");

        let metrics = Arc::new(Mutex::new(DaemonMetrics {
            checksum_errors: 12,
            retransmissions: 34,
            active_protocol: UartProtocol::TestProtocol,
            estimated_rtt_ms: 56,
            connection_drops: 78,
            handshake_failures: 90,
            ..Default::default()
        }));

        let control_listener = UnixListener::bind(&control_path).unwrap();
        let metrics_clone = metrics.clone();
        let server_task = fuchsia_async::Task::local(async move {
            let scope = fuchsia_async::Scope::new();
            HostDriver::serve_control_listener(&scope, control_listener, metrics_clone).await;
        });

        let mut client_stream = UnixStream::connect(&control_path).await.unwrap();
        let mut content = String::new();
        client_stream.read_to_string(&mut content).await.unwrap();

        let parsed: DaemonMetrics = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.checksum_errors, 12);
        assert_eq!(parsed.retransmissions, 34);
        assert_eq!(parsed.active_protocol, UartProtocol::TestProtocol);
        assert_eq!(parsed.estimated_rtt_ms, 56);
        assert_eq!(parsed.connection_drops, 78);
        assert_eq!(parsed.handshake_failures, 90);
        drop(server_task);
    }

    #[fuchsia::test]
    async fn test_control_socket_concurrent_clients_not_blocked() {
        let temp = tempdir().unwrap();
        let control_path = temp.path().join("ffx_uart_test_concurrent.control");

        let metrics =
            Arc::new(Mutex::new(DaemonMetrics { checksum_errors: 42, ..Default::default() }));

        let control_listener = UnixListener::bind(&control_path).unwrap();
        let metrics_clone = metrics.clone();
        let server_task = fuchsia_async::Task::local(async move {
            let scope = fuchsia_async::Scope::new();
            HostDriver::serve_control_listener(&scope, control_listener, metrics_clone).await;
        });

        // Connect first client but do not read immediately (simulating a slow/paused client).
        let _slow_client = UnixStream::connect(&control_path).await.unwrap();

        // Second client connects concurrently and should receive metrics without being blocked.
        let mut fast_client = UnixStream::connect(&control_path).await.unwrap();
        let mut content = String::new();
        fast_client.read_to_string(&mut content).await.unwrap();

        let parsed: DaemonMetrics = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.checksum_errors, 42);

        drop(server_task);
    }

    #[fuchsia::test]
    async fn test_verify_is_uart_driver_logic() {
        let self_pid = std::process::id();
        if cfg!(target_os = "linux") {
            assert!(!ffx_tool_uart::is_driver_running(self_pid));
            assert!(!ffx_tool_uart::is_driver_running(1));
        } else {
            assert!(ffx_tool_uart::is_driver_running(self_pid));
            assert!(ffx_tool_uart::is_driver_running(1));
        }
    }
    #[fuchsia::test]
    async fn test_uart_reader_with_initial_data() {
        let session_id = 12345u32;
        let payload = b"unconsumed_hello";
        let frame = encode_frame(session_id, 1, 0, FrameType::Data, payload).unwrap();

        let mut reader = UartReader::with_initial_data(MockReader::new(Vec::new()), frame);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let parsed = reader.next_frame(&metrics).await.unwrap();
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.channel_id, 1);
        assert_eq!(parsed.payload, payload);
    }

    #[fuchsia::test]
    async fn test_host_receiver_task_queue_full_drops_and_does_not_advance_ack() {
        let (sid, cid) = (55555u32, 1u16);
        let mut data = encode_frame(sid, cid, 0, FrameType::Data, b"data1").unwrap();
        data.extend_from_slice(&encode_frame(sid, cid, 1, FrameType::Data, b"data2").unwrap());

        // Channel capacity of 0 so the sender's 1-message slot fills on `data1`,
        // causing `data2` to fail `try_send` (queue full) and be dropped.
        let (serial_tx, mut serial_rx) = mpsc::channel(0);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(10);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let rx_handle = fuchsia_async::Task::local(receiver_task(
            UartReader::new(MockReader::new(data)),
            serial_tx,
            ack_tracker.clone(),
            incoming_event_tx,
            sid,
            metrics,
        ));

        let rx_res = rx_handle.await;
        assert!(matches!(
            rx_res,
            Err(TaskError::Receiver(ReceiverTaskError::Reader(ReaderError::Eof)))
        ));

        let event = serial_rx.next().await.unwrap();
        assert_eq!(event, UartEvent::UartData { channel_id: cid, data: b"data1".to_vec() });
        assert!(serial_rx.try_recv().is_err());

        assert_eq!(ack_tracker.take_ack(), Some((sid, 0)));
        assert_eq!(ack_tracker.take_ack(), None);
    }

    #[fuchsia::test]
    async fn test_host_receiver_task_handles_reset_frame() {
        let sid = 77777u32;
        let reset = encode_frame(sid, CONTROL_CHANNEL_ID, 0, FrameType::Reset, &[]).unwrap();
        let (serial_tx, _serial_rx) = mpsc::channel(10);
        let ack_tracker = AckTracker::new();
        let (incoming_event_tx, _incoming_event_rx) = mpsc::channel(10);
        let metrics = Arc::new(Mutex::new(DaemonMetrics::default()));

        let res = receiver_task(
            UartReader::new(MockReader::new(reset)),
            serial_tx,
            ack_tracker,
            incoming_event_tx,
            sid,
            metrics,
        )
        .await;
        assert!(matches!(res, Err(TaskError::Receiver(ReceiverTaskError::TargetRequestedReset))));
    }

    #[fuchsia::test]
    async fn test_driver_terminates_on_metadata_deletion() {
        let temp = tempdir().unwrap();
        let client_socket_path = temp.path().join("client.sock");
        let uart_socket_path = temp.path().join("uart.sock");
        let meta_path = temp.path().join("meta.json");

        write_test_metadata(
            &meta_path,
            "test-target",
            std::process::id(),
            ConnectionStatus::Connecting,
        );
        let client_listener = UnixListener::bind(&client_socket_path).unwrap();
        let uart_target_str = uart_socket_path.to_string_lossy().to_string();

        let driver_task = fuchsia_async::Task::local(async move {
            HostDriver::run(
                client_listener,
                uart_target_str,
                NonZeroU32::new(115200).unwrap(),
                Some(meta_path),
                false,
            )
            .await;
        });

        std::fs::remove_file(temp.path().join("meta.json")).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), driver_task).await;
        assert!(result.is_ok(), "Driver task did not terminate within timeout!");
        assert!(!client_socket_path.exists(), "Client socket was not cleaned up!");
    }

    #[fuchsia::test]
    async fn test_driver_terminates_on_target_path_deletion() {
        let temp = tempdir().unwrap();
        let client_socket_path = temp.path().join("client.sock");
        let uart_socket_path = temp.path().join("uart.sock");

        let uart_listener = UnixListener::bind(&uart_socket_path).unwrap();
        let client_listener = UnixListener::bind(&client_socket_path).unwrap();
        let uart_target_str = uart_socket_path.to_string_lossy().to_string();

        let driver_task = fuchsia_async::Task::local(async move {
            HostDriver::run(
                client_listener,
                uart_target_str,
                NonZeroU32::new(115200).unwrap(),
                None,
                false,
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(uart_listener);
        std::fs::remove_file(&uart_socket_path).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), driver_task).await;
        assert!(result.is_ok(), "Driver task did not terminate within timeout!");
        assert!(!client_socket_path.exists(), "Client socket was not cleaned up!");
    }

    #[fuchsia::test]
    async fn test_driver_retries_on_physical_tty_path_deletion() {
        let temp = tempdir().unwrap();
        let client_socket_path = temp.path().join("client.sock");
        let uart_file_path = temp.path().join("uart.bin");
        let meta_path = temp.path().join("meta.json");

        write_test_metadata(
            &meta_path,
            "test-target",
            std::process::id(),
            ConnectionStatus::Connecting,
        );
        std::fs::write(&uart_file_path, b"").unwrap();

        let client_listener = UnixListener::bind(&client_socket_path).unwrap();
        let uart_target_str = uart_file_path.to_string_lossy().to_string();

        let driver_task = fuchsia_async::Task::local(async move {
            HostDriver::run(
                client_listener,
                uart_target_str,
                NonZeroU32::new(115200).unwrap(),
                Some(meta_path),
                false,
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::fs::remove_file(&uart_file_path).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(client_socket_path.exists(), "Driver exited prematurely!");

        std::fs::write(&uart_file_path, b"").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(client_socket_path.exists(), "Driver exited after path restoration!");

        std::fs::remove_file(temp.path().join("meta.json")).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), driver_task).await;
        assert!(result.is_ok(), "Driver task did not terminate within timeout!");
    }

    #[fuchsia::test]
    async fn test_driver_terminates_on_physical_tty_path_deletion_when_no_retry() {
        let temp = tempdir().unwrap();
        let client_socket_path = temp.path().join("client.sock");
        let uart_file_path = temp.path().join("uart.bin");
        let meta_path = temp.path().join("meta.json");

        write_test_metadata(
            &meta_path,
            "test-target",
            std::process::id(),
            ConnectionStatus::Connecting,
        );
        std::fs::write(&uart_file_path, b"").unwrap();

        let client_listener = UnixListener::bind(&client_socket_path).unwrap();
        let uart_target_str = uart_file_path.to_string_lossy().to_string();

        let driver_task = fuchsia_async::Task::local(async move {
            HostDriver::run(
                client_listener,
                uart_target_str,
                NonZeroU32::new(115200).unwrap(),
                Some(meta_path),
                true,
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::fs::remove_file(&uart_file_path).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), driver_task).await;
        assert!(result.is_ok(), "Driver task did not terminate within timeout!");
        assert!(!client_socket_path.exists(), "Client socket was not cleaned up!");
    }

    #[fuchsia::test]
    async fn test_query_identity_once_nonexistent_socket() {
        let temp = tempdir().unwrap();
        let sock_path = temp.path().join("nonexistent.sock");
        let res = HostDriver::query_identity_once(&sock_path).await;
        assert!(matches!(res, Err(IdentityError::Connect(path, _)) if path == sock_path));
    }

    #[fuchsia::test]
    async fn test_update_status_invalidates_cached_identity() {
        let temp = tempdir().unwrap();
        let meta_path = temp.path().join("ffx_uart_test.json");

        let mut meta = make_test_metadata("/dev/ttyUSB0", ConnectionStatus::Connected);
        meta.nodename = Some("fuchsia-test".to_string());
        meta.serial = Some("SER12345".to_string());
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        HostDriver::update_status(&Some(meta_path.clone()), ConnectionStatus::Connecting);
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let updated: ConnectionMetadata = serde_json::from_str(&content).unwrap();
        assert_eq!(updated.status, ConnectionStatus::Connecting);
        assert_eq!((updated.nodename, updated.serial), (None, None));

        HostDriver::update_status(&Some(meta_path.clone()), ConnectionStatus::Connected);
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let reconnected: ConnectionMetadata = serde_json::from_str(&content).unwrap();
        assert_eq!(reconnected.status, ConnectionStatus::Connected);
        assert_eq!((reconnected.nodename, reconnected.serial), (None, None));
    }
}
