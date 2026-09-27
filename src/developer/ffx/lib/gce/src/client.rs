// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::{GceError, IoContext as _, Result};
use crate::models::{
    FirewallAllowed, FirewallRule, Image, Instance, InstanceList, Operation, SerialPortOutput,
};
use fuchsia_hyper::{HttpsClient, new_https_client};
use http_body_util::BodyExt;
use hyper::{Method, Request, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;
use url::Url;

pub type Body = http_body_util::Full<hyper::body::Bytes>;

/// Polling interval when waiting for GCE global or zonal operations to complete.
const OPERATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Maximum time to wait for a GCE global or zonal operation to reach `DONE` before giving up.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Chunk size (64 MiB) for GCS resumable uploads.
///
/// GCS requires non-final chunks to be multiples of 256 KiB. Each chunk requires a sequential
/// HTTP PUT + `308 Resume Incomplete` round-trip, so 64 MiB amortizes round-trip latency (~16
/// requests per GiB instead of 128 at 8 MiB) while keeping host memory usage and per-chunk
/// retry retransmission bounded to 64 MiB.
const GCS_UPLOAD_CHUNK_SIZE: usize = 64 * 1024 * 1024;

/// Maximum number of retry attempts for a failed chunk in a GCS resumable upload.
const GCS_UPLOAD_MAX_RETRIES: u32 = 3;

/// Delay between retry attempts for a failed GCS upload chunk.
const GCS_UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Google-internal Cloud Uberproxy IPv4 CIDR block used for SSH ingress to GCE VMs.
///
/// Note: External GCP environments or connections via Cloud Identity-Aware Proxy (IAP,
/// which uses `35.235.240.0/20`) require different source ranges; this could be made
/// configurable via `ffx config` in the future.
const SSH_INGRESS_SOURCE_RANGE: &str = "172.253.30.0/23";
const SSH_PORT: &str = "22";
const DEFAULT_FIREWALL_PRIORITY: u32 = 1000;

/// The Google Cloud API that serves an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Compute,
    Storage,
}

/// A REST endpoint: the URL to call plus the API that serves it.
///
/// Carrying the API alongside the URL lets failures be annotated with service-specific
/// remediation at the point they are reported, rather than re-deriving the service from the URL.
#[derive(Clone, Debug)]
struct Endpoint {
    api: Api,
    url: Url,
}

/// Whether an operation is global or scoped to a zone, which determines how `gcloud` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationScope<'a> {
    Global,
    Zone(&'a str),
}

impl OperationScope<'_> {
    /// Renders the `gcloud` invocation that shows `op`, for use in user-facing diagnostics.
    fn list_command(&self, project: &str, op: &Operation) -> String {
        let scope = match self {
            Self::Global => "--global".to_string(),
            Self::Zone(zone) => format!("--zones={zone}"),
        };
        // An unnamed operation can still be found through the resource it targets.
        let filter = match (op.name.as_deref(), op.target_link.as_deref()) {
            (Some(name), _) => format!(" --filter=\"name={name}\""),
            (None, Some(target)) => format!(" --filter=\"targetLink={target}\""),
            (None, None) => String::new(),
        };
        format!("gcloud compute operations list --project={project} {scope}{filter}")
    }
}

/// High-level client for interacting with Google Compute Engine (GCE) and
/// Google Cloud Storage (GCS) APIs.
#[derive(Debug)]
pub struct GceClient {
    http: HttpClient,
}

impl GceClient {
    pub fn new(access_token: String) -> Self {
        Self { http: HttpClient::new(access_token) }
    }

    /// Fetches a GCE image by name, returning `Ok(None)` if the image does not exist (`404 Not Found`).
    pub async fn get_image(&self, project: &str, image_name: &str) -> Result<Option<Image>> {
        self.http.get_optional_json(endpoints::image(project, image_name)?).await
    }

    pub async fn insert_image(&self, project: &str, image: &Image) -> Result<Operation> {
        self.http.post_json(endpoints::images(project)?, image).await
    }

    pub async fn delete_image(&self, project: &str, image_name: &str) -> Result<Operation> {
        self.http.delete_json(endpoints::image(project, image_name)?).await
    }

    /// Fetches a GCE instance by name, returning `Ok(None)` if the instance does not exist (`404 Not Found`).
    pub async fn get_instance(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
    ) -> Result<Option<Instance>> {
        self.http.get_optional_json(endpoints::instance(project, zone, instance_name)?).await
    }

    pub async fn get_serial_port_output(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
        port: u32,
        start: Option<i64>,
    ) -> Result<SerialPortOutput> {
        let url = endpoints::serial_port_output(project, zone, instance_name, port, start)?;
        self.http.get_json(url).await
    }

    pub async fn list_instances(&self, project: &str, zone: &str) -> Result<Vec<Instance>> {
        let list: InstanceList =
            self.http.get_json(endpoints::list_instances(project, zone)?).await?;
        Ok(list.items)
    }

    pub async fn insert_instance(
        &self,
        project: &str,
        zone: &str,
        instance: &Instance,
    ) -> Result<Operation> {
        self.http.post_json(endpoints::instances(project, zone)?, instance).await
    }

