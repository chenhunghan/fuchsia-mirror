// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Unit tests for [`MediumDispatcher`], driven over FIDL on a fake-time test executor.

use super::*;
use assert_matches::assert_matches;
use fidl::endpoints::{Proxy as _, create_proxy, create_proxy_and_stream};
use futures::future;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::pin;
use std::task::Poll;

const NODE_A: rfsim::NodeId = 7;
const NODE_B: rfsim::NodeId = 3;
const NODE_C: rfsim::NodeId = 9;
const NODE_D: rfsim::NodeId = 5;
const CHANNEL: rfsim::Channel = 11;

/// What one `TransmissionWatcher.Watch` returns.
type TransmissionBatch = (Vec<rfsim::Transmission>, u32);

/// The medium, its clients and the executor they all run on.
struct Harness {
    /// Radios of the nodes attached through the harness, by node id.
    radios: HashMap<rfsim::NodeId, rfsim::RadioProxy>,
    monitor: rfsim::MonitorProxy,
    medium_client: rfsim::MediumProxy,
    medium: MediumDispatcher,
    exec: fasync::TestExecutor,
}

impl Harness {
    fn new() -> Self {
        let exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));
        let medium = MediumDispatcher::new();
        let (medium_client, stream) = create_proxy_and_stream::<rfsim::MediumMarker>();
        medium.serve_medium(stream);
        let (monitor, stream) = create_proxy_and_stream::<rfsim::MonitorMarker>();
        medium.serve_monitor(stream);
        Self { radios: HashMap::new(), monitor, medium_client, medium, exec }
    }

    /// Runs every task until none can make progress.
    fn run_until_stalled(&mut self) {
        let _: Poll<()> = self.exec.run_until_stalled(&mut future::pending::<()>());
    }

    fn poll<F: Future + Unpin>(&mut self, fut: &mut F) -> Poll<F::Output> {
        self.exec.run_until_stalled(fut)
    }

    #[track_caller]
    fn ready<F: Future>(&mut self, fut: F) -> F::Output {
        let mut fut = pin!(fut);
        match self.exec.run_until_stalled(&mut fut) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("future did not complete"),
        }
    }

    fn advance(&mut self, duration: zx::MonotonicDuration) {
        self.exec.set_fake_time(fasync::MonotonicInstant::now() + duration);
    }

    /// Attaches `node_id`; on success keeps its radio in `radios`.
    #[track_caller]
    fn attach(&mut self, node_id: rfsim::NodeId) -> Result<(), zx::Status> {
        let (radio, server_end) = create_proxy::<rfsim::RadioMarker>();
        let attach = self.medium_client.attach(node_id, server_end);
        let result = self.ready(attach).expect("Medium.Attach").map_err(zx::Status::err_from_raw);
        self.run_until_stalled();
        if result.is_ok() {
            assert_matches!(
                self.radios.insert(node_id, radio),
                None,
                "node {node_id} already has a radio"
            );
        }
        result
    }

    #[track_caller]
    fn radio(&self, node_id: rfsim::NodeId) -> rfsim::RadioProxy {
        self.radios.get(&node_id).expect("node not attached").clone()
    }

    /// Detaches `node_id` by closing its radio.
    #[track_caller]
    fn detach(&mut self, node_id: rfsim::NodeId) {
        let _: rfsim::RadioProxy = self.radios.remove(&node_id).expect("node not attached");
        self.run_until_stalled();
    }

    /// Transmits `frame` from `node_id`'s radio and waits for the medium's reply.
    #[track_caller]
    fn transmit(&mut self, node_id: rfsim::NodeId, frame: &rfsim::Frame) {
        let radio = self.radio(node_id);
        let rfsim::Frame { channel, psdu, tx_power_dbm } = frame;
        self.ready(radio.transmit(*channel, psdu, *tx_power_dbm)).expect("Radio.Transmit");
        self.run_until_stalled();
    }

    #[track_caller]
    fn node_event_watcher(&mut self) -> rfsim::NodeEventWatcherProxy {
        let (watcher, server_end) = create_proxy::<rfsim::NodeEventWatcherMarker>();
        self.monitor.get_node_event_watcher(server_end).expect("GetNodeEventWatcher");
        self.run_until_stalled();
        watcher
    }

    #[track_caller]
    fn transmission_watcher(&mut self) -> rfsim::TransmissionWatcherProxy {
        let (watcher, server_end) = create_proxy::<rfsim::TransmissionWatcherMarker>();
        // The reply means the watcher is installed.
        let get = self.monitor.get_transmission_watcher(server_end);
        self.ready(get).expect("GetTransmissionWatcher");
        watcher
    }

    /// A `Watch` that must complete now.
    #[track_caller]
    fn watch_nodes(&mut self, watcher: &rfsim::NodeEventWatcherProxy) -> Vec<rfsim::NodeEvent> {
        self.ready(watcher.watch()).expect("NodeEventWatcher.Watch")
    }

    /// A `Watch` that must complete now.
    #[track_caller]
    fn watch_transmissions(
        &mut self,
        watcher: &rfsim::TransmissionWatcherProxy,
    ) -> TransmissionBatch {
        self.ready(watcher.watch()).expect("TransmissionWatcher.Watch")
    }

    /// Consumes a new watcher's initial `existing`... `idle` sequence, however it is batched, and
    /// returns the existing nodes sorted by id (the API promises no order), plus whatever
    /// followed `idle` in the same reply.
    #[track_caller]
    fn drain_initial(
        &mut self,
        watcher: &rfsim::NodeEventWatcherProxy,
    ) -> (Vec<rfsim::Node>, Vec<rfsim::NodeEvent>) {
        let mut existing = Vec::new();
        loop {
            let mut events = self.watch_nodes(watcher).into_iter();
            while let Some(event) = events.next() {
                match event {
                    rfsim::NodeEvent::Existing(node) => existing.push(node),
                    rfsim::NodeEvent::Idle(rfsim::Idle) => {
                        existing.sort_by_key(|node| node.node_id);
                        return (existing, events.collect());
                    }
                    other => panic!("{other:?} before idle"),
                }
            }
        }
    }

    /// The attached nodes, as the `existing` snapshot of a new watcher.
    #[track_caller]
    fn attached_ids(&mut self) -> BTreeSet<rfsim::NodeId> {
        let watcher = self.node_event_watcher();
        self.existing_ids(&watcher)
    }

    /// `drain_initial` for a watcher whose initial reply must be complete.
    #[track_caller]
    fn existing_ids(&mut self, watcher: &rfsim::NodeEventWatcherProxy) -> BTreeSet<rfsim::NodeId> {
        let (existing, rest) = self.drain_initial(watcher);
        assert_eq!(rest, vec![], "events after idle in the initial reply");
        existing.iter().map(|node| node.node_id).collect()
    }

    /// How many node event and transmission watchers the medium has registered.
    fn watcher_counts(&self) -> (usize, usize) {
        let state = self.medium.shared.state.borrow();
        (state.node_event_watchers.len(), state.transmission_watchers.len())
    }
}

