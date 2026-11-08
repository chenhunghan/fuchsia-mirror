// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::retained::{RetainedBlobs, RetainedIndex};
use super::writing::{CleanupGuard, WritingIndex};
use crate::frozen_index::BaseIndex;
use crate::required_blobs::{RequiredBlobs, find_required_blobs_best_effort};
use crate::upgradable_packages::UpgradablePackages;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_hash::Hash;
use fuchsia_inspect as finspect;
use fuchsia_url::fuchsia_pkg::UnpinnedAbsolutePackageUrl;
use futures::future::{BoxFuture, FutureExt as _};
use log::error;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The packages, other than the base, cache, and open packages, that pkg-cache protects from
/// garbage collection: the packages being written, the retained packages and blobs, and the
/// upgradable packages.
///
/// `PackageIndex` owns the only lock on this state, and uses it to serialize changes to the state
/// with garbage collection:
/// * Methods that change the state hold the lock for writing without awaiting while holding it, so
///   each change is atomic with respect to a garbage collection pass. Methods that read meta.fars
///   to determine the required blobs of retained packages do so without holding the lock, after
///   the packages are protected.
/// * A garbage collection pass holds the lock for reading from when it determines which blobs are
///   protected until it is done deleting blobs (see `Self::read_for_gc`). Because callers add
///   blobs to the index before checking whether they are present in blobfs (see
///   `Self::add_blobs`), a pass never deletes a blob that a caller found to be present.
/// * Nothing waits for upgradable packages to be initialized while holding the lock.
pub struct PackageIndex {
    state: async_lock::RwLock<State>,
    blobfs: blobfs::Client,
    upgradable_packages: Option<UpgradablePackages>,
}

impl std::fmt::Debug for PackageIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageIndex")
            .field("state", &self.state)
            .field("upgradable_packages_enabled", &self.upgradable_packages.is_some())
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
/// `PackageIndex::read_for_gc` requires this, so garbage collection waits for upgradable packages
/// to be initialized before it locks the index, and never collects upgradable packages before they
/// are known.
#[derive(Debug)]
pub struct UpgradablePackagesInitialized(());

impl PackageIndex {
    /// Creates an empty `PackageIndex` that reads meta.fars from `blobfs` to determine the
    /// required blobs of retained and upgradable packages. Upgradable packages are disabled if
    /// `upgradable_packages` is `None`.
    pub fn new(blobfs: blobfs::Client, upgradable_packages: Option<UpgradablePackages>) -> Self {
        Self { state: async_lock::RwLock::new(State::new()), blobfs, upgradable_packages }
    }

    /// Notifies the appropriate indices that the package with the given hash is going to be
    /// written, ensuring the meta.far blob is protected by the index.
    /// The returned `CleanupGuard` must be given back to `Self::stop_writing`.
    pub async fn start_writing(
        &self,
        pkg: Hash,
        gc_protection: fpkg::GcProtection,
    ) -> Option<CleanupGuard> {
        self.state.write().await.start_writing(pkg, gc_protection)
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
        self.state.write().await.add_blobs(package_hash, additional_blobs, gc_protection)
    }

    /// Records the complete set of required blobs (content blobs and transitive subpackage
    /// meta.fars and content blobs) for a package that has finished being fetched, marking its
    /// retained index entry [`RequiredBlobs::Complete`] if the package is retained.
    pub async fn complete_retained_blobs(&self, package_hash: Hash, blobs: HashSet<Hash>) {
        self.state.write().await.complete_retained_blobs(package_hash, blobs);
    }

    /// Notifies the appropriate indices that the package corresponding to `guard` that was
    /// obtained from an earlier call to `Self::start_writing` is no longer being written.
    pub async fn stop_writing(
        &self,
        guard: Option<CleanupGuard>,
    ) -> Result<(), super::writing::StopError> {
        self.state.write().await.stop_writing(guard)
    }

