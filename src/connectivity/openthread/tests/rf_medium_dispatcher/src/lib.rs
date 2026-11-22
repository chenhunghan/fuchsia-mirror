// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The simulated IEEE 802.15.4 medium: serves `test.lowpan.rfsim.Medium` to nodes and
//! `test.lowpan.rfsim.Monitor` to tests.
//!
//! Node events and transmissions are recorded only while a watcher for them exists. No history.
//! Watchers are owned by the medium and live until their own channel closes.
//!
//! Every connection is served by its own task on the medium's scope.
//!
//! Every transmitted frame is delivered at once to every other attached node, whatever its channel,
//! with `IDEAL_RSSI_DBM` and `IDEAL_LQI`, into a per-node receive queue of `MAX_RECEIVE_QUEUE`
//! frames; there is no airtime, collision or channel-energy model yet.

use fidl::endpoints::{RequestStream as _, Responder as _, ServerEnd};
use fidl_test_lowpan_rfsim as rfsim;
use fuchsia_async as fasync;
use futures::StreamExt as _;
use log::{error, info, warn};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque, btree_map};
use std::num::NonZeroUsize;
use std::rc::Rc;

// The FIDL limits as `usize`. They are `uint32`, which always fits in `usize` on Fuchsia, and `as`
// is the only conversion available in a const context.
const MAX_NODES: usize = rfsim::MAX_NODES as usize;
const MAX_NODE_EVENTS_PER_BATCH: usize = rfsim::MAX_NODE_EVENTS_PER_BATCH as usize;
const MAX_TRANSMISSIONS_PER_BATCH: usize = rfsim::MAX_TRANSMISSIONS_PER_BATCH as usize;

/// Node events queued per `NodeEventWatcher` between its `Watch` calls. Beyond this the watcher is
/// closed with `ZX_ERR_NO_RESOURCES`, since a lost event would leave its view of the medium wrong.
const MAX_NODE_EVENT_QUEUE: usize = 4 * MAX_NODE_EVENTS_PER_BATCH;

/// Transmissions queued per `TransmissionWatcher` between its `Watch` calls. Beyond this the
/// oldest are dropped and counted in the next reply.
const MAX_TRANSMISSION_QUEUE: usize = 16 * MAX_TRANSMISSIONS_PER_BATCH;

/// Frames queued per node between its `Radio.Receive` calls. Beyond this the oldest are dropped,
/// like a radio whose receive FIFO overflows. Nodes are expected to keep a `Receive` pending at all
/// times, so the queue only absorbs bursts while a node handles the previous frame. The value is
/// not tuned: it is ample for the bursts today's tests produce.
const MAX_RECEIVE_QUEUE: usize = 32;

/// Received signal strength every node observes of every frame delivered to it, in dBm: the
/// constant RSSI of the `Ideal` model in OTNS's `radiomodel` package, which every node in range
/// hears at.
const IDEAL_RSSI_DBM: i8 = -60;

/// Lowest channel of the 2.4 GHz O-QPSK band (`test.lowpan.rfsim/Channel`).
const MIN_CHANNEL: rfsim::Channel = 11;

/// Highest channel of the 2.4 GHz O-QPSK band (`test.lowpan.rfsim/Channel`).
const MAX_CHANNEL: rfsim::Channel = 26;

/// Link quality indicator every node observes of every frame delivered to it: the top of the
/// IEEE 802.15.4 LQI range, a perfect link, as in the `Ideal` model.
const IDEAL_LQI: u8 = 255;

/// The simulated medium. Should be created and used on a local executor.
///
/// Dropping it aborts every connection task.
pub struct MediumDispatcher {
    scope: fasync::Scope,
    shared: Rc<Shared>,
}

impl MediumDispatcher {
    pub fn new() -> Self {
        let scope = fasync::Scope::new_with_name("rf-medium-dispatcher");
        let shared =
            Rc::new(Shared { scope: scope.to_handle(), state: RefCell::new(State::default()) });
        Self { scope, shared }
    }

    /// Serves `test.lowpan.rfsim.Medium` on `stream` until the client closes it.
    pub fn serve_medium(&self, stream: rfsim::MediumRequestStream) {
        let _: fasync::JoinHandle<()> =
            self.scope.spawn_local(serve_medium(self.shared.clone(), stream));
    }

