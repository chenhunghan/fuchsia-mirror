// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use anyhow::{Context as _, Error, anyhow};
use assert_matches::assert_matches;
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_net::{self as fnet, MarkDomain};
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fidl_fuchsia_posix as fposix;
use fidl_fuchsia_posix_socket::{self as fposix_socket, OptionalUint32};
use fidl_fuchsia_posix_socket_raw as fposix_socket_raw;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
};
use futures::lock::Mutex;
use futures::{StreamExt as _, TryStreamExt as _};
use pretty_assertions::assert_eq;
use std::sync::Arc;

enum IncomingService {
    Provider(fposix_socket::ProviderRequestStream),
    RawProvider(fposix_socket_raw::ProviderRequestStream),
}

trait SetMarkRespond {
    fn send(self, result: Result<(), fposix::Errno>) -> Result<(), fidl::Error>;
}

trait GetMarkRespond {
    fn send(self, result: Result<&OptionalUint32, fposix::Errno>) -> Result<(), fidl::Error>;
}

trait SocketRequestExt {
    type SetMarkResponder: SetMarkRespond;
    type GetMarkResponder: GetMarkRespond;

    fn get_request(self) -> GenericSocketRequest<Self::SetMarkResponder, Self::GetMarkResponder>;
}

enum GenericSocketRequest<SetMarkResponder, GetMarkResponder> {
    SetMark { domain: MarkDomain, mark: OptionalUint32, responder: SetMarkResponder },
    GetMark { domain: MarkDomain, responder: GetMarkResponder },
    Other,
}

macro_rules! impl_socket_request_ext {
    ($($request:path => ($set_mark:path, $get_mark:path)),*) => {
        $(
            impl SetMarkRespond for $set_mark {
                fn send(self, result: Result<(), fposix::Errno>) -> Result<(), fidl::Error> {
                    <$set_mark>::send(self, result)
                }
            }

            impl GetMarkRespond for $get_mark {
                fn send(
                    self,
                    result: Result<&OptionalUint32, fposix::Errno>,
                ) -> Result<(), fidl::Error> {
                    <$get_mark>::send(self, result)
                }
            }

            impl SocketRequestExt for $request {
                type SetMarkResponder = $set_mark;
                type GetMarkResponder = $get_mark;

                fn get_request(
                    self,
                ) -> GenericSocketRequest<Self::SetMarkResponder, Self::GetMarkResponder> {
                    use $request::*;
                    match self {
                        SetMark { domain, mark, responder } => {
                            GenericSocketRequest::SetMark { domain, mark, responder }
                        }
                        GetMark { domain, responder } => {
                            GenericSocketRequest::GetMark { domain, responder }
                        }
                        _ => GenericSocketRequest::Other,
                    }
                }
            }
        )*
    };
    ($($request:path => ($set_mark:path, $get_mark:path)),*,) => {
        impl_socket_request_ext!($($request => ($set_mark, $get_mark)),*);
    };
}

impl_socket_request_ext! {
    fposix_socket::StreamSocketRequest => (
        fposix_socket::StreamSocketSetMarkResponder,
        fposix_socket::StreamSocketGetMarkResponder
    ),
    fposix_socket::SynchronousDatagramSocketRequest => (
        fposix_socket::SynchronousDatagramSocketSetMarkResponder,
        fposix_socket::SynchronousDatagramSocketGetMarkResponder
    ),
    fposix_socket::DatagramSocketRequest => (
        fposix_socket::DatagramSocketSetMarkResponder,
        fposix_socket::DatagramSocketGetMarkResponder
    ),
    fposix_socket_raw::SocketRequest => (
        fposix_socket_raw::SocketSetMarkResponder,
        fposix_socket_raw::SocketGetMarkResponder
    ),
}