/// A frame on `CHANNEL` at 0 dBm.
fn frame(psdu: &[u8]) -> rfsim::Frame {
    rfsim::Frame { channel: CHANNEL, psdu: psdu.to_vec(), tx_power_dbm: 0 }
}

/// The transmission a watcher captures for `frame` sent by `node_id` at `timestamp`; with no
/// destinations, since there is no delivery yet.
fn captured(
    node_id: rfsim::NodeId,
    frame: &rfsim::Frame,
    timestamp: zx::MonotonicInstant,
) -> rfsim::Transmission {
    rfsim::Transmission { node_id, frame: frame.clone(), timestamp, destinations: vec![] }
}

fn added(node_id: rfsim::NodeId, timestamp: zx::MonotonicInstant) -> rfsim::NodeEvent {
    rfsim::NodeEvent::Added(rfsim::Node { node_id, timestamp })
}

fn removed(node_id: rfsim::NodeId, timestamp: zx::MonotonicInstant) -> rfsim::NodeEvent {
    rfsim::NodeEvent::Removed(rfsim::Node { node_id, timestamp })
}

/// The epitaph a closed client saw; `PEER_CLOSED` if there was none.
#[track_caller]
fn closed_with<T: std::fmt::Debug>(result: Result<T, fidl::Error>) -> zx::Status {
    let epitaph =
        assert_matches!(result, Err(fidl::Error::ClientChannelClosed { epitaph, .. }) => epitaph);
    match epitaph {
        fidl::Epitaph::Explicit(Ok(())) => panic!("closed with a ZX_OK epitaph"),
        fidl::Epitaph::Explicit(Err(status)) => status,
        fidl::Epitaph::PeerClosed => zx::Status::PEER_CLOSED,
    }
}