    /// Serves `test.lowpan.rfsim.Monitor` on `stream` until the client closes it.
    pub fn serve_monitor(&self, stream: rfsim::MonitorRequestStream) {
        let _: fasync::JoinHandle<()> =
            self.scope.spawn_local(serve_monitor(self.shared.clone(), stream));
    }
}

impl Default for MediumDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// What every connection task holds.
struct Shared {
    scope: fasync::ScopeHandle,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    nodes: BTreeMap<rfsim::NodeId, Node>,
    topology: Topology,
    node_event_watchers: Vec<Rc<RefCell<NodeEventWatcher>>>,
    transmission_watchers: Vec<Rc<RefCell<TransmissionWatcher>>>,
}

impl State {
    /// The nodes that hear `node_id`.
    fn neighbors(&self, node_id: rfsim::NodeId) -> impl Iterator<Item = rfsim::NodeId> + '_ {
        let Self { nodes, topology, .. } = self;
        nodes.keys().copied().filter(move |&id| topology.hears(id, node_id))
    }

    /// The nodes that hear `node_id`, with their state.
    fn neighbors_mut(
        &mut self,
        node_id: rfsim::NodeId,
    ) -> impl Iterator<Item = (rfsim::NodeId, &mut Node)> + '_ {
        // Borrowed apart from `nodes`, so that they can be iterated mutably.
        let Self { nodes, topology, .. } = self;
        nodes
            .iter_mut()
            .filter(move |(id, _)| topology.hears(**id, node_id))
            .map(|(&id, node)| (id, node))
    }
}

/// Who hears whom on the medium.
#[derive(Default)]
struct Topology {}

impl Topology {
    /// Whether `listener` hears what `transmitter` sends.
    fn hears(&self, listener: rfsim::NodeId, transmitter: rfsim::NodeId) -> bool {
        // TODO(https://fxbug.dev/581467997): Thread network topology support. Every attached node
        // hears every other one for now.
        listener != transmitter
    }
}

struct Node {
    attached_at: zx::MonotonicInstant,
    /// Frames delivered while no `Radio.Receive` was pending, oldest first.
    receive_queue: VecDeque<rfsim::ReceivedFrame>,
    /// Frames dropped from `receive_queue` since the last reply to `Radio.Receive`, or `None` if
    /// none were.
    dropped: Option<NonZeroUsize>,
    /// The outstanding `Radio.Receive`, if any.
    pending_receive: Option<rfsim::RadioReceiveResponder>,
}

impl Node {
    fn new(attached_at: zx::MonotonicInstant) -> Self {
        Self { attached_at, receive_queue: VecDeque::new(), dropped: None, pending_receive: None }
    }

    /// Queues `frame`, and hands it to the pending `Receive` if any. `node_id` is this node's id.
    fn deliver(&mut self, node_id: rfsim::NodeId, frame: rfsim::ReceivedFrame) {
        if self.receive_queue.len() >= MAX_RECEIVE_QUEUE {
            self.dropped = Some(match self.dropped {
                None => {
                    warn!(
                        "Node {node_id} fell {MAX_RECEIVE_QUEUE} frames behind; dropping its \
                         oldest frames"
                    );
                    NonZeroUsize::MIN
                }
                Some(dropped) => dropped.saturating_add(1),
            });
            let _: Option<rfsim::ReceivedFrame> = self.receive_queue.pop_front();
        }
        self.receive_queue.push_back(frame);
        self.maybe_reply(node_id);
    }

    /// Replies with the oldest queued frame, or keeps `responder` until a frame is delivered.
    /// `node_id` is this node's id.
    ///
    /// Fails with `ZX_ERR_BAD_STATE` if a `Receive` is already pending, leaving the radio open for
    /// the caller to close.
    fn receive(
        &mut self,
        node_id: rfsim::NodeId,
        responder: rfsim::RadioReceiveResponder,
    ) -> Result<(), zx::Status> {
        if self.pending_receive.is_some() {
            // Dropping the responder normally closes the channel, which would beat the caller's
            // epitaph.
            responder.drop_without_shutdown();
            return Err(zx::Status::BAD_STATE);
        }
        self.pending_receive = Some(responder);
        self.maybe_reply(node_id);
        Ok(())
    }

