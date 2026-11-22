// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::resolve::get_binary_and_loader_from_pkg_dir;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fidl_fuchsia_process::{ResolverRequest, ResolverRequestStream};
use fuchsia_component::client::connect_to_protocol_at_path;
use fuchsia_url::fuchsia_pkg::AbsoluteComponentUrl;
use futures::prelude::*;
use log::warn;

pub async fn serve(stream: ResolverRequestStream) {
    let boot_resolver = connect_to_protocol_at_path::<fpkg::PackageResolverMarker>(
        "/svc/fuchsia.pkg.PackageResolver-boot",
    )
    .expect("Could not connect to fuchsia.pkg.PackageResolver-boot");

    serve_stream(stream, boot_resolver, || {
        match connect_to_protocol_at_path::<fpkg::PackageResolverMarker>(
            "/svc/fuchsia.pkg.PackageResolver-pkg",
        ) {
            Ok(proxy) => Some(proxy),
            Err(err) => {
                warn!("Could not connect to fuchsia.pkg.PackageResolver-pkg: {err:#}");
                None
            }
        }
    })
    .await;
}

async fn serve_stream(
    mut stream: ResolverRequestStream,
    boot_resolver: fpkg::PackageResolverProxy,
    mut connect_pkg_resolver: impl FnMut() -> Option<fpkg::PackageResolverProxy>,
) {
    let mut pkg_resolver: Option<Option<fpkg::PackageResolverProxy>> = None;

    while let Some(Ok(request)) = stream.next().await {
        match request {
            ResolverRequest::Resolve { name, responder } => {
                let get_pkg_resolver =
                    || pkg_resolver.get_or_insert_with(&mut connect_pkg_resolver).clone();
                match resolve(&boot_resolver, get_pkg_resolver, &name).await {
                    Ok((vmo, ldsvc)) => {
                        let _ = responder.send(zx::sys::ZX_OK, Some(vmo), ldsvc);
                    }
                    Err(status) => {
                        let _ = responder.send(status.into_raw(), None, None);
                    }
                }
            }
        }
    }
}

async fn resolve(
    boot_resolver: &fpkg::PackageResolverProxy,
    connect_pkg_resolver: impl FnOnce() -> Option<fpkg::PackageResolverProxy>,
    url: &str,
) -> Result<
    (fidl::Vmo, Option<fidl::endpoints::ClientEnd<fidl_fuchsia_ldsvc::LoaderMarker>>),
    zx::Status,