async fn run_stream_socket<Stream, Request>(
    stream: Stream,
    mark_1: Arc<Mutex<OptionalUint32>>,
    mark_2: Arc<Mutex<OptionalUint32>>,
) -> Result<(), Error>
where
    Stream: futures::Stream<Item = Result<Request, fidl::Error>>,
    Request: SocketRequestExt,
{
    stream
        .map(|result| result.context("failed request"))
        .try_for_each(|request| {
            let mark_1 = mark_1.clone();
            let mark_2 = mark_2.clone();
            async move {
                match request.get_request() {
                    GenericSocketRequest::SetMark { domain, mark, responder } => {
                        responder.send(match domain {
                            MarkDomain::Mark1 => {
                                *mark_1.lock().await = mark;
                                Ok(())
                            }
                            MarkDomain::Mark2 => {
                                *mark_2.lock().await = mark;
                                Ok(())
                            }
                        })
                    }
                    GenericSocketRequest::GetMark { domain, responder } => {
                        let lock_1 = *mark_1.lock().await;
                        let lock_2 = *mark_2.lock().await;
                        responder.send(match domain {
                            MarkDomain::Mark1 => Ok(&lock_1),
                            MarkDomain::Mark2 => Ok(&lock_2),
                        })
                    }
                    GenericSocketRequest::Other => {
                        unimplemented!("This method is unimplemented in this test")
                    }
                }
                .context("while responding")
            }
        })
        .await
}

