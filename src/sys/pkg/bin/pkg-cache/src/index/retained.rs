// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::required_blobs::{RequiredBlobs, find_required_blobs_best_effort};
use fuchsia_hash::Hash;
use fuchsia_inspect::{self as finspect};
use std::collections::{HashMap, HashSet};

/// An index of packages considered to be part of a new system's base package set.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct RetainedIndex {
    /// Map from package hash to known content and subpackage blobs (meta.fars and content blobs of
    /// all subpackages, plus the subpackages' subpackage blobs, recursively), tagged with whether
    /// all required blobs of the package have been determined.
    ///
    /// An entry is [`RequiredBlobs::Complete`] once the package's meta.far and the meta.fars of
    /// all its transitive subpackages have been read without error (when populating the index from
    /// blobfs or when fetching the package). Otherwise, it is [`RequiredBlobs::Partial`]
    /// containing whatever required blobs have been observed so far (e.g. from the resident subset
    /// of meta.fars or incrementally via [`RetainedIndex::add_blobs`] while caching).
    packages: HashMap<Hash, RequiredBlobs>,
}

impl RetainedIndex {
    /// Creates an empty instance of the RetainedIndex.
    pub fn new() -> Self {
        Default::default()
    }

    /// The packages protected by this index.
    pub fn retained_packages(&self) -> impl Iterator<Item = &Hash> {
        self.packages.keys()
    }

    #[cfg(test)]
    pub fn from_packages(packages: HashMap<Hash, RequiredBlobs>) -> Self {
        Self { packages }
    }

    #[cfg(test)]
    pub fn packages(&self) -> HashMap<Hash, RequiredBlobs> {
        self.packages.clone()
    }

    /// Associates blobs with the given package hash if that package is known to the retained
    /// index, protecting those blobs from garbage collection.
    ///
    /// Does not mark a [`RequiredBlobs::Partial`] entry [`RequiredBlobs::Complete`], because
    /// incremental blob additions (e.g. from the writing index or an in-progress
    /// `PackageCache.Get`) are not guaranteed to include all required blobs of the package.
    ///
    /// Returns true iff the package is known to this index.
    pub fn add_blobs(&mut self, meta_hash: &Hash, blobs: &HashSet<Hash>) -> bool {
        let Some(required_blobs) = self.packages.get_mut(meta_hash) else {
            return false;
        };
        required_blobs.extend(blobs);
        true
    }

    /// Adds the given package hash to the retained index if it is not already tracked, protecting
    /// its meta.far blob from garbage collection.
    pub fn add_package(&mut self, meta_hash: Hash) {
        let _: &mut RequiredBlobs = self.packages.entry(meta_hash).or_default();
    }

    /// Merges `blobs` into the entry for `meta_hash` if that package is known to the retained
    /// index, marking the entry [`RequiredBlobs::Complete`] if either the existing entry or
    /// `blobs` is complete.
    /// Returns true iff the package is known to this index.
    pub fn merge_blobs(&mut self, meta_hash: &Hash, blobs: RequiredBlobs) -> bool {
        let Some(existing) = self.packages.get_mut(meta_hash) else {
            return false;
        };
        existing.merge(blobs);
        true
    }

    /// Merges the blobs known to `other` into the packages that this index also tracks.
    pub fn add_known_blobs(&mut self, other: Self) {
        for (meta_hash, blobs) in other.packages {
            let _: bool = self.merge_blobs(&meta_hash, blobs);
        }
    }

    /// Whether all required blobs of `meta_hash` have been determined.
    pub fn has_complete_blobs(&self, meta_hash: &Hash) -> bool {
        self.packages.get(meta_hash).is_some_and(RequiredBlobs::is_complete)
    }

    /// The packages whose required blobs have not all been determined yet, e.g. because their
    /// meta.far or a subpackage's meta.far was not available when they were added.
    pub fn packages_with_partial_blobs(&self) -> impl Iterator<Item = Hash> {
        self.packages.iter().filter_map(|(hash, blobs)| blobs.is_partial().then_some(*hash))
    }

