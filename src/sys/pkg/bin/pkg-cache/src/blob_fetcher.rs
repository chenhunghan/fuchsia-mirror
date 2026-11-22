// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use blobfs::BlobStatus;
use fidl_fuchsia_pkg as fpkg;
use fidl_fuchsia_pkg_ext as pkg;
use fidl_fuchsia_pkg_http as fpkg_http;
use fuchsia_async::TimeoutExt as _;
use fuchsia_backoff::Backoff as _;
use fuchsia_inspect as finspect;
use fuchsia_inspect::NumericProperty as _;
use fuchsia_sync::Mutex;
use fuchsia_trace as ftrace;
use futures::future::{Either, FutureExt as _};
use http_uri_ext::HttpUriExt as _;
use log::warn;
use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;

mod queue;
mod retry;

use queue::WorkQueue;
pub(crate) use queue::{Cancelled, Priority};

const INSPECT_RECENT_FETCH_COUNT: usize = 25;

#[derive(Clone, Copy, Debug, typed_builder::TypedBuilder)]
pub(crate) struct Params {
    header_network_timeout: zx::BootDuration,
    body_network_timeout: zx::BootDuration,
    download_resumption_attempts_limit: u32,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub(crate) struct FetchParams {
    blob_base_url: http::Uri,
    conflict_behavior: ConflictBehavior,
}

/// How the blob fetcher should behave when asked to fetch a blob that is already in blobfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConflictBehavior {
    /// Ask blobfs if the existing blob should be overwritten, if not skip the fetch.
    AskBlobfs,
    /// Always perform the fetch and overwrite the existing blob.
    Overwrite,
}

impl FetchParams {
    pub(crate) fn new(blob_base_url: http::Uri, conflict_behavior: ConflictBehavior) -> Self {
        Self { blob_base_url, conflict_behavior }
    }
}

type FetchResult = Result<Option<u64>, Arc<FetchError>>;

/// A clonable handle to the prioritized blob fetch queue.
#[derive(Clone, Debug)]
pub struct BlobFetcher {
    queue: WorkQueue<pkg::BlobId, FetchParams, ftrace::Id, FetchResult>,
}

impl BlobFetcher {
    /// Creates an unbounded queue that fetches up to `max_concurrency` blobs at once.
    ///
    /// # Panics
    ///
    /// Panics if called outside the context of an executor.
    pub(crate) fn new(
        max_concurrency: NonZeroUsize,
        params: Params,
        blobfs_client: blobfs::Client,
        http_client: impl fidl_connector::Connect<Proxy = fpkg_http::ClientProxy>
        + Clone
        + Send
        + Sync
        + 'static,
        inspect: finspect::Node,
    ) -> Self {
        let weak_inspect = inspect.clone_weak();
        let inspect = Arc::new(Inspect::new(inspect));
        let queue = WorkQueue::new(
            max_concurrency,
            move |blob_id: pkg::BlobId,
                  fetch_params: FetchParams,
                  parent_trace_id: ftrace::Id,
                  mut cancel: queue::CancelSignal| {
                let http_client = http_client.clone();
                let blobfs_client = blobfs_client.clone();
                let inspect = Arc::clone(&inspect);
                async move {
                    let trace_id = ftrace::Id::new();
                    let guard = ftrace::async_enter!(
                        trace_id,
                        c"app",
                        c"fetch blob",
                        // Async tracing does not support multiple concurrent child durations, so we
                        // create a new top-level duration and attach the parent duration as
                        // metadata.
                        "parent_trace_id" => u64::from(parent_trace_id),
                        "hash" => blob_id.to_string().as_str()
                    );
                    let inspect_fetch = inspect.start_fetch(&blob_id, &fetch_params);
                    let res = fetch_blob_with_retry(
                        blob_id,
                        fetch_params,
                        params,
                        &blobfs_client,
                        &http_client,
                        &inspect_fetch,
                        trace_id,
                        &mut cancel,
                    )
                    .await
                    .map_err(Arc::new);
                    // Cancellation only makes the fetch stop early. It still runs until it returns
                    // so that its `BlobWriter` is closed before the queue starts another fetch of
                    // the blob, and the queue discards whatever a cancelled fetch returns.
                    let res = match cancel.check() {
                        Some(cancelled) => Err(cancelled),
                        None => Ok(res),
                    };
                    inspect.end_fetch(inspect_fetch, &res);
                    if let Some(o) = guard {
                        o.end(&[ftrace::ArgValue::of("result", format!("{res:?}").as_str())])
                    }
                    res
                }
            },
        );
        weak_inspect.record_lazy_child("queue", queue.record_lazy_inspect());
        Self { queue }
    }

