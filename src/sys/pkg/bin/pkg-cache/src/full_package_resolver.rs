// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::index::PackageIndex;
use anyhow::{Context as _, anyhow};
use fidl::endpoints::ServerEnd;
use fidl_connector::Connect as _;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_pkg as fpkg;
use fidl_fuchsia_pkg_ext as fpkg_ext;
use fidl_fuchsia_pkg_resolution as fpkg_resolution;
use fuchsia_inspect as finspect;
use fuchsia_sync::Mutex;
use fuchsia_trace as ftrace;
use fuchsia_url::fuchsia_pkg::{AbsolutePackageUrl, PackageUrl};
use futures::stream::TryStreamExt as _;
use log::{error, warn};
use std::borrow::Cow;
use std::sync::Arc;

const SLOW_CACHE_FALLBACK_WARN_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(10);
const SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION: zx::MonotonicDuration =
    zx::MonotonicDuration::from_minutes(10);
const INSPECT_RECENT_RESOLVE_COUNT: usize = 50;

/// Used to resolve most non-OTA packages on products that support ephemeral resolution.
/// * always uses open package tracking
/// * has a hard-coded priority of authorities (url to hash mappings):
///   1. base index
///   2. upgradable packages
///   3. eager packages
///   4. the remote TUF repositories managed by pkg-resolver
///   5. the cache index in case of certain TUF errors
pub(crate) struct Resolver {
    base_resolver: Arc<crate::base_package_resolver::Resolver>,
    package_index: Arc<PackageIndex>,
    tuf_authority: fidl_connector::ServiceReconnector<fpkg::AuthorityMarker>,
    cache_index: Arc<crate::CacheIndex>,
    package_fetcher: crate::package_fetcher::PackageFetcher,
    authenticator: context_authenticator::ContextAuthenticator,
    open_packages: crate::RootDirCache,
    executability_decider: crate::executability::Decider,
    scope: package_directory::ExecutionScope,

    // Only used for inspect.
    inspect_active: finspect::Node,
    inspect_recent: Mutex<fuchsia_inspect_contrib::nodes::BoundedListNode>,
    request_count: std::sync::atomic::AtomicU64,
    _inspect: finspect::Node,
}

impl Resolver {
    pub(crate) fn new(
        base_resolver: Arc<crate::base_package_resolver::Resolver>,
        package_index: Arc<PackageIndex>,
        tuf_authority: fidl_connector::ServiceReconnector<fpkg::AuthorityMarker>,
        cache_index: Arc<crate::CacheIndex>,
        package_fetcher: crate::package_fetcher::PackageFetcher,
        authenticator: context_authenticator::ContextAuthenticator,
        open_packages: crate::RootDirCache,
        executability_decider: crate::executability::Decider,
        scope: package_directory::ExecutionScope,
        inspect: finspect::Node,
    ) -> Arc<Self> {
        Arc::new(Self {
            base_resolver,
            package_index,
            tuf_authority,
            cache_index,
            package_fetcher,
            authenticator,
            open_packages,
            executability_decider,
            scope,
            inspect_active: inspect.create_child("active"),
            inspect_recent: Mutex::new(fuchsia_inspect_contrib::nodes::BoundedListNode::new(
                inspect.create_child("recent"),
                INSPECT_RECENT_RESOLVE_COUNT,
            )),
            request_count: std::sync::atomic::AtomicU64::new(0),
            _inspect: inspect,
        })
    }

    pub(crate) async fn serve_request_stream(
        self: Arc<Self>,
        stream: fpkg::PackageResolverRequestStream,
    ) -> anyhow::Result<()> {
        stream
            .map_err(anyhow::Error::new)
            .try_for_each_concurrent(None, |req| async {
                match req {
                    fpkg::PackageResolverRequest::Resolve { package_url, dir, responder } => {
                        self.handle_resolve_request(package_url, dir, responder).await
                    }
                    fpkg::PackageResolverRequest::ResolveWithContext {
                        package_url,
                        context,
                        dir,
                        responder,
                    } => {
                        self.handle_resolve_with_context_request(
                            package_url,
                            context,
                            dir,
                            responder,
                        )
                        .await
                    }
                    fpkg::PackageResolverRequest::GetHash { package_url, responder } => {
                        self.handle_get_hash_request(package_url, responder).await
                    }
                }
            })
            .await
    }

