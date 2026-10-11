// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Multiplexed FDomain channel coordinator and RCS socket bridge tasks.

use crate::error::Result;
use crate::receiver::SerialEvent;
use crate::sender::SenderMessage;
use fidl_fuchsia_developer_remotecontrol_connector::ConnectorProxy;
use futures::channel::mpsc;
use futures::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use futures::prelude::*;
use log::{error, warn};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use uart_fpl::{CONTROL_CHANNEL_ID, MAX_PAYLOAD_SIZE};

/// Default bounded mpsc channel capacity for internal control and frame queues.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

/// Default bounded mpsc channel capacity for multiplexed client data queues.
pub const DEFAULT_CLIENT_DATA_CAPACITY: usize = 64;

/// Maximum queue depth for per-channel asynchronous socket writers.
const CHANNEL_WRITER_QUEUE_CAPACITY: usize = 256;

/// Maximum number of concurrently open or pending FDomain channels.
const MAX_ACTIVE_CHANNELS: usize = 256;

/// Maximum buffer capacity (in bytes) staged for a single channel before socket connection.
const MAX_CHANNEL_STAGING_BYTES: usize = 2 * 1024 * 1024;

/// Events sent by asynchronous coordinator worker tasks to the main loop.
#[derive(Debug)]
enum InternalEvent {
    /// Asynchronous toolbox socket registration succeeded for a channel.
    Registered { channel_id: u16, generation: u64, socket: fuchsia_async::Socket },
    /// Asynchronous toolbox socket registration failed for a channel.
    RegistrationFailed { channel_id: u16, generation: u64, error: fidl::Error },
    /// A channel writer task finished flushing its queue (`error: None`) or encountered an error.
    WriterDone { channel_id: u16, generation: u64, error: Option<std::io::Error> },
}

/// Events sent by per-channel RCS socket readers to the coordinator.
#[derive(Debug)]
enum ClientEvent {
    /// Ingress data read from the local RCS socket to be sent over serial.
    Data { channel_id: u16, generation: u64, payload: Vec<u8> },
    /// The local RCS socket reached EOF (`error: None`) or encountered a read error.
    Closed { channel_id: u16, generation: u64, error: Option<std::io::Error> },
}

/// Lifecycle state of an active or pending FDomain channel.
enum ChannelState {
    /// Channel is waiting for `fdomain_toolbox_socket` FIDL registration to complete.
    Pending {
        generation: u64,
        buffer: Vec<Vec<u8>>,
        pending_bytes: usize,
        _registration_task: fuchsia_async::Task<()>,
    },
    /// Channel has an open, active bidirectional RCS socket bridge.
    Connected {
        generation: u64,
        sender: mpsc::Sender<Vec<u8>>,
        pending_incoming: VecDeque<Vec<u8>>,
        pending_bytes: usize,
        _reader_task: fuchsia_async::Task<()>,
        _writer_task: fuchsia_async::Task<()>,
    },
}

/// State machine for multiplexing FDomain channels and scheduling egress messages.
struct CoordinatorState {
    active: HashMap<u16, ChannelState>,
    closing_pending: HashMap<(u16, u64), (Vec<Vec<u8>>, fuchsia_async::Task<()>)>,
    closing_writers: HashMap<(u16, u64), fuchsia_async::Task<()>>,
    outgoing: BTreeMap<u16, VecDeque<SenderMessage>>,
    last_channel_id: Option<u16>,
    next_generation: u64,
    client_tx: mpsc::Sender<ClientEvent>,
    internal_tx: mpsc::Sender<InternalEvent>,
}

/// Reads data from an active RCS socket and forwards it as [`ClientEvent`]s to the coordinator.
///
/// When EOF (`Ok(0)`) or a socket read error (`Err(e)`) occurs, a [`ClientEvent::Closed`] is sent
/// carrying `error: Option<std::io::Error>` so [`CoordinatorState`] can distinguish normal socket
/// closure from transport errors on active generations.
async fn channel_reader_task(
    channel_id: u16,
    generation: u64,
    mut reader: ReadHalf<fuchsia_async::Socket>,
    mut client_data_tx: mpsc::Sender<ClientEvent>,
) {
    let mut buf = [0u8; MAX_PAYLOAD_SIZE];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                let _ = client_data_tx
                    .send(ClientEvent::Closed { channel_id, generation, error: None })
                    .await;
                break;
            }
            Ok(n) => {
                let event =
                    ClientEvent::Data { channel_id, generation, payload: buf[..n].to_vec() };
                if let Err(e) = client_data_tx.send(event).await {
                    error!("Failed to forward RCS data for channel {}: {:?}", channel_id, e);
                    break;
                }
            }
            Err(e) => {
                let _ = client_data_tx
                    .send(ClientEvent::Closed { channel_id, generation, error: Some(e) })
                    .await;
                break;
            }
        }
    }
}

