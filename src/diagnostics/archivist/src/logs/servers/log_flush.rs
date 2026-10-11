// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logs::error::LogsError;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fuchsia_async::Scope;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use log::warn;

pub struct LogFlushServer {
    scope: Scope,
    /// Channel used to request log flushing
    flush_channel: UnboundedSender<oneshot::Sender<()>>,
}

impl LogFlushServer {
    pub fn new(scope: Scope, flush_channel: UnboundedSender<oneshot::Sender<()>>) -> Self {
        Self { scope, flush_channel }
    }

    /// Listens for flush requests on a FIDL channel.
    pub fn spawn(&self, stream: fdiagnostics::LogFlusherRequestStream) {
        let flush_channel = self.flush_channel.clone();
        self.scope.spawn(async move {
            if let Err(e) = Self::handle_requests(stream, flush_channel).await {
                warn!("error handling Log requests: {}", e);
            }
        });
    }

    /// Actually handle the FIDL requests.
    async fn handle_requests(
        mut stream: fdiagnostics::LogFlusherRequestStream,
        flush_channel: UnboundedSender<oneshot::Sender<()>>,
    ) -> Result<(), LogsError> {
        while let Some(request) = stream.next().await {
            let request = request.map_err(|source| LogsError::HandlingRequests {
                protocol: fdiagnostics::LogFlusherMarker::PROTOCOL_NAME,
                source,
            })?;

            match request {
                fdiagnostics::LogFlusherRequest::WaitUntilFlushed { responder } => {
                    let (sender, receiver) = oneshot::channel();
                    // This will be dropped if we're in a configuration without serial, in which case
                    // we just ignore and reply to the request immediately.
                    let _ = flush_channel.unbounded_send(sender);

                    // Wait for flush to complete
                    let _ = receiver.await;

                    // We don't care if the other side exits
                    let _ = responder.send();
                }
                fdiagnostics::LogFlusherRequest::_UnknownMethod {
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
    use fidl::endpoints::{Proxy, create_proxy};
    use futures::FutureExt;
    use futures::channel::mpsc::unbounded;
    use std::pin::pin;

    #[fuchsia::test]
    async fn all_logs_get_flushed_when_flush_is_received_before_returning_from_flush() {
        let (flush_requester, mut flush_receiver) = unbounded();
        let server = LogFlushServer::new(Scope::new(), flush_requester);

        let (proxy, stream) = create_proxy::<fdiagnostics::LogFlusherMarker>();
        server.spawn(stream.into_stream());

        let mut flush_fut = pin!(proxy.wait_until_flushed());

        // The future shouldn't be ready.
        assert!(flush_fut.as_mut().now_or_never().is_none());

        // The server should have sent a flush request.
        let flush_ack_sender = flush_receiver.next().await.unwrap();

        // The future still shouldn't be ready.
        assert!(flush_fut.as_mut().now_or_never().is_none());

        // Ack the flush.
        flush_ack_sender.send(()).unwrap();

        // The future should be ready now.
        flush_fut.await.unwrap();
    }

    #[fuchsia::test]
    async fn flush_completes_when_flush_receiver_dropped() {
        let (flush_requester, flush_receiver) = unbounded();
        // Drop receiver immediately (simulating configuration without serial).
        drop(flush_receiver);
        let server = LogFlushServer::new(Scope::new(), flush_requester);

        let (proxy, stream) = create_proxy::<fdiagnostics::LogFlusherMarker>();
        server.spawn(stream.into_stream());

        // Calling wait_until_flushed should complete immediately.
        proxy.wait_until_flushed().await.unwrap();
    }

    #[fuchsia::test]
    async fn unknown_method_closes_with_unavailable_epitaph() {
        let (flush_requester, _flush_receiver) = unbounded();
        let server = LogFlushServer::new(Scope::new(), flush_requester);

        let (proxy, stream) = create_proxy::<fdiagnostics::LogFlusherMarker>();
        server.spawn(stream.into_stream());

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
        assert_matches::assert_matches!(
            event,
            Some(Err(fidl::Error::ClientChannelClosed {
                epitaph: fidl::Epitaph::Explicit(Err(zx::Status::UNAVAILABLE)),
                ..
            }))
        );
    }

    #[fuchsia::test]
    async fn stream_error_handling() {
        let (flush_requester, _flush_receiver) = unbounded();
        let (proxy, stream) = create_proxy::<fdiagnostics::LogFlusherMarker>();

        // Write invalid header bytes (magic number 0xff) to cause a stream error.
        proxy.as_channel().write(&[0xff; 16], &mut []).expect("write invalid bytes");

        let result = LogFlushServer::handle_requests(stream.into_stream(), flush_requester).await;
        assert_matches::assert_matches!(result, Err(LogsError::HandlingRequests { .. }));
    }
}