    pub async fn delete_instance(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
    ) -> Result<Operation> {
        self.http.delete_json(endpoints::instance(project, zone, instance_name)?).await
    }

    pub async fn stop_instance(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
    ) -> Result<Operation> {
        self.http.post_empty_json(endpoints::stop_instance(project, zone, instance_name)?).await
    }

    pub async fn start_instance(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
    ) -> Result<Operation> {
        self.http.post_empty_json(endpoints::start_instance(project, zone, instance_name)?).await
    }

    pub async fn get_global_operation(&self, project: &str, op_name: &str) -> Result<Operation> {
        self.http.get_json(endpoints::global_operation(project, op_name)?).await
    }

    pub async fn get_zone_operation(
        &self,
        project: &str,
        zone: &str,
        op_name: &str,
    ) -> Result<Operation> {
        self.http.get_json(endpoints::zone_operation(project, zone, op_name)?).await
    }

    /// Polls `op` until it reaches `DONE`, fails, or [`OPERATION_TIMEOUT`] elapses.
    async fn wait_for_operation(
        &self,
        project: &str,
        scope: OperationScope<'_>,
        op: &Operation,
    ) -> Result<()> {
        let Some(op_name) = pending_operation_name(project, scope, op)? else {
            return Ok(());
        };

        let deadline = std::time::Instant::now() + OPERATION_TIMEOUT;
        loop {
            let current = match scope {
                OperationScope::Global => self.get_global_operation(project, op_name).await?,
                OperationScope::Zone(zone) => {
                    self.get_zone_operation(project, zone, op_name).await?
                }
            };
            check_operation_error(&current)?;

            if current.status.as_deref() == Some("DONE") {
                return Ok(());
            }

            if std::time::Instant::now() >= deadline {
                return Err(GceError::OperationTimeout {
                    op_name: op_name.to_string(),
                    minutes: OPERATION_TIMEOUT.as_secs() / 60,
                    list_command: scope.list_command(project, op),
                });
            }

            fuchsia_async::Timer::new(OPERATION_POLL_INTERVAL).await;
        }
    }

    /// Waits for a global operation returned by a mutating API call to complete.
    pub async fn wait_for_global_operation(&self, project: &str, op: &Operation) -> Result<()> {
        self.wait_for_operation(project, OperationScope::Global, op).await
    }

    /// Waits for a zonal operation returned by a mutating API call to complete.
    pub async fn wait_for_zone_operation(
        &self,
        project: &str,
        zone: &str,
        op: &Operation,
    ) -> Result<()> {
        self.wait_for_operation(project, OperationScope::Zone(zone), op).await
    }

    /// Fetches a GCE firewall rule by name, returning `Ok(None)` if the rule does not exist (`404 Not Found`).
    pub async fn get_firewall_rule(
        &self,
        project: &str,
        rule_name: &str,
    ) -> Result<Option<FirewallRule>> {
        self.http.get_optional_json(endpoints::firewall_rule(project, rule_name)?).await
    }

    pub async fn insert_firewall_rule(
        &self,
        project: &str,
        rule: &FirewallRule,
    ) -> Result<Operation> {
        self.http.post_json(endpoints::firewalls(project)?, rule).await
    }

    /// Ensures a firewall ingress rule allowing SSH traffic exists for `network`.
    ///
    /// Errors querying the rule are propagated. If the rule is absent and cannot be created (for
    /// example, when the caller lacks `compute.firewalls.create` IAM permissions), the `gcloud`
    /// command that creates it is written to `warn` and `Ok(())` is returned, so connecting can
    /// still proceed against an equivalent pre-provisioned rule.
    pub async fn ensure_ssh_firewall_rule(
        &self,
        project: &str,
        network: &str,
        warn: &mut dyn std::io::Write,
    ) -> Result<()> {
        let rule_name = ssh_firewall_rule_name(network);

        // A failure here already names the firewall rule URL that was queried.
        if self.get_firewall_rule(project, &rule_name).await?.is_some() {
            return Ok(());
        }

        let trimmed_network = network.trim_matches('/');
        let network = if trimmed_network.starts_with("global/networks/")
            || trimmed_network.starts_with("projects/")
            || trimmed_network.starts_with("https://")
        {
            trimmed_network.to_string()
        } else {
            let net_name = if trimmed_network.is_empty() { "default" } else { trimmed_network };
            format!("global/networks/{net_name}")
        };
        let rule = FirewallRule {
            name: rule_name.clone(),
            network: Some(network.clone()),
            source_ranges: vec![SSH_INGRESS_SOURCE_RANGE.to_string()],
            allowed: vec![FirewallAllowed {
                ip_protocol: "tcp".to_string(),
                ports: vec![SSH_PORT.to_string()],
            }],
            direction: Some("INGRESS".to_string()),
            priority: Some(DEFAULT_FIREWALL_PRIORITY),
        };

        if let Err(e) = self.insert_firewall_rule(project, &rule).await {
            log::warn!("Failed to insert SSH firewall rule {rule_name}: {e:?}");
            writeln!(
                warn,
                "Warning: SSH ingress firewall rule '{rule_name}' is missing and could not be \
                 created: {e}\nSSH to the VM will time out unless an equivalent rule already \
                 exists. Ask someone with `compute.firewalls.create` to run:\n  {}",
                create_firewall_rule_command(project, &rule)
            )
            .io_context(|| "Failed to write firewall rule warning".to_string())?;
        }
        Ok(())
    }