    /// Replaces the retained index with one that tracks the given meta.far hashes.
    ///
    /// The packages are protected from garbage collection as soon as the new retained index is
    /// swapped in. This then determines their required blobs from the meta.fars already in blobfs,
    /// so that garbage collection passes don't have to. A pass that runs in between determines them
    /// itself (see `GcView::protected_blobs`).
    pub async fn set_retained_index(&self, meta_hashes: HashSet<Hash>) {
        let partial = {
            let mut state = self.state.write().await;
            let () = state.replace_retained_packages(meta_hashes);
            state.retained.packages_with_partial_blobs().collect::<HashSet<_>>()
        };
        if partial.is_empty() {
            return;
        }

        // Read and parse the meta.fars without holding the lock.
        let populated = super::retained::populate_retained_index(&self.blobfs, partial).await;
        let () = self.state.write().await.retained.add_known_blobs(populated);
    }

    /// Adds the package with the given meta.far hash to the retained index.
    ///
    /// The package is protected from garbage collection as soon as it is added. As with
    /// `Self::set_retained_index`, this then determines its required blobs from the meta.fars
    /// already in blobfs.
    pub async fn retain_package(&self, meta_hash: Hash) {
        {
            let mut state = self.state.write().await;
            let () = state.add_retained_package(meta_hash);
            if state.retained.has_complete_blobs(&meta_hash) {
                return;
            }
        }

        // Read and parse the meta.fars without holding the lock.
        let blobs = find_required_blobs_best_effort(
            &self.blobfs,
            &meta_hash,
            &async_lock::RwLock::new(HashMap::new()),
        )
        .await;
        let _: bool = self.state.write().await.retained.merge_blobs(&meta_hash, blobs);
    }

    /// Replaces the retained blobs with the given blob hashes.
    pub async fn set_retained_blobs(&self, blobs: HashSet<Hash>) {
        self.state.write().await.set_retained_blobs(RetainedBlobs::new(blobs));
    }

    /// Associates upgradable package URLs with the hashes they should resolve to.
    ///
    /// Holds the lock for writing so the upgradable package set does not change during a garbage
    /// collection pass, which means this waits for any pass in progress to finish. Callers must
    /// keep newly associated packages protected from garbage collection until this returns, e.g.
    /// by keeping them open.
    pub async fn set_upgradable_urls(
        &self,
        pinned_urls: Vec<fpkg::PackageUrl>,
        base_packages: &BaseIndex,
    ) -> Result<(), fpkg::SetUpgradableUrlsError> {
        let Some(upgradable_packages) = &self.upgradable_packages else {
            error!("SetUpgradableUrls called but upgradable packages are not enabled in pkg-cache");
            return Err(fpkg::SetUpgradableUrlsError::Internal);
        };
        let _state = self.state.write().await;
        upgradable_packages.set_upgradable_urls(pinned_urls, base_packages)
    }

    /// Returns the hash that the given URL resolves to as an upgradable package, or `None` if
    /// upgradable packages are disabled or the URL is not an upgradable package.
    ///
    /// Waits for upgradable packages to be initialized, without locking the index.
    pub async fn upgradable_hash(&self, url: &UnpinnedAbsolutePackageUrl) -> Option<Hash> {
        self.upgradable_packages.as_ref()?.get_hash(url).await
    }

    /// Whether upgradable packages are enabled.
    pub fn upgradable_packages_enabled(&self) -> bool {
        self.upgradable_packages.is_some()
    }

    /// Waits for upgradable packages to be initialized by the first call to
    /// `Self::set_upgradable_urls`. Returns immediately if upgradable packages are disabled.
    pub async fn wait_until_upgradable_packages_initialized(
        &self,
    ) -> UpgradablePackagesInitialized {
        if let Some(upgradable_packages) = &self.upgradable_packages {
            let () = upgradable_packages.wait_until_initialized().await;
        }
        UpgradablePackagesInitialized(())
    }

