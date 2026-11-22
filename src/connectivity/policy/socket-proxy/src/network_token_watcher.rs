// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Watches per-`NetworkToken` socket marks and lifecycle via
//! `fuchsia.net.policy.properties.Networks`.

use fidl::Koid;
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fuchsia_async as fasync;
use fuchsia_inspect_derive::{AttachError, IValue, Inspect};
use fuchsia_sync::Mutex as SyncMutex;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::lock::Mutex;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use crate::socket_provider::{NamespaceConnector, ProtocolConnector};

type InitResult = Result<WatchedTokenNetwork, fidl::Status>;
type SharedInitReceiver = Shared<oneshot::Receiver<InitResult>>;

/// Live socket marks and terminal epitaph signal for a watched `NetworkToken`.
#[derive(Clone, Debug)]
pub(crate) struct WatchedTokenNetwork {
    pub(crate) marks: Arc<Mutex<IValue<crate::SocketMarks>>>,
    pub(crate) closed: Shared<oneshot::Receiver<fidl::Status>>,
}

/// Manages active `PropertyWatcher` tasks keyed by `NetworkToken` KOID so
/// concurrent and repeated connections to the same network share one watcher.
pub(crate) struct NetworkTokenWatchers<C = NamespaceConnector> {
    scope: fasync::Scope,
    connect_networks: C,
    entries: SyncMutex<HashMap<Koid, SharedInitReceiver>>,
    inspect_node: SyncMutex<fuchsia_inspect::Node>,
    next_inspect_id: AtomicU64,
}

impl<C> NetworkTokenWatchers<C> {
    pub(crate) fn new(connect_networks: C) -> Self {
        Self {
            scope: fasync::Scope::new_with_name("network_token_watchers"),
            connect_networks,
            entries: SyncMutex::new(HashMap::new()),
            inspect_node: SyncMutex::new(fuchsia_inspect::Node::default()),
            next_inspect_id: AtomicU64::new(0),
        }
    }
}

impl Default for NetworkTokenWatchers {
    fn default() -> Self {
        Self::new(NamespaceConnector)
    }
}

impl<C> Inspect for &NetworkTokenWatchers<C> {
    fn iattach(
        self,
        parent: &fuchsia_inspect::Node,
        name: impl AsRef<str>,
    ) -> Result<(), AttachError> {
        *self.inspect_node.lock() = parent.create_child(name.as_ref());
        Ok(())
    }
}

impl<C: ProtocolConnector<fnp_properties::NetworksMarker> + 'static> NetworkTokenWatchers<C> {
    /// Returns the live marks and closure signal for `token`, starting a new
    /// `PropertyWatcher` if one is not already cached for its KOID.
    pub(crate) async fn get_or_watch(
        self: &Arc<Self>,
        token: fnp_properties::NetworkToken,
    ) -> Result<WatchedTokenNetwork, fidl::Status> {
        let koid = token.value.koid().map_err(|status| {
            info!("Rejecting network token with invalid handle: {status:?}");
            fidl::Status::NOT_FOUND
        })?;

        // Cache active watchers by token KOID so subsequent clients binding to the
        // same network receive its marks immediately without a round trip.
        let init_rx = self
            .entries
            .lock()
            .entry(koid)
            .or_insert_with(|| {
                let (init_tx, init_rx) = oneshot::channel();
                let shared_rx = init_rx.shared();
                let token_watchers = Arc::downgrade(self);
                let _: fasync::JoinHandle<()> = self.scope.spawn(async move {
                    watch_network_token_lifecycle(koid, token, token_watchers, init_tx).await;
                });
                shared_rx
            })
            .clone();

        init_rx.await.unwrap_or_else(|_: oneshot::Canceled| Err(fidl::Status::INTERNAL))
    }
}

/// Evicts `koid` from the watcher cache once its task fails or terminates.
///
/// Takes a `Weak` reference because watcher tasks run inside `self.scope` and
/// must not hold a strong `Arc` cycle that would block `Drop` cancellation.
fn remove_token_watcher_entry<C>(token_watchers: &Weak<NetworkTokenWatchers<C>>, koid: Koid) {
    if let Some(watchers) = token_watchers.upgrade() {
        let removed: Option<SharedInitReceiver> = watchers.entries.lock().remove(&koid);
        debug_assert!(removed.is_some());
    }
}

