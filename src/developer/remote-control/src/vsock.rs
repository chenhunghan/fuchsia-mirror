// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fdomain::serve_fdomain_connection;
use anyhow::Result;
use circuit::multi_stream::multi_stream_node_connection_to_async;
use fidl_fuchsia_vsock as vsock;
use fuchsia_async as fasync;
use futures::future::ready;
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt, pin_mut};
use overnet_core::Router;
use remote_control::RemoteControlService;
use std::rc::{Rc, Weak as WeakRc};
use std::sync::Weak;
use std::time::Duration;

const FDOMAIN_VSOCK_PORT: u32 = 203;
const OVERNET_VSOCK_PORT: u32 = 202;
const IDENTIFY_VSOCK_PORT: u32 = 201;

/// Default delay before reconnecting to the VSOCK connector upon disconnect or error.
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Controls the reconnect backoff strategy.
/// In production, uses `Duration` with a timer.
/// In unit tests, uses `Immediate` so tests are 100% deterministic with zero timers, sleeps, or waits.
#[derive(Clone, Copy, Debug)]
pub enum ReconnectBackoff {
    Duration(Duration),
    #[cfg(test)]
    Immediate,
}

impl ReconnectBackoff {
    async fn wait(&self) {
        match self {
            Self::Duration(d) => fasync::Timer::new(*d).await,
            #[cfg(test)]
            Self::Immediate => {}
        }
    }
}

/// Checks if an error indicates that VSOCK is permanently unrouted or unsupported on this device.
fn is_unsupported_error(err: &anyhow::Error) -> bool {
    if let Some(fidl_err) = err.downcast_ref::<fidl::Error>() {
        match fidl_err {
            fidl::Error::ClientChannelClosed { epitaph, .. } => {
                *epitaph == fidl::Status::NOT_FOUND
                    || *epitaph == fidl::Status::NOT_SUPPORTED
                    || *epitaph == fidl::Status::UNAVAILABLE
            }
            _ => false,
        }
    } else if let Some(status) = err.downcast_ref::<fidl::Status>() {
        *status == fidl::Status::NOT_FOUND
            || *status == fidl::Status::NOT_SUPPORTED
            || *status == fidl::Status::UNAVAILABLE
    } else if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        io_err.kind() == std::io::ErrorKind::NotFound
    } else {
        false
    }
}

pub async fn run_vsocks(router: Weak<Router>, service: WeakRc<RemoteControlService>) -> Result<()> {
    run_vsocks_internal(
        router,
        service,
        ReconnectBackoff::Duration(DEFAULT_RECONNECT_DELAY),
        || {
            fuchsia_component::client::connect_to_protocol::<vsock::ConnectorMarker>()
                .map_err(Into::into)
        },
    )
    .await
}

async fn run_vsocks_internal<F>(
    router: Weak<Router>,
    service: WeakRc<RemoteControlService>,
    reconnect_backoff: ReconnectBackoff,
    mut connector_factory: F,
) -> Result<()>
where
    F: FnMut() -> Result<vsock::ConnectorProxy>,
{
    loop {
        if router.upgrade().is_none() || service.upgrade().is_none() {
            log::info!("Router or service dropped; exiting VSOCK supervision loop");
            return Ok(());
        }

        let connector = match connector_factory() {
            Ok(c) => c,
            Err(e) => {
                if is_unsupported_error(&e) {
                    log::info!(
                        "VSOCK connector is not available on this system; stopping VSOCK service"
                    );
                    return Ok(());
                }
                log::info!(e:?; "Failed to connect to VSOCK connector; retrying");
                reconnect_backoff.wait().await;
                continue;
            }
        };

        match serve_vsocks_session(&connector, &router, &service).await {
            Ok(()) => {
                log::info!("VSOCK session ended normally");
                return Ok(());
            }
            Err(e) => {
                if is_unsupported_error(&e) {
                    log::info!(e:?; "VSOCK connector is not supported on this system; stopping VSOCK service");
                    return Ok(());
                }
                log::info!(e:?; "VSOCK session ended; reconnecting");
                reconnect_backoff.wait().await;
            }
        }
    }
}

