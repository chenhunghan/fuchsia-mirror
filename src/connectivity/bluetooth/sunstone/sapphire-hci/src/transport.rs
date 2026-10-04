// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use sapphire_async::pool::{ObjectGuard, PoolCfg, RcPoolCfg, RwSlotGuard};
use sapphire_collections::storage::StorageFamily;
use sapphire_collections::vec::Vec;

/// Errors reported by an HCI transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {}

/// Storage and pool configuration of the buffers an [`HciRx`] receives packets into.
pub trait HciRxCfg<'buf> {
    type EventStorage: StorageFamily + 'buf;
    type EventPool: RcPoolCfg + 'buf;

    type AclStorage: StorageFamily + 'buf;
    type AclPool: PoolCfg + 'buf;

    type IsoStorage: StorageFamily + 'buf;
    type IsoPool: PoolCfg + 'buf;

    type ScoStorage: StorageFamily + 'buf;
    type ScoPool: PoolCfg + 'buf;
}

/// The receive half of an HCI transport.
// The returned futures are not required to be `Send`.
// TODO(https://fxbug.dev/535983448): Replace with a `Stream` of `IncomingHciPacket`s.
#[allow(async_fn_in_trait)]
pub trait HciRx<'buf> {
    /// Buffer configuration of the received packets.
    type Cfg: HciRxCfg<'buf>;

    /// Waits for the next packet from the controller.
    async fn next_packet(&mut self) -> Result<IncomingHciPacket<'buf, Self::Cfg>, TransportError>;
}

/// The send half of an HCI transport.
// The returned futures are not required to be `Send`.
// TODO(https://fxbug.dev/535983448): Replace with a `Sink` of outgoing packets.
#[allow(async_fn_in_trait)]
pub trait HciTx {
    /// Sends one HCI command packet (without an H4 packet indicator byte) to the controller.
    // TODO(https://fxbug.dev/535983448): Take a validated outgoing packet enum (covering commands and data) instead
    // of raw command bytes once data channels are added.
    async fn send_packet(&mut self, packet: impl AsRef<[u8]>) -> Result<(), TransportError>;
}

/// An HCI packet received from the controller, without an H4 packet indicator byte.
pub enum IncomingHciPacket<'buf, Cfg: HciRxCfg<'buf>> {
    Event(RwSlotGuard<'buf, Vec<u8, Cfg::EventStorage>, Cfg::EventPool>),
    Acl(ObjectGuard<'buf, Vec<u8, Cfg::AclStorage>, Cfg::AclPool>),
    Sco(ObjectGuard<'buf, Vec<u8, Cfg::ScoStorage>, Cfg::ScoPool>),
    Iso(ObjectGuard<'buf, Vec<u8, Cfg::IsoStorage>, Cfg::IsoPool>),
}