    #[cfg(test)]
    pub(crate) fn new_closed() -> Self {
        Self {
            queue: WorkQueue::new(
                NonZeroUsize::MIN,
                |_blob_id, _params, _trace_id, _cancel| async {
                    unreachable!("unexpected blob fetch in closed BlobFetcher")
                },
            ),
        }
    }

    /// Allocates a new [`Priority`] with lower priority than any previously allocated [`Priority`].
    pub(crate) fn new_priority(&self) -> Priority {
        self.queue.new_priority()
    }

    /// Enqueues `blob_id` at `priority`.
    /// On success, returns the number of bytes downloaded (not necessarily equal to the
    /// uncompressed blob size), or `None` if the blob did not need to be downloaded.
    /// Dropping the returned future cancels the fetch if pending or unshared.
    pub(crate) fn push(
        &self,
        priority: Priority,
        blob_id: pkg::BlobId,
        params: FetchParams,
        parent_trace_id: ftrace::Id,
    ) -> impl Future<Output = Result<FetchResult, Cancelled>> + use<> {
        self.queue.push(priority, blob_id, params, parent_trace_id)
    }

    /// Enqueues all `blob_ids` at `priority`, returning futures that resolve to each result.
    /// Dropping an unresolved future cancels that fetch if pending or unshared.
    pub(crate) fn push_all<I: IntoIterator<Item = pkg::BlobId>>(
        &self,
        priority: Priority,
        blob_ids: I,
        params: FetchParams,
        parent_trace_id: ftrace::Id,
    ) -> impl Iterator<Item = impl Future<Output = Result<FetchResult, Cancelled>> + use<I>> + use<I>
    {
        self.queue.push_all(
            priority,
            blob_ids.into_iter().map(move |id| (id, params.clone(), parent_trace_id)),
        )
    }
}

/// On success, returns the number of bytes downloaded, which is not necessarily equal to the
/// uncompressed size of the blob, or None if the blob did not need to be downloaded.
///
/// Once `cancel` is signalled the fetch stops as soon as it can and its result is meaningless.
async fn fetch_blob_with_retry(
    blob_id: pkg::BlobId,
    FetchParams { blob_base_url, conflict_behavior }: FetchParams,
    params: Params,
    blobfs_client: &blobfs::Client,
    http_client: &impl fidl_connector::Connect<Proxy = fpkg_http::ClientProxy>,
    inspect: &InspectFetch,
    trace_id: ftrace::Id,
    cancel: &mut queue::CancelSignal,
) -> Result<Option<u64>, FetchError> {
    match conflict_behavior {
        ConflictBehavior::AskBlobfs => {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"pre-check presence");
            if blobfs_client.blob_present_and_up_to_date(&blob_id.into()).await {
                return Ok(None);
            }
        }
        ConflictBehavior::Overwrite => (),
    }
    let error_base = blob_base_url.clone();
    let blob_url = blob_base_url
        .extend_dir_with_path(&blob_id.to_string())
        .map_err(|source| FetchError::BlobUrl { source, base_url: error_base, blob_id })?;

    // Equivalent to `fuchsia_backoff::retry_or_first_error`, except that cancellation ends the
    // backoff early.
    let mut backoff = retry::blob_fetch();
    let mut first_error = None;
    loop {
        let err = match fetch_blob_attempt(
            blob_id,
            &blob_url,
            params,
            blobfs_client,
            http_client,
            inspect,
            trace_id,
            cancel,
        )
        .await
        {
            Ok(download_size) => return Ok(Some(download_size)),
            Err(err) => err,
        };
        // http-client fails a cancelled download with `Canceled`, which is not retried.
        let delay = backoff.next_backoff(&err);
        let err = first_error.take().unwrap_or(err);
        let Some(delay) = delay else {
            return Err(err);
        };
        if cancel.wait().map(|_: Cancelled| true).on_timeout(delay, || false).await {
            return Err(err);
        }
        first_error = Some(err);
    }
}