async fn serve_vsocks_session(
    connector: &vsock::ConnectorProxy,
    router: &Weak<Router>,
    service: &WeakRc<RemoteControlService>,
) -> Result<()> {
    let (client, overnet_requests) = fidl::endpoints::create_request_stream();
    connector.listen(OVERNET_VSOCK_PORT, client).await?.map_err(fidl::Status::err_from_raw)?;

    let (client, fdomain_requests) = fidl::endpoints::create_request_stream();
    connector.listen(FDOMAIN_VSOCK_PORT, client).await?.map_err(fidl::Status::err_from_raw)?;

    enum TaggedRequest {
        Overnet(vsock::AcceptorRequest),
        FDomain(vsock::AcceptorRequest),
        OvernetClosed,
        FDomainClosed,
    }

    let overnet_stream = overnet_requests
        .map(|r| r.map(TaggedRequest::Overnet))
        .chain(futures::stream::once(ready(Ok(TaggedRequest::OvernetClosed))));

    let fdomain_stream = fdomain_requests
        .map(|r| r.map(TaggedRequest::FDomain))
        .chain(futures::stream::once(ready(Ok(TaggedRequest::FDomainClosed))));

    let requests = futures::stream::select(overnet_stream, fdomain_stream);
    pin_mut!(requests);

    while let Some(request) = requests.next().await {
        match request? {
            TaggedRequest::OvernetClosed => {
                return Err(anyhow::anyhow!("Overnet VSOCK port listener closed"));
            }
            TaggedRequest::FDomainClosed => {
                return Err(anyhow::anyhow!("FDomain VSOCK port listener closed"));
            }
            TaggedRequest::Overnet(vsock::AcceptorRequest::Accept { addr, responder }) => {
                log::info!(addr:? = addr; "Accepted Overnet VSOCK connection");

                let (client, con) = fidl::endpoints::create_endpoints();
                let (data, socket) = fidl::Socket::create_stream();
                let socket = fuchsia_async::Socket::from_socket(socket);
                let (mut reader, mut writer) = socket.split();
                let (err_sender, mut err_receiver) = futures::channel::mpsc::unbounded();

                let scope = fasync::Scope::new();
                scope.spawn(async move {
                    while let Some(error) = err_receiver.next().await {
                        log::debug!(
                            error:? = error;
                            "Stream error for VSOCK link"
                        )
                    }
                });

                let Some(router) = router.upgrade() else { return Ok(()) };

                scope.spawn(async move {
                    let _client = client;

                    if let Err(error) = multi_stream_node_connection_to_async(
                        router.circuit_node(),
                        &mut reader,
                        &mut writer,
                        true,
                        circuit::Quality::LOCAL_SOCKET,
                        err_sender,
                        format!("VSOCK {addr:?}"),
                    )
                    .await
                    {
                        log::info!(
                            addr:? = addr,
                            error:? = error;
                            "VSOCK link terminated",
                        );
                    }
                });

                scope.detach();

                if let Err(e) = responder.send(Some(vsock::ConnectionTransport { data, con })) {
                    log::warn!(e:?; "Failed to send Overnet accept response");
                }
            }
            TaggedRequest::FDomain(vsock::AcceptorRequest::Accept { addr, responder }) => {
                debug_assert!(addr.local_port == FDOMAIN_VSOCK_PORT);

                if service.upgrade().is_none() {
                    return Ok(());
                }

                let (client, con) = fidl::endpoints::create_endpoints();
                let (data, socket) = fidl::Socket::create_stream();
                let socket = fuchsia_async::Socket::from_socket(socket);
                fasync::Task::local({
                    let service = service.clone();
                    async move {
                        let _client = client;

                        serve_fdomain_connection(service, socket).await;
                    }
                })
                .detach();

                if let Err(e) = responder.send(Some(vsock::ConnectionTransport { data, con })) {
                    log::warn!(e:?; "Failed to send FDomain accept response");
                }
            }
        }
    }
    Err(anyhow::anyhow!("VSOCK request stream ended unexpectedly"))
}

