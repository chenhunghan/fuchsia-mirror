// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module tests the specific properties of the fuchsia.pkg.resolution/PackageResolver.
//! This capability behaves like the -full resolver, it was made into a separate FIDL so that it
//! could be exposed to the toolbox realm (and therefore tools like ffx) without constraining
//! development of internal package resolution APIs.
//! All of its behaviors are already tested by the full resolver tests, we just need to check that
//! it is exposed correctly.

use fidl_fuchsia_pkg_resolution as fpkg_resolution;
use std::sync::Arc;

#[fuchsia::test]
async fn resolve() {
    let package = fuchsia_pkg_testing::PackageBuilder::new("test-package")
        .add_resource_at("blob", "blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&package)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo).server().start().unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&package],
        ))
        .build()
        .await;

    assert!(env.blobfs.list_blobs().unwrap().is_disjoint(&package.list_blobs()));
    let _: fpkg_resolution::ResolveResult =
        env.resolve_toolbox("fuchsia-pkg://example.org/test-package").await.unwrap();
    assert!(env.blobfs.list_blobs().unwrap().is_superset(&package.list_blobs()));
}
