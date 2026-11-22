// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::retained::{RetainedBlobs, RetainedIndex};
use super::upgradable::{ResolvedUpgradable, StopOpening, UpgradableIndex};
use super::writing::{StopWriting, WritingIndex};
use crate::frozen_index::{BaseIndex, CacheIndex};
use crate::required_blobs::{RequiredBlobs, find_required_blobs_best_effort};
use fidl_fuchsia_pkg as fpkg;
use fuchsia_hash::Hash;
use fuchsia_inspect as finspect;
use fuchsia_url::fuchsia_pkg::UnpinnedAbsolutePackageUrl;
use futures::channel::mpsc;
use futures::future::{BoxFuture, FutureExt as _};
use log::error;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// The packages, other than the base, cache, and open packages, that pkg-cache protects from
/// garbage collection:
/// * the packages being written
/// * the retained packages and blobs
/// * the upgradable packages
/// * the upgradable packages currently being opened.
///
/// The pkg-cache properties that inform the design of this type are as follows:
/// a. blob fetchers add blobs to this index before checking their presence in storage
/// b. garbage collection determines which blobs to delete by first obtaining the list of all
///    present blobs from storage and then obtaining the list of all protected blobs from this type,
///    which it subtracts from the former
/// c. some modifications to this index must be performed in pairs (e.g. adding and then removing a
///    package to the writing index during resolution) that span await points
///
/// To guarantee that blob fetchers never skip fetching a blob that is present but about to be
/// deleted by garbage collection (i.e. to make sure a. and b. work together), additive mutations to
/// this type need to be blocked during garbage collection, which spans await points (garbage
/// collection reads meta.fars to determine subpackage and content blob hashes and unlinks blobs
/// from storage, both of which are async).
///
/// To prevent dropped futures from losing the paired cleanup actions in c., [`CleanupGuard`]
/// triggers subtractive cleanup synchronously on drop.
///
/// To meet these two requirements with a single lock, [`locked::LockedIndices`] wraps [`Indices`]
/// and an `mpsc::UnboundedReceiver` inside one `async_lock::Mutex` in a private submodule:
/// * Every access to [`Indices`] goes through [`locked::LockedIndices::lock`], which acquires the
///   async mutex and synchronously drains all pending [`Cleanup`] messages before returning the
///   guard.
/// * A garbage collection pass calls [`Self::acquire_gc_lock`] and holds the returned [`GcGuard`]
///   (and therefore the async mutex) until blob deletion finishes, blocking all other index
///   accesses for the duration of the pass.
/// * [`CleanupGuard::drop`] sends [`Cleanup`] messages synchronously onto the
///   `mpsc::UnboundedSender` without blocking. Deferring subtractive cleanup until the next lock
///   acquisition only ever extends blob protection, and draining on lock guarantees any subsequent
///   [`Self::acquire_gc_lock`] or [`Self::record_lazy_inspect`] observes all prior drops without
///   needing a background cleanup task (keeping unread queue length bounded by peak concurrent
///   guards).
pub struct PackageIndex {
    indices: locked::LockedIndices,
    upgradable: Option<UpgradableConfig>,
    blobfs: blobfs::Client,
}

#[derive(Debug)]
struct UpgradableConfig {
    cache_packages: Arc<CacheIndex>,
    initialized: async_lock::OnceCell<()>,
}