/// A two-byte PSDU encoding `seq`, little-endian.
fn seq_psdu(seq: u16) -> [u8; 2] {
    seq.to_le_bytes()
}

#[test]
fn duplicate_node_id_is_rejected() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    assert_eq!(h.attach(NODE_A), Err(zx::Status::ALREADY_EXISTS));
    assert_eq!(h.attached_ids(), BTreeSet::from([NODE_A]));
}

#[test]
fn attach_beyond_max_nodes_is_refused() {
    let mut h = Harness::new();
    for node_id in 0..rfsim::MAX_NODES {
        assert_eq!(h.attach(node_id), Ok(()), "node {node_id}");
    }
    assert_eq!(h.attach(rfsim::MAX_NODES), Err(zx::Status::NO_RESOURCES));
    assert_eq!(h.attached_ids(), (0..rfsim::MAX_NODES).collect::<BTreeSet<_>>());

    // Detaching frees a slot, which a new node can take.
    h.detach(0);
    assert_eq!(h.attached_ids(), (1..rfsim::MAX_NODES).collect::<BTreeSet<_>>());
    assert_eq!(h.attach(rfsim::MAX_NODES), Ok(()));
    assert_eq!(h.attached_ids(), (1..=rfsim::MAX_NODES).collect::<BTreeSet<_>>());
}

#[test]
fn duplicate_is_reported_before_full() {
    let mut h = Harness::new();
    for node_id in 0..rfsim::MAX_NODES {
        assert_eq!(h.attach(node_id), Ok(()), "node {node_id}");
    }
    assert_eq!(h.attach(0), Err(zx::Status::ALREADY_EXISTS));
}

#[test]
fn closing_radio_detaches_node() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    assert_eq!(h.attach(NODE_B), Ok(()));
    h.detach(NODE_A);
    assert_eq!(h.attached_ids(), BTreeSet::from([NODE_B]));
    // The id is free again.
    assert_eq!(h.attach(NODE_A), Ok(()));
    assert_eq!(h.attached_ids(), BTreeSet::from([NODE_A, NODE_B]));
}

#[test]
fn transmit_is_captured_by_watcher() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));

    let watcher = h.transmission_watcher();
    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "no transmissions yet, must hang");

    h.advance(zx::MonotonicDuration::from_millis(5));
    let frame = rfsim::Frame { channel: 15, psdu: vec![0x41, 0x88, 0x01], tx_power_dbm: -7 };
    h.transmit(NODE_A, &frame);

    // Stamped with the executor's clock at the time of the Transmit.
    let batch = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(batch)) => batch);
    assert_eq!(batch, (vec![captured(NODE_A, &frame, now())], 0));
}

