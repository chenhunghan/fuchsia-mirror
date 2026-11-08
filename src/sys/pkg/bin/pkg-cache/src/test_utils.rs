// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_hash::Hash;
use fuchsia_pkg::{MetaContents, MetaPackage, MetaSubpackages};
use std::collections::BTreeMap;
use std::convert::TryInto as _;
use std::io;

pub fn add_meta_far_to_blobfs(
    blobfs: &fuchsia_pkg_testing::blobfs::Fake,
    hash: impl Into<Hash>,
    package_name: impl Into<String>,
    needed_blobs: impl IntoIterator<Item = Hash>,
    subpackages: impl IntoIterator<Item = Hash>,
) {
    let meta_far = get_meta_far(package_name, needed_blobs, subpackages);
    blobfs.add_blob(hash.into(), meta_far);
}

pub fn get_meta_far(
    package_name: impl Into<String>,
    needed_blobs: impl IntoIterator<Item = Hash>,
    subpackages: impl IntoIterator<Item = Hash>,
) -> Vec<u8> {
    let meta_subpackages =
        MetaSubpackages::from_iter(subpackages.into_iter().enumerate().map(|(i, hash)| {
            (fuchsia_url::RelativePackageUrl::parse(&format!("subpackage-{i}")).unwrap(), hash)
        }));
    let mut meta_subpackages_bytes = Vec::new();
    meta_subpackages.serialize(&mut meta_subpackages_bytes).unwrap();

    get_meta_far_with_subpackages_bytes(package_name, needed_blobs, meta_subpackages_bytes)
}

/// Like `get_meta_far`, but uses `subpackages_bytes` as the contents of the subpackages manifest
/// as-is, e.g. to create a meta.far with an invalid subpackages manifest.
pub fn get_meta_far_with_subpackages_bytes(
    package_name: impl Into<String>,
    needed_blobs: impl IntoIterator<Item = Hash>,
    subpackages_bytes: impl AsRef<[u8]>,
) -> Vec<u8> {
    let meta_contents = MetaContents::from_map(
        needed_blobs
            .into_iter()
            .enumerate()
            .map(|(i, hash)| (format!("{i}-{hash}"), hash))
            .collect(),
    )
    .unwrap();
    let mut meta_contents_bytes = Vec::new();
    meta_contents.serialize(&mut meta_contents_bytes).unwrap();

    let meta_package =
        MetaPackage::from_name_and_variant_zero(package_name.into().try_into().unwrap());
    let mut meta_package_bytes = Vec::new();
    meta_package.serialize(&mut meta_package_bytes).unwrap();

    let mut path_content_map: BTreeMap<&str, (u64, Box<dyn io::Read + '_>)> = BTreeMap::new();
    for (path, content) in [
        (MetaContents::PATH, meta_contents_bytes.as_slice()),
        (MetaPackage::PATH, meta_package_bytes.as_slice()),
        (MetaSubpackages::PATH, subpackages_bytes.as_ref()),
    ] {
        path_content_map.insert(path, (content.len() as u64, Box::new(content)));
    }
    let mut meta_far = Vec::new();
    fuchsia_archive::write(&mut meta_far, path_content_map).unwrap();
    meta_far
}
