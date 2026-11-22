// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use anyhow::{Context as _, Error, anyhow};
use assert_matches::assert_matches;
use diagnostics_assertions::assert_data_tree;
use diagnostics_reader::{ArchiveReader, DiagnosticsHierarchy};
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_net::{self as fnet, MarkDomain};
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy;
use fidl_fuchsia_posix as fposix;
use fidl_fuchsia_posix_socket::{self as fposix_socket, OptionalUint32};
use fidl_fuchsia_posix_socket_raw as fposix_socket_raw;
use fuchsia_async as fasync;
use fuchsia_component::client as fclient;
use fuchsia_component::server::{ServiceFs, ServiceFsDir};
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
};
use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::{StreamExt as _, TryStreamExt as _};
use pretty_assertions::assert_eq;
use std::collections::HashMap;
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
                    .try_for_each_concurrent(None, |request| {
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
                                    use fposix_socket::ProviderDatagramSocketResponse::*;
                                    responder
                                        .send(Ok(SynchronousDatagramSocket(client)))
                                        .expect("could not respond to DatagramSocket call");
                                    run_stream_socket(server.into_stream(), mark_1, mark_2).await?;
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
                                    proto: _,
                                    opts,
                                    responder,
                                } => {
                                    use fposix_socket::ProviderDatagramSocketWithOptionsResponse::*;
                                    let (mark_1, mark_2) = into_marks(opts.marks.unwrap());
                                    marks.lock().await.push((
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    ));
                                    let (client, server) = create_endpoints::<
                                        fposix_socket::SynchronousDatagramSocketMarker,
                                    >();
                                    responder.send(Ok(SynchronousDatagramSocket(client))).expect(
                                        "could not respond to DatagramSocketWithOptions call",
                                    );
                                    run_stream_socket(
                                        server.into_stream(),
                                        Arc::new(Mutex::new(mark_1)),
                                        Arc::new(Mutex::new(mark_2)),
                                    )
                                    .await?;
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
    })
    .await;

    Ok(())
}

/// Pending `MockNetcfg::sync` waiter `(target, tx)`.
///
/// `target` selects which watcher must become idle (`None` for the default
/// network watcher, `Some(koid)` for a non-default network watcher) before `tx`
/// signals that socket-proxy has polled again after consuming pending updates.
type PendingSync = (Option<zx::Koid>, oneshot::Sender<()>);

#[derive(Debug)]
struct NonDefaultNetwork {
    // Held open while the network is registered so the NetworkToken's peer stays alive.
    _peer_token: zx::EventPair,
    mark: Option<u32>,
}

#[derive(Default)]
struct MockNetcfgState {
    default_mark: Option<u32>,
    default_token: Option<(zx::EventPair, zx::EventPair)>,
    non_default_networks: HashMap<zx::Koid, NonDefaultNetwork>,
    watch_properties_counts: HashMap<zx::Koid, usize>,
    notifier: Vec<oneshot::Sender<()>>,
    sync_notify: Option<PendingSync>,
}

impl MockNetcfgState {
    fn default_koid(&self) -> Option<zx::Koid> {
        self.default_token.as_ref().map(|(t, _peer)| t.koid().expect("get default token koid"))
    }

    fn notify_waiters(&mut self) {
        for sender in self.notifier.drain(..) {
            sender.send(()).expect("notify waiter");
        }
    }

    fn notify_sync_if_pending(&mut self, target: Option<zx::Koid>) {
        if let Some((_sync_target, tx)) =
            self.sync_notify.take_if(|(sync_target, _tx)| *sync_target == target)
        {
            tx.send(()).expect("notify sync waiter");
        }
    }

    fn register_waiter(&mut self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.notifier.push(tx);
        rx
    }
}

