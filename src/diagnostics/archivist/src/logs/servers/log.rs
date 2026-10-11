// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logs::error::LogsError;
use crate::logs::listener::Listener;
use crate::logs::repository::LogsRepository;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_diagnostics::StreamMode;
use fidl_fuchsia_logger as flogger;
use fuchsia_async as fasync;
use futures::StreamExt;
use log::warn;
use std::pin::pin;
use std::sync::Arc;

pub struct LogServer {
    /// The repository holding the logs.
    logs_repo: Arc<LogsRepository>,

    /// Scope in which we spawn all of the server tasks.
    scope: fasync::Scope,
}

impl LogServer {
    pub fn new(logs_repo: Arc<LogsRepository>, scope: fasync::Scope) -> Self {
        Self { logs_repo, scope }
    }

    /// Spawn a task to handle requests from components reading the shared log.
    pub fn spawn(&self, stream: flogger::LogRequestStream) {
        let logs_repo = Arc::clone(&self.logs_repo);
        let scope = self.scope.to_handle();
        self.scope.spawn(async move {
            if let Err(e) = Self::handle_requests(logs_repo, stream, scope).await {
                warn!("error handling Log requests: {}", e);
            }
        });
    }

    /// Handle requests to `fuchsia.logger.Log`. All request types read the
    /// whole backlog from memory, `DumpLogs(Safe)` stops listening after that.
    async fn handle_requests(
        logs_repo: Arc<LogsRepository>,
        mut stream: flogger::LogRequestStream,
        scope: fasync::ScopeHandle,
    ) -> Result<(), LogsError> {
        let connection_id = logs_repo.new_interest_connection();
        while let Some(request) = stream.next().await {
            let request = request.map_err(|source| LogsError::HandlingRequests {
                protocol: flogger::LogMarker::PROTOCOL_NAME,
                source,
            })?;
            let listener = match request {
                flogger::LogRequest::ListenSafe { log_listener, options, .. } => {
                    Listener::new(log_listener, options)?
                }
            };
            let logs =
                logs_repo.logs_cursor(StreamMode::SnapshotThenSubscribe, Vec::new()).map(Arc::new);
            scope.spawn(async move {
                listener.run(pin!(logs)).await;
            });
        }
        logs_repo.finish_interest_connection(connection_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::shared_buffer::create_ring_buffer;
    use fidl::endpoints::{Proxy, create_proxy_and_stream};

    fn init_repo() -> Arc<LogsRepository> {
        LogsRepository::new(
            create_ring_buffer(65536),
            std::iter::empty(),
            &Default::default(),
            fasync::Scope::new(),
        )
    }

    #[fuchsia::test]
    async fn listen_safe_and_disconnect() {
        let repo = init_repo();
        let scope = fasync::Scope::new();
        let server = LogServer::new(Arc::clone(&repo), scope);

        let (proxy, stream) = create_proxy_and_stream::<flogger::LogMarker>();
        server.spawn(stream);

        let (client_end, _server_end) =
            fidl::endpoints::create_endpoints::<flogger::LogListenerSafeMarker>();
        proxy.listen_safe(client_end, None).unwrap();

        drop(proxy);
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
    }

    #[fuchsia::test]
    async fn stream_error_handling() {
        let repo = init_repo();
        let scope = fasync::Scope::new();
        let (proxy, stream) = create_proxy_and_stream::<flogger::LogMarker>();

        // Write invalid bytes to cause a stream error.
        proxy.as_channel().write(&[0xff; 16], &mut []).expect("write invalid bytes");

        let result = LogServer::handle_requests(repo, stream, scope.to_handle()).await;
        assert_matches::assert_matches!(result, Err(LogsError::HandlingRequests { .. }));
    }
}