    /// Replies to the pending `Receive` with the oldest queued frame, if both exist. The only place
    /// that replies, so a `Receive` is never left pending while a frame is queued.
    fn maybe_reply(&mut self, node_id: rfsim::NodeId) {
        if self.receive_queue.is_empty() {
            return;
        }
        let Some(responder) = self.pending_receive.take() else {
            return;
        };
        let frame = self.receive_queue.pop_front().expect("queue is not empty");
        if let Some(dropped) = self.dropped.take() {
            warn!(
                "Dropped {dropped} frame(s) queued for node {node_id} since its last Radio.Receive"
            );
        }
        if let Err(e) = responder.send(&frame.frame, frame.rssi_dbm, frame.lqi) {
            log_fidl_error(&format!("Failed to reply to Radio.Receive for node {node_id}"), e);
        }
    }
}

fn now() -> zx::MonotonicInstant {
    fasync::MonotonicInstant::now().into_zx()
}

/// Logs a FIDL error at `error!`, unless it only means that the peer closed the channel. The log
/// record carries the caller's location.
#[track_caller]
fn log_fidl_error(context: &str, e: fidl::Error) {
    if !e.is_closed() {
        error!("{context}: {e:?}");
    }
}

/// Fails with `ZX_ERR_INVALID_ARGS` if `channel` is outside the band the medium models.
fn check_channel(node_id: rfsim::NodeId, channel: rfsim::Channel) -> Result<(), zx::Status> {
    if (MIN_CHANNEL..=MAX_CHANNEL).contains(&channel) {
        Ok(())
    } else {
        warn!("Node {node_id} used invalid channel {channel}");
        Err(zx::Status::INVALID_ARGS)
    }
}

impl Shared {
    fn attach(
        self: &Rc<Self>,
        node_id: rfsim::NodeId,
        radio: ServerEnd<rfsim::RadioMarker>,
    ) -> Result<(), zx::Status> {
        let attached_at = {
            let mut state = self.state.borrow_mut();
            let attached = state.nodes.len();
            let btree_map::Entry::Vacant(entry) = state.nodes.entry(node_id) else {
                warn!("Node {node_id} is already attached");
                return Err(zx::Status::ALREADY_EXISTS);
            };
            if attached >= MAX_NODES {
                warn!("Refusing node {node_id}: {attached} nodes already attached");
                return Err(zx::Status::NO_RESOURCES);
            }
            let attached_at = now();
            let _: &mut Node = entry.insert(Node::new(attached_at));
            attached_at
        };
        info!("Node {node_id} attached");
        let _: fasync::JoinHandle<()> =
            self.scope.spawn_local(serve_radio(self.clone(), node_id, radio.into_stream()));
        self.publish_node_event(rfsim::NodeEvent::Added(rfsim::Node {
            node_id,
            timestamp: attached_at,
        }));
        Ok(())
    }

    fn detach(&self, node_id: rfsim::NodeId) {
        // Drops the node's receive queue and pending `Receive`. Only the node's own radio task
        // detaches it, and only once.
        let _: Node = self
            .state
            .borrow_mut()
            .nodes
            .remove(&node_id)
            .unwrap_or_else(|| panic!("Cannot detach node {node_id}: not attached"));
        info!("Node {node_id} detached");
        self.publish_node_event(rfsim::NodeEvent::Removed(rfsim::Node {
            node_id,
            timestamp: now(),
        }));
    }

    fn publish_node_event(&self, event: rfsim::NodeEvent) {
        // `push` closes a watcher if its queue is full.
        for watcher in &self.state.borrow().node_event_watchers {
            watcher.borrow_mut().push(event.clone());
        }
    }

    /// Delivers `frame` to every other attached node, and records it.
    fn transmit(&self, node_id: rfsim::NodeId, frame: rfsim::Frame) -> Result<(), zx::Status> {
        check_channel(node_id, frame.channel)?;
        let mut state = self.state.borrow_mut();
        // Note that for now we deliver to all nodes regardless of what channel they listen on.
        let mut destinations = Vec::new();
        for (id, node) in state.neighbors_mut(node_id) {
            destinations.push(rfsim::Destination {
                node_id: id,
                rssi_dbm: IDEAL_RSSI_DBM,
                lqi: IDEAL_LQI,
            });
            node.deliver(
                id,
                rfsim::ReceivedFrame {
                    frame: frame.clone(),
                    rssi_dbm: IDEAL_RSSI_DBM,
                    lqi: IDEAL_LQI,
                },
            );
        }
        let transmission = rfsim::Transmission { node_id, frame, timestamp: now(), destinations };
        for watcher in &state.transmission_watchers {
            watcher.borrow_mut().push(transmission.clone());
        }
        Ok(())
    }