impl std::fmt::Debug for PackageIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageIndex")
            .field("indices", &self.indices)
            .field("upgradable", &self.upgradable)
            .finish_non_exhaustive()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AddBlobsError {
    #[error("adding blobs to the writing index")]
    Writing(#[source] super::writing::AddBlobsError),
}

/// Proof that upgradable packages are initialized or disabled, obtained from
/// `PackageIndex::wait_until_upgradable_packages_initialized`.
///
/// `PackageIndex::acquire_gc_lock` requires this, so garbage collection waits for upgradable
/// packages to be initialized before it locks the index, and never collects upgradable packages
/// before they are known.
#[derive(Clone, Copy, Debug)]
pub struct UpgradablePackagesInitialized(());

/// Subtractive cleanup action enqueued synchronously when a [`CleanupGuard`] is dropped and
/// applied on the next [`locked::LockedIndices::lock`] acquisition.
#[derive(Debug)]
enum Cleanup {
    StopWriting(StopWriting),
    StopOpening(StopOpening),
}

/// RAII guard returned by [`PackageIndex::start_writing`] and [`PackageIndex::open_upgradable`]
/// that keeps a package protected from garbage collection until dropped.
///
/// To guarantee continuous protection from garbage collection, callers caching or opening a package
/// with [`fpkg::GcProtection::OpenPackageTracking`] should insert the package into open package
/// tracking before dropping this guard.
#[derive(Debug)]
#[must_use = "dropping CleanupGuard removes GC protection"]
pub struct CleanupGuard {
    active: Option<(Cleanup, mpsc::UnboundedSender<Cleanup>)>,
}

impl CleanupGuard {
    fn new(cleanup: Cleanup, sender: mpsc::UnboundedSender<Cleanup>) -> Self {
        Self { active: Some((cleanup, sender)) }
    }

    fn noop() -> Self {
        Self { active: None }
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some((cleanup, sender)) = self.active.take() {
            let _: Result<(), _> = sender.unbounded_send(cleanup);
        }
    }
}

impl PackageIndex {
    /// Creates an empty `PackageIndex` that reads meta.fars from `blobfs` to determine the
    /// required blobs of retained and upgradable packages. Upgradable packages are disabled if
    /// `upgradable_packages` is `None`.
    pub fn new(blobfs: blobfs::Client, upgradable_packages: Option<Arc<CacheIndex>>) -> Self {
        let upgradable = upgradable_packages.map(|cache_packages| UpgradableConfig {
            cache_packages,
            initialized: async_lock::OnceCell::new(),
        });
        Self { indices: locked::LockedIndices::new(Indices::new()), upgradable, blobfs }
    }

    /// Notifies the appropriate indices that the package with the given hash is going to be
    /// written, ensuring the meta.far blob is protected by the index until the returned
    /// [`CleanupGuard`] is dropped (for [`fpkg::GcProtection::OpenPackageTracking`]).
    pub async fn start_writing(
        &self,
        pkg: Hash,
        gc_protection: fpkg::GcProtection,
    ) -> CleanupGuard {
        let mut indices = self.indices.lock().await;
        match gc_protection {
            // Retained protection uses the retained package index instead of the writing and
            // open package indices so that the system-updater can have finer-grained control
            // over blob deletion during OTA.
            fpkg::GcProtection::Retained => CleanupGuard::noop(),
            fpkg::GcProtection::OpenPackageTracking => {
                let stop = indices.writing.start(pkg);
                CleanupGuard::new(Cleanup::StopWriting(stop), self.indices.cleanup_sender())
            }
        }
    }

    /// Associate additional blobs (e.g. subpackage meta.fars and content blobs) with a package
    /// that is being cached.
    ///
    /// The blobs are protected from garbage collection when this returns, so callers must add
    /// blobs before checking whether they are present in blobfs: a garbage collection pass in
    /// progress finishes deleting blobs before this returns.
    pub async fn add_blobs(
        &self,
        package_hash: Hash,
        additional_blobs: HashSet<Hash>,
        gc_protection: fpkg::GcProtection,
    ) -> Result<(), AddBlobsError> {
        self.indices.lock().await.add_blobs(package_hash, additional_blobs, gc_protection)
    }

    /// Records the complete set of required blobs (content blobs and transitive subpackage
    /// meta.fars and content blobs) for a package that has finished being fetched, marking its
    /// retained index entry [`RequiredBlobs::Complete`] if the package is retained.
    pub async fn complete_retained_blobs(&self, package_hash: Hash, blobs: HashSet<Hash>) {
        self.indices.lock().await.complete_retained_blobs(package_hash, blobs);
    }

    /// Replaces the retained index with one that tracks the given meta.far hashes.
    ///
    /// The packages are protected from garbage collection as soon as the new retained index is
    /// swapped in. This then determines their required blobs from the meta.fars already in blobfs,
    /// so that garbage collection passes don't have to. A pass that runs in between determines them
    /// itself (see [`GcGuard::protected_blobs`]).
    pub async fn set_retained_index(&self, meta_hashes: HashSet<Hash>) {
        let partial = {
            let mut indices = self.indices.lock().await;
            let () = indices.replace_retained_packages(meta_hashes);
            indices.retained.packages_with_partial_blobs().collect::<HashSet<_>>()
        };
        if partial.is_empty() {
            return;
        }

        // Read and parse the meta.fars without holding the index lock.
        let populated = super::retained::populate_retained_index(&self.blobfs, partial).await;
        let () = self.indices.lock().await.retained.add_known_blobs(populated);
    }

    /// Adds the package with the given meta.far hash to the retained index.
    ///
    /// The package is protected from garbage collection as soon as it is added. As with
    /// `Self::set_retained_index`, this then determines its required blobs from the meta.fars
    /// already in blobfs.
    pub async fn retain_package(&self, meta_hash: Hash) {
        let already_complete = {
            let mut indices = self.indices.lock().await;
            let () = indices.add_retained_package(meta_hash);
            indices.retained.has_complete_blobs(&meta_hash)
        };
        if already_complete {
            return;
        }

        // Read and parse the meta.fars without holding the index lock.
        let blobs = find_required_blobs_best_effort(
            &self.blobfs,
            &meta_hash,
            &async_lock::RwLock::new(HashMap::new()),
        )
        .await;
        let _: bool = self.indices.lock().await.retained.merge_blobs(&meta_hash, blobs);
    }

    /// Replaces the retained blobs with the given blob hashes.
    pub async fn set_retained_blobs(&self, blobs: HashSet<Hash>) {
        self.indices.lock().await.retained_blobs = RetainedBlobs::new(blobs);
    }

    /// Associates upgradable package URLs with the hashes they should resolve to.
    ///
    /// Waits for any active garbage collection pass to finish so the upgradable package set does
    /// not change during a pass. Callers must keep newly associated packages protected from garbage
    /// collection until this returns, e.g. by keeping them open.
    pub async fn set_upgradable_urls(
        &self,
        pinned_urls: Vec<fpkg::PackageUrl>,
        base_packages: &BaseIndex,
    ) -> Result<(), fpkg::SetUpgradableUrlsError> {
        let Some(upgradable) = &self.upgradable else {
            error!("SetUpgradableUrls called but upgradable packages are not enabled in pkg-cache");
            return Err(fpkg::SetUpgradableUrlsError::Internal);
        };
        let res =
            self.indices.lock().await.upgradable.set_upgradable_urls(pinned_urls, base_packages);
        let _: Result<&(), ()> = upgradable.initialized.set(()).await;
        res
    }

    /// Returns the hash that `url` resolves to as an upgradable package along with a
    /// [`CleanupGuard`] that keeps an upgraded package protected from garbage collection until
    /// dropped, or `None` if upgradable packages are disabled or `url` is not an upgradable
    /// package.
    ///
    /// Waits for upgradable packages to be initialized before resolving `url`. An inert
    /// [`CleanupGuard`] is returned when `url` resolves via the `CacheIndex` fallback, which is
    /// permanently protected.
    pub async fn open_upgradable(
        &self,
        url: &UnpinnedAbsolutePackageUrl,
    ) -> Option<(Hash, CleanupGuard)> {
        let upgradable = self.upgradable.as_ref()?;
        let (): &() = upgradable.initialized.wait().await;
        let mut indices = self.indices.lock().await;
        let (hash, guard) =
            match indices.upgradable.start_opening(url, &upgradable.cache_packages)? {
                ResolvedUpgradable::Upgradable(hash, stop) => (
                    hash,
                    CleanupGuard::new(Cleanup::StopOpening(stop), self.indices.cleanup_sender()),
                ),
                ResolvedUpgradable::CacheFallback(hash) => (hash, CleanupGuard::noop()),
            };
        Some((hash, guard))
    }

    /// Whether upgradable packages are enabled.
    pub fn upgradable_packages_enabled(&self) -> bool {
        self.upgradable.is_some()
    }

    /// Waits for upgradable packages to be initialized by the first call to
    /// `Self::set_upgradable_urls`. Returns immediately if upgradable packages are disabled.
    pub async fn wait_until_upgradable_packages_initialized(
        &self,
    ) -> UpgradablePackagesInitialized {
        if let Some(upgradable) = &self.upgradable {
            let (): &() = upgradable.initialized.wait().await;
        }
        UpgradablePackagesInitialized(())
    }

    /// Locks the index for a garbage collection pass.
    ///
    /// Drains any pending guard cleanups and returns a [`GcGuard`] holding the index mutex. Other
    /// index accesses wait until the [`GcGuard`] is dropped, so the pass must keep it until blob
    /// deletion completes.
    pub async fn acquire_gc_lock(&self, _: UpgradablePackagesInitialized) -> GcGuard<'_> {
        GcGuard { blobfs: &self.blobfs, indices: self.indices.lock().await }
    }

    /// Returns a callback to be given to `finspect::Node::record_lazy_child`.
    /// The callback holds a weak reference to the PackageIndex.
    pub fn record_lazy_inspect(
        self: &Arc<Self>,
    ) -> impl Fn() -> BoxFuture<'static, Result<finspect::Inspector, anyhow::Error>>
    + Send
    + Sync
    + 'static {
        let this = Arc::downgrade(self);
        move || {
            let this = this.clone();
            async move {
                let inspector = finspect::Inspector::default();
                if let Some(this) = this.upgrade() {
                    let indices = this.indices.lock().await;
                    let root = inspector.root();
                    let () = root.record_child("retained", |n| indices.retained.record_inspect(n));
                    let () = root.record_child("retained-blobs", |n| {
                        indices.retained_blobs.record_inspect(n)
                    });
                    let () = root.record_child("writing", |n| indices.writing.record_inspect(n));
                } else {
                    inspector.root().record_string("error", "the package index was dropped");
                }
                Ok(inspector)
            }
            .boxed()
        }
    }
}