async fn inner_provider_mock(
    handles: LocalComponentHandles,
    marks: Arc<Mutex<Vec<(Arc<Mutex<OptionalUint32>>, Arc<Mutex<OptionalUint32>>)>>>,
) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    let _ = fs
        .dir("svc")
        .add_fidl_service(IncomingService::Provider)
        .add_fidl_service(IncomingService::RawProvider);
    let _ = fs.serve_connection(handles.outgoing_dir)?;

    fn into_marks(
        marks: fnet::Marks,
    ) -> (fposix_socket::OptionalUint32, fposix_socket::OptionalUint32) {
        let fnet::Marks { mark_1, mark_2, __source_breaking } = marks;
        let into_optional_uint32 = |opt: Option<u32>| {
            opt.map_or_else(
                || fposix_socket::OptionalUint32::Unset(fposix_socket::Empty),
                |v| fposix_socket::OptionalUint32::Value(v),
            )
        };
        (into_optional_uint32(mark_1), into_optional_uint32(mark_2))
    }

    fs.for_each_concurrent(0, |service| {
        let marks = marks.clone();
        async move {
            match service {
                IncomingService::Provider(stream) => stream
                    .map(|result| result.context("Result came with error"))
                    .try_for_each(|request| {
                        let marks = marks.clone();
                        async move {
                            match request {
                                fposix_socket::ProviderRequest::StreamSocket {
                                    domain: _,
                                    proto: _,
                                    responder,
                                } => {
                                    let mark_1 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    let mark_2 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    marks.lock().await.push((mark_1.clone(), mark_2.clone()));
                                    let (client, server) =
                                        create_endpoints::<fposix_socket::StreamSocketMarker>();
                                    responder
                                        .send(Ok(client))
                                        .expect("could not respond to StreamSocket call");
                                    run_stream_socket(server.into_stream(), mark_1, mark_2).await?;
                                }
                                fposix_socket::ProviderRequest::DatagramSocketDeprecated {
                                    domain: _,
                                    proto: _,
                                    responder,
                                } => {
                                    let mark_1 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    let mark_2 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    marks.lock().await.push((mark_1.clone(), mark_2.clone()));
                                    let (client, server) = create_endpoints::<
                                        fposix_socket::SynchronousDatagramSocketMarker,
                                    >();
                                    responder
                                        .send(Ok(client))
                                        .expect("could not respond to StreamSocket call");
                                    run_stream_socket(server.into_stream(), mark_1, mark_2).await?;
                                }
                                fposix_socket::ProviderRequest::DatagramSocket {
                                    domain: _,
                                    proto,
                                    responder,
                                } => {
                                    use fposix_socket::ProviderDatagramSocketResponse::*;
                                    let mark_1 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    let mark_2 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    marks.lock().await.push((mark_1.clone(), mark_2.clone()));
                                    match proto {
                                        fposix_socket::DatagramSocketProtocol::Udp => {
                                            let (client, server) = create_endpoints::<
                                                fposix_socket::DatagramSocketMarker,
                                            >(
                                            );
                                            responder
                                                .send(Ok(DatagramSocket(client)))
                                                .expect("could not respond to DatagramSocket call");
                                            run_stream_socket(
                                                server.into_stream(),
                                                mark_1,
                                                mark_2,
                                            )
                                            .await?;
                                        }
                                        fposix_socket::DatagramSocketProtocol::IcmpEcho => {
                                            let (client, server) = create_endpoints::<
                                                fposix_socket::SynchronousDatagramSocketMarker,
                                            >(
                                            );
                                            responder
                                                .send(Ok(SynchronousDatagramSocket(client)))
                                                .expect("could not respond to DatagramSocket call");
                                            run_stream_socket(
                                                server.into_stream(),
                                                mark_1,
                                                mark_2,
                                            )
                                            .await?;
                                        }
                                    }
                                }
                                fposix_socket::ProviderRequest::StreamSocketWithOptions {
                                    domain: _,
                                    proto: _,
                                    opts,
                                    responder,
                                } => {
                                    let (mark_1, mark_2) = into_marks(opts.marks.unwrap());
                                    marks.lock().await.push((
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    ));
                                    let (client, server) =
                                        create_endpoints::<fposix_socket::StreamSocketMarker>();
                                    responder.send(Ok(client)).expect(
                                        "could not respond to StreamSocketWithOptions call",
                                    );
                                    run_stream_socket(
                                        server.into_stream(),
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    )
                                    .await?;
                                }

                                fposix_socket::ProviderRequest::DatagramSocketWithOptions {
                                    domain: _,
                                    proto,
                                    opts,
                                    responder,
                                } => {
                                    use fposix_socket::ProviderDatagramSocketWithOptionsResponse::*;
                                    let (mark_1, mark_2) = into_marks(opts.marks.unwrap());
                                    marks.lock().await.push((
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    ));
                                    match proto {
                                        fposix_socket::DatagramSocketProtocol::Udp => {
                                            let (client, server) = create_endpoints::<
                                                fposix_socket::DatagramSocketMarker,
                                            >(
                                            );
                                            responder
                                                .send(Ok(DatagramSocket(client)))
                                                .expect("could not respond to DatagramSocketWithOptions call");
                                            run_stream_socket(
                                                server.into_stream(),
                                                Arc::new(Mutex::new(mark_1)),
                                                Arc::new(Mutex::new(mark_2)),
                                            )
                                            .await?;
                                        }
                                        fposix_socket::DatagramSocketProtocol::IcmpEcho => {
                                            let (client, server) = create_endpoints::<
                                                fposix_socket::SynchronousDatagramSocketMarker,
                                            >(
                                            );
                                            responder
                                                .send(Ok(SynchronousDatagramSocket(client)))
                                                .expect("could not respond to DatagramSocketWithOptions call");
                                            run_stream_socket(
                                                server.into_stream(),
                                                Arc::new(Mutex::new(mark_1)),
                                                Arc::new(Mutex::new(mark_2)),
                                            )
                                            .await?;
                                        }
                                    }
                                }
                                _ => unimplemented!("this method is not used in this test"),
                            }
                            Ok(())
                        }
                    })
                    .await
                    .context("Failed to serve request stream")
                    .unwrap_or_else(|e| eprintln!("Error encountered: {e:?}")),
                IncomingService::RawProvider(stream) => stream
                    .map(|result| result.context("Result came with error"))
                    .try_for_each(|request| {
                        let marks = marks.clone();
                        async move {
                            match request {
                                fposix_socket_raw::ProviderRequest::Socket {
                                    domain: _,
                                    proto: _,
                                    responder,
                                } => {
                                    let mark_1 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    let mark_2 = Arc::new(Mutex::new(OptionalUint32::Unset(
                                        fposix_socket::Empty,
                                    )));
                                    marks.lock().await.push((mark_1.clone(), mark_2.clone()));
                                    let (client, server) =
                                        create_endpoints::<fposix_socket_raw::SocketMarker>();
                                    responder
                                        .send(Ok(client))
                                        .expect("could not respond to StreamSocket call");
                                    run_stream_socket(server.into_stream(), mark_1, mark_2).await?;
                                }
                                fposix_socket_raw::ProviderRequest::SocketWithOptions {
                                    domain: _,
                                    proto: _,
                                    opts,
                                    responder,
                                } => {
                                    let (mark_1, mark_2) = into_marks(opts.marks.unwrap());
                                    marks.lock().await.push((
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    ));
                                    let (client, server) =
                                        create_endpoints::<fposix_socket_raw::SocketMarker>();
                                    responder
                                        .send(Ok(client))
                                        .expect("could not respond to SocketWithOptions call");
                                    run_stream_socket(
                                        server.into_stream(),
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    )
                                    .await?;
                                }
                            }
                            Ok(())
                        }
                    })
                    .await
                    .context("Failed to serve request stream")
                    .unwrap_or_else(|e| eprintln!("Error encountered: {e:?}")),
            }
        }
    }).await;

    Ok(())
}