    /// Ensures the specified GCS bucket exists by attempting to create it directly
    /// and treating `409 Conflict` (already exists) as success.
    ///
    /// Note that GCS bucket names are globally unique, so `409 Conflict` can also mean the name is
    /// owned by a different project. That case surfaces as a `403 Forbidden` on the subsequent
    /// upload, which [`remediation_hint`] annotates with advice to pick a different bucket.
    pub async fn ensure_bucket(&self, project: &str, bucket: &str) -> Result<()> {
        let url = endpoints::create_bucket(project)?;
        let body = serde_json::to_vec(&serde_json::json!({ "name": bucket }))
            .map_err(GceError::JsonSerialize)?;
        self.http.post_raw(url, "application/json", body, &[StatusCode::CONFLICT]).await
    }

    /// Uploads a local file to GCS using the GCS Resumable Upload protocol (`uploadType=resumable`),
    /// streaming the file in fixed-size chunks to avoid loading large disk images into memory at once.
    pub async fn upload_gcs_file(
        &self,
        bucket: &str,
        object_name: &str,
        file_path: &Path,
    ) -> Result<()> {
        let mut file = File::open(file_path)
            .io_context(|| format!("Failed to open {}", file_path.display()))?;
        let total_size = file
            .metadata()
            .io_context(|| format!("Failed to read metadata for {}", file_path.display()))?
            .len();

        let init_url = endpoints::upload_gcs_object(bucket, object_name)?;
        let session_url =
            self.http.start_resumable_upload(init_url, "application/gzip", total_size).await?;

        // Upload failures already name the session URL, which identifies the destination object.
        upload_reader_resumable(
            &mut file,
            total_size,
            GCS_UPLOAD_CHUNK_SIZE,
            GCS_UPLOAD_MAX_RETRIES,
            GCS_UPLOAD_RETRY_DELAY,
            |content_range, chunk, is_final| {
                let url = session_url.clone();
                async move {
                    self.http
                        .put_upload_chunk(url, "application/gzip", &content_range, chunk, is_final)
                        .await
                }
            },
        )
        .await
    }

    pub async fn delete_gcs_file(&self, bucket: &str, object_name: &str) -> Result<()> {
        let url = endpoints::delete_gcs_object(bucket, object_name)?;
        self.http.delete_raw(url, &[StatusCode::NOT_FOUND]).await
    }
}

/// Fails if `op` reports an error, formatting all reported error items.
fn check_operation_error(op: &Operation) -> Result<()> {
    let Some(err) = op.error.as_ref().filter(|e| !e.errors.is_empty()) else {
        return Ok(());
    };
    let err_msg = err
        .errors
        .iter()
        .map(|e| {
            format!(
                "{}: {}",
                e.code.as_deref().unwrap_or_default(),
                e.message.as_deref().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(GceError::OperationFailed { message: err_msg })
}

/// Returns the name of `op` if it must still be awaited, or `None` if it has already completed.
fn pending_operation_name<'a>(
    project: &str,
    scope: OperationScope<'_>,
    op: &'a Operation,
) -> Result<Option<&'a str>> {
    check_operation_error(op)?;
    if op.status.as_deref() == Some("DONE") {
        return Ok(None);
    }
    op.name.as_deref().map(Some).ok_or_else(|| GceError::OperationMissingName {
        status: op.status.as_deref().unwrap_or("unknown").to_string(),
        list_command: scope.list_command(project, op),
    })
}

/// Renders the `gcloud` invocation that creates `rule`, for use in user-facing diagnostics.
fn create_firewall_rule_command(project: &str, rule: &FirewallRule) -> String {
    let ports = rule
        .allowed
        .iter()
        .map(|a| format!("{}:{}", a.ip_protocol, a.ports.join(",")))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "gcloud compute firewall-rules create {} --project={project} --network={} \
         --direction=INGRESS --source-ranges={} --allow={ports}",
        rule.name,
        rule.network.as_deref().unwrap_or("default"),
        rule.source_ranges.join(",")
    )
}

/// Returns advice on how to recover from an HTTP error status returned by `api`.
pub(crate) fn remediation_hint(api: Api, status: StatusCode) -> Option<&'static str> {
    match (api, status) {
        (_, StatusCode::UNAUTHORIZED) => {
            Some("Your Google Cloud credentials are missing or expired. Run `ffx auth generate`.")
        }
        (Api::Storage, StatusCode::FORBIDDEN) => Some(
            "Cloud Storage denied access. GCS bucket names are globally unique, so this name may \
             belong to another project; pick a different one with `--bucket <name>` or \
             `ffx config set gce.bucket <name>`. Otherwise, enable the API with \
             `gcloud services enable storage.googleapis.com`.",
        ),
        (Api::Compute, StatusCode::FORBIDDEN) => Some(
            "Compute Engine denied access. Enable the API with \
             `gcloud services enable compute.googleapis.com` and check that you have the required \
             IAM permissions on this project.",
        ),
        _ => None,
    }
}

