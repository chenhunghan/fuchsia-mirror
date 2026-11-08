// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::frozen_index::{BaseIndex, CacheIndex};
use crate::index::{PackageIndex, UpgradablePackagesInitialized};
use anyhow::{Context as _, anyhow};
use fidl_fuchsia_pkg_garbagecollector::{
    GcError as SpaceErrorCode, ManagerRequest as SpaceManagerRequest,
    ManagerRequestStream as SpaceManagerRequestStream,
};
use fidl_fuchsia_update::CommitStatusProviderProxy;
use fuchsia_async as fasync;
use fuchsia_inspect::{self as finspect, Property as _};
use futures::prelude::*;
use log::{error, info};
use std::collections::HashSet;
use std::sync::Arc;

/// Garbage collects blobs that are not protected by the base or cache packages, the package index,
/// or open packages.
///
/// Locking: a garbage collection pass waits for upgradable packages to be initialized, then
/// acquires `gc_pass`, then locks the package index for reading until it is done deleting blobs
/// (see `PackageIndex` and `PackageIndex::read_for_gc`). Only garbage collection acquires
/// `gc_pass`.
pub(crate) struct GcService {
    blobfs: blobfs::Client,
    base_packages: Arc<BaseIndex>,
    cache_packages: Arc<CacheIndex>,
    package_index: Arc<PackageIndex>,
    open_packages: crate::RootDirCache,
    commit_status_provider: CommitStatusProviderProxy,
    commit_event_pair: async_lock::OnceCell<zx::EventPair>,
    /// Serializes garbage collection passes.
    gc_pass: async_lock::Mutex<GcState>,
}

/// State protected by `GcService::gc_pass`.
#[derive(Default)]
struct GcState {
    /// Whether a garbage collection pass has completed since pkg-cache started.
    has_run: bool,
}

/// The outcome of a post-commit garbage collection attempt.
enum PostCommitGcOutcome {
    /// A garbage collection pass ran to completion.
    Completed,
    /// A garbage collection pass had already completed since pkg-cache started, so none was run.
    AlreadyRan,
}

impl GcService {
    pub(crate) fn new(
        blobfs: blobfs::Client,
        base_packages: Arc<BaseIndex>,
        cache_packages: Arc<CacheIndex>,
        package_index: Arc<PackageIndex>,
        open_packages: crate::RootDirCache,
        commit_status_provider: CommitStatusProviderProxy,
    ) -> Arc<Self> {
        Arc::new(Self {
            blobfs,
            base_packages,
            cache_packages,
            package_index,
            open_packages,
            commit_status_provider,
            commit_event_pair: async_lock::OnceCell::new(),
            gc_pass: async_lock::Mutex::new(GcState::default()),
        })
    }

    /// Returns the boot-lifetime commit status eventpair, fetching and caching it on first use so
    /// subsequent calls do not wake `system-update-committer` from idle stop.
    async fn commit_event_pair(&self) -> Result<&zx::EventPair, anyhow::Error> {
        self.commit_event_pair
            .get_or_try_init(|| async {
                self.commit_status_provider
                    .is_current_system_committed()
                    .await
                    .context("while getting event pair")
            })
            .await
    }

    async fn check_committed(&self) -> Result<(), SpaceErrorCode> {
        let event_pair = self.commit_event_pair().await.map_err(|e| {
            error!("Failed to get commit event pair: {e:#}");
            SpaceErrorCode::Internal
        })?;
        event_pair
            .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST)
            .map(|_: zx::Signals| ())
            .map_err(|e| {
                match e {
                    zx::Status::TIMED_OUT => {
                        info!("GC is blocked pending update.");
                    }
                    zx::Status::CANCELED => {
                        info!("Commit handle is closed, likely because we are rebooting.");
                    }
                    other => {
                        error!("Got unexpected status {other:?} while waiting on handle.");
                    }
                }
                SpaceErrorCode::PendingCommit
            })
    }

    /// Runs a garbage collection pass for a `fuchsia.pkg.garbagecollector/Manager.Gc` request.
    ///
    /// Checks `USER_0` without waiting so pre-commit GC calls fail fast.
    async fn gc(&self) -> Result<(), SpaceErrorCode> {
        let () = self.check_committed().await?;
        let initialized = self.package_index.wait_until_upgradable_packages_initialized().await;
        let mut state = self.gc_pass.lock().await;
        self.gc_locked(&mut state, initialized).await
    }

    /// Runs a garbage collection pass unless one already completed since pkg-cache started.
    ///
    /// Like `gc`, checks `USER_0` without waiting. Reports progress through `inspect`.
    async fn gc_if_not_already_run(
        &self,
        inspect: &finspect::StringProperty,
    ) -> Result<PostCommitGcOutcome, SpaceErrorCode> {
        let () = self.check_committed().await?;
        if self.package_index.upgradable_packages_enabled() {
            inspect.set("waiting for upgradable packages");
        }
        let initialized = self.package_index.wait_until_upgradable_packages_initialized().await;
        inspect.set("waiting for gc lock");
        let mut state = self.gc_pass.lock().await;
        if state.has_run {
            return Ok(PostCommitGcOutcome::AlreadyRan);
        }
        inspect.set("running gc");
        let () = self.gc_locked(&mut state, initialized).await?;
        Ok(PostCommitGcOutcome::Completed)
    }

    /// Runs a garbage collection pass while `gc_pass` (whose contents are `state`) is held.
    async fn gc_locked(
        &self,
        state: &mut GcState,
        initialized: UpgradablePackagesInitialized,
    ) -> Result<(), SpaceErrorCode> {
        info!("performing gc");

        // The primary purpose of GC is to free space to enable an OTA, so we continue if possible
        // through any errors and delete as many blobs as we can (because OTA'ing may be the only
        // way to actually fix the errors).
        let () = async move {
            // Determine all resident blobs before locking the package index to decrease the amount
            // of time the package index lock is held. Blobs written after this are implicitly
            // protected. This is for speed and not necessary for correctness. It would still be
            // correct if the list of resident blobs was determined any time after the package index
            // lock is taken. This error can not be ignored because we wouldn't know which blobs to
            // delete.
            let mut eligible_blobs = self.blobfs.list_known_blobs().await?;

            // Lock the package index until we are done deleting blobs. During resolution, required
            // blobs are added to the index before their presence in blobfs is checked, so locking
            // the index until we are done deleting blobs guarantees we will never delete a blob
            // that resolution thinks it can skip fetching.
            let package_index = self.package_index.read_for_gc(initialized).await;

            let () = package_index.protected_blobs().await.iter().for_each(|blob| {
                eligible_blobs.remove(blob);
            });

            let () = self.base_packages.list_blobs().iter().for_each(|blob| {
                eligible_blobs.remove(blob);
            });

            let () = self.cache_packages.list_blobs().iter().for_each(|blob| {
                eligible_blobs.remove(blob);
            });

            // The upgradable package set cannot change while the package index is locked. Clients
            // keep the packages they are upgrading open until `SetUpgradableUrls` returns, so those
            // packages are protected by `open_packages` until they are protected as upgradable
            // packages.
            let () = protect_open_blobs(&mut eligible_blobs, &self.open_packages).await;

            info!("Garbage collecting {} blobs...", eligible_blobs.len());
            let mut errors = 0;
            for (i, blob) in eligible_blobs.iter().enumerate() {
                let () = self.blobfs.delete_blob(blob).await.unwrap_or_else(|e| {
                    error!(blob:%; "Failed to delete blob: {:#}", anyhow!(e));
                    errors += 1;
                });
                if (i + 1).is_multiple_of(100) {
                    info!("{} blobs deleted...", i + 1 - errors);
                }
            }
            info!("Garbage collection done. Deleted {} blobs.", eligible_blobs.len() - errors);
            Ok(())
        }
        .await
        .map_err(|e: anyhow::Error| {
            error!("Failed to perform GC operation: {:#}", e);
            SpaceErrorCode::Internal
        })?;

        state.has_run = true;
        Ok(())
    }
}