    fn receive(
        &self,
        node_id: rfsim::NodeId,
        responder: rfsim::RadioReceiveResponder,
    ) -> Result<(), zx::Status> {
        // Only `serve_radio` detaches the node, once it stops serving its radio.
        let mut state = self.state.borrow_mut();
        let node = state
            .nodes
            .get_mut(&node_id)
            .unwrap_or_else(|| panic!("Radio.Receive from node {node_id}, which is not attached"));
        node.receive(node_id, responder)
    }

    fn add_node_event_watcher(
        self: &Rc<Self>,
        server_end: ServerEnd<rfsim::NodeEventWatcherMarker>,
    ) {
        let stream = server_end.into_stream();
        let mut watcher = NodeEventWatcher::new(stream.control_handle());
        for (&node_id, node) in &self.state.borrow().nodes {
            watcher.push(rfsim::NodeEvent::Existing(rfsim::Node {
                node_id,
                timestamp: node.attached_at,
            }));
        }
        watcher.push(rfsim::NodeEvent::Idle(rfsim::Idle));
        // Registered right after the snapshot, with no await in between: no event can be missed.
        let registration = Registration::new(self, watcher);
        let _: fasync::JoinHandle<()> =
            self.scope.spawn_local(serve_node_event_watcher(registration, stream));
    }

    fn add_transmission_watcher(
        self: &Rc<Self>,
        server_end: ServerEnd<rfsim::TransmissionWatcherMarker>,
    ) {
        let stream = server_end.into_stream();
        let registration =
            Registration::new(self, TransmissionWatcher::new(stream.control_handle()));
        let _: fasync::JoinHandle<()> =
            self.scope.spawn_local(serve_transmission_watcher(registration, stream));
    }

    fn get_neighbors(&self, node_id: rfsim::NodeId) -> Result<Vec<rfsim::NodeId>, zx::Status> {
        let state = self.state.borrow();
        if !state.nodes.contains_key(&node_id) {
            return Err(zx::Status::NOT_FOUND);
        }
        Ok(state.neighbors(node_id).collect())
    }
}

async fn serve_medium(shared: Rc<Shared>, mut stream: rfsim::MediumRequestStream) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(rfsim::MediumRequest::Attach { node_id, radio, responder }) => {
                let result = shared.attach(node_id, radio).map_err(zx::Status::into_raw);
                if let Err(e) = responder.send(result) {
                    log_fidl_error("Failed to reply to Medium.Attach", e);
                }
            }
            Err(e) => {
                log_fidl_error("Medium request error", e);
                break;
            }
        }
    }
}

async fn serve_radio(
    shared: Rc<Shared>,
    node_id: rfsim::NodeId,
    mut stream: rfsim::RadioRequestStream,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(rfsim::RadioRequest::Transmit { channel, psdu, tx_power_dbm, responder }) => {
                match shared.transmit(node_id, rfsim::Frame { channel, psdu, tx_power_dbm }) {
                    Ok(()) => {
                        // Replied as soon as the frame is taken.
                        if let Err(e) = responder.send() {
                            log_fidl_error("Failed to reply to Radio.Transmit", e);
                        }
                    }
                    Err(status) => {
                        warn!(
                            "Radio.Transmit from node {node_id} failed: {status}; closing its radio"
                        );
                        responder.control_handle().shutdown_with_epitaph(status);
                        break;
                    }
                }
            }
            Ok(rfsim::RadioRequest::Receive { responder }) => {
                let control_handle = responder.control_handle().clone();
                if let Err(status) = shared.receive(node_id, responder) {
                    warn!("Radio.Receive from node {node_id} failed: {status}; closing its radio");
                    control_handle.shutdown_with_epitaph(status);
                    break;
                }
            }
            Err(e) => {
                log_fidl_error(&format!("Radio request error for node {node_id}"), e);
                break;
            }
        }
    }
    shared.detach(node_id);
}

async fn serve_monitor(shared: Rc<Shared>, mut stream: rfsim::MonitorRequestStream) {
    while let Some(request) = stream.next().await {
        let result = match request {
            Ok(rfsim::MonitorRequest::GetNodeEventWatcher { watcher, control_handle: _ }) => {
                shared.add_node_event_watcher(watcher);
                Ok(())
            }
            Ok(rfsim::MonitorRequest::GetTransmissionWatcher { watcher, responder }) => {
                shared.add_transmission_watcher(watcher);
                responder.send()
            }
            Ok(rfsim::MonitorRequest::GetNeighbors { node_id, responder }) => {
                responder.send(shared.get_neighbors(node_id).as_deref().map_err(|s| s.into_raw()))
            }
            Err(e) => {
                log_fidl_error("Monitor request error", e);
                break;
            }
        };
        if let Err(e) = result {
            log_fidl_error("Failed to reply to Monitor", e);
        }
    }
}