fn ssh_firewall_rule_name(network: &str) -> String {
    let network_suffix = network
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    format!("allow-ssh-ingress-{network_suffix}")
}

/// Reads from `reader` until `buf` is completely filled or EOF is reached.
fn read_full_chunk(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total_read = 0;
    while total_read < buf.len() {
        match reader.read(&mut buf[total_read..]) {
            Ok(0) => break,
            Ok(n) => total_read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total_read)
}

/// Parses a GCS `Range` header (e.g., `bytes=0-8388607`) and returns the next byte offset (`8388608`).
fn parse_range_header(range_header: &str) -> Option<u64> {
    let range = range_header.trim().strip_prefix("bytes=")?;
    let (_, end_str) = range.split_once('-')?;
    let end_byte = end_str.parse::<u64>().ok()?;
    end_byte.checked_add(1)
}

/// Incrementally reads chunks from `reader` and uploads them via `put_chunk`, supporting
/// automatic retry on transient errors and resuming from server-reported `Range` offsets.
async fn upload_reader_resumable<R, F, Fut>(
    reader: &mut R,
    total_size: u64,
    chunk_size: usize,
    max_retries: u32,
    retry_delay: Duration,
    mut put_chunk: F,
) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(String, Vec<u8>, bool) -> Fut,
    Fut: std::future::Future<Output = Result<Option<u64>>>,
{
    if total_size == 0 {
        let mut attempt = 0;
        loop {
            match put_chunk("bytes */0".to_string(), Vec::new(), true).await {
                Ok(_) => return Ok(()),
                Err(_) if attempt < max_retries => {
                    attempt += 1;
                    fuchsia_async::Timer::new(retry_delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    let buf_size = std::cmp::min(total_size, chunk_size as u64) as usize;
    let mut buffer = vec![0u8; buf_size];
    let mut offset: u64 = 0;

    while offset < total_size {
        reader
            .seek(SeekFrom::Start(offset))
            .io_context(|| "Failed to seek upload file".to_string())?;
        let to_read = std::cmp::min(total_size - offset, buf_size as u64) as usize;
        let bytes_read = read_full_chunk(reader, &mut buffer[..to_read])
            .io_context(|| "Failed to read chunk from upload file".to_string())?;
        if bytes_read == 0 {
            return Err(GceError::UnexpectedEof { offset, total_size });
        }

        let end = offset + (bytes_read as u64) - 1;
        let is_final = offset + (bytes_read as u64) == total_size;
        let content_range = format!("bytes {offset}-{end}/{total_size}");

        let mut attempt = 0;
        let next_offset = loop {
            let chunk_vec = buffer[..bytes_read].to_vec();
            match put_chunk(content_range.clone(), chunk_vec, is_final).await {
                Ok(reported_next) => break reported_next,
                Err(_) if attempt < max_retries => {
                    attempt += 1;
                    fuchsia_async::Timer::new(retry_delay).await;
                }
                Err(err) => return Err(err),
            }
        };

        if is_final {
            offset = total_size;
        } else {
            let max_expected = offset + bytes_read as u64;
            let committed = next_offset.unwrap_or(max_expected);
            if committed <= offset || committed > max_expected {
                return Err(GceError::InvalidUploadOffset {
                    reported: committed,
                    start: offset,
                    end: max_expected,
                });
            }
            offset = committed;
        }
    }

    Ok(())
}

/// Authenticated HTTP transport and JSON serialization layer.
#[derive(Debug)]
struct HttpClient {
    access_token: String,
    https_client: HttpsClient,
}

impl HttpClient {
    fn new(access_token: String) -> Self {
        Self { access_token, https_client: new_https_client() }
    }

    async fn send_request(
        &self,
        method: Method,
        endpoint: Endpoint,
        headers: &[(&str, &str)],
        body: Vec<u8>,
        allowed_statuses: &[StatusCode],
    ) -> Result<(StatusCode, hyper::HeaderMap, hyper::body::Bytes)> {
        let Endpoint { api, url } = endpoint;
        let mut builder = Request::builder().method(&method).uri(url.as_str());

        if !self.access_token.is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", self.access_token));
        }

        for &(k, v) in headers {
            builder = builder.header(k, v);
        }

        let req = builder.body(Body::from(body))?;
        let res = self
            .https_client
            .request(req)
            .await
            .map_err(|e| GceError::Transport { url: url.clone(), source: Box::new(e) })?;
        let status = res.status();
        let res_headers = res.headers().clone();

        let collected = res
            .into_body()
            .collect()
            .await
            .map_err(|e| GceError::Transport { url: url.clone(), source: Box::new(e) })?;
        let bytes = collected.to_bytes();

        if !status.is_success() && !allowed_statuses.contains(&status) {
            return Err(GceError::Api {
                api,
                status,
                url,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }

        Ok((status, res_headers, bytes))
    }

    async fn send_raw(
        &self,
        method: Method,
        endpoint: Endpoint,
        body: Option<(&str, Vec<u8>)>,
        allowed_statuses: &[StatusCode],
    ) -> Result<hyper::body::Bytes> {
        let (headers, body_bytes) = match body {
            Some((content_type, bytes)) => {
                let len_str = bytes.len().to_string();
                let (_, _, res_bytes) = self
                    .send_request(
                        method,
                        endpoint,
                        &[("Content-Type", content_type), ("Content-Length", &len_str)],
                        bytes,
                        allowed_statuses,
                    )
                    .await?;
                return Ok(res_bytes);
            }
            None => {
                let headers: &[(&str, &str)] = if method == Method::POST {
                    &[("Content-Type", "application/json"), ("Content-Length", "0")]
                } else {
                    &[]
                };
                (headers, Vec::new())
            }
        };

        let (_, _, res_bytes) =
            self.send_request(method, endpoint, headers, body_bytes, allowed_statuses).await?;
        Ok(res_bytes)
    }

    async fn start_resumable_upload(
        &self,
        endpoint: Endpoint,
        content_type: &str,
        total_size: u64,
    ) -> Result<Endpoint> {
        let total_size_str = total_size.to_string();
        let headers = [
            ("Content-Type", "application/json; charset=UTF-8"),
            ("Content-Length", "0"),
            ("X-Upload-Content-Type", content_type),
            ("X-Upload-Content-Length", &total_size_str),
        ];
        let api = endpoint.api;
        let (_, res_headers, _) =
            self.send_request(Method::POST, endpoint, &headers, Vec::new(), &[]).await?;
        let location = res_headers
            .get(hyper::header::LOCATION)
            .ok_or(GceError::MissingUploadSession)?
            .to_str()
            .map_err(|e| GceError::InvalidUploadSession { reason: e.to_string() })?;
        let url = Url::parse(location)
            .map_err(|e| GceError::InvalidUploadSession { reason: e.to_string() })?;
        Ok(Endpoint { api, url })
    }

    async fn put_upload_chunk(
        &self,
        session: Endpoint,
        content_type: &str,
        content_range: &str,
        chunk: Vec<u8>,
        is_final: bool,
    ) -> Result<Option<u64>> {
        let len_str = chunk.len().to_string();
        let headers = [
            ("Content-Type", content_type),
            ("Content-Length", &len_str),
            ("Content-Range", content_range),
        ];
        let allowed_statuses: &[StatusCode] =
            if is_final { &[] } else { &[StatusCode::PERMANENT_REDIRECT] };
        let (status, res_headers, _) =
            self.send_request(Method::PUT, session, &headers, chunk, allowed_statuses).await?;

        if status == StatusCode::PERMANENT_REDIRECT {
            let next_offset = res_headers
                .get(hyper::header::RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_range_header);
            return Ok(next_offset);
        }

        Ok(None)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        endpoint: Endpoint,
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        let payload = body.map(|b| ("application/json", b));
        let bytes = self.send_raw(method, endpoint, payload, &[]).await?;
        serde_json::from_slice(&bytes).map_err(GceError::JsonParse)
    }

    async fn get_json<T: DeserializeOwned>(&self, endpoint: Endpoint) -> Result<T> {
        self.send_json(Method::GET, endpoint, None).await
    }

    async fn get_optional_json<T: DeserializeOwned>(
        &self,
        endpoint: Endpoint,
    ) -> Result<Option<T>> {
        let (status, _, bytes) = self
            .send_request(Method::GET, endpoint, &[], Vec::new(), &[StatusCode::NOT_FOUND])
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        serde_json::from_slice(&bytes).map(Some).map_err(GceError::JsonParse)
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        endpoint: Endpoint,
        body: &B,
    ) -> Result<T> {
        let bytes = serde_json::to_vec(body).map_err(GceError::JsonSerialize)?;
        self.send_json(Method::POST, endpoint, Some(bytes)).await
    }

    async fn post_empty_json<T: DeserializeOwned>(&self, endpoint: Endpoint) -> Result<T> {
        self.send_json(Method::POST, endpoint, None).await
    }

    async fn delete_json<T: DeserializeOwned>(&self, endpoint: Endpoint) -> Result<T> {
        self.send_json(Method::DELETE, endpoint, None).await
    }

    async fn post_raw(
        &self,
        endpoint: Endpoint,
        content_type: &str,
        body: Vec<u8>,
        allowed_statuses: &[StatusCode],
    ) -> Result<()> {
        self.send_raw(Method::POST, endpoint, Some((content_type, body)), allowed_statuses).await?;
        Ok(())
    }

    async fn delete_raw(&self, endpoint: Endpoint, allowed_statuses: &[StatusCode]) -> Result<()> {
        self.send_raw(Method::DELETE, endpoint, None, allowed_statuses).await?;
        Ok(())
    }
}

/// Stateless endpoint construction for GCE and GCS REST APIs.
mod endpoints {
    use super::{Api, Endpoint, GceError, Result, Url};

    const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";
    const STORAGE_BASE: &str = "https://storage.googleapis.com/storage/v1";
    const STORAGE_UPLOAD_BASE: &str = "https://storage.googleapis.com/upload/storage/v1";

    fn build(api: Api, base: &str, segments: &[&str], query: &[(&str, &str)]) -> Result<Endpoint> {
        let invalid = || GceError::InvalidEndpointUrl { base: base.to_string() };
        let mut url = Url::parse(base).map_err(|_| invalid())?;
        url.path_segments_mut().map_err(|_| invalid())?.extend(segments);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(Endpoint { api, url })
    }

    fn compute(segments: &[&str], query: &[(&str, &str)]) -> Result<Endpoint> {
        build(Api::Compute, COMPUTE_BASE, segments, query)
    }

    fn storage(segments: &[&str], query: &[(&str, &str)]) -> Result<Endpoint> {
        build(Api::Storage, STORAGE_BASE, segments, query)
    }

    pub fn images(project: &str) -> Result<Endpoint> {
        compute(&["projects", project, "global", "images"], &[])
    }

    pub fn image(project: &str, image_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "global", "images", image_name], &[])
    }

    pub fn instances(project: &str, zone: &str) -> Result<Endpoint> {
        compute(&["projects", project, "zones", zone, "instances"], &[])
    }

    pub fn list_instances(project: &str, zone: &str) -> Result<Endpoint> {
        instances(project, zone)
    }

    pub fn instance(project: &str, zone: &str, instance_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "zones", zone, "instances", instance_name], &[])
    }

    pub fn start_instance(project: &str, zone: &str, instance_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "zones", zone, "instances", instance_name, "start"], &[])
    }

    pub fn stop_instance(project: &str, zone: &str, instance_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "zones", zone, "instances", instance_name, "stop"], &[])
    }

    pub fn serial_port_output(
        project: &str,
        zone: &str,
        instance_name: &str,
        port: u32,
        start: Option<i64>,
    ) -> Result<Endpoint> {
        let port_str = port.to_string();
        let start_str = start.map(|s| s.to_string());
        let mut query = vec![("port", port_str.as_str())];
        if let Some(ref s) = start_str {
            query.push(("start", s.as_str()));
        }
        compute(
            &["projects", project, "zones", zone, "instances", instance_name, "serialPort"],
            &query,
        )
    }

    pub fn global_operation(project: &str, op_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "global", "operations", op_name], &[])
    }

    pub fn zone_operation(project: &str, zone: &str, op_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "zones", zone, "operations", op_name], &[])
    }

    pub fn firewalls(project: &str) -> Result<Endpoint> {
        compute(&["projects", project, "global", "firewalls"], &[])
    }

    pub fn firewall_rule(project: &str, rule_name: &str) -> Result<Endpoint> {
        compute(&["projects", project, "global", "firewalls", rule_name], &[])
    }

    pub fn create_bucket(project: &str) -> Result<Endpoint> {
        storage(&["b"], &[("project", project)])
    }

    pub fn upload_gcs_object(bucket: &str, object_name: &str) -> Result<Endpoint> {
        build(
            Api::Storage,
            STORAGE_UPLOAD_BASE,
            &["b", bucket, "o"],
            &[("uploadType", "resumable"), ("name", object_name)],
        )
    }

    pub fn delete_gcs_object(bucket: &str, object_name: &str) -> Result<Endpoint> {
        storage(&["b", bucket, "o", object_name], &[])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Api, Duration, FirewallAllowed, FirewallRule, GceError, Operation, OperationScope,
        StatusCode, Url, endpoints, parse_range_header, upload_reader_resumable,
    };
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    const GLOBAL: OperationScope<'static> = OperationScope::Global;

    #[fuchsia::test]
    fn test_image_url_construction() {
        let url = endpoints::image("test-p", "test-img").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/global/images/test-img"
        );
    }

    #[fuchsia::test]
    fn test_images_url_construction() {
        let url = endpoints::images("test-p").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/global/images"
        );
    }

    #[fuchsia::test]
    fn test_list_instances_url_construction() {
        let url = endpoints::list_instances("test-p", "test-z").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances"
        );
    }

    #[fuchsia::test]
    fn test_get_instance_url_construction() {
        let url = endpoints::instance("test-p", "test-z", "test-inst").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst"
        );
    }

    #[fuchsia::test]
    fn test_get_serial_port_output_url_construction() {
        let url =
            endpoints::serial_port_output("test-p", "test-z", "test-inst", 1, Some(100)).unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst/serialPort?port=1&start=100"
        );

        let url_no_start =
            endpoints::serial_port_output("test-p", "test-z", "test-inst", 1, None).unwrap();
        assert_eq!(
            url_no_start.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst/serialPort?port=1"
        );
    }

    #[fuchsia::test]
    fn test_start_instance_url_construction() {
        let url = endpoints::start_instance("test-p", "test-z", "test-inst").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst/start"
        );
    }

    #[fuchsia::test]
    fn test_stop_instance_url_construction() {
        let url = endpoints::stop_instance("test-p", "test-z", "test-inst").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst/stop"
        );
    }

    #[fuchsia::test]
    fn test_delete_instance_url_construction() {
        let url = endpoints::instance("test-p", "test-z", "test-inst").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst"
        );
    }

    #[fuchsia::test]
    fn test_global_operation_url_construction() {
        let url = endpoints::global_operation("test-p", "operation-123").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/global/operations/operation-123"
        );
    }

    #[fuchsia::test]
    fn test_zone_operation_url_construction() {
        let url = endpoints::zone_operation("test-p", "test-z", "operation-123").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/operations/operation-123"
        );
    }

    #[fuchsia::test]
    fn test_firewall_rule_url_construction() {
        let url = endpoints::firewall_rule("test-p", "test-firewall").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/global/firewalls/test-firewall"
        );
    }

    #[fuchsia::test]
    fn test_firewalls_url_construction() {
        let url = endpoints::firewalls("test-p").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/global/firewalls"
        );
    }

    #[fuchsia::test]
    fn test_create_bucket_url_construction() {
        let url = endpoints::create_bucket("my-project").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://storage.googleapis.com/storage/v1/b?project=my-project"
        );
    }

    #[fuchsia::test]
    fn test_upload_gcs_url_construction() {
        let url = endpoints::upload_gcs_object("my-bucket", "folder/item.tar.gz").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://storage.googleapis.com/upload/storage/v1/b/my-bucket/o?uploadType=resumable&name=folder%2Fitem.tar.gz"
        );
    }

    #[fuchsia::test]
    fn test_delete_gcs_url_construction() {
        let url = endpoints::delete_gcs_object("my-bucket", "folder/item.tar.gz").unwrap();
        assert_eq!(
            url.url.as_str(),
            "https://storage.googleapis.com/storage/v1/b/my-bucket/o/folder%2Fitem.tar.gz"
        );
    }

    #[fuchsia::test]
    fn test_parse_range_header() {
        assert_eq!(parse_range_header("bytes=0-262143"), Some(262144));
        assert_eq!(parse_range_header(" bytes=0-0 "), Some(1));
        assert_eq!(parse_range_header("invalid"), None);
        assert_eq!(parse_range_header("bytes=100"), None);
    }

    #[fuchsia::test]
    async fn test_upload_reader_resumable_multi_chunk() {
        let data: Vec<u8> = (0..600).map(|i| (i % 251) as u8).collect();
        let total_size = data.len() as u64;
        let mut cursor = Cursor::new(data.clone());

        let recorded_ranges = Rc::new(RefCell::new(Vec::new()));
        let assembled = Rc::new(RefCell::new(Vec::new()));

        let ranges_clone = recorded_ranges.clone();
        let assembled_clone = assembled.clone();

        upload_reader_resumable(
            &mut cursor,
            total_size,
            256,
            3,
            Duration::from_millis(1),
            move |range, chunk, is_final| {
                ranges_clone.borrow_mut().push((range, chunk.len(), is_final));
                assembled_clone.borrow_mut().extend_from_slice(&chunk);
                async move { Ok(None) }
            },
        )
        .await
        .expect("upload should succeed");

        let ranges = recorded_ranges.borrow();
        assert_eq!(
            *ranges,
            vec![
                ("bytes 0-255/600".to_string(), 256, false),
                ("bytes 256-511/600".to_string(), 256, false),
                ("bytes 512-599/600".to_string(), 88, true),
            ]
        );
        assert_eq!(*assembled.borrow(), data);
    }

    #[fuchsia::test]
    async fn test_upload_reader_resumable_partial_range_and_retry() {
        let data: Vec<u8> = (0..100).collect();
        let total_size = data.len() as u64;
        let mut cursor = Cursor::new(data.clone());

        let calls = Rc::new(RefCell::new(Vec::new()));
        let calls_clone = calls.clone();

        upload_reader_resumable(
            &mut cursor,
            total_size,
            50,
            3,
            Duration::from_millis(1),
            move |range, chunk, is_final| {
                let call_idx = {
                    let mut c = calls_clone.borrow_mut();
                    c.push((range, chunk, is_final));
                    c.len()
                };
                async move {
                    match call_idx {
                        // First chunk (0..50): server commits only 0..29 (next offset = 30)
                        1 => Ok(Some(30)),
                        // Second chunk (30..80): simulate transient network failure
                        2 => Err(GceError::Transport {
                            url: Url::parse("https://storage.googleapis.com/upload").unwrap(),
                            source: Box::new(std::io::Error::other("transient network failure")),
                        }),
                        // Retry of second chunk (30..80): succeeds completely
                        3 => Ok(Some(80)),
                        // Final chunk (80..100): succeeds
                        4 => Ok(None),
                        _ => unreachable!(),
                    }
                }
            },
        )
        .await
        .expect("upload with partial range and retry should succeed");

        let recorded = calls.borrow();
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[0].0, "bytes 0-49/100");
        assert_eq!(recorded[1].0, "bytes 30-79/100");
        assert_eq!(recorded[2].0, "bytes 30-79/100");
        assert_eq!(recorded[3].0, "bytes 80-99/100");
        assert!(recorded[3].2);
    }

    #[fuchsia::test]
    async fn test_upload_reader_resumable_empty_file() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let recorded_clone = recorded.clone();

        upload_reader_resumable(
            &mut cursor,
            0,
            256,
            1,
            Duration::from_millis(1),
            move |range, chunk, is_final| {
                recorded_clone.borrow_mut().push((range, chunk.len(), is_final));
                async move { Ok(None) }
            },
        )
        .await
        .expect("empty upload should succeed");

        assert_eq!(*recorded.borrow(), vec![("bytes */0".to_string(), 0, true)]);
    }

    #[fuchsia::test]
    fn test_ssh_firewall_rule_name() {
        assert_eq!(super::ssh_firewall_rule_name("default"), "allow-ssh-ingress-default");
        assert_eq!(
            super::ssh_firewall_rule_name("global/networks/default"),
            "allow-ssh-ingress-default"
        );
        assert_eq!(
            super::ssh_firewall_rule_name("global/networks/custom-net/"),
            "allow-ssh-ingress-custom-net"
        );
        assert_eq!(super::ssh_firewall_rule_name(""), "allow-ssh-ingress-default");
        assert_eq!(super::ssh_firewall_rule_name("///"), "allow-ssh-ingress-default");
    }

    #[fuchsia::test]
    fn test_pending_operation_name() {
        let done = Operation { status: Some("DONE".to_string()), ..Default::default() };
        assert_eq!(super::pending_operation_name("p", GLOBAL, &done).unwrap(), None);

        let running = Operation {
            name: Some("operation-123".to_string()),
            status: Some("RUNNING".to_string()),
            ..Default::default()
        };
        assert_eq!(
            super::pending_operation_name("p", GLOBAL, &running).unwrap(),
            Some("operation-123")
        );

        // A pending operation without a name cannot be awaited, so dependent calls must not run.
        let nameless = Operation { status: Some("RUNNING".to_string()), ..Default::default() };
        let err = super::pending_operation_name("p", GLOBAL, &nameless).unwrap_err().to_string();
        assert!(err.contains("--project=p"), "{err}");

        let failed = Operation {
            status: Some("DONE".to_string()),
            error: Some(crate::models::OperationError {
                errors: vec![crate::models::OperationErrorItem {
                    code: Some("QUOTA_EXCEEDED".to_string()),
                    message: Some("out of quota".to_string()),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        let err = super::pending_operation_name("p", GLOBAL, &failed).unwrap_err().to_string();
        assert!(err.contains("QUOTA_EXCEEDED: out of quota"), "{err}");
    }

    #[fuchsia::test]
    fn test_operation_scope_list_command() {
        let named = Operation { name: Some("operation-123".to_string()), ..Default::default() };
        assert_eq!(
            GLOBAL.list_command("my-proj", &named),
            "gcloud compute operations list --project=my-proj --global \
             --filter=\"name=operation-123\""
        );

        // Without a name, the target resource is the only handle the user has on the operation.
        let nameless = Operation {
            target_link: Some("https://compute.googleapis.com/vm".to_string()),
            ..Default::default()
        };
        assert_eq!(
            OperationScope::Zone("us-central1-a").list_command("my-proj", &nameless),
            "gcloud compute operations list --project=my-proj --zones=us-central1-a \
             --filter=\"targetLink=https://compute.googleapis.com/vm\""
        );
    }

    #[fuchsia::test]
    fn test_create_firewall_rule_command() {
        let rule = FirewallRule {
            name: "allow-ssh-ingress-default".to_string(),
            network: Some("global/networks/default".to_string()),
            source_ranges: vec!["172.253.30.0/23".to_string()],
            allowed: vec![FirewallAllowed {
                ip_protocol: "tcp".to_string(),
                ports: vec!["22".to_string()],
            }],
            direction: Some("INGRESS".to_string()),
            priority: Some(1000),
        };
        assert_eq!(
            super::create_firewall_rule_command("my-proj", &rule),
            "gcloud compute firewall-rules create allow-ssh-ingress-default --project=my-proj \
             --network=global/networks/default --direction=INGRESS \
             --source-ranges=172.253.30.0/23 --allow=tcp:22"
        );
    }

    #[fuchsia::test]
    fn test_remediation_hint() {
        assert!(
            super::remediation_hint(Api::Compute, StatusCode::UNAUTHORIZED)
                .unwrap()
                .contains("ffx auth generate")
        );
        assert!(
            super::remediation_hint(Api::Compute, StatusCode::FORBIDDEN)
                .unwrap()
                .contains("compute.googleapis.com")
        );
        assert!(
            super::remediation_hint(Api::Storage, StatusCode::FORBIDDEN)
                .unwrap()
                .contains("globally unique")
        );
        assert_eq!(super::remediation_hint(Api::Compute, StatusCode::NOT_FOUND), None);

        // Endpoints declare which API serves them, so the hint does not depend on the URL.
        assert_eq!(endpoints::images("my-proj").unwrap().api, Api::Compute);
        assert_eq!(endpoints::upload_gcs_object("b", "o").unwrap().api, Api::Storage);
    }
}