> {
    // `fdio_spawn` passes the rest of a `#!resolve` line verbatim, so ignore surrounding ASCII
    // whitespace (such as the `\r` of a CRLF line ending), which URL parsing rejects.
    let url = url.trim_ascii();

    // Parse the URL
    let boot_url = fuchsia_url::boot::AbsoluteComponentUrl::parse(url);
    let pkg_url = AbsoluteComponentUrl::parse(url);
    let owned_pkg_resolver;
    let (resolver, pkg_url, bin_path) = match (boot_url, pkg_url) {
        (Ok(url), _) => {
            // Break it into a package URL and binary path
            let pkg_url = url.to_package_url().to_string();
            let bin_path = url.resource().to_string();
            (boot_resolver, pkg_url, bin_path)
        }
        (_, Ok(url)) => {
            // Break it into a package URL and binary path
            let pkg_url = url.package_url().to_string();
            let bin_path = url.resource().to_string();
            owned_pkg_resolver = connect_pkg_resolver().ok_or(zx::Status::NOT_FOUND)?;
            (&owned_pkg_resolver, pkg_url, bin_path)
        }
        (Err(boot_err), Err(pkg_err)) => {
            warn!(
                "Could not parse {url:?} as a fuchsia-boot ({boot_err:#}) or fuchsia-pkg \
                 ({pkg_err:#}) component URL"
            );
            // There is a CTS test that attempts to resolve `fuchsia-pkg://example.com/mustnotexist`
            // and expects to see INTERNAL, instead of the more suitable INVALID_ARGS.
            return Err(zx::Status::INTERNAL);
        }
    };

    // Resolve the package URL
    let (pkg_dir, server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    let _subpackage_context = resolver
        .resolve(&pkg_url, server)
        .await
        .map_err(|err| match err {
            fidl::Error::ClientChannelClosed { epitaph, .. }
                if epitaph == zx::Status::NOT_FOUND =>
            {
                zx::Status::NOT_FOUND
            }
            err => {
                warn!("Could not resolve {pkg_url}: {err:#}");
                zx::Status::INTERNAL
            }
        })?
        .map_err(|e| {
            if e == fpkg::ResolveError::PackageNotFound {
                zx::Status::NOT_FOUND
            } else {
                warn!("Could not resolve {}: {:?}", pkg_url, e);
                zx::Status::INTERNAL
            }
        })?;

    get_binary_and_loader_from_pkg_dir(&pkg_dir, &bin_path, &pkg_url).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_process::ResolverMarker;
    use fuchsia_async as fasync;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn uncalled_pkg_resolver(scope: &fasync::Scope) -> fpkg::PackageResolverProxy {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        scope.spawn(async move {
            let mut stream = server_end.into_stream();
            if let Some(request) = stream.next().await {
                panic!("Unexpected request to uncalled PackageResolver: {request:?}");
            }
        });
        proxy
    }

    fn serve_pkg_resolver_success(scope: &fasync::Scope) -> fpkg::PackageResolverProxy {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        scope.spawn(async move {
            let mut stream = server_end.into_stream();
            let (package_url, dir, responder) =
                if let fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } =
                    stream.next().await.unwrap().unwrap()
                {
                    (package_url, dir, responder)
                } else {
                    panic!("Unexpected call to PackageResolver");
                };
            assert_eq!(package_url, "fuchsia-pkg://fuchsia.com/foo");

            // Return the test's own pkg directory. This is guaranteed to support
            // the readable + executable rights needed by this test.
            fuchsia_fs::directory::open_channel_in_namespace(
                "/pkg",
                fio::PERM_READABLE | fio::PERM_EXECUTABLE,
                dir,
            )
            .unwrap();

            responder.send(Ok(&fpkg::ResolutionContext { bytes: vec![] })).unwrap();
        });
        proxy
    }

    fn serve_boot_pkg_resolver_success(scope: &fasync::Scope) -> fpkg::PackageResolverProxy {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        scope.spawn(async move {
            let mut stream = server_end.into_stream();
            let (package_url, dir, responder) =
                if let fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } =
                    stream.next().await.unwrap().unwrap()
                {
                    (package_url, dir, responder)
                } else {
                    panic!("Unexpected call to PackageResolver");
                };
            assert_eq!(package_url, "fuchsia-boot:///foo");

            // Return the test's own pkg directory. This is guaranteed to support
            // the readable + executable rights needed by this test.
            fuchsia_fs::directory::open_channel_in_namespace(
                "/pkg",
                fio::PERM_READABLE | fio::PERM_EXECUTABLE,
                dir,
            )
            .unwrap();

            responder.send(Ok(&fpkg::ResolutionContext { bytes: vec![] })).unwrap();
        });
        proxy
    }

    fn serve_pkg_resolver_fail(
        scope: &fasync::Scope,
        error: fpkg::ResolveError,
    ) -> fpkg::PackageResolverProxy {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        scope.spawn(async move {
            let mut stream = server_end.into_stream();
            let (package_url, responder) =
                if let fpkg::PackageResolverRequest::Resolve { package_url, responder, .. } =
                    stream.next().await.unwrap().unwrap()
                {
                    (package_url, responder)
                } else {
                    panic!("Unexpected call to PackageResolver");
                };
            assert_eq!(package_url, "fuchsia-pkg://fuchsia.com/foo");
            responder.send(Err(error)).unwrap();
        });
        proxy
    }

    fn serve_multi_request_pkg_resolver(
        scope: &fasync::Scope,
        expected_url: &'static str,
    ) -> fpkg::PackageResolverProxy {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        scope.spawn(async move {
            let mut stream = server_end.into_stream();
            while let Some(Ok(request)) = stream.next().await {
                if let fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } =
                    request
                {
                    assert_eq!(package_url, expected_url);
                    fuchsia_fs::directory::open_channel_in_namespace(
                        "/pkg",
                        fio::PERM_READABLE | fio::PERM_EXECUTABLE,
                        dir,
                    )
                    .unwrap();
                    responder.send(Ok(&fpkg::ResolutionContext { bytes: vec![] })).unwrap();
                } else {
                    panic!("Unexpected call to PackageResolver");
                }
            }
        });
        proxy
    }

    #[fuchsia::test]
    async fn success_resolve() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let pkg_resolver = serve_pkg_resolver_success(&scope);
        let (vmo, _) = resolve(
            &boot_pkg_resolver,
            || Some(pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/process_resolver_bin_test",
        )
        .await
        .unwrap();

        // We don't know much about this test binary. Make a basic assertion.
        assert!(vmo.get_content_size().unwrap() > 0);

        // We can't make any assumptions about the libraries, especially in variations like ASAN.
    }

    #[fuchsia::test]
    async fn success_resolve_ignores_surrounding_ascii_whitespace() {
        // `fdio_spawn` passes the rest of a `#!resolve` line verbatim, which may end with the `\r`
        // of a CRLF line ending.
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let pkg_resolver = serve_pkg_resolver_success(&scope);
        let (vmo, _) = resolve(
            &boot_pkg_resolver,
            || Some(pkg_resolver),
            " fuchsia-pkg://fuchsia.com/foo#bin/process_resolver_bin_test \r",
        )
        .await
        .unwrap();

        // We don't know much about this test binary. Make a basic assertion.
        assert!(vmo.get_content_size().unwrap() > 0);
    }

    #[fuchsia::test]
    async fn success_bootfs_resolve() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = serve_boot_pkg_resolver_success(&scope);
        let (vmo, _) = resolve(
            &boot_pkg_resolver,
            || panic!("pkg_resolver should not be connected"),
            "fuchsia-boot:///foo#bin/process_resolver_bin_test",
        )
        .await
        .unwrap();

        // We don't know much about this test binary. Make a basic assertion.
        assert!(vmo.get_content_size().unwrap() > 0);

        // We can't make any assumptions about the libraries, especially in variations like ASAN.
    }

    #[fuchsia::test]
    async fn bootfs_resolve_succeeds_without_pkg_resolver() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = serve_multi_request_pkg_resolver(&scope, "fuchsia-boot:///foo");
        let (vmo, _) = resolve(
            &boot_pkg_resolver,
            || panic!("pkg_resolver should not be connected"),
            "fuchsia-boot:///foo#bin/process_resolver_bin_test",
        )
        .await
        .unwrap();
        assert!(vmo.get_content_size().unwrap() > 0);

        let (vmo, _) = resolve(
            &boot_pkg_resolver,
            || None,
            "fuchsia-boot:///foo#bin/process_resolver_bin_test",
        )
        .await
        .unwrap();
        assert!(vmo.get_content_size().unwrap() > 0);
    }

    #[fuchsia::test]
    async fn pkg_resolver_unavailable() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);

        // `pkg_resolver` is `None` (failed to connect).
        let status = resolve(&boot_pkg_resolver, || None, "fuchsia-pkg://fuchsia.com/foo#bin/bar")
            .await
            .unwrap_err();
        assert_eq!(status, zx::Status::NOT_FOUND);

        // `pkg_resolver` closed with `ZX_ERR_NOT_FOUND` epitaph (routed `from: "void"`).
        let (void_pkg_resolver, server_end) =
            fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        server_end.close_with_epitaph(zx::Status::NOT_FOUND).unwrap();
        let status = resolve(
            &boot_pkg_resolver,
            || Some(void_pkg_resolver.clone()),
            "fuchsia-pkg://fuchsia.com/foo#bin/bar",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::NOT_FOUND);

        // Subsequent calls using the same cached proxy also report `NOT_FOUND`.
        let status = resolve(
            &boot_pkg_resolver,
            || Some(void_pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/bar",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::NOT_FOUND);

        // Channel closed without `NOT_FOUND` epitaph still maps to `INTERNAL`.
        let (peer_closed_pkg_resolver, server_end) =
            fidl::endpoints::create_proxy::<fpkg::PackageResolverMarker>();
        drop(server_end);
        let status = resolve(
            &boot_pkg_resolver,
            || Some(peer_closed_pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/bar",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::INTERNAL);
    }

    #[fuchsia::test]
    async fn serve_stream_lazily_connects_and_caches_pkg_resolver() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = serve_multi_request_pkg_resolver(&scope, "fuchsia-boot:///foo");
        let pkg_resolver =
            serve_multi_request_pkg_resolver(&scope, "fuchsia-pkg://fuchsia.com/foo");
        let connect_count = Arc::new(AtomicUsize::new(0));

        let (resolver_proxy, stream) = fidl::endpoints::create_proxy_and_stream::<ResolverMarker>();
        let connect_count_clone = Arc::clone(&connect_count);
        scope.spawn(async move {
            serve_stream(stream, boot_pkg_resolver, move || {
                connect_count_clone.fetch_add(1, Ordering::SeqCst);
                Some(pkg_resolver.clone())
            })
            .await;
        });

        // Resolving a bootfs URL must not connect to `PackageResolver-pkg`.
        let (status, vmo, _) = resolver_proxy
            .resolve("fuchsia-boot:///foo#bin/process_resolver_bin_test")
            .await
            .unwrap();
        assert_eq!(status, zx::sys::ZX_OK);
        assert!(vmo.unwrap().get_content_size().unwrap() > 0);
        assert_eq!(connect_count.load(Ordering::SeqCst), 0);

        // Resolving `fuchsia-pkg://` connects once and caches the proxy across requests.
        let (status, vmo, _) = resolver_proxy
            .resolve("fuchsia-pkg://fuchsia.com/foo#bin/process_resolver_bin_test")
            .await
            .unwrap();
        assert_eq!(status, zx::sys::ZX_OK);
        assert!(vmo.unwrap().get_content_size().unwrap() > 0);
        assert_eq!(connect_count.load(Ordering::SeqCst), 1);

        let (status, vmo, _) = resolver_proxy
            .resolve("fuchsia-pkg://fuchsia.com/foo#bin/process_resolver_bin_test")
            .await
            .unwrap();
        assert_eq!(status, zx::sys::ZX_OK);
        assert!(vmo.unwrap().get_content_size().unwrap() > 0);
        assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    }

    #[fuchsia::test]
    async fn malformed_url() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let status = resolve(
            &boot_pkg_resolver,
            || panic!("pkg_resolver should not be connected"),
            "fuchsia-pkg://fuchsia.com/foo",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::INTERNAL);
        let status =
            resolve(&boot_pkg_resolver, || panic!("pkg_resolver should not be connected"), "abcd")
                .await
                .unwrap_err();
        assert_eq!(status, zx::Status::INTERNAL);
    }

    #[fuchsia::test]
    async fn package_resolver_not_found() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let pkg_resolver = serve_pkg_resolver_fail(&scope, fpkg::ResolveError::PackageNotFound);
        let status = resolve(
            &boot_pkg_resolver,
            || Some(pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/bar",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::NOT_FOUND);
    }

    #[fuchsia::test]
    async fn package_resolver_internal() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let pkg_resolver = serve_pkg_resolver_fail(&scope, fpkg::ResolveError::NoSpace);
        let status = resolve(
            &boot_pkg_resolver,
            || Some(pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/bar",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::INTERNAL);
    }

    #[fuchsia::test]
    async fn binary_not_found() {
        let scope = fasync::Scope::new();
        let boot_pkg_resolver = uncalled_pkg_resolver(&scope);
        let pkg_resolver = serve_pkg_resolver_success(&scope);
        let status = resolve(
            &boot_pkg_resolver,
            || Some(pkg_resolver),
            "fuchsia-pkg://fuchsia.com/foo#bin/does_not_exist",
        )
        .await
        .unwrap_err();
        assert_eq!(status, zx::Status::NOT_FOUND);
    }
}