pub(crate) async fn serve(
    gc_service: Arc<GcService>,
    mut stream: SpaceManagerRequestStream,
) -> Result<(), anyhow::Error> {
    while let Some(event) = stream.try_next().await? {
        let SpaceManagerRequest::Gc { responder } = event else {
            return Err(anyhow!("Unknown method called on garbage collector."));
        };
        responder.send(gc_service.gc().await)?;
    }
    Ok(())
}

/// Runs at most one garbage collection pass after the system is committed during this boot.
///
/// Waits for `USER_0` on the commit status eventpair. If `USER_1` is also asserted (the system was
/// committed during this boot), waits until `earliest_time` on the boot timeline, then runs a pass
/// unless one already completed since pkg-cache started. Progress is reported through `inspect`.
///
/// Returns an error (and sets `inspect` to `error: ...`) if the commit status cannot be obtained or
/// the garbage collection pass fails.
pub(crate) async fn gc_after_commit(
    gc_service: Arc<GcService>,
    earliest_time: zx::BootInstant,
    inspect: Arc<finspect::StringProperty>,
) -> Result<(), anyhow::Error> {
    async {
        let event_pair = gc_service.commit_event_pair().await?;
        let observed = fasync::OnSignals::new(event_pair, zx::Signals::USER_0)
            .await
            .context("while waiting for commit")?;
        if observed.contains(zx::Signals::USER_1) {
            info!(
                "System was committed during this boot, waiting until {}s on the boot timeline \
                 before post-commit garbage collection.",
                (earliest_time - zx::BootInstant::ZERO).into_seconds()
            );
            inspect.set("waiting until earliest boot time");
            let () = fasync::Timer::new(earliest_time).await;
            match gc_service
                .gc_if_not_already_run(&inspect)
                .await
                .map_err(|e| anyhow!("while running gc: {e:?}"))?
            {
                PostCommitGcOutcome::Completed => inspect.set("gc completed"),
                PostCommitGcOutcome::AlreadyRan => {
                    info!("Skipping post-commit garbage collection because GC already ran.");
                    inspect.set("gc already ran");
                }
            }
        } else {
            inspect.set("gc unnecessary");
        }
        Ok(())
    }
    .await
    .inspect_err(|e| inspect.set(&format!("error: {e:#}")))
}

