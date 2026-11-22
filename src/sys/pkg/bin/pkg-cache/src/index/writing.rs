// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect as finspect;
use fuchsia_merkle::Hash;
use log::error;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::{HashMap, HashSet};

/// Proof that [`WritingIndex::start`] incremented the protection count for a package, to be
/// returned to [`WritingIndex::stop`].
#[derive(Debug)]
#[must_use = "must be passed to WritingIndex::stop when done writing"]
pub(super) struct StopWriting(Hash);

/// Protects packages from GC while they are being written. This index does not protect packages
/// from GC after they have been written, so, to guarantee continuous protection, clients should
/// arrange for protection from another source, e.g. open package tracking, before informing the
/// index that the write is complete.
///
/// Allows clients to concurrently protect the same package (implemented with reference counting);
/// packages will be protected until all clients say they are done writing.
/// This avoids race conditions where pkg-resolver retries a PackageCache.Get call before
/// pkg-cache tears down the original call.
#[derive(Debug)]
pub(super) struct WritingIndex {
    pkg_to_refcount_and_blobs: HashMap<Hash, (usize, HashSet<Hash>)>,
}

impl WritingIndex {
    /// Creates an empty `WritingIndex`.
    pub(super) fn new() -> Self {
        Self { pkg_to_refcount_and_blobs: HashMap::new() }
    }

    /// Starts to protect `pkg`. Supports multiple outstanding calls to `start` for the same `pkg`.
    /// `pkg` will be protected until each returned [`StopWriting`] token has been passed to
    /// [`Self::stop`].
    pub(super) fn start(&mut self, pkg: Hash) -> StopWriting {
        self.pkg_to_refcount_and_blobs.entry(pkg).or_insert_with(|| (0, HashSet::new())).0 += 1;
        StopWriting(pkg)
    }

    /// Adds `blobs` to the set of blobs protected by `pkg`.
    /// `Self::start` must already have been called with `pkg`.
    pub(super) fn add_blobs(
        &mut self,
        pkg: &Hash,
        blobs: &HashSet<Hash>,
    ) -> Result<(), AddBlobsError> {
        self.pkg_to_refcount_and_blobs
            .get_mut(pkg)
            .ok_or(AddBlobsError::UnknownPackage { pkg: *pkg })?
            .1
            .extend(blobs);
        Ok(())
    }

    /// Returns all blobs protected by the index.
    pub(super) fn all_blobs(&self) -> HashSet<Hash> {
        let mut ret = HashSet::new();
        self.pkg_to_refcount_and_blobs.iter().for_each(|(p, (_, bs))| {
            ret.insert(*p);
            ret.extend(bs);
        });
        ret
    }

    /// Decrements the protection count for the package in [`StopWriting`], removing it from the
    /// index when the last outstanding writer stops.
    pub(super) fn stop(&mut self, StopWriting(pkg): StopWriting) {
        match self.pkg_to_refcount_and_blobs.entry(pkg) {
            Occupied(mut entry) => {
                let count = &mut entry.get_mut().0;
                *count = count.saturating_sub(1);
                if *count == 0 {
                    entry.remove();
                }
            }
            // `StopWriting` is not `Clone`, is only created by `Self::start`, and `PackageIndex`
            // never replaces its `WritingIndex`, so this branch should not be reachable in
            // production.
            Vacant(_) => {
                error!("WritingIndex cannot stop writing an unknown package {pkg}");
            }
        }
    }

    /// Records self to `node`.
    pub(super) fn record_inspect(&self, node: &finspect::Node) {
        for (pkg, (count, _)) in &self.pkg_to_refcount_and_blobs {
            node.record_child(pkg.to_string(), |n| {
                n.record_uint("count", *count as u64);
            })
        }
    }

    /// The blobs (content and subpackage) known to be required by `package`. Not guaranteed to be
    /// complete (more required blobs are discovered as the meta.far and subpackage meta.fars are
    /// written).
    pub(super) fn required_blobs(&self, package: &Hash) -> Option<&HashSet<Hash>> {
        self.pkg_to_refcount_and_blobs.get(package).map(|(_, bs)| bs)
    }

    #[cfg(test)]
    pub(super) fn packages(&self) -> HashMap<Hash, HashSet<Hash>> {
        self.pkg_to_refcount_and_blobs.iter().map(|(p, (_, bs))| (*p, bs.clone())).collect()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AddBlobsError {
    #[error("WritingIndex cannot add blobs to an unknown package {pkg}")]
    UnknownPackage { pkg: Hash },
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    fn hash(b: u8) -> Hash {
        [b; 32].into()
    }

    #[fuchsia::test]
    fn regular_flow() {
        let mut index = WritingIndex::new();
        assert_eq!(index.all_blobs(), HashSet::new());

        let stop = index.start(hash(0));
        assert_eq!(index.all_blobs(), HashSet::from_iter([hash(0)]));

        let () = index.add_blobs(&hash(0), &HashSet::from_iter([hash(1), hash(2)])).unwrap();
        assert_eq!(index.all_blobs(), HashSet::from_iter([hash(0), hash(1), hash(2)]));
        assert_eq!(index.required_blobs(&hash(0)), Some(&HashSet::from_iter([hash(1), hash(2)])));

        index.stop(stop);
        assert_eq!(index.all_blobs(), HashSet::new());
        assert_eq!(index.pkg_to_refcount_and_blobs, HashMap::new());
    }

    #[fuchsia::test]
    fn refcount_prevents_removal() {
        let mut index = WritingIndex::new();

        let stop0 = index.start(hash(0));
        let () = index.add_blobs(&hash(0), &HashSet::from_iter([hash(1), hash(2)])).unwrap();

        let stop1 = index.start(hash(0));
        assert_eq!(index.all_blobs(), HashSet::from_iter([hash(0), hash(1), hash(2)]));
        assert_eq!(index.pkg_to_refcount_and_blobs[&hash(0)].0, 2);

        index.stop(stop0);
        assert_eq!(index.all_blobs(), HashSet::from_iter([hash(0), hash(1), hash(2)]));
        assert_eq!(index.pkg_to_refcount_and_blobs[&hash(0)].0, 1);

        index.stop(stop1);
        assert_eq!(index.all_blobs(), HashSet::new());
        assert_eq!(index.pkg_to_refcount_and_blobs, HashMap::new());
    }

    #[fuchsia::test]
    fn add_blobs_to_unknown_package() {
        let mut index = WritingIndex::new();
        let stop = index.start(hash(0));

        assert_matches!(
            index.add_blobs(&hash(1), &HashSet::from_iter([hash(1), hash(2)])),
            Err(AddBlobsError::UnknownPackage { pkg }) if pkg == hash(1)
        );

        index.stop(stop);
    }

    #[fuchsia::test]
    fn stop_unknown_package_does_not_panic() {
        let stop = WritingIndex::new().start(hash(0));
        let mut index = WritingIndex::new();
        index.stop(stop);
        assert_eq!(index.pkg_to_refcount_and_blobs, HashMap::new());
    }

    #[fuchsia::test]
    async fn inspect() {
        let mut index = WritingIndex::new();
        let stop = index.start(hash(0));
        let () = index.add_blobs(&hash(0), &HashSet::from_iter([hash(1), hash(2)])).unwrap();

        let inspector = finspect::Inspector::default();
        inspector.root().record_child("writing-index", |n| index.record_inspect(n));

        diagnostics_assertions::assert_data_tree!(
            inspector,
            root: {
                "writing-index": {
                    "0000000000000000000000000000000000000000000000000000000000000000": {
                        "count": 1u64
                    }
                }
            }
        );

        index.stop(stop);
    }
}
