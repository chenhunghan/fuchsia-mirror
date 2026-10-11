// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logs::error::LogsError;
use crate::logs::servers::StreamError;
use fidl_fuchsia_diagnostics::BatchIteratorControlHandle;
use log::warn;
use thiserror::Error;
use zx_status::Status as ZxStatus;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Logs(#[from] LogsError),

    #[error("Failed to serve outgoing dir: {0}")]
    ServeOutgoing(#[source] anyhow::Error),

    #[error(transparent)]
    Inspect(#[from] fuchsia_inspect::Error),

    #[error(
        "Encountered a diagnostics data repository node with more than one artifact container. {0:?}"
    )]
    MultipleArtifactContainers(Vec<String>),

    #[error(transparent)]
    Hierarchy(#[from] diagnostics_hierarchy::Error),

    #[error(transparent)]
    Selectors(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum AccessorError {
    #[error("data_type must be set")]
    MissingDataType,

    #[error("client_selector_configuration must be set")]
    MissingSelectors,

    #[error("no selectors were provided")]
    EmptySelectors,

    #[error("requested selectors are unsupported: {}", .0)]
    InvalidSelectors(&'static str),

    #[error("couldn't parse/validate the provided selectors: {}", .0)]
    ParseSelectors(#[from] selectors::Error),

    #[error("only selectors of type `component:root` are supported for logs at the moment")]
    InvalidLogSelector,

    #[error("format must be set")]
    MissingFormat,

    #[error("only JSON supported right now")]
    UnsupportedFormat,

    #[error("stream_mode must be set")]
    MissingMode,

    #[error("only snapshot supported right now")]
    UnsupportedMode,

    #[error("IPC failure")]
    Ipc {
        #[from]
        source: fidl::Error,
    },

    #[error("Stream error")]
    Stream {
        #[from]
        source: StreamError,
    },

    #[error("Unable to create a VMO -- extremely unusual!")]
    VmoCreate(#[source] ZxStatus),

    #[error("Unable to write to VMO -- we may be OOMing")]
    VmoWrite(#[source] ZxStatus),

    #[error("Unable to get VMO size -- extremely unusual")]
    VmoSize(#[source] ZxStatus),

    #[error("JSON serialization failure: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("CBOR serialization failure: {0}")]
    CborSerialization(#[source] anyhow::Error),

    #[error("batch timeout was set on StreamParameter and on PerformanceConfiguration")]
    DuplicateBatchTimeout,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl AccessorError {
    pub fn close(self, control: BatchIteratorControlHandle) {
        warn!(error:% = self; "Closing BatchIterator.");
        let epitaph = match self {
            AccessorError::DuplicateBatchTimeout
            | AccessorError::MissingDataType
            | AccessorError::EmptySelectors
            | AccessorError::MissingSelectors
            | AccessorError::InvalidSelectors(_)
            | AccessorError::InvalidLogSelector
            | AccessorError::ParseSelectors(_) => ZxStatus::INVALID_ARGS,
            AccessorError::VmoCreate(status)
            | AccessorError::VmoWrite(status)
            | AccessorError::VmoSize(status) => status,
            AccessorError::MissingFormat | AccessorError::MissingMode => ZxStatus::INVALID_ARGS,
            AccessorError::UnsupportedFormat | AccessorError::UnsupportedMode => {
                ZxStatus::WRONG_TYPE
            }
            AccessorError::Serialization { .. } => ZxStatus::BAD_STATE,
            AccessorError::CborSerialization { .. } => ZxStatus::BAD_STATE,
            AccessorError::Ipc { .. } | AccessorError::Stream { .. } | AccessorError::Io(_) => {
                ZxStatus::IO
            }
        };
        control.shutdown_with_epitaph(epitaph);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::{RequestStream, create_proxy_and_stream};
    use fidl_fuchsia_diagnostics::BatchIteratorMarker;
    use futures::StreamExt;

    #[fuchsia::test]
    async fn close_epitaph_mapping() {
        let cases = [
            (ZxStatus::INVALID_ARGS, AccessorError::DuplicateBatchTimeout),
            (ZxStatus::INVALID_ARGS, AccessorError::MissingDataType),
            (ZxStatus::INVALID_ARGS, AccessorError::EmptySelectors),
            (ZxStatus::INVALID_ARGS, AccessorError::MissingSelectors),
            (ZxStatus::INVALID_ARGS, AccessorError::InvalidSelectors("test")),
            (ZxStatus::INVALID_ARGS, AccessorError::InvalidLogSelector),
            (
                ZxStatus::INVALID_ARGS,
                AccessorError::ParseSelectors(selectors::Error::NonFlatDirectory),
            ),
            (ZxStatus::INVALID_ARGS, AccessorError::MissingFormat),
            (ZxStatus::INVALID_ARGS, AccessorError::MissingMode),
            (ZxStatus::NO_MEMORY, AccessorError::VmoCreate(ZxStatus::NO_MEMORY)),
            (ZxStatus::BUFFER_TOO_SMALL, AccessorError::VmoWrite(ZxStatus::BUFFER_TOO_SMALL)),
            (ZxStatus::BAD_HANDLE, AccessorError::VmoSize(ZxStatus::BAD_HANDLE)),
            (ZxStatus::WRONG_TYPE, AccessorError::UnsupportedFormat),
            (ZxStatus::WRONG_TYPE, AccessorError::UnsupportedMode),
            (
                ZxStatus::BAD_STATE,
                AccessorError::Serialization(serde_json::from_str::<i32>("invalid").unwrap_err()),
            ),
            (ZxStatus::BAD_STATE, AccessorError::CborSerialization(anyhow::anyhow!("cbor fail"))),
            (ZxStatus::IO, AccessorError::Ipc { source: fidl::Error::ExtraBytes }),
            (
                ZxStatus::IO,
                AccessorError::Stream { source: StreamError::Io(std::io::Error::other("test")) },
            ),
            (ZxStatus::IO, AccessorError::Io(std::io::Error::other("test"))),
        ];

        for (expected, err) in cases {
            let (proxy, stream) = create_proxy_and_stream::<BatchIteratorMarker>();
            err.close(stream.control_handle());
            assert_matches!(
                proxy.take_event_stream().next().await,
                Some(Err(fidl::Error::ClientChannelClosed {
                    epitaph: fidl::Epitaph::Explicit(Err(status)),
                    ..
                })) if status == expected
            );
        }
    }
}
