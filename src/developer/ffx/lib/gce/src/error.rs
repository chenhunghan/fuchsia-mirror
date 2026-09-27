// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The error type returned by this crate.
//!
//! Failure modes that a caller may want to react to programmatically (an HTTP status from a
//! Google Cloud API, an unsupported guest architecture, missing credentials) each get their own
//! variant carrying the data needed to make that decision. Every variant renders a message that
//! names the resource it concerns, so failures stay actionable without an `anyhow`-style chain of
//! contexts wrapped around them.

use crate::client::{Api, remediation_hint};
use discovery::gce_watcher::InstanceError;
use hyper::StatusCode;
use product_bundle::ProductBundleLoadError;
use sdk_metadata::CpuArchitecture;
use url::Url;

/// A failure reported by a dependency that does not expose a typed error.
pub type BoxedError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result of an operation in this crate.
pub type Result<T, E = GceError> = std::result::Result<T, E>;

/// An error encountered while managing a Fuchsia VM on Google Compute Engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GceError {
    /// A setting was provided neither on the command line nor in `ffx config`.
    #[error(
        "No {name} specified. Provide --{param} or configure via \
         `ffx config set gce.{param} <{param}>`."
    )]
    MissingSetting { name: &'static str, param: &'static str },

    /// There are no Google Cloud credentials to authenticate API calls with.
    #[error("No Google Cloud credentials found. Run `ffx auth generate`.")]
    MissingCredentials,

    /// Stored credentials could not be exchanged for an access token.
    #[error(
        "Failed to obtain Google Cloud access token. Your credentials may have expired; \
         run `ffx auth generate`."
    )]
    AccessToken(#[source] gcs::error::GcsError),

    /// A project, zone, or instance name is not a legal GCE identifier.
    #[error(transparent)]
    InvalidInstance(#[from] InstanceError),

    /// A Google Cloud REST call returned an unexpected HTTP status.
    ///
    /// The status is carried separately from the message so callers can distinguish, for example,
    /// "this resource does not exist" from "you are not allowed to look at it".
    #[error(
        "API request to {url} failed with status {status}: {body}{}",
        remediation_suffix(.api, .status)
    )]
    Api { api: Api, status: StatusCode, url: Url, body: String },

    /// An HTTP request could not be completed.
    #[error("HTTP request to {url} failed: {source}")]
    Transport {
        url: Url,
        #[source]
        source: BoxedError,
    },

    /// An HTTP request could not be constructed. This indicates a bug in this crate.
    #[error("Failed to build HTTP request: {0}")]
    RequestBuild(#[from] hyper::http::Error),

    /// An API base URL is malformed. This indicates a bug in this crate.
    #[error("Invalid API base URL '{base}'")]
    InvalidEndpointUrl { base: String },

    /// A request body could not be serialized.
    #[error("Failed to serialize request body as JSON: {0}")]
    JsonSerialize(#[source] serde_json::Error),

    /// A response body was not the JSON this crate expected.
    #[error("Failed to parse JSON response: {0}")]
    JsonParse(#[source] serde_json::Error),

    /// A GCE operation completed unsuccessfully.
    #[error("Operation failed: {message}")]
    OperationFailed { message: String },

    /// A GCE operation did not reach `DONE` in time.
    #[error(
        "Timed out after {minutes} minutes waiting for GCE operation '{op_name}' to finish. \
         Check its state with `{list_command}`."
    )]
    OperationTimeout { op_name: String, minutes: u64, list_command: String },

    /// GCE returned an unfinished operation that cannot be polled.
    #[error(
        "GCE returned an operation with status '{status}' but no name, so its completion cannot \
         be awaited. Retry, or inspect it with `{list_command}`."
    )]
    OperationMissingName { status: String, list_command: String },

    /// A GCS resumable upload could not be started.
    #[error("GCS resumable upload response is missing a Location header")]
    MissingUploadSession,

    /// A GCS resumable upload session URL could not be used.
    #[error("GCS resumable upload session URL is invalid: {reason}")]
    InvalidUploadSession { reason: String },

    /// A file being uploaded ended before the length it reported.
    #[error("Unexpected EOF while reading upload file at offset {offset}/{total_size}")]
    UnexpectedEof { offset: u64, total_size: u64 },

    /// A GCS resumable upload acknowledged an offset outside the range just sent.
    #[error(
        "GCS resumable upload reported invalid next offset {reported} after uploading {start}..{end}"
    )]
    InvalidUploadOffset { reported: u64, start: u64, end: u64 },

    /// A path that has to be rendered into a manifest is not valid UTF-8.
    #[error("Path is not valid UTF-8: {path}")]
    NonUtf8Path { path: String },

    /// A product bundle could not be loaded.
    #[error("Failed to load product bundle at {path}: {source}")]
    ProductBundle {
        path: String,
        #[source]
        source: ProductBundleLoadError,
    },

    /// A product bundle does not contain an image this crate needs.
    #[error("No .{extension} image found in system_a of product bundle at {path}")]
    MissingSystemImage { extension: String, path: String },

    /// GCE cannot host the architecture the product bundle targets.
    #[error("`ffx gce start` supports x64 and arm64 product bundles, but this one targets {arch}.")]
    UnsupportedArchitecture { arch: CpuArchitecture },

    /// There is not enough scratch space to synthesize a disk image.
    #[error(
        "Synthesizing a GCE disk image needs about {needed_gib} GiB of free space in {dir}, \
         which has {available_gib} GiB. Set TMPDIR to a directory with more space and try again."
    )]
    InsufficientScratchSpace { dir: String, needed_gib: u64, available_gib: u64 },

    /// The scratch filesystem filled up while a disk image was being synthesized.
    #[error(
        "{tmpdir} ran out of space while building the GCE disk image. Set TMPDIR to a directory \
         with at least {needed_gib} GiB free and try again."
    )]
    OutOfScratchSpace {
        tmpdir: String,
        needed_gib: u64,
        #[source]
        source: Box<GceError>,
    },

    /// No local TCP port was available for the SSH tunnel.
    #[error("Failed to pick an unused local TCP port")]
    NoAvailablePort,

    /// `ssh` reported a failure that retrying cannot resolve.
    #[error("{remediation}")]
    SshUnrecoverable { remediation: &'static str },

    /// The SSH tunnel could not be established.
    #[error("Failed to establish SSH tunnel after {attempts} attempts: {detail}")]
    TunnelFailed { attempts: usize, detail: String },

    /// An I/O operation failed, annotated with what was being attempted.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// An operation delegated to another crate failed.
    #[error("{context}: {source}")]
    Dependency {
        context: String,
        #[source]
        source: BoxedError,
    },
}

