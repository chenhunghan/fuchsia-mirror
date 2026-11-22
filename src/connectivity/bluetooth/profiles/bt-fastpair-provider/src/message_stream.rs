// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::pin::Pin;
use core::task::{Context, Poll};
use fidl_fuchsia_bluetooth::ChannelParameters;
use fidl_fuchsia_bluetooth_bredr::{
    ProfileProxy, ProtocolDescriptor, ProtocolIdentifier, ServiceDefinition,
};
use fuchsia_bluetooth::types::{Channel, PeerId, Uuid};
use futures::SinkExt;
use futures::stream::{FusedStream, Stream, StreamExt};
use log::{info, trace, warn};
use packet_encoding::Encodable;
use profile_client::{ProfileClient, ProfileEvent};
use std::collections::HashMap;

use crate::types::Error;
use crate::types::packets::MessageStreamPacket;

/// The service UUID associated with the Fast Pair service.
/// "df21fe2c-2515-4fdb-8886-f12c4d67927c"
/// See https://developers.google.com/nearby/fast-pair/specifications/extensions/messagestream#MessageStream
const FASTPAIR_SERVICE_UUID: Uuid = Uuid::from_bytes([
    0x7c, 0x92, 0x67, 0x4d, 0x2c, 0xf1, 0x86, 0x88, 0xdb, 0x4f, 0x15, 0x25, 0x2c, 0xfe, 0x21, 0xdf,
]);

/// The `MessageStream` manages the Fast Pair advertisement of RFCOMM and the RFCOMM connections
/// to remote peers. It processes events over the `fuchsia.bluetooth.bredr.Profile` API and
/// handles incoming RFCOMM connections.
///
/// `MessageStream` implements Stream where each item is an incoming RFCOMM connection from a
/// remote peer. The connection is not used until it is explicitly accepted with
/// `MessageStream::accept`; dropping it rejects the connection. At most one connection is active
/// per peer, and connections from different peers are independent. The object _must_ be polled
/// in order to receive these events and process data received over accepted channels.
///
/// The `Provider` only accepts connections from bonded peers.
pub struct MessageStream {
    profile: ProfileClient,
    /// Accepted and active RFCOMM connections, keyed by remote peer ID.
    channels: HashMap<PeerId, Channel>,
}

impl MessageStream {
    pub fn new(profile: ProfileProxy) -> Self {
        // Failure to advertise is not fatal. Fast Pair still works without RFCOMM.
        let profile = match Self::advertise(&profile) {
            Ok(p) => p,
            Err(e) => {
                warn!(e:?; "Couldn't advertise the RFCOMM service");
                ProfileClient::new(profile)
            }
        };
        Self { profile, channels: HashMap::new() }
    }

