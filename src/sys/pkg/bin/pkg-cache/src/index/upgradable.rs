// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::frozen_index::{BaseIndex, CacheIndex};
use fidl_fuchsia_pkg as fpkg;
use fuchsia_hash::Hash;
use fuchsia_url::fuchsia_pkg::{PinnedAbsolutePackageUrl, UnpinnedAbsolutePackageUrl};
use log::error;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::{HashMap, HashSet};

/// Proof that [`UpgradableIndex::start_opening`] incremented the `opening` refcount for a
/// package, to be returned to [`UpgradableIndex::stop_opening`].
#[derive(Debug)]
#[must_use = "must be passed to UpgradableIndex::stop_opening when done opening"]
pub(super) struct StopOpening(Hash);

/// Outcome of resolving an upgradable package URL in [`UpgradableIndex::start_opening`].
#[derive(Debug)]
#[must_use]
pub(super) enum ResolvedUpgradable {
    /// Resolved from `SetUpgradableUrls`; `opening` refcount was incremented and must be
    /// decremented via [`UpgradableIndex::stop_opening`].
    Upgradable(Hash, StopOpening),
    /// Resolved from the permanently protected `CacheIndex` fallback; `opening` was not modified.
    CacheFallback(Hash),
}

/// Tracks upgradable package URLs, the hashes they currently resolve to, and upgraded package
/// hashes currently being opened.
#[derive(Debug, Default)]
pub(super) struct UpgradableIndex {
    packages: HashMap<UnpinnedAbsolutePackageUrl, Hash>,
    opening: HashMap<Hash, usize>,
}

impl UpgradableIndex {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Resolves `url` to its current hash. If resolved from `SetUpgradableUrls` (rather than the
    /// `CacheIndex` fallback), increments the `opening` refcount for that hash and returns a
    /// [`StopOpening`] token so the caller pairs it with [`Self::stop_opening`].
    pub(super) fn start_opening(
        &mut self,
        url: &UnpinnedAbsolutePackageUrl,
        cache_packages: &CacheIndex,
    ) -> Option<ResolvedUpgradable> {
        if let Some(&hash) = self.packages.get(url) {
            *self.opening.entry(hash).or_insert(0) += 1;
            Some(ResolvedUpgradable::Upgradable(hash, StopOpening(hash)))
        } else {
            cache_packages
                .root_package_urls_and_hashes()
                .get(url)
                .map(|&hash| ResolvedUpgradable::CacheFallback(hash))
        }
    }

    /// Decrements the `opening` refcount for the package in [`StopOpening`], removing it when the
    /// last opener finishes.
    pub(super) fn stop_opening(&mut self, StopOpening(pkg): StopOpening) {
        match self.opening.entry(pkg) {
            Occupied(mut entry) => {
                let count = entry.get_mut();
                *count = count.saturating_sub(1);
                if *count == 0 {
                    let _: usize = entry.remove();
                }
            }
            // `StopOpening` is not `Clone`, is only created by `Self::start_opening`, and
            // `PackageIndex` never replaces its `UpgradableIndex`, so this branch should not be
            // reachable in production.
            Vacant(_) => {
                error!("UpgradableIndex cannot stop opening an unknown package {pkg}");
            }
        }
    }

    /// Associates upgradable package URLs with their hashes.
    pub(super) fn set_upgradable_urls(
        &mut self,
        pinned_urls: Vec<fpkg::PackageUrl>,
        base_packages: &BaseIndex,
    ) -> Result<(), fpkg::SetUpgradableUrlsError> {
        let mut partial_set = false;
        for fpkg::PackageUrl { url } in pinned_urls {
            let url = match url.parse::<PinnedAbsolutePackageUrl>() {
                Ok(url) => url,
                Err(e) => {
                    error!("failed to parse pinned url {url:?}: {e:?}");
                    partial_set = true;
                    continue;
                }
            };
            if base_packages.root_package_urls_and_hashes().contains_key(url.as_unpinned()) {
                error!("upgrade base package {} is not allowed", url.as_unpinned());
                partial_set = true;
                continue;
            }
            let (unpinned, hash) = url.into_unpinned_and_hash();
            self.packages.insert(unpinned, hash);
        }

        if partial_set { Err(fpkg::SetUpgradableUrlsError::PartialSet) } else { Ok(()) }
    }

    /// Returns the root meta.far hashes of all currently registered upgradable packages and any
    /// previously registered upgradable packages that are still being opened.
    pub(super) fn protected_packages(&self) -> HashSet<Hash> {
        self.packages.values().chain(self.opening.keys()).copied().collect()
    }

    #[cfg(test)]
    pub(super) fn opening(&self) -> &HashMap<Hash, usize> {
        &self.opening
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(b: u8) -> Hash {
        [b; 32].into()
    }

    #[fuchsia::test]
    fn stop_opening_unknown_package_does_not_panic() {
        let upgradable_url: UnpinnedAbsolutePackageUrl =
            "fuchsia-pkg://fuchsia.com/upgradable".parse().unwrap();
        let base_packages = BaseIndex::new_test_only(HashSet::new(), []);
        let cache_packages = CacheIndex::new_test_only(HashSet::new(), []);

        let mut other = UpgradableIndex::new();
        other
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{upgradable_url}?hash={}", hash(1)) }],
                &base_packages,
            )
            .unwrap();
        let Some(ResolvedUpgradable::Upgradable(_, stop)) =
            other.start_opening(&upgradable_url, &cache_packages)
        else {
            panic!("expected Upgradable resolution");
        };

        let mut index = UpgradableIndex::new();
        index.stop_opening(stop);
        assert_eq!(index.opening, HashMap::new());
    }
}
