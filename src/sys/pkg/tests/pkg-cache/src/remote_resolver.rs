// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module tests shared properties of the package resolution capabilities that use an external
//! package authority and fetch remote blobs (capabilities that use the PackageFetcher, i.e. the
//! -full and -ota resolvers).
//!
//! Uses the -ota resolver capability because it just uses the PackageFetcher with retained GC
//! protection, unlike the -full resolver which has additional logic for base package short
//! circuiting and cache fallback, etc.

use delivery_blob::DeliveryBlobType;
use diagnostics_assertions::{assert_data_tree, tree_assertion};
use fidl_fuchsia_pkg as fpkg;
use fuchsia_async as fasync;
use futures::future::FutureExt as _;
use futures::stream::StreamExt as _;
use rand::{RngReader, SeedableRng as _};
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;

// Creates a repo that contains `pkg`, resolves `pkg`, and verifies the resolved package directory
// against `pkg`.
async fn verify_resolution(
    pkg: &fuchsia_pkg_testing::Package,
    delivery_blob_type: DeliveryBlobType,
) {
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .delivery_blob_type(delivery_blob_type)
            .add_package(pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo).server().start().unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());

    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages_with_type(
            &repo_config,
            &[pkg],
            delivery_blob_type,
        ))
        .build()
        .await;

    let (package, _context) =
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await.unwrap();

    let () = pkg.verify_contents(&package).await.unwrap();
    let () = env.stop().await;
}

#[test_case::test_case(DeliveryBlobType::Type1)]
#[test_case::test_case(DeliveryBlobType::Type2)]
#[fuchsia::test]
async fn metafar_only(delivery_blob_type: DeliveryBlobType) {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package").build().await.unwrap();
    let () = verify_resolution(&pkg, delivery_blob_type).await;
}

#[test_case::test_case(DeliveryBlobType::Type1)]
#[test_case::test_case(DeliveryBlobType::Type2)]
#[fuchsia::test]
async fn empty_blob(delivery_blob_type: DeliveryBlobType) {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package")
        .add_resource_at("empty-blob", "".as_bytes())
        .build()
        .await
        .unwrap();
    let () = verify_resolution(&pkg, delivery_blob_type).await;
}

#[test_case::test_case(DeliveryBlobType::Type1)]
#[test_case::test_case(DeliveryBlobType::Type2)]
#[fuchsia::test]
async fn duplicate_blob(delivery_blob_type: DeliveryBlobType) {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package")
        .add_resource_at("small-blob", "blob-contents".as_bytes())
        .add_resource_at("duplicate-blob", "blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let () = verify_resolution(&pkg, delivery_blob_type).await;
}

#[test_case::test_case(DeliveryBlobType::Type1)]
#[test_case::test_case(DeliveryBlobType::Type2)]
#[fuchsia::test]
async fn large_blob(delivery_blob_type: DeliveryBlobType) {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package")
        .add_resource_at(
            "large-blob",
            RngReader(rand::rngs::StdRng::from_seed([0u8; 32])).take(1024 * 1024),
        )
        .build()
        .await
        .unwrap();
    let () = verify_resolution(&pkg, delivery_blob_type).await;
}

#[test_case::test_case(DeliveryBlobType::Type1)]
#[test_case::test_case(DeliveryBlobType::Type2)]
#[fuchsia::test]
async fn many_blobs(delivery_blob_type: DeliveryBlobType) {
    let mut pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package");
    let mut rng = rand::rngs::StdRng::from_seed([0u8; 32]);
    for i in 0..200 {
        pkg = pkg.add_resource_at(format!("blob-{i}"), RngReader(&mut rng).take(10));
    }
    let pkg = pkg.build().await.unwrap();
    let () = verify_resolution(&pkg, delivery_blob_type).await;
}

#[fuchsia::test]
async fn merkle_pin_overrides_authority() {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("pin-me").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo).server().start().unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());

    // The PkgAuthority will return the hash of zeroes, make sure this is not the package's actual
    // hash so that a successful resolve means that the authority's hash was overridden by the pin.
    assert_ne!(*pkg.hash(), [0; 32].into());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::new(HashMap::from([
            (
                "fuchsia-pkg://example.org/pin-me".to_owned(),
                Ok(([0; 32].into(), format!("{}/1", repo_config.mirrors()[0].blob_mirror_url()))),
            ),
            (
                "fuchsia-pkg://example.org/missing-package".to_owned(),
                Err(fpkg::AuthorityLookupError::PackageNotFound),
            ),
        ])))
        .build()
        .await;

    let (package, _context) = env
        .resolve_ota(&format!("fuchsia-pkg://example.org/pin-me?hash={}", pkg.hash()))
        .await
        .unwrap();

    let () = pkg.verify_contents(&package).await.unwrap();

    // Even if pinned, the package should still be known to the authority to be resolved.
    std::assert_matches!(
        env.resolve_ota(&format!("fuchsia-pkg://example.org/missing-package?hash={}", pkg.hash()))
            .await,
        Err(fpkg::ResolveError::PackageNotFound)
    );
}