/// Writes buffered and incoming channel payloads from the coordinator to the active RCS socket.
///
/// Once all buffered and queued data has been written and flushed (or if a write fails), an
/// [`InternalEvent::WriterDone`] notification is sent back to the coordinator along with any
/// write/flush error encountered.
async fn channel_writer_task(
    channel_id: u16,
    generation: u64,
    mut writer: WriteHalf<fuchsia_async::Socket>,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<Vec<u8>>,
    mut internal_tx: mpsc::Sender<InternalEvent>,
) {
    async fn write_and_flush(
        writer: &mut WriteHalf<fuchsia_async::Socket>,
        data: &[u8],
    ) -> std::io::Result<()> {
        writer.write_all(data).await?;
        writer.flush().await
    }

    let mut write_error = None;
    for data in buffer {
        if let Err(e) = write_and_flush(&mut writer, &data).await {
            write_error = Some(e);
            break;
        }
    }
    while write_error.is_none()
        && let Some(data) = receiver.next().await
    {
        if let Err(e) = write_and_flush(&mut writer, &data).await {
            write_error = Some(e);
            break;
        }
    }
    let _ = internal_tx
        .send(InternalEvent::WriterDone { channel_id, generation, error: write_error })
        .await;
}

/// Attempts to send an incoming chunk directly to the writer channel, or stages it in
/// `pending_incoming` if the writer channel is temporarily full.
fn stage_or_send_connected(
    channel_id: u16,
    data: Vec<u8>,
    sender: &mut mpsc::Sender<Vec<u8>>,
    pending_incoming: &mut VecDeque<Vec<u8>>,
    pending_bytes: &mut usize,
) -> std::result::Result<(), String> {
    let to_stage = if pending_incoming.is_empty() {
        match sender.try_send(data) {
            Ok(()) => return Ok(()),
            Err(e) if e.is_disconnected() => {
                let reason = format!("Channel {channel_id} writer disconnected");
                error!("{reason}, dropping channel");
                return Err(reason);
            }
            Err(e) => e.into_inner(),
        }
    } else {
        data
    };
    if *pending_bytes + to_stage.len() > MAX_CHANNEL_STAGING_BYTES {
        let reason = format!(
            "Channel {channel_id} exceeded max staging buffer ({MAX_CHANNEL_STAGING_BYTES} bytes)"
        );
        error!("{reason}, dropping channel");
        return Err(reason);
    }
    *pending_bytes += to_stage.len();
    pending_incoming.push_back(to_stage);
    Ok(())
}

/// Stages an incoming serial payload in the pending channel buffer before socket connection.
fn stage_pending_data(
    channel_id: u16,
    data: Vec<u8>,
    buffer: &mut Vec<Vec<u8>>,
    pending_bytes: &mut usize,
) -> std::result::Result<(), String> {
    if *pending_bytes + data.len() > MAX_CHANNEL_STAGING_BYTES {
        let reason = format!(
            "Pending channel {channel_id} exceeded max staging buffer ({MAX_CHANNEL_STAGING_BYTES} bytes)"
        );
        error!("{reason}, dropping channel");
        return Err(reason);
    }
    *pending_bytes += data.len();
    buffer.push(data);
    Ok(())
}

/// Spawns an asynchronous task to register a channel socket with the RCS connector.
fn spawn_channel_registration(
    channel_id: u16,
    generation: u64,
    rcs_connector: ConnectorProxy,
    mut internal_tx: mpsc::Sender<InternalEvent>,
) -> fuchsia_async::Task<()> {
    fuchsia_async::Task::spawn(async move {
        let (local_socket, remote_socket) = fidl::Socket::create_stream();
        if let Err(error) = rcs_connector.fdomain_toolbox_socket(remote_socket).await {
            let _ = internal_tx
                .send(InternalEvent::RegistrationFailed { channel_id, generation, error })
                .await;
            return;
        }
        let socket = fuchsia_async::Socket::from_socket(local_socket);
        let _ =
            internal_tx.send(InternalEvent::Registered { channel_id, generation, socket }).await;
    })
}

impl CoordinatorState {
    /// Creates a new [`CoordinatorState`].
    fn new(client_tx: mpsc::Sender<ClientEvent>, internal_tx: mpsc::Sender<InternalEvent>) -> Self {
        Self {
            active: HashMap::new(),
            closing_pending: HashMap::new(),
            closing_writers: HashMap::new(),
            outgoing: BTreeMap::new(),
            last_channel_id: None,
            next_generation: 1,
            client_tx,
            internal_tx,
        }
    }