pub async fn run_identify_vsock(service: Rc<RemoteControlService>) -> Result<()> {
    let weak_service = Rc::downgrade(&service);
    let _service = service;
    run_identify_vsock_internal(
        weak_service,
        ReconnectBackoff::Duration(DEFAULT_RECONNECT_DELAY),
        || {
            fuchsia_component::client::connect_to_protocol::<vsock::ConnectorMarker>()
                .map_err(Into::into)
        },
    )
    .await
}

async fn run_identify_vsock_internal<F>(
    service: WeakRc<RemoteControlService>,
    reconnect_backoff: ReconnectBackoff,
    mut connector_factory: F,
) -> Result<()>
where
    F: FnMut() -> Result<vsock::ConnectorProxy>,
{
    loop {
        let Some(service) = service.upgrade() else {
            log::info!("Service dropped; exiting VSOCK identify supervision loop");
            return Ok(());
        };

        let connector = match connector_factory() {
            Ok(c) => c,
            Err(e) => {
                if is_unsupported_error(&e) {
                    log::info!(
                        "VSOCK connector is not available on this system; stopping identify service"
                    );
                    return Ok(());
                }
                log::info!(e:?; "Failed to connect to VSOCK connector for identify; retrying");
                reconnect_backoff.wait().await;
                continue;
            }
        };

        match serve_identify_vsock_session(&connector, &service).await {
            Ok(()) => {
                log::info!("VSOCK identify session ended normally");
                return Ok(());
            }
            Err(e) => {
                if is_unsupported_error(&e) {
                    log::info!(e:?; "VSOCK connector is not supported on this system; stopping identify service");
                    return Ok(());
                }
                log::info!(e:?; "VSOCK identify session ended; reconnecting");
                reconnect_backoff.wait().await;
            }
        }
    }
}