// Watchers see transmissions in the order the medium received them, across nodes, starting from
// their registration.
#[test]
fn transmissions_follow_medium_order() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    assert_eq!(h.attach(NODE_B), Ok(()));
    let frames: Vec<_> = (1..=5).map(|i| frame(&[i])).collect();
    // Transmissions before any watcher exists are not captured.
    h.transmit(NODE_A, &frames[0]);
    h.transmit(NODE_B, &frames[1]);

    let first = h.transmission_watcher();
    h.transmit(NODE_B, &frames[2]);
    h.transmit(NODE_A, &frames[3]);
    let second = h.transmission_watcher();
    h.transmit(NODE_A, &frames[4]);

    let now = now();
    assert_eq!(
        h.watch_transmissions(&first),
        (
            vec![
                captured(NODE_B, &frames[2], now),
                captured(NODE_A, &frames[3], now),
                captured(NODE_A, &frames[4], now),
            ],
            0
        )
    );

    // The second watcher only sees what came after its registration.
    assert_eq!(h.watch_transmissions(&second), (vec![captured(NODE_A, &frames[4], now)], 0));
}

#[test]
fn second_outstanding_receive_closes_radio() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let radio = h.radio(NODE_A);
    let mut first = pin!(radio.receive());
    assert_matches!(h.poll(&mut first), Poll::Pending);
    assert_eq!(h.attached_ids(), BTreeSet::from([NODE_A]));

    let mut second = pin!(radio.receive());
    let second = assert_matches!(h.poll(&mut second), Poll::Ready(result) => result);
    assert_eq!(closed_with(second), zx::Status::BAD_STATE);
    // A closed radio is a detached node.
    h.run_until_stalled();
    assert_eq!(h.attached_ids(), BTreeSet::new());
}

#[test]
fn get_neighbors_of_unattached_node_is_not_found() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let get = h.monitor.get_neighbors(NODE_B);
    let result = h.ready(get).expect("GetNeighbors");
    assert_eq!(result, Err(zx::Status::NOT_FOUND.into_raw()));
}

#[test]
fn node_event_watcher_starts_idle_then_reports_attach_and_detach() {
    let mut h = Harness::new();
    let watcher = h.node_event_watcher();
    // Nothing is attached: the initial sequence is just `idle`.
    assert_eq!(h.existing_ids(&watcher), BTreeSet::new());

    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "nothing attached yet, must hang");

    let attach_time = now();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let events = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(events)) => events);
    assert_eq!(events, vec![added(NODE_A, attach_time)]);

    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "nothing changed, must hang");

    h.advance(zx::MonotonicDuration::from_seconds(1));
    let detach_time = now();
    h.detach(NODE_A);
    let events = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(events)) => events);
    assert_eq!(events, vec![removed(NODE_A, detach_time)]);

    // Several events between calls arrive as one batch, oldest first.
    assert_eq!(h.attach(NODE_B), Ok(()));
    assert_eq!(h.attach(NODE_D), Ok(()));
    let now = now();
    assert_eq!(h.watch_nodes(&watcher), vec![added(NODE_B, now), added(NODE_D, now)]);
}

// A watcher created after nodes came and went first learns the nodes attached right now, as
// `existing` events carrying their original attach time, then `idle`, and from then on sees only
// what happens afterwards: no history is kept.
#[test]
fn node_event_watcher_starts_from_existing_nodes() {
    let mut h = Harness::new();
    let first_attach = now();
    assert_eq!(h.attach(NODE_A), Ok(()));
    h.advance(zx::MonotonicDuration::from_seconds(1));
    assert_eq!(h.attach(NODE_B), Ok(()));
    h.advance(zx::MonotonicDuration::from_seconds(1));
    assert_eq!(h.attach(NODE_C), Ok(()));
    h.advance(zx::MonotonicDuration::from_seconds(1));
    h.detach(NODE_B); // Gone before the watcher exists and is not reported.

    let watcher = h.node_event_watcher();
    // An attach between registration and the first Watch is a live event after `idle`.
    let late_attach = now();
    assert_eq!(h.attach(NODE_D), Ok(()));

    let (existing, rest) = h.drain_initial(&watcher);
    assert_eq!(
        existing,
        vec![
            rfsim::Node { node_id: NODE_A, timestamp: first_attach },
            rfsim::Node {
                node_id: NODE_C,
                timestamp: first_attach + zx::MonotonicDuration::from_seconds(2)
            },
        ]
    );
    assert_eq!(rest, vec![added(NODE_D, late_attach)]);

    // From here on only new events: the snapshot is not repeated.
    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending);
    h.detach(NODE_A);
    let events = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(events)) => events);
    assert_eq!(events, vec![removed(NODE_A, now())]);
}