#[derive(Default)]
struct MockNetcfgState {
    default_mark: Option<u32>,
    notifier: Vec<futures::channel::oneshot::Sender<()>>,
    sync_notify: Option<futures::channel::oneshot::Sender<()>>,
}

impl MockNetcfgState {
    fn notify_waiters(&mut self) {
        for sender in self.notifier.drain(..) {
            sender.send(()).expect("notify waiter");
        }
    }
}

/// Controller for mock Netcfg serving `fuchsia.net.policy.properties/Networks`.
///
/// Updates default network marks and synchronizes with socket-proxy.
#[derive(Clone, Default)]
struct MockNetcfg {
    state: Arc<Mutex<MockNetcfgState>>,
}

impl MockNetcfg {
    async fn sync(&self) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.state.lock().await.sync_notify = Some(tx);
        self.state.lock().await.notify_waiters();
        let () = rx.await.expect("wait for sync");
    }

    async fn set_default_mark(&self, mark: Option<u32>) {
        {
            let mut state = self.state.lock().await;
            state.default_mark = mark;
        }
        self.sync().await;
    }

    async fn wait_for_change(&self) {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.state.lock().await.notifier.push(tx);
        let () = rx.await.expect("wait for change notification");
    }

    async fn notify_sync_if_pending(&self) {
        if let Some(tx) = self.state.lock().await.sync_notify.take() {
            tx.send(()).expect("notify sync waiter");
        }
    }
}

enum NetcfgService {
    Properties(fnp_properties::NetworksRequestStream),
}

async fn handle_watch_default(
    mock_netcfg: &MockNetcfg,
    last_reported_has_default: &mut Option<bool>,
    responder: fnp_properties::NetworksWatchDefaultResponder,
) {
    loop {
        let has_default = mock_netcfg.state.lock().await.default_mark.is_some();
        if *last_reported_has_default != Some(has_default) {
            *last_reported_has_default = Some(has_default);
            let response = if has_default {
                let (token_handle, _peer_token) = zx::EventPair::create();
                fnp_properties::NetworksWatchDefaultResponse::Network(
                    fnp_properties::NetworkToken { value: token_handle },
                )
            } else {
                fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(
                    fnp_properties::Empty,
                )
            };
            responder.send(response).expect("send response");
            break;
        }

        if !has_default {
            mock_netcfg.notify_sync_if_pending().await;
        }
        mock_netcfg.wait_for_change().await;
    }
}

