// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_channel_test_support::{Transport, create_test_channels};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_bluetooth as fidl_bt;
use fidl_fuchsia_bluetooth_bredr as bredr;
use fuchsia_bluetooth::types::{self as bt, PeerId};
use futures::{Stream, StreamExt};
use profile_client::ProfileClient;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Endpoints returned by [`TestProfileServer::new`] when creating a [`ProfileClient`] alongside a
/// [`TestProfileServer`].
pub struct TestProfileServerEndpoints {
    pub proxy: bredr::ProfileProxy,
    pub client: ProfileClient,
    pub test_server: TestProfileServer,
}

/// Specifies the L2CAP PSM or RFCOMM channel number to assert on an incoming `Profile.Connect`
/// request.
#[derive(Debug)]
pub enum ConnectChannel {
    L2CapPsm(u16),
    RfcommChannel(u8), // Valid channels are 1-30
}

/// An incoming `fuchsia.bluetooth.bredr.Profile/Connect` request received by
/// [`TestProfileServer::expect_connect_request`].
///
/// Provides chainable assertions for verifying the target peer and channel parameters before
/// responding with a connected test channel or error.
#[derive(Debug)]
#[must_use]
pub struct ConnectRequest {
    pub peer_id: PeerId,
    pub connection: bredr::ConnectParameters,
    pub responder: bredr::ProfileConnectResponder,
}

impl ConnectRequest {
    /// Asserts that the request targets `expected_peer`.
    #[track_caller]
    pub fn assert_peer(self, expected_peer: PeerId) -> Self {
        assert_eq!(expected_peer, self.peer_id);
        self
    }

    /// Asserts that the request targets `expected_channel`.
    #[track_caller]
    pub fn assert_channel(self, expected_channel: ConnectChannel) -> Self {
        match (expected_channel, &self.connection) {
            (
                ConnectChannel::L2CapPsm(expected_psm),
                bredr::ConnectParameters::L2cap(bredr::L2capParameters { psm: psm_option, .. }),
            ) => assert_eq!(Some(expected_psm), *psm_option),
            (
                ConnectChannel::RfcommChannel(expected_channel),
                bredr::ConnectParameters::Rfcomm(bredr::RfcommParameters {
                    channel: channel_option,
                    ..
                }),
            ) => assert_eq!(Some(expected_channel), *channel_option),
            (expected_channel, connection) => {
                panic!("On connect, expected {expected_channel:?}, got {connection:?}")
            }
        }
        self
    }

    /// Accepts the request, returning the remote end of a new `Transport::Socket` test channel.
    #[track_caller]
    pub fn accept(self) -> bt::Channel {
        self.accept_with_transport(Transport::Socket)
    }

    /// Accepts the request, returning the remote end of a new test channel backed by `transport`.
    #[track_caller]
    pub fn accept_with_transport(self, transport: Transport) -> bt::Channel {
        let (far_bt_channel, near_bt_channel) = create_test_channels(transport);
        let far_bredr_channel: bredr::Channel =
            far_bt_channel.try_into().expect("BT Channel into FIDL BREDR Channel");
        self.responder.send(Ok(far_bredr_channel)).expect("Send channel");
        near_bt_channel
    }

    /// Rejects the request with the given FIDL `error`.
    #[track_caller]
    pub fn reject(self, error: fidl_bt::ErrorCode) {
        self.responder.send(Err(error)).expect("Send error");
    }
}

/// A mock server for `fuchsia.bluetooth.bredr.Profile` used in unit tests of Bluetooth profiles and
/// libraries.
///
/// `TestProfileServer` wraps the server-side [`bredr::ProfileRequestStream`] along with the
/// client-side proxies (`SearchResultsProxy`, `ConnectionReceiver2Proxy`) and responder
/// (`ProfileAdvertiseResponder`) registered by the component under test. Use it in unit tests for
/// components that interact with `bredr.Profile` (directly via [`bredr::ProfileProxy`] or through
/// [`ProfileClient`]) to drive service discovery, incoming connections, and outbound connection
/// requests without manual FIDL request stream matching.
///
/// For components that require custom multi-peer or stateful `bredr.Profile` routing beyond single
/// request/response assertions, use a dedicated mock server or inspect the underlying stream
/// directly via `TestProfileServer`'s [`Stream`] implementation.
///
/// Keep the `TestProfileServer` instance alive for as long as the profile connection or
/// advertisement should remain active; dropping it completes any pending `Advertise` responder and
/// closes the request stream.
///
/// # Examples
///
/// ```ignore
/// use test_profile_server::{ConnectChannel, TestProfileServer};
///
/// // Create endpoints (or use `TestProfileServer::new(...)` when passing a `ProfileClient`).
/// let (proxy, mut server) = TestProfileServer::create_endpoints();
/// let _profile_task = spawn_component_under_test(proxy);
///
/// // Wait for registration (`expect_advertise`, `expect_search`, or `expect_search_and_advertise`).
/// server.expect_search_and_advertise().await;
///
/// // Simulate a discovered peer and handle the resulting outbound connection.
/// server.send_service_found(PeerId(1), Some(protocol), vec![]).await.unwrap();
/// let mut remote_chan = server
///     .expect_connect_request()
///     .await
///     .assert_peer(PeerId(1))
///     .assert_channel(ConnectChannel::RfcommChannel(1))
///     .accept();
///
/// // Or simulate an incoming connection from a remote peer.
/// let mut incoming_chan = server.send_connected(PeerId(2), protocol);
///
/// // Drop `server` to terminate the advertisement and close `Profile`.
/// drop(server);
/// ```
// TODO(b/333456020): Clean up `advertise_responder`
pub struct TestProfileServer {
    profile_request_stream: bredr::ProfileRequestStream,
    search_results_proxy: Option<bredr::SearchResultsProxy>,
    connection_receiver_proxy: Option<bredr::ConnectionReceiver2Proxy>,
    advertise_responder: Option<bredr::ProfileAdvertiseResponder>,
}