impl GceError {
    /// Wraps a failure reported by a dependency that does not expose a typed error.
    pub fn dependency(context: impl Into<String>, source: impl Into<BoxedError>) -> Self {
        Self::Dependency { context: context.into(), source: source.into() }
    }

    /// Returns the first [`std::io::Error`] in this error's source chain, if any.
    pub fn io_cause(&self) -> Option<&std::io::Error> {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(self);
        while let Some(err) = source {
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                return Some(io);
            }
            source = err.source();
        }
        None
    }
}

/// `write_file_atomically` reports its own I/O failures through the caller's error type, which
/// gives it no opportunity to describe what it was doing.
impl From<std::io::Error> for GceError {
    fn from(source: std::io::Error) -> Self {
        Self::Io { context: "I/O error".to_string(), source }
    }
}

/// Renders the advice for an HTTP failure as a suffix that is empty when there is no advice.
fn remediation_suffix(api: &Api, status: &StatusCode) -> String {
    remediation_hint(*api, *status).map(|hint| format!("\n{hint}")).unwrap_or_default()
}

/// Attaches a description of the operation being attempted to an I/O failure.
///
/// This mirrors how the failing call reads, so that the resulting [`GceError::Io`] names the file
/// and the operation rather than just the `errno`.
pub trait IoContext<T> {
    fn io_context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, std::io::Error> {
    fn io_context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| GceError::Io { context: context(), source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_api_error_includes_remediation() {
        let err = GceError::Api {
            api: Api::Compute,
            status: StatusCode::FORBIDDEN,
            url: Url::parse("https://compute.googleapis.com/compute/v1/projects/p").unwrap(),
            body: "denied".to_string(),
        };
        let message = err.to_string();
        assert!(message.contains("403 Forbidden"), "{message}");
        assert!(message.contains("compute.googleapis.com"), "{message}");

        // Statuses without advice must not grow a trailing blank line.
        let not_found = GceError::Api {
            api: Api::Compute,
            status: StatusCode::NOT_FOUND,
            url: Url::parse("https://compute.googleapis.com/compute/v1/projects/p").unwrap(),
            body: "missing".to_string(),
        };
        assert!(not_found.to_string().ends_with("missing"), "{not_found}");
    }

    #[fuchsia::test]
    fn test_io_context_names_the_operation() {
        let err: GceError = std::result::Result::<(), _>::Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))
        .io_context(|| "Failed to open /tmp/disk.raw".to_string())
        .unwrap_err();
        assert!(err.to_string().starts_with("Failed to open /tmp/disk.raw: "), "{err}");
        assert_eq!(err.io_cause().map(|e| e.kind()), Some(std::io::ErrorKind::PermissionDenied));
    }

    #[fuchsia::test]
    fn test_io_cause_walks_the_source_chain() {
        let nested = GceError::OutOfScratchSpace {
            tmpdir: "/tmp".to_string(),
            needed_gib: 23,
            source: Box::new(GceError::Io {
                context: "Failed to write disk.raw".to_string(),
                source: std::io::Error::from(std::io::ErrorKind::StorageFull),
            }),
        };
        assert_eq!(nested.io_cause().map(|e| e.kind()), Some(std::io::ErrorKind::StorageFull));

        // Errors with no I/O cause must not report one.
        assert!(GceError::MissingCredentials.io_cause().is_none());
    }
}
