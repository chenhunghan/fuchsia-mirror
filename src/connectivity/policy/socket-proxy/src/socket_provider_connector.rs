// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implements `fuchsia.net.policy.socketproxy.SocketProviderConnector`.

use anyhow::{Context as _, Error};
use fidl::endpoints::{RequestStream as _, ServerEnd};
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy;
use fidl_fuchsia_posix_socket as fposix_socket;
use fuchsia_inspect_derive::Inspect;
use futures::{FutureExt as _, StreamExt as _, TryStreamExt as _};
use std::sync::Arc;

use crate::network_token_watcher::NetworkTokenWatchers;
use crate::socket_provider::{NamespaceConnector, ProtocolConnector, SocketProvider};

/// Serves `fuchsia.net.policy.socketproxy.SocketProviderConnector`, vending
/// configured `fuchsia.posix.socket.Provider` connections.
#[derive(Clone, Inspect)]
pub(crate) struct SocketProviderConnector<C = NamespaceConnector> {
    default_provider: SocketProvider<C>,
    #[inspect(forward)]
    token_watchers: Arc<NetworkTokenWatchers<C>>,
}

impl SocketProviderConnector {
    pub(crate) fn new(default_provider: SocketProvider) -> Self {
        Self { default_provider, token_watchers: Arc::new(NetworkTokenWatchers::default()) }
    }
}