/// The list in `State` that a kind of watcher is registered in.
trait Registry: Sized {
    fn registry(state: &mut State) -> &mut Vec<Rc<RefCell<Self>>>;
}

impl Registry for NodeEventWatcher {
    fn registry(state: &mut State) -> &mut Vec<Rc<RefCell<Self>>> {
        &mut state.node_event_watchers
    }
}

impl Registry for TransmissionWatcher {
    fn registry(state: &mut State) -> &mut Vec<Rc<RefCell<Self>>> {
        &mut state.transmission_watchers
    }
}

/// A watcher registered in `State`, receiving what the medium publishes, for as long as this
/// value lives. Owned by the task serving the watcher, so it is unregistered however that task
/// ends.
struct Registration<W: Registry> {
    shared: Rc<Shared>,
    watcher: Rc<RefCell<W>>,
}

impl<W: Registry> Registration<W> {
    fn new(shared: &Rc<Shared>, watcher: W) -> Self {
        let watcher = Rc::new(RefCell::new(watcher));
        W::registry(&mut shared.state.borrow_mut()).push(watcher.clone());
        Self { shared: shared.clone(), watcher }
    }
}

impl<W: Registry> Drop for Registration<W> {
    fn drop(&mut self) {
        let mut state = self.shared.state.borrow_mut();
        let registry = W::registry(&mut state);
        let registered = registry.len();
        registry.retain(|w| !Rc::ptr_eq(w, &self.watcher));
        assert_eq!(registry.len() + 1, registered, "watcher must be registered exactly once");
    }
}

/// One `NodeEventWatcher`: the events not yet returned, and the hanging get that drains them.
enum NodeEventWatcher {
    Active {
        queue: VecDeque<rfsim::NodeEvent>,
        pending: Option<rfsim::NodeEventWatcherWatchResponder>,
        control_handle: rfsim::NodeEventWatcherControlHandle,
    },
    /// Closed with an epitaph; waiting for its serving task to end.
    Closed,
}

impl NodeEventWatcher {
    fn new(control_handle: rfsim::NodeEventWatcherControlHandle) -> Self {
        Self::Active { queue: VecDeque::new(), pending: None, control_handle }
    }

    /// Closes the channel with `epitaph` and drops everything queued.
    fn close(&mut self, epitaph: zx::Status) {
        if let Self::Active { pending, control_handle, .. } = std::mem::replace(self, Self::Closed)
        {
            if let Some(responder) = pending {
                // Dropping the responder normally closes the channel, which would beat the
                // epitaph.
                responder.drop_without_shutdown();
            }
            control_handle.shutdown_with_epitaph(epitaph);
        }
    }

    fn push(&mut self, event: rfsim::NodeEvent) {
        let Self::Active { queue, .. } = self else { return };
        if queue.len() >= MAX_NODE_EVENT_QUEUE {
            warn!("NodeEventWatcher fell {} events behind; closing it", queue.len());
            self.close(zx::Status::NO_RESOURCES);
            return;
        }
        queue.push_back(event);
        self.maybe_reply();
    }

    /// Fails with `ZX_ERR_BAD_STATE`, closing the watcher with that epitaph, if a `Watch` is
    /// already pending.
    fn watch(
        &mut self,
        responder: rfsim::NodeEventWatcherWatchResponder,
    ) -> Result<(), zx::Status> {
        let Self::Active { pending, .. } = self else {
            // Closing shuts the request stream down, so it yields no request after that.
            unreachable!("Watch on a closed watcher");
        };
        if pending.is_some() {
            // Dropping the responder normally closes the channel, which would beat the epitaph.
            responder.drop_without_shutdown();
            self.close(zx::Status::BAD_STATE);
            return Err(zx::Status::BAD_STATE);
        }
        *pending = Some(responder);
        self.maybe_reply();
        Ok(())
    }

    fn maybe_reply(&mut self) {
        let Self::Active { queue, pending, .. } = self else { return };
        if queue.is_empty() {
            return;
        }
        let Some(responder) = pending.take() else { return };
        let count = queue.len().min(MAX_NODE_EVENTS_PER_BATCH);
        let batch: Vec<_> = queue.drain(..count).collect();
        if let Err(e) = responder.send(&batch) {
            log_fidl_error("Failed to reply to NodeEventWatcher.Watch", e);
        }
    }
}