enum WatchStep<T> {
    Respond(T),
    Wait(oneshot::Receiver<()>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchedNetwork {
    Default(zx::Koid),
    NonDefault(zx::Koid),
}

/// Controller for mock Netcfg serving `fuchsia.net.policy.properties/Networks`.
///
/// Updates default and non-default network marks and synchronizes with
/// socket-proxy.
#[derive(Clone, Default)]
struct MockNetcfg {
    state: Arc<Mutex<MockNetcfgState>>,
}

impl MockNetcfg {
    async fn sync(&self, target: Option<zx::Koid>) {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state.sync_notify = Some((target, tx));
            state.notify_waiters();
        }
        let () = rx.await.expect("wait for sync");
    }

    async fn set_default_mark(&self, mark: Option<u32>) {
        {
            let mut state = self.state.lock().await;
            state.default_mark = mark;
            match mark {
                Some(_) if state.default_token.is_none() => {
                    state.default_token = Some(zx::EventPair::create());
                }
                None => {
                    state.default_token = None;
                }
                Some(_) => {}
            }
        }
        self.sync(None).await;
    }

    /// Replaces the default network with a fresh token (so watchers tied to the
    /// prior default token see it disappear) and waits for socket-proxy to sync.
    async fn switch_default_network(&self, mark: u32) {
        {
            let mut state = self.state.lock().await;
            state.default_token = Some(zx::EventPair::create());
            state.default_mark = Some(mark);
        }
        self.sync(None).await;
    }

    /// Returns a duplicated handle to the active default network's token,
    /// simulating a client that obtained it via `Networks.WatchDefault`.
    async fn default_network_token(&self) -> fnp_properties::NetworkToken {
        let state = self.state.lock().await;
        let (token, _peer) = state.default_token.as_ref().expect("default network must be active");
        let handle =
            token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate default token");
        fnp_properties::NetworkToken { value: handle }
    }

    /// Registers a non-default network with `mark`, holding its peer EventPair
    /// open while active, and wakes waiting watchers.
    async fn add_non_default_network(&self, mark: u32) -> (zx::Koid, fnp_properties::NetworkToken) {
        let (token_handle, peer_token) = zx::EventPair::create();
        let koid = token_handle.koid().expect("get token koid");
        let mut state = self.state.lock().await;
        assert_matches!(
            state
                .non_default_networks
                .insert(koid, NonDefaultNetwork { _peer_token: peer_token, mark: Some(mark) }),
            None
        );
        state.notify_waiters();
        (koid, fnp_properties::NetworkToken { value: token_handle })
    }

    /// Updates a registered non-default network's mark and waits until its
    /// `PropertyWatcher` polls again after consuming the update.
    async fn set_non_default_network_mark(&self, koid: zx::Koid, mark: Option<u32>) {
        {
            let mut state = self.state.lock().await;
            let network = state
                .non_default_networks
                .get_mut(&koid)
                .unwrap_or_else(|| panic!("network {koid:?} must already be registered"));
            network.mark = mark;
        }
        self.sync(Some(koid)).await;
    }

    /// Unregisters a non-default network and drops its peer token so watchers
    /// observe `NetworkGone`.
    async fn remove_non_default_network(&self, koid: zx::Koid) {
        let mut state = self.state.lock().await;
        let removed: Option<NonDefaultNetwork> = state.non_default_networks.remove(&koid);
        assert!(removed.is_some(), "network {koid:?} must already be registered");
        state.notify_waiters();
    }

    /// Returns how many `WatchProperties` RPCs socket-proxy issued for `koid`.
    async fn watch_properties_count(&self, koid: zx::Koid) -> usize {
        self.state.lock().await.watch_properties_counts.get(&koid).copied().unwrap_or(0)
    }
}

enum NetcfgService {
    Properties(fnp_properties::NetworksRequestStream),
}

async fn handle_watch_default(
    mock_netcfg: &MockNetcfg,
    last_reported_default_koid: &mut Option<Option<zx::Koid>>,
    responder: fnp_properties::NetworksWatchDefaultResponder,
) {
    loop {
        let step = {
            let mut state = mock_netcfg.state.lock().await;
            let current_koid = state.default_koid();
            if *last_reported_default_koid != Some(current_koid) {
                *last_reported_default_koid = Some(current_koid);
                WatchStep::Respond(match &state.default_token {
                    Some((token, _peer)) => fnp_properties::NetworksWatchDefaultResponse::Network(
                        fnp_properties::NetworkToken {
                            value: token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("duplicate default token"),
                        },
                    ),
                    None => fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(
                        fnp_properties::Empty,
                    ),
                })
            } else {
                if state.default_mark.is_none() {
                    state.notify_sync_if_pending(None);
                }
                WatchStep::Wait(state.register_waiter())
            }
        };
        match step {
            WatchStep::Respond(response) => {
                responder.send(response).expect("send response");
                break;
            }
            WatchStep::Wait(rx) => {
                let () = rx.await.expect("wait for change notification");
            }
        }
    }
}

fn spawn_property_watcher(
    scope: &fasync::Scope,
    mock_netcfg: MockNetcfg,
    watched: WatchedNetwork,
    watcher: fidl::endpoints::ServerEnd<fnp_properties::PropertyWatcherMarker>,
) {
    let mut watcher_stream = watcher.into_stream();
    let sync_target = match watched {
        WatchedNetwork::Default(_) => None,
        WatchedNetwork::NonDefault(koid) => Some(koid),
    };
    let _: fasync::JoinHandle<()> = scope.spawn(async move {
        let mut last_reported_mark: Option<Option<u32>> = None;
        while let Some(fnp_properties::PropertyWatcherRequest::Watch { responder }) =
            watcher_stream.try_next().await.expect("watcher request error")
        {
            loop {
                let step = {
                    let mut state = mock_netcfg.state.lock().await;
                    let registered = match watched {
                        WatchedNetwork::NonDefault(koid) => {
                            state.non_default_networks.get(&koid).map(|network| network.mark)
                        }
                        WatchedNetwork::Default(koid) => {
                            (state.default_koid() == Some(koid)).then_some(state.default_mark)
                        }
                    };
                    match registered {
                        None => WatchStep::Respond(Err(
                            fnp_properties::PropertyWatcherError::NetworkGone,
                        )),
                        Some(mark) if last_reported_mark != Some(mark) => {
                            last_reported_mark = Some(mark);
                            let marks = fnet::Marks { mark_1: mark, ..Default::default() };
                            let update = fnp_properties::PropertyUpdate {
                                socket_marks: Some(marks),
                                ..Default::default()
                            };
                            WatchStep::Respond(Ok(update))
                        }
                        Some(_unchanged_mark) => {
                            state.notify_sync_if_pending(sync_target);
                            WatchStep::Wait(state.register_waiter())
                        }
                    }
                };
                match step {
                    WatchStep::Respond(Ok(update)) => {
                        if let Err(e) = responder.send(Ok(&update)) {
                            log::info!("Failed to send property update: {e:?}");
                            return;
                        }
                        break;
                    }
                    WatchStep::Respond(Err(err)) => {
                        if let Err(e) = responder.send(Err(err)) {
                            log::info!("Failed to send property watcher error {err:?}: {e:?}");
                        }
                        return;
                    }
                    WatchStep::Wait(rx) => {
                        let () = rx.await.expect("wait for change notification");
                    }
                }
            }
        }
    });
}

/// Simulates Netcfg's properties server to verify mark updates in integration tests.
async fn netcfg_mock(handles: LocalComponentHandles, mock_netcfg: MockNetcfg) -> Result<(), Error> {
    let scope = fasync::Scope::new();
    let mut fs = ServiceFs::new();
    let _: &mut ServiceFsDir<'_, _> = fs.dir("svc").add_fidl_service(NetcfgService::Properties);
    let _: &mut ServiceFs<_> = fs.serve_connection(handles.outgoing_dir)?;

    fs.map(Ok)
        .try_for_each_concurrent(0, |req| async {
            match req {
                NetcfgService::Properties(mut stream) => {
                    let mut last_reported_default_koid = None;
                    while let Some(req) = stream.try_next().await? {
                        match req {
                            fnp_properties::NetworksRequest::WatchDefault { responder } => {
                                handle_watch_default(
                                    &mock_netcfg,
                                    &mut last_reported_default_koid,
                                    responder,
                                )
                                .await;
                            }
                            fnp_properties::NetworksRequest::WatchProperties {
                                payload,
                                responder,
                            } => {
                                let token =
                                    payload.network.expect("network token must be provided");
                                let koid = token.value.koid().expect("token koid");
                                let watcher = payload.watcher.expect("watcher must be provided");
                                let watched = {
                                    let mut state = mock_netcfg.state.lock().await;
                                    *state.watch_properties_counts.entry(koid).or_insert(0) += 1;
                                    if state.default_koid() == Some(koid) {
                                        Ok(WatchedNetwork::Default(koid))
                                    } else if state.non_default_networks.contains_key(&koid) {
                                        Ok(WatchedNetwork::NonDefault(koid))
                                    } else {
                                        Err(fnp_properties::WatchError::InvalidNetworkToken)
                                    }
                                };
                                match watched {
                                    Ok(watched) => {
                                        responder.send(Ok(())).expect("send response");
                                        spawn_property_watcher(
                                            &scope,
                                            mock_netcfg.clone(),
                                            watched,
                                            watcher,
                                        );
                                    }
                                    Err(e) => {
                                        responder.send(Err(e)).expect("send response");
                                    }
                                }
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
            .add_child(
                "socket_proxy",
                "network-socket-proxy#meta/network-socket-proxy.cm",
                ChildOptions::new().eager(),
            )
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
                    .capability(Capability::protocol::<
                        fnp_socketproxy::SocketProviderConnectorMarker,
                    >())
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

    async fn socket_proxy_inspect(&self) -> DiagnosticsHierarchy {
        let mut results = ArchiveReader::inspect()
            .select_all_for_component(format!(
                "realm_builder:{}/socket_proxy",
                self.realm.root.child_name()
            ))
            .snapshot()
            .await
            .expect("snapshot socket_proxy inspect")
            .into_iter();
        let data = results.next().expect("socket_proxy inspect entry");
        assert!(results.next().is_none(), "expected single socket_proxy inspect entry");
        data.payload.expect("socket_proxy inspect payload")
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
            fposix_socket::ProviderDatagramSocketResponse::SynchronousDatagramSocket(s) => s
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

#[fuchsia::test]
async fn socket_provider_connector() {
    const DEFAULT_MARK: u32 = 100;
    const SWITCHED_DEFAULT_MARK: u32 = 101;
    const NON_DEFAULT_MARK: u32 = 200;
    const UPDATED_NON_DEFAULT_MARK: u32 = 202;
    const CALLER_SUPPLIED_MARK: u32 = 999;

    let test_realm = TestRealm::new().await.expect("create test realm");
    let connector: fnp_socketproxy::SocketProviderConnectorProxy =
        test_realm.connect_to_protocol().expect("connect SocketProviderConnector");

    let connect_provider = |network_token: Option<fnp_properties::NetworkToken>| {
        let (provider, server_end) =
            fidl::endpoints::create_proxy::<fposix_socket::ProviderMarker>();
        connector
            .connect(
                fnp_socketproxy::SocketProviderOptions { network_token, ..Default::default() },
                server_end,
            )
            .expect("connect provider");
        provider
    };

    let get_mark = |provider: &fposix_socket::ProviderProxy| {
        let provider = provider.clone();
        async move {
            let socket = provider
                .stream_socket(
                    fposix_socket::Domain::Ipv4,
                    fposix_socket::StreamSocketProtocol::Tcp,
                )
                .await
                .expect("stream_socket fidl")
                .expect("stream_socket")
                .into_proxy();
            socket.get_mark(MarkDomain::Mark1).await.expect("get_mark fidl").expect("get_mark")
        }
    };

    let duplicate_token = |token: &fnp_properties::NetworkToken| fnp_properties::NetworkToken {
        value: token
            .value
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("duplicate network token"),
    };

    async fn assert_closed_with(provider: fposix_socket::ProviderProxy, expected: fidl::Status) {
        assert_matches!(
            provider.take_event_stream().next().await,
            Some(Err(fidl::Error::ClientChannelClosed { epitaph, .. }))
                if epitaph == expected
        );
    }

    // An invalid NetworkToken closes the provider channel with NOT_FOUND.
    let (invalid_handle, _invalid_peer): (_, zx::EventPair) = zx::EventPair::create();
    let invalid_provider =
        connect_provider(Some(fnp_properties::NetworkToken { value: invalid_handle }));
    assert_closed_with(invalid_provider, fidl::Status::NOT_FOUND).await;

    // Omitting a NetworkToken follows the default network's socket marks, including when no
    // default network is active.
    let default_provider = connect_provider(None);
    assert_eq!(get_mark(&default_provider).await, OptionalUint32::Unset(fposix_socket::Empty));
    test_realm.mock_netcfg.set_default_mark(Some(DEFAULT_MARK)).await;
    assert_eq!(get_mark(&default_provider).await, OptionalUint32::Value(DEFAULT_MARK));
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        marks: {
            mark_1: u64::from(DEFAULT_MARK),
            has_mark: true,
        },
        watched_networks: {},
    });

    // A token from WatchDefault is tied to the current default network; the
    // Provider closes with CANCELED once the default network changes.
    let watch_default_token = test_realm.mock_netcfg.default_network_token().await;
    let default_epoch_provider = connect_provider(Some(watch_default_token));
    assert_eq!(get_mark(&default_epoch_provider).await, OptionalUint32::Value(DEFAULT_MARK));

    // Providing a network-specific NetworkToken pins the vended Provider to that network's marks.
    let (non_default_koid, non_default_token) =
        test_realm.mock_netcfg.add_non_default_network(NON_DEFAULT_MARK).await;
    let duplicate_non_default_token = duplicate_token(&non_default_token);
    let removed_non_default_token = duplicate_token(&non_default_token);
    let pinned_provider = connect_provider(Some(non_default_token));
    assert_eq!(get_mark(&pinned_provider).await, OptionalUint32::Value(NON_DEFAULT_MARK));
    assert_eq!(test_realm.mock_netcfg.watch_properties_count(non_default_koid).await, 1);

    // Connecting a second Provider for the same network reuses the cached watcher and marks
    // without issuing another WatchProperties round trip.
    let second_pinned_provider = connect_provider(Some(duplicate_non_default_token));
    assert_eq!(get_mark(&second_pinned_provider).await, OptionalUint32::Value(NON_DEFAULT_MARK));
    assert_eq!(test_realm.mock_netcfg.watch_properties_count(non_default_koid).await, 1);
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        marks: {
            mark_1: u64::from(DEFAULT_MARK),
            has_mark: true,
        },
        watched_networks: {
            "0": {
                mark_1: u64::from(DEFAULT_MARK),
                has_mark: true,
            },
            "1": {
                mark_1: u64::from(NON_DEFAULT_MARK),
                has_mark: true,
            },
        },
    });

    // Caller-supplied SocketCreationOptions.marks on a token-bound Provider are overridden by
    // the bound network's marks.
    let overridden_socket = pinned_provider
        .stream_socket_with_options(
            fposix_socket::Domain::Ipv4,
            fposix_socket::StreamSocketProtocol::Tcp,
            fposix_socket::SocketCreationOptions {
                marks: Some(fnet::Marks {
                    mark_1: Some(CALLER_SUPPLIED_MARK),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("stream_socket_with_options fidl")
        .expect("stream_socket_with_options")
        .into_proxy();
    assert_eq!(
        overridden_socket.get_mark(MarkDomain::Mark1).await.expect("get_mark fidl"),
        Ok(OptionalUint32::Value(NON_DEFAULT_MARK))
    );

    // Switching the default network updates the unbound default provider, cancels the
    // WatchDefault-bound provider, and leaves the non-default-network-pinned providers untouched.
    test_realm.mock_netcfg.switch_default_network(SWITCHED_DEFAULT_MARK).await;
    assert_eq!(get_mark(&default_provider).await, OptionalUint32::Value(SWITCHED_DEFAULT_MARK));
    assert_eq!(get_mark(&pinned_provider).await, OptionalUint32::Value(NON_DEFAULT_MARK));
    assert_eq!(get_mark(&second_pinned_provider).await, OptionalUint32::Value(NON_DEFAULT_MARK));
    assert_closed_with(default_epoch_provider, fidl::Status::CANCELED).await;

    // Clearing the default network leaves the unbound default provider open and unmarked.
    test_realm.mock_netcfg.set_default_mark(None).await;
    assert_eq!(get_mark(&default_provider).await, OptionalUint32::Unset(fposix_socket::Empty));

    // Updating or clearing the pinned network's mark updates future sockets vended by all
    // providers bound to that network without closing them.
    test_realm
        .mock_netcfg
        .set_non_default_network_mark(non_default_koid, Some(UPDATED_NON_DEFAULT_MARK))
        .await;
    assert_eq!(get_mark(&pinned_provider).await, OptionalUint32::Value(UPDATED_NON_DEFAULT_MARK));
    assert_eq!(
        get_mark(&second_pinned_provider).await,
        OptionalUint32::Value(UPDATED_NON_DEFAULT_MARK)
    );
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        marks: {
            has_mark: false,
        },
        watched_networks: {
            "1": {
                mark_1: u64::from(UPDATED_NON_DEFAULT_MARK),
                has_mark: true,
            },
        },
    });
    test_realm.mock_netcfg.set_non_default_network_mark(non_default_koid, None).await;
    assert_eq!(get_mark(&pinned_provider).await, OptionalUint32::Unset(fposix_socket::Empty));
    assert_eq!(
        get_mark(&second_pinned_provider).await,
        OptionalUint32::Unset(fposix_socket::Empty)
    );
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        watched_networks: {
            "1": {
                has_mark: false,
            },
        },
    });

    // Removing the pinned network closes all bound provider channels with CANCELED and evicts the
    // cached watcher so subsequent connections with a stale token fail with NOT_FOUND.
    test_realm.mock_netcfg.remove_non_default_network(non_default_koid).await;
    assert_closed_with(pinned_provider, fidl::Status::CANCELED).await;
    assert_closed_with(second_pinned_provider, fidl::Status::CANCELED).await;
    let post_removal_provider = connect_provider(Some(removed_non_default_token));
    assert_closed_with(post_removal_provider, fidl::Status::NOT_FOUND).await;
    assert_eq!(test_realm.mock_netcfg.watch_properties_count(non_default_koid).await, 2);
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        watched_networks: {},
    });

    // Watching a new network after previous watchers are evicted allocates the next monotonic ID
    // (skipping failed watches and never reusing evicted IDs).
    let (_next_koid, next_token) =
        test_realm.mock_netcfg.add_non_default_network(NON_DEFAULT_MARK).await;
    let next_pinned_provider = connect_provider(Some(next_token));
    assert_eq!(get_mark(&next_pinned_provider).await, OptionalUint32::Value(NON_DEFAULT_MARK));
    assert_data_tree!(test_realm.socket_proxy_inspect().await, root: contains {
        watched_networks: {
            "2": {
                mark_1: u64::from(NON_DEFAULT_MARK),
                has_mark: true,
            },
        },
    });
}