#[fuchsia::test(logging_tags = ["RESOLVE_TEST"])]
async fn error_codes() {
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::new(HashMap::from([
            (
                "fuchsia-pkg://example.org/missing-repo".to_owned(),
                Err(fpkg::AuthorityLookupError::RepositoryNotFound),
            ),
            (
                "fuchsia-pkg://example.org/missing-package".to_owned(),
                Err(fpkg::AuthorityLookupError::PackageNotFound),
            ),
        ])))
        .build()
        .await;

    // Invalid URL
    std::assert_matches!(
        env.resolve_ota("fuchsia-pkg://test/bad-url!").await,
        Err(fidl_fuchsia_pkg::ResolveError::InvalidUrl)
    );

    // Nonexistent repo
    std::assert_matches!(
        env.resolve_ota("fuchsia-pkg://example.org/missing-repo").await,
        Err(fidl_fuchsia_pkg::ResolveError::RepoNotFound)
    );

    // Nonexistent package
    std::assert_matches!(
        env.resolve_ota("fuchsia-pkg://example.org/missing-package").await,
        Err(fidl_fuchsia_pkg::ResolveError::PackageNotFound)
    );
}

#[fuchsia::test]
async fn retry_blob_fetch_network_error() {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fuchsia_pkg_testing::serve::responder::ForPathPrefix::new(
            "/blobs",
            fuchsia_pkg_testing::serve::responder::OncePerPath::new(
                fuchsia_pkg_testing::serve::responder::StaticResponseCode::server_error(),
            ),
        ))
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());

    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg],
        ))
        .build()
        .await;

    let (package, _context) =
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await.unwrap();

    let () = pkg.verify_contents(&package).await.unwrap();
    let path = format!("/blobs/1/{}", pkg.hash());
    assert_eq!(
        *served_repository.history().lock(),
        vec![
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: path.clone(),
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
            },
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: path.clone(),
                status: http::StatusCode::OK,
            }
        ]
    );
}

#[fuchsia::test]
async fn retry_blob_fetch_network_rate_limit() {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fuchsia_pkg_testing::serve::responder::ForPathPrefix::new(
            "/blobs",
            fuchsia_pkg_testing::serve::responder::OncePerPath::new(
                fuchsia_pkg_testing::serve::responder::StaticResponseCode::too_many_requests(),
            ),
        ))
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());

    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg],
        ))
        .build()
        .await;

    let (package, _context) =
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await.unwrap();

    let () = pkg.verify_contents(&package).await.unwrap();
    let path = format!("/blobs/1/{}", pkg.hash());
    assert_eq!(
        *served_repository.history().lock(),
        vec![
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: path.clone(),
                status: http::StatusCode::TOO_MANY_REQUESTS,
            },
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: path.clone(),
                status: http::StatusCode::OK,
            }
        ]
    );
}

#[fuchsia::test]
async fn test_concurrent_blob_writes() {
    let duplicate_contents = "duplicate-contents".as_bytes();
    let pkg1 = fuchsia_pkg_testing::PackageBuilder::new("package1")
        .add_resource_at("duplicate-blob-1", duplicate_contents)
        .build()
        .await
        .unwrap();
    let pkg2 = fuchsia_pkg_testing::PackageBuilder::new("package2")
        .add_resource_at("duplicate-blob-2", duplicate_contents)
        .add_resource_at("unique-blob", "unique-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let duplicate_blob_merkle =
        pkg1.meta_contents().unwrap().contents()["duplicate-blob-1"].to_string();
    let unique_blob_merkle = pkg2.meta_contents().unwrap().contents()["unique-blob"];

    // A responder to block the download of the duplicate blob.
    let (blocking_responder, unblocking_closure_receiver) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodyOnce::new();
    let blocking_responder = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{}", duplicate_blob_merkle),
        blocking_responder,
    );

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg1)
            .add_package(&pkg2)
            .build()
            .await
            .unwrap(),
    );
    let served_repository =
        Arc::clone(&repo).server().response_overrider(blocking_responder).start().unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg1, &pkg2],
        ))
        .build()
        .await;

    // pkg-cache OTA resolver uses try_for_each_concurrent to handle its FIDL request stream, so the
    // concurrent requests can be made on the same channel.
    let (pkg1_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg1_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package1", server_end);

    // Wait for the GET request for the duplicate blob to be received by the blob server.
    let send_shared_blob_body = unblocking_closure_receiver.await.unwrap();

    // With the duplicate blob fetch active in the blob fetch queue, start the second resolve.
    let (pkg2_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg2_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package2", server_end);

    // Wait for the unique blob to exist in blobfs. This approximates waiting for the second resolve
    // to request the duplicate blob from the fetch queue.
    let blobfs_reader = env.blobfs.blob_reader_proxy().unwrap();
    while blobfs_reader
        .get_vmo(unique_blob_merkle.as_bytes().try_into().unwrap())
        .await
        .expect("Getting vmo")
        .is_err()
    {
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
    }

    // At this point, both package resolves should be blocked on the shared blob download. Unblock
    // the server and verify both packages resolve to valid directories.
    let () = send_shared_blob_body();
    let ((), ()) = futures::join!(
        async move {
            let _: fpkg::ResolutionContext = pkg1_fut.await.unwrap().unwrap();
        },
        async move {
            let _: fpkg::ResolutionContext = pkg2_fut.await.unwrap().unwrap();
        },
    );
    let () = pkg1.verify_contents(&pkg1_dir).await.unwrap();
    let () = pkg2.verify_contents(&pkg2_dir).await.unwrap();

    // The duplicate blob should be the second blob requested (the responder blocks the response
    // body, not the response header, and so will not shift the request down in the history) and it
    // should only have been requested once.
    assert_eq!(
        *served_repository.history().lock(),
        vec![
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: format!("/blobs/1/{}", pkg1.hash()),
                status: http::StatusCode::OK,
            },
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: format!("/blobs/1/{}", duplicate_blob_merkle),
                status: http::StatusCode::OK,
            },
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: format!("/blobs/1/{}", pkg2.hash()),
                status: http::StatusCode::OK,
            },
            fuchsia_pkg_testing::serve::HistoryEntry {
                method: http::Method::GET,
                path: format!("/blobs/1/{}", unique_blob_merkle),
                status: http::StatusCode::OK,
            },
        ]
    );
}