impl From<bredr::ProfileRequestStream> for TestProfileServer {
    fn from(profile_request_stream: bredr::ProfileRequestStream) -> Self {
        Self {
            profile_request_stream,
            search_results_proxy: None,
            connection_receiver_proxy: None,
            advertise_responder: None,
        }
    }
}

impl TestProfileServer {
    /// Create a new Profile proxy and test server wrapping the stream.
    pub fn create_endpoints() -> (bredr::ProfileProxy, TestProfileServer) {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<bredr::ProfileMarker>();
        (proxy, TestProfileServer::from(stream))
    }

    /// Create a new Profile proxy and stream, and create a profile client that wraps the proxy and a
    /// test server that wraps the stream.
    ///
    /// If service_class_profile_id is Some, add a search for that service class.
    ///
    /// If service_definition is Some, advertise with that service definition.
    ///
    /// Returns a struct containing the proxy, profile client and test server.
    pub fn new(
        service_definition: Option<bredr::ServiceDefinition>,
        service_class_profile_id: Option<bredr::ServiceClassProfileIdentifier>,
    ) -> TestProfileServerEndpoints {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<bredr::ProfileMarker>();

        let mut client = match service_definition {
            None => ProfileClient::new(proxy.clone()),
            Some(service_definition) => {
                let channel_params = fidl_bt::ChannelParameters::default();
                ProfileClient::advertise(proxy.clone(), vec![service_definition], channel_params)
                    .expect("Failed to advertise.")
            }
        };

        if let Some(service_class_profile_id) = service_class_profile_id {
            client.add_search(service_class_profile_id, None).expect("Failed to search for peers.");
        }

        let test_server = TestProfileServer::from(stream);

        TestProfileServerEndpoints { proxy, client, test_server }
    }

    pub fn results(&self) -> Option<&bredr::SearchResultsProxy> {
        self.search_results_proxy.as_ref()
    }

    pub fn receiver(&self) -> Option<&bredr::ConnectionReceiver2Proxy> {
        self.connection_receiver_proxy.as_ref()
    }

    /// Waits for both an `Advertise` and a `Search` request in arbitrary order.
    ///
    /// Prefer [`Self::expect_advertise`] or [`Self::expect_search`] directly for profiles that only
    /// register one of the two.
    pub async fn expect_search_and_advertise(&mut self) {
        let mut receiver = None;
        let mut results = None;
        while let Some(request) = self.profile_request_stream.next().await {
            match request {
                Ok(bredr::ProfileRequest::Advertise { payload, responder, .. }) => {
                    if receiver.is_some() {
                        panic!("unexpected second advertise request");
                    }
                    receiver = Some(payload.connection_receiver.unwrap().into_proxy());
                    let _ = responder.send(Ok(&bredr::ProfileAdvertiseResponse {
                        services: payload.services.clone(),
                        ..Default::default()
                    }));
                    if receiver.is_some() && results.is_some() {
                        break;
                    }
                }
                Ok(bredr::ProfileRequest::Search { payload, .. }) => {
                    if results.is_some() {
                        panic!("unexpected second search request");
                    }
                    results = Some(payload.results.unwrap().into_proxy());
                    if receiver.is_some() && results.is_some() {
                        break;
                    }
                }
                _ => panic!("unexpected result on profile request stream: {:?}", request),
            }
        }
        self.connection_receiver_proxy = receiver;
        self.search_results_proxy = results;
    }

