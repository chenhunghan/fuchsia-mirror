// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logs::error::LogsError;
use crate::logs::repository::{LogsRepository, STATIC_CONNECTION_ID};
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fuchsia_async as fasync;
use futures::StreamExt;
use log::warn;
use std::sync::Arc;

pub struct LogSettingsServer {
    /// The repository holding the logs.
    logs_repo: Arc<LogsRepository>,

    /// Scope holding all of the server Tasks.
    scope: fasync::Scope,
}

impl LogSettingsServer {
    pub fn new(logs_repo: Arc<LogsRepository>, scope: fasync::Scope) -> Self {
        Self { logs_repo, scope }
    }

    /// Spawn a task to handle requests from components reading the shared log.
    pub fn spawn(&self, stream: fdiagnostics::LogSettingsRequestStream) {
        let logs_repo = Arc::clone(&self.logs_repo);
        self.scope.spawn(async move {
            if let Err(e) = Self::handle_requests(logs_repo, stream).await {
                warn!("error handling Log requests: {}", e);
            }
        });
    }

    pub async fn handle_requests(
        logs_repo: Arc<LogsRepository>,
        mut stream: fdiagnostics::LogSettingsRequestStream,
    ) -> Result<(), LogsError> {
        let connection_id = logs_repo.new_interest_connection();
        while let Some(request) = stream.next().await {
            let request = request.map_err(|source| LogsError::HandlingRequests {
                protocol: fdiagnostics::LogSettingsMarker::PROTOCOL_NAME,
                source,
            })?;
            match request {
                fidl_fuchsia_diagnostics::LogSettingsRequest::SetComponentInterest {
                    payload,
                    responder,
                } => {
                    if let Some(selectors) = payload.selectors {
                        let connection_id = if payload.persist.unwrap_or(false) {
                            STATIC_CONNECTION_ID
                        } else {
                            connection_id
                        };
                        logs_repo.update_logs_interest(connection_id, selectors);
                    }
                    responder.send().ok();
                }
            }
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
    use fidl_fuchsia_diagnostics_types::{Interest, Severity};
    use selectors::{VerboseError, parse_component_selector};

    fn init_repo() -> Arc<LogsRepository> {
        LogsRepository::new(
            create_ring_buffer(65536),
            std::iter::empty(),
            &Default::default(),
            fasync::Scope::new(),
        )
    }

    #[fuchsia::test]
    async fn set_component_interest_with_selectors() {
        let repo = init_repo();
        let scope = fasync::Scope::new();
        let server = LogSettingsServer::new(Arc::clone(&repo), scope);

        let (proxy, stream) = create_proxy_and_stream::<fdiagnostics::LogSettingsMarker>();
        server.spawn(stream);

        let component_selector = parse_component_selector::<VerboseError>("a/b/c").unwrap();
        let interests = vec![fdiagnostics::LogInterestSelector {
            selector: component_selector,
            interest: Interest { min_severity: Some(Severity::Info), ..Default::default() },
        }];

        // Test non-persistent
        proxy
            .set_component_interest(&fdiagnostics::LogSettingsSetComponentInterestRequest {
                selectors: Some(interests.clone()),
                persist: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();

        // Test persistent
        proxy
            .set_component_interest(&fdiagnostics::LogSettingsSetComponentInterestRequest {
                selectors: Some(interests),
                persist: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    #[fuchsia::test]
    async fn set_component_interest_without_selectors() {
        let repo = init_repo();
        let scope = fasync::Scope::new();
        let server = LogSettingsServer::new(Arc::clone(&repo), scope);

        let (proxy, stream) = create_proxy_and_stream::<fdiagnostics::LogSettingsMarker>();
        server.spawn(stream);

        // Test with selectors: None
        proxy
            .set_component_interest(&fdiagnostics::LogSettingsSetComponentInterestRequest {
                selectors: None,
                persist: None,
                ..Default::default()
            })
            .await
            .unwrap();
    }

    #[fuchsia::test]
    async fn stream_error_handling() {
        let repo = init_repo();
        let (proxy, stream) = create_proxy_and_stream::<fdiagnostics::LogSettingsMarker>();

        // Write invalid header to trigger decode failure in stream.
        proxy.as_channel().write(&[0xff; 16], &mut []).expect("write invalid bytes");

        let result = LogSettingsServer::handle_requests(repo, stream).await;
        assert_matches::assert_matches!(result, Err(LogsError::HandlingRequests { .. }));
    }
}