// A backlog longer than one batch: the first reply is full, the next returns the remainder
// immediately, and only then does the hanging get hang.
#[test]
fn node_event_watcher_splits_backlog_into_batches() {
    const _: () = assert!(4 * rfsim::MAX_NODES == rfsim::MAX_NODE_EVENTS_PER_BATCH);
    let mut h = Harness::new();
    let watcher = h.node_event_watcher();
    assert_eq!(h.existing_ids(&watcher), BTreeSet::new());
    // Two rounds of attach all, detach all: exactly one batch of events, then one more.
    for _ in 0..2 {
        for node_id in 0..rfsim::MAX_NODES {
            assert_eq!(h.attach(node_id), Ok(()), "node {node_id}");
        }
        for node_id in 0..rfsim::MAX_NODES {
            h.detach(node_id);
        }
    }
    const LAST_NODE: rfsim::NodeId = 1000;
    assert_eq!(h.attach(LAST_NODE), Ok(()));

    // The clock never moved: every event carries the same timestamp.
    let now = now();
    let round: Vec<_> = (0..rfsim::MAX_NODES)
        .map(|node_id| added(node_id, now))
        .chain((0..rfsim::MAX_NODES).map(|node_id| removed(node_id, now)))
        .collect();
    assert_eq!(h.watch_nodes(&watcher), [round.clone(), round].concat());

    assert_eq!(
        h.watch_nodes(&watcher),
        vec![added(LAST_NODE, now)],
        "remainder must be returned immediately"
    );

    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "backlog drained, must hang");
}

#[test]
fn node_event_watcher_reports_every_existing_node() {
    let mut h = Harness::new();
    for node_id in 0..rfsim::MAX_NODES {
        assert_eq!(h.attach(node_id), Ok(()), "node {node_id}");
    }
    let watcher = h.node_event_watcher();
    let (existing, rest) = h.drain_initial(&watcher);
    let now = now();
    assert_eq!(
        existing,
        (0..rfsim::MAX_NODES)
            .map(|node_id| rfsim::Node { node_id, timestamp: now })
            .collect::<Vec<_>>()
    );
    assert_eq!(rest, vec![]);
    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "snapshot delivered, must hang");
}

// A watcher that never calls `Watch` is closed, not grown without bound, once its queue is full.
#[test]
fn node_event_watcher_is_closed_when_queue_overflows() {
    let mut h = Harness::new();
    let watcher = h.node_event_watcher();
    // The initial `idle` occupies one slot; attach/detach cycles of one node add two each.
    const _: () = assert!((MAX_NODE_EVENT_QUEUE - 1) % 2 == 1);
    for _ in 0..(MAX_NODE_EVENT_QUEUE - 1) / 2 {
        assert_eq!(h.attach(NODE_A), Ok(()));
        h.detach(NODE_A);
    }
    // One more attach fills the last slot exactly.
    assert_eq!(h.attach(NODE_A), Ok(()));
    assert!(!watcher.is_closed(), "queue exactly full is still fine");

    h.detach(NODE_A);
    assert!(watcher.is_closed());
    let event = h.ready(watcher.take_event_stream().next());
    assert_eq!(closed_with(event.expect("epitaph")), zx::Status::NO_RESOURCES);

    // The medium keeps serving other watchers; a new one starts from the current state, not from
    // the churn that closed the previous one.
    assert_eq!(h.attach(NODE_A), Ok(()));
    let fresh = h.node_event_watcher();
    assert_eq!(h.existing_ids(&fresh), BTreeSet::from([NODE_A]));
    let mut watch = pin!(fresh.watch());
    assert_matches!(
        h.poll(&mut watch),
        Poll::Pending,
        "nothing happened since registration, must hang"
    );
    h.detach(NODE_A);
    let events = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(events)) => events);
    assert_eq!(events, vec![removed(NODE_A, now())]);
}