    pub async fn expect_search(&mut self) {
        let request = self.profile_request_stream.next().await;
        match request {
            Some(Ok(bredr::ProfileRequest::Search { payload, .. })) => {
                self.search_results_proxy = Some(payload.results.unwrap().into_proxy());
            }
            _ => panic!(
                "unexpected result on profile request stream while waiting for search: {request:?}"
            ),
        }
    }

    pub async fn expect_advertise(&mut self) {
        let request = self.profile_request_stream.next().await;
        match request {
            Some(Ok(bredr::ProfileRequest::Advertise { payload, responder, .. })) => {
                self.connection_receiver_proxy =
                    Some(payload.connection_receiver.unwrap().into_proxy());
                if let Some(_old_responder) = self.advertise_responder.replace(responder) {
                    panic!("Got new advertise request before old request is complete.");
                }
            }
            _ => panic!(
                "unexpected result on profile request stream while waiting for advertisement: {request:?}"
            ),
        }
    }

    /// Waits for an incoming `Profile.Connect` request and returns a [`ConnectRequest`] for
    /// asserting parameters and responding.
    pub async fn expect_connect_request(&mut self) -> ConnectRequest {
        let request = self.profile_request_stream.next().await;
        match request {
            Some(Ok(bredr::ProfileRequest::Connect { peer_id, connection, responder, .. })) => {
                ConnectRequest { peer_id: peer_id.into(), connection, responder }
            }
            _ => panic!(
                "Unexpected result on profile request stream expecting connection: {request:?}",
            ),
        }
    }

    pub async fn expect_sco_connect(
        &mut self,
        expected_initiator: bool,
    ) -> ServerEnd<bredr::ScoConnectionMarker> {
        let request = self.profile_request_stream.next().await;
        let connection = match request {
            Some(Ok(bredr::ProfileRequest::ConnectSco {
                payload: bredr::ProfileConnectScoRequest { initiator, connection, .. },
                ..
            })) if initiator == Some(expected_initiator) => connection,
            Some(Ok(bredr::ProfileRequest::ConnectSco {
                payload: bredr::ProfileConnectScoRequest { initiator, .. },
                ..
            })) => {
                panic!(
                    "Got SCO connection request expected initatior: {expected_initiator:}, actual initiator: {initiator:?}"
                );
            }
            _ => panic!(
                "Unexpected result on profile request stream expecting SCO connection: {request:?}",
            ),
        };

        connection.expect("Got no connection when expecting SCO connection.")
    }

    pub fn send_service_found(
        &mut self,
        peer_id: PeerId,
        protocol_list: Option<Vec<bredr::ProtocolDescriptor>>,
        attributes: Vec<bredr::Attribute>,
    ) -> fidl::client::QueryResponseFut<()> {
        let search_results_proxy = self.search_results_proxy.as_ref().expect("Search result proxy");
        search_results_proxy.service_found(&peer_id.into(), protocol_list.as_deref(), &attributes)
    }

    pub fn send_connected_with_transport(
        &mut self,
        peer_id: PeerId,
        protocol_list: Vec<bredr::ProtocolDescriptor>,
        transport: Transport,
    ) -> bt::Channel {
        let (far_bt_channel, near_bt_channel) = create_test_channels(transport);
        let far_bredr_channel: bredr::Channel =
            far_bt_channel.try_into().expect("BT Channel into FIDL BREDR Channel");

        let connection_receiver_proxy =
            self.connection_receiver_proxy.as_ref().expect("Connection receiver proxy");
        let _ = connection_receiver_proxy
            .connected(bredr::ConnectionReceiver2ConnectedRequest {
                peer_id: Some(peer_id.into()),
                channel: Some(far_bredr_channel),
                protocol: Some(protocol_list),
                ..Default::default()
            })
            .check()
            .expect("Connected");

        near_bt_channel
    }

    pub fn send_connected(
        &mut self,
        peer_id: PeerId,
        protocol_list: Vec<bredr::ProtocolDescriptor>,
    ) -> bt::Channel {
        self.send_connected_with_transport(peer_id, protocol_list, Transport::Socket)
    }
}

/// Expose the underlying ProfileRequestStream
impl Stream for TestProfileServer {
    type Item = Result<bredr::ProfileRequest, fidl::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pinned_stream = Pin::new(&mut self.profile_request_stream);
        pinned_stream.poll_next(context)
    }
}

impl Drop for TestProfileServer {
    fn drop(&mut self) {
        // TODO(b/333456020): Clean-up to not store responder.
        if let Some(responder) = self.advertise_responder.take() {
            responder
                .send(Ok(&bredr::ProfileAdvertiseResponse::default()))
                .expect("Drop responder");
        }
    }
}