    fn advertise(profile: &ProfileProxy) -> Result<ProfileClient, Error> {
        let service = ServiceDefinition {
            service_class_uuids: Some(vec![FASTPAIR_SERVICE_UUID.into()]),
            protocol_descriptor_list: Some(vec![
                ProtocolDescriptor {
                    protocol: Some(ProtocolIdentifier::L2Cap),
                    params: Some(vec![]),
                    ..Default::default()
                },
                ProtocolDescriptor {
                    protocol: Some(ProtocolIdentifier::Rfcomm),
                    params: Some(vec![]),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        // No security requirements: requiring an authenticated (MITM-protected) link would reject
        // Seekers that bonded via Just Works, which GFPS allows to retroactively write an Account
        // Key.
        ProfileClient::advertise(profile.clone(), vec![service], ChannelParameters::default())
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub fn rfcomm_connected(&self, id: &PeerId) -> bool {
        self.channels.contains_key(id)
    }

    /// Accepts the RFCOMM `channel` from the remote peer `id`. Replaces any existing connection
    /// to the same peer.
    pub fn accept(&mut self, id: PeerId, channel: Channel) {
        if self.channels.insert(id, channel).is_some() {
            trace!(id:%; "RFCOMM connection already exists. Overwriting");
        }
    }

    /// Closes the RFCOMM connection to the remote peer `id`, if it exists.
    pub fn disconnect(&mut self, id: &PeerId) {
        if self.channels.remove(id).is_some() {
            info!(id:%; "Closed RFCOMM connection");
        }
    }

    /// Closes all RFCOMM connections.
    pub fn disconnect_all(&mut self) {
        self.channels.clear();
    }

    /// Sends the `packet` to the remote peer `id`.
    /// Returns Error if there is no accepted connection to the peer or the send fails. The
    /// connection is closed if the send fails.
    pub async fn send(&mut self, id: &PeerId, packet: MessageStreamPacket) -> Result<(), Error> {
        let channel = self
            .channels
            .get_mut(id)
            .ok_or_else(|| Error::internal("No RFCOMM connection available"))?;
        let mut buf = vec![0; packet.encoded_len()];
        packet.encode(&mut buf[..])?;
        if let Err(e) = channel.send(buf).await {
            self.disconnect(id);
            return Err(Error::internal(&format!("{e:?}")));
        }
        Ok(())
    }

    /// Processes an event from the `bredr.Profile` protocol.
    /// Returns the PeerId and channel if the `event` is an incoming RFCOMM connection from a
    /// remote peer.
    fn handle_profile_event(event: ProfileEvent) -> Option<(PeerId, Channel)> {
        let ProfileEvent::PeerConnected { id, protocol, channel } = event else {
            return None;
        };

        if !protocol
            .iter()
            .any(|descriptor| descriptor.protocol == Some(ProtocolIdentifier::Rfcomm))
        {
            warn!(id:%; "Received non-RFCOMM connection. Ignoring");
            return None;
        }
        Some((id, channel))
    }

    fn handle_rfcomm_data(id: PeerId, data: Vec<u8>) {
        trace!(id:%, len = data.len(); "Received Message Stream request");
        // We don't support any Message Stream requests.
        // TODO(https://fxbug.dev/42062580): Parse and handle Message Stream requests.
    }
}

impl Stream for MessageStream {
    type Item = (PeerId, Channel);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_terminated() {
            panic!("Cannot poll a terminated stream");
        }

        // Check the active RFCOMM advertisement. Events that are ignored or errors don't register
        // a waker, so this loops until the advertisement returns Poll::Pending.
        loop {
            match self.profile.poll_next_unpin(cx) {
                Poll::Pending => break,
                Poll::Ready(Some(Ok(event))) => {
                    if let Some(connection) = Self::handle_profile_event(event) {
                        return Poll::Ready(Some(connection));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(e:?; "Error in ProfileClient stream");
                }
                Poll::Ready(None) => {
                    // The stream will not be polled again, so the accepted connections can no
                    // longer be serviced.
                    info!("ProfileClient stream terminated");
                    self.disconnect_all();
                    return Poll::Ready(None);
                }
            }
        }

        // Check the accepted RFCOMM connections - each is polled until it returns Poll::Pending.
        // Closed connections are removed.
        self.channels.retain(|id, channel| {
            loop {
                match channel.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(data))) => Self::handle_rfcomm_data(*id, data),
                    Poll::Ready(Some(Err(e))) => {
                        warn!(id:%, e:?; "Error in RFCOMM channel");
                        break false;
                    }
                    Poll::Ready(None) => {
                        info!(id:%; "RFCOMM connection closed");
                        break false;
                    }
                    Poll::Pending => break true,
                }
            }
        });
        Poll::Pending
    }
}

impl FusedStream for MessageStream {
    fn is_terminated(&self) -> bool {
        self.profile.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::ModelId;
    use assert_matches::assert_matches;
    use bt_channel_test_support::Transport;
    use fidl_fuchsia_bluetooth_bredr::DataElement;
    use fuchsia_bluetooth::types::Address;
    use futures::FutureExt;
    use test_case::test_case;
    use test_profile_server::TestProfileServer;

    async fn expect_data(remote: &mut Channel, expected_data: Vec<u8>) {
        let read = match remote.next().await {
            Some(Ok(data)) => data,
            x => panic!("Expected data from the peer, got {x:?}"),
        };
        assert_eq!(read.len(), expected_data.len());
        assert_eq!(read, expected_data);
    }

    #[test_case(Transport::Socket ; "socket")]
    #[test_case(Transport::Fidl ; "fidl")]
    #[fuchsia::test]
    async fn message_stream_receives_data(transport: Transport) {
        let (profile, mut profile_server) = TestProfileServer::create_endpoints();
        let mut message_stream = MessageStream::new(profile);

        // Expect the RFCOMM advertisement.
        profile_server.expect_advertise().await;

        // Remote peer connection.
        let id = PeerId(123);
        let mut remote =
            profile_server.send_connected_with_transport(id, rfcomm_protocol(), transport);

        // Expect the MessageStream to produce the connected event. The connection is not used
        // until it is accepted.
        let (connected_id, channel) = message_stream.select_next_some().await;
        assert_eq!(connected_id, id);
        assert!(!message_stream.rfcomm_connected(&id));
        message_stream.accept(id, channel);
        assert!(message_stream.rfcomm_connected(&id));

        // Remote can write data - it's just logged.
        remote.send(vec![0, 1, 2, 3]).await.expect("can write data");
        assert_matches!(message_stream.select_next_some().now_or_never(), None);

        // Local can send Model ID and local address.
        let model_id_packet =
            MessageStreamPacket::new_model_id(ModelId::try_from(0x123456).expect("valid"));
        message_stream.send(&id, model_id_packet).await.expect("can send model id");
        expect_data(&mut remote, vec![0x03, 0x01, 0x00, 0x03, 0x12, 0x34, 0x56]).await;
        let address_packet = MessageStreamPacket::new_address(Address::Public([1, 2, 3, 4, 5, 6]));
        message_stream.send(&id, address_packet).await.expect("can send address");
        expect_data(&mut remote, vec![0x03, 0x02, 0x00, 0x06, 0x6, 0x5, 0x4, 0x3, 0x2, 0x1]).await;

        // Remote disconnects RFCOMM - MessageStream itself should still be active. The closed
        // connection is removed once the stream observes it.
        drop(remote);
        futures::future::poll_fn(|cx| {
            assert_matches!(message_stream.poll_next_unpin(cx), Poll::Pending);
            if message_stream.rfcomm_connected(&id) { Poll::Pending } else { Poll::Ready(()) }
        })
        .await;
        assert!(!message_stream.is_terminated());

        // If the upstream Profile Server closes the advertisement, then the message stream will
        // terminate.
        drop(profile_server);
        assert_matches!(message_stream.next().await, None);
        assert!(message_stream.is_terminated());
    }

    fn rfcomm_protocol() -> Vec<ProtocolDescriptor> {
        vec![
            ProtocolDescriptor {
                protocol: Some(ProtocolIdentifier::L2Cap),
                params: Some(vec![]),
                ..Default::default()
            },
            ProtocolDescriptor {
                protocol: Some(ProtocolIdentifier::Rfcomm),
                params: Some(vec![DataElement::Uint8(1)]),
                ..Default::default()
            },
        ]
    }

    /// Returns a new MessageStream and the TestProfileServer managing its RFCOMM advertisement,
    /// which must be kept alive for the advertisement to stay active.
    async fn setup_message_stream() -> (MessageStream, TestProfileServer) {
        let (profile, mut profile_server) = TestProfileServer::create_endpoints();
        let message_stream = MessageStream::new(profile);
        profile_server.expect_advertise().await;
        (message_stream, profile_server)
    }

    /// Connects an RFCOMM channel from the peer `id` and accepts it. Returns the remote end.
    async fn connect_and_accept(
        message_stream: &mut MessageStream,
        profile_server: &mut TestProfileServer,
        id: PeerId,
    ) -> Channel {
        let remote = profile_server.send_connected(id, rfcomm_protocol());
        let (connected_id, channel) = message_stream.select_next_some().await;
        assert_eq!(connected_id, id);
        message_stream.accept(id, channel);
        remote
    }

    #[fuchsia::test]
    async fn connections_from_multiple_peers_are_isolated() {
        let (mut message_stream, mut profile_server) = setup_message_stream().await;

        // First peer connects and is accepted.
        let id1 = PeerId(1);
        let mut remote1 = connect_and_accept(&mut message_stream, &mut profile_server, id1).await;

        // Second peer connects but is not accepted - the first connection is unaffected.
        let id2 = PeerId(2);
        let mut remote2 = profile_server.send_connected(id2, rfcomm_protocol());
        let (connected_id, channel) = message_stream.select_next_some().await;
        assert_eq!(connected_id, id2);
        drop(channel);
        assert_matches!(remote2.next().await, None | Some(Err(_)));
        assert!(message_stream.rfcomm_connected(&id1));
        assert!(!message_stream.rfcomm_connected(&id2));
        assert_matches!(
            message_stream
                .send(&id2, MessageStreamPacket::new_address(Address::Public([1, 2, 3, 4, 5, 6])))
                .await,
            Err(_)
        );

        // Data is only sent to the targeted peer.
        let address_packet = MessageStreamPacket::new_address(Address::Public([1, 2, 3, 4, 5, 6]));
        message_stream.send(&id1, address_packet).await.expect("can send address");
        expect_data(&mut remote1, vec![0x03, 0x02, 0x00, 0x06, 0x6, 0x5, 0x4, 0x3, 0x2, 0x1]).await;

        // Disconnecting the peer closes its connection.
        message_stream.disconnect(&id1);
        assert!(!message_stream.rfcomm_connected(&id1));
        assert_matches!(remote1.next().await, None | Some(Err(_)));
    }

    #[fuchsia::test]
    async fn reconnection_replaces_existing_connection() {
        let (mut message_stream, mut profile_server) = setup_message_stream().await;
        let id = PeerId(1);
        let mut remote1 = connect_and_accept(&mut message_stream, &mut profile_server, id).await;

        // The same peer reconnects - the new connection replaces the old one.
        let mut remote2 = connect_and_accept(&mut message_stream, &mut profile_server, id).await;
        assert_matches!(remote1.next().await, None | Some(Err(_)));
        assert!(message_stream.rfcomm_connected(&id));

        // Data is sent over the new connection.
        let address_packet = MessageStreamPacket::new_address(Address::Public([1, 2, 3, 4, 5, 6]));
        message_stream.send(&id, address_packet).await.expect("can send address");
        expect_data(&mut remote2, vec![0x03, 0x02, 0x00, 0x06, 0x6, 0x5, 0x4, 0x3, 0x2, 0x1]).await;
    }

    #[fuchsia::test]
    async fn accepted_connections_closed_when_advertisement_terminates() {
        let (mut message_stream, mut profile_server) = setup_message_stream().await;
        let id = PeerId(1);
        let mut remote = connect_and_accept(&mut message_stream, &mut profile_server, id).await;

        // The upstream Profile server closes the advertisement - the stream terminates and the
        // accepted connection is closed, since it will no longer be polled.
        drop(profile_server);
        assert_matches!(message_stream.next().await, None);
        assert!(message_stream.is_terminated());
        assert!(!message_stream.rfcomm_connected(&id));
        assert_matches!(remote.next().await, None | Some(Err(_)));
    }
}