/// Converts a `PropertyWatcher.Watch` response into a `PropertyUpdate` or a
/// terminal epitaph status (`gone_status` is `NOT_FOUND` on the initial watch
/// and `CANCELED` once steady-state watching has started).
fn map_watch_result(
    koid: Koid,
    result: Result<
        Result<fnp_properties::PropertyUpdate, fnp_properties::PropertyWatcherError>,
        fidl::Error,
    >,
    gone_status: fidl::Status,
) -> Result<fnp_properties::PropertyUpdate, fidl::Status> {
    match result {
        Ok(Ok(update)) => Ok(update),
        err @ (Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
        | Err(fidl::Error::ClientChannelClosed {
            epitaph: fidl::Epitaph::PeerClosed, ..
        })) => {
            info!("PropertyWatcher ended for {koid:?} ({err:?}); closing with {gone_status}");
            Err(gone_status)
        }
        err => {
            warn!("PropertyWatcher.Watch failed for {koid:?}: {err:?}");
            Err(fidl::Status::INTERNAL)
        }
    }
}

/// Connects to `Networks`, registers a `PropertyWatcher` for `token`, and reads
/// its initial `SocketMarks` snapshot in a single pipelined round trip.
async fn connect_and_snapshot_token_watcher<
    C: ProtocolConnector<fnp_properties::NetworksMarker>,
>(
    koid: Koid,
    token: fnp_properties::NetworkToken,
    token_watchers: &Weak<NetworkTokenWatchers<C>>,
) -> Result<(crate::SocketMarks, fnp_properties::PropertyWatcherProxy), fidl::Status> {
    let networks = {
        let Some(watchers) = token_watchers.upgrade() else {
            return Err(fidl::Status::CANCELED);
        };
        watchers.connect_networks.connect().map_err(|e| {
            info!("Unable to connect to fuchsia.net.policy.properties.Networks: {e:?}");
            fidl::Status::INTERNAL
        })?
    };
    let (watcher, watcher_server_end) =
        fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
    let watch_props_fut =
        networks.watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
            network: Some(token),
            properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
            watcher: Some(watcher_server_end),
            ..Default::default()
        });
    // Pipeline the initial `Watch` alongside `WatchProperties` so valid tokens
    // obtain their initial socket marks in a single round trip, while still
    // returning immediately if `WatchProperties` itself fails.
    let initial_watch_fut = watcher.watch();

    match watch_props_fut.await {
        Ok(Ok(())) => {}
        Ok(Err(fnp_properties::WatchError::InvalidNetworkToken)) => {
            info!("Rejecting invalid network token ({koid:?})");
            return Err(fidl::Status::NOT_FOUND);
        }
        err => {
            warn!("WatchProperties failed for {koid:?}: {err:?}");
            return Err(fidl::Status::INTERNAL);
        }
    }

    let update = map_watch_result(koid, initial_watch_fut.await, fidl::Status::NOT_FOUND)?;
    Ok((crate::SocketMarks::from(update.socket_marks.unwrap_or_default()), watcher))
}