fn spawn_property_watcher(
    scope: &fasync::Scope,
    mock_netcfg: MockNetcfg,
    watcher: fidl::endpoints::ServerEnd<fnp_properties::PropertyWatcherMarker>,
) {
    let mut watcher_stream = watcher.into_stream();
    let _watcher_task = scope.spawn(async move {
        let mut last_reported_mark = None;
        while let Some(fnp_properties::PropertyWatcherRequest::Watch { responder }) =
            watcher_stream.try_next().await.expect("watcher request error")
        {
            loop {
                let current = mock_netcfg.state.lock().await.default_mark;
                if let Some(mark) = current {
                    if last_reported_mark != Some(mark) {
                        last_reported_mark = Some(mark);
                        let marks = fnet::Marks { mark_1: Some(mark), ..Default::default() };
                        let update = fnp_properties::PropertyUpdate {
                            socket_marks: Some(marks),
                            ..Default::default()
                        };
                        responder.send(Ok(&update)).expect("send response");
                        break;
                    }
                    mock_netcfg.notify_sync_if_pending().await;
                }
                mock_netcfg.wait_for_change().await;
            }
        }
    });
}

/// Simulates Netcfg's properties server to verify mark updates in integration tests.
async fn netcfg_mock(handles: LocalComponentHandles, mock_netcfg: MockNetcfg) -> Result<(), Error> {
    let scope = fasync::Scope::new();
    let mut fs = ServiceFs::new();
    let _svc_dir = fs.dir("svc").add_fidl_service(NetcfgService::Properties);
    let _fs = fs.serve_connection(handles.outgoing_dir)?;

    fs.map(Ok)
        .try_for_each_concurrent(0, |req| async {
            match req {
                NetcfgService::Properties(mut stream) => {
                    let mut last_reported_has_default = None;
                    while let Some(req) = stream.try_next().await? {
                        match req {
                            fnp_properties::NetworksRequest::WatchDefault { responder } => {
                                handle_watch_default(
                                    &mock_netcfg,
                                    &mut last_reported_has_default,
                                    responder,
                                )
                                .await;
                            }
                            fnp_properties::NetworksRequest::WatchProperties {
                                payload,
                                responder,
                            } => {
                                let watcher = payload.watcher.expect("watcher must be provided");
                                responder.send(Ok(())).expect("send response");
                                spawn_property_watcher(&scope, mock_netcfg.clone(), watcher);
                            }
                            fnp_properties::NetworksRequest::_UnknownMethod { .. } => {}
                        }
                    }
                }
            }
            Ok(())
        })
        .await
}

/// Integration test fixture encapsulating the component realm and mock dependencies.
struct TestRealm {
    /// The running component test realm.
    realm: RealmInstance,
    /// Controller for the mock Netcfg component.
    mock_netcfg: MockNetcfg,
}

impl TestRealm {
    async fn new() -> Result<Self, Error> {
        let mock_netcfg = MockNetcfg::default();
        let marks = Arc::new(Mutex::new(Vec::new()));
        let builder = RealmBuilder::new().await?;
        let inner_provider = builder
            .add_local_child(
                "inner_provider",
                {
                    let marks = marks.clone();
                    move |handles: LocalComponentHandles| {
                        Box::pin(inner_provider_mock(handles, marks.clone()))
                    }
                },
                ChildOptions::new(),
            )
            .await?;
        let netcfg = builder
            .add_local_child(
                "netcfg",
                {
                    let mock_netcfg = mock_netcfg.clone();
                    move |handles: LocalComponentHandles| {
                        Box::pin(netcfg_mock(handles, mock_netcfg.clone()))
                    }
                },
                ChildOptions::new().eager(),
            )
            .await?;
        let socket_proxy = builder
            .add_child("socket_proxy", "#meta/network-socket-proxy.cm", ChildOptions::new().eager())
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fposix_socket::ProviderMarker>())
                    .capability(Capability::protocol::<fposix_socket_raw::ProviderMarker>())
                    .from(&inner_provider)
                    .to(&socket_proxy),
            )
            .await?;

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fnp_properties::NetworksMarker>())
                    .from(&netcfg)
                    .to(&socket_proxy),
            )
            .await?;

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fposix_socket::ProviderMarker>())
                    .capability(Capability::protocol::<fposix_socket_raw::ProviderMarker>())
                    .from(&socket_proxy)
                    .to(Ref::parent()),
            )
            .await?;

        let realm = builder.build().await?;
        Ok(Self { realm, mock_netcfg })
    }

    fn connect_to_protocol<P: fclient::Connect>(&self) -> Result<P, Error> {
        self.realm.root.connect_to_protocol_at_exposed_dir::<P>().context("connect to protocol")
    }
}