/// Exclusive access to [`PackageIndex`] for a garbage collection pass, obtained from
/// [`PackageIndex::acquire_gc_lock`].
///
/// All other [`PackageIndex`] index accesses wait until this guard is dropped, so the pass must
/// keep it until blob deletion completes.
#[derive(Debug)]
#[must_use = "dropping GcGuard immediately unblocks index mutations during GC"]
pub struct GcGuard<'a> {
    blobfs: &'a blobfs::Client,
    indices: locked::IndicesGuard<'a>,
}

impl GcGuard<'_> {
    /// Returns the blobs protected by the index: the meta.fars of the packages being written and
    /// of the retained packages, the blobs known to be required by those packages, the retained
    /// blobs, and the upgradable and opening packages and their required blobs.
    ///
    /// Reads meta.fars from blobfs to determine the required blobs of upgradable and opening
    /// packages and of retained packages whose required blobs were not yet complete.
    pub async fn protected_blobs(&self) -> HashSet<Hash> {
        // `find_required_blobs_best_effort` only returns content and subpackage blobs; the root
        // `meta_hash`es are already in `protected` from `self.indices.all_known_blobs()`.
        let mut protected = self.indices.all_known_blobs();

        let memoized_packages = async_lock::RwLock::new(HashMap::new());
        for meta_hash in self.indices.retained.packages_with_partial_blobs() {
            let blobs =
                find_required_blobs_best_effort(self.blobfs, &meta_hash, &memoized_packages).await;
            protected.extend(blobs.into_hashes());
        }

        for meta_hash in self.indices.upgradable.protected_packages() {
            match find_required_blobs_best_effort(self.blobfs, &meta_hash, &memoized_packages).await
            {
                RequiredBlobs::Complete(blobs) => protected.extend(blobs),
                RequiredBlobs::Partial(blobs) => {
                    error!("upgradable package {meta_hash} missing or unreadable required blobs");
                    protected.extend(blobs);
                }
            }
        }

        protected
    }
}

#[derive(Debug)]
struct Indices {
    retained: RetainedIndex,
    retained_blobs: RetainedBlobs,
    writing: WritingIndex,
    upgradable: UpgradableIndex,
}

impl Indices {
    fn new() -> Self {
        Self {
            retained: RetainedIndex::new(),
            retained_blobs: RetainedBlobs::default(),
            writing: WritingIndex::new(),
            upgradable: UpgradableIndex::new(),
        }
    }

    fn apply_cleanup(&mut self, cleanup: Cleanup) {
        match cleanup {
            Cleanup::StopWriting(stop) => self.writing.stop(stop),
            Cleanup::StopOpening(stop) => self.upgradable.stop_opening(stop),
        }
    }

    fn add_blobs(
        &mut self,
        package_hash: Hash,
        additional_blobs: HashSet<Hash>,
        gc_protection: fpkg::GcProtection,
    ) -> Result<(), AddBlobsError> {
        // Always give the retained index information about dependent blobs to improve OTA forward
        // progress.
        let _: bool = self.retained.add_blobs(&package_hash, &additional_blobs);
        match gc_protection {
            fpkg::GcProtection::Retained => Ok(()),
            fpkg::GcProtection::OpenPackageTracking => self
                .writing
                .add_blobs(&package_hash, &additional_blobs)
                .map_err(AddBlobsError::Writing),
        }
    }

    fn complete_retained_blobs(&mut self, package_hash: Hash, blobs: HashSet<Hash>) {
        let _: bool = self.retained.merge_blobs(&package_hash, RequiredBlobs::Complete(blobs));
    }

    fn set_retained_index(&mut self, mut index: RetainedIndex) {
        // Populate the new retained index with the relevant blobs from the writing index.
        let retained_packages = index.retained_packages().copied().collect::<Vec<_>>();
        for hash in retained_packages {
            if let Some(blobs) = self.writing.required_blobs(&hash) {
                // TODO(https://fxbug.dev/42064103) Consider replacing this panic with an error, or
                // e.g. adding a method to the retained index that makes both unnecessary.
                assert!(index.add_blobs(&hash, blobs));
            }
        }

        // Replace our retained index with the new one, which will populate content blobs from the
        // old retained index.
        self.retained.replace(index);
    }

    /// Replaces the retained index with one that tracks the given packages, with the required blobs
    /// already known to the retained and writing indices.
    fn replace_retained_packages(&mut self, meta_hashes: impl IntoIterator<Item = Hash>) {
        let mut index = RetainedIndex::new();
        for meta_hash in meta_hashes {
            index.add_package(meta_hash);
        }
        self.set_retained_index(index);
    }

    fn add_retained_package(&mut self, hash: Hash) {
        self.retained.add_package(hash);
        if let Some(blobs) = self.writing.required_blobs(&hash) {
            assert!(self.retained.add_blobs(&hash, blobs));
        }
    }

    /// Returns all blobs protected by these indices whose hashes are known without reading
    /// meta.fars (including the root meta.far hashes of partial retained packages and upgradable
    /// packages, whose remaining required blobs must be read from blobfs).
    fn all_known_blobs(&self) -> HashSet<Hash> {
        let mut all = self.retained.all_blobs();
        all.extend(self.retained_blobs.all_blobs());
        all.extend(self.writing.all_blobs());
        all.extend(self.upgradable.protected_packages());
        all
    }
}

/// Encapsulates [`Indices`] and the guard-cleanup receiver behind a module privacy boundary so
/// [`Indices`] cannot be accessed without first draining pending [`super::CleanupGuard`] drops.
mod locked {
    use super::{Cleanup, Deref, DerefMut, Indices, mpsc};

    #[derive(Debug)]
    pub(super) struct LockedIndices {
        // Uses `Mutex` rather than `RwLock` because every lock acquisition drains
        // `cleanup_receiver` (requiring `&mut Inner`).
        inner: async_lock::Mutex<Inner>,
        cleanup_sender: mpsc::UnboundedSender<Cleanup>,
    }

    #[derive(Debug)]
    struct Inner {
        indices: Indices,
        cleanup_receiver: mpsc::UnboundedReceiver<Cleanup>,
    }