// TODO(https://fxbug.dev/308158482): re-enable when ring works on riscv64
#[cfg(not(target_arch = "riscv64"))]
async fn test_https_endpoint(bind_addr: impl Into<std::net::IpAddr>) {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package")
        .add_resource_at("blob", "blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .use_https_domain(fuchsia_pkg_testing::serve::Domain::TestFuchsiaCom)
        .bind_to_addr(bind_addr)
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());

    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg],
        ))
        .build()
        .await;

    let (package, _context) =
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await.unwrap();
    let () = pkg.verify_contents(&package).await.unwrap();
}

// TODO(https://fxbug.dev/308158482): re-enable when ring works on riscv64
#[cfg(not(target_arch = "riscv64"))]
#[fuchsia::test]
async fn https_endpoint_ipv6_only() {
    test_https_endpoint(std::net::Ipv6Addr::LOCALHOST).await
}

// TODO(https://fxbug.dev/308158482): re-enable when ring works on riscv64
#[cfg(not(target_arch = "riscv64"))]
#[fuchsia::test]
async fn https_endpoint_ipv4_only() {
    test_https_endpoint(std::net::Ipv4Addr::LOCALHOST).await
}

// Tests subpackage recursion, including:
//   1. if a recursed subpackage meta.far was already fetched as a content blob.
//   2. duplicate subpackages
#[fuchsia::test]
async fn subsubpackage() {
    let subsubpackage = fuchsia_pkg_testing::PackageBuilder::new("subsubpackage")
        .add_resource_at("subsubpackage-blob", "subsubpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let subpackage = fuchsia_pkg_testing::PackageBuilder::new("subpackage")
        .add_subpackage("my-subsubpackage", &subsubpackage)
        .add_subpackage("duplicate-subpackage", &subsubpackage)
        .add_resource_at("subpackage-blob", "subpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let superpackage = fuchsia_pkg_testing::PackageBuilder::new("superpackage")
        .add_subpackage("my-subpackage", &subpackage)
        .add_resource_at("superpackage-blob", "superpackage-blob-contents".as_bytes())
        .add_resource_at("subsubpackage-meta-far", subsubpackage.contents().0.contents.as_slice())
        .build()
        .await
        .unwrap();

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&superpackage)
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
            &[&superpackage],
        ))
        .build()
        .await;

    let (super_dir, context) =
        env.resolve_ota("fuchsia-pkg://example.org/superpackage").await.unwrap();
    let () = superpackage.verify_contents(&super_dir).await.unwrap();

    let (sub_dir, context) = env.resolve_with_context_ota("my-subpackage", &context).await.unwrap();
    let () = subpackage.verify_contents(&sub_dir).await.unwrap();

    let (subsub_dir, _context) =
        env.resolve_with_context_ota("my-subsubpackage", &context).await.unwrap();
    let () = subsubpackage.verify_contents(&subsub_dir).await.unwrap();

    let (subsub_dir, _context) =
        env.resolve_with_context_ota("duplicate-subpackage", &context).await.unwrap();
    let () = subsubpackage.verify_contents(&subsub_dir).await.unwrap();
}

#[fuchsia::test]
async fn resolve_of_already_cached_package_is_not_blocked_by_in_progress_blob_fetches() {
    let blocking_pkg =
        fuchsia_pkg_testing::PackageBuilder::new("blocking-package").build().await.unwrap();
    let cached_subpackage = fuchsia_pkg_testing::PackageBuilder::new("cached-subpackage")
        .add_resource_at("subpackage-blob", "subpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let cached_superpackage = fuchsia_pkg_testing::PackageBuilder::new("cached-superpackage")
        .add_subpackage("my-subpackage", &cached_subpackage)
        .add_resource_at("superpackage-blob", "superpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&blocking_pkg)
            .add_package(&cached_superpackage)
            .build()
            .await
            .unwrap(),
    );
    let (blocker, mut blocked_fetches) =
        fuchsia_pkg_testing::serve::responder::BlockResponseHeaders::new();
    let responder = fuchsia_pkg_testing::serve::responder::ForPathPrefix::new("/blobs/1/", blocker);
    let served_repository =
        Arc::clone(&repo).server().response_overrider(responder).start().unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&blocking_pkg, &cached_superpackage],
        ))
        .blob_fetch_concurrency_limit(1)
        .build()
        .await;

    let () = cached_superpackage.write_to_blobfs(&env.blobfs).await;

    // Wait for the meta.far to be blocked.
    let (blocking_pkg_dir, server_end) = fidl::endpoints::create_proxy();
    let mut blocking_pkg_fut = std::pin::pin!(
        env.proxies
            .ota_package_resolver
            .resolve("fuchsia-pkg://example.org/blocking-package", server_end)
            .fuse()
    );
    let blocked_meta_far = blocked_fetches.next().await.unwrap();

    // The already cached package should resolve even with a full blob fetch queue, and the blocking
    // resolve should not complete.
    futures::select_biased! {
        res  = blocking_pkg_fut => panic!("blocking resolve should not complete {res:?}"),
        already_cached_resolve = env
            .resolve_ota("fuchsia-pkg://example.org/cached-superpackage").fuse() => {
            let (dir, _context) = already_cached_resolve.unwrap();
            let () = cached_superpackage.verify_contents(&dir).await.unwrap();
        }
    }

    // Finish the blocked resolve to make sure it completes successfully and didn't get canceled by
    // timeout.
    let () = blocked_meta_far.unblock();
    let _: fpkg::ResolutionContext = blocking_pkg_fut.await.unwrap().unwrap();
    let () = blocking_pkg.verify_contents(&blocking_pkg_dir).await.unwrap();
}

