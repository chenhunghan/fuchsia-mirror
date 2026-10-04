// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use anyhow::Context as _;
use fidl_fuchsia_net_policy_properties as fnp_properties;
use futures::FutureExt as _;
use log::{debug, error, info};

use crate::{SocketMarks, SocketProxy};

/// Manages the state of the active network's property watcher.
#[derive(Default)]
struct WatcherState {
    watcher: Option<fnp_properties::PropertyWatcherProxy>,
    next_watch: futures::future::OptionFuture<
        fidl::client::QueryResponseFut<
            Result<fnp_properties::PropertyUpdate, fnp_properties::PropertyWatcherError>,
        >,
    >,
}

impl WatcherState {
    /// Sets a new active property watcher and initiates a watch future.
    fn set(&mut self, watcher: fnp_properties::PropertyWatcherProxy) {
        self.next_watch = Some(watcher.watch()).into();
        self.watcher = Some(watcher);
    }

    /// Re-arms the watch future on the current active watcher.
    fn watch_again(&mut self) {
        self.next_watch = self.watcher.as_ref().map(|w| w.watch()).into();
    }

    /// Clears the active watcher state and resets the default marks.
    async fn reset(&mut self, proxy: &SocketProxy) {
        let had_watcher = self.watcher.take().is_some();
        self.next_watch = None.into();
        if had_watcher {
            info!("No default network observed");
        }
        proxy.set_marks(SocketMarks::default()).await;
    }
}

/// Continuously watches network properties from
/// `fuchsia.net.policy.properties.Networks`.
pub(crate) async fn watch_properties(proxy: Arc<SocketProxy>) {
    let networks_proxy = match fuchsia_component::client::connect_to_protocol::<
        fnp_properties::NetworksMarker,
    >() {
        Ok(p) => p,
        Err(e) => {
            debug!("failed to connect to fuchsia.net.policy.properties.Networks protocol: {e:?}");
            proxy.set_marks(SocketMarks::default()).await;
            return;
        }
    };

    watch_properties_loop(&proxy, &networks_proxy).await;
}

async fn watch_properties_loop(
    proxy: &Arc<SocketProxy>,
    networks_proxy: &fnp_properties::NetworksProxy,
) {
    info!("No default network observed");
    if let Err(e) = run_watch_properties_loop(proxy, networks_proxy).await {
        debug!("Networks property watcher loop ended: {e:?}");
    }
    proxy.set_marks(SocketMarks::default()).await;
}