/// Owns the full background task for one cached `NetworkToken` KOID: initializes
/// the watcher, broadcasts the shared `WatchedTokenNetwork` (or init error) via
/// `init_tx` to all coalesced callers, then applies mark updates until the
/// network disappears and evicts the cache entry before signaling `closed_tx`.
async fn watch_network_token_lifecycle<C: ProtocolConnector<fnp_properties::NetworksMarker>>(
    koid: Koid,
    token: fnp_properties::NetworkToken,
    token_watchers: Weak<NetworkTokenWatchers<C>>,
    init_tx: oneshot::Sender<InitResult>,
) {
    let (initial_marks, watcher) =
        match connect_and_snapshot_token_watcher(koid, token, &token_watchers).await {
            Ok(res) => res,
            Err(status) => {
                remove_token_watcher_entry(&token_watchers, koid);
                if init_tx.send(Err(status)).is_err() {
                    debug!(
                        "Network token watcher init failed after all callers dropped \
                        (koid={koid:?}, status={status})"
                    );
                }
                return;
            }
        };

    let marks = {
        let Some(watchers) = token_watchers.upgrade() else {
            return;
        };
        let inspect_node = watchers.inspect_node.lock();
        let inspect_id = watchers.next_inspect_id.fetch_add(1, Ordering::Relaxed);
        Arc::new(Mutex::new(IValue::attached(initial_marks, &inspect_node, inspect_id.to_string())))
    };

    let (closed_tx, closed_rx) = oneshot::channel::<fidl::Status>();
    let watched = WatchedTokenNetwork { marks: Arc::clone(&marks), closed: closed_rx.shared() };
    if init_tx.send(Ok(watched)).is_err() {
        debug!("All receivers dropped before network token watcher ready (koid={koid:?})");
        return;
    }

    let status = loop {
        match map_watch_result(koid, watcher.watch().await, fidl::Status::CANCELED) {
            Ok(update) => {
                // `None` indicates `socket_marks` was not part of the delta update;
                // clearing a mark sends `Some(Marks { mark_1: None, mark_2: None, .. })`.
                if let Some(new_marks) = update.socket_marks {
                    marks.lock().await.iset(crate::SocketMarks::from(new_marks));
                }
            }
            Err(status) => break status,
        }
    };
    remove_token_watcher_entry(&token_watchers, koid);
    if closed_tx.send(status).is_err() {
        debug!(
            "Watched network token ended with no active bound providers remaining \
            (koid={koid:?}, status={status})"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Error;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::RequestStream as _;
    use fidl_fuchsia_net as fnet;
    use fidl_fuchsia_posix_socket::{self as fposix_socket, OptionalUint32};
    use futures::StreamExt as _;
    use futures::future::join;
    use std::pin::pin;

    const INITIAL_MARK_1: u32 = 42;
    const INITIAL_MARK_2: u32 = 99;

    #[derive(Clone)]
    struct MockNetworksConnector(fnp_properties::NetworksProxy);

    impl ProtocolConnector<fnp_properties::NetworksMarker> for MockNetworksConnector {
        fn connect(&self) -> Result<fnp_properties::NetworksProxy, Error> {
            Ok(self.0.clone())
        }
    }

    #[derive(Clone)]
    struct FailingNetworksConnector;

    impl ProtocolConnector<fnp_properties::NetworksMarker> for FailingNetworksConnector {
        fn connect(&self) -> Result<fnp_properties::NetworksProxy, Error> {
            Err(anyhow::anyhow!("Networks protocol unavailable"))
        }
    }

    struct WatchersHarness {
        watchers: Arc<NetworkTokenWatchers<MockNetworksConnector>>,
        networks_stream: fnp_properties::NetworksRequestStream,
        inspector: fuchsia_inspect::Inspector,
    }

    impl WatchersHarness {
        fn new() -> Self {
            let (proxy, networks_stream) =
                fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
            let watchers = Arc::new(NetworkTokenWatchers::new(MockNetworksConnector(proxy)));
            let inspector = fuchsia_inspect::Inspector::default();
            watchers
                .iattach(inspector.root(), "watched_networks")
                .expect("attach watched_networks inspect");
            Self { watchers, networks_stream, inspector }
        }
    }

    fn make_token() -> (Koid, fnp_properties::NetworkToken, zx::EventPair) {
        let (handle, peer) = zx::EventPair::create();
        let koid = handle.koid().expect("token koid");
        (koid, fnp_properties::NetworkToken { value: handle }, peer)
    }

    fn duplicate_token(token: &fnp_properties::NetworkToken) -> fnp_properties::NetworkToken {
        fnp_properties::NetworkToken {
            value: token
                .value
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate network token"),
        }
    }

    async fn expect_watch_properties(
        stream: &mut fnp_properties::NetworksRequestStream,
    ) -> (
        fnp_properties::NetworksWatchPropertiesRequest,
        fnp_properties::NetworksWatchPropertiesResponder,
    ) {
        match stream.next().await.expect("networks stream item").expect("networks stream ok") {
            fnp_properties::NetworksRequest::WatchProperties { payload, responder } => {
                (payload, responder)
            }
            other => panic!("unexpected Networks request: {other:?}"),
        }
    }

    async fn expect_watcher_watch(
        stream: &mut fnp_properties::PropertyWatcherRequestStream,
    ) -> fnp_properties::PropertyWatcherWatchResponder {
        match stream.next().await.expect("watcher stream item").expect("watcher stream ok") {
            fnp_properties::PropertyWatcherRequest::Watch { responder } => responder,
        }
    }

    struct MockWatcher {
        stream: fnp_properties::PropertyWatcherRequestStream,
        next_watch: fnp_properties::PropertyWatcherWatchResponder,
    }

    impl MockWatcher {
        /// Sends a `PropertyUpdate` on the pending hanging-get `Watch` call and
        /// waits for the watcher task to issue its next `Watch`.
        async fn send_update(mut self, socket_marks: Option<fnet::Marks>) -> Self {
            self.next_watch
                .send(Ok(&fnp_properties::PropertyUpdate { socket_marks, ..Default::default() }))
                .expect("send property update");
            self.next_watch = expect_watcher_watch(&mut self.stream).await;
            self
        }
    }

    /// Drives the server side of `WatchProperties` + initial snapshot `Watch`
    /// and waits for the steady-state hanging-get `Watch` request.
    async fn accept_watcher(
        stream: &mut fnp_properties::NetworksRequestStream,
        socket_marks: Option<fnet::Marks>,
    ) -> MockWatcher {
        let (payload, responder) = expect_watch_properties(stream).await;
        let mut watcher_stream = payload.watcher.expect("watcher present").into_stream();
        let initial_watch = expect_watcher_watch(&mut watcher_stream).await;
        responder.send(Ok(())).expect("send WatchProperties ok");
        initial_watch
            .send(Ok(&fnp_properties::PropertyUpdate { socket_marks, ..Default::default() }))
            .expect("send initial snapshot");
        let next_watch = expect_watcher_watch(&mut watcher_stream).await;
        MockWatcher { stream: watcher_stream, next_watch }
    }

    async fn assert_watch_error<C: ProtocolConnector<fnp_properties::NetworksMarker> + 'static>(
        watchers: &Arc<NetworkTokenWatchers<C>>,
        stream: &mut fnp_properties::NetworksRequestStream,
        token: fnp_properties::NetworkToken,
        watch_err: fnp_properties::WatchError,
        expected: fidl::Status,
    ) {
        let (res, ()) = join(watchers.get_or_watch(token), async {
            let (_payload, responder) = expect_watch_properties(stream).await;
            responder.send(Err(watch_err)).expect("send WatchError");
        })
        .await;
        assert_matches!(res, Err(status) if status == expected);
    }

    // Verifies that concurrent `get_or_watch` calls for the same KOID coalesce
    // onto one `WatchProperties` RPC while initialization is in flight, both on
    // failure (evicting the entry for retries) and on success.
    #[fuchsia::test]
    async fn concurrent_watch_coalesces_watch_properties() {
        let mut harness = WatchersHarness::new();

        // Concurrent in-flight failure -> both get NOT_FOUND and KOID is evicted.
        let (fail_koid, fail_token_1, _fail_peer) = make_token();
        let fail_token_2 = duplicate_token(&fail_token_1);
        let fail_token_retry = duplicate_token(&fail_token_1);

        let mut fut_1 = pin!(harness.watchers.get_or_watch(fail_token_1));
        let mut fut_2 = pin!(harness.watchers.get_or_watch(fail_token_2));
        assert_matches!(fut_1.as_mut().now_or_never(), None);
        assert_matches!(fut_2.as_mut().now_or_never(), None);

        let (payload, responder) = expect_watch_properties(&mut harness.networks_stream).await;
        assert_eq!(
            payload.network.as_ref().map(|t| t.value.koid().expect("koid")),
            Some(fail_koid)
        );
        responder
            .send(Err(fnp_properties::WatchError::InvalidNetworkToken))
            .expect("send InvalidNetworkToken");
        let (res_1, res_2) = join(fut_1, fut_2).await;
        assert_matches!(res_1, Err(fidl::Status::NOT_FOUND));
        assert_matches!(res_2, Err(fidl::Status::NOT_FOUND));

        assert_watch_error(
            &harness.watchers,
            &mut harness.networks_stream,
            fail_token_retry,
            fnp_properties::WatchError::InvalidNetworkToken,
            fidl::Status::NOT_FOUND,
        )
        .await;
        assert_data_tree!(harness.inspector, root: contains { watched_networks: {} });

        // Concurrent in-flight success -> single WatchProperties and shared marks.
        let (_koid, token_1, _peer) = make_token();
        let token_2 = duplicate_token(&token_1);
        let mut fut_1 = pin!(harness.watchers.get_or_watch(token_1));
        let mut fut_2 = pin!(harness.watchers.get_or_watch(token_2));
        assert_matches!(fut_1.as_mut().now_or_never(), None);
        assert_matches!(fut_2.as_mut().now_or_never(), None);

        let _mock_watcher = accept_watcher(
            &mut harness.networks_stream,
            Some(fnet::Marks { mark_1: Some(INITIAL_MARK_1), ..Default::default() }),
        )
        .await;
        let (res_1, res_2) = join(fut_1, fut_2).await;
        let expected = crate::SocketMarks {
            mark_1: OptionalUint32::Value(INITIAL_MARK_1),
            mark_2: OptionalUint32::Unset(fposix_socket::Empty),
        };
        assert_eq!(**res_1.expect("first ok").marks.lock().await, expected);
        assert_eq!(**res_2.expect("second ok").marks.lock().await, expected);

        assert_data_tree!(harness.inspector, root: contains {
            watched_networks: { "0": { mark_1: u64::from(INITIAL_MARK_1), has_mark: true } },
        });
        assert_matches!(harness.networks_stream.next().now_or_never(), None);
    }

    #[fuchsia::test]
    async fn errors_mapped_to_expected_status() {
        let failing_watchers = Arc::new(NetworkTokenWatchers::new(FailingNetworksConnector));
        let invalid_token =
            fnp_properties::NetworkToken { value: zx::NullableHandle::invalid().into() };
        // Invalid handle fails before calling `connect()` (which would return INTERNAL).
        assert_matches!(
            failing_watchers.get_or_watch(invalid_token).await,
            Err(fidl::Status::NOT_FOUND)
        );

        let (_koid, token, _peer) = make_token();
        assert_matches!(
            failing_watchers.get_or_watch(duplicate_token(&token)).await,
            Err(fidl::Status::INTERNAL)
        );

        let mut harness = WatchersHarness::new();

        // Unexpected WatchError maps to INTERNAL and evicts entry.
        assert_watch_error(
            &harness.watchers,
            &mut harness.networks_stream,
            duplicate_token(&token),
            fnp_properties::WatchError::NoProperties,
            fidl::Status::INTERNAL,
        )
        .await;

        // Initial Watch NetworkGone maps to NOT_FOUND and evicts entry.
        let (res, ()) = join(harness.watchers.get_or_watch(duplicate_token(&token)), async {
            let (payload, responder) = expect_watch_properties(&mut harness.networks_stream).await;
            let mut watcher_stream = payload.watcher.expect("watcher").into_stream();
            responder.send(Ok(())).expect("send WatchProperties ok");
            expect_watcher_watch(&mut watcher_stream)
                .await
                .send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                .expect("send NetworkGone");
        })
        .await;
        assert_matches!(res, Err(fidl::Status::NOT_FOUND));

        // Explicit epitaph during steady-state watch closes with INTERNAL and evicts entry.
        let (watched, mock_watcher) = join(
            harness.watchers.get_or_watch(duplicate_token(&token)),
            accept_watcher(
                &mut harness.networks_stream,
                Some(fnet::Marks { mark_1: Some(INITIAL_MARK_1), ..Default::default() }),
            ),
        )
        .await;
        let watched = watched.expect("watcher ready");
        mock_watcher.stream.control_handle().shutdown_with_epitaph(fidl::Status::ALREADY_EXISTS);
        drop(mock_watcher);
        assert_eq!(watched.closed.await, Ok(fidl::Status::INTERNAL));

        // Closed Networks channel maps to INTERNAL.
        drop(harness.networks_stream);
        assert_matches!(harness.watchers.get_or_watch(token).await, Err(fidl::Status::INTERNAL));
    }

    #[fuchsia::test]
    async fn property_updates_and_steady_state_close() {
        let mut harness = WatchersHarness::new();
        let (_koid, token, _peer) = make_token();

        // Initial unmarked snapshot produces default marks and Inspect `has_mark: false`.
        let (watched, mock_watcher) = join(
            harness.watchers.get_or_watch(token),
            accept_watcher(&mut harness.networks_stream, None),
        )
        .await;
        let watched = watched.expect("watcher ok");
        assert_eq!(**watched.marks.lock().await, crate::SocketMarks::default());
        assert_data_tree!(harness.inspector, root: contains {
            watched_networks: { "0": { has_mark: false } },
        });

        // Update sets both marks; a subsequent `socket_marks: None` delta preserves them.
        let mock_watcher = mock_watcher
            .send_update(Some(fnet::Marks {
                mark_1: Some(INITIAL_MARK_1),
                mark_2: Some(INITIAL_MARK_2),
                ..Default::default()
            }))
            .await;
        let updated_expected = crate::SocketMarks {
            mark_1: OptionalUint32::Value(INITIAL_MARK_1),
            mark_2: OptionalUint32::Value(INITIAL_MARK_2),
        };
        assert_eq!(**watched.marks.lock().await, updated_expected);
        assert_data_tree!(harness.inspector, root: contains {
            watched_networks: {
                "0": {
                    mark_1: u64::from(INITIAL_MARK_1),
                    mark_2: u64::from(INITIAL_MARK_2),
                    has_mark: true,
                },
            },
        });

        let mock_watcher = mock_watcher.send_update(None).await;
        assert_eq!(**watched.marks.lock().await, updated_expected);

        // Update with `Some(Marks::default())` clears marks without closing.
        let mock_watcher = mock_watcher.send_update(Some(fnet::Marks::default())).await;
        assert_eq!(**watched.marks.lock().await, crate::SocketMarks::default());
        assert_data_tree!(harness.inspector, root: contains {
            watched_networks: { "0": { has_mark: false } },
        });

        // Dropping the watcher closes with CANCELED and drops the Inspect node once `marks` drops.
        drop(mock_watcher);
        assert_eq!(watched.closed.await, Ok(fidl::Status::CANCELED));
        drop(watched.marks);
        assert_data_tree!(harness.inspector, root: contains { watched_networks: {} });

        // Watching another network after the first is evicted allocates the next monotonic ID.
        let (_next_koid, next_token, _next_peer) = make_token();
        let (next_watched, _next_mock_watcher) = join(
            harness.watchers.get_or_watch(next_token),
            accept_watcher(
                &mut harness.networks_stream,
                Some(fnet::Marks { mark_1: Some(INITIAL_MARK_1), ..Default::default() }),
            ),
        )
        .await;
        assert!(next_watched.is_ok());
        assert_data_tree!(harness.inspector, root: contains {
            watched_networks: { "1": { mark_1: u64::from(INITIAL_MARK_1), has_mark: true } },
        });
    }

    #[fuchsia::test]
    async fn dropping_watchers_cancels_active_tasks() {
        let mut watcher_stream = {
            let mut harness = WatchersHarness::new();
            let (_koid, token, _peer) = make_token();
            let (watched, mock_watcher) = join(
                harness.watchers.get_or_watch(token),
                accept_watcher(
                    &mut harness.networks_stream,
                    Some(fnet::Marks { mark_1: Some(INITIAL_MARK_1), ..Default::default() }),
                ),
            )
            .await;
            assert!(watched.is_ok());
            mock_watcher.stream
        };

        assert_matches!(watcher_stream.next().await, None);
    }
}