// Capture starts when the watcher is registered; the first Watch hangs until something is
// captured, later ones return everything since the previous reply as one batch, oldest first.
#[test]
fn transmission_watcher_captures_from_registration_in_batches() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let frames: Vec<_> = (1..=5).map(|i| frame(&[i])).collect();
    // Before any watcher exists: not recorded at all.
    h.transmit(NODE_A, &frames[0]);
    h.transmit(NODE_A, &frames[1]);

    let watcher = h.transmission_watcher();
    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "nothing captured yet, must hang");
    h.transmit(NODE_A, &frames[2]);
    let now = now();
    let batch = assert_matches!(h.poll(&mut watch), Poll::Ready(Ok(batch)) => batch);
    assert_eq!(batch, (vec![captured(NODE_A, &frames[2], now)], 0));

    // Several transmissions between calls arrive as one batch, oldest first.
    h.transmit(NODE_A, &frames[3]);
    h.transmit(NODE_A, &frames[4]);
    assert_eq!(
        h.watch_transmissions(&watcher),
        (vec![captured(NODE_A, &frames[3], now), captured(NODE_A, &frames[4], now)], 0)
    );

    // Nothing new: hangs again.
    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending);
}

#[test]
fn transmission_watcher_splits_backlog_into_batches() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let watcher = h.transmission_watcher();
    let per_batch =
        u16::try_from(rfsim::MAX_TRANSMISSIONS_PER_BATCH).expect("batch size fits in u16");
    let frames: Vec<_> = (0..per_batch + 3).map(|seq| frame(&seq_psdu(seq))).collect();
    for frame in &frames {
        h.transmit(NODE_A, frame);
    }

    let now = now();
    let expected: Vec<_> = frames.iter().map(|frame| captured(NODE_A, frame, now)).collect();
    let (first, rest) = expected.split_at(per_batch.into());
    assert_eq!(h.watch_transmissions(&watcher), (first.to_vec(), 0));
    assert_eq!(
        h.watch_transmissions(&watcher),
        (rest.to_vec(), 0),
        "remainder must be returned immediately"
    );

    let mut watch = pin!(watcher.watch());
    assert_matches!(h.poll(&mut watch), Poll::Pending, "backlog drained, must hang");
}

// A slow watcher loses the oldest transmissions, and is told how many, rather than being closed
// or letting the queue grow without bound.
#[test]
fn transmission_watcher_drops_oldest_when_queue_overflows() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let watcher = h.transmission_watcher();
    const DROPPED: u16 = 3;
    let queue = u16::try_from(MAX_TRANSMISSION_QUEUE).expect("queue size fits in u16");
    for seq in 0..queue + DROPPED {
        h.transmit(NODE_A, &frame(&seq_psdu(seq)));
    }

    // The oldest DROPPED are gone; the queue starts at sequence number DROPPED.
    let now = now();
    let per_batch =
        u16::try_from(rfsim::MAX_TRANSMISSIONS_PER_BATCH).expect("batch size fits in u16");
    let expected: Vec<_> = (DROPPED..DROPPED + per_batch)
        .map(|seq| captured(NODE_A, &frame(&seq_psdu(seq)), now))
        .collect();
    assert_eq!(h.watch_transmissions(&watcher), (expected, DROPPED.into()));

    // The count is per reply: nothing was dropped since.
    let expected: Vec<_> = (DROPPED + per_batch..DROPPED + 2 * per_batch)
        .map(|seq| captured(NODE_A, &frame(&seq_psdu(seq)), now))
        .collect();
    assert_eq!(h.watch_transmissions(&watcher), (expected, 0));
}