async fn serve_node_event_watcher(
    registration: Registration<NodeEventWatcher>,
    mut stream: rfsim::NodeEventWatcherRequestStream,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(rfsim::NodeEventWatcherRequest::Watch { responder }) => {
                let result = registration.watcher.borrow_mut().watch(responder);
                if let Err(status) = result {
                    // The watcher closed itself with `status` as the epitaph.
                    warn!("Closed NodeEventWatcher: {status}");
                    break;
                }
            }
            Err(e) => {
                log_fidl_error("NodeEventWatcher request error", e);
                break;
            }
        }
    }
}

/// One `TransmissionWatcher`: a bounded queue that drops its oldest entries when full, counting
/// them for the next reply, and the hanging get that drains it.
enum TransmissionWatcher {
    Active {
        queue: VecDeque<rfsim::Transmission>,
        dropped: u32,
        pending: Option<rfsim::TransmissionWatcherWatchResponder>,
        control_handle: rfsim::TransmissionWatcherControlHandle,
    },
    /// Closed with an epitaph; waiting for its serving task to end.
    Closed,
}

impl TransmissionWatcher {
    fn new(control_handle: rfsim::TransmissionWatcherControlHandle) -> Self {
        Self::Active { queue: VecDeque::new(), dropped: 0, pending: None, control_handle }
    }

    /// Closes the channel with `epitaph` and drops everything queued.
    fn close(&mut self, epitaph: zx::Status) {
        if let Self::Active { pending, control_handle, .. } = std::mem::replace(self, Self::Closed)
        {
            if let Some(responder) = pending {
                // Dropping the responder normally closes the channel, which would beat the
                // epitaph.
                responder.drop_without_shutdown();
            }
            control_handle.shutdown_with_epitaph(epitaph);
        }
    }

    fn push(&mut self, transmission: rfsim::Transmission) {
        let Self::Active { queue, dropped, .. } = self else { return };
        if queue.len() >= MAX_TRANSMISSION_QUEUE {
            if *dropped == 0 {
                warn!(
                    "TransmissionWatcher fell {MAX_TRANSMISSION_QUEUE} transmissions behind; \
                     dropping the oldest"
                );
            }
            let _: Option<rfsim::Transmission> = queue.pop_front();
            *dropped = dropped.saturating_add(1);
        }
        queue.push_back(transmission);
        self.maybe_reply();
    }

    /// Fails with `ZX_ERR_BAD_STATE`, closing the watcher with that epitaph, if a `Watch` is
    /// already pending.
    fn watch(
        &mut self,
        responder: rfsim::TransmissionWatcherWatchResponder,
    ) -> Result<(), zx::Status> {
        let Self::Active { pending, .. } = self else {
            // Closing shuts the request stream down, so it yields no request after that.
            unreachable!("Watch on a closed watcher");
        };
        if pending.is_some() {
            // Dropping the responder normally closes the channel, which would beat the epitaph.
            responder.drop_without_shutdown();
            self.close(zx::Status::BAD_STATE);
            return Err(zx::Status::BAD_STATE);
        }
        *pending = Some(responder);
        self.maybe_reply();
        Ok(())
    }

    fn maybe_reply(&mut self) {
        let Self::Active { queue, dropped, pending, .. } = self else { return };
        if queue.is_empty() {
            return;
        }
        let Some(responder) = pending.take() else { return };
        let count = queue.len().min(MAX_TRANSMISSIONS_PER_BATCH);
        let batch: Vec<_> = queue.drain(..count).collect();
        if let Err(e) = responder.send(&batch, std::mem::take(dropped)) {
            log_fidl_error("Failed to reply to TransmissionWatcher.Watch", e);
        }
    }
}

async fn serve_transmission_watcher(
    registration: Registration<TransmissionWatcher>,
    mut stream: rfsim::TransmissionWatcherRequestStream,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(rfsim::TransmissionWatcherRequest::Watch { responder }) => {
                let result = registration.watcher.borrow_mut().watch(responder);
                if let Err(status) = result {
                    // The watcher closed itself with `status` as the epitaph.
                    warn!("Closed TransmissionWatcher: {status}");
                    break;
                }
            }
            Err(e) => {
                log_fidl_error("TransmissionWatcher request error", e);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests;