async fn protect_open_blobs(
    eligible_blobs: &mut HashSet<fuchsia_hash::Hash>,
    open_packages: &crate::RootDirCache,
) {
    let mut to_visit = open_packages.list();
    let mut enqueued = to_visit.iter().map(|pkg| *pkg.hash()).collect::<HashSet<_>>();
    while let Some(pkg) = to_visit.pop() {
        eligible_blobs.remove(pkg.hash());
        pkg.external_file_hashes().for_each(|h| {
            eligible_blobs.remove(h);
        });
        let subpackages = match pkg.subpackages().await {
            Ok(subpackages) => subpackages,
            Err(e) => {
                // Blobs necessary for OTA are already protected by the base index.
                error!(
                    "Could not determine subpackages of open package {}. \
                     The subpackages will NOT be protected from GC: {:#}",
                    pkg.hash(),
                    anyhow!(e)
                );
                continue;
            }
        };
        for h in subpackages.into_hashes_undeduplicated() {
            if enqueued.insert(h) {
                let sub = match open_packages.get_or_insert(h, None).await {
                    Ok(sub) => sub,
                    Err(e) => {
                        // Blobs necessary for OTA are already protected by the base index.
                        error!(
                            "Could not determine blobs of {h}, a subpackage of open package {}. \
                             The subpackage will NOT be protected from GC: {:#}",
                            pkg.hash(),
                            anyhow!(e)
                        );
                        continue;
                    }
                };
                to_visit.push(sub);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgradable_packages::UpgradablePackages;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fidl_fuchsia_fxfs as ffxfs;
    use fidl_fuchsia_io as fio;
    use fidl_fuchsia_pkg as fpkg;
    use fidl_fuchsia_pkg_garbagecollector::ManagerMarker as SpaceManagerMarker;
    use fidl_fuchsia_update::{
        CommitStatusProviderMarker, CommitStatusProviderRequest, CommitStatusProviderRequestStream,
    };
    use std::collections::HashMap;
    use std::pin::{Pin, pin};
    use std::task::Poll;
    use zx::Peered as _;

    fn create_test_gc_service(
        executor: &mut fasync::TestExecutor,
    ) -> (Arc<GcService>, fio::DirectoryRequestStream, CommitStatusProviderRequestStream) {
        create_test_gc_service_with_upgradable_packages(executor, None)
    }

    fn create_test_gc_service_with_upgradable_packages(
        executor: &mut fasync::TestExecutor,
        upgradable_packages: Option<UpgradablePackages>,
    ) -> (Arc<GcService>, fio::DirectoryRequestStream, CommitStatusProviderRequestStream) {
        let (gc_service, blobfs_dir_stream, _blob_reader_stream, commit_stream) =
            create_test_gc_service_with_blob_reader(executor, upgradable_packages);
        (gc_service, blobfs_dir_stream, commit_stream)
    }

    fn create_test_gc_service_with_blob_reader(
        executor: &mut fasync::TestExecutor,
        upgradable_packages: Option<UpgradablePackages>,
    ) -> (
        Arc<GcService>,
        fio::DirectoryRequestStream,
        ffxfs::BlobReaderRequestStream,
        CommitStatusProviderRequestStream,
    ) {
        let (blobfs, blobfs_dir_stream, blob_reader_stream, _) = blobfs::Client::new_test();
        let (commit_status_provider, commit_stream) =
            fidl::endpoints::create_proxy_and_stream::<CommitStatusProviderMarker>();

        let blobfs_clone = blobfs.clone();
        let mut setup_fut = pin!(async move {
            let bootfs = fidl::endpoints::create_proxy::<fio::DirectoryMarker>().0;
            let (_, open_packages) = crate::root_dir::new(bootfs, blobfs_clone).await.unwrap();
            let package_index = Arc::new(PackageIndex::new(blobfs.clone(), upgradable_packages));
            GcService::new(
                blobfs,
                Arc::new(BaseIndex::new_test_only(HashSet::new(), [])),
                Arc::new(CacheIndex::new_test_only(HashSet::new(), [])),
                package_index,
                open_packages,
                commit_status_provider,
            )
        });
        let gc_service = match executor.run_until_stalled(&mut setup_fut) {
            Poll::Ready(gc_service) => gc_service,
            Poll::Pending => panic!("setup stalled"),
        };
        (gc_service, blobfs_dir_stream, blob_reader_stream, commit_stream)
    }

    fn set_committed(
        executor: &mut fasync::TestExecutor,
        gc_service: &GcService,
        commit_stream: &mut CommitStatusProviderRequestStream,
    ) -> zx::EventPair {
        let mut fut = pin!(gc_service.commit_event_pair());
        assert!(executor.run_until_stalled(&mut fut).is_pending());
        let (event_pair_peer, event_pair) = zx::EventPair::create();
        event_pair_peer.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).unwrap();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(event_pair).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert_matches!(executor.run_until_stalled(&mut fut), Poll::Ready(Ok(_)));
        event_pair_peer
    }

    fn complete_empty_blobfs_list<F: Future + Unpin>(
        executor: &mut fasync::TestExecutor,
        gc_fut: &mut F,
        blobfs_dir_stream: &mut fio::DirectoryRequestStream,
    ) -> F::Output {
        let mut cloned_stream = match executor.run_until_stalled(&mut blobfs_dir_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Clone { request, .. }))) => {
                fidl::endpoints::ServerEnd::<fio::DirectoryMarker>::new(request.into_channel())
                    .into_stream()
            }
            other => panic!("unexpected blobfs dir request: {other:?}"),
        };

        match executor.run_until_stalled(&mut cloned_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Rewind { responder }))) => {
                responder.send(zx::sys::ZX_OK).unwrap();
            }
            other => panic!("unexpected cloned dir request: {other:?}"),
        }
        assert!(executor.run_until_stalled(gc_fut).is_pending());

        match executor.run_until_stalled(&mut cloned_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::ReadDirents {
                max_bytes: _,
                responder,
            }))) => {
                responder.send(zx::sys::ZX_OK, &[]).unwrap();
            }
            other => panic!("unexpected cloned dir request: {other:?}"),
        }
        match executor.run_until_stalled(gc_fut) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("gc_fut did not complete after empty blobfs list"),
        }
    }

    /// Fails the blobfs listing of a GC pass, and returns the output of `gc_fut` once the pass
    /// fails.
    fn fail_blobfs_list<F: Future + Unpin>(
        executor: &mut fasync::TestExecutor,
        gc_fut: &mut F,
        blobfs_dir_stream: &mut fio::DirectoryRequestStream,
    ) -> F::Output {
        let mut cloned_stream = match executor.run_until_stalled(&mut blobfs_dir_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Clone { request, .. }))) => {
                fidl::endpoints::ServerEnd::<fio::DirectoryMarker>::new(request.into_channel())
                    .into_stream()
            }
            other => panic!("unexpected blobfs dir request: {other:?}"),
        };

        match executor.run_until_stalled(&mut cloned_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Rewind { responder }))) => {
                responder.send(zx::sys::ZX_ERR_IO).unwrap();
            }
            other => panic!("unexpected cloned dir request: {other:?}"),
        }
        match executor.run_until_stalled(gc_fut) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("gc_fut did not complete after failed blobfs list"),
        }
    }

    /// Serves the blobfs directory listing at the start of `gc_fut`, reporting `resident` as the
    /// blobs in blobfs. Leaves `gc_fut` pending at its next await point after `list_known_blobs`.
    fn serve_blobfs_list<F: Future + Unpin>(
        executor: &mut fasync::TestExecutor,
        gc_fut: &mut F,
        blobfs_dir_stream: &mut fio::DirectoryRequestStream,
        resident: impl IntoIterator<Item = fuchsia_hash::Hash>,
    ) {
        let mut dirents = vec![];
        for blob in resident {
            let name = blob.to_string();
            dirents.extend_from_slice(&0u64.to_le_bytes());
            dirents.push(u8::try_from(name.len()).unwrap());
            dirents.push(fio::DirentType::File.into_primitive());
            dirents.extend_from_slice(name.as_bytes());
        }

        assert!(executor.run_until_stalled(gc_fut).is_pending());
        let mut cloned_stream = match executor.run_until_stalled(&mut blobfs_dir_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Clone { request, .. }))) => {
                fidl::endpoints::ServerEnd::<fio::DirectoryMarker>::new(request.into_channel())
                    .into_stream()
            }
            other => panic!("unexpected blobfs dir request: {other:?}"),
        };
        match executor.run_until_stalled(&mut cloned_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Rewind { responder }))) => {
                responder.send(zx::sys::ZX_OK).unwrap();
            }
            other => panic!("unexpected cloned dir request: {other:?}"),
        }
        // An empty response ends the listing.
        for dirents in [dirents, vec![]] {
            assert!(executor.run_until_stalled(gc_fut).is_pending());
            match executor.run_until_stalled(&mut cloned_stream.next()) {
                Poll::Ready(Some(Ok(fio::DirectoryRequest::ReadDirents {
                    max_bytes: _,
                    responder,
                }))) => {
                    responder.send(zx::sys::ZX_OK, &dirents).unwrap();
                }
                other => panic!("unexpected cloned dir request: {other:?}"),
            }
        }
    }

    /// Drives a GC pass to completion, serving its blobfs requests, reporting `resident` as the
    /// blobs in blobfs, and serving its reads of the meta.fars in `meta_fars`. Returns the blobs
    /// the pass deleted.
    fn serve_gc_pass<F: Future<Output = Result<(), SpaceErrorCode>> + Unpin>(
        executor: &mut fasync::TestExecutor,
        gc_fut: &mut F,
        blobfs_dir_stream: &mut fio::DirectoryRequestStream,
        resident: impl IntoIterator<Item = fuchsia_hash::Hash>,
        blob_reader_stream: &mut ffxfs::BlobReaderRequestStream,
        meta_fars: &HashMap<fuchsia_hash::Hash, Vec<u8>>,
    ) -> HashSet<fuchsia_hash::Hash> {
        serve_blobfs_list(executor, gc_fut, blobfs_dir_stream, resident);

        let mut deleted = HashSet::new();
        loop {
            if let Poll::Ready(res) = executor.run_until_stalled(gc_fut) {
                assert_matches!(res, Ok(()));
                return deleted;
            }
            if let Poll::Ready(Some(Ok(request))) =
                executor.run_until_stalled(&mut blob_reader_stream.next())
            {
                let ffxfs::BlobReaderRequest::GetVmo { blob_hash, .. } = &request;
                let blob_hash = fuchsia_hash::Hash::from(*blob_hash);
                respond_with_blob(request, blob_hash, &meta_fars[&blob_hash]);
                continue;
            }
            match executor.run_until_stalled(&mut blobfs_dir_stream.next()) {
                Poll::Ready(Some(Ok(fio::DirectoryRequest::Unlink {
                    name,
                    options: _,
                    responder,
                }))) => {
                    assert!(deleted.insert(name.parse().unwrap()));
                    responder.send(Ok(())).unwrap();
                }
                other => panic!("unexpected blobfs dir request: {other:?}"),
            }
        }
    }

    fn next_blob_reader_request(
        executor: &mut fasync::TestExecutor,
        blob_reader_stream: &mut ffxfs::BlobReaderRequestStream,
    ) -> ffxfs::BlobReaderRequest {
        match executor.run_until_stalled(&mut blob_reader_stream.next()) {
            Poll::Ready(Some(Ok(request))) => request,
            other => panic!("unexpected blob reader request: {other:?}"),
        }
    }

    #[fuchsia::test]
    fn gc_after_commit_waits_until_boot_timeline_deadline() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting for commit",
            }
        );

        // At 10s on the boot timeline, signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting until earliest boot time",
            }
        );

        // At 14s on the boot timeline, GC should still be waiting.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(14_000_000_000));
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());

        // Simulate 1s of suspend advancing boot time to 15s while monotonic time stays at 14s.
        executor.set_fake_boot_to_mono_offset(zx::BootDuration::from_seconds(1));
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "running gc",
            }
        );
        let res = complete_empty_blobfs_list(&mut executor, &mut gc_fut, &mut blobfs_dir_stream);
        assert_matches!(res, Ok(()));
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc completed",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_runs_immediately_if_deadline_already_passed() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        // At 20s on the boot timeline (past the 15s deadline), signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(20_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "running gc",
            }
        );
        let res = complete_empty_blobfs_list(&mut executor, &mut gc_fut, &mut blobfs_dir_stream);
        assert_matches!(res, Ok(()));
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc completed",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_skips_when_committed_before_this_boot() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        // At 5s (before the 15s deadline), signal USER_0 without USER_1: the system was already
        // committed before this boot, so post-commit GC completes immediately without waiting for
        // `earliest_time` or listing blobfs.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(5_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).unwrap();
        assert_matches!(executor.run_until_stalled(&mut gc_fut), Poll::Ready(Ok(())));
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc unnecessary",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_skips_if_gc_already_ran() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut post_commit_gc_fut: Pin<&mut _> =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 10s on the boot timeline, signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 12s (before the 15s post-commit GC deadline), run a manual GC to completion.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(12_000_000_000));
        {
            let mut manual_gc_fut = pin!(gc_service.gc());
            assert!(executor.run_until_stalled(&mut manual_gc_fut).is_pending());
            let res = complete_empty_blobfs_list(
                &mut executor,
                &mut manual_gc_fut,
                &mut blobfs_dir_stream,
            );
            assert_matches!(res, Ok(()));
        }

        // At 15s on the boot timeline, post-commit GC wakes up, sees GC already ran, and skips.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        assert_matches!(executor.run_until_stalled(&mut post_commit_gc_fut), Poll::Ready(Ok(())));
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc already ran",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_runs_if_manual_gc_failed_before_commit() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut post_commit_gc_fut: Pin<&mut _> =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 5s, a manual GC fails because the system is not committed yet.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(5_000_000_000));
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.gc())),
            Poll::Ready(Err(SpaceErrorCode::PendingCommit))
        );

        // At 10s on the boot timeline, signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 15s, post-commit GC runs a pass, because the failed manual GC did not count as one.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        let res = complete_empty_blobfs_list(
            &mut executor,
            &mut post_commit_gc_fut,
            &mut blobfs_dir_stream,
        );
        assert_matches!(res, Ok(()));
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc completed",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_runs_if_earlier_gc_pass_failed() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut post_commit_gc_fut: Pin<&mut _> =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 10s on the boot timeline, signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 12s, a manual GC pass fails partway because blobfs cannot be listed.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(12_000_000_000));
        {
            let mut manual_gc_fut = pin!(gc_service.gc());
            assert!(executor.run_until_stalled(&mut manual_gc_fut).is_pending());
            let res = fail_blobfs_list(&mut executor, &mut manual_gc_fut, &mut blobfs_dir_stream);
            assert_matches!(res, Err(SpaceErrorCode::Internal));
        }

        // At 15s, post-commit GC runs a pass, because the failed pass did not complete.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        let res = complete_empty_blobfs_list(
            &mut executor,
            &mut post_commit_gc_fut,
            &mut blobfs_dir_stream,
        );
        assert_matches!(res, Ok(()));
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc completed",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_skips_if_concurrent_gc_completes_at_deadline() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut post_commit_gc_fut: Pin<&mut _> =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 10s on the boot timeline, signal USER_0 | USER_1.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // At 12s, start a manual GC that holds the lock while waiting on blobfs listing.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(12_000_000_000));
        let mut manual_gc_fut = pin!(gc_service.gc());
        assert!(executor.run_until_stalled(&mut manual_gc_fut).is_pending());

        // At 15s, the post-commit GC timer fires, and post_commit_gc_fut waits on the GC lock.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting for gc lock",
            }
        );

        // Complete the manual GC; post_commit_gc_fut then acquires the lock and skips.
        let res =
            complete_empty_blobfs_list(&mut executor, &mut manual_gc_fut, &mut blobfs_dir_stream);
        assert_matches!(res, Ok(()));

        assert_matches!(executor.run_until_stalled(&mut post_commit_gc_fut), Poll::Ready(Ok(())));
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc already ran",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_waits_for_upgradable_packages_without_locking_package_index() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let upgradable_packages =
            UpgradablePackages::new(Arc::new(CacheIndex::new_test_only(HashSet::new(), [])));
        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service_with_upgradable_packages(
                &mut executor,
                Some(upgradable_packages),
            );
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        // Advance past earliest_time without initializing UpgradablePackages yet.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting for upgradable packages",
            }
        );

        // Neither blobfs listing nor a package index lock should start until set_upgradable_urls
        // has initialized UpgradablePackages, so the package index can still be changed.
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_matches!(
            executor.run_until_stalled(&mut pin!(
                gc_service.package_index.set_retained_blobs(HashSet::new())
            )),
            Poll::Ready(())
        );

        // Once SetUpgradableUrls runs, GC proceeds to list blobs and completes. SetUpgradableUrls
        // does not block because GC waits for initialization without holding any locks.
        assert_matches!(
            executor.run_until_stalled(&mut pin!(
                gc_service.package_index.set_upgradable_urls(vec![], &gc_service.base_packages)
            )),
            Poll::Ready(Ok(()))
        );
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        let res = complete_empty_blobfs_list(&mut executor, &mut gc_fut, &mut blobfs_dir_stream);
        assert_matches!(res, Ok(()));
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc completed",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_skips_when_manual_gc_waits_for_upgradable_packages() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let upgradable_packages =
            UpgradablePackages::new(Arc::new(CacheIndex::new_test_only(HashSet::new(), [])));
        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service_with_upgradable_packages(
                &mut executor,
                Some(upgradable_packages),
            );
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let earliest_time = zx::BootInstant::ZERO + zx::BootDuration::from_seconds(15);
        let mut post_commit_gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), earliest_time, Arc::clone(&inspect)));

        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        // Commit at 10s, start manual GC at 12s (which waits on UpgradablePackages without holding
        // any locks), and let the post-commit GC deadline pass at 15s.
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(12_000_000_000));
        let mut manual_gc_fut = pin!(gc_service.gc());
        assert!(executor.run_until_stalled(&mut manual_gc_fut).is_pending());

        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(15_000_000_000));
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting for upgradable packages",
            }
        );

        // Once UpgradablePackages initializes, manual GC completes a single blobfs pass and
        // post-commit GC skips instead of running a second pass.
        assert_matches!(
            executor.run_until_stalled(&mut pin!(
                gc_service.package_index.set_upgradable_urls(vec![], &gc_service.base_packages)
            )),
            Poll::Ready(Ok(()))
        );
        assert!(executor.run_until_stalled(&mut manual_gc_fut).is_pending());
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "waiting for gc lock",
            }
        );

        let res =
            complete_empty_blobfs_list(&mut executor, &mut manual_gc_fut, &mut blobfs_dir_stream);
        assert_matches!(res, Ok(()));
        assert_matches!(executor.run_until_stalled(&mut post_commit_gc_fut), Poll::Ready(Ok(())));
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "gc already ran",
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_reports_error_if_commit_status_unavailable() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, _blobfs_dir_stream, commit_stream) = create_test_gc_service(&mut executor);
        drop(commit_stream);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let mut gc_fut =
            pin!(gc_after_commit(gc_service, zx::BootInstant::ZERO, Arc::clone(&inspect)));
        let err = match executor.run_until_stalled(&mut gc_fut) {
            Poll::Ready(Err(e)) => e,
            other => panic!("unexpected post-commit gc result: {other:?}"),
        };
        assert!(format!("{err:#}").starts_with("while getting event pair: "), "{err:#}");
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": format!("error: {err:#}"),
            }
        );
    }

    #[fuchsia::test]
    fn gc_after_commit_reports_error_if_gc_pass_fails() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        let mut gc_fut =
            pin!(gc_after_commit(gc_service, zx::BootInstant::ZERO, Arc::clone(&inspect)));
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let (p0, p1) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(p1).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        p0.signal_peer(zx::Signals::NONE, zx::Signals::USER_0 | zx::Signals::USER_1).unwrap();
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        let err = match fail_blobfs_list(&mut executor, &mut gc_fut, &mut blobfs_dir_stream) {
            Err(e) => e,
            Ok(()) => panic!("post-commit gc unexpectedly succeeded"),
        };
        assert_eq!(format!("{err:#}"), "while running gc: Internal");
        assert_data_tree!(
            @executor executor,
            inspector,
            root: {
                "post_commit_gc": "error: while running gc: Internal",
            }
        );
    }

    fn respond_with_blob(
        request: ffxfs::BlobReaderRequest,
        expected_hash: fuchsia_hash::Hash,
        contents: &[u8],
    ) {
        match request {
            ffxfs::BlobReaderRequest::GetVmo { blob_hash, responder } => {
                assert_eq!(fuchsia_hash::Hash::from(blob_hash), expected_hash);
                let vmo = zx::Vmo::create(contents.len() as u64).unwrap();
                vmo.write(contents, 0).unwrap();
                responder.send(Ok(vmo)).unwrap();
            }
        }
    }

    #[fuchsia::test]
    fn set_upgradable_urls_waits_for_gc_pass_in_progress() {
        let mut executor = fasync::TestExecutor::new();
        let upgradable_packages =
            UpgradablePackages::new(Arc::new(CacheIndex::new_test_only(HashSet::new(), [])));
        let (gc_service, mut blobfs_dir_stream, mut blob_reader_stream, mut commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, Some(upgradable_packages));

        const URL: &str = "fuchsia-pkg://fuchsia.com/upgradable";
        let old_content = fuchsia_hash::Hash::from([1u8; 32]);
        let old_meta = fuchsia_hash::Hash::from([2u8; 32]);
        let old_meta_bytes = crate::test_utils::get_meta_far("upgradable", [old_content], []);

        let new_content = fuchsia_hash::Hash::from([3u8; 32]);
        let new_meta = fuchsia_hash::Hash::from([4u8; 32]);
        let new_meta_bytes = crate::test_utils::get_meta_far("upgradable", [new_content], []);

        let unowned = fuchsia_hash::Hash::from([5u8; 32]);

        // Open `new_meta`, simulating a client that wrote it with
        // `PackageCache.Get(..., OpenPackageTracking)` and keeps it open until `SetUpgradableUrls`
        // returns.
        let new_pkg_dir = {
            let mut open_fut = pin!(gc_service.open_packages.get_or_insert(new_meta, None));
            assert!(executor.run_until_stalled(&mut open_fut).is_pending());
            let request = next_blob_reader_request(&mut executor, &mut blob_reader_stream);
            respond_with_blob(request, new_meta, &new_meta_bytes);
            match executor.run_until_stalled(&mut open_fut) {
                Poll::Ready(Ok(dir)) => dir,
                other => panic!("unexpected get_or_insert result: {other:?}"),
            }
        };

        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{URL}?hash={old_meta}") }],
                &gc_service.base_packages,
            ))),
            Poll::Ready(Ok(()))
        );

        let _event_pair_peer = set_committed(&mut executor, &gc_service, &mut commit_stream);

        let mut gc_fut = pin!(gc_service.gc());
        serve_blobfs_list(
            &mut executor,
            &mut gc_fut,
            &mut blobfs_dir_stream,
            [old_content, old_meta, new_content, new_meta, unowned],
        );
        // Advance `gc_fut` into `package_index.protected_blobs().await`, where it locks the index
        // for reading and pauses reading `old_meta`.
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());
        let get_old_meta = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        // Try to replace `old_meta` with `new_meta` while the pass is reading `old_meta`; `set_fut`
        // must wait for the pass to finish.
        let mut set_fut = pin!(gc_service.package_index.set_upgradable_urls(
            vec![fpkg::PackageUrl { url: format!("{URL}?hash={new_meta}") }],
            &gc_service.base_packages,
        ));
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());

        respond_with_blob(get_old_meta, old_meta, &old_meta_bytes);
        assert!(executor.run_until_stalled(&mut gc_fut).is_pending());

        // The pass protects both the upgradable package from the start of the pass (`old_meta`) and
        // the open package (`new_meta`), deleting only `unowned`. While `Unlink` is in flight,
        // `set_fut` must still wait because the pass holds `GcView` until blob deletion completes.
        let unlink_responder = match executor.run_until_stalled(&mut blobfs_dir_stream.next()) {
            Poll::Ready(Some(Ok(fio::DirectoryRequest::Unlink {
                name,
                options: _,
                responder,
            }))) => {
                assert_eq!(name.parse::<fuchsia_hash::Hash>().unwrap(), unowned);
                responder
            }
            other => panic!("unexpected blobfs dir request: {other:?}"),
        };
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());

        unlink_responder.send(Ok(())).unwrap();
        assert_matches!(executor.run_until_stalled(&mut gc_fut), Poll::Ready(Ok(())));
        assert!(executor.run_until_stalled(&mut blobfs_dir_stream.next()).is_pending());

        // The replacement completes once the pass finishes. After the client closes the new
        // package, it is protected as an upgradable package and the old package is collected.
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Ready(Ok(())));
        drop(new_pkg_dir);
        let mut gc_fut = pin!(gc_service.gc());
        assert_eq!(
            serve_gc_pass(
                &mut executor,
                &mut gc_fut,
                &mut blobfs_dir_stream,
                [old_content, old_meta, new_content, new_meta],
                &mut blob_reader_stream,
                &HashMap::from([(new_meta, new_meta_bytes)]),
            ),
            HashSet::from([old_content, old_meta])
        );
    }

    #[fuchsia::test]
    fn gc_protects_package_retained_by_set_retained_index_in_progress() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, mut blobfs_dir_stream, mut blob_reader_stream, mut commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, None);
        let content = fuchsia_hash::Hash::from([1u8; 32]);
        let meta = fuchsia_hash::Hash::from([2u8; 32]);
        let unowned = fuchsia_hash::Hash::from([3u8; 32]);
        let meta_far = crate::test_utils::get_meta_far("pkg", [content], []);
        let _event_pair_peer = set_committed(&mut executor, &gc_service, &mut commit_stream);

        // Replacing the retained index swaps in the new retained index, then pauses while reading
        // `meta` to find the blobs of the newly retained package.
        let mut set_fut = pin!(gc_service.package_index.set_retained_index(HashSet::from([meta])));
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());
        let get_meta = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        // A GC pass started now does not wait for the replacement to finish. It protects the newly
        // retained package by reading `meta` itself.
        let mut gc_fut = pin!(gc_service.gc());
        assert_eq!(
            serve_gc_pass(
                &mut executor,
                &mut gc_fut,
                &mut blobfs_dir_stream,
                [content, meta, unowned],
                &mut blob_reader_stream,
                &HashMap::from([(meta, meta_far.clone())]),
            ),
            HashSet::from([unowned])
        );

        respond_with_blob(get_meta, meta, &meta_far);
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn gc_protects_package_retained_by_retain_package_in_progress() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, mut blobfs_dir_stream, mut blob_reader_stream, mut commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, None);
        let content = fuchsia_hash::Hash::from([1u8; 32]);
        let meta = fuchsia_hash::Hash::from([2u8; 32]);
        let unowned = fuchsia_hash::Hash::from([3u8; 32]);
        let meta_far = crate::test_utils::get_meta_far("pkg", [content], []);
        let _event_pair_peer = set_committed(&mut executor, &gc_service, &mut commit_stream);

        // Retaining an already cached package adds the package to the retained index, then pauses
        // while reading `meta` to find the package's required blobs.
        let mut retain_fut = pin!(gc_service.package_index.retain_package(meta));
        assert!(executor.run_until_stalled(&mut retain_fut).is_pending());
        let get_meta = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        // A GC pass started now does not wait for the retain to finish. It protects the package's
        // required blobs by reading `meta` itself.
        let mut gc_fut = pin!(gc_service.gc());
        assert_eq!(
            serve_gc_pass(
                &mut executor,
                &mut gc_fut,
                &mut blobfs_dir_stream,
                [content, meta, unowned],
                &mut blob_reader_stream,
                &HashMap::from([(meta, meta_far.clone())]),
            ),
            HashSet::from([unowned])
        );

        respond_with_blob(get_meta, meta, &meta_far);
        assert_matches!(executor.run_until_stalled(&mut retain_fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn gc_protects_package_retained_by_set_retained_index_while_in_writing_index() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, mut blobfs_dir_stream, mut blob_reader_stream, mut commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, None);
        let content0 = fuchsia_hash::Hash::from([1u8; 32]);
        let meta0 = fuchsia_hash::Hash::from([2u8; 32]);
        let meta0_far = crate::test_utils::get_meta_far("pkg0", [content0], []);

        let content1_a = fuchsia_hash::Hash::from([3u8; 32]);
        let content1_b = fuchsia_hash::Hash::from([4u8; 32]);
        let meta1 = fuchsia_hash::Hash::from([5u8; 32]);
        let meta1_far = crate::test_utils::get_meta_far("pkg1", [content1_a, content1_b], []);

        let unowned = fuchsia_hash::Hash::from([6u8; 32]);
        let meta_fars = HashMap::from([(meta0, meta0_far), (meta1, meta1_far)]);

        let _event_pair_peer = set_committed(&mut executor, &gc_service, &mut commit_stream);

        // Put `meta0` in the writing index with an empty blob set, and `meta1` with a partial blob
        // set (`content1_a` only, missing `content1_b`).
        let guard0 = match executor.run_until_stalled(&mut pin!(
            gc_service.package_index.start_writing(meta0, fpkg::GcProtection::OpenPackageTracking)
        )) {
            Poll::Ready(guard) => guard,
            Poll::Pending => panic!("start_writing stalled"),
        };
        let guard1 = match executor.run_until_stalled(&mut pin!(
            gc_service.package_index.start_writing(meta1, fpkg::GcProtection::OpenPackageTracking)
        )) {
            Poll::Ready(guard) => guard,
            Poll::Pending => panic!("start_writing stalled"),
        };
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.add_blobs(
                meta1,
                HashSet::from([content1_a]),
                fpkg::GcProtection::OpenPackageTracking,
            ))),
            Poll::Ready(Ok(()))
        );

        // Replacing the retained index copies any writing-index blobs into the retained index as
        // `RequiredBlobs::Partial`, then pauses reading the meta.fars from blobfs.
        let mut set_fut =
            pin!(gc_service.package_index.set_retained_index(HashSet::from([meta0, meta1])));
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());
        let first_get = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        // Both `OpenPackageTracking` operations finish before `set_retained_index` finishes reading
        // meta.fars, removing `meta0` and `meta1` from the writing index.
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.stop_writing(guard0))),
            Poll::Ready(Ok(()))
        );
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.stop_writing(guard1))),
            Poll::Ready(Ok(()))
        );

        // A GC pass running in this window still scans `meta0` and `meta1` because their retained
        // entries are `Partial`, protecting `content0` and `content1_b`.
        let mut gc_fut = pin!(gc_service.gc());
        assert_eq!(
            serve_gc_pass(
                &mut executor,
                &mut gc_fut,
                &mut blobfs_dir_stream,
                [content0, meta0, content1_a, content1_b, meta1, unowned],
                &mut blob_reader_stream,
                &meta_fars,
            ),
            HashSet::from([unowned])
        );

        let ffxfs::BlobReaderRequest::GetVmo { blob_hash, .. } = &first_get;
        let first_hash = fuchsia_hash::Hash::from(*blob_hash);
        respond_with_blob(first_get, first_hash, &meta_fars[&first_hash]);
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());
        let second_get = next_blob_reader_request(&mut executor, &mut blob_reader_stream);
        let ffxfs::BlobReaderRequest::GetVmo { blob_hash, .. } = &second_get;
        let second_hash = fuchsia_hash::Hash::from(*blob_hash);
        respond_with_blob(second_get, second_hash, &meta_fars[&second_hash]);
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn gc_protects_package_retained_by_retain_package_while_in_writing_index() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, mut blobfs_dir_stream, mut blob_reader_stream, mut commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, None);
        let content0 = fuchsia_hash::Hash::from([1u8; 32]);
        let content1 = fuchsia_hash::Hash::from([2u8; 32]);
        let meta = fuchsia_hash::Hash::from([3u8; 32]);
        let unowned = fuchsia_hash::Hash::from([4u8; 32]);
        let meta_far = crate::test_utils::get_meta_far("pkg", [content0, content1], []);
        let _event_pair_peer = set_committed(&mut executor, &gc_service, &mut commit_stream);

        // Put `meta` in the writing index with a partial blob set (`content0` only, missing
        // `content1`).
        let guard = match executor.run_until_stalled(&mut pin!(
            gc_service.package_index.start_writing(meta, fpkg::GcProtection::OpenPackageTracking)
        )) {
            Poll::Ready(guard) => guard,
            Poll::Pending => panic!("start_writing stalled"),
        };
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.add_blobs(
                meta,
                HashSet::from([content0]),
                fpkg::GcProtection::OpenPackageTracking,
            ))),
            Poll::Ready(Ok(()))
        );

        // Retaining `meta` copies `content0` into the retained index as `RequiredBlobs::Partial`,
        // then pauses while reading `meta` from blobfs.
        let mut retain_fut = pin!(gc_service.package_index.retain_package(meta));
        assert!(executor.run_until_stalled(&mut retain_fut).is_pending());
        let get_meta = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        // Finish the `OpenPackageTracking` write so `meta` is no longer in the writing index.
        assert_matches!(
            executor.run_until_stalled(&mut pin!(gc_service.package_index.stop_writing(guard))),
            Poll::Ready(Ok(()))
        );

        // A GC pass running before `retain_package` finishes reading `meta` still reads `meta`
        // itself and protects `content1`.
        let mut gc_fut = pin!(gc_service.gc());
        assert_eq!(
            serve_gc_pass(
                &mut executor,
                &mut gc_fut,
                &mut blobfs_dir_stream,
                [content0, content1, meta, unowned],
                &mut blob_reader_stream,
                &HashMap::from([(meta, meta_far.clone())]),
            ),
            HashSet::from([unowned])
        );

        respond_with_blob(get_meta, meta, &meta_far);
        assert_matches!(executor.run_until_stalled(&mut retain_fut), Poll::Ready(()));
    }

    #[fuchsia::test]
    fn caches_commit_event_pair_across_post_commit_gc_and_manager_connections() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, mut blobfs_dir_stream, mut commit_stream) =
            create_test_gc_service(&mut executor);
        let inspector = finspect::Inspector::default();
        let inspect =
            Arc::new(inspector.root().create_string("post_commit_gc", "waiting for commit"));

        // Start both `gc_after_commit` and a `Manager` client call before `CommitStatusProvider`
        // responds; both should coalesce onto a single `IsCurrentSystemCommitted` FIDL call.
        let mut post_commit_gc_fut =
            pin!(gc_after_commit(Arc::clone(&gc_service), zx::BootInstant::ZERO, inspect));
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());

        let (manager_proxy_1, manager_stream_1) =
            fidl::endpoints::create_proxy_and_stream::<SpaceManagerMarker>();
        let mut serve_fut_1 = pin!(serve(Arc::clone(&gc_service), manager_stream_1));
        let mut gc_call_1 = pin!(manager_proxy_1.gc());
        assert!(executor.run_until_stalled(&mut gc_call_1).is_pending());
        assert!(executor.run_until_stalled(&mut serve_fut_1).is_pending());

        let (event_pair_peer, event_pair) = zx::EventPair::create();
        match executor.run_until_stalled(&mut commit_stream.next()) {
            Poll::Ready(Some(Ok(CommitStatusProviderRequest::IsCurrentSystemCommitted {
                responder,
            }))) => {
                responder.send(event_pair).unwrap();
            }
            other => panic!("unexpected commit request: {other:?}"),
        }
        assert!(executor.run_until_stalled(&mut commit_stream.next()).is_pending());
        // Drop `commit_stream` to simulate `system-update-committer` stopping on idle and to prove
        // no further `IsCurrentSystemCommitted` calls are made.
        drop(commit_stream);

        // Before `USER_0` is signaled, `manager_proxy_1.gc()` fails with `PendingCommit` while
        // `post_commit_gc_fut` stays waiting for commit.
        assert!(executor.run_until_stalled(&mut post_commit_gc_fut).is_pending());
        assert!(executor.run_until_stalled(&mut serve_fut_1).is_pending());
        assert_matches!(
            executor.run_until_stalled(&mut gc_call_1),
            Poll::Ready(Ok(Err(SpaceErrorCode::PendingCommit)))
        );

        // Signal `USER_0` (without `USER_1`): `post_commit_gc_fut` finishes, and both the existing
        // `Manager` connection and a newly opened `Manager` connection reuse the cached eventpair.
        event_pair_peer.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).unwrap();
        assert_matches!(executor.run_until_stalled(&mut post_commit_gc_fut), Poll::Ready(Ok(())));

        {
            let mut pass_1 = pin!(futures::future::join(
                async {
                    let res = manager_proxy_1.gc().await;
                    drop(manager_proxy_1);
                    res
                },
                serve_fut_1,
            ));
            assert!(executor.run_until_stalled(&mut pass_1).is_pending());
            let (gc_res, serve_res) =
                complete_empty_blobfs_list(&mut executor, &mut pass_1, &mut blobfs_dir_stream);
            assert_matches!(gc_res, Ok(Ok(())));
            assert_matches!(serve_res, Ok(()));
        }

        let (manager_proxy_2, manager_stream_2) =
            fidl::endpoints::create_proxy_and_stream::<SpaceManagerMarker>();
        let mut pass_2 = pin!(futures::future::join(
            async {
                let res = manager_proxy_2.gc().await;
                drop(manager_proxy_2);
                res
            },
            serve(Arc::clone(&gc_service), manager_stream_2),
        ));
        assert!(executor.run_until_stalled(&mut pass_2).is_pending());
        let (gc_res, serve_res) =
            complete_empty_blobfs_list(&mut executor, &mut pass_2, &mut blobfs_dir_stream);
        assert_matches!(gc_res, Ok(Ok(())));
        assert_matches!(serve_res, Ok(()));
    }

    #[fuchsia::test]
    fn retain_package_does_not_wait_for_other_retains() {
        let mut executor = fasync::TestExecutor::new();
        let (gc_service, _blobfs_dir_stream, mut blob_reader_stream, _commit_stream) =
            create_test_gc_service_with_blob_reader(&mut executor, None);
        let meta0 = fuchsia_hash::Hash::from([1u8; 32]);
        let meta1 = fuchsia_hash::Hash::from([2u8; 32]);
        let content0 = fuchsia_hash::Hash::from([3u8; 32]);
        let content1 = fuchsia_hash::Hash::from([4u8; 32]);

        // While one retain is paused reading its meta.far, another retain starts reading its own.
        let mut retain0_fut = pin!(gc_service.package_index.retain_package(meta0));
        assert!(executor.run_until_stalled(&mut retain0_fut).is_pending());
        let get_meta0 = next_blob_reader_request(&mut executor, &mut blob_reader_stream);
        let mut retain1_fut = pin!(gc_service.package_index.retain_package(meta1));
        assert!(executor.run_until_stalled(&mut retain1_fut).is_pending());
        let get_meta1 = next_blob_reader_request(&mut executor, &mut blob_reader_stream);

        respond_with_blob(
            get_meta1,
            meta1,
            &crate::test_utils::get_meta_far("pkg1", [content1], []),
        );
        assert_matches!(executor.run_until_stalled(&mut retain1_fut), Poll::Ready(()));
        respond_with_blob(
            get_meta0,
            meta0,
            &crate::test_utils::get_meta_far("pkg0", [content0], []),
        );
        assert_matches!(executor.run_until_stalled(&mut retain0_fut), Poll::Ready(()));

        let mut protected_fut = pin!(async {
            let init = gc_service.package_index.wait_until_upgradable_packages_initialized().await;
            let view = gc_service.package_index.read_for_gc(init).await;
            view.protected_blobs().await
        });
        assert_eq!(
            executor.run_until_stalled(&mut protected_fut),
            Poll::Ready(HashSet::from([meta0, content0, meta1, content1]))
        );
        assert!(executor.run_until_stalled(&mut blob_reader_stream.next()).is_pending());
    }
}