#[test]
fn second_outstanding_node_event_watch_closes_watcher() {
    let mut h = Harness::new();
    let watcher = h.node_event_watcher();
    assert_eq!(h.existing_ids(&watcher), BTreeSet::new());

    // The first call hangs (nothing attached); a second one is a protocol violation.
    let mut first = pin!(watcher.watch());
    assert_matches!(h.poll(&mut first), Poll::Pending);
    let mut second = pin!(watcher.watch());
    let second = assert_matches!(h.poll(&mut second), Poll::Ready(result) => result);
    assert_eq!(closed_with(second), zx::Status::BAD_STATE);
}

#[test]
fn second_outstanding_transmission_watch_closes_watcher() {
    let mut h = Harness::new();
    let watcher = h.transmission_watcher();

    // The first call hangs (no transmissions); a second one is a protocol violation.
    let mut first = pin!(watcher.watch());
    assert_matches!(h.poll(&mut first), Poll::Pending);
    let mut second = pin!(watcher.watch());
    let second = assert_matches!(h.poll(&mut second), Poll::Ready(result) => result);
    assert_eq!(closed_with(second), zx::Status::BAD_STATE);
}

// Watchers that go away with a hanging get outstanding are torn down cleanly, and the medium
// keeps serving others.
#[test]
fn closing_watchers_with_pending_watch_is_harmless() {
    let mut h = Harness::new();
    {
        let doomed_node_events = h.node_event_watcher();
        let doomed_transmissions = h.transmission_watcher();
        assert_eq!(h.existing_ids(&doomed_node_events), BTreeSet::new());
        let mut node_watch = pin!(doomed_node_events.watch());
        let mut transmission_watch = pin!(doomed_transmissions.watch());
        assert_matches!(h.poll(&mut node_watch), Poll::Pending);
        assert_matches!(h.poll(&mut transmission_watch), Poll::Pending);
    }

    // The medium observes the closes and drops the watchers.
    h.run_until_stalled();

    let node_events = h.node_event_watcher();
    let transmissions = h.transmission_watcher();
    assert_eq!(h.existing_ids(&node_events), BTreeSet::new());
    let mut node_watch = pin!(node_events.watch());
    let mut transmission_watch = pin!(transmissions.watch());
    assert_matches!(h.poll(&mut node_watch), Poll::Pending);
    assert_matches!(h.poll(&mut transmission_watch), Poll::Pending);
    assert_eq!(h.attach(NODE_A), Ok(()));
    let frame = frame(&[0x01]);
    h.transmit(NODE_A, &frame);

    let now = now();
    let events = assert_matches!(h.poll(&mut node_watch), Poll::Ready(Ok(events)) => events);
    assert_eq!(events, vec![added(NODE_A, now)]);
    let batch = assert_matches!(h.poll(&mut transmission_watch), Poll::Ready(Ok(b)) => b);
    assert_eq!(batch, (vec![captured(NODE_A, &frame, now)], 0));
}

// Watchers live on their own channels: closing the Monitor connection that created them does not
// close them.
#[test]
fn watchers_outlive_their_monitor() {
    let mut h = Harness::new();
    let (monitor, stream) = create_proxy_and_stream::<rfsim::MonitorMarker>();
    h.medium.serve_monitor(stream);
    let (node_events, server_end) = create_proxy::<rfsim::NodeEventWatcherMarker>();
    monitor.get_node_event_watcher(server_end).expect("GetNodeEventWatcher");
    let (transmissions, server_end) = create_proxy::<rfsim::TransmissionWatcherMarker>();
    // The reply means both watchers are installed.
    h.ready(monitor.get_transmission_watcher(server_end)).expect("GetTransmissionWatcher");
    assert_eq!(h.existing_ids(&node_events), BTreeSet::new());

    drop(monitor);
    h.run_until_stalled();
    assert_eq!(h.watcher_counts(), (1, 1));

    assert_eq!(h.attach(NODE_A), Ok(()));
    let frame = frame(&[0x01]);
    h.transmit(NODE_A, &frame);
    let now = now();
    assert_eq!(h.watch_nodes(&node_events), vec![added(NODE_A, now)]);
    assert_eq!(h.watch_transmissions(&transmissions), (vec![captured(NODE_A, &frame, now)], 0));
}

