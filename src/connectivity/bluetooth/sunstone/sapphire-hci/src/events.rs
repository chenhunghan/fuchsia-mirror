// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use sapphire_async::broadcast::{
    BroadcastCfg, FilteredBroadcastChannel, FilteredSubscriber, Interest,
};
use sapphire_async::pool::{RoSlotGuard, RwSlotGuard};
use sapphire_collections::vec::Vec;
use sapphire_emboss::hci_common::EventPacket;
use sapphire_emboss::{CheckOk, OkState};

use crate::hci::HciError;
use crate::transport::HciRxCfg;

/// Maximum size of an HCI event packet.
pub use sapphire_emboss::hci_common::event_packet::MAX_SIZE_IN_BYTES as MAX_EVENT_SIZE;

/// A validated event packet backed by a read-only, reference-counted slot from the transport's
/// event pool, so it can be shared with every interested subscriber without copying.
pub(super) type PublishedEventPacket<'buf, RxCfg> = EventPacket<
    RoSlotGuard<
        'buf,
        Vec<u8, <RxCfg as HciRxCfg<'buf>>::EventStorage>,
        <RxCfg as HciRxCfg<'buf>>::EventPool,
    >,
    OkState,
>;

/// Validates received HCI events and broadcasts them to the subscribers whose filter accepts them.
pub(crate) struct EventRouter<'buf, RxCfg: HciRxCfg<'buf>, BCfg: BroadcastCfg> {
    channel: FilteredBroadcastChannel<PublishedEventPacket<'buf, RxCfg>, BCfg>,
}

impl<'buf, RxCfg: HciRxCfg<'buf>, BCfg: BroadcastCfg> Default for EventRouter<'buf, RxCfg, BCfg>
where
    FilteredBroadcastChannel<PublishedEventPacket<'buf, RxCfg>, BCfg>: Default,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<'buf, RxCfg: HciRxCfg<'buf>, BCfg: BroadcastCfg> EventRouter<'buf, RxCfg, BCfg> {
    /// Creates an event router with no subscribers and no queued events.
    pub(crate) fn new() -> Self
    where
        FilteredBroadcastChannel<PublishedEventPacket<'buf, RxCfg>, BCfg>: Default,
    {
        Self { channel: FilteredBroadcastChannel::new() }
    }

    /// Subscribes to the events accepted by `filter`.
    ///
    /// Returns `None` if the router has no room for another subscriber.
    pub(crate) fn subscribe(
        &self,
        filter: fn(&PublishedEventPacket<'buf, RxCfg>) -> Interest,
    ) -> Option<EventSubscriber<'_, 'buf, RxCfg, BCfg>> {
        Some(EventSubscriber { subscriber: self.channel.subscribe(filter)? })
    }

    /// Validates the event in `buf` and broadcasts it to every interested subscriber, waiting if
    /// the event queue is full.
    pub(crate) async fn route_event(
        &self,
        buf: RwSlotGuard<'buf, Vec<u8, RxCfg::EventStorage>, RxCfg::EventPool>,
    ) -> Result<(), HciError> {
        let event =
            EventPacket::new(buf.share()).check_ok().map_err(|_| HciError::ControllerError)?;
        self.channel.publish(event).await;
        Ok(())
    }
}

/// Receives the events accepted by its filter.
pub struct EventSubscriber<'r, 'buf, RxCfg: HciRxCfg<'buf>, BCfg: BroadcastCfg> {
    #[expect(dead_code, reason = "read once the subscriber is implemented")]
    subscriber: FilteredSubscriber<'r, PublishedEventPacket<'buf, RxCfg>, BCfg>,
}

impl<'r, 'buf, RxCfg: HciRxCfg<'buf>, BCfg: BroadcastCfg> EventSubscriber<'r, 'buf, RxCfg, BCfg> {
    /// Waits for the next event accepted by the filter.
    pub async fn next(&mut self) -> PublishedEventPacket<'buf, RxCfg> {
        // TODO(https://fxbug.dev/535983448): Receive the next event.
        todo!()
    }
}