    /// Replaces this retained index instance with other, populating other's blob sets
    /// using data from `self` when possible.
    pub fn replace(&mut self, mut other: Self) {
        for (meta_hash, other_blobs) in other.packages.iter_mut() {
            if let Some(this_blobs) = self.packages.remove(meta_hash) {
                other_blobs.merge(this_blobs);
            }
        }
        self.packages = other.packages;
    }

    /// Returns the set of all blobs currently protected by the retained index.
    pub fn all_blobs(&self) -> HashSet<Hash> {
        self.packages
            .iter()
            .flat_map(|(meta_hash, required_blobs)| {
                std::iter::once(meta_hash).chain(required_blobs.hashes())
            })
            .copied()
            .collect()
    }

    /// Records self to `node`.
    pub fn record_inspect(&self, node: &finspect::Node) {
        node.record_child("entries", |n| {
            for (hash, required_blobs) in &self.packages {
                n.record_child(hash.to_string(), |n| {
                    let (state, blobs) = match required_blobs {
                        RequiredBlobs::Partial(blobs) => ("partial-blob-set", blobs),
                        RequiredBlobs::Complete(blobs) => ("complete-blob-set", blobs),
                    };
                    n.record_string("state", state);
                    n.record_uint("blobs-count", blobs.len() as u64);
                })
            }
        })
    }
}

/// Constructs a new [`RetainedIndex`] from the given blobfs client and set of package meta.far
/// hashes, populating blob hashes for any packages with a meta.far present in blobfs.
/// Packages whose meta.far or any transitive subpackage meta.far is unreadable in blobfs are
/// recorded as [`RequiredBlobs::Partial`].
pub async fn populate_retained_index(
    blobfs: &blobfs::Client,
    meta_hashes: HashSet<Hash>,
) -> RetainedIndex {
    let mut packages = HashMap::with_capacity(meta_hashes.len());
    let memoized_packages = async_lock::RwLock::new(HashMap::new());
    for meta_hash in meta_hashes {
        let found = find_required_blobs_best_effort(blobfs, &meta_hash, &memoized_packages).await;
        packages.insert(meta_hash, found);
    }
    RetainedIndex { packages }
}

/// An index of blobs considered to be part of a new system's base package set.
#[derive(Clone, Debug, Default)]
pub struct RetainedBlobs(HashSet<Hash>);

impl RetainedBlobs {
    pub fn new(blobs: HashSet<Hash>) -> Self {
        Self(blobs)
    }

    /// Returns the set of all blobs currently protected by the retained blobs index.
    pub fn all_blobs(&self) -> &HashSet<Hash> {
        &self.0
    }

    /// Records self to `node`.
    pub fn record_inspect(&self, node: &finspect::Node) {
        node.record_uint("blobs-count", self.0.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::add_meta_far_to_blobfs;
    use maplit::{hashmap, hashset};
    use test_case::test_case;

    fn hash(n: u8) -> Hash {
        Hash::from([n; 32])
    }

    #[test]
    fn add_blobs_nops_if_content_blobs_are_already_known() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
        });