    #[derive(Debug)]
    pub(super) struct IndicesGuard<'a> {
        guard: async_lock::MutexGuard<'a, Inner>,
    }

    impl Deref for IndicesGuard<'_> {
        type Target = Indices;

        fn deref(&self) -> &Indices {
            &self.guard.indices
        }
    }

    impl DerefMut for IndicesGuard<'_> {
        fn deref_mut(&mut self) -> &mut Indices {
            &mut self.guard.indices
        }
    }

    impl LockedIndices {
        pub(super) fn new(indices: Indices) -> Self {
            let (cleanup_sender, cleanup_receiver) = mpsc::unbounded();
            Self {
                inner: async_lock::Mutex::new(Inner { indices, cleanup_receiver }),
                cleanup_sender,
            }
        }

        /// Acquires the mutex and drains all pending [`Cleanup`] messages before returning access
        /// to [`Indices`].
        pub(super) async fn lock(&self) -> IndicesGuard<'_> {
            let mut guard = self.inner.lock().await;
            while let Ok(cleanup) = guard.cleanup_receiver.try_recv() {
                guard.indices.apply_cleanup(cleanup);
            }
            IndicesGuard { guard }
        }

        pub(super) fn cleanup_sender(&self) -> mpsc::UnboundedSender<Cleanup> {
            self.cleanup_sender.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        add_meta_far_to_blobfs, expect_get_vmo_async, get_meta_far, next_get_vmo_async,
        respond_get_vmo,
    };
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async as fasync;
    use futures::StreamExt as _;
    use maplit::hashmap;
    use std::pin::pin;
    use std::task::Poll;

    fn hash(n: u8) -> Hash {
        Hash::from([n; 32])
    }

    #[fuchsia::test]
    fn set_retained_index_hashes_are_extended_with_writing_index_hashes() {
        let mut indices = Indices::new();
        let _: StopWriting = indices.writing.start(hash(0));
        indices
            .add_blobs(
                hash(0),
                HashSet::from([hash(1), hash(2)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1)])),
        }));

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2)])),
            }
        );
    }

    #[fuchsia::test]
    fn retained_index_is_not_informed_of_packages_it_does_not_track() {
        let mut indices = Indices::new();

        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
        }));

        // Install a package not tracked by the retained index.
        let _: StopWriting = indices.writing.start(hash(2));
        indices
            .add_blobs(hash(2), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
                hash(1) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
        assert_eq!(
            indices.writing.packages(),
            hashmap! {
                hash(2) => HashSet::from_iter([hash(10)]),
            }
        );
    }

    #[fuchsia::test]
    fn set_retained_index_to_self_is_nop() {
        let mut indices = Indices::new();
        let _: StopWriting = indices.writing.start(hash(3));
        let _: StopWriting = indices.writing.start(hash(5));
        indices
            .add_blobs(hash(3), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
            hash(3) => RequiredBlobs::Partial(HashSet::new()),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::new()),
        }));

        indices
            .add_blobs(hash(5), HashSet::from([hash(12)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        let retained_index = indices.retained.clone();

        indices.set_retained_index(retained_index.clone());
        assert_eq!(indices.retained, retained_index);
    }

    #[fuchsia::test]
    fn add_blobs_with_open_package_tracking_protection_adds_to_retained_and_writing() {
        let mut indices = Indices::new();

        let _: StopWriting = indices.writing.start(hash(2));
        indices
            .add_blobs(hash(2), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();
        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
        }));

        indices
            .add_blobs(hash(2), HashSet::from([hash(11)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(2) => RequiredBlobs::Partial(HashSet::from([hash(10), hash(11)]))
            }
        );
        assert_eq!(
            indices.writing.packages(),
            hashmap! {
                hash(2) => HashSet::from([hash(10), hash(11)])
            }
        );
    }

    #[fuchsia::test]
    fn add_blobs_with_retained_protection_adds_to_retained_index_only() {
        let mut indices = Indices::new();

        let _: StopWriting = indices.writing.start(hash(2));
        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::new())
        }));

        indices
            .add_blobs(hash(2), HashSet::from([hash(11)]), fpkg::GcProtection::Retained)
            .unwrap();

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(2) => RequiredBlobs::Partial(HashSet::from([hash(11)]))
            }
        );
        assert_eq!(
            indices.writing.packages(),
            hashmap! {
                hash(2) => HashSet::new()
            }
        );
    }

    #[fuchsia::test]
    fn add_blobs_errors_if_writing_index_in_wrong_state() {
        let mut indices = Indices::new();

        assert_matches!(
            indices.add_blobs(
                hash(2),
                HashSet::from([hash(11)]),
                fpkg::GcProtection::OpenPackageTracking
            ),
            Err(AddBlobsError::Writing(super::super::writing::AddBlobsError::UnknownPackage {
                pkg
            })) if pkg == hash(2)
        );
    }

    #[fuchsia::test]
    fn complete_retained_blobs_marks_retained_package_complete_and_ignores_unretained() {
        let mut indices = Indices::new();
        let _: StopWriting = indices.writing.start(hash(2));
        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from([hash(10)])),
            hash(1) => RequiredBlobs::Complete(HashSet::from([hash(20)])),
        }));

        indices.complete_retained_blobs(hash(0), HashSet::from([hash(11), hash(12)]));
        indices.complete_retained_blobs(hash(1), HashSet::from([hash(20), hash(21)]));
        indices.complete_retained_blobs(hash(2), HashSet::from([hash(30)]));

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from([hash(10), hash(11), hash(12)])),
                hash(1) => RequiredBlobs::Complete(HashSet::from([hash(20), hash(21)])),
            }
        );
        assert_eq!(
            indices.writing.packages(),
            hashmap! {
                hash(2) => HashSet::new(),
            }
        );
    }

    #[fuchsia::test]
    fn all_known_blobs_produces_union_of_all_sub_indices() {
        let mut indices = Indices::new();

        let _: StopWriting = indices.writing.start(hash(0));

        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::new()),
            hash(6) => RequiredBlobs::Complete(HashSet::from_iter([hash(60), hash(61)])),
        }));
        indices.retained_blobs = RetainedBlobs::new(HashSet::from([hash(70)]));
        indices
            .upgradable
            .set_upgradable_urls(
                vec![fpkg::PackageUrl {
                    url: format!("fuchsia-pkg://fuchsia.com/upgradable?hash={}", hash(80)),
                }],
                &BaseIndex::new_test_only(HashSet::new(), []),
            )
            .unwrap();

        let _: StopWriting = indices.writing.start(hash(1));

        indices
            .add_blobs(hash(0), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();
        indices
            .add_blobs(
                hash(1),
                HashSet::from([hash(11), hash(61)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        assert_eq!(
            indices.all_known_blobs(),
            HashSet::from([
                hash(0),
                hash(1),
                hash(5),
                hash(6),
                hash(10),
                hash(11),
                hash(60),
                hash(61),
                hash(70),
                hash(80),
            ])
        );
    }

    #[fuchsia::test]
    async fn dropping_writing_guard_stops_writing() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let index = PackageIndex::new(blobfs, None);

        let guard = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        assert_eq!(
            index.indices.lock().await.writing.packages(),
            hashmap! { hash(0) => HashSet::new() }
        );
        drop(guard);
        assert_eq!(index.indices.lock().await.writing.packages(), hashmap! {});
    }

    #[fuchsia::test]
    async fn retained_gc_protection_writing_guard_is_noop() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let index = PackageIndex::new(blobfs, None);

        let guard = index.start_writing(hash(0), fpkg::GcProtection::Retained).await;
        assert_eq!(index.indices.lock().await.writing.packages(), hashmap! {});
        drop(guard);
        assert_eq!(index.indices.lock().await.writing.packages(), hashmap! {});
    }

    #[fuchsia::test]
    fn add_retained_package_preserves_other_retained_packages_and_merges_writing_blobs() {
        let mut indices = Indices::new();
        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
        }));
        let _: StopWriting = indices.writing.start(hash(1));
        indices
            .add_blobs(
                hash(1),
                HashSet::from([hash(11), hash(12)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        indices.add_retained_package(hash(1));
        indices.add_retained_package(hash(2));

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(11), hash(12)])),
                hash(2) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
    }

    #[fuchsia::test]
    fn replace_retained_packages_keeps_known_blobs_and_merges_writing_blobs() {
        let mut indices = Indices::new();
        indices.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
            hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
        }));
        let _: StopWriting = indices.writing.start(hash(2));
        indices
            .add_blobs(hash(2), HashSet::from([hash(12)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        indices.replace_retained_packages([hash(1), hash(2), hash(3)]);

        assert_eq!(
            indices.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
                hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(12)])),
                hash(3) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
    }

    #[fuchsia::test]
    async fn set_retained_index_populates_blobs_from_blobfs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);
        let index = PackageIndex::new(blobfs, None);

        index.set_retained_index(HashSet::from([hash(0), hash(1)])).await;

        assert_eq!(
            index.indices.lock().await.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::new()),
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(11),
                    hash(2),
                    hash(21),
                ])),
            }
        );
    }

    #[fuchsia::test]
    async fn retain_package_populates_blobs_from_blobfs_and_keeps_existing() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);

        let index = PackageIndex::new(blobfs, None);
        index.set_retained_index(HashSet::from([hash(0)])).await;

        index.retain_package(hash(1)).await;
        index.retain_package(hash(3)).await;

        assert_eq!(
            index.indices.lock().await.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::new()),
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(11),
                    hash(2),
                    hash(21),
                ])),
                hash(3) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
    }

    #[fuchsia::test]
    async fn retain_package_upgrades_partial_to_complete_and_preserves_existing_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let index = PackageIndex::new(blobfs, None);
        index.indices.lock().await.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
        }));
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);

        index.retain_package(hash(1)).await;

        assert_eq!(
            index.indices.lock().await.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(10),
                    hash(11),
                    hash(2),
                    hash(21),
                ])),
            }
        );
    }

    #[fuchsia::test]
    async fn retain_package_records_partial_blobs_when_subpackage_missing() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        let index = PackageIndex::new(blobfs, None);

        index.retain_package(hash(1)).await;

        assert_eq!(
            index.indices.lock().await.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(11), hash(2)])),
            }
        );
    }

    #[fuchsia::test]
    async fn acquire_gc_lock_protects_required_blobs_of_retained_packages_with_partial_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(4), "pkg-4", [hash(41)], [hash(5)]);
        let index = PackageIndex::new(blobfs, None);

        // Retain the packages before `hash(1)`'s meta.far and `hash(5)`'s subpackage meta.far are
        // in blobfs so their required blobs remain `Partial` in the retained index until
        // `acquire_gc_lock` reads blobfs. The meta.far of `hash(3)` is never added.
        index.retain_package(hash(1)).await;
        index.retain_package(hash(3)).await;
        index.retain_package(hash(4)).await;
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);
        add_meta_far_to_blobfs(&blobfs_fake, hash(5), "pkg-5", [hash(51)], []);

        let gc_guard =
            index.acquire_gc_lock(index.wait_until_upgradable_packages_initialized().await).await;
        assert_eq!(
            gc_guard.protected_blobs().await,
            HashSet::from([
                hash(1),
                hash(11),
                hash(2),
                hash(21),
                hash(3),
                hash(4),
                hash(41),
                hash(5),
                hash(51),
            ])
        );
    }

    #[fuchsia::test]
    async fn acquire_gc_lock_protects_required_blobs_of_upgradable_packages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "upgradable-complete", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "subpackage", [hash(21)], []);
        // `hash(4)`'s subpackage meta.far and `hash(5)`'s root meta.far are missing from blobfs;
        // best-effort lookup still protects the root hashes and all readable blobs.
        add_meta_far_to_blobfs(&blobfs_fake, hash(3), "upgradable-partial", [hash(31)], [hash(4)]);
        let index = PackageIndex::new(
            blobfs,
            Some(Arc::new(CacheIndex::new_test_only(HashSet::new(), []))),
        );
        let () = index
            .set_upgradable_urls(
                vec![
                    fpkg::PackageUrl {
                        url: format!(
                            "fuchsia-pkg://fuchsia.com/upgradable-complete?hash={}",
                            hash(1)
                        ),
                    },
                    fpkg::PackageUrl {
                        url: format!(
                            "fuchsia-pkg://fuchsia.com/upgradable-partial?hash={}",
                            hash(3)
                        ),
                    },
                    fpkg::PackageUrl {
                        url: format!(
                            "fuchsia-pkg://fuchsia.com/upgradable-missing?hash={}",
                            hash(5)
                        ),
                    },
                ],
                &BaseIndex::new_test_only(HashSet::new(), []),
            )
            .await
            .unwrap();

        let gc_guard =
            index.acquire_gc_lock(index.wait_until_upgradable_packages_initialized().await).await;
        assert_eq!(
            gc_guard.protected_blobs().await,
            HashSet::from([
                hash(1),
                hash(11),
                hash(2),
                hash(21),
                hash(3),
                hash(31),
                hash(4),
                hash(5),
            ])
        );
    }

    #[fuchsia::test]
    async fn upgradable_packages_disabled() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let index = PackageIndex::new(blobfs, None);

        assert!(!index.upgradable_packages_enabled());
        assert_eq!(
            index
                .set_upgradable_urls(
                    vec![fpkg::PackageUrl {
                        url: format!("fuchsia-pkg://fuchsia.com/upgradable?hash={}", hash(1)),
                    }],
                    &BaseIndex::new_test_only(HashSet::new(), []),
                )
                .await,
            Err(fpkg::SetUpgradableUrlsError::Internal)
        );
        assert_matches!(
            index
                .open_upgradable(
                    &"fuchsia-pkg://fuchsia.com/upgradable"
                        .parse::<UnpinnedAbsolutePackageUrl>()
                        .unwrap()
                )
                .await,
            None
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn retain_package_and_set_retained_index_skip_blobfs_for_complete_packages() {
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let index = PackageIndex::new(blobfs, None);
        index.indices.lock().await.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
        }));

        index.retain_package(hash(1)).await;
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut blob_reader_stream.next()).await,
            Poll::Pending,
            "retain_package must not read blobfs for a Complete package"
        );

        // `set_retained_index` with only already-`Complete` packages also skips blobfs.
        index.set_retained_index(HashSet::from([hash(1)])).await;
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut blob_reader_stream.next()).await,
            Poll::Pending,
            "set_retained_index must not read blobfs for Complete packages"
        );

        // `set_retained_index` with both `Complete` (`hash(1)`) and new (`hash(2)`) packages only
        // reads `hash(2)` from blobfs.
        let mut set_fut = pin!(index.set_retained_index(HashSet::from([hash(1), hash(2)])));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut set_fut).await,
            Poll::Pending
        );
        let responder = expect_get_vmo_async(&mut blob_reader_stream, hash(2)).await;
        respond_get_vmo(responder, &get_meta_far("pkg-2", [hash(21)], []));
        set_fut.await;
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut blob_reader_stream.next()).await,
            Poll::Pending
        );
        assert_eq!(
            index.indices.lock().await.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
                hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(21)])),
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn in_flight_blobfs_reads_do_not_resurrect_removed_retained_packages() {
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let index = PackageIndex::new(blobfs, None);

        // Pause `retain_package(hash(1))` while its meta.far read is in flight, then clear the
        // retained index before the read completes.
        let mut retain_fut = pin!(index.retain_package(hash(1)));
        assert!(fasync::TestExecutor::poll_until_stalled(&mut retain_fut).await.is_pending());
        let get_hash_1 = expect_get_vmo_async(&mut blob_reader_stream, hash(1)).await;

        index.set_retained_index(HashSet::new()).await;

        respond_get_vmo(get_hash_1, &get_meta_far("pkg-1", [hash(11)], []));
        retain_fut.await;
        assert_eq!(index.indices.lock().await.retained.packages(), hashmap! {});

        // Likewise, pause `set_retained_index({hash(2)})` while its meta.far read is in flight,
        // then clear the retained index before the read completes.
        let mut set_fut = pin!(index.set_retained_index(HashSet::from([hash(2)])));
        assert!(fasync::TestExecutor::poll_until_stalled(&mut set_fut).await.is_pending());
        let get_hash_2 = expect_get_vmo_async(&mut blob_reader_stream, hash(2)).await;

        index.set_retained_index(HashSet::new()).await;

        respond_get_vmo(get_hash_2, &get_meta_far("pkg-2", [hash(21)], []));
        set_fut.await;
        assert_eq!(index.indices.lock().await.retained.packages(), hashmap! {});
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn writing_guard_drops_during_gc_while_other_accesses_wait() {
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let base_packages = BaseIndex::new_test_only(HashSet::new(), []);
        let index = Arc::new(PackageIndex::new(
            blobfs,
            Some(Arc::new(CacheIndex::new_test_only(HashSet::new(), []))),
        ));
        // Initialize upgradable packages so `acquire_gc_lock` can proceed, while leaving a
        // subsequent `set_upgradable_urls` call to be tested against `GcGuard` below.
        index.set_upgradable_urls(vec![], &base_packages).await.unwrap();

        // `guard0` will be dropped during GC; `guard1` stays alive across GC so
        // `add_blobs(hash(1), ..)` can queue behind `GcGuard` and succeed afterward.
        let guard0 = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        let guard1 = index.start_writing(hash(1), fpkg::GcProtection::OpenPackageTracking).await;

        // `set_retained_index` and `retain_package` each lock `indices` twice (once to protect the
        // meta.far hashes as `Partial`, and again after reading meta.fars from blobfs). Start one
        // of each before GC, `set_retained_index` first so its replacement does not clear
        // `hash(4)`, and pause them on their blobfs reads so their second lock acquisitions can be
        // tested against `GcGuard`.
        let mut in_flight_set_retained_fut =
            pin!(index.set_retained_index(HashSet::from([hash(3)])));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut in_flight_set_retained_fut).await,
            Poll::Pending
        );
        let get_hash_3 = expect_get_vmo_async(&mut blob_reader_stream, hash(3)).await;

        let mut in_flight_retain_fut = pin!(index.retain_package(hash(4)));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut in_flight_retain_fut).await,
            Poll::Pending
        );
        let get_hash_4 = expect_get_vmo_async(&mut blob_reader_stream, hash(4)).await;

        // Acquire `GcGuard`. `protected_blobs()` attempts to read blobfs for `Partial` retained
        // packages `hash(3)` and `hash(4)` (whose pre-GC callers are still mid-read) and still
        // protects their meta.far hashes along with writing packages `hash(0)` and `hash(1)`.
        let initialized = index.wait_until_upgradable_packages_initialized().await;
        let gc_guard = index.acquire_gc_lock(initialized).await;
        let (protected, gc_checked_partial) = futures::join!(gc_guard.protected_blobs(), async {
            let mut checked = HashSet::new();
            for _ in 0..2 {
                let (blob_hash, responder) = next_get_vmo_async(&mut blob_reader_stream).await;
                checked.insert(blob_hash);
                responder.send(Err(zx::Status::NOT_FOUND.into_raw())).unwrap();
            }
            checked
        });
        assert_eq!(gc_checked_partial, HashSet::from([hash(3), hash(4)]));
        assert_eq!(protected, HashSet::from([hash(0), hash(1), hash(3), hash(4)]));

        // A second `acquire_gc_lock` and every other index access wait until `GcGuard` is dropped.
        // `async_lock::Mutex` wakes contended waiters one at a time in FIFO order of first poll:
        // 1. Poll `second_gc_fut` first so `drop(gc_guard)` wakes it when awaited alone below.
        // 2. Poll `complete_retained_fut` and resume `in_flight_set_retained_fut` /
        //    `in_flight_retain_fut` (by answering their blobfs reads so they hit their post-blobfs
        //    locks) before `set_retained_index_fut`, so `hash(3)` and `hash(4)` are already
        //    `Complete` when `set_retained_index_fut` runs and it only reads `hash(5)` from blobfs.
        // 3. Poll `set_retained_index_fut` (`{3, 4, 5}`) before `retain_package_fut` (`6`) so
        //    replacing the retained index keeps `3` and `4` without clobbering `6`.
        // 4. Poll `inspect_fut` last so it observes all synchronous updates plus `5` and `6` in
        //    their initial `Partial` state before their blobfs reads are answered.
        let mut second_gc_fut = pin!(index.acquire_gc_lock(initialized));
        assert!(fasync::TestExecutor::poll_until_stalled(&mut second_gc_fut).await.is_pending());

        let mut start_fut =
            pin!(index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut start_fut).await,
            Poll::Pending
        );

        let mut add_fut = pin!(index.add_blobs(
            hash(1),
            HashSet::from([hash(11)]),
            fpkg::GcProtection::OpenPackageTracking,
        ));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut add_fut).await,
            Poll::Pending
        );

        let mut complete_retained_fut =
            pin!(index.complete_retained_blobs(hash(3), HashSet::from([hash(31)])));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut complete_retained_fut).await,
            Poll::Pending
        );

        respond_get_vmo(get_hash_3, &get_meta_far("pkg-3", [hash(32)], []));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut in_flight_set_retained_fut).await,
            Poll::Pending
        );

        respond_get_vmo(get_hash_4, &get_meta_far("pkg-4", [hash(41)], []));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut in_flight_retain_fut).await,
            Poll::Pending
        );

        let mut set_retained_index_fut =
            pin!(index.set_retained_index(HashSet::from([hash(3), hash(4), hash(5)])));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut set_retained_index_fut).await,
            Poll::Pending
        );

        let mut retain_package_fut = pin!(index.retain_package(hash(6)));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut retain_package_fut).await,
            Poll::Pending
        );

        let mut set_blobs_fut = pin!(index.set_retained_blobs(HashSet::from([hash(30)])));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut set_blobs_fut).await,
            Poll::Pending
        );

        let upgradable_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/upgradable".parse().unwrap();
        let mut set_upgradable_fut = pin!(index.set_upgradable_urls(
            vec![fpkg::PackageUrl { url: format!("{upgradable_url}?hash={}", hash(7)) }],
            &base_packages,
        ));
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut set_upgradable_fut).await,
            Poll::Pending
        );

        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut pin!(
                index.open_upgradable(&upgradable_url)
            ))
            .await,
            Poll::Pending
        );
        let inspect_callback = index.record_lazy_inspect();
        let mut inspect_fut = inspect_callback();
        assert!(fasync::TestExecutor::poll_until_stalled(&mut inspect_fut).await.is_pending());
        // Neither `set_retained_index_fut` nor `retain_package_fut` has passed its initial lock to
        // issue a `GetVmo` request for `hash(5)` or `hash(6)`.
        assert_matches!(
            fasync::TestExecutor::poll_until_stalled(&mut blob_reader_stream.next()).await,
            Poll::Pending
        );

        // Dropping an existing `CleanupGuard` enqueues cleanup synchronously without waiting on
        // `GcGuard`, and neither the queued drop nor any blocked waiter mutates `PackageIndex`
        // state while `GcGuard` is held.
        drop(guard0);
        assert_eq!(
            gc_guard.indices.writing.packages(),
            hashmap! {
                hash(0) => HashSet::new(),
                hash(1) => HashSet::new(),
            }
        );
        assert_eq!(
            gc_guard.indices.retained.packages(),
            hashmap! {
                hash(3) => RequiredBlobs::Partial(HashSet::new()),
                hash(4) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
        assert_eq!(gc_guard.indices.retained_blobs.all_blobs(), &HashSet::new());
        assert_eq!(gc_guard.indices.upgradable.protected_packages(), HashSet::new());

        // Once `GcGuard` is dropped, `second_gc_fut` acquires the lock, drains `guard0`'s drop, and
        // re-blocks waiting accesses until `second_gc_guard` is also dropped.
        drop(gc_guard);
        let second_gc_guard = second_gc_fut.await;
        let mut all_waiters = pin!(async {
            futures::join!(
                start_fut,
                add_fut,
                complete_retained_fut,
                in_flight_set_retained_fut,
                in_flight_retain_fut,
                set_retained_index_fut,
                retain_package_fut,
                set_blobs_fut,
                set_upgradable_fut,
                inspect_fut,
            )
        });
        assert!(fasync::TestExecutor::poll_until_stalled(&mut all_waiters).await.is_pending());
        assert_eq!(
            second_gc_guard.indices.writing.packages(),
            hashmap! { hash(1) => HashSet::new() }
        );
        drop(second_gc_guard);

        // Dropping `second_gc_guard` unblocks the remaining waiters in FIFO order: single-lock
        // waiters complete (including `inspect_fut`, which snapshots `hash(5)` and `hash(6)` while
        // they are `Partial` awaiting `GetVmo`), leaving `all_waiters` stalled only on the blobfs
        // reads for `hash(5)` and `hash(6)`. Answering those `GetVmo` requests then lets
        // `set_retained_index_fut` and `retain_package_fut` finish their post-blobfs lock updates.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut all_waiters).await.is_pending());

        let meta_fars = HashMap::from([
            (hash(5), get_meta_far("pkg-5", [hash(51)], [])),
            (hash(6), get_meta_far("pkg-6", [hash(61)], [])),
        ]);
        let mut requested_hashes = HashSet::new();
        for _ in 0..2 {
            let (blob_hash, responder) = next_get_vmo_async(&mut blob_reader_stream).await;
            requested_hashes.insert(blob_hash);
            respond_get_vmo(responder, &meta_fars[&blob_hash]);
        }
        assert_eq!(requested_hashes, HashSet::from([hash(5), hash(6)]));

        let (guard2, add_res, (), (), (), (), (), (), set_upgradable_res, inspect_res) =
            all_waiters.await;
        assert_matches!(add_res, Ok(()));
        assert_matches!(set_upgradable_res, Ok(()));
        assert_data_tree!(
            inspect_res.unwrap(),
            root: {
                "retained": {
                    "entries": {
                        hash(3).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 2u64,
                        },
                        hash(4).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 1u64,
                        },
                        hash(5).to_string() => {
                            "state": "partial-blob-set",
                            "blobs-count": 0u64,
                        },
                        hash(6).to_string() => {
                            "state": "partial-blob-set",
                            "blobs-count": 0u64,
                        },
                    },
                },
                "retained-blobs": {
                    "blobs-count": 1u64,
                },
                "writing": {
                    hash(1).to_string() => {
                        "count": 1u64,
                    },
                    hash(2).to_string() => {
                        "count": 1u64,
                    },
                },
            }
        );

        {
            let indices = index.indices.lock().await;
            assert_eq!(
                indices.writing.packages(),
                hashmap! {
                    hash(1) => HashSet::from([hash(11)]),
                    hash(2) => HashSet::new(),
                }
            );
            assert_eq!(
                indices.retained.packages(),
                hashmap! {
                    hash(3) => RequiredBlobs::Complete(HashSet::from([hash(31), hash(32)])),
                    hash(4) => RequiredBlobs::Complete(HashSet::from([hash(41)])),
                    hash(5) => RequiredBlobs::Complete(HashSet::from([hash(51)])),
                    hash(6) => RequiredBlobs::Complete(HashSet::from([hash(61)])),
                }
            );
            assert_eq!(indices.retained_blobs.all_blobs(), &HashSet::from([hash(30)]));
        }

        let (resolved_hash, opening_guard) = index.open_upgradable(&upgradable_url).await.unwrap();
        assert_eq!(resolved_hash, hash(7));
        assert!(opening_guard.active.is_some());
        drop(opening_guard);

        drop(guard1);
        drop(guard2);
        // `record_lazy_inspect` drains the queued `guard1` and `guard2` drops on lock acquisition
        // before recording `"writing"`.
        assert_data_tree!(
            inspect_callback().await.unwrap(),
            root: {
                "retained": {
                    "entries": {
                        hash(3).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 2u64,
                        },
                        hash(4).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 1u64,
                        },
                        hash(5).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 1u64,
                        },
                        hash(6).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 1u64,
                        },
                    },
                },
                "retained-blobs": {
                    "blobs-count": 1u64,
                },
                "writing": {},
            }
        );
        let indices = index.indices.lock().await;
        assert_eq!(indices.writing.packages(), hashmap! {});
        assert_eq!(indices.upgradable.opening(), &hashmap! {});
    }

    #[fuchsia::test]
    async fn drain_on_lock_processes_queued_drops_before_gc_and_retries() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let index = PackageIndex::new(blobfs, None);
        let initialized = index.wait_until_upgradable_packages_initialized().await;

        // Dropping a guard immediately before `acquire_gc_lock` drains on lock so GC does not treat
        // a cancelled write's blobs as still protected.
        let guard = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        index
            .add_blobs(hash(0), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .await
            .unwrap();
        drop(guard);
        assert_eq!(
            index.acquire_gc_lock(initialized).await.protected_blobs().await,
            HashSet::new()
        );

        // With two concurrent guards for the same package, dropping one leaves the package
        // protected until the second is also dropped.
        let guard_a = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        let guard_b = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        index
            .add_blobs(hash(0), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .await
            .unwrap();
        drop(guard_a);
        assert_eq!(
            index.acquire_gc_lock(initialized).await.protected_blobs().await,
            HashSet::from([hash(0), hash(10)])
        );
        drop(guard_b);
        assert_eq!(
            index.acquire_gc_lock(initialized).await.protected_blobs().await,
            HashSet::new()
        );

        // If a guard drops during GC and a retry for the same hash queues behind `GcGuard`, the
        // retry drains the prior guard's cleanup before starting the new entry.
        let guard_old = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        index
            .add_blobs(hash(0), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .await
            .unwrap();
        let gc_guard = index.acquire_gc_lock(initialized).await;
        drop(guard_old);
        let mut retry_fut =
            pin!(index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking));
        assert_matches!(futures::poll!(&mut retry_fut), Poll::Pending);
        drop(gc_guard);

        let guard_retry = retry_fut.await;
        assert_eq!(
            index.indices.lock().await.writing.packages(),
            hashmap! { hash(0) => HashSet::new() }
        );
        drop(guard_retry);
        assert_eq!(index.indices.lock().await.writing.packages(), hashmap! {});
    }

    #[fuchsia::test]
    async fn open_upgradable_waits_for_init_and_protects_replaced_package_until_guard_dropped() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "upgradable", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "subpackage", [hash(21)], []);
        add_meta_far_to_blobfs(&blobfs_fake, hash(3), "upgradable", [hash(31)], []);

        let upgradable_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/upgradable".parse().unwrap();
        let cache_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/cache-fallback".parse().unwrap();
        let unknown_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/unknown".parse().unwrap();
        let base_packages = BaseIndex::new_test_only(HashSet::new(), []);
        let cache_packages = Arc::new(CacheIndex::new_test_only(
            HashSet::from([hash(9)]),
            [(cache_url.clone(), hash(9))],
        ));
        let index = PackageIndex::new(blobfs, Some(cache_packages));

        // Both `open_upgradable` and `wait_until_upgradable_packages_initialized` wait until the
        // first `set_upgradable_urls` call.
        let mut open_fut_a = pin!(index.open_upgradable(&upgradable_url));
        let mut init_fut = pin!(index.wait_until_upgradable_packages_initialized());
        assert_matches!(futures::poll!(&mut open_fut_a), Poll::Pending);
        assert_matches!(futures::poll!(&mut init_fut), Poll::Pending);

        index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{upgradable_url}?hash={}", hash(1)) }],
                &base_packages,
            )
            .await
            .unwrap();

        let initialized = init_fut.await;
        let (resolved_a, guard_a) = open_fut_a.await.unwrap();
        assert_eq!(resolved_a, hash(1));
        assert!(guard_a.active.is_some());

        // A second concurrent `open_upgradable` increments the refcount for `hash(1)`.
        let (resolved_b, guard_b) = index.open_upgradable(&upgradable_url).await.unwrap();
        assert_eq!(resolved_b, hash(1));
        assert!(guard_b.active.is_some());
        assert_eq!(index.indices.lock().await.upgradable.opening(), &hashmap! { hash(1) => 2 });

        // `CacheIndex` fallback resolves with an inert guard without inserting into `opening`
        // (since `CacheIndex` is permanently protected), and unknown URLs resolve to `None`.
        let (resolved_cache, cache_guard) = index.open_upgradable(&cache_url).await.unwrap();
        assert_eq!(resolved_cache, hash(9));
        assert!(cache_guard.active.is_none());
        assert_matches!(index.open_upgradable(&unknown_url).await, None);
        assert_eq!(index.indices.lock().await.upgradable.opening(), &hashmap! { hash(1) => 2 });

        // Replace `upgradable_url` with `hash(3)` and drop one of the two guards for `hash(1)`.
        index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{upgradable_url}?hash={}", hash(3)) }],
                &base_packages,
            )
            .await
            .unwrap();
        drop(guard_a);
        assert_eq!(index.indices.lock().await.upgradable.opening(), &hashmap! { hash(1) => 1 });

        // GC locks the index with both `hash(3)` (current upgradable) and `hash(1)` (still held by
        // `guard_b`) protected.
        let gc_guard = index.acquire_gc_lock(initialized).await;
        // Dropping `guard_b` while `GcGuard` is active queues cleanup without affecting the active
        // `GcGuard`.
        drop(guard_b);
        assert_eq!(
            gc_guard.protected_blobs().await,
            HashSet::from([hash(1), hash(11), hash(2), hash(21), hash(3), hash(31)])
        );
        drop(gc_guard);

        // A subsequent GC pass drains `guard_b`'s queued cleanup upon locking, so only `hash(3)`
        // remains protected.
        let next_gc_guard = index.acquire_gc_lock(initialized).await;
        assert_eq!(next_gc_guard.indices.upgradable.opening(), &hashmap! {});
        assert_eq!(next_gc_guard.protected_blobs().await, HashSet::from([hash(3), hash(31)]));
    }

    #[fuchsia::test]
    async fn cleanup_guard_drop_after_package_index_dropped_is_noop() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let upgradable_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/upgradable".parse().unwrap();
        let index = PackageIndex::new(
            blobfs,
            Some(Arc::new(CacheIndex::new_test_only(HashSet::new(), []))),
        );
        index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{upgradable_url}?hash={}", hash(1)) }],
                &BaseIndex::new_test_only(HashSet::new(), []),
            )
            .await
            .unwrap();
        let writing_guard =
            index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking).await;
        let (_, opening_guard) = index.open_upgradable(&upgradable_url).await.unwrap();
        assert!(opening_guard.active.is_some());
        drop(index);
        drop(writing_guard);
        drop(opening_guard);
    }
}
