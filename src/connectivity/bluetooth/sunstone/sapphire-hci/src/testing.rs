// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test doubles and packet builders shared by the unit tests of this crate.

use core::cell::RefCell;
use core::task::Poll;
use sapphire_async::broadcast::BroadcastCfg;
use sapphire_async::condition::Condition;
use sapphire_async::pool::{GuardedPool, PoolCfg, RcPool, RcPoolCfg};
use sapphire_async::rpc::RpcCfg;
use sapphire_collections::deque::Deque;
use sapphire_collections::storage::{ArrayStorage, Global};
use sapphire_collections::vec::StackVec;
use sapphire_emboss::hci_common::{CommandPacket, EventCode, EventHeaderWriter};
use sapphire_emboss::{CheckComplete, CheckOk};
use sapphire_sync::atomic::SingleThreadAtomics;
use sapphire_sync::mutex::raw::SingleThreadMutex;
use std::vec::Vec as StdVec;

use crate::command::{MAX_COMMAND_SIZE, OutgoingCommandPacket};
use crate::events::MAX_EVENT_SIZE;
use crate::hci::HciCfg;
use crate::transport::{HciRx, HciRxCfg, HciTx, IncomingHciPacket, TransportError};

/// Number of events the broadcast channel can buffer.
pub const EVENT_QUEUE_DEPTH: usize = 4;
/// Maximum number of simultaneous event subscribers.
pub const MAX_SUBSCRIBERS: usize = 4;
/// Number of event buffers in the [`MockController`]'s event pool. Larger than
/// [`EVENT_QUEUE_DEPTH`] so a full broadcast queue never starves the receive path.
const EVENT_POOL_SIZE: usize = 8;

/// Opcode of HCI_Reset.
pub const RESET_OPCODE: u16 = 0x0C03;

/// Number of command requests the RPC channel queues.
const RPC_QUEUE_DEPTH: usize = 4;

/// Heap-backed, single-threaded pool configuration.
pub struct TestPoolCfg;

impl PoolCfg for TestPoolCfg {
    type Storage = Global;
    type Mutex = SingleThreadMutex;
}

impl RcPoolCfg for TestPoolCfg {
    type Atomics = SingleThreadAtomics;
}

/// Single-threaded broadcast configuration with room for [`EVENT_QUEUE_DEPTH`] events and
/// [`MAX_SUBSCRIBERS`] subscribers.
pub struct TestBroadcastCfg;

impl BroadcastCfg for TestBroadcastCfg {
    type Buffer = ArrayStorage<EVENT_QUEUE_DEPTH>;
    type SubscriptionStore = ArrayStorage<MAX_SUBSCRIBERS>;
    type Mtx = SingleThreadMutex;
}

/// Single-threaded RPC configuration for the command channel.
pub struct TestRpcCfg;

impl RpcCfg for TestRpcCfg {
    type Mtx = SingleThreadMutex;
    type Chan = ArrayStorage<RPC_QUEUE_DEPTH>;
}

/// Receive buffer configuration of [`MockRx`]. Only events are ever received.
pub struct TestRxCfg;

impl<'buf> HciRxCfg<'buf> for TestRxCfg {
    type EventStorage = ArrayStorage<MAX_EVENT_SIZE>;
    type EventPool = TestPoolCfg;
    type AclStorage = ArrayStorage<1>;
    type AclPool = TestPoolCfg;
    type IsoStorage = ArrayStorage<1>;
    type IsoPool = TestPoolCfg;
    type ScoStorage = ArrayStorage<1>;
    type ScoPool = TestPoolCfg;
}

/// [`HciCfg`] that connects an HCI layer to a [`MockController`].
pub struct TestHciCfg;

impl<'buf> HciCfg<'buf> for TestHciCfg {
    type EventBroadcastCfg = TestBroadcastCfg;
    type HciTx = MockTx<'buf>;
    type HciRx = MockRx<'buf>;
    type CommandPoolCfg = TestPoolCfg;
    type CommandRpcCfg = TestRpcCfg;
}

/// Pool that command buffers are claimed from.
pub type CommandPool = GuardedPool<StackVec<u8, MAX_COMMAND_SIZE>, TestPoolCfg>;

type EventPool = RcPool<StackVec<u8, MAX_EVENT_SIZE>, TestPoolCfg>;