    async fn handle_resolve_request(
        &self,
        package_url: String,
        dir: ServerEnd<fio::DirectoryMarker>,
        responder: fpkg::PackageResolverResolveResponder,
    ) -> Result<(), anyhow::Error> {
        match self.resolve_unparsed_and_serve(&package_url, dir).await {
            Ok(context) => responder.send(Ok(&context)),
            Err(e) => {
                let fidl_error = (&e).into();
                error!("full resolver failed to resolve {package_url}: {:#}", anyhow!(e));
                responder.send(Err(fidl_error))
            }
        }
        .context("sending fuchsia.pkg/PackageResolver-full.Resolve response")
    }

    async fn handle_resolve_with_context_request(
        &self,
        package_url: String,
        context: fpkg::ResolutionContext,
        dir: ServerEnd<fio::DirectoryMarker>,
        responder: fpkg::PackageResolverResolveWithContextResponder,
    ) -> Result<(), anyhow::Error> {
        match self.resolve_with_context_unparsed_and_serve(&package_url, context, dir).await {
            Ok(context) => responder.send(Ok(&context)),
            Err(e) => {
                let fidl_error = (&e).into();
                error!(
                    "full resolver failed to resolve with context {package_url}: {:#}",
                    anyhow!(e)
                );
                responder.send(Err(fidl_error))
            }
        }
        .context("sending fuchsia.pkg/PackageResolver-full.ResolveWithContext response")
    }

    async fn handle_get_hash_request(
        &self,
        package_url: fpkg::PackageUrl,
        responder: fpkg::PackageResolverGetHashResponder,
    ) -> Result<(), anyhow::Error> {
        match self.lookup_unparsed(&package_url.url).await {
            Ok(hash) => responder.send(Ok(&fpkg::BlobId { merkle_root: hash.into() })),
            Err(e) => {
                let status = zx::Status::from(&e);
                error!("full resolver failed to get hash {}: {:#}", package_url.url, anyhow!(e));
                responder.send(Err(status.into_raw()))
            }
        }
        .context("sending fuchsia.pkg/PackageResolver-full.GetHash response")
    }