impl<C> SocketProviderConnector<C>
where
    C: ProtocolConnector<fposix_socket::ProviderMarker>
        + ProtocolConnector<fnp_properties::NetworksMarker>
        + Clone
        + 'static,
{
    #[cfg(test)]
    fn new_with_connector(default_provider: SocketProvider<C>, connector: C) -> Self {
        Self { default_provider, token_watchers: Arc::new(NetworkTokenWatchers::new(connector)) }
    }

    /// Run an instance of `fuchsia.net.policy.socketproxy.SocketProviderConnector`.
    pub(crate) async fn run(
        &self,
        stream: fnp_socketproxy::SocketProviderConnectorRequestStream,
    ) -> Result<(), Error> {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each_concurrent(None, |request| async move {
                match request {
                    fnp_socketproxy::SocketProviderConnectorRequest::Connect {
                        options,
                        provider,
                        ..
                    } => {
                        if let Err(e) = self.handle_connect(options, provider).await {
                            log::warn!("Error serving configured socket Provider: {e:?}");
                        }
                    }
                    fnp_socketproxy::SocketProviderConnectorRequest::_UnknownMethod {
                        ordinal,
                        ..
                    } => {
                        log::warn!(
                            "Received unknown method with ordinal {ordinal} on \
                            SocketProviderConnector"
                        );
                    }
                }
                Ok(())
            })
            .await
    }

    /// Serves `provider` with the marks of the network token in `options`, or
    /// of the default network when no token is given.
    async fn handle_connect(
        &self,
        options: fnp_socketproxy::SocketProviderOptions,
        provider: ServerEnd<fposix_socket::ProviderMarker>,
    ) -> Result<(), Error> {
        let fnp_socketproxy::SocketProviderOptions { network_token, __source_breaking } = options;
        let Some(token) = network_token else {
            return self.default_provider.run(provider.into_stream()).await;
        };

        let watched = match self.token_watchers.get_or_watch(token).await {
            Ok(watched) => watched,
            Err(status) => {
                if let Err(e) = provider.close_with_epitaph(status) {
                    log::debug!("Failed to close provider with epitaph {status}: {e:?}");
                }
                return Ok(());
            }
        };

        let pinned_provider = self.default_provider.with_marks(watched.marks);
        let provider_stream = provider.into_stream();
        let control_handle = provider_stream.control_handle();

        futures::select_biased! {
            epitaph_res = watched.closed => {
                let status = epitaph_res.unwrap_or(fidl::Status::CANCELED);
                control_handle.shutdown_with_epitaph(status);
                Ok(())
            }
            res = pinned_provider.run(provider_stream).fuse() => res,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_net::MarkDomain;
    use fidl_fuchsia_posix_socket::OptionalUint32;
    use fuchsia_async as fasync;
    use fuchsia_inspect_derive::IValue;
    use futures::lock::Mutex;

    const DEFAULT_MARK_1: u32 = 7;
    const UPDATED_DEFAULT_MARK_1: u32 = 9;

    fn spawn_mock_inner_provider(scope: &fasync::ScopeHandle) -> fposix_socket::ProviderProxy {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fposix_socket::ProviderMarker>();
        let child_scope = scope.clone();
        let _: fasync::JoinHandle<()> = scope.spawn(async move {
            while let Some(req) = stream.try_next().await.expect("inner provider request") {
                match req {
                    fposix_socket::ProviderRequest::StreamSocketWithOptions {
                        opts,
                        responder,
                        ..
                    } => {
                        let marks = crate::SocketMarks::from(opts.marks.unwrap_or_default());
                        let (socket_client, mut socket_stream) =
                            fidl::endpoints::create_request_stream::<
                                fposix_socket::StreamSocketMarker,
                            >();
                        responder.send(Ok(socket_client)).expect("send stream socket");
                        let _: fasync::JoinHandle<()> = child_scope.spawn(async move {
                            while let Some(sock_req) =
                                socket_stream.try_next().await.expect("stream socket request")
                            {
                                match sock_req {
                                    fposix_socket::StreamSocketRequest::GetMark {
                                        domain,
                                        responder,
                                    } => {
                                        let mark = match domain {
                                            MarkDomain::Mark1 => marks.mark_1,
                                            MarkDomain::Mark2 => marks.mark_2,
                                        };
                                        responder.send(Ok(&mark)).expect("send mark");
                                    }
                                    other => panic!("unexpected StreamSocketRequest: {other:?}"),
                                }
                            }
                        });
                    }
                    other => panic!("unexpected ProviderRequest: {other:?}"),
                }
            }
        });
        proxy
    }

    async fn create_socket_marks(provider: &fposix_socket::ProviderProxy) -> crate::SocketMarks {
        let socket = provider
            .stream_socket(fposix_socket::Domain::Ipv4, fposix_socket::StreamSocketProtocol::Tcp)
            .await
            .expect("stream_socket fidl")
            .expect("stream_socket ok")
            .into_proxy();
        let mark_1 =
            socket.get_mark(MarkDomain::Mark1).await.expect("get_mark fidl").expect("get_mark ok");
        let mark_2 =
            socket.get_mark(MarkDomain::Mark2).await.expect("get_mark fidl").expect("get_mark ok");
        crate::SocketMarks { mark_1, mark_2 }
    }

    #[derive(Clone)]
    struct MockConnector {
        scope: fasync::ScopeHandle,
    }

    impl ProtocolConnector<fposix_socket::ProviderMarker> for MockConnector {
        fn connect(&self) -> Result<fposix_socket::ProviderProxy, Error> {
            Ok(spawn_mock_inner_provider(&self.scope))
        }
    }

    impl ProtocolConnector<fnp_properties::NetworksMarker> for MockConnector {
        fn connect(&self) -> Result<fnp_properties::NetworksProxy, Error> {
            unreachable!("connect_without_token does not connect to Networks")
        }
    }

    struct ConnectorHarness {
        default_marks: Arc<Mutex<IValue<crate::SocketMarks>>>,
        connector: fnp_socketproxy::SocketProviderConnectorProxy,
        _scope: fasync::Scope,
    }

    impl ConnectorHarness {
        fn new() -> Self {
            let scope = fasync::Scope::new();
            let default_marks = Arc::new(Mutex::new(IValue::new(crate::SocketMarks {
                mark_1: OptionalUint32::Value(DEFAULT_MARK_1),
                mark_2: OptionalUint32::Unset(fposix_socket::Empty),
            })));
            let mock_connector = MockConnector { scope: scope.to_handle() };
            let socket_provider = SocketProvider::new_with_connector(
                Arc::clone(&default_marks),
                mock_connector.clone(),
            );
            let connector_server =
                SocketProviderConnector::new_with_connector(socket_provider, mock_connector);
            let (connector, connector_stream) = fidl::endpoints::create_proxy_and_stream::<
                fnp_socketproxy::SocketProviderConnectorMarker,
            >();
            let _: fasync::JoinHandle<()> = scope.spawn(async move {
                connector_server.run(connector_stream).await.expect("run connector");
            });
            Self { default_marks, connector, _scope: scope }
        }

        fn connect_provider(
            &self,
            network_token: Option<fnp_properties::NetworkToken>,
        ) -> fposix_socket::ProviderProxy {
            let (provider, server_end) =
                fidl::endpoints::create_proxy::<fposix_socket::ProviderMarker>();
            self.connector
                .connect(
                    fnp_socketproxy::SocketProviderOptions { network_token, ..Default::default() },
                    server_end,
                )
                .expect("connect provider");
            provider
        }
    }

    #[fuchsia::test]
    async fn connect_without_token_follows_default_network() {
        let harness = ConnectorHarness::new();
        let provider = harness.connect_provider(None);
        assert_eq!(
            create_socket_marks(&provider).await,
            crate::SocketMarks {
                mark_1: OptionalUint32::Value(DEFAULT_MARK_1),
                mark_2: OptionalUint32::Unset(fposix_socket::Empty),
            }
        );

        harness.default_marks.lock().await.iset(crate::SocketMarks {
            mark_1: OptionalUint32::Value(UPDATED_DEFAULT_MARK_1),
            mark_2: OptionalUint32::Unset(fposix_socket::Empty),
        });
        assert_eq!(
            create_socket_marks(&provider).await,
            crate::SocketMarks {
                mark_1: OptionalUint32::Value(UPDATED_DEFAULT_MARK_1),
                mark_2: OptionalUint32::Unset(fposix_socket::Empty),
            }
        );
    }
}