/// A simulated controller: records the packets the host sends and delivers the events the
/// test queues.
pub struct MockController {
    event_pool: EventPool,
    pending_events: Condition<SingleThreadMutex, Deque<StackVec<u8, MAX_EVENT_SIZE>, Global>>,
    sent_packets: RefCell<StdVec<StdVec<u8>>>,
}

impl MockController {
    /// Creates a controller with no queued events and nothing sent.
    pub fn new() -> Self {
        Self {
            event_pool: EventPool::new(EVENT_POOL_SIZE).expect("event pool allocates"),
            pending_events: Condition::new(Deque::default()),
            sent_packets: RefCell::default(),
        }
    }

    /// Returns the host's transport halves connected to this controller.
    pub fn transport(&self) -> (MockTx<'_>, MockRx<'_>) {
        (MockTx { controller: self }, MockRx { controller: self })
    }

    /// Queues an event whose header declares `declared_len` parameter bytes but which carries
    /// only `payload`.
    pub fn push_truncated_event(&self, event_code: EventCode, declared_len: u8, payload: &[u8]) {
        let mut bytes = make_event_packet(event_code, payload);
        bytes[1] = declared_len;
        self.push_event_bytes(bytes);
    }

    fn push_event_bytes(&self, bytes: StackVec<u8, MAX_EVENT_SIZE>) {
        assert!(self.pending_events.lock().push_back(bytes).is_ok(), "event queue has room");
        self.pending_events.notify_one();
    }
}

/// The host's send half of a [`MockController`] transport.
#[derive(Clone)]
pub struct MockTx<'a> {
    controller: &'a MockController,
}

impl HciTx for MockTx<'_> {
    async fn send_packet(&mut self, packet: impl AsRef<[u8]>) -> Result<(), TransportError> {
        self.controller.sent_packets.borrow_mut().push(StdVec::from(packet.as_ref()));
        Ok(())
    }
}

/// The host's receive half of a [`MockController`] transport.
pub struct MockRx<'a> {
    controller: &'a MockController,
}

impl<'a> HciRx<'a> for MockRx<'a> {
    type Cfg = TestRxCfg;

    async fn next_packet(&mut self) -> Result<IncomingHciPacket<'a, TestRxCfg>, TransportError> {
        // Claim the buffer first so that cancelling this future never loses a queued event.
        let mut slot = self.controller.event_pool.claim().await;
        *slot = self
            .controller
            .pending_events
            .when(|events| events.pop_front().map_or(Poll::Pending, Poll::Ready))
            .await;
        Ok(IncomingHciPacket::Event(slot))
    }
}

/// Builds an event packet with a valid header for `event_code` and `payload`.
pub fn make_event_packet(event_code: EventCode, payload: &[u8]) -> StackVec<u8, MAX_EVENT_SIZE> {
    let mut header = [0u8; 2];
    let _ = EventHeaderWriter::new(&mut header[..])
        .check_complete()
        .expect("event header buffer is complete")
        .write_event_code(event_code)
        .write_parameter_total_size(u8::try_from(payload.len()).expect("payload fits in an event"));
    let mut packet = StackVec::new();
    packet.try_extend(&header).expect("header fits in MAX_EVENT_SIZE");
    packet.try_extend(payload).expect("payload fits in MAX_EVENT_SIZE");
    packet
}

/// Builds a command packet for `opcode` with `params`, in a buffer claimed from `pool`.
pub fn make_command<'p>(
    pool: &'p CommandPool,
    opcode: u16,
    params: &[u8],
) -> OutgoingCommandPacket<'p, TestPoolCfg> {
    let mut buffer = pool.try_claim().expect("command pool has a free buffer");
    buffer.clear();
    buffer.try_extend(&command_bytes(opcode, params)).expect("command fits in MAX_COMMAND_SIZE");
    CommandPacket::new(buffer).check_ok().unwrap_or_else(|_| panic!("command is valid"))
}

/// Returns the bytes of the command packet for `opcode` with `params`, as sent on the wire.
pub fn command_bytes(opcode: u16, params: &[u8]) -> StdVec<u8> {
    let [opcode_lo, opcode_hi] = opcode.to_le_bytes();
    let params_len = u8::try_from(params.len()).expect("params fit in a command");
    let mut bytes = StdVec::from([opcode_lo, opcode_hi, params_len]);
    bytes.extend_from_slice(params);
    bytes
}