#[fuchsia::test]
// TODO(https://fxbug.dev/519687989): Enable when Fuchsia system releases are managed in the repo.
#[ignore]
async fn mismatched_pinned_merkle_resolution_fails() {
    let pkg1 = fuchsia_pkg_testing::PackageBuilder::new("pkg1").build().await.unwrap();
    let pkg2 = fuchsia_pkg_testing::PackageBuilder::new("pkg2").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg1)
            .add_package(&pkg2)
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
            &[&pkg1],
        ))
        .build()
        .await;

    // Resolve "pkg1" but with the hash of "pkg2"
    assert_ne!(pkg1.hash(), pkg2.hash());
    let pkg1_url_with_pkg2_merkle = format!("fuchsia-pkg://example.org/pkg1?hash={}", pkg2.hash());

    std::assert_matches!(
        env.resolve_ota(&pkg1_url_with_pkg2_merkle).await,
        Err(fpkg::ResolveError::PackageNotFound)
    );
}

#[fuchsia::test]
async fn download_blob_header_timeout() {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fuchsia_pkg_testing::serve::responder::Hang)
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg],
        ))
        .blob_network_header_timeout_seconds(0)
        .build()
        .await;

    std::assert_matches!(
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await,
        Err(fpkg::ResolveError::UnavailableBlob)
    );
}

#[fuchsia::test]
async fn download_blob_body_timeout() {
    let pkg = fuchsia_pkg_testing::PackageBuilder::new("test-package").build().await.unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fuchsia_pkg_testing::serve::responder::HangBody)
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg],
        ))
        .blob_network_body_timeout_seconds(0)
        .build()
        .await;

    std::assert_matches!(
        env.resolve_ota("fuchsia-pkg://example.org/test-package").await,
        Err(fpkg::ResolveError::UnavailableBlob)
    );
}

#[test_case::test_case(blobfs_ramdisk::Implementation::CppBlobfs; "blobfs")]
#[test_case::test_case(blobfs_ramdisk::Implementation::Fxblob; "fxblob")]
#[fuchsia::test]
async fn does_not_fetch_up_to_date_blobs(blobfs_impl: blobfs_ramdisk::Implementation) {
    let subpackage = fuchsia_pkg_testing::PackageBuilder::new("subpackage")
        .add_resource_at("subpackage-blob", "subpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let superpackage = fuchsia_pkg_testing::PackageBuilder::new("superpackage")
        .add_subpackage("my-subpackage", &subpackage)
        .add_resource_at("superpackage-blob", "superpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&superpackage)
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
            &[&superpackage],
        ))
        .blobfs_impl(blobfs_impl)
        .build()
        .await;

    // First resolve should download all 4 blobs.
    let (_dir, _context) = env.resolve_ota("fuchsia-pkg://example.org/superpackage").await.unwrap();
    assert_eq!(std::mem::take(&mut *served_repository.history().lock()).len(), 4);

    // Second resolve should not download any blobs.
    let (dir, _context) = env.resolve_ota("fuchsia-pkg://example.org/superpackage").await.unwrap();
    assert_eq!(std::mem::take(&mut *served_repository.history().lock()).len(), 0);
    let () = superpackage.verify_contents(&dir).await.unwrap();

    // Delete some non-root blobs, only they should be downloaded.
    let blobfs_client = env.blobfs.client();
    let () = blobfs_client.delete_blob(subpackage.hash()).await.unwrap();
    let () = blobfs_client
        .delete_blob(&superpackage.content_blob_files().next().unwrap().merkle)
        .await
        .unwrap();
    let (dir, _context) = env.resolve_ota("fuchsia-pkg://example.org/superpackage").await.unwrap();
    assert_eq!(served_repository.history().lock().len(), 2);
    let () = superpackage.verify_contents(&dir).await.unwrap();
}