async fn serve_identify_vsock_session(
    connector: &vsock::ConnectorProxy,
    service: &Rc<RemoteControlService>,
) -> Result<()> {
    let (client, mut requests) = fidl::endpoints::create_request_stream();
    connector.listen(IDENTIFY_VSOCK_PORT, client).await?.map_err(fidl::Status::err_from_raw)?;

    while let Some(request) = requests.next().await {
        let vsock::AcceptorRequest::Accept { addr, responder } = request?;

        log::info!(addr:? = addr; "Accepted VSOCK identify connection");

        let (_client, con) = fidl::endpoints::create_endpoints();
        let (data, socket) = fidl::Socket::create_stream();
        let socket = fuchsia_async::Socket::from_socket(socket);
        let (_reader, mut writer) = socket.split();
        if let Err(e) = responder.send(Some(vsock::ConnectionTransport { data, con })) {
            log::warn!(e:?; "Failed to send identify accept response");
            continue;
        }

        let header = fidl::encoding::TransactionHeader::new(
            0,
            0x6035e1ab368deee1,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let identity_result = service.get_host_identity().await;

        let buf = fidl_message::encode_response_result(header, identity_result)?;
        if let Err(e) = writer.write_all(&buf).await {
            log::warn!(e:?; "Failed to write host identity to VSOCK socket");
        }
    }
    Err(anyhow::anyhow!("VSOCK identify request stream closed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_hardware_vsock as fhardware_vsock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn expect_listen(
        stream: &mut vsock::ConnectorRequestStream,
    ) -> (u32, fidl::endpoints::ClientEnd<vsock::AcceptorMarker>, vsock::ConnectorListenResponder)
    {
        match stream.next().await.expect("request").expect("ok") {
            vsock::ConnectorRequest::Listen { local_port, acceptor, responder } => {
                (local_port, acceptor, responder)
            }
            other => panic!("expected Listen, got {other:?}"),
        }
    }

    #[fuchsia::test]
    async fn test_run_vsocks_reconnects_on_channel_closure() {
        let router = overnet_core::Router::new(None).expect("failed to create router");
        let weak_router = Arc::downgrade(&router);
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let reconnect_count = Arc::new(AtomicUsize::new(0));
        let reconnect_count_clone = Arc::clone(&reconnect_count);

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();

        let connector_factory = move || {
            let count = reconnect_count_clone.fetch_add(1, Ordering::SeqCst);
            let (proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<vsock::ConnectorMarker>();
            let _ = sender.unbounded_send((count, stream));
            Ok(proxy)
        };

        let supervision_task = fasync::Task::local(async move {
            run_vsocks_internal(
                weak_router,
                weak_service,
                ReconnectBackoff::Immediate,
                connector_factory,
            )
            .await
        });

        // First session:
        let (count0, mut stream0) = receiver.next().await.expect("received stream 0");
        assert_eq!(count0, 0);

        let (port1, acceptor1, resp1) = expect_listen(&mut stream0).await;
        assert_eq!(port1, OVERNET_VSOCK_PORT);
        resp1.send(Ok(())).expect("sent ok");

        let (port2, acceptor2, resp2) = expect_listen(&mut stream0).await;
        assert_eq!(port2, FDOMAIN_VSOCK_PORT);
        resp2.send(Ok(())).expect("sent ok");

        // Simulate vsock_service crashing/restarting:
        drop(acceptor1);
        drop(acceptor2);
        drop(stream0);

        // Second session: supervision loop should reconnect!
        let (count1, mut stream1) = receiver.next().await.expect("received stream 1");
        assert_eq!(count1, 1);

        let (port1_re, _acceptor1_re, resp1_re) = expect_listen(&mut stream1).await;
        assert_eq!(port1_re, OVERNET_VSOCK_PORT);
        resp1_re.send(Ok(())).expect("sent ok");

        let (port2_re, _acceptor2_re, resp2_re) = expect_listen(&mut stream1).await;
        assert_eq!(port2_re, FDOMAIN_VSOCK_PORT);
        resp2_re.send(Ok(())).expect("sent ok");

        // Drop router and service to signal shutdown to the supervision loop
        drop(router);
        drop(service);

        // Drop acceptors and stream to finish session 2
        drop(_acceptor1_re);
        drop(_acceptor2_re);
        drop(stream1);

        let result = supervision_task.await;
        assert!(result.is_ok(), "supervision loop exited with: {result:?}");
        assert_eq!(reconnect_count.load(Ordering::SeqCst), 2);
    }

    #[fuchsia::test]
    async fn test_run_vsocks_unsupported_exits_cleanly() {
        let router = overnet_core::Router::new(None).expect("failed to create router");
        let weak_router = Arc::downgrade(&router);
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let connector_factory = || {
            let (proxy, server_end) = fidl::endpoints::create_proxy::<vsock::ConnectorMarker>();
            server_end.close_with_epitaph(fidl::Status::NOT_FOUND).expect("closed epitaph");
            Ok(proxy)
        };

        let result = run_vsocks_internal(
            weak_router,
            weak_service,
            ReconnectBackoff::Immediate,
            connector_factory,
        )
        .await;

        assert!(result.is_ok(), "expected clean Ok exit for unsupported vsock, got: {result:?}");
    }

    #[fuchsia::test]
    async fn test_run_vsocks_factory_error_reconnects() {
        let router = overnet_core::Router::new(None).expect("failed to create router");
        let weak_router = Arc::downgrade(&router);
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = Arc::clone(&attempts);

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();

        let connector_factory = move || {
            let attempt = attempts_clone.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                // First attempt fails transiently
                Err(anyhow::anyhow!("transient connection error"))
            } else {
                // Second attempt succeeds
                let (proxy, stream) =
                    fidl::endpoints::create_proxy_and_stream::<vsock::ConnectorMarker>();
                let _ = sender.unbounded_send(stream);
                Ok(proxy)
            }
        };

        let supervision_task = fasync::Task::local(async move {
            run_vsocks_internal(
                weak_router,
                weak_service,
                ReconnectBackoff::Immediate,
                connector_factory,
            )
            .await
        });

        let mut stream = receiver.next().await.expect("received stream");
        let (port1, _a1, resp1) = expect_listen(&mut stream).await;
        assert_eq!(port1, OVERNET_VSOCK_PORT);
        resp1.send(Ok(())).expect("sent ok");

        let (port2, _a2, resp2) = expect_listen(&mut stream).await;
        assert_eq!(port2, FDOMAIN_VSOCK_PORT);
        resp2.send(Ok(())).expect("sent ok");

        drop(router);
        drop(service);
        drop(_a1);
        drop(_a2);
        drop(stream);

        let result = supervision_task.await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[fuchsia::test]
    async fn test_run_identify_vsock_reconnects_on_channel_closure() {
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let reconnect_count = Arc::new(AtomicUsize::new(0));
        let reconnect_count_clone = Arc::clone(&reconnect_count);

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();

        let connector_factory = move || {
            let count = reconnect_count_clone.fetch_add(1, Ordering::SeqCst);
            let (proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<vsock::ConnectorMarker>();
            let _ = sender.unbounded_send((count, stream));
            Ok(proxy)
        };

        let supervision_task = fasync::Task::local(async move {
            run_identify_vsock_internal(
                weak_service,
                ReconnectBackoff::Immediate,
                connector_factory,
            )
            .await
        });

        // First session:
        let (count0, mut stream0) = receiver.next().await.expect("received stream 0");
        assert_eq!(count0, 0);

        let (port, acceptor, resp) = expect_listen(&mut stream0).await;
        assert_eq!(port, IDENTIFY_VSOCK_PORT);
        resp.send(Ok(())).expect("sent ok");

        // Simulate vsock_service crashing/restarting:
        drop(acceptor);
        drop(stream0);

        // Second session:
        let (count1, mut stream1) = receiver.next().await.expect("received stream 1");
        assert_eq!(count1, 1);

        let (port_re, _acceptor_re, resp_re) = expect_listen(&mut stream1).await;
        assert_eq!(port_re, IDENTIFY_VSOCK_PORT);
        resp_re.send(Ok(())).expect("sent ok");

        drop(service);
        drop(_acceptor_re);
        drop(stream1);

        let result = supervision_task.await;
        assert!(result.is_ok(), "supervision loop exited with: {result:?}");
        assert_eq!(reconnect_count.load(Ordering::SeqCst), 2);
    }

    #[fuchsia::test]
    async fn test_run_identify_vsock_serves_request() {
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<vsock::ConnectorMarker>();
        let mut factory_called = false;
        let connector_factory = move || {
            if !factory_called {
                factory_called = true;
                Ok(proxy.clone())
            } else {
                Err(anyhow::anyhow!(fidl::Status::NOT_FOUND))
            }
        };

        let supervision_task = fasync::Task::local(async move {
            run_identify_vsock_internal(
                weak_service,
                ReconnectBackoff::Immediate,
                connector_factory,
            )
            .await
        });

        let (port, acceptor, resp) = expect_listen(&mut stream).await;
        assert_eq!(port, IDENTIFY_VSOCK_PORT);
        resp.send(Ok(())).expect("sent ok");

        let acceptor_proxy = acceptor.into_proxy();
        let accept_fut = acceptor_proxy.accept(&fhardware_vsock::Addr {
            local_port: IDENTIFY_VSOCK_PORT,
            remote_cid: 2,
            remote_port: 12345,
        });

        let transport = accept_fut.await.expect("accept result").expect("connection transport");
        let socket = fasync::Socket::from_socket(transport.data);
        let mut buf = vec![0u8; 1024];
        let (mut reader, _) = socket.split();
        let bytes_read = reader.read(&mut buf).await.expect("read identity response");
        assert!(bytes_read > 0, "expected identity response bytes");

        drop(acceptor_proxy);
        drop(service);
        drop(stream);

        let result = supervision_task.await;
        assert!(result.is_ok());
    }

    #[fuchsia::test]
    async fn test_run_identify_vsock_unsupported_exits_cleanly() {
        let service = Rc::new(RemoteControlService::new_with_default_allocator(|_, _| ()).await);
        let weak_service = Rc::downgrade(&service);

        let connector_factory = || {
            let (proxy, server_end) = fidl::endpoints::create_proxy::<vsock::ConnectorMarker>();
            server_end.close_with_epitaph(fidl::Status::NOT_FOUND).expect("closed epitaph");
            Ok(proxy)
        };

        let result = run_identify_vsock_internal(
            weak_service,
            ReconnectBackoff::Immediate,
            connector_factory,
        )
        .await;

        assert!(
            result.is_ok(),
            "expected clean Ok exit for unsupported vsock identify, got: {result:?}"
        );
    }
}