    /// Allocates a new monotonically increasing generation identifier for a channel session.
    fn alloc_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        generation
    }

    /// Enqueues a [`SenderMessage::Close`] message into the outgoing serial queue for `channel_id`.
    fn enqueue_close(&mut self, channel_id: u16, reason: Option<String>) {
        self.outgoing
            .entry(channel_id)
            .or_default()
            .push_back(SenderMessage::Close { channel_id, reason });
    }

    /// Synchronously drains staged incoming chunks from `pending_incoming` into the connected
    /// writer sender channels using non-blocking `try_send`.
    ///
    /// If a channel writer channel is disconnected, the channel is marked as disconnected and
    /// queued for closure.
    fn drain_pending_incoming(&mut self) {
        let mut disconnected_channels = Vec::new();
        for (&channel_id, state) in self.active.iter_mut() {
            if let ChannelState::Connected { sender, pending_incoming, pending_bytes, .. } = state {
                while let Some(front) = pending_incoming.pop_front() {
                    let len = front.len();
                    match sender.try_send(front) {
                        Ok(()) => *pending_bytes = pending_bytes.saturating_sub(len),
                        Err(e) if e.is_disconnected() => {
                            disconnected_channels.push(channel_id);
                            break;
                        }
                        Err(e) => {
                            pending_incoming.push_front(e.into_inner());
                            break;
                        }
                    }
                }
            }
        }
        for channel_id in disconnected_channels {
            if self.active.remove(&channel_id).is_some() {
                self.enqueue_close(
                    channel_id,
                    Some(format!("Channel {channel_id} writer disconnected")),
                );
            }
        }
    }

    /// Asynchronously polls readiness of the connected channel writer sinks that have staged
    /// incoming data in `pending_incoming`.
    ///
    /// Returns `Poll::Ready(())` if any staged data was successfully sent into a writer sink,
    /// waking the event loop to continue making progress.
    fn poll_pending_incoming(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut made_progress = false;
        let mut disconnected_channels = Vec::new();
        for (&channel_id, state) in self.active.iter_mut() {
            if let ChannelState::Connected { sender, pending_incoming, pending_bytes, .. } = state {
                while !pending_incoming.is_empty() {
                    match futures::sink::Sink::poll_ready(Pin::new(&mut *sender), cx) {
                        Poll::Ready(Ok(())) => {
                            let front = pending_incoming.pop_front().unwrap();
                            let len = front.len();
                            if futures::sink::Sink::start_send(Pin::new(&mut *sender), front)
                                .is_err()
                            {
                                disconnected_channels.push(channel_id);
                                break;
                            }
                            *pending_bytes = pending_bytes.saturating_sub(len);
                            made_progress = true;
                        }
                        Poll::Ready(Err(_)) => {
                            disconnected_channels.push(channel_id);
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
            }
        }
        for channel_id in disconnected_channels {
            if self.active.remove(&channel_id).is_some() {
                self.enqueue_close(
                    channel_id,
                    Some(format!("Channel {channel_id} writer disconnected")),
                );
            }
        }
        if made_progress { Poll::Ready(()) } else { Poll::Pending }
    }

    /// Binds an established RCS socket to `channel_id`, spawning reader and writer tasks.
    ///
    /// If `closing` is true, the socket is bound in a closing/draining state: the writer task is
    /// spawned to flush `buffer` before closing, without spawning a reader task.
    fn connect_socket(
        &mut self,
        channel_id: u16,
        generation: u64,
        socket: fuchsia_async::Socket,
        buffer: Vec<Vec<u8>>,
        closing: bool,
    ) {
        let (reader, writer) = socket.split();
        let (mut writer_tx, writer_rx) = mpsc::channel(CHANNEL_WRITER_QUEUE_CAPACITY);
        let writer_task = fuchsia_async::Task::spawn(channel_writer_task(
            channel_id,
            generation,
            writer,
            writer_rx,
            buffer,
            self.internal_tx.clone(),
        ));
        if closing {
            writer_tx.close_channel();
            self.closing_writers.insert((channel_id, generation), writer_task);
            return;
        }
        let reader_task = fuchsia_async::Task::spawn(channel_reader_task(
            channel_id,
            generation,
            reader,
            self.client_tx.clone(),
        ));
        self.active.insert(
            channel_id,
            ChannelState::Connected {
                generation,
                sender: writer_tx,
                pending_incoming: VecDeque::new(),
                pending_bytes: 0,
                _reader_task: reader_task,
                _writer_task: writer_task,
            },
        );
    }

    /// Handles a successful socket registration event from a background worker task.
    ///
    /// Connects the newly opened RCS socket if the channel generation matches and the channel
    /// remains active or is in the process of closing.
    fn handle_registered_event(
        &mut self,
        channel_id: u16,
        generation: u64,
        socket: fuchsia_async::Socket,
    ) {
        if let Some((buffer, _)) = self.closing_pending.remove(&(channel_id, generation)) {
            self.connect_socket(channel_id, generation, socket, buffer, true);
            return;
        }
        let is_matching_pending = matches!(
            self.active.get(&channel_id),
            Some(ChannelState::Pending { generation: active_gen, .. }) if *active_gen == generation
        );
        if !is_matching_pending {
            return;
        }
        if let Some(ChannelState::Pending { buffer, .. }) = self.active.remove(&channel_id) {
            self.connect_socket(channel_id, generation, socket, buffer, false);
        }
    }

    /// Dispatches an [`InternalEvent`] emitted by asynchronous background workers.
    fn handle_internal_event(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::Registered { channel_id, generation, socket } => {
                self.handle_registered_event(channel_id, generation, socket);
            }
            InternalEvent::RegistrationFailed { channel_id, generation, error } => {
                if self.closing_pending.remove(&(channel_id, generation)).is_some() {
                    return;
                }
                let matches_gen = matches!(
                    self.active.get(&channel_id),
                    Some(ChannelState::Pending { generation: active_gen, .. }) if *active_gen == generation
                );
                if matches_gen && self.active.remove(&channel_id).is_some() {
                    let reason = format!(
                        "Failed to register RCS socket for channel {channel_id}: {error:?}"
                    );
                    error!("{reason}");
                    self.enqueue_close(channel_id, Some(reason));
                }
            }
            InternalEvent::WriterDone { channel_id, generation, error } => {
                self.closing_writers.remove(&(channel_id, generation));
                let matches_gen = matches!(
                    self.active.get(&channel_id),
                    Some(ChannelState::Connected { generation: active_gen, .. }) if *active_gen == generation
                );
                if matches_gen && self.active.remove(&channel_id).is_some() {
                    let reason = error.map(|e| {
                        let msg = format!("RCS socket write error for channel {channel_id}: {e:?}");
                        error!("{msg}");
                        msg
                    });
                    self.enqueue_close(channel_id, reason);
                }
            }
        }
    }

    /// Initiates opening a new channel upon receiving the first serial data chunk.
    ///
    /// Allocates a new generation, enforces staging byte limits and channel count limits,
    /// stages `data` in a pending buffer, and spawns the asynchronous socket registration task.
    fn open_pending_channel(
        &mut self,
        channel_id: u16,
        data: Vec<u8>,
        rcs_connector: &ConnectorProxy,
    ) {
        if self.active.len() >= MAX_ACTIVE_CHANNELS {
            let reason = format!(
                "Max active channels ({MAX_ACTIVE_CHANNELS}) reached; rejecting new channel {channel_id}"
            );
            warn!("{reason}");
            self.enqueue_close(channel_id, Some(reason));
            return;
        }
        if data.len() > MAX_CHANNEL_STAGING_BYTES {
            let reason = format!(
                "Initial frame on channel {channel_id} exceeds max staging bytes ({MAX_CHANNEL_STAGING_BYTES})"
            );
            error!("{reason}");
            self.enqueue_close(channel_id, Some(reason));
            return;
        }
        let generation = self.alloc_generation();
        let pending_bytes = data.len();
        let _registration_task = spawn_channel_registration(
            channel_id,
            generation,
            rcs_connector.clone(),
            self.internal_tx.clone(),
        );
        self.active.insert(
            channel_id,
            ChannelState::Pending {
                generation,
                buffer: vec![data],
                pending_bytes,
                _registration_task,
            },
        );
    }

    /// Processes incoming [`SerialEvent::SerialData`] for a channel.
    ///
    /// Directs data to the connected writer sink (staging if temporarily blocked), appends
    /// to pending buffers if registration is in flight, or opens a new pending channel.
    fn handle_serial_data_event(
        &mut self,
        channel_id: u16,
        data: Vec<u8>,
        rcs_connector: &ConnectorProxy,
    ) {
        if channel_id == CONTROL_CHANNEL_ID {
            warn!("Ignoring SerialData for CONTROL_CHANNEL_ID in coordinator");
            return;
        }
        self.drain_pending_incoming();
        match self.active.get_mut(&channel_id) {
            Some(ChannelState::Connected { sender, pending_incoming, pending_bytes, .. }) => {
                if let Err(reason) = stage_or_send_connected(
                    channel_id,
                    data,
                    sender,
                    pending_incoming,
                    pending_bytes,
                ) {
                    self.active.remove(&channel_id);
                    self.enqueue_close(channel_id, Some(reason));
                }
            }
            Some(ChannelState::Pending { buffer, pending_bytes, .. }) => {
                if let Err(reason) = stage_pending_data(channel_id, data, buffer, pending_bytes) {
                    self.active.remove(&channel_id);
                    self.enqueue_close(channel_id, Some(reason));
                }
            }
            None => self.open_pending_channel(channel_id, data, rcs_connector),
        }
    }

    /// Transitions a connected writer into the `closing_writers` table to flush any queued
    /// incoming data before terminating.
    fn drain_connected_writer(
        &mut self,
        channel_id: u16,
        generation: u64,
        mut sender: mpsc::Sender<Vec<u8>>,
        pending_incoming: VecDeque<Vec<u8>>,
        writer_task: fuchsia_async::Task<()>,
    ) {
        if pending_incoming.is_empty() {
            sender.close_channel();
            self.closing_writers.insert((channel_id, generation), writer_task);
        } else {
            let drain_task = fuchsia_async::Task::spawn(async move {
                for chunk in pending_incoming {
                    if sender.send(chunk).await.is_err() {
                        break;
                    }
                }
                sender.close_channel();
                writer_task.await;
            });
            self.closing_writers.insert((channel_id, generation), drain_task);
        }
    }

    /// Handles a [`SerialEvent::SerialClose`] indicating the remote peer closed the channel.
    ///
    /// Removes the channel from active routing, discards outgoing messages, and gracefully
    /// drains or cancels in-flight pending buffers and writer tasks.
    fn handle_serial_close(&mut self, channel_id: u16) {
        if channel_id == CONTROL_CHANNEL_ID {
            warn!("Ignoring SerialClose for CONTROL_CHANNEL_ID in coordinator");
            return;
        }
        self.drain_pending_incoming();
        self.outgoing.remove(&channel_id);
        match self.active.remove(&channel_id) {
            Some(ChannelState::Pending { generation, buffer, _registration_task, .. })
                if !buffer.is_empty() =>
            {
                self.closing_pending.insert((channel_id, generation), (buffer, _registration_task));
            }
            Some(ChannelState::Connected {
                generation,
                sender,
                pending_incoming,
                _writer_task,
                ..
            }) => {
                self.drain_connected_writer(
                    channel_id,
                    generation,
                    sender,
                    pending_incoming,
                    _writer_task,
                );
            }
            _ => {}
        }
    }

    /// Dispatches a [`SerialEvent`] received from the deserializer stream.
    fn handle_serial_event(&mut self, event: SerialEvent, rcs_connector: &ConnectorProxy) {
        match event {
            SerialEvent::SerialData { channel_id, data } => {
                self.handle_serial_data_event(channel_id, data, rcs_connector);
            }
            SerialEvent::SerialClose { channel_id } => self.handle_serial_close(channel_id),
        }
    }

    /// Processes a [`ClientEvent`] emitted by a channel's RCS socket reader task.
    ///
    /// Enqueues egress data into the channel's outgoing serial queue or handles socket EOF / closure.
    fn handle_client_event(&mut self, event: ClientEvent) {
        let (channel_id, generation) = match &event {
            ClientEvent::Data { channel_id, generation, .. }
            | ClientEvent::Closed { channel_id, generation, .. } => (*channel_id, *generation),
        };
        let is_current = matches!(
            self.active.get(&channel_id),
            Some(ChannelState::Connected { generation: active_gen, .. })
                if *active_gen == generation
        );
        if !is_current {
            return;
        }
        match event {
            ClientEvent::Data { channel_id, payload, .. } => {
                self.outgoing
                    .entry(channel_id)
                    .or_default()
                    .push_back(SenderMessage::Data { channel_id, payload });
            }
            ClientEvent::Closed { channel_id, error, .. } => {
                let reason = error.map(|e| {
                    let msg = format!("RCS socket read error for channel {channel_id}: {e:?}");
                    error!("{msg}");
                    msg
                });
                if let Some(state) = self.active.remove(&channel_id) {
                    if let ChannelState::Connected {
                        generation,
                        sender,
                        pending_incoming,
                        _writer_task,
                        ..
                    } = state
                    {
                        self.drain_connected_writer(
                            channel_id,
                            generation,
                            sender,
                            pending_incoming,
                            _writer_task,
                        );
                    }
                    self.enqueue_close(channel_id, reason);
                }
            }
        }
    }

    /// Selects the next outgoing [`SenderMessage`] to transmit over serial using round-robin
    /// arbitration across all channels with non-empty egress queues.
    fn select_next_message(&mut self) -> Option<SenderMessage> {
        let channel_id = self
            .last_channel_id
            .and_then(|last_id| last_id.checked_add(1))
            .and_then(|start_id| self.outgoing.range(start_id..).next().map(|(&k, _)| k))
            .or_else(|| self.outgoing.keys().next().copied())?;
        let (message, is_empty) = {
            let queue = self.outgoing.get_mut(&channel_id)?;
            (queue.pop_front(), queue.is_empty())
        };
        if is_empty {
            self.outgoing.remove(&channel_id);
        }
        if message.is_some() {
            self.last_channel_id = Some(channel_id);
        }
        message
    }

    /// Polls readiness of the outgoing serial sender sink and transfers the next round-robin
    /// queued message if the sink is ready to accept it.
    fn poll_send_next_queued(
        &mut self,
        sender_tx: &mut mpsc::Sender<SenderMessage>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if self.outgoing.is_empty() {
            return Poll::Pending;
        }
        match futures::sink::Sink::poll_ready(Pin::new(sender_tx), cx) {
            Poll::Ready(Ok(())) => match self.select_next_message() {
                Some(message) => {
                    let _ = futures::sink::Sink::start_send(Pin::new(sender_tx), message);
                    Poll::Ready(())
                }
                None => Poll::Pending,
            },
            Poll::Ready(Err(_)) => {
                self.outgoing.clear();
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    /// Polls internal readiness across both incoming socket staging queues and outgoing serial
    /// transmission queues.
    fn poll_ready_queues(
        &mut self,
        sender_tx: &mut mpsc::Sender<SenderMessage>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if self.poll_pending_incoming(cx).is_ready() {
            return Poll::Ready(());
        }
        self.poll_send_next_queued(sender_tx, cx)
    }
}

/// Awaits the next [`ClientEvent`] from active RCS socket readers.
///
/// Applies backpressure by stalling if the total number of queued outgoing messages across
/// all channels exceeds [`DEFAULT_CLIENT_DATA_CAPACITY`].
async fn next_client_event(
    client_rx: &mut mpsc::Receiver<ClientEvent>,
    total_queued: usize,
) -> Option<ClientEvent> {
    if total_queued < DEFAULT_CLIENT_DATA_CAPACITY {
        client_rx.next().await
    } else {
        futures::future::pending().await
    }
}

/// Multiplexes FDomain channels between incoming [`SerialEvent`]s and RCS toolbox
/// sockets via `rcs_connector`, scheduling outgoing [`SenderMessage`]s across
/// active channels in round-robin order.
///
/// # Errors
///
/// Returns `Ok(())` when `serial_rx` closes cleanly.
pub async fn coordinator_task(
    mut serial_rx: mpsc::Receiver<SerialEvent>,
    mut sender_tx: mpsc::Sender<SenderMessage>,
    rcs_connector: ConnectorProxy,
) -> Result<()> {
    let (internal_tx, mut internal_rx) = mpsc::channel::<InternalEvent>(DEFAULT_CHANNEL_CAPACITY);
    let (client_tx, mut client_rx) = mpsc::channel::<ClientEvent>(DEFAULT_CLIENT_DATA_CAPACITY);
    let mut state = CoordinatorState::new(client_tx, internal_tx);
    loop {
        let total_queued: usize = state.outgoing.values().map(|queue| queue.len()).sum();
        futures::select_biased! {
            internal_event = internal_rx.next() => if let Some(event) = internal_event {
                state.handle_internal_event(event);
            },
            _ = futures::future::poll_fn(|cx| state.poll_ready_queues(&mut sender_tx, cx)).fuse() => {}
            client_event = next_client_event(&mut client_rx, total_queued).fuse() => if let Some(event) = client_event {
                state.handle_client_event(event);
            },
            serial_event = serial_rx.next() => match serial_event {
                Some(event) => state.handle_serial_event(event, &rcs_connector),
                None => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_developer_remotecontrol_connector::{ConnectorMarker, ConnectorRequest};

    #[fuchsia::test]
    async fn test_coordinator_cancels_tasks_on_channel_close() {
        let (mut serial_tx, serial_rx) = mpsc::channel(10);
        let (sender_tx, mut sender_rx) = mpsc::channel(10);
        let (connector_proxy, mut connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        let coordinator_handle = fuchsia_async::Task::spawn(async move {
            coordinator_task(serial_rx, sender_tx, connector_proxy).await
        });
        let channel_id = 42u16;
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: b"init".to_vec() })
            .await
            .unwrap();
        let Some(Ok(ConnectorRequest::FdomainToolboxSocket { socket, responder })) =
            connector_stream.next().await
        else {
            panic!("Expected FdomainToolboxSocket request");
        };
        responder.send().unwrap();
        let mut rcs_socket = fuchsia_async::Socket::from_socket(socket);
        let mut init_buf = [0u8; 4];
        rcs_socket.read_exact(&mut init_buf).await.unwrap();
        assert_eq!(&init_buf, b"init");

        rcs_socket.write_all(b"hello").await.unwrap();
        assert_eq!(
            sender_rx.next().await.unwrap(),
            SenderMessage::Data { channel_id, payload: b"hello".to_vec() }
        );
        drop(rcs_socket);
        assert_eq!(
            sender_rx.next().await.unwrap(),
            SenderMessage::Close { channel_id, reason: None }
        );
        drop(serial_tx);
        coordinator_handle.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_coordinator_flushes_buffered_data_on_serial_close() {
        let (mut serial_tx, serial_rx) = mpsc::channel(10);
        let (sender_tx, _sender_rx) = mpsc::channel(10);
        let (connector_proxy, mut connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        let coordinator_handle = fuchsia_async::Task::spawn(async move {
            coordinator_task(serial_rx, sender_tx, connector_proxy).await
        });
        let channel_id = 7u16;
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: b"flush_me".to_vec() })
            .await
            .unwrap();
        serial_tx.send(SerialEvent::SerialClose { channel_id }).await.unwrap();

        let Some(Ok(ConnectorRequest::FdomainToolboxSocket { socket, responder })) =
            connector_stream.next().await
        else {
            panic!("Expected FdomainToolboxSocket request");
        };
        responder.send().unwrap();
        let mut rcs_socket = fuchsia_async::Socket::from_socket(socket);
        let mut received = Vec::new();
        rcs_socket.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"flush_me");
        drop(serial_tx);
        coordinator_handle.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_coordinator_channel_id_reuse_preserves_closing_generation() {
        let (mut serial_tx, serial_rx) = mpsc::channel(10);
        let (sender_tx, _sender_rx) = mpsc::channel(10);
        let (connector_proxy, mut connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        let coordinator_handle = fuchsia_async::Task::spawn(async move {
            coordinator_task(serial_rx, sender_tx, connector_proxy).await
        });
        let channel_id = 11u16;
        // Generation 1 opens, buffers data, and closes before registration completes.
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: b"gen1_payload".to_vec() })
            .await
            .unwrap();
        serial_tx.send(SerialEvent::SerialClose { channel_id }).await.unwrap();
        // Immediately reuse the same `channel_id` for Generation 2.
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: b"gen2_payload".to_vec() })
            .await
            .unwrap();

        // Both registration requests should arrive and receive their respective payloads.
        let Some(Ok(ConnectorRequest::FdomainToolboxSocket { socket: sock1, responder: resp1 })) =
            connector_stream.next().await
        else {
            panic!("Expected first FdomainToolboxSocket request");
        };
        let Some(Ok(ConnectorRequest::FdomainToolboxSocket { socket: sock2, responder: resp2 })) =
            connector_stream.next().await
        else {
            panic!("Expected second FdomainToolboxSocket request");
        };
        resp1.send().unwrap();
        resp2.send().unwrap();

        let mut rcs_sock1 = fuchsia_async::Socket::from_socket(sock1);
        let mut gen1_received = Vec::new();
        rcs_sock1.read_to_end(&mut gen1_received).await.unwrap();
        assert_eq!(gen1_received, b"gen1_payload");

        let mut rcs_sock2 = fuchsia_async::Socket::from_socket(sock2);
        let mut gen2_buf = [0u8; 12];
        rcs_sock2.read_exact(&mut gen2_buf).await.unwrap();
        assert_eq!(&gen2_buf, b"gen2_payload");

        drop(serial_tx);
        coordinator_handle.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_coordinator_enforces_pending_staging_limit() {
        let (mut serial_tx, serial_rx) = mpsc::channel(10);
        let (sender_tx, mut sender_rx) = mpsc::channel(10);
        let (connector_proxy, mut connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        let coordinator_handle = fuchsia_async::Task::spawn(async move {
            coordinator_task(serial_rx, sender_tx, connector_proxy).await
        });
        let channel_id = 9u16;
        let large_chunk = vec![0xAAu8; (MAX_CHANNEL_STAGING_BYTES / 2) + 1];
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: large_chunk.clone() })
            .await
            .unwrap();
        serial_tx.send(SerialEvent::SerialData { channel_id, data: large_chunk }).await.unwrap();
        assert_eq!(
            sender_rx.next().await.unwrap(),
            SenderMessage::Close {
                channel_id,
                reason: Some(format!(
                    "Pending channel {channel_id} exceeded max staging buffer ({MAX_CHANNEL_STAGING_BYTES} bytes)"
                )),
            }
        );
        assert!(connector_stream.next().now_or_never().is_none());

        drop(serial_tx);
        coordinator_handle.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_coordinator_propagates_registration_failure_reason() {
        let (mut serial_tx, serial_rx) = mpsc::channel(10);
        let (sender_tx, mut sender_rx) = mpsc::channel(10);
        let (connector_proxy, connector_stream) =
            fidl::endpoints::create_proxy_and_stream::<ConnectorMarker>();
        drop(connector_stream);
        let coordinator_handle = fuchsia_async::Task::spawn(async move {
            coordinator_task(serial_rx, sender_tx, connector_proxy).await
        });
        let channel_id = 12u16;
        serial_tx
            .send(SerialEvent::SerialData { channel_id, data: b"hello".to_vec() })
            .await
            .unwrap();
        match sender_rx.next().await.unwrap() {
            SenderMessage::Close { channel_id: id, reason: Some(reason) } => {
                assert_eq!(id, channel_id);
                assert!(
                    reason.contains("Failed to register RCS socket for channel 12"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("Expected Close with reason, got {other:?}"),
        }
        drop(serial_tx);
        coordinator_handle.await.unwrap();
    }

    #[fuchsia::test]
    async fn test_select_next_message_round_robin_when_channel_drained() {
        let (client_tx, _client_rx) = mpsc::channel(10);
        let (internal_tx, _internal_rx) = mpsc::channel(10);
        let mut state = CoordinatorState::new(client_tx, internal_tx);

        state
            .outgoing
            .entry(1)
            .or_default()
            .push_back(SenderMessage::Data { channel_id: 1, payload: b"ch1_a".to_vec() });
        state
            .outgoing
            .entry(1)
            .or_default()
            .push_back(SenderMessage::Data { channel_id: 1, payload: b"ch1_b".to_vec() });
        state
            .outgoing
            .entry(5)
            .or_default()
            .push_back(SenderMessage::Data { channel_id: 5, payload: b"ch5_only".to_vec() });
        state
            .outgoing
            .entry(10)
            .or_default()
            .push_back(SenderMessage::Data { channel_id: 10, payload: b"ch10_a".to_vec() });
        state
            .outgoing
            .entry(10)
            .or_default()
            .push_back(SenderMessage::Data { channel_id: 10, payload: b"ch10_b".to_vec() });

        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Data { channel_id: 1, payload: b"ch1_a".to_vec() })
        );
        // Draining channel 5 removes key 5 from `self.outgoing`.
        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Data { channel_id: 5, payload: b"ch5_only".to_vec() })
        );
        assert!(!state.outgoing.contains_key(&5));
        // Next message must advance to channel 10 rather than resetting to channel 1.
        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Data { channel_id: 10, payload: b"ch10_a".to_vec() })
        );
        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Data { channel_id: 1, payload: b"ch1_b".to_vec() })
        );
        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Data { channel_id: 10, payload: b"ch10_b".to_vec() })
        );
        assert_eq!(state.select_next_message(), None);
    }

    #[fuchsia::test]
    async fn test_client_close_drains_connected_writer_gracefully() {
        let (client_tx, _client_rx) = mpsc::channel(10);
        let (internal_tx, mut internal_rx) = mpsc::channel(10);
        let mut state = CoordinatorState::new(client_tx, internal_tx);

        let channel_id = 15u16;
        let generation = 1u64;
        let (local_socket, remote_socket) = fidl::Socket::create_stream();
        let socket = fuchsia_async::Socket::from_socket(local_socket);
        state.connect_socket(channel_id, generation, socket, vec![b"initial_".to_vec()], false);

        if let Some(ChannelState::Connected { pending_incoming, pending_bytes, .. }) =
            state.active.get_mut(&channel_id)
        {
            pending_incoming.push_back(b"staged".to_vec());
            *pending_bytes = 6;
        } else {
            panic!("Expected channel to be in Connected state");
        }

        state.handle_client_event(ClientEvent::Closed { channel_id, generation, error: None });
        assert!(!state.active.contains_key(&channel_id));
        assert!(state.closing_writers.contains_key(&(channel_id, generation)));
        assert_eq!(
            state.select_next_message(),
            Some(SenderMessage::Close { channel_id, reason: None })
        );

        let mut rcs_socket = fuchsia_async::Socket::from_socket(remote_socket);
        let mut received = [0u8; 14];
        rcs_socket.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"initial_staged");

        match internal_rx.next().await {
            Some(InternalEvent::WriterDone { channel_id: id, generation: done_gen, error }) => {
                assert_eq!(id, channel_id);
                assert_eq!(done_gen, generation);
                assert!(error.is_none());
                state.handle_internal_event(InternalEvent::WriterDone {
                    channel_id: id,
                    generation: done_gen,
                    error,
                });
            }
            other => panic!("Expected WriterDone, got {:?}", other),
        }
        assert!(!state.closing_writers.contains_key(&(channel_id, generation)));
    }
}