    /// Locks the index for reading for a garbage collection pass. The index cannot change until
    /// the returned `GcView` is dropped, so the pass must keep it until it is done deleting blobs.
    pub async fn read_for_gc(&self, _: UpgradablePackagesInitialized) -> GcView<'_> {
        GcView { index: self, state: self.state.read().await }
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
                    let state: State = (*this.state.read().await).clone();
                    let root = inspector.root();
                    let () = root.record_child("retained", |n| state.retained.record_inspect(n));
                    let () = root
                        .record_child("retained-blobs", |n| state.retained_blobs.record_inspect(n));
                    let () = root.record_child("writing", |n| state.writing.record_inspect(n));
                } else {
                    inspector.root().record_string("error", "the package index was dropped");
                }
                Ok(inspector)
            }
            .boxed()
        }
    }
}

/// Read access to a `PackageIndex` for a garbage collection pass, obtained from
/// `PackageIndex::read_for_gc`. The index cannot change while this exists.
#[derive(Debug)]
pub struct GcView<'a> {
    index: &'a PackageIndex,
    state: async_lock::RwLockReadGuard<'a, State>,
}

impl GcView<'_> {
    /// Returns the blobs protected by the index: the meta.fars of the packages being written and
    /// of the retained packages, the blobs known to be required by those packages, the retained
    /// blobs, and the upgradable packages and their required blobs.
    ///
    /// Reads meta.fars from blobfs to determine the required blobs of the upgradable packages, and
    /// of the retained packages whose required blobs have not all been determined yet, e.g. because
    /// they were retained while their meta.fars were being read.
    pub async fn protected_blobs(&self) -> HashSet<Hash> {
        let mut protected = self.state.all_blobs();

        let memoized_packages = async_lock::RwLock::new(HashMap::new());
        for meta_hash in self.state.retained.packages_with_partial_blobs() {
            let blobs =
                find_required_blobs_best_effort(&self.index.blobfs, &meta_hash, &memoized_packages)
                    .await;
            protected.extend(blobs.into_hashes());
        }

        if let Some(upgradable_packages) = &self.index.upgradable_packages {
            protected.extend(upgradable_packages.list_blobs(&self.index.blobfs).await);
        }
        protected
    }
}

/// The indices protected by `PackageIndex`'s lock.
#[derive(Clone, Debug)]
struct State {
    retained: RetainedIndex,
    retained_blobs: RetainedBlobs,
    writing: WritingIndex,
}

impl State {
    /// Creates an empty State.
    fn new() -> Self {
        Self {
            retained: RetainedIndex::new(),
            retained_blobs: RetainedBlobs::default(),
            writing: WritingIndex::new(),
        }
    }