    async fn resolve_with_context_unparsed_and_serve(
        &self,
        package_url: &str,
        context: fpkg::ResolutionContext,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<fpkg::ResolutionContext, Error> {
        let (context, _) = self
            .resolve_with_context_and_serve(
                &PackageUrl::parse(package_url).map_err(Error::InvalidUrl)?,
                context,
                dir,
            )
            .await?;
        Ok(context)
    }

    async fn resolve_with_context_and_serve(
        &self,
        url: &PackageUrl,
        context: fpkg::ResolutionContext,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<(fpkg::ResolutionContext, Arc<crate::root_dir::CachedRootDir>), Error> {
        let root_dir = match url {
            PackageUrl::Absolute(url) => {
                if !context.bytes.is_empty() {
                    return Err(Error::ContextWithAbsoluteUrl);
                }
                self.resolve_manage_inspect(url).await
            }
            PackageUrl::Relative(url) => self.resolve_subpackage(url, context).await,
        }?;
        let hash = *root_dir.hash();
        let flags = self.executability_decider.decide(hash).into();
        vfs::directory::serve_on(Arc::clone(&root_dir), flags, self.scope.clone(), dir);
        Ok((self.authenticator.clone().create(&hash), root_dir))
    }

    async fn resolve_unparsed_and_serve(
        &self,
        url: &str,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<fpkg::ResolutionContext, Error> {
        let (context, _) =
            self.resolve_and_serve(&url.parse().map_err(Error::InvalidUrl)?, dir).await?;
        Ok(context)
    }

    async fn resolve_and_serve(
        &self,
        url: &AbsolutePackageUrl,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<(fpkg::ResolutionContext, Arc<crate::root_dir::CachedRootDir>), Error> {
        let root_dir = self.resolve_manage_inspect(url).await?;
        let hash = *root_dir.hash();
        let flags = self.executability_decider.decide(hash).into();
        vfs::directory::serve_on(Arc::clone(&root_dir), flags, self.scope.clone(), dir);
        Ok((self.authenticator.clone().create(&hash), root_dir))
    }

    async fn resolve_manage_inspect(
        &self,
        url: &AbsolutePackageUrl,
    ) -> Result<Arc<crate::root_dir::CachedRootDir>, Error> {
        let inspect = self.inspect_active.create_child(
            self.request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string(),
        );
        let trace_id = ftrace::Id::new();
        let guard = ftrace::async_enter!(
            trace_id,
            c"app",
            c"full resolve",
            "url" => url.to_string().as_ref(),
            // An async duration cannot have multiple concurrent child async durations so we include
            // the id as metadata to manually determine the relationship.
            "trace_id" => u64::from(trace_id)
        );
        let res = self.resolve(url, &inspect, trace_id).await;
        let result_str = res.as_ref().err().map_or(Cow::Borrowed("success"), |e| {
            format!("error: {}", crate::stringify_error(e)).into()
        });
        if let Some(o) = guard {
            o.end(&[ftrace::ArgValue::of("result", result_str.as_ref())]);
        }
        inspect.record_string("result", result_str);
        inspect.record_int("end_boot_ns", zx::BootInstant::get().into_nanos());
        self.move_inspect_node_to_recent(inspect);
        res
    }

    async fn resolve(
        &self,
        url: &AbsolutePackageUrl,
        inspect: &finspect::Node,
        trace_id: ftrace::Id,
    ) -> Result<Arc<crate::root_dir::CachedRootDir>, Error> {
        inspect.record_int("start_boot_ns", zx::BootInstant::get().into_nanos());
        inspect.record_string("url", url.to_string());
        let (pkg_id, source) = {
            let _guard = ftrace::async_enter!(trace_id, c"app", c"lookup");
            self.lookup(url, inspect).await?
        };
        inspect.record_string("hash", pkg_id.to_string());
        inspect.record_string("blob_source", format!("{:?}", source.blob_source()));
        if let Some(root_dir) = self.open_packages.get(&pkg_id) {
            return Ok(root_dir);
        }
        match source {
            LookupSource::Remote(blob_source) => {
                ftrace::async_instant!(trace_id, c"app", c"enqueue fetch");
                self.package_fetcher
                    .fetch_with_open_package_tracking(pkg_id, blob_source, trace_id)
                    .await
                    .map_err(Error::PackageFetcher)
            }
            LookupSource::Local(cleanup_guard) => {
                let res = self
                    .open_packages
                    .get_or_insert(pkg_id, None)
                    .await
                    .map_err(|source| Error::CreatingRootDir { source, pkg_id });
                // Explicitly drop to guarantee it lives across open package insertion.
                drop(cleanup_guard);
                res
            }
        }
    }

    async fn lookup_unparsed(&self, url: &str) -> Result<fuchsia_hash::Hash, Error> {
        // lookup_unparsed is only used by GetHash which is only used for debugging, we don't want
        // that complicating the inspect.
        let (pkg_id, _source) = self
            .lookup(&url.parse().map_err(Error::InvalidUrl)?, &finspect::Node::default())
            .await?;
        Ok(pkg_id)
    }

    async fn lookup(
        &self,
        url: &AbsolutePackageUrl,
        inspect: &finspect::Node,
    ) -> Result<(fuchsia_hash::Hash, LookupSource), Error> {
        // Use monotonic timeline to warn on slow cache fallback to avoid warning on suspension.
        let start_mono = zx::MonotonicInstant::get();
        let () = match self.base_resolver.lookup(url) {
            Ok(pkg_id) => {
                inspect.record_string("authority", "base");
                return Ok((pkg_id, LookupSource::Local(None)));
            }
            Err(crate::base_package_resolver::Error::PackageNotInIndex) => (),
            Err(e) => return Err(Error::BaseResolver(e)),
        };

        // `CleanupGuard` keeps the resolved package protected from GC even if `SetUpgradableUrls`
        // replaces its URL before `resolve` inserts it into `open_packages`.
        if let Some((hash, cleanup_guard)) =
            self.package_index.open_upgradable(url.as_unpinned()).await
        {
            if url.hash().is_some() {
                return Err(Error::PinnedUpgradablePackage);
            }
            inspect.record_string("authority", "upgradable");
            return Ok((hash, LookupSource::Local(Some(cleanup_guard))));
        }

        // TODO(https://fxbug.dev/542690903): Add eager package support.

        let (tuf_err, deprecated_fallback) = match self
            .tuf_authority
            .connect()
            .map_err(Error::ConnectToAuthority)?
            .lookup(&fpkg::PackageUrl { url: url.as_unpinned().to_string() })
            .await
            .map_err(Error::AuthorityFidl)?
        {
            Ok((fpkg::BlobId { merkle_root }, http_blob_dir)) => {
                // TODO(https://fxbug.dev/519687989): Forbid pinned URL authority override.
                let pkg_id = url.hash().unwrap_or_else(|| merkle_root.into());
                inspect.record_string("authority", "tuf");
                return Ok((
                    pkg_id,
                    LookupSource::Remote(http_blob_dir.parse().map_err(Error::InvalidBlobDirUri)?),
                ));
            }
            // TODO(https://fxbug.dev/42127880): Remove package not found cache fallback.
            Err(e @ fpkg::AuthorityLookupError::PackageNotFound) => (e, true),
            Err(e @ fpkg::AuthorityLookupError::RepositoryNotFound) => (e, false),
            Err(e @ fpkg::AuthorityLookupError::UpstreamConnection) => (e, false),
            Err(e) => return Err(Error::Authority(e)),
        };

        let Some(pkg_id) = lookup_cache_fallback(url, self.cache_index.as_ref()) else {
            return Err(Error::Authority(tuf_err));
        };
        if deprecated_fallback {
            log::warn!(
                "Did not find {url} in a TUF repo, but did find a matching package name in the \
                built-in cache packages set, so falling back to it. Your package repository may \
                not be configured to serve the package correctly, or may be overriding the domain \
                for the repository which would normally serve this package. This will be an error \
                in a future version of Fuchsia, see https://fxbug.dev/42127862."
            );
        }
        let () = log_slow_cache_fallback(start_mono, url);
        inspect.record_string("authority", "cache");
        Ok((pkg_id, LookupSource::Local(None)))
    }

    async fn resolve_subpackage(
        &self,
        url: &fuchsia_url::RelativePackageUrl,
        context: fpkg::ResolutionContext,
    ) -> Result<Arc<crate::root_dir::CachedRootDir>, Error> {
        let super_hash = self
            .authenticator
            .clone()
            .authenticate(context)
            .map_err(Error::ContextAuthenticator)?;
        let super_package = self.open_packages.get(&super_hash).ok_or_else(|| {
            Error::SuperpackageNotOpen { superpackage: super_hash, subpackage: url.clone() }
        })?;
        let sub_hash = *super_package
            .subpackages()
            .await
            .map_err(Error::ReadingSubpackages)?
            .subpackages()
            .get(url)
            .ok_or_else(|| Error::SubpackageNotFound {
                subpackage: url.clone(),
                superpackage: super_hash,
            })?;
        self.open_packages
            .get_or_insert(sub_hash, None)
            .await
            .map_err(|source| Error::CreatingSubpackageRootDir { source, subpackage: sub_hash })
    }

    fn move_inspect_node_to_recent(&self, node: finspect::Node) {
        let () = self
            .inspect_recent
            .lock()
            .adopt_entry(node)
            .map(|_: &finspect::Node| ())
            .unwrap_or_else(|e| warn!("failed to move inspect node to recent: {:#}", anyhow!(e)));
    }

    pub(crate) async fn serve_toolbox_resolver_request_stream(
        self: Arc<Self>,
        stream: fpkg_resolution::PackageResolverRequestStream,
    ) -> anyhow::Result<()> {
        stream
            .map_err(anyhow::Error::new)
            .try_for_each_concurrent(None, |req| async {
                match req {
                    fpkg_resolution::PackageResolverRequest::Resolve { payload, responder } => {
                        self.handle_toolbox_resolve_request(payload, responder).await
                    }
                    fpkg_resolution::PackageResolverRequest::_UnknownMethod {
                        method_type,
                        ordinal,
                        ..
                    } => {
                        warn!(method_type:?, ordinal; "Unknown PackageResolverRequest");
                        Ok(())
                    }
                }
            })
            .await
    }

    async fn handle_toolbox_resolve_request(
        &self,
        payload: fpkg_resolution::PackageResolverResolveRequest,
        responder: fpkg_resolution::PackageResolverResolveResponder,
    ) -> Result<(), anyhow::Error> {
        match self.resolve_toolbox(&payload).await {
            Ok(()) => responder.send(Ok(fpkg_resolution::ResolveResult { ..Default::default() })),
            Err(e) => {
                let fidl_error = internal_to_toolbox_error((&e).into());
                error!("toolbox resolver failed to resolve {payload:?}: {:#}", anyhow!(e));
                responder.send(Err(fidl_error))
            }
        }
        .context("sending fuchsia.pkg.resolution/PackageResolver.Resolve response")
    }

    async fn resolve_toolbox(
        &self,
        payload: &fpkg_resolution::PackageResolverResolveRequest,
    ) -> Result<(), Error> {
        self.resolve_manage_inspect(
            &payload
                .package_url
                .as_ref()
                .ok_or(Error::MissingUrl)?
                .parse()
                .map_err(Error::InvalidUrl)?,
        )
        .await
        .map(|_| ())
    }
}

impl crate::component_resolver::PackageResolver for Resolver {
    type Error = Error;

    async fn resolve_and_serve(
        &self,
        url: &AbsolutePackageUrl,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<(fpkg::ResolutionContext, Arc<crate::root_dir::CachedRootDir>), Error> {
        self.resolve_and_serve(url, dir).await
    }

    async fn resolve_with_context_and_serve(
        &self,
        url: &PackageUrl,
        context: fpkg::ResolutionContext,
        dir: ServerEnd<fio::DirectoryMarker>,
    ) -> Result<(fpkg::ResolutionContext, Arc<crate::root_dir::CachedRootDir>), Error> {
        self.resolve_with_context_and_serve(url, context, dir).await
    }
}

fn lookup_cache_fallback(
    url: &AbsolutePackageUrl,
    cache_index: &crate::CacheIndex,
) -> Option<fuchsia_hash::Hash> {
    // TODO(https://fxbug.dev/335388895): Remove variant concept.
    // The URLs in the cache index do not have a variant, but we still want to accept incoming URLs
    // with a variant of "0".
    let mut no_variant;
    let url = match url.variant() {
        None => url,
        Some(variant) if !variant.is_zero() => {
            return None;
        }
        Some(_) => {
            no_variant = url.clone();
            no_variant.clear_variant();
            &no_variant
        }
    };
    match (cache_index.url_to_hash(url), url.hash()) {
        (None, _) => None,
        (Some(index_hash), None) => Some(*index_hash),
        (Some(index_hash), Some(url_hash)) if *index_hash == url_hash => Some(*index_hash),
        _ => None,
    }
}

fn log_slow_cache_fallback(start_ts: zx::MonotonicInstant, url: &AbsolutePackageUrl) {
    static LAST_LOG_TIME: std::sync::LazyLock<fuchsia_sync::Mutex<zx::MonotonicInstant>> =
        std::sync::LazyLock::new(|| fuchsia_sync::Mutex::new(zx::MonotonicInstant::INFINITE_PAST));

    let now = zx::MonotonicInstant::get();
    let resolve_duration = now - start_ts;
    if resolve_duration < SLOW_CACHE_FALLBACK_WARN_DURATION {
        return;
    }
    {
        let mut last = LAST_LOG_TIME.lock();
        if now - *last < SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION {
            return;
        } else {
            *last = now;
        }
    }
    log::warn!(
        "Resolve of {} via cache fallback took {} seconds. This could be slowing down your system, \
         and may be due to trouble connecting to a remote repository. This log will only print \
         every {} minutes, so the issue may be occuring more often. See inspect for more \
         information.",
        url,
        resolve_duration.into_seconds(),
        SLOW_CACHE_FALLBACK_WARN_SQUELCH_DURATION.into_minutes(),
    );
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid url")]
    InvalidUrl(#[source] fuchsia_url::ParseError),

    #[error("absolute package URLs must have an empty context")]
    ContextWithAbsoluteUrl,

    #[error("forwarding to base resolver")]
    BaseResolver(#[source] crate::base_package_resolver::Error),

    #[error("upgradable packages must not be pinned")]
    PinnedUpgradablePackage,

    #[error("connecting to authority")]
    ConnectToAuthority(#[source] anyhow::Error),

    #[error("authority call failed")]
    AuthorityFidl(#[source] fidl::Error),

    #[error("authority lookup failed: {0:?}")]
    Authority(fpkg::AuthorityLookupError),

    #[error("invalid blob dir URI")]
    InvalidBlobDirUri(#[source] http::uri::InvalidUri),

    #[error("forwarding to package fetcher")]
    PackageFetcher(#[source] crate::package_fetcher::Error),

    #[error("creating root dir")]
    CreatingRootDir {
        #[source]
        source: package_directory::Error,
        pkg_id: fuchsia_merkle::Hash,
    },

    #[error("authenticating context")]
    ContextAuthenticator(#[source] context_authenticator::ContextAuthenticatorError),

    #[error(
        "package directory for {superpackage} was not open when resolving subpackage {subpackage}"
    )]
    SuperpackageNotOpen {
        superpackage: fuchsia_merkle::Hash,
        subpackage: fuchsia_url::RelativePackageUrl,
    },

    #[error("reading subpackage manifest")]
    ReadingSubpackages(#[source] package_directory::SubpackagesError),

    #[error("subpackage {subpackage} of {superpackage} not found")]
    SubpackageNotFound {
        subpackage: fuchsia_url::RelativePackageUrl,
        superpackage: fuchsia_merkle::Hash,
    },

    #[error("creating subpackage root dir")]
    CreatingSubpackageRootDir {
        #[source]
        source: package_directory::Error,
        subpackage: fuchsia_merkle::Hash,
    },

    #[error("the url was not set in the fidl request")]
    MissingUrl,
}

impl From<&Error> for fidl_fuchsia_component_resolution::ResolverError {
    fn from(err: &Error) -> fidl_fuchsia_component_resolution::ResolverError {
        use Error::*;
        use fidl_fuchsia_component_resolution::ResolverError as Err;
        match err {
            InvalidUrl(_) => Err::InvalidArgs,
            ContextWithAbsoluteUrl => Err::InvalidArgs,
            BaseResolver(e) => e.into(),
            PinnedUpgradablePackage => Err::InvalidArgs,
            ConnectToAuthority(_) => Err::Io,
            AuthorityFidl(_) => Err::Io,
            Authority(e) => authority_to_component_resolve_err(e),
            InvalidBlobDirUri(_) => Err::Internal,
            CreatingRootDir { .. } => Err::Io,
            PackageFetcher(source) => source.into(),
            ContextAuthenticator(_) => Err::InvalidArgs,
            SuperpackageNotOpen { .. } => Err::Internal,
            ReadingSubpackages(_) => Err::Io,
            SubpackageNotFound { .. } => Err::PackageNotFound,
            CreatingSubpackageRootDir { .. } => Err::Io,
            MissingUrl => Err::InvalidArgs,
        }
    }
}

fn authority_to_component_resolve_err(
    e: &fpkg::AuthorityLookupError,
) -> fidl_fuchsia_component_resolution::ResolverError {
    use fidl_fuchsia_component_resolution::ResolverError as Err;
    use fpkg::AuthorityLookupError::*;
    match e {
        InvalidUrl => Err::InvalidArgs,
        PinnedUrlNotAllowed => Err::Internal,
        RepositoryNotFound => Err::ResourceUnavailable,
        PackageNotFound => Err::PackageNotFound,
        UpstreamConnection => Err::Io,
        Internal => Err::Internal,
    }
}

impl From<&Error> for fpkg::ResolveError {
    fn from(err: &Error) -> Self {
        use Error::*;
        use fpkg::ResolveError as Err;
        match err {
            InvalidUrl(_) => Err::InvalidUrl,
            ContextWithAbsoluteUrl => Err::InvalidContext,
            BaseResolver(e) => e.into(),
            PinnedUpgradablePackage => Err::InvalidUrl,
            ConnectToAuthority(_) => Err::Io,
            AuthorityFidl(_) => Err::Io,
            Authority(e) => fpkg_ext::errors::authority_to_resolve_err(e),
            InvalidBlobDirUri(_) => Err::Internal,
            CreatingRootDir { .. } => Err::Io,
            PackageFetcher(source) => source.into(),
            ContextAuthenticator(_) => Err::InvalidContext,
            SuperpackageNotOpen { .. } => Err::Internal,
            ReadingSubpackages(_) => Err::Io,
            SubpackageNotFound { .. } => Err::PackageNotFound,
            CreatingSubpackageRootDir { .. } => Err::Io,
            MissingUrl => Err::InvalidUrl,
        }
    }
}

impl From<&Error> for zx::Status {
    fn from(err: &Error) -> Self {
        let fidl_err: fpkg::ResolveError = err.into();
        use fpkg::ResolveError::*;
        match fidl_err {
            Internal => zx::Status::INTERNAL,
            AccessDenied => zx::Status::ACCESS_DENIED,
            Io => zx::Status::IO,
            BlobNotFound => zx::Status::INTERNAL,
            PackageNotFound => zx::Status::NOT_FOUND,
            RepoNotFound => zx::Status::NOT_FOUND,
            NoSpace => zx::Status::NO_SPACE,
            UnavailableBlob => zx::Status::UNAVAILABLE,
            UnavailableRepoMetadata => zx::Status::UNAVAILABLE,
            InvalidUrl => zx::Status::INVALID_ARGS,
            InvalidContext => zx::Status::INVALID_ARGS,
        }
    }
}

enum LookupSource {
    /// Package blobs may not all be present in local storage and can be fetched from this HTTP
    /// blob directory.
    Remote(http::Uri),
    /// Package blobs are guaranteed to be present in local storage. Holds an optional
    /// `CleanupGuard` keeping an upgradable package protected from GC until opened.
    Local(Option<crate::index::CleanupGuard>),
}

impl LookupSource {
    fn blob_source(&self) -> Option<&http::Uri> {
        match self {
            Self::Remote(uri) => Some(uri),
            Self::Local(_) => None,
        }
    }
}

fn internal_to_toolbox_error(err: fpkg::ResolveError) -> fpkg_resolution::ResolveError {
    use fpkg::ResolveError::*;
    use fpkg_resolution::ResolveError as Err;
    match err {
        Internal => Err::Internal,
        AccessDenied => Err::AccessDenied,
        Io => Err::Io,
        BlobNotFound => Err::BlobNotFound,
        PackageNotFound => Err::PackageNotFound,
        RepoNotFound => Err::RepoNotFound,
        NoSpace => Err::NoSpace,
        UnavailableBlob => Err::UnavailableBlob,
        UnavailableRepoMetadata => Err::UnavailableRepoMetadata,
        InvalidUrl => Err::InvalidUrl,
        InvalidContext => Err::InvalidContext,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        expect_get_vmo_async, get_meta_far, next_get_vmo_async, respond_get_vmo,
    };
    use fidl_fuchsia_fxfs as ffxfs;
    use fuchsia_async as fasync;
    use std::collections::{HashMap, HashSet};
    use std::pin::pin;
    use std::task::Poll;

    async fn read_protected_blobs(
        package_index: &PackageIndex,
        initialized: crate::index::UpgradablePackagesInitialized,
        blob_reader_stream: &mut ffxfs::BlobReaderRequestStream,
        meta_fars: &HashMap<fuchsia_hash::Hash, Vec<u8>>,
    ) -> HashSet<fuchsia_hash::Hash> {
        let gc_guard = package_index.acquire_gc_lock(initialized).await;
        let mut protected_blobs_future = pin!(gc_guard.protected_blobs());
        loop {
            if let Poll::Ready(blobs) =
                fasync::TestExecutor::poll_until_stalled(&mut protected_blobs_future).await
            {
                return blobs;
            }
            let (blob_hash, responder) = next_get_vmo_async(blob_reader_stream).await;
            respond_get_vmo(responder, &meta_fars[&blob_hash]);
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn resolve_keeps_replaced_upgradable_package_protected_until_opened() {
        let (blobfs, _blobfs_dir_stream, mut blob_reader_stream, _) = blobfs::Client::new_test();
        let base_index = Arc::new(crate::BaseIndex::new_test_only(HashSet::new(), []));
        let cache_index = Arc::new(crate::CacheIndex::new_test_only(HashSet::new(), []));
        let package_index =
            Arc::new(PackageIndex::new(blobfs.clone(), Some(Arc::clone(&cache_index))));

        let bootfs = fidl::endpoints::create_proxy::<fio::DirectoryMarker>().0;
        let (root_dir_factory, open_packages) =
            crate::root_dir::new(bootfs, blobfs.clone()).await.unwrap();

        let authenticator = context_authenticator::ContextAuthenticator::new();
        let scope = vfs::execution_scope::ExecutionScope::new();
        let base_resolver = crate::base_package_resolver::Resolver::new(
            Arc::clone(&base_index),
            authenticator.clone(),
            open_packages.clone(),
            scope.clone(),
        );
        let inspector = finspect::Inspector::default();
        let package_fetcher = crate::package_fetcher::PackageFetcher::new(
            Arc::clone(&package_index),
            blobfs,
            crate::blob_fetcher::BlobFetcher::new_closed(),
            root_dir_factory,
            open_packages.clone(),
            inspector.root().create_child("package_fetcher"),
        );
        let executability_decider = crate::executability::Decider::new(
            system_image::ExecutabilityRestrictions::Enforce,
            Arc::clone(&base_index),
        );
        let resolver = Resolver::new(
            base_resolver,
            Arc::clone(&package_index),
            fidl_connector::ServiceReconnector::<fpkg::AuthorityMarker>::new(),
            cache_index,
            package_fetcher,
            authenticator,
            open_packages.clone(),
            executability_decider,
            scope,
            inspector.root().create_child("resolver"),
        );

        let url: AbsolutePackageUrl = "fuchsia-pkg://fuchsia.com/upgradable".parse().unwrap();
        let old_content = fuchsia_hash::Hash::from([1u8; 32]);
        let old_meta = fuchsia_hash::Hash::from([2u8; 32]);
        let old_meta_bytes = get_meta_far("upgradable", [old_content], []);

        let new_content = fuchsia_hash::Hash::from([3u8; 32]);
        let new_meta = fuchsia_hash::Hash::from([4u8; 32]);
        let new_meta_bytes = get_meta_far("upgradable", [new_content], []);

        let meta_fars =
            HashMap::from([(old_meta, old_meta_bytes.clone()), (new_meta, new_meta_bytes)]);

        package_index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{url}?hash={old_meta}") }],
                &base_index,
            )
            .await
            .unwrap();
        let initialized = package_index.wait_until_upgradable_packages_initialized().await;

        // Start resolving `url`; `Resolver::resolve` obtains a `CleanupGuard` for `old_meta` and
        // pauses inside `open_packages.get_or_insert` while reading `old_meta` from `BlobReader`.
        let mut resolve_future = pin!(resolver.resolve_manage_inspect(&url));
        assert!(fasync::TestExecutor::poll_until_stalled(&mut resolve_future).await.is_pending());
        let get_old_meta_responder = expect_get_vmo_async(&mut blob_reader_stream, old_meta).await;

        // Replace `url` with `new_meta` before `resolve_future` inserts `old_meta` into
        // `open_packages`.
        package_index
            .set_upgradable_urls(
                vec![fpkg::PackageUrl { url: format!("{url}?hash={new_meta}") }],
                &base_index,
            )
            .await
            .unwrap();
        assert!(open_packages.list().is_empty());

        // `cleanup_guard` held inside `Resolver::resolve` keeps `old_meta` and `old_content`
        // protected in `PackageIndex` alongside `new_meta` and `new_content`.
        assert_eq!(
            read_protected_blobs(&package_index, initialized, &mut blob_reader_stream, &meta_fars)
                .await,
            HashSet::from([old_meta, old_content, new_meta, new_content])
        );

        // Complete the `BlobReader` request so `Resolver::resolve` inserts `old_meta` into
        // `open_packages` and drops `cleanup_guard` upon returning.
        respond_get_vmo(get_old_meta_responder, &old_meta_bytes);
        let old_root_dir = resolve_future.await.unwrap();
        assert_eq!(*old_root_dir.hash(), old_meta);
        assert_eq!(
            open_packages.list().into_iter().map(|dir| *dir.hash()).collect::<Vec<_>>(),
            vec![old_meta]
        );

        // Once `Resolver::resolve` returns, `PackageIndex` only protects `new_meta` (while
        // `old_meta` remains protected via `open_packages` until `old_root_dir` is dropped).
        assert_eq!(
            read_protected_blobs(&package_index, initialized, &mut blob_reader_stream, &meta_fars)
                .await,
            HashSet::from([new_meta, new_content])
        );
    }
}