async fn fetch_blob_attempt(
    blob_id: pkg::BlobId,
    blob_url: &http::Uri,
    Params {
        header_network_timeout,
        body_network_timeout,
        download_resumption_attempts_limit,
    }: Params,
    blobfs_client: &blobfs::Client,
    http_client: &impl fidl_connector::Connect<Proxy = fpkg_http::ClientProxy>,
    inspect: &InspectFetch,
    trace_id: ftrace::Id,
    cancel: &mut queue::CancelSignal,
) -> Result<u64, FetchError> {
    inspect.attempt();
    let blob = {
        let _guard = ftrace::async_enter!(trace_id, c"app", c"open blob");
        blobfs_client
            .open_blob_for_write(&blob_id.into(), true)
            .await
            .map_err(FetchError::CreateBlob)?
    };
    let guard = ftrace::async_enter!(trace_id, c"app", c"download blob");
    let res = download_blob(
        http_client,
        blob_url,
        blob,
        header_network_timeout,
        body_network_timeout,
        download_resumption_attempts_limit,
        cancel,
    )
    .await;
    if let Some(o) = guard {
        o.end(&[ftrace::ArgValue::of("result", format!("{res:?}").as_str())])
    }
    let download_size = res?;
    {
        let _guard = ftrace::async_enter!(trace_id, c"app", c"post-check presence");
        // Recheck presence to catch the client lying about writing the blob, but accept
        // NeedsOverwrite (unlike in the pre-write check) because the incoming blob may not
        // have been intended by the remote blobstore to pass blobfs' up-to-date
        // requirements. This check can be tightened to UpToDate if we implement blob format
        // negotiation.
        match blobfs_client
            .blob_status(&blob_id.into())
            .await
            .map_err(FetchError::PostWriteStatusCheck)?
        {
            BlobStatus::UpToDate | BlobStatus::NeedsOverwrite => (),
            BlobStatus::Absent => {
                return Err(FetchError::BlobAbsentAfterWrite);
            }
        }
    }
    Ok(download_size)
}