const DEFAULT_SOCKET_MARK: u32 = 123;

async fn assert_all_socket_types_have_mark(
    posix_socket: &fposix_socket::ProviderProxy,
    posix_socket_raw: &fposix_socket_raw::ProviderProxy,
    expected_mark: OptionalUint32,
) -> Result<(), Error> {
    {
        let socket = posix_socket
            .stream_socket(fposix_socket::Domain::Ipv4, fposix_socket::StreamSocketProtocol::Tcp)
            .await?
            .map_err(|e| anyhow!("Could not get socket: {e:?}"))?
            .into_proxy();
        assert_eq!(socket.get_mark(MarkDomain::Mark1).await?, Ok(expected_mark));
    }

    {
        let socket = posix_socket
            .datagram_socket_deprecated(
                fposix_socket::Domain::Ipv4,
                fposix_socket::DatagramSocketProtocol::Udp,
            )
            .await?
            .map_err(|e| anyhow!("Could not get socket: {e:?}"))?
            .into_proxy();
        assert_eq!(socket.get_mark(MarkDomain::Mark1).await?, Ok(expected_mark));
    }

    {
        let response = posix_socket
            .datagram_socket(
                fposix_socket::Domain::Ipv4,
                fposix_socket::DatagramSocketProtocol::Udp,
            )
            .await?
            .map_err(|e| anyhow!("Could not get socket: {e:?}"))?;
        let socket = assert_matches!(
            response,
            fposix_socket::ProviderDatagramSocketResponse::DatagramSocket(s) => s
        )
        .into_proxy();
        assert_eq!(socket.get_mark(MarkDomain::Mark1).await?, Ok(expected_mark));
    }

    {
        let response = posix_socket
            .datagram_socket(
                fposix_socket::Domain::Ipv4,
                fposix_socket::DatagramSocketProtocol::IcmpEcho,
            )
            .await?
            .map_err(|e| anyhow!("Could not get socket: {e:?}"))?;
        let socket = assert_matches!(
            response,
            fposix_socket::ProviderDatagramSocketResponse::SynchronousDatagramSocket(s) => s
        )
        .into_proxy();
        assert_eq!(socket.get_mark(MarkDomain::Mark1).await?, Ok(expected_mark));
    }

    {
        let socket = posix_socket_raw
            .socket(
                fposix_socket::Domain::Ipv4,
                &fposix_socket_raw::ProtocolAssociation::Unassociated(fposix_socket_raw::Empty),
            )
            .await?
            .map_err(|e| anyhow!("Could not get socket: {e:?}"))?
            .into_proxy();
        assert_eq!(socket.get_mark(MarkDomain::Mark1).await?, Ok(expected_mark));
    }

    Ok(())
}

#[fuchsia::test]
async fn integration() -> Result<(), Error> {
    let test_realm = TestRealm::new().await?;
    let posix_socket: fposix_socket::ProviderProxy = test_realm.connect_to_protocol()?;
    let posix_socket_raw: fposix_socket_raw::ProviderProxy = test_realm.connect_to_protocol()?;

    // Sockets created without a default network are unmarked.
    assert_all_socket_types_have_mark(
        &posix_socket,
        &posix_socket_raw,
        OptionalUint32::Unset(fposix_socket::Empty),
    )
    .await?;

    // Sockets receive the active default network mark.
    test_realm.mock_netcfg.set_default_mark(Some(DEFAULT_SOCKET_MARK)).await;
    assert_all_socket_types_have_mark(
        &posix_socket,
        &posix_socket_raw,
        OptionalUint32::Value(DEFAULT_SOCKET_MARK),
    )
    .await?;

    // Sockets are unmarked again once the default network is cleared.
    test_realm.mock_netcfg.set_default_mark(None).await;
    assert_all_socket_types_have_mark(
        &posix_socket,
        &posix_socket_raw,
        OptionalUint32::Unset(fposix_socket::Empty),
    )
    .await?;

    Ok(())
}