#[fuchsia::test]
async fn resolve_marks_retained_package_complete() {
    let subpackage = fuchsia_pkg_testing::PackageBuilder::new("subpackage")
        .add_resource_at("subpackage-blob", "subpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let superpackage = fuchsia_pkg_testing::PackageBuilder::new("superpackage")
        .add_subpackage("my-subpackage", &subpackage)
        .add_resource_at("superpackage-blob", "superpackage-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&superpackage)
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
            &[&superpackage],
        ))
        .build()
        .await;

    crate::replace_retained_packages(
        &env.proxies.retained_packages,
        &[(*superpackage.hash()).into()],
    )
    .await;
    assert_data_tree!(
        env.inspect_hierarchy().await,
        "root": contains {
            "index": contains {
                "retained": {
                    "entries": {
                        superpackage.hash().to_string() => {
                            "state": "partial-blob-set",
                            "blobs-count": 0u64,
                        },
                    },
                },
            },
        }
    );

    let (pkg_dir, _context) =
        env.resolve_full("fuchsia-pkg://example.org/superpackage").await.unwrap();
    let () = superpackage.verify_contents(&pkg_dir).await.unwrap();

    assert_data_tree!(
        env.inspect_hierarchy().await,
        "root": contains {
            "index": contains {
                "retained": {
                    "entries": {
                        superpackage.hash().to_string() => {
                            "state": "complete-blob-set",
                            "blobs-count": 3u64,
                        },
                    },
                },
            },
        }
    );
}

#[fuchsia::test]
async fn failed_resolve_stops_fetching_blobs() {
    const MAX_CONCURRENT_BLOB_FETCHES: u16 = 2;
    const CONTENT_BLOB_COUNT: usize = 10;

    let pkg_many_blobs = {
        let mut pkg = fuchsia_pkg_testing::PackageBuilder::new("many-blobs");
        for i in 0..CONTENT_BLOB_COUNT {
            pkg = pkg.add_resource_at(format!("blob_{i}"), format!("contents_{i}").as_bytes());
        }
        pkg.build().await.unwrap()
    };
    let pkg_different = fuchsia_pkg_testing::PackageBuilder::new("different-hash")
        .add_resource_at("diff_blob_0", &b"diff_contents_0"[..])
        .add_resource_at("diff_blob_1", &b"diff_contents_1"[..])
        .build()
        .await
        .unwrap();

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg_many_blobs)
            .add_package(&pkg_different)
            .build()
            .await
            .unwrap(),
    );

    let (fail_blocker, mut blocked_failing_fetch) =
        fuchsia_pkg_testing::serve::responder::BlockResponseHeaders::new();
    let (hang_blocker, mut blocked_other_fetches) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodies::new();
    let content_blob_paths: std::collections::HashSet<String> =
        pkg_many_blobs.content_blob_files().map(|b| format!("/blobs/1/{}", b.merkle)).collect();
    let failing_path = Arc::new(fuchsia_sync::Mutex::new(None::<String>));
    let fail_first_blob = fuchsia_pkg_testing::serve::responder::Filter::new(
        {
            let content_blob_paths = content_blob_paths.clone();
            let failing_path = Arc::clone(&failing_path);
            move |req: &http::Request<_>| {
                let path = req.uri().path();
                content_blob_paths.contains(path)
                    && failing_path.lock().get_or_insert_with(|| path.to_owned()) == path
            }
        },
        fuchsia_pkg_testing::serve::responder::Chain::new(vec![
            Box::new(fail_blocker),
            Box::new(fuchsia_pkg_testing::serve::responder::StaticResponseCode::not_found()),
        ]),
    );
    let block_other_blobs = fuchsia_pkg_testing::serve::responder::Filter::new(
        {
            let failing_path = Arc::clone(&failing_path);
            move |req: &http::Request<_>| {
                let path = req.uri().path();
                content_blob_paths.contains(path) && failing_path.lock().as_deref() != Some(path)
            }
        },
        hang_blocker,
    );
    let (diff_blocker, mut blocked_diff_fetches) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodies::new();
    let diff_blob_paths = pkg_different
        .content_blob_files()
        .map(|b| format!("/blobs/1/{}", b.merkle).into())
        .collect();
    let block_diff_blobs =
        fuchsia_pkg_testing::serve::responder::ForPaths::new(diff_blob_paths, diff_blocker);

    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fail_first_blob)
        .response_overrider(block_other_blobs)
        .response_overrider(block_diff_blobs)
        .start()
        .expect("Starting server succeeds");
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg_many_blobs, &pkg_different],
        ))
        .blob_fetch_concurrency_limit(MAX_CONCURRENT_BLOB_FETCHES)
        .build()
        .await;

    let (_many_dir, server_end) = fidl::endpoints::create_proxy();
    let many_fut = env
        .proxies
        .ota_package_resolver
        .resolve("fuchsia-pkg://example.org/many-blobs", server_end);

    // Wait for both concurrency slots to block on the server, then fail one with 404 while keeping
    // the other blocked on the server.
    let failing_1 = blocked_failing_fetch.next().await.unwrap();
    let still_blocked_2 = blocked_other_fetches.next().await.unwrap();
    failing_1.unblock();
    assert_eq!(many_fut.await.unwrap(), Err(fpkg::ResolveError::UnavailableBlob));

    // Dropping the remaining `WorkFuture`s when `Resolve` fails cancels `still_blocked_2` (plus at
    // most one more fetch if the queue driver filled the freed slot before `PackageFetcher` woke to
    // drop `futs`) and removes the remaining pending content blobs without starting them.
    let hierarchy = env
        .wait_for_and_return_inspect_state(tree_assertion!(
            root: contains {
                blob_fetcher: contains {
                    active: {},
                    queue: {
                        length: 0u64,
                    },
                }
            }
        ))
        .await;
    let recent = hierarchy.get_child_by_path(&["blob_fetcher", "recent"]).unwrap();
    let results: Vec<_> = recent
        .children
        .iter()
        .map(|c| (c.name.as_str(), c.get_property("result").and_then(|p| p.string()).unwrap()))
        .collect();
    assert!(
        matches!(results.len(), 3 | 4),
        "expected 3 or 4 recent fetches out of {} blobs, got {}: {results:?}",
        CONTENT_BLOB_COUNT + 1,
        results.len()
    );
    assert_eq!(results.iter().filter(|(_, r)| r.starts_with("success:")).count(), 1);
    assert_eq!(results.iter().filter(|(_, r)| r.starts_with("error:")).count(), 1);
    assert_eq!(results.iter().filter(|(_, r)| *r == "cancelled").count(), results.len() - 2);

    // Resolve another 2-blob package while `still_blocked_2` is held and wait for both of its
    // content blobs to arrive concurrently at the server, proving both concurrency slots are free.
    let (diff_dir, server_end) = fidl::endpoints::create_proxy();
    let diff_fut = env
        .proxies
        .ota_package_resolver
        .resolve("fuchsia-pkg://example.org/different-hash", server_end);
    let diff_unblock_1 = blocked_diff_fetches.next().await.unwrap();
    let diff_unblock_2 = blocked_diff_fetches.next().await.unwrap();
    diff_unblock_1.unblock();
    diff_unblock_2.unblock();
    let _: fpkg::ResolutionContext = diff_fut.await.unwrap().unwrap();
    let () = pkg_different.verify_contents(&diff_dir).await.unwrap();

    assert!(blocked_failing_fetch.next().now_or_never().is_none());
    let _: Result<_, _> = blocked_other_fetches.try_recv();
    assert!(blocked_other_fetches.try_recv().is_err(), "expected at most 1 extra HTTP request");
    drop(still_blocked_2);
}

