// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_diagnostics_system as ftarget;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use log::warn;

#[derive(Clone)]
pub struct LogFreezeServer {
    freezer: UnboundedSender<oneshot::Sender<zx::EventPair>>,
}

impl LogFreezeServer {
    pub fn new() -> (Self, UnboundedReceiver<oneshot::Sender<zx::EventPair>>) {
        let (freezer, rx) = unbounded();
        (Self { freezer }, rx)
    }

    /// Actually handle the FIDL request. This handles only a single request, then exits.
    pub async fn handle_requests(
        &self,
        mut stream: ftarget::SerialLogControlRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) = stream.next().await {
            match request? {
                fidl_fuchsia_diagnostics_system::SerialLogControlRequest::FreezeSerialForwarding { responder } => {
                    let (tx, rx) = oneshot::channel();
                    self.freezer.unbounded_send(tx)?;
                    // Ignore errors.
                    let _ = responder.send(rx.await?);
                },
                ftarget::SerialLogControlRequest::_UnknownMethod {
                                            ordinal,
                                            method_type,
                                            control_handle,
                                            ..
                                        } => {
                                            warn!(ordinal, method_type:?; "Unknown request. Closing connection");
                                            control_handle.shutdown_with_epitaph(zx::Status::UNAVAILABLE);
                                        }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::{Proxy, create_proxy_and_stream};

    #[fuchsia::test]
    async fn freeze_serial_forwarding_success() {
        let (server, mut rx) = LogFreezeServer::new();
        let (proxy, stream) = create_proxy_and_stream::<ftarget::SerialLogControlMarker>();

        let server_task = fuchsia_async::Task::spawn(async move {
            server.handle_requests(stream).await.unwrap();
        });

        let freeze_fut = proxy.freeze_serial_forwarding();

        let (p1, p2) = zx::EventPair::create();
        let tx = rx.next().await.expect("received freezer channel");
        tx.send(p1).expect("send eventpair to server");

        let received_p2 = freeze_fut.await.expect("freeze response received");
        assert_eq!(p2.basic_info().unwrap().related_koid, received_p2.koid().unwrap());

        drop(proxy);
        server_task.await;
    }

    #[fuchsia::test]
    async fn unknown_method_closes_with_unavailable_epitaph() {
        let (server, _rx) = LogFreezeServer::new();
        let (proxy, stream) = create_proxy_and_stream::<ftarget::SerialLogControlMarker>();

        let server_task = fuchsia_async::Task::spawn(async move {
            server.handle_requests(stream).await.unwrap();
        });

        // Send a flexible one-way unknown method ordinal.
        let header = fidl::encoding::TransactionHeader::new(
            0,
            0x1234_5678,
            fidl::encoding::DynamicFlags::FLEXIBLE,
        );
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                std::mem::size_of_val(&header),
            )
        };
        proxy.as_channel().write(bytes, &mut []).expect("write raw unknown method header");

        let mut event_stream = proxy.take_event_stream();
        let event = event_stream.next().await;
        assert_matches!(
            event,
            Some(Err(fidl::Error::ClientChannelClosed {
                epitaph: fidl::Epitaph::Explicit(Err(zx::Status::UNAVAILABLE)),
                ..
            }))
        );

        server_task.await;
    }

    #[fuchsia::test]
    async fn client_closes_channel() {
        let (server, _rx) = LogFreezeServer::new();
        let (proxy, stream) = create_proxy_and_stream::<ftarget::SerialLogControlMarker>();

        drop(proxy);
        assert!(server.handle_requests(stream).await.is_ok());
    }

    #[fuchsia::test]
    async fn freezer_dropped_returns_error() {
        let (server, rx) = LogFreezeServer::new();
        drop(rx);

        let (proxy, stream) = create_proxy_and_stream::<ftarget::SerialLogControlMarker>();

        let server_task =
            fuchsia_async::Task::spawn(async move { server.handle_requests(stream).await });

        let freeze_fut = proxy.freeze_serial_forwarding();
        let (result, _) = futures::join!(server_task, freeze_fut);
        assert!(result.is_err());
    }
}
