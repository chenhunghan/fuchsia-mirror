// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use fuchsia_hash::Hash;
use futures::future::FutureExt as _;
use log::warn;
use std::collections::{HashMap, HashSet};

/// Known required blobs (the content blobs and the transitive closure of subpackage meta.fars and
/// content blobs) of a package, and whether that set is known to be complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RequiredBlobs {
    /// Not all of the package's required blobs are guaranteed to be known, e.g. because its
    /// meta.far or a transitive subpackage's meta.far was not readable when scanning blobfs, or
    /// blobs were only observed incrementally while caching.
    Partial(HashSet<Hash>),

    /// All of the package's required blobs have been determined by reading its meta.far and the
    /// meta.fars of all its transitive subpackages.
    Complete(HashSet<Hash>),
}

impl RequiredBlobs {
    /// The known required blobs.
    pub(crate) fn hashes(&self) -> &HashSet<Hash> {
        match self {
            Self::Partial(hashes) | Self::Complete(hashes) => hashes,
        }
    }

    pub(crate) fn into_hashes(self) -> HashSet<Hash> {
        match self {
            Self::Partial(hashes) | Self::Complete(hashes) => hashes,
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    pub(crate) fn is_partial(&self) -> bool {
        matches!(self, Self::Partial(_))
    }

    /// Merges `other` into `self`, unioning their blob sets and marking `self` `Complete` if either
    /// `self` or `other` is `Complete`.
    pub(crate) fn merge(&mut self, other: Self) {
        // Extending a `Complete` should always be a no-op, we do it anyway just to be safe.
        match (&mut *self, other) {
            (Self::Complete(this), Self::Complete(other) | Self::Partial(other)) => {
                this.extend(other);
            }
            (Self::Partial(this), Self::Partial(other)) => {
                this.extend(other);
            }
            (Self::Partial(this), Self::Complete(mut other)) => {
                other.extend(this.drain());
                other.shrink_to_fit();
                *self = Self::Complete(other);
            }
        }
    }
}

impl Default for RequiredBlobs {
    /// Returns an empty [`RequiredBlobs::Partial`], because no required blobs are known yet.
    fn default() -> Self {
        Self::Partial(HashSet::new())
    }
}

impl<'a> Extend<&'a Hash> for RequiredBlobs {
    /// Adds known required blobs without changing whether the set is complete. Blobs can only be
    /// added, never removed, so a [`RequiredBlobs::Complete`] set always contains all required
    /// blobs.
    fn extend<I: IntoIterator<Item = &'a Hash>>(&mut self, iter: I) {
        match self {
            Self::Partial(hashes) | Self::Complete(hashes) => hashes.extend(iter),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum FindRequiredBlobsError {
    #[error("failed to create a RootDir for meta.far {meta_far}")]
    CreateRootDir { source: package_directory::Error, meta_far: Hash },

    #[error("reading the subpackages manifest of {meta_far}")]
    InvalidSubpackagesManifest { source: package_directory::SubpackagesError, meta_far: Hash },
}

/// Returns all required blobs (the content blobs and the transitive closure of all subpackage
/// meta.fars and content blobs) of package `meta_hash`, failing if any meta.far or subpackages
/// manifest cannot be read.
///
/// Caches intermediate results in `memoized_packages` to de-duplicate work in case there are
/// shared subpackages. `memoized_packages` maps meta.fars to their required blobs and on success
/// will contain entries for `meta_hash` and all its (transitive) subpackages.
///
/// TODO(https://fxbug.dev/42064108) Replace with an iterative implementation to avoid stack overflows.
pub(crate) fn find_required_blobs_recursive<'a>(
    blobfs: &'a blobfs::Client,
    meta_hash: &'a Hash,
    memoized_packages: &'a async_lock::RwLock<HashMap<Hash, HashSet<Hash>>>,
) -> futures::future::BoxFuture<'a, Result<HashSet<Hash>, FindRequiredBlobsError>> {
    async move {
        if let Some(required_blobs) = memoized_packages.read().await.get(meta_hash) {
            return Ok(required_blobs.clone());
        }

        let root_dir =
            package_directory::RootDir::new(blobfs.clone(), *meta_hash).await.map_err(|e| {
                FindRequiredBlobsError::CreateRootDir { source: e, meta_far: *meta_hash }
            })?;

        let mut required_blobs = root_dir.external_file_hashes().copied().collect::<HashSet<_>>();

        let subpackages = root_dir
            .subpackages()
            .await
            .map_err(|e| FindRequiredBlobsError::InvalidSubpackagesManifest {
                source: e,
                meta_far: *meta_hash,
            })?
            .into_subpackages();
        for subpackage in subpackages.values() {
            required_blobs.insert(*subpackage);
            let subpackage_required_blobs =
                find_required_blobs_recursive(blobfs, subpackage, memoized_packages).await?;
            required_blobs.extend(subpackage_required_blobs);
        }

        memoized_packages.write().await.insert(*meta_hash, required_blobs.clone());
        required_blobs.shrink_to_fit();
        Ok(required_blobs)
    }
    .boxed()
}

/// Best-effort version of [`find_required_blobs_recursive`] that returns
/// [`RequiredBlobs::Complete`] if `meta_hash` and all of its transitive subpackages are readable in
/// `blobfs`, or [`RequiredBlobs::Partial`] with whatever required blobs could be determined
/// otherwise.
///
/// Logs errors other than a missing meta.far, which is expected while a package is being cached.
///
/// Caches results for readable `meta.far`s in `memoized_packages` (preserving whether each was
/// `Complete` or `Partial`) to de-duplicate work across shared subpackages.
///
/// TODO(https://fxbug.dev/42064108) Replace with an iterative implementation to avoid stack overflows.
pub(crate) fn find_required_blobs_best_effort<'a>(
    blobfs: &'a blobfs::Client,
    meta_hash: &'a Hash,
    memoized_packages: &'a async_lock::RwLock<HashMap<Hash, RequiredBlobs>>,
) -> futures::future::BoxFuture<'a, RequiredBlobs> {
    async move {
        if let Some(required_blobs) = memoized_packages.read().await.get(meta_hash) {
            return required_blobs.clone();
        }

        let root_dir = match package_directory::RootDir::new(blobfs.clone(), *meta_hash).await {
            Ok(root_dir) => root_dir,
            Err(package_directory::Error::MissingMetaFar) => {
                return RequiredBlobs::Partial(HashSet::new());
            }
            Err(e) => {
                warn!("failed to read meta.far {meta_hash}: {:#}", anyhow!(e));
                return RequiredBlobs::Partial(HashSet::new());
            }
        };

        let mut hashes = root_dir.external_file_hashes().copied().collect::<HashSet<_>>();

        let subpackages = match root_dir.subpackages().await {
            Ok(subpackages) => subpackages,
            Err(e) => {
                warn!("failed to read the subpackages manifest of {meta_hash}: {:#}", anyhow!(e));
                hashes.shrink_to_fit();
                let required_blobs = RequiredBlobs::Partial(hashes);
                memoized_packages.write().await.insert(*meta_hash, required_blobs.clone());
                return required_blobs;
            }
        };

        let mut complete = true;
        for subpackage in subpackages.into_subpackages().into_values() {
            hashes.insert(subpackage);
            match find_required_blobs_best_effort(blobfs, &subpackage, memoized_packages).await {
                RequiredBlobs::Complete(subpackage_hashes) => hashes.extend(subpackage_hashes),
                RequiredBlobs::Partial(subpackage_hashes) => {
                    complete = false;
                    hashes.extend(subpackage_hashes);
                }
            }
        }

        hashes.shrink_to_fit();
        let required_blobs =
            if complete { RequiredBlobs::Complete(hashes) } else { RequiredBlobs::Partial(hashes) };
        memoized_packages.write().await.insert(*meta_hash, required_blobs.clone());
        required_blobs
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{add_meta_far_to_blobfs, get_meta_far_with_subpackages_bytes};
    use assert_matches::assert_matches;
    use async_lock::RwLock;

    fn hash(n: u8) -> Hash {
        Hash::from([n; 32])
    }

    fn add_meta_far_with_invalid_subpackages_to_blobfs(
        blobfs: &fuchsia_pkg_testing::blobfs::Fake,
        hash: Hash,
        package_name: &str,
        needed_blobs: impl IntoIterator<Item = Hash>,
    ) {
        let meta_far =
            get_meta_far_with_subpackages_bytes(package_name, needed_blobs, b"not valid json");
        blobfs.add_blob(hash, meta_far);
    }

    #[fuchsia::test]
    async fn strict_success_no_subpackages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [hash(1)], []);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_recursive(&blobfs, &hash(0), &memoized_packages).await.unwrap(),
            HashSet::from_iter([hash(1)]),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([(hash(0), HashSet::from_iter([hash(1)]))]),
        );
    }

    #[fuchsia::test]
    async fn best_effort_success_no_subpackages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [hash(1)], []);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(0), &memoized_packages).await,
            RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([(hash(0), RequiredBlobs::Complete(HashSet::from_iter([hash(1)])))]),
        );
    }

    #[fuchsia::test]
    async fn strict_success_recursive_subpackages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(1), hash(2)]);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(3)], []);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [], [hash(4)]);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(4), "pkg-4", [hash(5)], []);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_recursive(&blobfs, &hash(0), &memoized_packages).await.unwrap(),
            HashSet::from_iter([hash(1), hash(2), hash(3), hash(4), hash(5)]),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([
                (hash(0), HashSet::from_iter([hash(1), hash(2), hash(3), hash(4), hash(5)])),
                (hash(1), HashSet::from_iter([hash(3)])),
                (hash(2), HashSet::from_iter([hash(4), hash(5)])),
                (hash(4), HashSet::from_iter([hash(5)])),
            ]),
        );
    }

    #[fuchsia::test]
    async fn best_effort_success_recursive_subpackages() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(1), hash(2)]);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(3)], []);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [], [hash(4)]);
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(4), "pkg-4", [hash(5)], []);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(0), &memoized_packages).await,
            RequiredBlobs::Complete(HashSet::from_iter([
                hash(1),
                hash(2),
                hash(3),
                hash(4),
                hash(5),
            ])),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([
                (
                    hash(0),
                    RequiredBlobs::Complete(HashSet::from_iter([
                        hash(1),
                        hash(2),
                        hash(3),
                        hash(4),
                        hash(5),
                    ]))
                ),
                (hash(1), RequiredBlobs::Complete(HashSet::from_iter([hash(3)]))),
                (hash(2), RequiredBlobs::Complete(HashSet::from_iter([hash(4), hash(5)]))),
                (hash(4), RequiredBlobs::Complete(HashSet::from_iter([hash(5)]))),
            ]),
        );
    }

    #[fuchsia::test]
    async fn strict_missing_meta_far() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        let memoized_packages = RwLock::new(HashMap::new());
        assert_matches!(
            find_required_blobs_recursive(&blobfs, &hash(0), &memoized_packages).await,
            Err(FindRequiredBlobsError::CreateRootDir { .. })
        );
        assert!(memoized_packages.read().await.is_empty());
    }

    #[fuchsia::test]
    async fn best_effort_missing_meta_far() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(0), &memoized_packages).await,
            RequiredBlobs::Partial(HashSet::new()),
        );
        assert!(memoized_packages.read().await.is_empty());
    }

    #[fuchsia::test]
    async fn best_effort_missing_subpackage_meta_far() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [hash(1)], [hash(2)]);
        // A second package sharing `pkg-0` as a subpackage must also observe `Partial` through
        // `memoized_packages`.
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(3), "pkg-3", [], [hash(0)]);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(0), &memoized_packages).await,
            RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2)])),
        );
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(3), &memoized_packages).await,
            RequiredBlobs::Partial(HashSet::from_iter([hash(0), hash(1), hash(2)])),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([
                (hash(0), RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2)]))),
                (hash(3), RequiredBlobs::Partial(HashSet::from_iter([hash(0), hash(1), hash(2)]))),
            ]),
        );
    }

    #[fuchsia::test]
    async fn strict_missing_subpackage_meta_far() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [hash(1)], [hash(2)]);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_matches!(
            find_required_blobs_recursive(&blobfs, &hash(0), &memoized_packages).await,
            Err(FindRequiredBlobsError::CreateRootDir { .. })
        );
        assert!(memoized_packages.read().await.is_empty());
    }

    #[fuchsia::test]
    async fn best_effort_invalid_subpackages_manifest() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_with_invalid_subpackages_to_blobfs(
            &blobfs_fake,
            hash(0),
            "pkg-0",
            [hash(1)],
        );
        // A second package sharing `pkg-0` as a subpackage must also observe `Partial` through
        // `memoized_packages`, while preserving `pkg-0`'s content blob `hash(1)`.
        let () = add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(3)], [hash(0)]);

        let memoized_packages = RwLock::new(HashMap::new());
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(0), &memoized_packages).await,
            RequiredBlobs::Partial(HashSet::from_iter([hash(1)])),
        );
        assert_eq!(
            find_required_blobs_best_effort(&blobfs, &hash(2), &memoized_packages).await,
            RequiredBlobs::Partial(HashSet::from_iter([hash(0), hash(1), hash(3)])),
        );
        assert_eq!(
            *memoized_packages.read().await,
            HashMap::from_iter([
                (hash(0), RequiredBlobs::Partial(HashSet::from_iter([hash(1)]))),
                (hash(2), RequiredBlobs::Partial(HashSet::from_iter([hash(0), hash(1), hash(3)]))),
            ]),
        );
    }

    #[fuchsia::test]
    async fn strict_invalid_subpackages_manifest() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();
        let () = add_meta_far_with_invalid_subpackages_to_blobfs(
            &blobfs_fake,
            hash(0),
            "pkg-0",
            [hash(1)],
        );

        let memoized_packages = RwLock::new(HashMap::new());
        assert_matches!(
            find_required_blobs_recursive(&blobfs, &hash(0), &memoized_packages).await,
            Err(FindRequiredBlobsError::InvalidSubpackagesManifest { meta_far, .. })
                if meta_far == hash(0)
        );
        assert!(memoized_packages.read().await.is_empty());
    }
}