    fn start_writing(
        &mut self,
        pkg: Hash,
        gc_protection: fpkg::GcProtection,
    ) -> Option<CleanupGuard> {
        match gc_protection {
            // The purpose of Retained protection Get's is to not automatically protect the package
            // and instead to rely entirely on the client (the system-updater) to protect the
            // package using the Retained index. This allows the system-updater to meet the GC
            // requirements of the OTA process.
            fpkg::GcProtection::Retained => None,
            fpkg::GcProtection::OpenPackageTracking => Some(self.writing.start(pkg)),
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

    fn stop_writing(
        &mut self,
        guard: Option<super::writing::CleanupGuard>,
    ) -> Result<(), super::writing::StopError> {
        if let Some(guard) = guard {
            let () = self.writing.stop(guard)?;
        }
        Ok(())
    }

    fn set_retained_index(&mut self, mut index: RetainedIndex) {
        // Populate the new retained index with the relevant blobs from the writing index.
        let retained_packages = index.retained_packages().copied().collect::<Vec<_>>();
        for hash in retained_packages {
            if let Some(blobs) = self.writing.get_required_blobs(&hash) {
                // TODO(https://fxbug.dev/42064103) Consider replacing this panic with an error, or
                // e.g. adding a method to the retained index that makes both unnecessary.
                assert!(index.add_blobs(&hash, blobs));
            }
        }

        // Replace our retained index with the new one, which will populate content blobs from the
        // old retained index.
        self.retained.replace(index);
    }

    /// Replaces the retained index with one that tracks the given packages, with the required
    /// blobs already known to the retained and writing indices.
    fn replace_retained_packages(&mut self, meta_hashes: impl IntoIterator<Item = Hash>) {
        let mut index = RetainedIndex::new();
        for meta_hash in meta_hashes {
            index.add_package(meta_hash);
        }
        self.set_retained_index(index);
    }

    fn add_retained_package(&mut self, hash: Hash) {
        self.retained.add_package(hash);
        if let Some(blobs) = self.writing.get_required_blobs(&hash) {
            assert!(self.retained.add_blobs(&hash, blobs));
        }
    }

    fn set_retained_blobs(&mut self, blobs: RetainedBlobs) {
        self.retained_blobs = blobs;
    }

    /// Returns all blobs protected by these indices whose hashes are known without reading
    /// meta.fars.
    fn all_blobs(&self) -> HashSet<Hash> {
        let mut all = self.retained.all_blobs();
        all.extend(self.retained_blobs.all_blobs());
        all.extend(self.writing.all_blobs());
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frozen_index::CacheIndex;
    use crate::test_utils::{add_meta_far_to_blobfs, get_meta_far};
    use assert_matches::assert_matches;
    use fidl_fuchsia_fxfs as ffxfs;
    use fuchsia_async as fasync;
    use futures::StreamExt as _;
    use maplit::hashmap;
    use std::pin::pin;
    use std::task::Poll;

    fn hash(n: u8) -> Hash {
        Hash::from([n; 32])
    }

    #[test]
    fn set_retained_index_hashes_are_extended_with_writing_index_hashes() {
        let mut index = State::new();
        let guard = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking);
        index
            .add_blobs(
                hash(0),
                HashSet::from([hash(1), hash(2)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1)])),
        }));

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2)])),
            }
        );

        let () = index.stop_writing(guard).unwrap();
    }

    #[test]
    fn retained_index_is_not_informed_of_packages_it_does_not_track() {
        let mut index = State::new();

        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
        }));

        // install a package not tracked by the retained index
        let guard = index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking);
        index
            .add_blobs(hash(2), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
                hash(1) => RequiredBlobs::Partial(HashSet::new()),
            }
        );
        assert_eq!(
            index.writing.packages(),
            hashmap! {
                hash(2) => HashSet::from_iter([hash(10)]),
            }
        );
        let () = index.stop_writing(guard).unwrap();
    }

    #[test]
    fn set_retained_index_to_self_is_nop() {
        let mut index = State::new();
        let guard0 = index.start_writing(hash(3), fpkg::GcProtection::OpenPackageTracking);
        let guard1 = index.start_writing(hash(5), fpkg::GcProtection::OpenPackageTracking);

        index
            .add_blobs(hash(3), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
            hash(3) => RequiredBlobs::Partial(HashSet::new()),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::new()),
        }));

        index
            .add_blobs(hash(5), HashSet::from([hash(12)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        let retained_index = index.retained.clone();

        index.set_retained_index(retained_index.clone());
        assert_eq!(index.retained, retained_index);

        let () = index.stop_writing(guard0).unwrap();
        let () = index.stop_writing(guard1).unwrap();
    }

    #[test]
    fn add_blobs_with_open_package_tracking_protection_adds_to_retained_and_writing() {
        let mut index = State::new();

        let guard = index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking);
        index
            .add_blobs(hash(2), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();
        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
        }));

        index
            .add_blobs(hash(2), HashSet::from([hash(11)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(2) => RequiredBlobs::Partial(HashSet::from([hash(10), hash(11)]))
            }
        );
        assert_eq!(
            index.writing.packages(),
            hashmap! {
                hash(2) => HashSet::from([hash(10), hash(11)])
            }
        );
        let () = index.stop_writing(guard).unwrap();
    }

    #[test]
    fn add_blobs_with_retained_protection_adds_to_retained_index_only() {
        let mut index = State::new();

        let guard = index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking);
        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::new())
        }));

        index.add_blobs(hash(2), HashSet::from([hash(11)]), fpkg::GcProtection::Retained).unwrap();

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(2) => RequiredBlobs::Partial(HashSet::from([hash(11)]))
            }
        );
        assert_eq!(
            index.writing.packages(),
            hashmap! {
                hash(2) => HashSet::new()
            }
        );
        let () = index.stop_writing(guard).unwrap();
    }

    #[test]
    fn add_blobs_errors_if_writing_index_in_wrong_state() {
        let mut index = State::new();

        assert_matches!(
            index.add_blobs(
                hash(2),
                HashSet::from([hash(11)]),
                fpkg::GcProtection::OpenPackageTracking
            ),
            Err(AddBlobsError::Writing(super::super::writing::AddBlobsError::UnknownPackage{
                pkg
            })) if pkg == hash(2)
        );
    }

    #[fuchsia::test]
    fn complete_retained_blobs_marks_retained_package_complete_and_ignores_unretained() {
        let mut index = State::new();
        let guard = index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking);
        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from([hash(10)])),
            hash(1) => RequiredBlobs::Complete(HashSet::from([hash(20)])),
        }));

        index.complete_retained_blobs(hash(0), HashSet::from([hash(11), hash(12)]));
        index.complete_retained_blobs(hash(1), HashSet::from([hash(20), hash(21)]));
        index.complete_retained_blobs(hash(2), HashSet::from([hash(30)]));

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from([hash(10), hash(11), hash(12)])),
                hash(1) => RequiredBlobs::Complete(HashSet::from([hash(20), hash(21)])),
            }
        );
        assert_eq!(
            index.writing.packages(),
            hashmap! {
                hash(2) => HashSet::new(),
            }
        );
        let () = index.stop_writing(guard).unwrap();
    }

    #[test]
    fn all_blobs_produces_union_of_retained_and_writing_all_blobs() {
        let mut index = State::new();

        let guard0 = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking);

        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::new()),
            hash(6) => RequiredBlobs::Complete(HashSet::from_iter([hash(60), hash(61)])),
        }));

        let guard1 = index.start_writing(hash(1), fpkg::GcProtection::OpenPackageTracking);

        index
            .add_blobs(hash(0), HashSet::from([hash(10)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();
        index
            .add_blobs(
                hash(1),
                HashSet::from([hash(11), hash(61)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        assert_eq!(
            index.all_blobs(),
            HashSet::from([
                hash(0),
                hash(1),
                hash(5),
                hash(6),
                hash(10),
                hash(11),
                hash(60),
                hash(61)
            ])
        );
        let () = index.stop_writing(guard0).unwrap();
        let () = index.stop_writing(guard1).unwrap();
    }

    #[test]
    fn stop_writing_fowarded_to_writing_index() {
        let mut index = State::new();

        let guard = index.start_writing(hash(0), fpkg::GcProtection::OpenPackageTracking);
        let () = index.stop_writing(guard).unwrap();
        assert_eq!(index.writing.packages(), hashmap! {})
    }

    #[test]
    fn add_retained_package_preserves_other_retained_packages_and_merges_writing_blobs() {
        let mut index = State::new();
        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
        }));
        let guard = index.start_writing(hash(1), fpkg::GcProtection::OpenPackageTracking);
        index
            .add_blobs(
                hash(1),
                HashSet::from([hash(11), hash(12)]),
                fpkg::GcProtection::OpenPackageTracking,
            )
            .unwrap();

        index.add_retained_package(hash(1));
        index.add_retained_package(hash(2));

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(11), hash(12)])),
                hash(2) => RequiredBlobs::Partial(HashSet::new()),
            }
        );

        let () = index.stop_writing(guard).unwrap();
    }

    #[fuchsia::test]
    fn replace_retained_packages_keeps_known_blobs_and_merges_writing_blobs() {
        let mut index = State::new();
        index.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(10)])),
            hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
        }));
        let guard = index.start_writing(hash(2), fpkg::GcProtection::OpenPackageTracking);
        index
            .add_blobs(hash(2), HashSet::from([hash(12)]), fpkg::GcProtection::OpenPackageTracking)
            .unwrap();

        index.replace_retained_packages([hash(1), hash(2), hash(3)]);

        assert_eq!(
            index.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
                hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(12)])),
                hash(3) => RequiredBlobs::Partial(HashSet::new()),
            }
        );

        let () = index.stop_writing(guard).unwrap();
    }

    #[fuchsia::test]
    async fn set_retained_index_populates_blobs_from_blobfs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);
        let index = PackageIndex::new(blobfs, None);

        index.set_retained_index(HashSet::from([hash(0), hash(1)])).await;

        assert_eq!(
            index.state.read().await.retained.packages(),
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
            index.state.read().await.retained.packages(),
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
        index.state.write().await.set_retained_index(RetainedIndex::from_packages(hashmap! {
            hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
        }));
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);

        index.retain_package(hash(1)).await;

        assert_eq!(
            index.state.read().await.retained.packages(),
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
            index.state.read().await.retained.packages(),
            hashmap! {
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(11), hash(2)])),
            }
        );
    }

    #[fuchsia::test]
    async fn gc_view_protects_required_blobs_of_retained_packages_with_partial_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(4), "pkg-4", [hash(41)], [hash(5)]);
        let index = PackageIndex::new(blobfs, None);

        // Retain the packages before `hash(1)`'s meta.far and `hash(5)`'s subpackage meta.far are
        // in blobfs so their required blobs remain `Partial` in the retained index until
        // `GcView::protected_blobs` reads blobfs. The meta.far of `hash(3)` is never added.
        index.retain_package(hash(1)).await;
        index.retain_package(hash(3)).await;
        index.retain_package(hash(4)).await;
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(21)], []);
        add_meta_far_to_blobfs(&blobfs_fake, hash(5), "pkg-5", [hash(51)], []);

        let view =
            index.read_for_gc(index.wait_until_upgradable_packages_initialized().await).await;
        assert_eq!(
            view.protected_blobs().await,
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
    async fn gc_view_protects_required_blobs_of_upgradable_packages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "upgradable", [hash(11)], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "subpackage", [hash(21)], []);
        let index = PackageIndex::new(
            blobfs,
            Some(UpgradablePackages::new(Arc::new(CacheIndex::new_test_only(HashSet::new(), [])))),
        );
        let () = index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl {
                    url: format!("fuchsia-pkg://fuchsia.com/upgradable?hash={}", hash(1)),
                }],
                &BaseIndex::new_test_only(HashSet::new(), []),
            )
            .await
            .unwrap();

        let view =
            index.read_for_gc(index.wait_until_upgradable_packages_initialized().await).await;
        assert_eq!(
            view.protected_blobs().await,
            HashSet::from([hash(1), hash(11), hash(2), hash(21)])
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
        assert_eq!(
            index
                .upgradable_hash(
                    &"fuchsia-pkg://fuchsia.com/upgradable"
                        .parse::<UnpinnedAbsolutePackageUrl>()
                        .unwrap()
                )
                .await,
            None
        );
    }

    fn respond_to_get_vmo(
        executor: &mut fasync::TestExecutor,
        blob_reader_stream: &mut ffxfs::BlobReaderRequestStream,
        expected_hash: Hash,
        contents: &[u8],
    ) {
        match executor.run_until_stalled(&mut blob_reader_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobReaderRequest::GetVmo { blob_hash, responder }))) => {
                assert_eq!(Hash::from(blob_hash), expected_hash);
                let vmo = zx::Vmo::create(contents.len() as u64).unwrap();
                vmo.write(contents, 0).unwrap();
                responder.send(Ok(vmo)).unwrap();
            }
            other => panic!("unexpected blob reader request: {other:?}"),
        }
    }

    #[fuchsia::test]
    fn retain_package_and_set_retained_index_skip_blobfs_for_complete_packages() {
        let mut executor = fasync::TestExecutor::new();
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let index = PackageIndex::new(blobfs, None);
        index.state.try_write().unwrap().set_retained_index(RetainedIndex::from_packages(
            hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
            },
        ));

        assert_matches!(
            executor.run_until_stalled(&mut pin!(index.retain_package(hash(1)))),
            Poll::Ready(())
        );
        assert_matches!(
            executor.run_until_stalled(&mut blob_reader_stream.next()),
            Poll::Pending,
            "retain_package must not read blobfs for a Complete package"
        );

        // `set_retained_index` with only already-`Complete` packages also skips blobfs.
        assert_matches!(
            executor
                .run_until_stalled(&mut pin!(index.set_retained_index(HashSet::from([hash(1)])))),
            Poll::Ready(())
        );
        assert_matches!(
            executor.run_until_stalled(&mut blob_reader_stream.next()),
            Poll::Pending,
            "set_retained_index must not read blobfs for Complete packages"
        );

        // `set_retained_index` with both `Complete` (`hash(1)`) and new (`hash(2)`) packages only
        // reads `hash(2)` from blobfs.
        let mut set_fut = pin!(index.set_retained_index(HashSet::from([hash(1), hash(2)])));
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Pending);
        respond_to_get_vmo(
            &mut executor,
            &mut blob_reader_stream,
            hash(2),
            &get_meta_far("pkg-2", [hash(21)], []),
        );
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Ready(()));
        assert_matches!(executor.run_until_stalled(&mut blob_reader_stream.next()), Poll::Pending);
        assert_eq!(
            executor.run_until_stalled(&mut pin!(async {
                index.state.read().await.retained.packages()
            })),
            Poll::Ready(hashmap! {
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(11)])),
                hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(21)])),
            })
        );
    }

    #[fuchsia::test]
    fn in_flight_blobfs_reads_do_not_resurrect_removed_retained_packages() {
        let mut executor = fasync::TestExecutor::new();
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let index = PackageIndex::new(blobfs, None);

        // Pause `retain_package(hash(1))` while its meta.far read is in flight, then clear the
        // retained index before the read completes.
        let mut retain_fut = pin!(index.retain_package(hash(1)));
        assert!(executor.run_until_stalled(&mut retain_fut).is_pending());
        let get_hash_1 = match executor.run_until_stalled(&mut blob_reader_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobReaderRequest::GetVmo { blob_hash, responder }))) => {
                assert_eq!(Hash::from(blob_hash), hash(1));
                responder
            }
            other => panic!("unexpected blob reader request: {other:?}"),
        };

        assert_matches!(
            executor.run_until_stalled(&mut pin!(index.set_retained_index(HashSet::new()))),
            Poll::Ready(())
        );

        let meta_1 = get_meta_far("pkg-1", [hash(11)], []);
        let vmo_1 = zx::Vmo::create(meta_1.len() as u64).unwrap();
        vmo_1.write(&meta_1, 0).unwrap();
        get_hash_1.send(Ok(vmo_1)).unwrap();
        assert_matches!(executor.run_until_stalled(&mut retain_fut), Poll::Ready(()));
        assert_eq!(
            executor.run_until_stalled(&mut pin!(async {
                index.state.read().await.retained.packages()
            })),
            Poll::Ready(hashmap! {})
        );

        // Likewise, pause `set_retained_index({hash(2)})` while its meta.far read is in flight,
        // then clear the retained index before the read completes.
        let mut set_fut = pin!(index.set_retained_index(HashSet::from([hash(2)])));
        assert!(executor.run_until_stalled(&mut set_fut).is_pending());
        let get_hash_2 = match executor.run_until_stalled(&mut blob_reader_stream.next()) {
            Poll::Ready(Some(Ok(ffxfs::BlobReaderRequest::GetVmo { blob_hash, responder }))) => {
                assert_eq!(Hash::from(blob_hash), hash(2));
                responder
            }
            other => panic!("unexpected blob reader request: {other:?}"),
        };

        assert_matches!(
            executor.run_until_stalled(&mut pin!(index.set_retained_index(HashSet::new()))),
            Poll::Ready(())
        );

        let meta_2 = get_meta_far("pkg-2", [hash(21)], []);
        let vmo_2 = zx::Vmo::create(meta_2.len() as u64).unwrap();
        vmo_2.write(&meta_2, 0).unwrap();
        get_hash_2.send(Ok(vmo_2)).unwrap();
        assert_matches!(executor.run_until_stalled(&mut set_fut), Poll::Ready(()));
        assert_eq!(
            executor.run_until_stalled(&mut pin!(async {
                index.state.read().await.retained.packages()
            })),
            Poll::Ready(hashmap! {})
        );
    }
}
