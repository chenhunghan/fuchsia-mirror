// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_pkg as fpkg;
use fuchsia_inspect as finspect;
use fuchsia_inspect::NumericProperty as _;
use fuchsia_trace as ftrace;
use futures::stream::{FuturesUnordered, StreamExt as _};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PackageFetcher {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    package_index: Arc<crate::index::PackageIndex>,
    blobfs_client: blobfs::Client,
    blob_fetcher: crate::blob_fetcher::BlobFetcher,
    root_dir_factory: crate::root_dir::RootDirFactory,
    open_packages: crate::RootDirCache,
    inspect: finspect::Node,
    fetch_count: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
enum MaybeCachedRootDir {
    Uncached(Arc<crate::RootDir>),
    Cached(Arc<crate::CachedRootDir>),
}

impl PackageFetcher {
    pub(crate) fn new(
        package_index: Arc<crate::index::PackageIndex>,
        blobfs_client: blobfs::Client,
        blob_fetcher: crate::blob_fetcher::BlobFetcher,
        root_dir_factory: crate::root_dir::RootDirFactory,
        open_packages: crate::RootDirCache,
        inspect: finspect::Node,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                package_index,
                blobfs_client,
                blob_fetcher,
                root_dir_factory,
                open_packages,
                inspect,
                fetch_count: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub(crate) async fn fetch_with_open_package_tracking(
        &self,
        pkg_id: fuchsia_hash::Hash,
        blob_source: http::Uri,
        trace_id: ftrace::Id,
    ) -> Result<Arc<crate::CachedRootDir>, Error> {
        match self
            .fetch(pkg_id, blob_source, fpkg::GcProtection::OpenPackageTracking, trace_id)
            .await?
        {
            MaybeCachedRootDir::Cached(root_dir) => Ok(root_dir),
            MaybeCachedRootDir::Uncached(_) => {
                unreachable!("fetch with OpenPackageTracking always returns Cached")
            }
        }
    }

    pub(crate) async fn fetch_retained(
        &self,
        pkg_id: fuchsia_hash::Hash,
        blob_source: http::Uri,
        trace_id: ftrace::Id,
    ) -> Result<Arc<crate::RootDir>, Error> {
        match self.fetch(pkg_id, blob_source, fpkg::GcProtection::Retained, trace_id).await? {
            MaybeCachedRootDir::Uncached(root_dir) => Ok(root_dir),
            MaybeCachedRootDir::Cached(_) => {
                unreachable!("fetch with Retained always returns Uncached")
            }
        }
    }

    async fn fetch(
        &self,
        pkg_id: fuchsia_hash::Hash,
        blob_source: http::Uri,
        gc_protection: fpkg::GcProtection,
        trace_id: ftrace::Id,
    ) -> Result<MaybeCachedRootDir, Error> {
        let inspect = self.inner.inspect.create_child(
            self.inner.fetch_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string(),
        );
        fetch_impl(
            pkg_id,
            blob_source,
            gc_protection,
            self.inner.package_index.as_ref(),
            &self.inner.blobfs_client,
            &self.inner.blob_fetcher,
            &self.inner.root_dir_factory,
            &self.inner.open_packages,
            inspect,
            trace_id,
        )
        .await
    }
}

async fn fetch_impl(
    pkg_id: fuchsia_hash::Hash,
    blob_source: http::Uri,
    gc_protection: fpkg::GcProtection,
    package_index: &crate::index::PackageIndex,
    blobfs_client: &blobfs::Client,
    blob_fetcher: &crate::blob_fetcher::BlobFetcher,
    root_dir_factory: &crate::root_dir::RootDirFactory,
    open_packages: &crate::RootDirCache,
    inspect: finspect::Node,
    trace_id: ftrace::Id,
) -> Result<MaybeCachedRootDir, Error> {
    let _trace_guard = ftrace::async_enter!(
        trace_id,
        c"app",
        c"fetch",
        "hash" => pkg_id.to_string().as_str(),
        "gc_protection" => format!("{gc_protection:?}").as_str()
    );
    // Acquired before pushing any `WorkFuture`s so blob fetches drop (detaching/cancelling) before
    // writing protection is removed if this future errors or is cancelled.
    let _writing_guard = package_index.start_writing(pkg_id, gc_protection).await;
    let priority = blob_fetcher.new_priority();
    inspect.record_int("start_boot_ns", zx::BootInstant::get().into_nanos());
    inspect.record_string("package_hash", pkg_id.to_string());
    inspect.record_string("gc_protection", format!("{gc_protection:?}"));
    inspect.record_string("blob_source", blob_source.to_string());
    let inspect_outstanding = inspect.create_uint("known_outstanding_blobs", 1);
    let mut queue = std::collections::VecDeque::from([pkg_id]);
    let mut queued = HashSet::from([pkg_id]);
    let mut all_blobs = HashSet::new();
    let fetch_params = crate::blob_fetcher::FetchParams::new(
        blob_source,
        crate::blob_fetcher::ConflictBehavior::AskBlobfs,
    );
    let mut ret = None;
    while let Some(blob_id) = queue.pop_front() {
        // The blob fetcher performs this check as well, but check here to avoid blocking the fetch
        // of an already cached package on a full blob fetch queue.
        let present_and_up_to_date = {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"check meta.far presence");
            blobfs_client.blob_present_and_up_to_date(&blob_id).await
        };
        if !present_and_up_to_date {
            let _guard = ftrace::async_enter!(
                trace_id, c"app", c"fetch meta.far", "hash" => blob_id.to_string().as_str()
            );
            let _: Option<u64> = blob_fetcher
                .push(priority, blob_id.into(), fetch_params.clone(), trace_id)
                .await
                .map_err(Error::BlobPush)?
                .map_err(Error::BlobFetch)?;
        }
        inspect_outstanding.subtract(1);
        let root_dir = {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"create root dir");
            root_dir_factory
                .create(blob_id)
                .await
                .map_err(|source| Error::CreatingRootDir { source, pkg: blob_id })?
        };
        let subpackages = root_dir
            .subpackages()
            .await
            .map_err(|source| Error::ReadingSubpackages { source, pkg: blob_id })?
            .into_hashes_undeduplicated()
            .collect::<HashSet<_>>();
        all_blobs.extend(&subpackages);
        let () = package_index
            .add_blobs(pkg_id, subpackages.clone(), gc_protection)
            .await
            .map_err(Error::ProtectBlobs)?;
        for sub in subpackages {
            if queued.insert(sub) {
                queue.push_back(sub);
                inspect_outstanding.add(1);
            }
        }
        let content = root_dir.external_file_hashes().copied().collect::<HashSet<_>>();
        let () = package_index
            .add_blobs(pkg_id, content.clone(), gc_protection)
            .await
            .map_err(Error::ProtectBlobs)?;
        let mut missing_content = vec![];
        {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"check content blob presence");
            for content_id in content.into_iter() {
                if all_blobs.insert(content_id)
                    && !blobfs_client.blob_present_and_up_to_date(&content_id).await
                {
                    missing_content.push(content_id);
                }
            }
        }
        inspect_outstanding.add(missing_content.len() as u64);
        if !missing_content.is_empty() {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"fetch content blobs");
            let mut futs = FuturesUnordered::from_iter(blob_fetcher.push_all(
                priority,
                missing_content.into_iter().map(Into::into),
                fetch_params.clone(),
                trace_id,
            ));
            while let Some(res) = futs.next().await {
                let _: Option<u64> = res.map_err(Error::BlobPush)?.map_err(Error::BlobFetch)?;
                inspect_outstanding.subtract(1);
            }
        }
        ret.get_or_insert(root_dir);
    }
    let () = package_index.complete_retained_blobs(pkg_id, all_blobs).await;
    let root_dir = ret.expect("queue starts with an entry");
    Ok(match gc_protection {
        fpkg::GcProtection::Retained => MaybeCachedRootDir::Uncached(Arc::new(root_dir)),
        fpkg::GcProtection::OpenPackageTracking => MaybeCachedRootDir::Cached(
            open_packages
                .get_or_insert(pkg_id, Some(root_dir))
                .await
                .map_err(Error::CreatingTrackedRootDir)?,
        ),
    })
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("pushing a blob onto the fetch queue")]
    BlobPush(#[source] crate::blob_fetcher::Cancelled),

    #[error("fetching blob")]
    BlobFetch(#[source] Arc<crate::blob_fetcher::FetchError>),

    #[error("creating root dir for {pkg}")]
    CreatingRootDir {
        #[source]
        source: package_directory::Error,
        pkg: fuchsia_merkle::Hash,
    },

    #[error("reading subpackages for {pkg}")]
    ReadingSubpackages {
        #[source]
        source: package_directory::SubpackagesError,
        pkg: fuchsia_merkle::Hash,
    },

    #[error("adding blobs to package index")]
    ProtectBlobs(#[source] crate::index::AddBlobsError),

    #[error("creating root dir with open package tracking")]
    CreatingTrackedRootDir(#[source] package_directory::Error),
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fpkg::ResolveError as Err;
        match err {
            BlobPush(_) => Err::Internal,
            BlobFetch(e) => e.as_ref().into(),
            CreatingRootDir { .. } => Err::Io,
            ReadingSubpackages { .. } => Err::Io,
            ProtectBlobs(_) => Err::Internal,
            CreatingTrackedRootDir(_) => Err::Io,
        }
    }
}

impl From<&Error> for fidl_fuchsia_component_resolution::ResolverError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fidl_fuchsia_component_resolution::ResolverError as Err;
        match err {
            BlobPush(_) => Err::Internal,
            BlobFetch(e) => fetch_to_component_resolve_err(e),
            CreatingRootDir { .. } => Err::Io,
            ReadingSubpackages { .. } => Err::Io,
            ProtectBlobs(_) => Err::Internal,
            CreatingTrackedRootDir(_) => Err::Io,
        }
    }
}