        assert!(index.add_blobs(&hash(0), &hashset! {hash(1)}));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
            })
        );
    }

    #[test]
    fn add_blobs_ignores_unknown_packages() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::new()),
        });

        assert!(!index.add_blobs(&hash(1), &hashset! {}));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::new()),
            })
        );
    }

    #[test]
    fn add_blobs_remembers_content_blobs_for_partial_package_without_marking_complete() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
        });

        assert!(index.add_blobs(&hash(0), &hashset! {hash(1), hash(2), hash(3)}));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2), hash(3)])),
            })
        );
    }

    #[test]
    fn add_blobs_add_to_existing_blobs() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
        });

        assert!(index.add_blobs(&hash(0), &hashset! {hash(1)}));
        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1)])),
            })
        );

        assert!(index.add_blobs(&hash(0), &hashset! {hash(2)}));
        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2)])),
            })
        );
    }

    #[test]
    fn add_package_inserts_unknown_package_with_empty_partial() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
        });

        index.add_package(hash(2));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
                hash(2) => RequiredBlobs::Partial(HashSet::new()),
            })
        );
    }

    #[test]
    fn add_package_preserves_existing_blobs() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(3)])),
        });

        index.add_package(hash(0));
        index.add_package(hash(2));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
                hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(3)])),
            })
        );
    }

    #[test_case(
        RequiredBlobs::Partial(HashSet::from([hash(10)])),
        RequiredBlobs::Partial(HashSet::from([hash(11)])),
        RequiredBlobs::Partial(HashSet::from([hash(10), hash(11)]));
        "partial_with_partial_stays_partial"
    )]
    #[test_case(
        RequiredBlobs::Partial(HashSet::from([hash(10)])),
        RequiredBlobs::Complete(HashSet::from([hash(11)])),
        RequiredBlobs::Complete(HashSet::from([hash(10), hash(11)]));
        "partial_with_complete_becomes_complete"
    )]
    #[test_case(
        RequiredBlobs::Complete(HashSet::from([hash(10)])),
        RequiredBlobs::Partial(HashSet::from([hash(11)])),
        RequiredBlobs::Complete(HashSet::from([hash(10), hash(11)]));
        "complete_with_partial_stays_complete"
    )]
    #[test_case(
        RequiredBlobs::Complete(HashSet::from([hash(10)])),
        RequiredBlobs::Complete(HashSet::from([hash(11)])),
        RequiredBlobs::Complete(HashSet::from([hash(10), hash(11)]));
        "complete_with_complete_stays_complete"
    )]
    #[test_case(
        RequiredBlobs::Partial(HashSet::new()),
        RequiredBlobs::Partial(HashSet::new()),
        RequiredBlobs::Partial(HashSet::new());
        "empty_partial_with_empty_partial_stays_empty_partial"
    )]
    #[fuchsia::test]
    fn merge_blobs_unions_hashes_and_is_complete_if_either_side_is(
        existing: RequiredBlobs,
        merged: RequiredBlobs,
        expected: RequiredBlobs,
    ) {
        let mut index = RetainedIndex::from_packages(hashmap! { hash(0) => existing });

        assert!(index.merge_blobs(&hash(0), merged));

        assert_eq!(index, RetainedIndex::from_packages(hashmap! { hash(0) => expected }));
    }

    #[fuchsia::test]
    fn merge_blobs_ignores_unknown_packages() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
        });

        assert!(
            !index.merge_blobs(&hash(1), RequiredBlobs::Complete(HashSet::from_iter([hash(11)])))
        );

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
            })
        );
    }

    #[fuchsia::test]
    fn add_known_blobs_extends_tracked_packages_only_and_merges_completeness() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(31)])),
            hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(20)])),
            hash(3) => RequiredBlobs::Partial(HashSet::new()),
        });

        index.add_known_blobs(RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(30)])),
            hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(11)])),
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(21)])),
            hash(3) => RequiredBlobs::Partial(HashSet::new()),
            hash(4) => RequiredBlobs::Complete(HashSet::from_iter([hash(40)])),
        }));

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(30), hash(31)])),
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(10), hash(11)])),
                hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(20), hash(21)])),
                hash(3) => RequiredBlobs::Partial(HashSet::new()),
            })
        );
    }

    #[fuchsia::test]
    fn packages_with_partial_blobs_excludes_complete_packages() {
        let index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(1) => RequiredBlobs::Complete(HashSet::new()),
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(12)])),
            hash(3) => RequiredBlobs::Partial(HashSet::from_iter([hash(13)])),
        });

        assert!(!index.has_complete_blobs(&hash(0)));
        assert!(index.has_complete_blobs(&hash(1)));
        assert!(index.has_complete_blobs(&hash(2)));
        assert!(!index.has_complete_blobs(&hash(3)));
        assert!(!index.has_complete_blobs(&hash(4)));
        assert_eq!(
            HashSet::<Hash>::from_iter(index.packages_with_partial_blobs()),
            HashSet::from([hash(0), hash(3)])
        );
    }

    #[test]
    fn replace_retains_keys_from_other_only() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
        });
        let other = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
            hash(3) => RequiredBlobs::Partial(HashSet::new()),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
        });

        index.replace(other.clone());
        assert_eq!(index, other);
    }

    #[test]
    fn replace_retains_values_already_present_in_other() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
        });
        let other = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(10), hash(11)])),
            hash(3) => RequiredBlobs::Partial(HashSet::from_iter([hash(12), hash(13)])),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
        });

        index.replace(other.clone());
        assert_eq!(index, other);
    }

    #[test]
    fn replace_populates_known_values_from_self() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(123)])),
            hash(1) => RequiredBlobs::Partial(HashSet::new()),
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(10), hash(11)])),
            hash(5) => RequiredBlobs::Partial(HashSet::from_iter([hash(14)])),
        });
        let other = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::new()),
            hash(3) => RequiredBlobs::Complete(HashSet::from_iter([hash(12), hash(13)])),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::new()),
        });
        let merged = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(10), hash(11)])),
            hash(3) => RequiredBlobs::Complete(HashSet::from_iter([hash(12), hash(13)])),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::from_iter([hash(14)])),
        });

        index.replace(other);
        assert_eq!(index, merged);
    }

    #[test]
    fn replace_allows_both_self_and_other_values_to_be_populated() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(10), hash(11)])),
        });
        let other = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Complete(HashSet::from_iter([hash(11), hash(12)])),
        });

        index.replace(other);
        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(2) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(10),
                    hash(11),
                    hash(12),
                ])),
            })
        );
    }

    #[test]
    fn replace_extends_other_from_self() {
        let mut index = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(10)])),
            hash(3) => RequiredBlobs::Partial(HashSet::from_iter([hash(20)])),
            hash(4) => RequiredBlobs::Complete(HashSet::from_iter([hash(30)])),
        });
        let other = RetainedIndex::from_packages(hashmap! {
            hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(11)])),
            hash(3) => RequiredBlobs::Complete(HashSet::from_iter([hash(21)])),
            hash(4) => RequiredBlobs::Partial(HashSet::from_iter([hash(31)])),
        });

        index.replace(other);
        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(2) => RequiredBlobs::Partial(HashSet::from_iter([hash(10), hash(11)])),
                hash(3) => RequiredBlobs::Complete(HashSet::from_iter([hash(20), hash(21)])),
                hash(4) => RequiredBlobs::Complete(HashSet::from_iter([hash(30), hash(31)])),
            })
        );
    }

    #[fuchsia::test]
    async fn record_inspect_records_state_and_blobs_count() {
        let index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Partial(HashSet::new()),
            hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(10), hash(11)])),
            hash(2) => RequiredBlobs::Complete(HashSet::new()),
            hash(3) => RequiredBlobs::Complete(HashSet::from_iter([hash(30)])),
        });

        let inspector = finspect::Inspector::default();
        inspector.root().record_child("retained", |n| index.record_inspect(n));

        diagnostics_assertions::assert_data_tree!(
            inspector,
            root: {
                "retained": {
                    "entries": {
                        hash(0).to_string() => {
                            "state": "partial-blob-set",
                            "blobs-count": 0u64,
                        },
                        hash(1).to_string() => {
                            "state": "partial-blob-set",
                            "blobs-count": 2u64,
                        },
                        hash(2).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 0u64,
                        },
                        hash(3).to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 1u64,
                        },
                    }
                }
            }
        );
    }

    #[test]
    fn all_blobs_produces_union_of_meta_and_content_hashes() {
        let index = RetainedIndex::from_packages(hashmap! {
            hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1), hash(2), hash(3)])),
            hash(4) => RequiredBlobs::Partial(HashSet::new()),
            hash(5) => RequiredBlobs::Partial(HashSet::from_iter([hash(0), hash(4)])),
        });

        let expected = (0..=5).map(hash).collect::<HashSet<Hash>>();
        assert_eq!(expected.len(), 6);

        assert_eq!(expected, index.all_blobs());
    }

    #[fuchsia::test]
    async fn populate_retained_index_maps_missing_packages_to_empty_partial_blobs() {
        let (_blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        let index = populate_retained_index(&blobfs, [hash(0), hash(1), hash(2)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::new()),
                hash(1) => RequiredBlobs::Partial(HashSet::new()),
                hash(2) => RequiredBlobs::Partial(HashSet::new()),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_maps_present_packages_to_all_content_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(
            &blobfs_fake,
            hash(0),
            "pkg-0",
            vec![hash(1), hash(2), hash(30)],
            [],
        );
        add_meta_far_to_blobfs(
            &blobfs_fake,
            hash(10),
            "pkg-1",
            vec![hash(11), hash(12), hash(30)],
            [],
        );

        let index = populate_retained_index(&blobfs, [hash(0), hash(10)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(1),
                    hash(2),
                    hash(30),
                ])),
                hash(10) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(11),
                    hash(12),
                    hash(30),
                ])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_maps_invalid_meta_far_to_empty_partial_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        blobfs_fake.add_blob(hash(0), b"invalid blob");
        add_meta_far_to_blobfs(
            &blobfs_fake,
            hash(10),
            "pkg-0",
            vec![hash(1), hash(2), hash(3)],
            [],
        );

        let index = populate_retained_index(&blobfs, [hash(0), hash(10)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::new()),
                hash(10) => RequiredBlobs::Complete(HashSet::from_iter([
                    hash(1),
                    hash(2),
                    hash(3),
                ])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_dedupes_content_blobs() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", vec![hash(1), hash(1)], []);

        let index = populate_retained_index(&blobfs, [hash(0)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1)])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_adds_subpackage() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(1)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [hash(2)], []);

        let index = populate_retained_index(&blobfs, [hash(0)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1), hash(2)])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_adds_subpackage_of_subpackage() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(1)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(3)], []);

        let index = populate_retained_index(&blobfs, [hash(0)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(1), hash(2), hash(3)])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_ignores_missing_subpackage_and_marks_partial() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(1), hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [hash(3)], []);

        let index = populate_retained_index(&blobfs, [hash(0)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(1), hash(2), hash(3)])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_duplicate_subpackage() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [], [hash(3)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(3), "pkg-3", [], []);

        let index = populate_retained_index(&blobfs, [hash(0), hash(1)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Complete(HashSet::from_iter([hash(2), hash(3)])),
                hash(1) => RequiredBlobs::Complete(HashSet::from_iter([hash(2), hash(3)])),
            })
        );
    }

    #[fuchsia::test]
    async fn populate_retained_index_duplicate_partial_subpackage_marks_all_dependents_partial() {
        let (blobfs_fake, blobfs) = fuchsia_pkg_testing::blobfs::Fake::new();

        // `hash(2)` is a subpackage of both retained packages, and its subpackage `hash(3)` is
        // missing, so whichever package is scanned second must observe `Partial` through the
        // memoized `hash(2)`.
        add_meta_far_to_blobfs(&blobfs_fake, hash(0), "pkg-0", [], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(1), "pkg-1", [], [hash(2)]);
        add_meta_far_to_blobfs(&blobfs_fake, hash(2), "pkg-2", [], [hash(3)]);

        let index = populate_retained_index(&blobfs, [hash(0), hash(1)].into()).await;

        assert_eq!(
            index,
            RetainedIndex::from_packages(hashmap! {
                hash(0) => RequiredBlobs::Partial(HashSet::from_iter([hash(2), hash(3)])),
                hash(1) => RequiredBlobs::Partial(HashSet::from_iter([hash(2), hash(3)])),
            })
        );
    }
}