#[test_case::test_case(blobfs_ramdisk::Implementation::CppBlobfs; "blobfs")]
#[test_case::test_case(blobfs_ramdisk::Implementation::Fxblob; "fxblob")]
#[fuchsia::test]
async fn failed_resolve_does_not_prevent_other_resolve_from_fetching_shared_blob(
    blobfs_impl: blobfs_ramdisk::Implementation,
) {
    let shared_contents = "shared-blob-contents".as_bytes();
    let pkg1 = fuchsia_pkg_testing::PackageBuilder::new("package1")
        .add_resource_at("shared-blob", shared_contents)
        .add_resource_at("failing-blob", "failing-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let pkg2 = fuchsia_pkg_testing::PackageBuilder::new("package2")
        .add_resource_at("shared-blob", shared_contents)
        .add_resource_at("pkg2-blob", "pkg2-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let shared_blob_merkle = pkg1.meta_contents().unwrap().contents()["shared-blob"];
    let failing_blob_merkle = pkg1.meta_contents().unwrap().contents()["failing-blob"];
    let pkg2_blob_merkle = pkg2.meta_contents().unwrap().contents()["pkg2-blob"];

    let (blocker, shared_unblocker_receiver) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodyOnce::new();
    let block_shared = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{shared_blob_merkle}"),
        blocker,
    );
    let (fail_blocker, mut blocked_failing_fetches) =
        fuchsia_pkg_testing::serve::responder::BlockResponseHeaders::new();
    let block_then_404_failing = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{failing_blob_merkle}"),
        fuchsia_pkg_testing::serve::responder::Chain::new(vec![
            Box::new(fail_blocker),
            Box::new(fuchsia_pkg_testing::serve::responder::StaticResponseCode::not_found()),
        ]),
    );
    let (notify_pkg2_blob, mut pkg2_blob_requested) =
        fuchsia_pkg_testing::serve::responder::NotifyWhenRequested::new();
    let notify_pkg2_blob = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{pkg2_blob_merkle}"),
        notify_pkg2_blob,
    );

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg1)
            .add_package(&pkg2)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(block_shared)
        .response_overrider(block_then_404_failing)
        .response_overrider(notify_pkg2_blob)
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg1, &pkg2],
        ))
        .blobfs_impl(blobfs_impl)
        .blob_fetch_concurrency_limit(3)
        .build()
        .await;

    let (_pkg1_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg1_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package1", server_end);

    // Wait for pkg1 to start fetching both shared-blob and failing-blob.
    let unblock_shared = shared_unblocker_receiver.await.unwrap();
    let failing_unblocker = blocked_failing_fetches.next().await.unwrap();

    // Start pkg2 and wait until it requests pkg2-blob; since `push_all` enqueues shared-blob and
    // pkg2-blob under a single lock acquisition, pkg2 has already attached to the in-flight
    // shared-blob fetch in `blob_fetcher`.
    let (pkg2_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg2_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package2", server_end);
    let () = pkg2_blob_requested.next().await.unwrap();

    // Unblock failing-blob (404), which fails pkg1 while leaving the shared in-flight fetch of
    // shared-blob running uninterrupted for pkg2.
    let () = failing_unblocker.unblock();
    assert_eq!(pkg1_fut.await.unwrap(), Err(fpkg::ResolveError::UnavailableBlob));

    // Unblocking the original shared-blob download completes pkg2 without a second HTTP request
    // (`BlockResponseBodyOnce` panics if requested more than once).
    let () = unblock_shared();
    let _: fpkg::ResolutionContext = pkg2_fut.await.unwrap().unwrap();
    let () = pkg2.verify_contents(&pkg2_dir).await.unwrap();
}

#[test_case::test_case(blobfs_ramdisk::Implementation::CppBlobfs; "blobfs")]
#[test_case::test_case(blobfs_ramdisk::Implementation::Fxblob; "fxblob")]
#[fuchsia::test]
async fn failed_resolve_cancels_in_flight_blob_and_unblocks_other_mirror_resolve(
    blobfs_impl: blobfs_ramdisk::Implementation,
) {
    let shared_contents = "shared-blob-contents".as_bytes();
    let pkg1 = fuchsia_pkg_testing::PackageBuilder::new("package1")
        .add_resource_at("shared-blob", shared_contents)
        .add_resource_at("failing-blob", "failing-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let pkg2 = fuchsia_pkg_testing::PackageBuilder::new("package2")
        .add_resource_at("shared-blob", shared_contents)
        .add_resource_at("pkg2-blob", "pkg2-blob-contents".as_bytes())
        .build()
        .await
        .unwrap();
    let shared_blob_merkle = pkg1.meta_contents().unwrap().contents()["shared-blob"];
    let failing_blob_merkle = pkg1.meta_contents().unwrap().contents()["failing-blob"];
    let pkg2_blob_merkle = pkg2.meta_contents().unwrap().contents()["pkg2-blob"];

    let (blocker1, mut blocked_shared_repo1) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodies::new();
    let block_shared_repo1 = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{shared_blob_merkle}"),
        blocker1,
    );
    let (fail_blocker, mut blocked_failing_fetches) =
        fuchsia_pkg_testing::serve::responder::BlockResponseHeaders::new();
    let block_then_404_failing = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{failing_blob_merkle}"),
        fuchsia_pkg_testing::serve::responder::Chain::new(vec![
            Box::new(fail_blocker),
            Box::new(fuchsia_pkg_testing::serve::responder::StaticResponseCode::not_found()),
        ]),
    );
    let (notify_pkg2_blob, mut pkg2_blob_requested) =
        fuchsia_pkg_testing::serve::responder::NotifyWhenRequested::new();
    let notify_pkg2_blob = fuchsia_pkg_testing::serve::responder::ForPath::new(
        format!("/blobs/1/{pkg2_blob_merkle}"),
        notify_pkg2_blob,
    );

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg1)
            .add_package(&pkg2)
            .build()
            .await
            .unwrap(),
    );
    let served_repo1 = Arc::clone(&repo)
        .server()
        .response_overrider(block_shared_repo1)
        .response_overrider(block_then_404_failing)
        .start()
        .unwrap();
    let served_repo2 =
        Arc::clone(&repo).server().response_overrider(notify_pkg2_blob).start().unwrap();
    let repo1_config = served_repo1.make_repo_config("fuchsia-pkg://example1.org".parse().unwrap());
    let repo2_config = served_repo2.make_repo_config("fuchsia-pkg://example2.org".parse().unwrap());
    let authority = crate::MockPkgAuthority::from_repo_config_and_packages(&repo1_config, &[&pkg1])
        .add_lookup_response(
            "fuchsia-pkg://example2.org/package2",
            Ok((*pkg2.hash(), format!("{}/1", repo2_config.mirrors()[0].blob_mirror_url()))),
        );
    let env = crate::TestEnv::builder()
        .pkg_authority(authority)
        .blobfs_impl(blobfs_impl)
        .blob_fetch_concurrency_limit(3)
        .build()
        .await;

    let (_pkg1_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg1_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example1.org/package1", server_end);

    // Wait for pkg1 to open shared-blob for write in blobfs and block on its response body from
    // repo1.
    let shared_unblocker_repo1 = blocked_shared_repo1.next().await.unwrap();
    let failing_unblocker = blocked_failing_fetches.next().await.unwrap();

    // Start pkg2 from repo2 (different mirror URL, so shared-blob cannot merge and queues behind
    // pkg1's in-flight download). Waiting for pkg2-blob to be requested guarantees `push_all` has
    // already queued shared-blob as well.
    let (pkg2_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg2_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example2.org/package2", server_end);
    let () = pkg2_blob_requested.next().await.unwrap();

    // Fail pkg1 while keeping `shared_unblocker_repo1` blocked on the server!
    // `pkg-cache` must cancel `http-client`'s in-flight `DownloadBlob` (dropping its `BlobWriter`
    // in blobfs) and then allow pkg2 to download and write shared-blob from repo2.
    let () = failing_unblocker.unblock();
    assert_eq!(pkg1_fut.await.unwrap(), Err(fpkg::ResolveError::UnavailableBlob));

    let _: fpkg::ResolutionContext = pkg2_fut.await.unwrap().unwrap();
    let () = pkg2.verify_contents(&pkg2_dir).await.unwrap();
    drop(shared_unblocker_repo1);
}

#[fuchsia::test]
async fn second_resolve_of_same_package_not_blocked_by_intervening_resolve() {
    let pkg0 = fuchsia_pkg_testing::PackageBuilder::new("package0")
        .add_resource_at("blob_0", &b"pkg0_blob_0"[..])
        .add_resource_at("blob_1", &b"pkg0_blob_1"[..])
        .build()
        .await
        .unwrap();
    let pkg1 = fuchsia_pkg_testing::PackageBuilder::new("package1").build().await.unwrap();

    let (blocker0, mut blocked_pkg0) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodies::new();
    let pkg0_content_paths =
        pkg0.content_blob_files().map(|b| format!("/blobs/1/{}", b.merkle).into()).collect();
    let (blocker1, mut blocked_pkg1) =
        fuchsia_pkg_testing::serve::responder::BlockResponseBodies::new();

    let repo = Arc::new(
        fuchsia_pkg_testing::RepositoryBuilder::from_template_dir(crate::EMPTY_REPO_PATH)
            .add_package(&pkg0)
            .add_package(&pkg1)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo)
        .server()
        .response_overrider(fuchsia_pkg_testing::serve::responder::ForPaths::new(
            pkg0_content_paths,
            blocker0,
        ))
        .response_overrider(fuchsia_pkg_testing::serve::responder::ForPath::new(
            format!("/blobs/1/{}", pkg1.hash()),
            blocker1,
        ))
        .start()
        .unwrap();
    let repo_config =
        served_repository.make_repo_config("fuchsia-pkg://example.org".parse().unwrap());
    let env = crate::TestEnv::builder()
        .pkg_authority(crate::MockPkgAuthority::from_repo_config_and_packages(
            &repo_config,
            &[&pkg0, &pkg1],
        ))
        .blob_fetch_concurrency_limit(1)
        .build()
        .await;

    // 1. Request resolve of package0: its meta.far downloads, and one of its two content blobs
    // occupies the single concurrency slot while the other waits at package0's priority.
    let (pkg0_dir_1, server_end) = fidl::endpoints::create_proxy();
    let pkg0_fut_1 =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package0", server_end);
    let unblock_pkg0_a = blocked_pkg0.next().await.unwrap();

    // 2. Request resolve of package1 and wait for its meta.far fetch to queue behind package0's
    // content blobs (`length == 3`: two content blobs for package0 + one meta.far for package1).
    let (pkg1_dir, server_end) = fidl::endpoints::create_proxy();
    let pkg1_fut =
        env.proxies.ota_package_resolver.resolve("fuchsia-pkg://example.org/package1", server_end);
    let _ = env
        .wait_for_and_return_inspect_state(tree_assertion!(
            "root": contains {
                "blob_fetcher": contains {
                    "queue": {
                        "length": 3u64,
                    }
                }
            }
        ))
        .await;

    // 3. Request a second resolve of package0 at lower priority than package1 (via the full
    // resolver, also exercising coalescing across different GC protections) while package0's
    // content blobs are still in flight and queued.
    let (pkg0_dir_2, server_end) = fidl::endpoints::create_proxy();
    let pkg0_fut_2 =
        env.proxies.full_package_resolver.resolve("fuchsia-pkg://example.org/package0", server_end);

    // Wait for the second resolve of package0 to record `known_outstanding_blobs: 2` and then
    // evaluate the lazy `blob_fetcher.queue` node on `pkg-cache`'s single-threaded executor. Since
    // there is no `.await` between updating `known_outstanding_blobs` and `blob_fetcher.push_all`,
    // `pkg-cache` must have yielded after `push_all` (coalescing onto package0's active and queued
    // content blob fetches) before serving `queue`.
    let _ = env
        .wait_for_and_return_inspect_state(tree_assertion!(
            "root": contains {
                "blob_fetcher": contains {
                    "queue": contains {},
                },
                "package_fetcher": contains {
                    "2": contains {
                        "package_hash": pkg0.hash().to_string(),
                        "known_outstanding_blobs": 2u64,
                    },
                },
            }
        ))
        .await;

    // 4. Unblock package0's first content blob: package0's second content blob (higher priority
    // than package1) claims the single slot next. Unblock package0's second content blob so
    // package1 immediately claims and blocks the sole concurrency slot: both resolves of package0
    // complete without waiting for package1!
    let () = unblock_pkg0_a.unblock();
    let unblock_pkg0_b = blocked_pkg0.next().await.unwrap();
    let () = unblock_pkg0_b.unblock();
    let unblock_pkg1 = blocked_pkg1.next().await.unwrap();
    let _: fpkg::ResolutionContext = pkg0_fut_1.await.unwrap().unwrap();
    let _: fpkg::ResolutionContext = pkg0_fut_2.await.unwrap().unwrap();
    let () = pkg0.verify_contents(&pkg0_dir_1).await.unwrap();
    let () = pkg0.verify_contents(&pkg0_dir_2).await.unwrap();

    // 5. Unblock package1 and verify it completes as well, with each blob only fetched once.
    let () = unblock_pkg1.unblock();
    let _: fpkg::ResolutionContext = pkg1_fut.await.unwrap().unwrap();
    let () = pkg1.verify_contents(&pkg1_dir).await.unwrap();
    assert_eq!(served_repository.history().lock().len(), 4);
}