fn fetch_to_component_resolve_err(
    err: &crate::blob_fetcher::FetchError,
) -> fidl_fuchsia_component_resolution::ResolverError {
    use crate::blob_fetcher::FetchError::*;
    use fidl_fuchsia_component_resolution::ResolverError as Err;
    match err {
        CreateBlob(blobfs::CreateError::OutOfSpace) => Err::NoSpace,
        CreateBlob(_) => Err::Io,
        BlobUrl { .. } => Err::Internal,
        ConnectToHttpClient(_) => Err::Io,
        DownloadBlobFidl { .. } => Err::Internal,
        DownloadBlob(e) => {
            use fidl_fuchsia_pkg_http::ClientDownloadBlobError::*;
            match e {
                NoSpace => Err::NoSpace,
                Network => Err::Io,
                NotFound => Err::ResourceUnavailable,
                NetworkRateLimit => Err::Io,
                Other => Err::Io,
                Canceled => Err::Internal,
            }
        }
        PostWriteStatusCheck { .. } => Err::Internal,
        BlobAbsentAfterWrite => Err::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RootDirCache;
    use crate::blob_fetcher::{BlobFetcher, Params as BlobFetcherParams};
    use crate::index::{PackageIndex, UpgradablePackagesInitialized};
    use crate::test_utils::{expect_get_vmo, get_meta_far, respond_get_vmo};
    use assert_matches::assert_matches;
    use fidl_fuchsia_fxfs as ffxfs;
    use fidl_fuchsia_io as fio;
    use fidl_fuchsia_pkg_http as fpkg_http;
    use fuchsia_async as fasync;
    use fuchsia_hash::Hash;
    use futures::future::FutureExt as _;
    use std::num::NonZeroUsize;
    use std::pin::{Pin, pin};
    use std::task::Poll;

    const TEST_CONCURRENCY_LIMIT: NonZeroUsize = NonZeroUsize::new(2).unwrap();
    const SINGLE_SLOT_CONCURRENCY_LIMIT: NonZeroUsize = NonZeroUsize::new(1).unwrap();
    const TEST_NETWORK_TIMEOUT_SECONDS: i64 = 30;
    const TEST_RESUMPTION_ATTEMPTS_LIMIT: u32 = 0;
    const TEST_BLOB_SOURCE_URL: &str = "http://example.org/blobs/1";
    const TEST_PACKAGE_NAME: &str = "test-package";
    const SECOND_PACKAGE_NAME: &str = "second-package";
    const SUBPACKAGE_NAME: &str = "subpackage-0";
    const TEST_DOWNLOADED_BYTES: u64 = 128;
    const META_HASH_BYTE: u8 = 1;
    const FIRST_CONTENT_HASH_BYTE: u8 = 2;
    const SUBPACKAGE_META_HASH_BYTE: u8 = 3;
    const SUBPACKAGE_CONTENT_HASH_BYTE: u8 = 4;
    const SECOND_META_HASH_BYTE: u8 = 5;
    const SECOND_CONTENT_HASH_BYTE_A: u8 = 6;
    const SECOND_CONTENT_HASH_BYTE_B: u8 = 7;
    const SECOND_CONTENT_HASH_BYTE_C: u8 = 8;
    const SECOND_CONTENT_HASH_BYTE_D: u8 = 9;
    const HASH_BYTE_LEN: usize = 32;

    #[derive(Clone)]
    struct FixedProxyConnector(fpkg_http::ClientProxy);

    impl fidl_connector::Connect for FixedProxyConnector {
        type Proxy = fpkg_http::ClientProxy;

        fn connect(&self) -> Result<Self::Proxy, anyhow::Error> {
            Ok(self.0.clone())
        }
    }

    struct TestEnv {
        package_index: Arc<PackageIndex>,
        initialized: UpgradablePackagesInitialized,
        open_packages: RootDirCache,
        package_fetcher: PackageFetcher,
        blob_reader_stream: ffxfs::BlobReaderRequestStream,
        blob_creator_stream: ffxfs::BlobCreatorRequestStream,
        http_client_stream: fpkg_http::ClientRequestStream,
        _inspector: finspect::Inspector,
        _blobfs_dir_stream: fio::DirectoryRequestStream,
    }

    impl TestEnv {
        fn new(executor: &mut fasync::TestExecutor) -> Self {
            Self::with_concurrency_limit(executor, TEST_CONCURRENCY_LIMIT)
        }

        fn with_concurrency_limit(
            executor: &mut fasync::TestExecutor,
            concurrency_limit: NonZeroUsize,
        ) -> Self {
            let (blobfs_client, blobfs_dir_stream, blob_reader_stream, blob_creator_stream) =
                blobfs::Client::new_test();
            let package_index = Arc::new(PackageIndex::new(blobfs_client.clone(), None));
            let initialized = match executor.run_until_stalled(&mut pin!(
                package_index.wait_until_upgradable_packages_initialized()
            )) {
                Poll::Ready(initialized) => initialized,
                Poll::Pending => panic!("wait_until_upgradable_packages_initialized stalled"),
            };
            let (root_dir_factory, open_packages) = {
                let bootfs = fidl::endpoints::create_proxy::<fio::DirectoryMarker>().0;
                let mut setup_future = pin!(crate::root_dir::new(bootfs, blobfs_client.clone()));
                match executor.run_until_stalled(&mut setup_future) {
                    Poll::Ready(Ok(helpers)) => helpers,
                    other => panic!("unexpected root_dir::new result: {other:?}"),
                }
            };
            let (http_proxy, http_client_stream) =
                fidl::endpoints::create_proxy_and_stream::<fpkg_http::ClientMarker>();
            let inspector = finspect::Inspector::default();
            let blob_fetcher = BlobFetcher::new(
                concurrency_limit,
                BlobFetcherParams::builder()
                    .header_network_timeout(zx::BootDuration::from_seconds(
                        TEST_NETWORK_TIMEOUT_SECONDS,
                    ))
                    .body_network_timeout(zx::BootDuration::from_seconds(
                        TEST_NETWORK_TIMEOUT_SECONDS,
                    ))
                    .download_resumption_attempts_limit(TEST_RESUMPTION_ATTEMPTS_LIMIT)
                    .build(),
                blobfs_client.clone(),
                FixedProxyConnector(http_proxy),
                inspector.root().create_child("blob_fetcher"),
            );
            let package_fetcher = PackageFetcher::new(
                Arc::clone(&package_index),
                blobfs_client,
                blob_fetcher,
                root_dir_factory,
                open_packages.clone(),
                inspector.root().create_child("package_fetcher"),
            );
            Self {
                package_index,
                initialized,
                open_packages,
                package_fetcher,
                blob_reader_stream,
                blob_creator_stream,
                http_client_stream,
                _inspector: inspector,
                _blobfs_dir_stream: blobfs_dir_stream,
            }
        }

        fn start_tracked_fetch(
            &self,
            pkg_id: Hash,
        ) -> Pin<Box<impl Future<Output = Result<Arc<crate::CachedRootDir>, Error>> + use<>>>
        {
            let fetcher = self.package_fetcher.clone();
            let blob_source: http::Uri = TEST_BLOB_SOURCE_URL.parse().unwrap();
            Box::pin(async move {
                fetcher
                    .fetch_with_open_package_tracking(pkg_id, blob_source, ftrace::Id::new())
                    .await
            })
        }

        fn read_protected_blobs(&self) -> HashSet<Hash> {
            let gc_guard = self
                .package_index
                .acquire_gc_lock(self.initialized)
                .now_or_never()
                .expect("acquire_gc_lock completes without awaiting");
            gc_guard
                .protected_blobs()
                .now_or_never()
                .expect("protected_blobs completes without awaiting")
        }

        fn respond_needs_overwrite(
            &mut self,
            executor: &mut fasync::TestExecutor,
            expected_hash: Hash,
            response: Result<bool, zx::Status>,
        ) {
            match executor.run_until_stalled(&mut self.blob_creator_stream.next()) {
                Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::NeedsOverwrite {
                    blob_hash,
                    responder,
                }))) => {
                    assert_eq!(Hash::from(blob_hash), expected_hash);
                    responder.send(response.map_err(zx::Status::into_raw)).unwrap();
                }
                other => panic!("unexpected blob_creator_stream request: {other:?}"),
            }
        }

        fn respond_create_blob(
            &mut self,
            executor: &mut fasync::TestExecutor,
            expected_hash: Hash,
        ) {
            match executor.run_until_stalled(&mut self.blob_creator_stream.next()) {
                Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::Create {
                    hash,
                    allow_existing,
                    responder,
                }))) => {
                    assert_eq!(Hash::from(hash), expected_hash);
                    assert!(allow_existing);
                    let (writer_client, _writer_server) =
                        fidl::endpoints::create_endpoints::<ffxfs::BlobWriterMarker>();
                    responder.send(Ok(writer_client)).unwrap();
                }
                other => panic!("unexpected blob_creator_stream request: {other:?}"),
            }
        }

        fn respond_get_vmo(
            &mut self,
            executor: &mut fasync::TestExecutor,
            expected_hash: Hash,
            contents: &[u8],
        ) {
            let responder = expect_get_vmo(executor, &mut self.blob_reader_stream, expected_hash);
            respond_get_vmo(responder, contents);
        }

        fn next_download_blob(
            &mut self,
            executor: &mut fasync::TestExecutor,
        ) -> (String, zx::EventPair, fpkg_http::ClientDownloadBlobResponder) {
            match executor.run_until_stalled(&mut self.http_client_stream.next()) {
                Poll::Ready(Some(Ok(fpkg_http::ClientRequest::DownloadBlob {
                    url,
                    cancel,
                    responder,
                    ..
                }))) => (url, cancel.expect("cancel handle provided"), responder),
                other => panic!("unexpected http_client_stream request: {other:?}"),
            }
        }

        fn drive_present_meta_far_to_content_checks(
            &mut self,
            executor: &mut fasync::TestExecutor,
            fetch_future: &mut Pin<Box<impl Future + ?Sized>>,
            meta_hash: Hash,
            meta_far_bytes: &[u8],
            missing_content_blobs: &[Hash],
        ) {
            assert!(executor.run_until_stalled(fetch_future).is_pending());
            self.respond_needs_overwrite(executor, meta_hash, Ok(false));

            assert!(executor.run_until_stalled(fetch_future).is_pending());
            self.respond_get_vmo(executor, meta_hash, meta_far_bytes);

            let mut remaining = missing_content_blobs.iter().copied().collect::<HashSet<_>>();
            while !remaining.is_empty() {
                assert!(executor.run_until_stalled(fetch_future).is_pending());
                match executor.run_until_stalled(&mut self.blob_creator_stream.next()) {
                    Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::NeedsOverwrite {
                        blob_hash,
                        responder,
                    }))) => {
                        let hash = Hash::from(blob_hash);
                        assert!(
                            remaining.remove(&hash),
                            "unexpected content blob presence check for {hash}"
                        );
                        responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
                    }
                    other => panic!("unexpected blob_creator_stream request: {other:?}"),
                }
            }
            assert!(executor.run_until_stalled(fetch_future).is_pending());
        }

        fn drive_queued_blob_to_download(
            &mut self,
            executor: &mut fasync::TestExecutor,
            expected_hash: Hash,
        ) -> fpkg_http::ClientDownloadBlobResponder {
            self.respond_needs_overwrite(executor, expected_hash, Err(zx::Status::NOT_FOUND));
            self.respond_create_blob(executor, expected_hash);
            let (download_url, _cancel, responder) = self.next_download_blob(executor);
            assert_eq!(download_url, format!("{TEST_BLOB_SOURCE_URL}/{expected_hash}"));
            responder
        }

        fn drive_next_blob_from_set_to_download(
            &mut self,
            executor: &mut fasync::TestExecutor,
            expected_hashes: &mut HashSet<Hash>,
        ) -> (Hash, fpkg_http::ClientDownloadBlobResponder) {
            let scheduled_hash =
                match executor.run_until_stalled(&mut self.blob_creator_stream.next()) {
                    Poll::Ready(Some(Ok(ffxfs::BlobCreatorRequest::NeedsOverwrite {
                        blob_hash,
                        responder,
                    }))) => {
                        let hash = Hash::from(blob_hash);
                        assert!(
                            expected_hashes.remove(&hash),
                            "unexpected scheduled blob {hash}, expected one of {expected_hashes:?}"
                        );
                        responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
                        hash
                    }
                    other => panic!("unexpected blob_creator_stream request: {other:?}"),
                };
            self.respond_create_blob(executor, scheduled_hash);
            let (download_url, _cancel, responder) = self.next_download_blob(executor);
            assert_eq!(download_url, format!("{TEST_BLOB_SOURCE_URL}/{scheduled_hash}"));
            (scheduled_hash, responder)
        }
    }

    #[fuchsia::test]
    fn dropping_in_flight_fetch_cancels_blob_and_cleans_up_writing_index_during_gc() {
        let mut executor = fasync::TestExecutor::new();
        let mut env = TestEnv::new(&mut executor);
        let meta_hash = Hash::from([META_HASH_BYTE; HASH_BYTE_LEN]);

        let mut fetch_future = env.start_tracked_fetch(meta_hash);

        // Drive `PackageFetcher` past `start_writing` and its initial meta.far check, then drive
        // `BlobFetcher` until `DownloadBlob` is in flight.
        assert!(executor.run_until_stalled(&mut fetch_future).is_pending());
        env.respond_needs_overwrite(&mut executor, meta_hash, Err(zx::Status::NOT_FOUND));
        assert!(executor.run_until_stalled(&mut fetch_future).is_pending());
        env.respond_needs_overwrite(&mut executor, meta_hash, Err(zx::Status::NOT_FOUND));
        env.respond_create_blob(&mut executor, meta_hash);

        let (download_url, cancel_eventpair, download_responder) =
            env.next_download_blob(&mut executor);
        assert_eq!(download_url, format!("{TEST_BLOB_SOURCE_URL}/{meta_hash}"));
        assert!(
            fasync::OnSignals::new(&cancel_eventpair, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .now_or_never()
                .is_none()
        );

        // Start a GC pass: `meta_hash` is protected while `GcGuard` is held.
        let package_index = Arc::clone(&env.package_index);
        let gc_guard = match executor
            .run_until_stalled(&mut pin!(package_index.acquire_gc_lock(env.initialized)))
        {
            Poll::Ready(guard) => guard,
            Poll::Pending => panic!("acquire_gc_lock stalled"),
        };
        let protected_blobs =
            match executor.run_until_stalled(&mut pin!(gc_guard.protected_blobs())) {
                Poll::Ready(blobs) => blobs,
                Poll::Pending => panic!("protected_blobs stalled"),
            };
        assert_eq!(protected_blobs, HashSet::from([meta_hash]));

        // Dropping the fetch future while `GcGuard` is held must synchronously enqueue
        // `WritingIndex` cleanup via `CleanupGuard::drop` and cancel the unshared `DownloadBlob`.
        drop(fetch_future);
        assert_matches!(
            executor.run_until_stalled(&mut pin!(fasync::OnSignals::new(
                &cancel_eventpair,
                zx::Signals::EVENTPAIR_PEER_CLOSED,
            ))),
            Poll::Ready(Ok(signals)) if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED)
        );

        // A new fetch started during the active GC pass must wait on `GcGuard` in `start_writing`
        // before touching blobfs.
        let mut blocked_retry = env.start_tracked_fetch(meta_hash);
        assert!(executor.run_until_stalled(&mut blocked_retry).is_pending());
        assert!(executor.run_until_stalled(&mut env.blob_creator_stream.next()).is_pending());
        drop(blocked_retry);

        // Once `GcGuard` drops, the queued `CleanupGuard` drop is drained on the next lock
        // acquisition without needing any async cleanup task.
        drop(gc_guard);
        assert_eq!(env.read_protected_blobs(), HashSet::new());

        // A subsequent retry does not coalesce onto the cancelled download and starts a fresh
        // download once the cancelled `DownloadBlob` finishes tearing down.
        let mut retry_future = env.start_tracked_fetch(meta_hash);
        assert!(executor.run_until_stalled(&mut retry_future).is_pending());
        env.respond_needs_overwrite(&mut executor, meta_hash, Err(zx::Status::NOT_FOUND));
        assert!(executor.run_until_stalled(&mut retry_future).is_pending());
        assert!(executor.run_until_stalled(&mut env.blob_creator_stream.next()).is_pending());

        download_responder.send(Err(fpkg_http::ClientDownloadBlobError::Canceled)).unwrap();
        env.respond_needs_overwrite(&mut executor, meta_hash, Err(zx::Status::NOT_FOUND));
        env.respond_create_blob(&mut executor, meta_hash);
        let (retry_url, retry_cancel, _retry_responder) = env.next_download_blob(&mut executor);
        assert_eq!(retry_url, format!("{TEST_BLOB_SOURCE_URL}/{meta_hash}"));
        assert!(
            fasync::OnSignals::new(&retry_cancel, zx::Signals::EVENTPAIR_PEER_CLOSED)
                .now_or_never()
                .is_none()
        );
    }

    #[fuchsia::test]
    fn dropping_one_of_two_concurrent_fetches_preserves_writing_protection_and_shared_download() {
        let mut executor = fasync::TestExecutor::new();
        let mut env = TestEnv::new(&mut executor);
        let meta_hash = Hash::from([META_HASH_BYTE; HASH_BYTE_LEN]);
        let content_hash = Hash::from([FIRST_CONTENT_HASH_BYTE; HASH_BYTE_LEN]);
        let meta_far_bytes = get_meta_far(TEST_PACKAGE_NAME, [content_hash], []);

        let mut first_fetch = env.start_tracked_fetch(meta_hash);
        env.drive_present_meta_far_to_content_checks(
            &mut executor,
            &mut first_fetch,
            meta_hash,
            &meta_far_bytes,
            &[content_hash],
        );

        env.respond_needs_overwrite(&mut executor, content_hash, Err(zx::Status::NOT_FOUND));
        env.respond_create_blob(&mut executor, content_hash);
        let (_download_url, cancel_eventpair, download_responder) =
            env.next_download_blob(&mut executor);

        // Start a second concurrent fetch for the same package; it increments `WritingIndex`'s
        // refcount and merges onto the in-flight content blob download.
        let mut second_fetch = env.start_tracked_fetch(meta_hash);
        env.drive_present_meta_far_to_content_checks(
            &mut executor,
            &mut second_fetch,
            meta_hash,
            &meta_far_bytes,
            &[content_hash],
        );
        assert!(executor.run_until_stalled(&mut env.http_client_stream.next()).is_pending());

        // Dropping the first fetch decrements the writing refcount from 2 to 1 without removing
        // protection or cancelling the shared blob download.
        drop(first_fetch);
        assert!(
            executor
                .run_until_stalled(&mut pin!(fasync::OnSignals::new(
                    &cancel_eventpair,
                    zx::Signals::EVENTPAIR_PEER_CLOSED,
                )))
                .is_pending()
        );
        assert_eq!(env.read_protected_blobs(), HashSet::from([meta_hash, content_hash]));

        // Completing the shared download finishes the second fetch, inserts `meta_hash` into
        // `open_packages`, and clears `WritingIndex`.
        download_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, content_hash, Ok(false));

        let cached_root_dir = match executor.run_until_stalled(&mut second_fetch) {
            Poll::Ready(Ok(root_dir)) => root_dir,
            other => panic!("unexpected second_fetch result: {other:?}"),
        };
        assert!(env.open_packages.get(&meta_hash).is_some());
        assert_eq!(env.read_protected_blobs(), HashSet::new());

        drop(cached_root_dir);
        assert!(env.open_packages.get(&meta_hash).is_none());
    }

    #[fuchsia::test]
    fn earlier_package_resolve_prioritizes_content_and_subpackage_blobs_ahead_of_later_resolve() {
        let mut executor = fasync::TestExecutor::new();
        let mut env = TestEnv::with_concurrency_limit(&mut executor, SINGLE_SLOT_CONCURRENCY_LIMIT);

        let first_meta_hash = Hash::from([META_HASH_BYTE; HASH_BYTE_LEN]);
        let first_content_hash = Hash::from([FIRST_CONTENT_HASH_BYTE; HASH_BYTE_LEN]);
        let subpackage_meta_hash = Hash::from([SUBPACKAGE_META_HASH_BYTE; HASH_BYTE_LEN]);
        let subpackage_content_hash = Hash::from([SUBPACKAGE_CONTENT_HASH_BYTE; HASH_BYTE_LEN]);
        let first_meta_far =
            get_meta_far(TEST_PACKAGE_NAME, [first_content_hash], [subpackage_meta_hash]);
        let subpackage_meta_far = get_meta_far(SUBPACKAGE_NAME, [subpackage_content_hash], []);

        let second_meta_hash = Hash::from([SECOND_META_HASH_BYTE; HASH_BYTE_LEN]);
        let second_content_blobs = [
            Hash::from([SECOND_CONTENT_HASH_BYTE_A; HASH_BYTE_LEN]),
            Hash::from([SECOND_CONTENT_HASH_BYTE_B; HASH_BYTE_LEN]),
            Hash::from([SECOND_CONTENT_HASH_BYTE_C; HASH_BYTE_LEN]),
            Hash::from([SECOND_CONTENT_HASH_BYTE_D; HASH_BYTE_LEN]),
        ];
        let second_meta_far = get_meta_far(SECOND_PACKAGE_NAME, second_content_blobs, []);

        // Start the earlier resolve (`first_pkg_fetch`), occupying the single download slot with
        // its `meta.far`.
        let mut first_pkg_fetch = env.start_tracked_fetch(first_meta_hash);
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_needs_overwrite(&mut executor, first_meta_hash, Err(zx::Status::NOT_FOUND));
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        let first_meta_responder =
            env.drive_queued_blob_to_download(&mut executor, first_meta_hash);

        // Start a later resolve (`second_pkg_fetch`) whose `meta.far` is already resident so all 4
        // of its content blobs are queued in `BlobFetcher` before `first_pkg_fetch` discovers any
        // of its content or subpackage blobs.
        let mut second_pkg_fetch = env.start_tracked_fetch(second_meta_hash);
        env.drive_present_meta_far_to_content_checks(
            &mut executor,
            &mut second_pkg_fetch,
            second_meta_hash,
            &second_meta_far,
            &second_content_blobs,
        );

        // Complete `first_meta_hash`: one blob from `second_pkg_fetch` starts immediately while the
        // other three remain pending at `second_pkg_fetch`'s lower priority.
        first_meta_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, first_meta_hash, Ok(false));
        let mut remaining_second_pkg_blobs = HashSet::from(second_content_blobs);
        let (active_second_blob_1, second_blob_responder_1) = env
            .drive_next_blob_from_set_to_download(&mut executor, &mut remaining_second_pkg_blobs);

        // Advance `first_pkg_fetch` through reading `first_meta_far` so it queues
        // `first_content_hash` at its per-resolve priority.
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_get_vmo(&mut executor, first_meta_hash, &first_meta_far);
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_needs_overwrite(&mut executor, first_content_hash, Err(zx::Status::NOT_FOUND));
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());

        // Completing `active_second_blob_1` must schedule `first_content_hash` ahead of the three
        // still-pending blobs from `second_pkg_fetch`.
        second_blob_responder_1.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, active_second_blob_1, Ok(false));
        let first_content_responder =
            env.drive_queued_blob_to_download(&mut executor, first_content_hash);

        // Completing `first_content_hash` starts a second blob from `second_pkg_fetch` (leaving two
        // pending), while `first_pkg_fetch` advances to queue `subpackage_meta_hash`.
        first_content_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, first_content_hash, Ok(false));
        let (active_second_blob_2, second_blob_responder_2) = env
            .drive_next_blob_from_set_to_download(&mut executor, &mut remaining_second_pkg_blobs);

        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_needs_overwrite(
            &mut executor,
            subpackage_meta_hash,
            Err(zx::Status::NOT_FOUND),
        );
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());

        // Completing `active_second_blob_2` must schedule `subpackage_meta_hash` ahead of the two
        // still-pending blobs from `second_pkg_fetch`.
        second_blob_responder_2.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, active_second_blob_2, Ok(false));
        let subpackage_meta_responder =
            env.drive_queued_blob_to_download(&mut executor, subpackage_meta_hash);

        // Completing `subpackage_meta_hash` starts a third blob from `second_pkg_fetch` (leaving
        // one pending), while `first_pkg_fetch` reads `subpackage_meta_far` and queues
        // `subpackage_content_hash`.
        subpackage_meta_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, subpackage_meta_hash, Ok(false));
        let (active_second_blob_3, second_blob_responder_3) = env
            .drive_next_blob_from_set_to_download(&mut executor, &mut remaining_second_pkg_blobs);

        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_get_vmo(&mut executor, subpackage_meta_hash, &subpackage_meta_far);
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());
        env.respond_needs_overwrite(
            &mut executor,
            subpackage_content_hash,
            Err(zx::Status::NOT_FOUND),
        );
        assert!(executor.run_until_stalled(&mut first_pkg_fetch).is_pending());

        // Completing `active_second_blob_3` must schedule `subpackage_content_hash` ahead of the
        // final pending blob from `second_pkg_fetch`.
        second_blob_responder_3.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, active_second_blob_3, Ok(false));
        let subpackage_content_responder =
            env.drive_queued_blob_to_download(&mut executor, subpackage_content_hash);

        // Finish `subpackage_content_hash` and verify `first_pkg_fetch` completes before the final
        // blob from `second_pkg_fetch` finishes.
        subpackage_content_responder.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, subpackage_content_hash, Ok(false));
        let (active_second_blob_4, second_blob_responder_4) = env
            .drive_next_blob_from_set_to_download(&mut executor, &mut remaining_second_pkg_blobs);
        assert!(remaining_second_pkg_blobs.is_empty());

        assert_matches!(executor.run_until_stalled(&mut first_pkg_fetch), Poll::Ready(Ok(_)));
        assert!(executor.run_until_stalled(&mut second_pkg_fetch).is_pending());

        second_blob_responder_4.send(Ok(TEST_DOWNLOADED_BYTES)).unwrap();
        env.respond_needs_overwrite(&mut executor, active_second_blob_4, Ok(false));
        assert_matches!(executor.run_until_stalled(&mut second_pkg_fetch), Poll::Ready(Ok(_)));
    }
}