/// Drives two coordinated hanging-get loops against `fuchsia.net.policy.properties/Networks`:
/// * `WatchDefault`: tracks changes to the active default network token (`NetworkToken`).
/// * `PropertyWatcher.Watch`: tracks socket mark updates on that active network.
///
/// If the default network is unset (`NoDefaultNetwork`), disconnected, or errors, the
/// default marks are reset so newly created sockets are left unmarked.
async fn run_watch_properties_loop(
    proxy: &Arc<SocketProxy>,
    networks_proxy: &fnp_properties::NetworksProxy,
) -> Result<(), anyhow::Error> {
    let mut next_default = networks_proxy.watch_default().fuse();
    let mut watcher_state = WatcherState::default();

    let create_watcher = |token: fnp_properties::NetworkToken| {
        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let request = fnp_properties::NetworksWatchPropertiesRequest {
            network: Some(token),
            properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
            watcher: Some(server_end),
            ..Default::default()
        };
        (networks_proxy.watch_properties(request), watcher)
    };

    loop {
        futures::select! {
            props_res = &mut watcher_state.next_watch => {
                match props_res.expect("next_watch should always be Some") {
                    Ok(Ok(updates)) => {
                        // If `socket_marks` is omitted (as Netcfg does for Fuchsia networks)
                        // or specified with a `None` mark, this is an unmarked network.
                        let marks = updates.socket_marks.map(SocketMarks::from).unwrap_or_default();
                        info!("New default network observed (mark={marks})");
                        proxy.set_marks(marks).await;
                        watcher_state.watch_again();
                    }
                    Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                    | Err(fidl::Error::ClientChannelClosed { .. }) => {
                        debug!("PropertyWatcher ended (network removed or watcher closed)");
                        watcher_state.reset(proxy).await;
                    }
                    err => {
                        error!("PropertyWatcher failed: {err:?}");
                        watcher_state.reset(proxy).await;
                    }
                }
            }

            response = next_default => {
                let response = match response {
                    Ok(r) => r,
                    Err(fidl::Error::ClientChannelClosed { .. }) => {
                        debug!("Networks.WatchDefault channel closed");
                        return Ok(());
                    }
                    Err(e) => return Err(e).context("WatchDefault RPC failed"),
                };
                match response {
                    fnp_properties::NetworksWatchDefaultResponse::Network(token) => {
                        let (watch_props_fut, watcher) = create_watcher(token);
                        match watch_props_fut.await {
                            Ok(Ok(())) => {
                                watcher_state.set(watcher);
                            }
                            Ok(Err(fnp_properties::WatchError::InvalidNetworkToken)) => {
                                debug!("WatchProperties: network token is no longer valid");
                                watcher_state.reset(proxy).await;
                            }
                            Err(fidl::Error::ClientChannelClosed { .. }) => {
                                debug!("Networks.WatchProperties channel closed");
                                watcher_state.reset(proxy).await;
                                return Ok(());
                            }
                            err => {
                                error!("WatchProperties failed: {err:?}");
                                watcher_state.reset(proxy).await;
                            }
                        }
                    }
                    fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(_) => {
                        watcher_state.reset(proxy).await;
                    }
                    fnp_properties::NetworksWatchDefaultResponse::__SourceBreaking { .. } => {
                        unreachable!("Networks.WatchDefault should not return __SourceBreaking");
                    }
                }
                next_default = networks_proxy.watch_default().fuse();
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl::EventPair;
    use fidl_fuchsia_net as fnet;
    use fidl_fuchsia_posix_socket as fposix_socket;
    use fidl_fuchsia_posix_socket::OptionalUint32;
    use fuchsia_async as fasync;
    use futures::StreamExt as _;

    const DEFAULT_MARK: u32 = 123;
    const UPDATED_MARK: u32 = 456;
    const MARK_1: u32 = 1;
    const MARK_2: u32 = 2;
    const TOKEN_A_MARK: u32 = 111;
    const TOKEN_B_MARK: u32 = 222;

    fn fidl_mark(mark_1: u32) -> fnet::Marks {
        fnet::Marks {
            mark_1: Some(mark_1),
            mark_2: None,
            __source_breaking: fidl::marker::SourceBreaking,
        }
    }

    fn fidl_dual_marks(mark_1: u32, mark_2: u32) -> fnet::Marks {
        fnet::Marks {
            mark_1: Some(mark_1),
            mark_2: Some(mark_2),
            __source_breaking: fidl::marker::SourceBreaking,
        }
    }

    fn expected_mark(mark_1: u32) -> SocketMarks {
        SocketMarks {
            mark_1: OptionalUint32::Value(mark_1),
            mark_2: OptionalUint32::Unset(fposix_socket::Empty),
        }
    }

    fn expected_dual_marks(mark_1: u32, mark_2: u32) -> SocketMarks {
        SocketMarks { mark_1: OptionalUint32::Value(mark_1), mark_2: OptionalUint32::Value(mark_2) }
    }

    async fn expect_watch_default(
        stream: &mut fnp_properties::NetworksRequestStream,
    ) -> fnp_properties::NetworksWatchDefaultResponder {
        match stream.next().await.expect("networks stream closed").expect("request error") {
            fnp_properties::NetworksRequest::WatchDefault { responder } => responder,
            req => panic!("unexpected request: {req:?}"),
        }
    }

    async fn expect_watch_properties(
        stream: &mut fnp_properties::NetworksRequestStream,
    ) -> (
        fnp_properties::NetworksWatchPropertiesRequest,
        fnp_properties::NetworksWatchPropertiesResponder,
    ) {
        match stream.next().await.expect("networks stream closed").expect("request error") {
            fnp_properties::NetworksRequest::WatchProperties { payload, responder } => {
                (payload, responder)
            }
            req => panic!("unexpected request: {req:?}"),
        }
    }

    async fn expect_watcher_watch(
        stream: &mut fnp_properties::PropertyWatcherRequestStream,
    ) -> fnp_properties::PropertyWatcherWatchResponder {
        let fnp_properties::PropertyWatcherRequest::Watch { responder } =
            stream.next().await.expect("watcher stream closed").expect("request error");
        responder
    }

    struct ActiveNetwork {
        next_default: fnp_properties::NetworksWatchDefaultResponder,
        watcher_stream: fnp_properties::PropertyWatcherRequestStream,
        watch_responder: Option<fnp_properties::PropertyWatcherWatchResponder>,
    }

    impl ActiveNetwork {
        async fn establish_with_default(
            networks_stream: &mut fnp_properties::NetworksRequestStream,
            default_responder: fnp_properties::NetworksWatchDefaultResponder,
            initial_marks: Option<fnet::Marks>,
        ) -> Self {
            let (token_handle, _peer) = EventPair::create();
            let token_koid = token_handle.koid().unwrap();

            default_responder
                .send(fnp_properties::NetworksWatchDefaultResponse::Network(
                    fnp_properties::NetworkToken { value: token_handle },
                ))
                .expect("send WatchDefault response");

            let (payload, watch_props_responder) = expect_watch_properties(networks_stream).await;
            assert_eq!(payload.network.as_ref().map(|t| t.value.koid().unwrap()), Some(token_koid));
            assert_eq!(payload.properties, Some(fnp_properties::PropertyInterest::SOCKET_MARKS));
            let mut watcher_stream =
                payload.watcher.expect("watcher server end present").into_stream();
            watch_props_responder.send(Ok(())).expect("send WatchProperties response");

            let watch_responder = expect_watcher_watch(&mut watcher_stream).await;
            watch_responder
                .send(Ok(&fnp_properties::PropertyUpdate {
                    socket_marks: initial_marks,
                    ..Default::default()
                }))
                .expect("send initial PropertyUpdate");
            let next_watch_responder = expect_watcher_watch(&mut watcher_stream).await;
            let next_default_responder = expect_watch_default(networks_stream).await;

            Self {
                next_default: next_default_responder,
                watcher_stream,
                watch_responder: Some(next_watch_responder),
            }
        }

        /// Switches the default network to a new network token with `initial_marks`.
        async fn switch_to(
            self,
            networks_stream: &mut fnp_properties::NetworksRequestStream,
            initial_marks: Option<fnet::Marks>,
        ) -> Self {
            Self::establish_with_default(networks_stream, self.next_default, initial_marks).await
        }

        /// Switches to an invalid network token to test error handling on WatchProperties.
        async fn switch_to_invalid_token(
            self,
            networks_stream: &mut fnp_properties::NetworksRequestStream,
        ) -> fnp_properties::NetworksWatchDefaultResponder {
            let (token_handle, _peer) = EventPair::create();
            let token_koid = token_handle.koid().unwrap();
            self.next_default
                .send(fnp_properties::NetworksWatchDefaultResponse::Network(
                    fnp_properties::NetworkToken { value: token_handle },
                ))
                .expect("send WatchDefault with invalid token");

            let (payload, watch_props_responder) = expect_watch_properties(networks_stream).await;
            assert_eq!(payload.network.as_ref().map(|t| t.value.koid().unwrap()), Some(token_koid));
            watch_props_responder
                .send(Err(fnp_properties::WatchError::InvalidNetworkToken))
                .expect("send InvalidNetworkToken");

            expect_watch_default(networks_stream).await
        }

        /// Sends a property update on the active watcher and awaits the re-armed watch.
        async fn send_marks(&mut self, marks: Option<fnet::Marks>) {
            let responder = self.watch_responder.take().expect("watch_responder present");
            responder
                .send(Ok(&fnp_properties::PropertyUpdate {
                    socket_marks: marks,
                    ..Default::default()
                }))
                .expect("send PropertyUpdate");
            self.watch_responder = Some(expect_watcher_watch(&mut self.watcher_stream).await);
        }

        /// Sends an error on the active watcher.
        fn send_error(mut self, error: fnp_properties::PropertyWatcherError) {
            let responder = self.watch_responder.take().expect("watch_responder present");
            responder.send(Err(error)).expect("send PropertyWatcher error");
        }
    }

    struct TestHarness {
        proxy: Arc<SocketProxy>,
        stream: fnp_properties::NetworksRequestStream,
        _scope: fasync::Scope,
        loop_task: fasync::JoinHandle<()>,
    }

    impl TestHarness {
        fn new() -> Self {
            let (networks_proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
            let proxy = Arc::new(SocketProxy::new());
            let scope = fasync::Scope::new();
            let loop_task = scope.spawn({
                let proxy = Arc::clone(&proxy);
                async move {
                    run_watch_properties_loop(&proxy, &networks_proxy).await.expect("loop failed");
                }
            });
            Self { proxy, stream, _scope: scope, loop_task }
        }

        fn new_single_run() -> Self {
            let (networks_proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
            let proxy = Arc::new(SocketProxy::new());
            let scope = fasync::Scope::new();
            let loop_task = scope.spawn({
                let proxy = Arc::clone(&proxy);
                async move {
                    watch_properties_loop(&proxy, &networks_proxy).await;
                }
            });
            Self { proxy, stream, _scope: scope, loop_task }
        }

        async fn current_marks(&self) -> SocketMarks {
            **self.proxy.marks.lock().await
        }

        async fn establish_network(&mut self, initial_marks: Option<fnet::Marks>) -> ActiveNetwork {
            let default_responder = expect_watch_default(&mut self.stream).await;
            ActiveNetwork::establish_with_default(
                &mut self.stream,
                default_responder,
                initial_marks,
            )
            .await
        }

        async fn finish(self) {
            drop(self.stream);
            self.loop_task.await;
        }

        async fn finish_and_assert_marks(self, expected: SocketMarks) {
            let proxy = Arc::clone(&self.proxy);
            self.finish().await;
            assert_eq!(**proxy.marks.lock().await, expected);
        }
    }

    #[fuchsia::test]
    async fn test_watch_default_no_default_network() {
        let mut harness = TestHarness::new();

        let default_responder = expect_watch_default(&mut harness.stream).await;
        default_responder
            .send(fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(
                fnp_properties::Empty,
            ))
            .expect("send NoDefaultNetwork");

        let next_default = expect_watch_default(&mut harness.stream).await;
        assert_eq!(harness.current_marks().await, SocketMarks::default());

        drop(next_default);
        harness.finish().await;
    }

    #[fuchsia::test]
    async fn test_property_updates() {
        let mut harness = TestHarness::new();
        let mut network = harness.establish_network(Some(fidl_dual_marks(MARK_1, MARK_2))).await;
        assert_eq!(harness.current_marks().await, expected_dual_marks(MARK_1, MARK_2));

        // Update marks on the same watcher to verify re-arming.
        network.send_marks(Some(fidl_mark(UPDATED_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(UPDATED_MARK));

        // Clear marks with socket_marks: None (Netcfg convention for unmarked networks).
        network.send_marks(None).await;
        assert_eq!(harness.current_marks().await, SocketMarks::default());

        drop(network);
        harness.finish().await;
    }

    #[fuchsia::test]
    async fn test_default_network_switch() {
        let mut harness = TestHarness::new();
        let network_a = harness.establish_network(Some(fidl_mark(TOKEN_A_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(TOKEN_A_MARK));

        let network_b =
            network_a.switch_to(&mut harness.stream, Some(fidl_mark(TOKEN_B_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(TOKEN_B_MARK));

        drop(network_b);
        harness.finish().await;
    }

    #[fuchsia::test]
    async fn test_watch_network_gone_resets_marks() {
        let mut harness = TestHarness::new();
        let network = harness.establish_network(Some(fidl_mark(DEFAULT_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(DEFAULT_MARK));

        // Report NetworkGone when network is removed.
        network.send_error(fnp_properties::PropertyWatcherError::NetworkGone);
        harness.finish_and_assert_marks(SocketMarks::default()).await;
    }

    #[fuchsia::test]
    async fn test_watch_properties_invalid_token_resets_marks() {
        let mut harness = TestHarness::new();
        let network = harness.establish_network(Some(fidl_mark(DEFAULT_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(DEFAULT_MARK));

        let next_default = network.switch_to_invalid_token(&mut harness.stream).await;
        assert_eq!(harness.current_marks().await, SocketMarks::default());

        drop(next_default);
        harness.finish().await;
    }

    #[fuchsia::test]
    async fn test_watch_default_channel_closed() {
        let mut harness = TestHarness::new_single_run();
        let network = harness.establish_network(Some(fidl_mark(DEFAULT_MARK))).await;
        assert_eq!(harness.current_marks().await, expected_mark(DEFAULT_MARK));

        drop(network);
        harness.finish_and_assert_marks(SocketMarks::default()).await;
    }
}