async fn download_blob(
    http_client: &impl fidl_connector::Connect<Proxy = fpkg_http::ClientProxy>,
    blob_url: &http::Uri,
    blob: fidl::endpoints::ClientEnd<fidl_fuchsia_fxfs::BlobWriterMarker>,
    header_network_timeout: zx::BootDuration,
    body_network_timeout: zx::BootDuration,
    download_resumption_attempts_limit: u32,
    cancel: &mut queue::CancelSignal,
) -> Result<u64, FetchError> {
    let (cancel_tx, cancel_rx) = zx::EventPair::create();
    let proxy = http_client.connect().map_err(FetchError::ConnectToHttpClient)?;
    let download_fut = proxy.download_blob(
        &blob_url.to_string(),
        blob,
        header_network_timeout.into_nanos(),
        body_network_timeout.into_nanos(),
        download_resumption_attempts_limit,
        Some(cancel_rx),
    );
    let res = match futures::future::select(pin!(cancel.wait()), download_fut).await {
        Either::Left((Cancelled { .. }, download_fut)) => {
            // Tell http-client to stop, then wait for its reply, which it sends after closing
            // `blob`.
            drop(cancel_tx);
            download_fut.await
        }
        Either::Right((res, _)) => res,
    };
    res.map_err(FetchError::DownloadBlobFidl)?.map_err(FetchError::DownloadBlob)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    #[error("could not create blob")]
    CreateBlob(#[source] blobfs::CreateError),

    #[error("creating blob url from base: {base_url}, id: {blob_id}")]
    BlobUrl {
        #[source]
        source: http_uri_ext::Error,
        base_url: http::Uri,
        blob_id: pkg::BlobId,
    },

    #[error("connecting to fuchsia.pkg.http.Client")]
    ConnectToHttpClient(#[source] anyhow::Error),

    #[error("FIDL error while calling fuchsia.pkg.http.Client.DownloadBlob")]
    DownloadBlobFidl(#[source] fidl::Error),

    #[error("fuchsia.pkg.http.Client.DownloadBlob failed: {0:?}")]
    DownloadBlob(fpkg_http::ClientDownloadBlobError),

    #[error("checking blob status after write")]
    PostWriteStatusCheck(#[source] blobfs::BlobfsError),

    #[error("blob was not in blobfs after successful write")]
    BlobAbsentAfterWrite,
}

impl FetchError {
    fn kind(&self) -> FetchErrorKind {
        use FetchError::*;
        match self {
            DownloadBlob(e) => match e {
                fpkg_http::ClientDownloadBlobError::NetworkRateLimit => {
                    FetchErrorKind::NetworkRateLimit
                }
                fpkg_http::ClientDownloadBlobError::Network => FetchErrorKind::Network,
                fpkg_http::ClientDownloadBlobError::NotFound => FetchErrorKind::NotFound,
                fpkg_http::ClientDownloadBlobError::NoSpace
                | fpkg_http::ClientDownloadBlobError::Other
                | fpkg_http::ClientDownloadBlobError::Canceled => FetchErrorKind::Other,
            },
            CreateBlob { .. }
            | BlobUrl { .. }
            | ConnectToHttpClient(_)
            | DownloadBlobFidl { .. }
            | PostWriteStatusCheck { .. }
            | BlobAbsentAfterWrite => FetchErrorKind::Other,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchErrorKind {
    NetworkRateLimit,
    Network,
    NotFound,
    Other,
}

impl From<&FetchError> for fpkg::ResolveError {
    fn from(err: &FetchError) -> Self {
        use FetchError::*;
        use fidl_fuchsia_pkg::ResolveError as Err;
        match err {
            CreateBlob(blobfs::CreateError::OutOfSpace) => Err::NoSpace,
            CreateBlob(_) => Err::Io,
            BlobUrl { .. } => Err::Internal,
            ConnectToHttpClient(_) => Err::Io,
            DownloadBlobFidl { .. } => Err::Internal,
            DownloadBlob(e) => match e {
                fpkg_http::ClientDownloadBlobError::NoSpace => Err::NoSpace,
                fpkg_http::ClientDownloadBlobError::Network => Err::UnavailableBlob,
                fpkg_http::ClientDownloadBlobError::NotFound => Err::UnavailableBlob,
                fpkg_http::ClientDownloadBlobError::NetworkRateLimit => Err::Io,
                fpkg_http::ClientDownloadBlobError::Other => Err::Io,
                fpkg_http::ClientDownloadBlobError::Canceled => Err::Internal,
            },
            PostWriteStatusCheck { .. } => Err::Internal,
            BlobAbsentAfterWrite => Err::Internal,
        }
    }
}

struct Inspect {
    active: finspect::Node,
    fetch_count: std::sync::atomic::AtomicU64,
    recent: Mutex<fuchsia_inspect_contrib::nodes::BoundedListNode>,
    _node: finspect::Node,
}

impl Inspect {
    fn new(node: finspect::Node) -> Self {
        Self {
            active: node.create_child("active"),
            fetch_count: std::sync::atomic::AtomicU64::new(0),
            recent: Mutex::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                node.create_child("recent"),
                INSPECT_RECENT_FETCH_COUNT,
            )),
            _node: node,
        }
    }

    fn start_fetch(&self, blob_id: &pkg::BlobId, params: &FetchParams) -> InspectFetch {
        let FetchParams { blob_base_url, conflict_behavior } = params;
        let node = self.active.create_child(
            self.fetch_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string(),
        );
        node.record_int("start_boot_ns", zx::BootInstant::get().into_nanos());
        node.record_string("hash", blob_id.to_string());
        node.record_string("base_url", blob_base_url.to_string());
        node.record_string("conflict_behavior", format!("{conflict_behavior:?}"));
        InspectFetch { attempts: node.create_uint("attempts", 0), node }
    }

    fn end_fetch(&self, fetch: InspectFetch, res: &Result<FetchResult, Cancelled>) {
        let InspectFetch { attempts, node } = fetch;
        node.record_int("end_boot_ns", zx::BootInstant::get().into_nanos());
        node.record_string(
            "result",
            match res {
                Ok(Ok(Some(download_size))) => {
                    Cow::Owned(format!("success: downloaded {download_size} bytes"))
                }
                Ok(Ok(None)) => Cow::Borrowed("success: download not necessary"),
                Ok(Err(e)) => Cow::Owned(format!("error: {}", crate::stringify_error(e.as_ref()))),
                Err(Cancelled { .. }) => Cow::Borrowed("cancelled"),
            },
        );
        node.record(attempts);
        let () =
            self.recent.lock().adopt_entry(node).map(|_: &finspect::Node| ()).unwrap_or_else(|e| {
                warn!("failed to move inspect node to recent: {:#}", anyhow!(e))
            });
    }
}

struct InspectFetch {
    attempts: finspect::UintProperty,
    node: finspect::Node,
}

impl InspectFetch {
    fn attempt(&self) {
        self.attempts.add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fidl_fuchsia_fxfs as ffxfs;
    use fuchsia_async as fasync;
    use fuchsia_hash::Hash;
    use futures::channel::oneshot;
    use futures::stream::StreamExt as _;
    use std::task::Poll;

    const TEST_TIMEOUT: zx::BootDuration = zx::BootDuration::from_seconds(30);
    const TEST_CONCURRENCY_LIMIT: NonZeroUsize = NonZeroUsize::new(2).unwrap();
    const TEST_RESUMPTION_LIMIT: u32 = 0;
    const TEST_DOWNLOADED_BYTES: u64 = 64;
    const TEST_BLOB_HASH_BYTE: u8 = 0xab;
    const HASH_BYTE_LEN: usize = 32;
    const TEST_BASE_URL: &str = "http://example.org/blobs/1";
    const TEST_BLOB_URL: &str = "http://example.org/blobs/1/deadbeef";

    #[derive(Clone)]
    struct FixedProxyConnector(fpkg_http::ClientProxy);

    impl fidl_connector::Connect for FixedProxyConnector {
        type Proxy = fpkg_http::ClientProxy;

        fn connect(&self) -> Result<Self::Proxy, anyhow::Error> {
            Ok(self.0.clone())
        }
    }

    fn test_params() -> Params {
        Params::builder()
            .header_network_timeout(TEST_TIMEOUT)
            .body_network_timeout(TEST_TIMEOUT)
            .download_resumption_attempts_limit(TEST_RESUMPTION_LIMIT)
            .build()
    }

    #[fuchsia::test]
    async fn download_blob_cancel_signals_eventpair_and_awaits_response() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
        let connector = FixedProxyConnector(proxy);
        let (blob_client_end, _blob_server_end) =
            fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut cancel_signal = queue::CancelSignal::new(cancel_rx);
        let uri: http::Uri = TEST_BLOB_URL.parse().unwrap();

        let mut download_fut = pin!(download_blob(
            &connector,
            &uri,
            blob_client_end,
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            TEST_RESUMPTION_LIMIT,
            &mut cancel_signal,
        ));
        assert!(futures::poll!(&mut download_fut).is_pending());

        let (cancel_eventpair, responder) = match stream.next().await.unwrap().unwrap() {
            fpkg_http::ClientRequest::DownloadBlob { cancel, responder, .. } => {
                (cancel.expect("cancel handle provided"), responder)
            }
        };
        assert!(
            fasync::OnSignals::new(&cancel_eventpair, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .now_or_never()
                .is_none()
        );

        // Triggering `CancelSignal` drops `cancel_tx` (signaling `EVENTPAIR_PEER_CLOSED`) while
        // keeping `download_blob` pending until `http-client` sends its FIDL reply.
        drop(cancel_tx);
        assert!(futures::poll!(&mut download_fut).is_pending());

        let signals = fasync::OnSignals::new(&cancel_eventpair, zx::Signals::EVENTPAIR_PEER_CLOSED)
            .now_or_never()
            .expect("PEER_CLOSED already signaled")
            .unwrap();
        assert!(signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED));

        let () = responder.send(Err(fpkg_http::ClientDownloadBlobError::Canceled)).unwrap();
        std::assert_matches!(
            download_fut.await,
            Err(FetchError::DownloadBlob(fpkg_http::ClientDownloadBlobError::Canceled))
        );
    }

    #[fuchsia::test]
    fn unsolicited_canceled_from_http_client_fails_fetch_without_retry() {
        let mut executor = fasync::TestExecutor::new();
        let (blobfs_client, _dir_stream, _reader_stream, mut creator_stream) =
            blobfs::Client::new_test();
        let (http_proxy, mut http_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
        let inspector = finspect::Inspector::default();
        let fetcher = BlobFetcher::new(
            TEST_CONCURRENCY_LIMIT,
            test_params(),
            blobfs_client,
            FixedProxyConnector(http_proxy),
            inspector.root().create_child("blob_fetcher"),
        );

        let hash = Hash::from([TEST_BLOB_HASH_BYTE; HASH_BYTE_LEN]);
        let blob_id = pkg::BlobId::from(hash);
        let base_url: http::Uri = TEST_BASE_URL.parse().unwrap();

        let mut fetch_fut = pin!(fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url, ConflictBehavior::Overwrite),
            ftrace::Id::new(),
        ));
        match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create { responder, .. }))) => {
                let (writer_client, _writer_server) =
                    fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
                responder.send(Ok(writer_client)).unwrap();
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        }
        match executor.run_until_stalled(&mut http_stream.next()) {
            Poll::Ready(Some(Ok(fpkg_http::ClientRequest::DownloadBlob { responder, .. }))) => {
                responder.send(Err(fpkg_http::ClientDownloadBlobError::Canceled)).unwrap();
            }
            other => panic!("unexpected http_stream request: {other:?}"),
        }

        // The fetch was not cancelled, so its request receives the error.
        std::assert_matches!(
            executor.run_until_stalled(&mut fetch_fut),
            Poll::Ready(Ok(Err(err)))
                if matches!(
                    err.as_ref(),
                    FetchError::DownloadBlob(fpkg_http::ClientDownloadBlobError::Canceled)
                )
        );
    }

    #[fuchsia::test]
    fn cancel_during_open_blob_for_write_awaits_create_and_cancelled_download_before_overwrite() {
        let mut executor = fasync::TestExecutor::new();
        let (blobfs_client, _dir_stream, _reader_stream, mut creator_stream) =
            blobfs::Client::new_test();
        let (http_proxy, mut http_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
        let inspector = finspect::Inspector::default();
        let fetcher = BlobFetcher::new(
            TEST_CONCURRENCY_LIMIT,
            test_params(),
            blobfs_client,
            FixedProxyConnector(http_proxy),
            inspector.root().create_child("blob_fetcher"),
        );

        let hash = Hash::from([TEST_BLOB_HASH_BYTE; HASH_BYTE_LEN]);
        let blob_id = pkg::BlobId::from(hash);
        let base_url: http::Uri = TEST_BASE_URL.parse().unwrap();

        // Start an `AskBlobfs` fetch and suspend it at `BlobCreator.Create`.
        let ask_fut = fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url.clone(), ConflictBehavior::AskBlobfs),
            ftrace::Id::new(),
        );
        match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::NeedsOverwrite {
                blob_hash,
                responder,
            }))) => {
                assert_eq!(Hash::from(blob_hash), hash);
                responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        }
        let first_create_responder = match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create {
                hash: req_hash,
                allow_existing,
                responder,
            }))) => {
                assert_eq!(Hash::from(req_hash), hash);
                assert!(allow_existing);
                responder
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        };

        // Queue a subsequent `Overwrite` fetch for the same blob and cancel the first fetch while
        // `BlobCreator.Create` is still in flight.
        let mut overwrite_fut = pin!(fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url, ConflictBehavior::Overwrite),
            ftrace::Id::new(),
        ));
        drop(ask_fut);

        // Per-blob serialization must remain held until the in-flight `Create` finishes (and its
        // `BlobWriter` is closed); otherwise `Create` #2 could be handled by blobfs while `Create`
        // #1's `ClientEnd<BlobWriter>` still sits unread in the `BlobCreator` channel, keeping the
        // first writer alive and failing the `Overwrite` fetch with `ALREADY_EXISTS`.
        assert!(executor.run_until_stalled(&mut overwrite_fut).is_pending());
        assert!(executor.run_until_stalled(&mut creator_stream.next()).is_pending());

        let (first_writer_client, _first_writer_server) =
            fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
        first_create_responder.send(Ok(first_writer_client)).unwrap();

        // The cancelled fetch passes the writer on to `http-client` with `cancel` already
        // signalled. Per-blob serialization must remain held until `http-client`, which closes the
        // writer first, replies.
        let (first_cancel, first_download_responder) =
            match executor.run_until_stalled(&mut http_stream.next()) {
                Poll::Ready(Some(Ok(fpkg_http::ClientRequest::DownloadBlob {
                    cancel,
                    responder,
                    ..
                }))) => (cancel.expect("cancel handle provided"), responder),
                other => panic!("unexpected http_stream request: {other:?}"),
            };
        std::assert_matches!(
            executor.run_until_stalled(&mut pin!(fasync::OnSignals::new(
                &first_cancel,
                zx::Signals::EVENTPAIR_PEER_CLOSED,
            ))),
            Poll::Ready(Ok(signals)) if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );
        assert!(executor.run_until_stalled(&mut creator_stream.next()).is_pending());
        first_download_responder.send(Err(fpkg_http::ClientDownloadBlobError::Canceled)).unwrap();
        let second_create_responder = match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create {
                hash: req_hash,
                allow_existing,
                responder,
            }))) => {
                assert_eq!(Hash::from(req_hash), hash);
                assert!(allow_existing);
                responder
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        };

        // Complete the `Overwrite` fetch normally.
        let (second_writer_client, _second_writer_server) =
            fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
        second_create_responder.send(Ok(second_writer_client)).unwrap();
        let download_responder = match executor.run_until_stalled(&mut http_stream.next()) {
            Poll::Ready(Some(Ok(fpkg_http::ClientRequest::DownloadBlob { responder, .. }))) => {
                responder
            }
            other => panic!("unexpected http_stream request: {other:?}"),
        };
        download_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::NeedsOverwrite {
                blob_hash,
                responder,
            }))) => {
                assert_eq!(Hash::from(blob_hash), hash);
                responder.send(Ok(false)).unwrap();
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        }
        std::assert_matches!(
            executor.run_until_stalled(&mut overwrite_fut),
            Poll::Ready(Ok(Ok(Some(TEST_DOWNLOADED_BYTES))))
        );
    }

    #[fuchsia::test]
    fn cancel_during_retry_backoff_aborts_without_waiting_for_timer() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        let (blobfs_client, _dir_stream, _reader_stream, mut creator_stream) =
            blobfs::Client::new_test();
        let (http_proxy, mut http_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
        let inspector = finspect::Inspector::default();
        let fetcher = BlobFetcher::new(
            TEST_CONCURRENCY_LIMIT,
            test_params(),
            blobfs_client,
            FixedProxyConnector(http_proxy),
            inspector.root().create_child("blob_fetcher"),
        );

        let hash = Hash::from([TEST_BLOB_HASH_BYTE; HASH_BYTE_LEN]);
        let blob_id = pkg::BlobId::from(hash);
        let base_url: http::Uri = TEST_BASE_URL.parse().unwrap();

        let mut fetch_fut = fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url.clone(), ConflictBehavior::Overwrite),
            ftrace::Id::new(),
        );
        match executor.run_until_stalled(&mut creator_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create { responder, .. }))) => {
                let (writer_client, _writer_server) =
                    fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
                responder.send(Ok(writer_client)).unwrap();
            }
            other => panic!("unexpected creator_stream request: {other:?}"),
        }
        match executor.run_until_stalled(&mut http_stream.next()) {
            Poll::Ready(Some(Ok(fpkg_http::ClientRequest::DownloadBlob { responder, .. }))) => {
                responder.send(Err(fpkg_http::ClientDownloadBlobError::NetworkRateLimit)).unwrap();
            }
            other => panic!("unexpected http_stream request: {other:?}"),
        }

        // Attempt 1 failed with 429 (`NetworkRateLimit`) and is now sleeping in the backoff timer.
        assert!(executor.run_until_stalled(&mut fetch_fut).is_pending());
        assert!(executor.run_until_stalled(&mut creator_stream.next()).is_pending());
        assert!(fasync::TestExecutor::next_timer().is_some());
        assert!(!executor.wake_expired_timers());

        // Dropping `fetch_fut` cancels the backoff sleep immediately without advancing fake time,
        // allowing a subsequent fetch for the same blob to start right away.
        drop(fetch_fut);
        let _next_fut = fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url, ConflictBehavior::Overwrite),
            ftrace::Id::new(),
        );
        std::assert_matches!(
            executor.run_until_stalled(&mut creator_stream.next()),
            Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create { .. })))
        );
        assert_eq!(fasync::TestExecutor::next_timer(), None);
    }

    #[fuchsia::test]
    fn cancel_while_queued_does_not_record_inspect_fetch() {
        let mut executor = fasync::TestExecutor::new();
        let (blobfs_client, _dir_stream, _reader_stream, _creator_stream) =
            blobfs::Client::new_test();
        let (http_proxy, _http_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
        let inspector = finspect::Inspector::default();
        let fetcher = BlobFetcher::new(
            TEST_CONCURRENCY_LIMIT,
            test_params(),
            blobfs_client,
            FixedProxyConnector(http_proxy),
            inspector.root().create_child("blob_fetcher"),
        );

        let hash = Hash::from([TEST_BLOB_HASH_BYTE; HASH_BYTE_LEN]);
        let blob_id = pkg::BlobId::from(hash);
        let base_url: http::Uri = TEST_BASE_URL.parse().unwrap();

        // `push` only queues the request; dropping `fetch_fut` removes it before the queue starts
        // a task for it.
        let fetch_fut = fetcher.push(
            fetcher.new_priority(),
            blob_id,
            FetchParams::new(base_url, ConflictBehavior::Overwrite),
            ftrace::Id::new(),
        );
        drop(fetch_fut);

        assert!(executor.run_until_stalled(&mut std::future::pending::<()>()).is_pending());
        assert_eq!(
            executor.run_until_stalled(&mut pin!(async {
                assert_data_tree!(
                    inspector,
                    root: {
                        blob_fetcher: {
                            active: {},
                            queue: {
                                length: 0u64,
                            },
                            recent: {},
                        }
                    }
                );
            })),
            Poll::Ready(())
        );
    }
}