// However a watcher closes, the medium unregisters it.
#[test]
fn closed_watchers_are_unregistered() {
    let mut h = Harness::new();
    assert_eq!(h.watcher_counts(), (0, 0));

    // Closed by the client.
    {
        let _node_events = h.node_event_watcher();
        let _transmissions = h.transmission_watcher();
        assert_eq!(h.watcher_counts(), (1, 1));
    }
    h.run_until_stalled();
    assert_eq!(h.watcher_counts(), (0, 0));

    // Closed by the medium with ZX_ERR_BAD_STATE, on a second outstanding Watch.
    {
        let node_events = h.node_event_watcher();
        let transmissions = h.transmission_watcher();
        assert_eq!(h.existing_ids(&node_events), BTreeSet::new());
        assert_eq!(h.watcher_counts(), (1, 1));
        let mut first_node_watch = pin!(node_events.watch());
        let mut first_transmission_watch = pin!(transmissions.watch());
        assert_matches!(h.poll(&mut first_node_watch), Poll::Pending);
        assert_matches!(h.poll(&mut first_transmission_watch), Poll::Pending);
        let second = h.ready(node_events.watch());
        assert_eq!(closed_with(second), zx::Status::BAD_STATE);
        let second = h.ready(transmissions.watch());
        assert_eq!(closed_with(second), zx::Status::BAD_STATE);
        h.run_until_stalled();
        assert_eq!(h.watcher_counts(), (0, 0));
    }

    // Closed by the medium with ZX_ERR_NO_RESOURCES, on overflow.
    let node_events = h.node_event_watcher();
    assert_eq!(h.watcher_counts(), (1, 0));
    while !node_events.is_closed() {
        assert_eq!(h.attach(NODE_A), Ok(()));
        h.detach(NODE_A);
    }
    h.run_until_stalled();
    assert_eq!(h.watcher_counts(), (0, 0));
}

// Destroying the medium while hanging gets are outstanding fails the calls and closes every
// connection, rather than leaving the clients waiting.
#[test]
fn destroying_medium_with_pending_calls_closes_everything() {
    let mut h = Harness::new();
    assert_eq!(h.attach(NODE_A), Ok(()));
    let radio = h.radio(NODE_A);
    let mut receive = pin!(radio.receive());
    assert_matches!(h.poll(&mut receive), Poll::Pending, "no frames, must hang");

    let node_events = h.node_event_watcher();
    assert_eq!(h.existing_ids(&node_events), BTreeSet::from([NODE_A]));
    let mut node_watch = pin!(node_events.watch());
    assert_matches!(h.poll(&mut node_watch), Poll::Pending);

    let transmissions = h.transmission_watcher();
    let mut transmission_watch = pin!(transmissions.watch());
    assert_matches!(h.poll(&mut transmission_watch), Poll::Pending, "no transmissions, must hang");

    // Moves only the medium out: `h.exec` must outlive the clients and futures above, which are
    // dropped before `h`.
    drop(h.medium);
    let exec = &mut h.exec;
    let _: Poll<()> = exec.run_until_stalled(&mut future::pending::<()>());

    let result = assert_matches!(exec.run_until_stalled(&mut receive), Poll::Ready(r) => r);
    assert_eq!(closed_with(result), zx::Status::PEER_CLOSED);
    let result = assert_matches!(exec.run_until_stalled(&mut node_watch), Poll::Ready(r) => r);
    assert_eq!(closed_with(result), zx::Status::PEER_CLOSED);
    let result =
        assert_matches!(exec.run_until_stalled(&mut transmission_watch), Poll::Ready(r) => r);
    assert_eq!(closed_with(result), zx::Status::PEER_CLOSED);
    assert!(radio.is_closed());
    assert!(h.monitor.is_closed());
    assert!(h.medium_client.is_closed());
}
