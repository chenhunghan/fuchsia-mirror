// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::info;
use addr::{TargetAddr, TargetIpAddr};
use anyhow::Result;
use discovery::query::TargetInfoQuery;
use discovery::{Discovery, DiscoveryBuilder, DiscoverySources, TargetEvent, TargetHandle};
use fdomain_fuchsia_developer_remotecontrol::{IdentifyHostResponse, RemoteControlProxy};
use ffx_command_error::user_error;
use ffx_config::{EnvironmentContext, TryFromEnvContext, keys};
use ffx_diagnostics_analytics::ResultExt;
use ffx_diagnostics_formatting::TargetInfoQueryExt;
use fidl_fuchsia_developer_ffx::{self as ffx};
use fuchsia_async::TimeoutExt;
use futures::future::{BoxFuture, LocalBoxFuture};
use futures::{FutureExt, Stream, StreamExt, pin_mut};
use std::fmt::{Debug, Display};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use target_errors::{FfxTargetError, TargetSource};
use tokio::sync::Mutex;

use crate::analytics::PointOfFailure;
use crate::connection::Connection;
use crate::ssh_connector::SshConnector;
use crate::uart_connector::UartConnector;
use crate::usb_connector::{UsbConnector, try_daemon_autostart};
use crate::vsock_connector::VSockConnector;
use crate::{TargetInfo, get_target_specifier_with_source, target_source_for_query};

const CONFIG_TARGET_SSH_TIMEOUT: &str = "target.host_pipe_ssh_timeout";

const TARGET_DEFAULT_PORT: u16 = 22;
const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;

#[cfg(test)]
use {mockall::mock, mockall::predicate::*};

fn replace_default_port(sa: SocketAddr) -> SocketAddr {
    let mut sa = sa;
    if sa.port() == 0 {
        sa.set_port(TARGET_DEFAULT_PORT);
    }
    sa
}

fn target_error_to_analytics<'a>(
    err: &'a FfxTargetError,
) -> Option<crate::analytics::PointOfFailure<'a>> {
    match err {
        FfxTargetError::OpenTargetError {
            err: ffx::OpenTargetError::TargetNotFound,
            target,
            ..
        } => Some(crate::analytics::PointOfFailure::NoMatchingTargets {
            query: target.as_ref().map(|s| {
                TargetInfoQuery::try_from(s.clone())
                    .map(|q| q.to_analytics_tag())
                    .unwrap_or_else(|_| "invalid".to_owned())
            }),
            discovery_sources: DiscoverySources::all(),
        }),
        FfxTargetError::OpenTargetError {
            err: ffx::OpenTargetError::QueryAmbiguous,
            target,
            ..
        } => Some(crate::analytics::PointOfFailure::TooManyMatchingTargets {
            query: target.as_ref().map(|s| {
                TargetInfoQuery::try_from(s.clone())
                    .map(|q| q.to_analytics_tag())
                    .unwrap_or_else(|_| "invalid".to_owned())
            }),
            discovery_sources: DiscoverySources::all(),
        }),
        _ => None,
    }
}

pub(crate) fn expect_single_target<T>(
    query: &TargetInfoQuery,
    targets: Vec<T>,
    source: Option<TargetSource>,
) -> Result<T, FfxTargetError>
where
    T: Display,
{
    match targets.len() {
        0 => Err(FfxTargetError::OpenTargetError {
            err: ffx::OpenTargetError::TargetNotFound,
            target: Some(query.into()),
            targets: vec![],
            target_source: source,
        })
        .into(),
        1 => Ok(targets.into_iter().next().unwrap()),
        _ => Err(FfxTargetError::OpenTargetError {
            err: ffx::OpenTargetError::QueryAmbiguous,
            target: Some(query.into()),
            targets: targets.iter().map(|f| format!("{}", f)).collect(),
            target_source: source,
        })
        .into(),
    }
}

/// Discover a target; useful when we don't necessarily want to make a connection,
/// but we _do_ want to get the information available when discovering (TargetState,
/// etc).
pub async fn discover_single_default_target(
    ctx: &EnvironmentContext,
) -> std::result::Result<TargetHandle, crate::FfxTargetCrateError> {
    let (query_s, source) = get_target_specifier_with_source(ctx)?;
    let query = TargetInfoQuery::try_from(query_s)?;

    let handles = get_discovered_targets(query.clone(), true, true, ctx).await?;
    let res = expect_single_target(&query, handles, source)
        .or_else_maybe_analytics(|e| target_error_to_analytics(e).map(Into::into))
        .await?;
    Ok(res)
}

/// A trait for resolving target queries into concrete target information.
///
/// This trait provides an abstraction over the process of finding a target
/// based on a user-provided specifier, which could be a nodename, a serial
/// number, an IP address, or other forms of target identification.
///
/// Implementors of this trait are responsible for searching for targets,
/// handling ambiguity, and returning a `Resolution` object that can be used
/// to connect to the target.
pub trait TargetResolver {
    /// Gets a set of handles from the resolver's sources that match the query
    fn discovered_targets(
        &self,
        query: TargetInfoQuery,
    ) -> impl Future<Output = Result<Vec<TargetHandle>>>;

    /// Attempts to resolve a target by name from a manually configured list.
    ///
    /// This method is used to check for targets that have been explicitly
    /// defined by the user, outside of the standard discovery mechanisms.
    fn try_resolve_manual_target(
        &self,
        name: &str,
        ctx: &EnvironmentContext,
    ) -> impl Future<Output = Result<Option<Resolution>>>;

    /// Resolves a target specifier into a `Resolution` containing a connectable address.
    ///
    /// This is a high-level method that should handle various query types and
    /// ensure that a single, unambiguous target is found. It is an error if the query
    /// does not resolve to exactly one target.
    fn resolve_target_address(
        &self,
        target_spec: &TargetInfoQuery,
        use_cache: bool,
        ctx: &EnvironmentContext,
        source: Option<TargetSource>,
    ) -> impl Future<Output = Result<Resolution, FfxTargetError>> {
        async move {
            if let Some(target_addr) = target_spec.get_target_addr() {
                let query_tag = target_spec.to_analytics_tag();
                emit_cache_event("explicit_addr", &query_tag).await;
                return Ok(Resolution::from_target(target_addr.into()));
            }
            let res = self.resolve_single_target(target_spec, use_cache, ctx, source).await?;
            let target_spec_info: String = target_spec.into();
            log::debug!("resolved target spec {target_spec_info} to address {:?}", res.addr());
            Ok(res)
        }
    }

    /// Resolves a target specifier to a single `Resolution`.
    ///
    /// This method orchestrates the discovery process, including checking
    /// manual targets. We return a Resolution because checking manual
    /// targets involves talking to them, so we want to take advantage of
    /// that to preserve the full Resolution state.
    fn discover_matching_targets(
        &self,
        target_spec: &TargetInfoQuery,
        env_context: &EnvironmentContext,
        source: Option<TargetSource>,
    ) -> impl Future<Output = Result<Vec<Resolution>, FfxTargetError>> {
        async move {
            let handles_fut = self.discovered_targets(target_spec.clone()).fuse();
            pin_mut!(handles_fut);
            let mut discovered: Option<Vec<TargetHandle>> = None;

            // If the query is not specified, we won't even bother with trying to resolve a manual target.
            if let TargetInfoQuery::NodenameOrId(s) = &target_spec {
                // We want to query both the manual targets and the discoverable handles concurrently.
                let manual_target_fut = self.try_resolve_manual_target(s, env_context).fuse();
                pin_mut!(manual_target_fut);
                futures::select! {
                    mtr = &mut manual_target_fut => match mtr {
                        Err(e) => {
                            log::debug!("Failed to resolve target {s} as manual target: {e:?}");
                            // Keep going, waiting for the discovery to complete
                        }
                        Ok(Some(res)) => return Ok(vec![res]), // We found a manual target, so we're done
                        _ => (), // No manual target with this name
                    },
                    handles_res = handles_fut => match handles_res {
                        Ok(r) => {
                            if r.is_empty() {
                                match manual_target_fut.await {
                                    Err(e) => {
                                        log::debug!(
                                            "Failed to resolve target {s} as manual target: {e:?}"
                                        );
                                    }
                                    Ok(Some(res)) => return Ok(vec![res]),
                                    Ok(None) => (),
                                }
                            }
                            discovered = Some(r);
                        }
                        Err(e) => {
                            log::warn!("Target discovery failed: {e:?}");
                            return Err(FfxTargetError::OpenTargetError {
                                err: ffx::OpenTargetError::FailedDiscovery,
                                target: Some(target_spec.into()),
                                targets: vec![],
                                target_source: source.clone(),
                            })
                        },
                    },
                }
            }

            // If we haven't gotten a result yet (because the query wasn't
            // NodenameOrId, or because the manual-target query completed
            // without finding a target), get it now.
            let discovered = match discovered {
                Some(targets) => targets,
                None => handles_fut.await.map_err(|e| {
                    log::warn!("Target discovery failed: {e:?}");
                    FfxTargetError::OpenTargetError {
                        err: ffx::OpenTargetError::FailedDiscovery,
                        target: Some(target_spec.into()),
                        targets: vec![],
                        target_source: source.clone(),
                    }
                })?,
            };
            discovered
                .into_iter()
                .map(|th| {
                    Resolution::from_target_handle(th).map_err(|e| {
                        log::warn!("Conversion to Resolution failed: {e:?}");
                        FfxTargetError::OpenTargetError {
                            err: ffx::OpenTargetError::FailedDiscovery,
                            target: Some(target_spec.into()),
                            targets: vec![],
                            target_source: source.clone(),
                        }
                    })
                })
                .collect()
        }
    }

    /// Resolves a target specifier to a single `Resolution`.
    ///
    /// Note: Explicit address specifiers (`-t <addr>`) are short-circuited in
    /// `resolve_target_address()` before this method is called.
    ///
    /// In strict mode, symbolic queries (`id:<serial>` or `<nodename>`) resolve from the
    /// registered targets file (`target.registered_targets`, defaulting to
    /// `$SHARED_DATA/targets.json`) without consulting the interactive cache or performing
    /// active discovery.
    ///
    /// In non-strict mode, it tries the discovery cache (when `use_cache` is true) and falls
    /// back to active discovery (`discover_matching_targets()`).
    fn resolve_single_target(
        &self,
        target_spec: &TargetInfoQuery,
        use_cache: bool,
        env_context: &EnvironmentContext,
        source: Option<TargetSource>,
    ) -> impl Future<Output = Result<Resolution, FfxTargetError>> {
        async move {
            let cached = if env_context.is_strict() {
                lookup_registered_targets(target_spec, env_context)
            } else {
                lookup_discovery_cache(target_spec, use_cache, env_context).await
            };
            let resolutions = match cached {
                Some(resolutions) => resolutions,
                None => {
                    self.discover_matching_targets(target_spec, env_context, source.clone()).await?
                }
            };

            let mut resolution = expect_single_target(target_spec, resolutions, source)
                .or_else_maybe_analytics(|e| target_error_to_analytics(e).map(Into::into))
                .await?;
            if resolution.expected_query.is_none() {
                let desc = resolution.discovered.as_ref().map(discovery::Description::from);
                resolution.expected_query = expected_query_for_match(
                    target_spec,
                    desc.as_ref().and_then(|d| d.nodename.as_deref()),
                    desc.as_ref().and_then(|d| d.serial.as_deref()),
                );
            }
            Ok(resolution)
        }
    }
}

// Narrows an unannotated `NodenameOrId(s)` query to `Id(s)` when the resolved target matched `s`
// by serial number rather than nodename, so `identity_matches` treats an absent system serial as
// consistent instead of rejecting the target's different nodename.
fn expected_query_for_match(
    query: &TargetInfoQuery,
    nodename: Option<&str>,
    serial: Option<&str>,
) -> Option<TargetInfoQuery> {
    match query {
        TargetInfoQuery::Id(_) => Some(query.clone()),
        TargetInfoQuery::NodenameOrId(s) => {
            if nodename != Some(s.as_str()) && serial == Some(s.as_str()) {
                Some(TargetInfoQuery::Id(s.clone()))
            } else {
                Some(query.clone())
            }
        }
        _ => None,
    }
}

// Filters a list of TargetInfo entries against a TargetInfoQuery and converts matches to Resolutions.
fn match_target_infos_to_resolutions(
    targets: Vec<TargetInfo>,
    target_spec: &TargetInfoQuery,
) -> Vec<Resolution> {
    targets
        .into_iter()
        .filter(|ti| ti.match_query(target_spec))
        .filter_map(|ti| {
            let mut res = ResolutionTarget::from_target_info(&ti).map(Resolution::from_target)?;
            res.expected_query = expected_query_for_match(
                target_spec,
                ti.nodename.as_deref(),
                ti.serial_number.as_deref(),
            );
            Some(res)
        })
        .collect()
}

// Looks up `target_spec` in `$SHARED_DATA/targets.json`, returning `None` if unconfigured or missing.
// TODO(b/571246542): Stop returning `None` once `botanist` (`force_ffx_usb`) populates `targets.json`.
fn lookup_registered_targets(
    target_spec: &TargetInfoQuery,
    env_context: &EnvironmentContext,
) -> Option<Vec<Resolution>> {
    match crate::registry::targets_file_path(env_context)
        .and_then(|path| crate::registry::load_registered_targets(&path))
    {
        Ok(targets) => Some(match_target_infos_to_resolutions(targets, target_spec)),
        Err(crate::registry::RegistryError::SharedData(_)) => None,
        Err(crate::registry::RegistryError::Store(err)) if err.is_not_found() => None,
        Err(e) => {
            log::warn!("Loading registered targets failed: {e:?}");
            Some(Vec::new())
        }
    }
}

// Looks up `target_spec` in the non-strict discovery cache when `use_cache` is true.
async fn lookup_discovery_cache(
    target_spec: &TargetInfoQuery,
    use_cache: bool,
    env_context: &EnvironmentContext,
) -> Option<Vec<Resolution>> {
    let query_tag = target_spec.to_analytics_tag();
    if !use_cache {
        emit_cache_event("cache_disabled", &query_tag).await;
        return None;
    }
    let Some(cache_file) = crate::cache::get_discovery_cache_file(env_context) else {
        emit_cache_event("no_cache_file", &query_tag).await;
        return None;
    };
    match crate::cache::Cache::load(&cache_file) {
        Ok(cache) => {
            let resolutions = match_target_infos_to_resolutions(cache.targets, target_spec);
            for r in &resolutions {
                r.from_cache.store(true, Ordering::Relaxed);
            }
            let status = if resolutions.is_empty() { "cache_miss" } else { "cache_hit" };
            emit_cache_event(status, &query_tag).await;
            (!resolutions.is_empty()).then_some(resolutions)
        }
        Err(crate::cache::CacheError::Store(err)) if err.is_not_found() => {
            emit_cache_event("no_cache_file", &query_tag).await;
            None
        }
        Err(crate::cache::CacheError::Expired(_)) => {
            emit_cache_event("cache_expired", &query_tag).await;
            None
        }
        Err(e) => {
            emit_cache_event("cache_invalid", &query_tag).await;
            log::warn!("Cache loading failed: {e:?}");
            None
        }
    }
}

#[cfg(test)]
mock! {
    TargetResolver{}
    impl TargetResolver for TargetResolver {
        async fn discovered_targets(
            &self,
            query: TargetInfoQuery,
        ) -> Result<Vec<TargetHandle>>;

        async fn try_resolve_manual_target(
            &self,
            name: &str,
            ctx: &EnvironmentContext,
        ) -> Result<Option<Resolution>>;
    }
}

pub(crate) fn build_discovery_builder(
    sources: DiscoverySources,
    ctx: &EnvironmentContext,
) -> DiscoveryBuilder {
    let mut builder = DiscoveryBuilder::default()
        .set_source(sources)
        .with_timeout_msecs(ctx.get(ffx_config::keys::DISCOVERY_TIMEOUT_MS).ok());

    if sources.contains(DiscoverySources::EMULATOR) {
        // If there is an error getting the config option, it is ignored to prevent
        // ffx-strict from failing when default configuration is not overridden.
        let emu_instance_root = ctx.get(ffx_config::keys::EMU_INSTANCE_ROOT_DIR).ok();
        builder = builder.with_emulator_instance_root(emu_instance_root);
    }
    if sources.contains(DiscoverySources::FASTBOOT_FILE) {
        // If there is an error getting the config option, it is ignored to prevent
        // ffx-strict from failing when default configuration is not overridden.
        let fastboot_file_path = ctx.get(ffx_config::keys::FASTBOOT_FILE_PATH).ok();
        builder = builder.with_fastboot_devices_file_path(fastboot_file_path);
    }

    if sources.contains(DiscoverySources::USB_VSOCK) {
        let usb_driver_socket = ctx.get(usb_driver_api::CONFIG_USB_SOCKET_PATH).ok();
        if let Some(path) = &usb_driver_socket {
            try_daemon_autostart(path, ctx);
        }
        builder = builder.with_usb_vsock_driver_socket_path(usb_driver_socket);
    }

    if sources.contains(DiscoverySources::GCE) {
        let gce_instance_root = ctx.get(ffx_config::keys::GCE_INSTANCE_ROOT_DIR).ok();
        builder = builder.with_gce_instance_root(gce_instance_root);
    }

    if sources.contains(DiscoverySources::UART) {
        let uart_driver_socket_dir = ctx
            .get::<PathBuf, _>("shared_data")
            .ok()
            .or_else(|| ctx.get_shared_data_path().ok())
            .map(|p| p.join(uart_driver_api::UART_SHARED_SUBDIR));
        builder = builder.with_uart_driver_socket_dir(uart_driver_socket_dir);
    }

    builder
}

pub fn build_discovery(sources: DiscoverySources, ctx: &EnvironmentContext) -> Discovery {
    let builder = build_discovery_builder(sources, ctx);
    builder.build(ctx)
}

pub fn build_discovery_from_config(ctx: &EnvironmentContext) -> Discovery {
    build_discovery_builder_common(ctx).build(ctx)
}

pub fn build_discovery_builder_common(ctx: &EnvironmentContext) -> DiscoveryBuilder {
    let mut sources = DiscoverySources::all();
    // Fastboot targets (bootloaders) are not ssh-able and do not support target
    // address resolution, so we exclude them from configuration-based discovery.
    sources.remove(DiscoverySources::USB_FASTBOOT | DiscoverySources::FASTBOOT_FILE);
    if !ctx.get(ffx_config::keys::USB_ENABLED).unwrap_or(true) {
        sources.remove(DiscoverySources::USB_VSOCK);
    }
    if !ctx.get(ffx_config::keys::NETWORK_ENABLED).unwrap_or(true) {
        sources.remove(DiscoverySources::MDNS);
    }
    if !ctx.get(ffx_config::keys::UART_ENABLED).unwrap_or(false) {
        sources.remove(DiscoverySources::UART);
    }
    build_discovery_builder(sources, ctx)
}

fn handle_transport_sources(th: &TargetHandle) -> DiscoverySources {
    match &th.state {
        discovery::TargetState::Product { addrs, .. } => {
            let mut seen = DiscoverySources::empty();
            for addr in addrs {
                match addr {
                    TargetAddr::Net(_) => seen.insert(DiscoverySources::MDNS),
                    TargetAddr::UsbCtx(_) => seen.insert(DiscoverySources::USB_VSOCK),
                    TargetAddr::Uart(_) => seen.insert(DiscoverySources::UART),
                    TargetAddr::VSockCtx(_) => seen.insert(DiscoverySources::EMULATOR),
                }
            }
            if seen.is_empty() { DiscoverySources::all() } else { seen }
        }
        discovery::TargetState::Fastboot(_)
        | discovery::TargetState::Zedboot
        | discovery::TargetState::Unknown => DiscoverySources::all(),
    }
}

fn short_circuit_stream_by_sources(
    stream: impl Stream<Item = TargetEvent>,
    query: TargetInfoQuery,
    sources: DiscoverySources,
) -> impl Stream<Item = TargetHandle> {
    let expected_transports =
        sources & (DiscoverySources::MDNS | DiscoverySources::USB_VSOCK | DiscoverySources::UART);
    let wait_for_all_transports =
        matches!(query, TargetInfoQuery::NodenameOrId(_) | TargetInfoQuery::Id(_));
    let is_first = matches!(query, TargetInfoQuery::First);
    let (single_target_tx, single_target_rx) = futures::channel::oneshot::channel();
    stream
        .scan((DiscoverySources::empty(), Some(single_target_tx)), move |(seen, tx), ev| {
            if tx.is_none() {
                return futures::future::ready(None);
            }
            let th = if let TargetEvent::Added(th) = ev {
                if !is_first {
                    seen.insert(handle_transport_sources(&th));
                    if !wait_for_all_transports
                        || seen.contains(expected_transports)
                        || seen.contains(DiscoverySources::EMULATOR)
                    {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                Some(th)
            } else {
                None
            };
            futures::future::ready(Some(th))
        })
        .filter_map(futures::future::ready)
        .take_until(single_target_rx)
}

// Return a stream of TargetHandles that come from the specified sources, and
// match the query. The discovery will return early once all enabled transport
// sources have reported a match for the query (or on the first match when only
// a single transport is active).
fn get_discovery_stream_with_sources(
    query: TargetInfoQuery,
    sources: DiscoverySources,
    ctx: &EnvironmentContext,
) -> std::result::Result<impl Stream<Item = TargetHandle>, crate::FfxTargetCrateError> {
    let discovery = build_discovery_builder(sources, ctx).with_short_circuit(false).build(ctx);
    let stream = discovery.discovery_stream(query.clone())?;
    Ok(short_circuit_stream_by_sources(stream, query, sources))
}

// Return a set of TargetHandles that come from the specified sources, and match
// the query. The discovery will return early if a result "perfectly" matches the
// query (e.g. an mDNS response with the exact name requested).
async fn get_discovered_targets_with_sources(
    query: TargetInfoQuery,
    sources: DiscoverySources,
    ctx: &EnvironmentContext,
) -> Result<Vec<TargetHandle>> {
    let discovery = build_discovery(sources, ctx);
    discovery.discover_devices(query.clone()).await.map_err(|e| anyhow::anyhow!(e))
}

/// Attempts to resolve the query into a target's ssh-able address. It is an error
/// if it the query doesn't match exactly one target.
// Perhaps refactor as connect_to_target() -> Result<Connection>, since that seems
// to be the only way this function is used?
pub async fn resolve_target_address(
    target_spec: &TargetInfoQuery,
    use_cache: bool,
    ctx: &EnvironmentContext,
) -> Result<Resolution, FfxTargetError> {
    let source = target_source_for_query(target_spec, ctx);
    let discovery = build_discovery_from_config(ctx);
    let resolver = DefaultTargetResolver::new(discovery);
    resolver.resolve_target_address(target_spec, use_cache, ctx, source).await
}

pub struct DefaultTargetResolver {
    discovery: Discovery,
}

impl DefaultTargetResolver {
    pub fn new(discovery: Discovery) -> Self {
        Self { discovery }
    }

    pub fn discovery_timeout(&self) -> Option<Duration> {
        self.discovery.timeout()
    }

    pub fn set_discovery_timeout(&mut self, timeout: Option<Duration>) {
        self.discovery.set_timeout(timeout);
    }
}

/// Return a stream of handles matching the query. If a target matches the query
/// exactly (i.e. the query is the full name of the target, not a substring),
/// return the handle immediately. Otherwise, run until the timeout (default 2
/// seconds, configurable via "discovery.timeout"). The contents of the stream
/// can be filtered by USB and mDNS.
pub fn get_discovery_stream(
    query: TargetInfoQuery,
    usb: bool,
    mdns: bool,
    ctx: &EnvironmentContext,
) -> std::result::Result<impl Stream<Item = TargetHandle>, crate::FfxTargetCrateError> {
    let mut sources = DiscoverySources::MANUAL
        | DiscoverySources::EMULATOR
        | DiscoverySources::FASTBOOT_FILE
        | DiscoverySources::GCE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;

        if ctx.get(keys::USB_ENABLED).unwrap_or(true) {
            sources = sources | DiscoverySources::USB_VSOCK;
        }
    }
    if mdns && ctx.get(keys::NETWORK_ENABLED).unwrap_or(true) {
        sources = sources | DiscoverySources::MDNS;
    }
    if ctx.get(keys::UART_ENABLED).unwrap_or(false) {
        sources = sources | DiscoverySources::UART;
    }
    Ok(get_discovery_stream_with_sources(query, sources, ctx)?)
}

/// Return a list of handles matching the query. If a target matches the query
/// exactly (i.e. the query is the full name of the target, not a substring),
/// return the handle immediately. Otherwise, run until the timeout (default 2
/// seconds, configurable via "discovery.timeout"). The contents of the set can
/// be filtered by USB and mDNS.
pub async fn get_discovered_targets(
    query: TargetInfoQuery,
    usb: bool,
    mdns: bool,
    ctx: &EnvironmentContext,
) -> std::result::Result<Vec<TargetHandle>, crate::FfxTargetCrateError> {
    let mut sources = DiscoverySources::MANUAL
        | DiscoverySources::EMULATOR
        | DiscoverySources::FASTBOOT_FILE
        | DiscoverySources::GCE;
    if usb {
        sources = sources | DiscoverySources::USB_FASTBOOT;

        if ctx.get(keys::USB_ENABLED).unwrap_or(true) {
            sources = sources | DiscoverySources::USB_VSOCK;
        }
    }
    if mdns && ctx.get(keys::NETWORK_ENABLED).unwrap_or(true) {
        sources = sources | DiscoverySources::MDNS;
    }
    if ctx.get(keys::UART_ENABLED).unwrap_or(false) {
        sources = sources | DiscoverySources::UART;
    }
    // Get nodename, in case we're trying to find an exact match
    Ok(get_discovered_targets_with_sources(query, sources, ctx).await?)
}

impl TargetResolver for DefaultTargetResolver {
    async fn discovered_targets(&self, query: TargetInfoQuery) -> Result<Vec<TargetHandle>> {
        self.discovery
            .discover_devices(query.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .or_analytics(PointOfFailure::DiscoveryFailure {
                query: Some(query.to_analytics_tag()),
                discovery_sources: self.discovery.sources(),
            })
            .await
    }

    async fn try_resolve_manual_target(
        &self,
        name: &str,
        ctx: &EnvironmentContext,
    ) -> Result<Option<Resolution>> {
        // This is something that is often mocked for testing. An improvement here would be to use the
        // environment context for locating manual targets.
        let finder = manual_targets::Config::new_from_context(ctx);
        let ssh_timeout: u64 = ctx.get(CONFIG_TARGET_SSH_TIMEOUT).unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
        let ssh_timeout = Duration::from_millis(ssh_timeout);
        let mut res = None;
        for t in manual_targets::watcher::parse_manual_targets(&finder).await.into_iter() {
            let addr = t.addr();
            let resolution = Resolution::from_addr(addr);
            let identify = resolution
                .identify(ctx)
                .on_timeout(ssh_timeout, || {
                    Err(crate::error::TargetResolutionError::ManualTargetTimeout {
                        addr,
                        timeout: ssh_timeout,
                    }
                    .into())
                })
                .await?;

            if identify.nodename == Some(String::from(name)) {
                res = Some(resolution);
            }
        }
        Ok(res)
    }
}

enum ResolutionTarget {
    Addr(SocketAddr),
    Usb(u32),
    Vsock(u32),
    Uart(String),
    TestMock(Box<dyn Fn() -> Result<Connection> + Send + Sync>),
    TestMockAsync(Box<dyn Fn() -> BoxFuture<'static, Result<Connection>> + Send + Sync>),
}

impl Debug for ResolutionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Addr(arg0) => f.debug_tuple("Addr").field(arg0).finish(),
            Self::Usb(arg0) => f.debug_tuple("Usb").field(arg0).finish(),
            Self::Vsock(arg0) => f.debug_tuple("Vsock").field(arg0).finish(),
            Self::Uart(arg0) => f.debug_tuple("Uart").field(arg0).finish(),
            Self::TestMock(_) => f.debug_tuple("TestMock").field(&"..").finish(),
            Self::TestMockAsync(_) => f.debug_tuple("TestMockAsync").field(&"..").finish(),
        }
    }
}

// When choosing an address for resolution, we choose in the following order:
// - any VSock address
// - any USB address
// - any link-local V6 address
// - any other network address
// - any UART address
fn choose_address_from_addresses(
    nodename: &Option<String>,
    addresses: &[TargetAddr],
) -> Option<TargetAddr> {
    if addresses.is_empty() {
        log::warn!("Target discovered but does not contain addresses: {nodename:?}");
        return None;
    }
    Some(
        addresses
            .iter()
            .cloned()
            .min_by(TargetAddr::compare_by_priority)
            .expect("Address list mysteriously became empty!"),
    )
}

impl From<TargetAddr> for ResolutionTarget {
    fn from(addr: TargetAddr) -> Self {
        match addr {
            TargetAddr::Net(sock) => {
                let addr: SocketAddr = replace_default_port(sock);
                Self::Addr(addr)
            }
            TargetAddr::VSockCtx(cid) => Self::Vsock(cid),
            TargetAddr::UsbCtx(cid) => Self::Usb(cid),
            TargetAddr::Uart(endpoint) => Self::Uart(endpoint),
        }
    }
}

impl ResolutionTarget {
    fn from_addrs(name: &Option<String>, addresses: &[TargetAddr]) -> Option<ResolutionTarget> {
        // We'll ignore products where we can't choose an address, e.g. because
        // they don't seem to have one.
        let addr = choose_address_from_addresses(name, addresses)?;
        Some(addr.into())
    }
    fn from_target_handle(target: &TargetHandle) -> Option<ResolutionTarget> {
        match &target.state {
            // If the target is a product, use the "best" network address from the target.
            discovery::TargetState::Product { addrs: addresses, .. } => {
                Self::from_addrs(&target.node_name, addresses)
            }
            // Resolutions are only supported for devices in Product mode
            _ => None,
        }
    }

    fn from_target_info(info: &TargetInfo) -> Option<ResolutionTarget> {
        match info.target_state {
            // If the target is a product, use the "best" network address from the target.
            info::TargetState::Product => Self::from_addrs(&info.nodename, &info.addresses),
            // Resolutions are only supported for devices in Product mode
            _ => {
                log::debug!(
                    "Skipping target {:?} in unsupported state {:?}",
                    info.nodename,
                    info.target_state
                );
                None
            }
        }
    }

    fn to_spec(&self) -> String {
        match &self {
            ResolutionTarget::Addr(ssh_addr) => {
                format!("{ssh_addr}")
            }
            ResolutionTarget::Usb(cid) => {
                format!("usb:cid:{cid}")
            }
            ResolutionTarget::Vsock(cid) => {
                format!("vsock:cid:{cid}")
            }
            ResolutionTarget::Uart(endpoint) => {
                format!("uart:{endpoint}")
            }
            ResolutionTarget::TestMock(_) | ResolutionTarget::TestMockAsync(_) => {
                format!("mock_target")
            }
        }
    }
}

#[derive(Debug)]
enum ConnectionState {
    Uninitialized,
    Connected(Arc<Connection>),
    Failed,
}

// The identity that RCS reports when it cannot ask the system for one (see `DefaultIdentifier`
// in //src/developer/remote-control/src/host_identifier.rs).
const DEFAULT_NODENAME: &str = "fuchsia-default-nodename";
const DEFAULT_SERIAL_NUMBER: &str = "fuchsia-default-serial-number";

// Returns whether `identify` is consistent with the target that `query` asked for. A target that
// does not report what the query asks about (e.g. one booted without a serial number) cannot
// contradict the query, so it is consistent with it.
fn identity_matches(query: &TargetInfoQuery, identify: &IdentifyHostResponse) -> bool {
    let nodename = identify.nodename.clone().filter(|n| !n.is_empty() && n != DEFAULT_NODENAME);
    let serial =
        identify.serial_number.clone().filter(|s| !s.is_empty() && s != DEFAULT_SERIAL_NUMBER);
    match query {
        TargetInfoQuery::Id(_) if serial.is_none() => true,
        _ if nodename.is_none() && serial.is_none() => true,
        _ => query.match_description(&discovery::Description {
            nodename,
            serial,
            ..Default::default()
        }),
    }
}

// Group the information collected when resolving the address. (This is
// particularly important for the rcs_proxy, which we may need when resolving
// a manual target -- we don't want make an RCS connection just to resolve the
// name, drop it, then re-establish it later.)
#[derive(Debug)]
pub struct Resolution {
    target: ResolutionTarget,
    discovered: Option<TargetHandle>,
    // Set when loaded from the non-strict discovery cache, and cleared once a connection to the
    // target succeeds, so it only marks resolutions that have not been verified against a live
    // target.
    from_cache: AtomicBool,
    // Symbolic query (`id:<serial>` or `<nodename>`) that produced this resolution, used to
    // verify the connected target's IdentifyHost identity. `None` for direct-address and
    // wildcard (`First`) queries.
    expected_query: Option<TargetInfoQuery>,
    connection: Mutex<ConnectionState>,
    rcs_proxy: Mutex<Option<RemoteControlProxy>>,
    identify_host_response: Mutex<Option<IdentifyHostResponse>>,
}

impl Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "Target: {:?}", self.target)?;
        if let Some(discovered) = &self.discovered {
            write!(f, " Discovered: {}", discovered)?;
        }
        Ok(())
    }
}

impl Resolution {
    fn from_target(target: ResolutionTarget) -> Self {
        Self {
            target,
            discovered: None,
            from_cache: AtomicBool::new(false),
            expected_query: None,
            connection: Mutex::new(ConnectionState::Uninitialized),
            rcs_proxy: Mutex::new(None),
            identify_host_response: Mutex::new(None),
        }
    }

    /// Returns `true` if this [`Resolution`] was loaded from the non-strict discovery cache and
    /// has not yet been verified by a successful connection.
    pub fn from_cache(&self) -> bool {
        self.from_cache.load(Ordering::Relaxed)
    }

    fn from_addr(sa: SocketAddr) -> Self {
        let scope_id = if let SocketAddr::V6(addr) = sa { addr.scope_id() } else { 0 };
        let port = match sa.port() {
            0 => TARGET_DEFAULT_PORT,
            p => p,
        };
        let addr = TargetIpAddr::new(sa.ip(), scope_id, port).into();
        Self::from_target(ResolutionTarget::Addr(addr))
    }

    pub fn from_target_handle(
        th: TargetHandle,
    ) -> std::result::Result<Self, crate::FfxTargetCrateError> {
        let target = ResolutionTarget::from_target_handle(&th).ok_or_else(|| {
            crate::error::TargetResolutionError::MissingProductAddress {
                node_name: th.node_name.clone(),
            }
        })?;
        Ok(Self { discovered: Some(th), ..Self::from_target(target) })
    }

    pub fn addr(&self) -> std::result::Result<SocketAddr, crate::FfxTargetCrateError> {
        match self.target {
            ResolutionTarget::Addr(addr) => Ok(addr),
            _ => Err(crate::error::TargetResolutionError::NonNetworkTarget.into()),
        }
    }

    pub fn mock(f: impl Fn() -> Result<Connection> + Send + Sync + 'static) -> Self {
        Self::from_target(ResolutionTarget::TestMock(Box::new(f)))
    }

    pub fn mock_async<F, Fut>(f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Connection>> + Send + 'static,
    {
        Self::from_target(ResolutionTarget::TestMockAsync(Box::new(move || Box::pin(f()))))
    }

    pub async fn set_connection_for_test(&self, connection: Option<Connection>) {
        *self.connection.lock().await = match connection {
            Some(conn) => ConnectionState::Connected(Arc::new(conn)),
            None => ConnectionState::Uninitialized,
        };
    }

    pub fn usb_cid(&self) -> Option<u32> {
        if let ResolutionTarget::Usb(cid) = &self.target { Some(*cid) } else { None }
    }

    pub fn vsock_cid(&self) -> Option<u32> {
        if let ResolutionTarget::Vsock(cid) = &self.target { Some(*cid) } else { None }
    }

    /// Returns the target UART endpoint if this resolution represents a serial connection.
    ///
    /// # Returns
    ///
    /// Returns `Some(endpoint)` containing the UART endpoint string,
    /// or `None` if this target resolves over a network or USB/VSOCK connection.
    pub fn uart_endpoint(&self) -> Option<String> {
        if let ResolutionTarget::Uart(endpoint) = &self.target {
            Some(endpoint.clone())
        } else {
            None
        }
    }

    pub fn target_spec(&self) -> String {
        self.target.to_spec()
    }

    pub async fn is_usable(&self) -> bool {
        // Acquire the lock to read the connection state. Note that this may suspend/block if a
        // connection attempt is currently in progress (which holds the lock). This blocking is
        // correct and desirable: it prevents duplicate, parallel connection attempts by forcing
        // us to wait and see if the in-progress attempt succeeds.
        match &*self.connection.lock().await {
            // We lazily connect to the target (some callers like SshAddrHolder only query the
            // address without connecting). An Uninitialized state is still considered healthy/usable
            // because we haven't failed any connection attempts yet.
            ConnectionState::Uninitialized => {
                log::debug!(
                    "Resolution::is_usable(): connection is uninitialized for target {:?}",
                    self.target
                );
                true
            }
            ConnectionState::Connected(conn) => {
                if conn.is_terminated() {
                    log::debug!(
                        "Resolution::is_usable(): connection is terminated for target {:?}",
                        self.target
                    );
                    false
                } else {
                    log::debug!(
                        "Resolution::is_usable(): connection is active for target {:?}",
                        self.target
                    );
                    true
                }
            }
            // If connection failed, we treat the cache entry as unusable to force eviction and
            // re-resolution. Even if this results in a false positive (e.g. the target recovered
            // at the same address by the time we check), re-resolution is safe and necessary in
            // cases where the target address has changed.
            ConnectionState::Failed => {
                log::debug!(
                    "Resolution::is_usable(): previous connection attempt failed for target {:?}",
                    self.target
                );
                false
            }
        }
    }

    pub async fn get_connection(
        &self,
        context: &EnvironmentContext,
    ) -> std::result::Result<Arc<Connection>, crate::FfxTargetCrateError> {
        // Hold a lock to make sure only one connection is being initialized at a time.
        // Note that this is a tokio Mutex, not a std Mutex, so it's safe to hold across
        // await points.
        let mut conn_guard = self.connection.lock().await;

        if let ConnectionState::Connected(conn) = &*conn_guard
            && !conn.is_terminated()
        {
            return Ok(conn.clone());
        }

        // Transition to `Failed` before awaiting `connect_new` so that if this future is
        // cancelled by a caller's timeout while `connect_new` is pending, the resolution is
        // still marked as unusable (`is_usable() == false`) and will be re-resolved on the
        // next attempt.
        *conn_guard = ConnectionState::Failed;

        let conn_res =
            self.connect_new(context).await.map_err(|err| self.explain_unreachable(err, context));
        match &conn_res {
            Ok(conn) => {
                self.from_cache.store(false, Ordering::Relaxed);
                *conn_guard = ConnectionState::Connected(conn.clone());
            }
            Err(_) => {
                // If a connection attempt fails, invalidate the discovery cache file so subsequent
                // attempts re-discover rather than pinning to a stale or unreachable cached entry.
                if let Err(e) = crate::cache::remove_target_cache(context) {
                    log::debug!("Failed to remove target cache on connection failure: {e:?}");
                }
            }
        }
        conn_res
    }

    // Names the query and the address it was registered at when a strict-mode target can't be
    // reached, since the transport's error shows neither and a stale registry entry is a likely
    // cause.
    fn explain_unreachable(
        &self,
        err: crate::FfxTargetCrateError,
        context: &EnvironmentContext,
    ) -> crate::FfxTargetCrateError {
        match (err, &self.expected_query) {
            (
                crate::FfxTargetCrateError::Knock(crate::KnockError::NonCritical(source)),
                Some(query),
            ) if context.is_strict() => crate::KnockError::NonCritical(
                crate::KnockNonCriticalError::RegisteredTargetUnreachable {
                    query: query.to_string(),
                    target_spec: self.target_spec(),
                    source: Box::new(source),
                },
            )
            .into(),
            (err, _) => err,
        }
    }

    // Checks that the target reached by `conn` is the one that `query` asked for, so that an
    // address which now belongs to a different device is not mistaken for the target.
    async fn verify_identity(
        &self,
        conn: &Connection,
        query: &TargetInfoQuery,
        context: &EnvironmentContext,
    ) -> std::result::Result<(), crate::FfxTargetCrateError> {
        let target_spec = self.target_spec();
        let timeout_ms = context.get(CONFIG_TARGET_SSH_TIMEOUT).unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
        let answer = async {
            // TODO(b/571246542): Cache the RCS proxy established here so `get_rcs_proxy()` reuses
            // it instead of opening a second proxy and doubling `ffx_rcs_proxy` analytics.
            let rcs_proxy = conn.rcs_proxy_fdomain().await.map_err(|e| e.to_string())?;
            rcs_proxy.identify_host().await.map_err(|e| e.to_string())
        }
        .on_timeout(Duration::from_millis(timeout_ms), || Err("timed out".into()))
        .await;
        let identify = match answer {
            Ok(Ok(identify)) => identify,
            Ok(Err(e)) => {
                log::warn!("Cannot verify that {target_spec} is '{query}': IdentifyHost: {e:?}");
                return Ok(());
            }
            // The target did not answer, so report a failed connection, which callers may retry.
            Err(why) => {
                return Err(crate::KnockError::NonCritical(
                    crate::KnockNonCriticalError::RcsKnockFailed {
                        detail: format!("IdentifyHost on {target_spec}: {why}"),
                    },
                )
                .into());
            }
        };
        if !identity_matches(query, &identify) {
            return Err(crate::error::TargetResolutionError::IdentityMismatch {
                expected_query: query.to_string(),
                target_spec,
                actual_nodename: identify.nodename,
                actual_serial: identify.serial_number,
            }
            .into());
        }
        // Save `identify()` a second IdentifyHost call. `identify()` does not hold this lock
        // while it connects, so this only fails if another `identify()` is asking right now.
        if let Ok(mut cached) = self.identify_host_response.try_lock() {
            *cached = Some(identify);
        }
        Ok(())
    }

    async fn connect_new(
        &self,
        context: &EnvironmentContext,
    ) -> std::result::Result<Arc<Connection>, crate::FfxTargetCrateError> {
        let conn = match &self.target {
            ResolutionTarget::Addr(socket_addr) => {
                if !context.get(keys::NETWORK_ENABLED).unwrap_or(true) {
                    return Err(crate::error::TargetResolutionError::NetworkDisabled.into());
                }
                let connector = SshConnector::new(
                    netext::ScopedSocketAddr::from_socket_addr(*socket_addr)?,
                    context,
                )?;
                emit_target_connection_event("SSH").await;
                Connection::new(connector).await.map_err(crate::KnockError::from)?
            }
            ResolutionTarget::Usb(cid) => {
                if !context.get(keys::USB_ENABLED).unwrap_or(true) {
                    return Err(crate::error::TargetResolutionError::UsbDisabled.into());
                }
                let connector = UsbConnector::new(*cid, context).await?;
                emit_target_connection_event("USB").await;
                Connection::new(connector).await.map_err(crate::KnockError::from)?
            }
            ResolutionTarget::Vsock(cid) => {
                if !context.get(keys::VSOCK_ENABLED).unwrap_or(false) {
                    return Err(crate::error::TargetResolutionError::VsockDisabled.into());
                }
                let connector = VSockConnector::new(*cid);
                emit_target_connection_event("VSOCK").await;
                Connection::new(connector).await.map_err(crate::KnockError::from)?
            }
            ResolutionTarget::Uart(endpoint) => {
                let connector = UartConnector::new(endpoint.clone(), context)?;
                emit_target_connection_event("UART").await;
                Connection::new(connector).await.map_err(crate::KnockError::from)?
            }
            ResolutionTarget::TestMock(f) => f()?,
            ResolutionTarget::TestMockAsync(f) => f().await?,
        };

        if let Some(query) = &self.expected_query {
            self.verify_identity(&conn, query, context).await?;
        }
        Ok(Arc::new(conn))
    }

    pub fn get_connection_if_already_established(&self) -> Option<Arc<Connection>> {
        if let Ok(guard) = self.connection.try_lock() {
            match &*guard {
                ConnectionState::Connected(conn) => Some(conn.clone()),
                _ => None,
            }
        } else {
            None
        }
    }

    async fn get_rcs_proxy(
        &self,
        context: &EnvironmentContext,
    ) -> std::result::Result<RemoteControlProxy, crate::FfxTargetCrateError> {
        // Hold a lock to make sure only one RCS proxy is being initialized at a time.
        // Note that this is a tokio Mutex, not a std Mutex, so it's safe to hold across
        // await points.
        let mut rcs_proxy_guard = self.rcs_proxy.lock().await;
        if rcs_proxy_guard.is_none() {
            let conn = self.get_connection(context).await?;
            *rcs_proxy_guard = Some(conn.rcs_proxy_fdomain().await?);
        }
        // Unwrap safety: either the guard was already Some(), or we just initialized it with Some()
        Ok(rcs_proxy_guard.as_ref().unwrap().clone())
    }

    pub async fn identify(
        &self,
        context: &EnvironmentContext,
    ) -> std::result::Result<IdentifyHostResponse, crate::FfxTargetCrateError> {
        if let Some(identify) = self.identify_host_response.lock().await.clone() {
            return Ok(identify);
        }
        // Connect before taking the lock, so that a connection that verifies the target's
        // identity can record the response it got instead of this asking again.
        let rcs_proxy = self.get_rcs_proxy(context).await?;
        // Hold a lock to make sure only one IdentifyHost is being called at a time.
        // Note that this is a tokio Mutex, not a std Mutex, so it's safe to hold across
        // await points.
        let mut identify_guard = self.identify_host_response.lock().await;
        if identify_guard.is_none() {
            *identify_guard = Some(
                rcs_proxy
                    .identify_host()
                    .await?
                    .map_err(crate::FfxTargetCrateError::IdentifyHost)?,
            );
        }
        // Unwrap safety: either the guard was already Some(), or we just initialized it with Some()
        Ok(identify_guard.as_ref().unwrap().clone())
    }

    pub async fn get_target_info(
        &self,
        addr: TargetAddr,
        context: &EnvironmentContext,
    ) -> Result<TargetInfo> {
        let identify = self.identify(context).await?;
        // This is only called in a situation where we started from an address, so we're
        // going to only return that address. The problem is that we can't really trust
        // the result of IdentifyHost: its "addresses" don't include the port.
        Ok(TargetInfo {
            nodename: identify.nodename,
            addresses: vec![addr],
            // If we could Identify, then clearly RCS is up
            rcs_state: info::RemoteControlState::Up,
            target_state: info::TargetState::Product,
            product_config: identify.product_config,
            board_config: identify.board_config,
            serial_number: identify.serial_number,
            // "is_manual" is not available without discovery, but manual targets are going away, so this is reasonable
            is_manual: false,
            boot_id: identify.boot_id,
            is_default: None,
        })
    }
}

async fn emit_target_connection_event(ty: &str) {
    let _ = analytics::add_custom_event(
        Some("ffx_target_connection"),
        Some(ty),
        None,
        [].into_iter().collect(),
    )
    .await;
}

async fn emit_cache_event(status: &'static str, query_type: &str) {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("status", analytics::GA4Value::from(status));
    fields.insert("query_type", analytics::GA4Value::from(query_type));
    let _ =
        analytics::add_custom_event(Some("ffx_target_resolution_cache"), None, None, fields).await;
}

impl Resolution {
    pub async fn try_from_env_context_with_resolver<R: TargetResolver>(
        resolver: &R,
        env: &EnvironmentContext,
        use_cache: bool,
    ) -> ffx_command_error::Result<Self> {
        let (target_spec, source) = get_target_specifier_with_source(env)?;
        if env.is_strict() && target_spec.is_none() {
            return Err(user_error!(
                "You must specify a target via `-t <target_name>` before any command arguments"
            )
            .into());
        }
        let spec: TargetInfoQuery = TargetInfoQuery::try_from(target_spec)
            .map_err(|e| user_error!("Invalid target specifier: {}", e))?;

        let resolution = resolver
            .resolve_target_address(&spec, use_cache, env, source)
            .await
            .map_err(|e| crate::FfxTargetCrateError::from(e).into_command_error())?;
        Ok(resolution)
    }

    pub async fn try_from_env_context_with_cache(
        env: &EnvironmentContext,
        use_cache: bool,
    ) -> ffx_command_error::Result<Self> {
        let discovery = build_discovery_from_config(env);
        let resolver = DefaultTargetResolver::new(discovery);
        Self::try_from_env_context_with_resolver(&resolver, env, use_cache).await
    }
}

impl TryFromEnvContext for Resolution {
    fn try_from_env_context<'a>(
        env: &'a EnvironmentContext,
    ) -> LocalBoxFuture<'a, ffx_command_error::Result<Self>> {
        Box::pin(async { Self::try_from_env_context_with_cache(env, true).await })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_config::environment::ExecutableKind;
    use fidl_fuchsia_developer_remotecontrol as rcs_fidl;
    use std::cmp::Ordering;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

    #[fuchsia::test]
    async fn test_sort_socket_addrs() {
        struct TestCase {
            a1: SocketAddr,
            a2: SocketAddr,
            want_res: Ordering,
            name: String,
        }

        let cases: Vec<TestCase> = vec![
            TestCase {
                a1: "127.0.0.1:8082".parse().expect("Valid SocketAddr"),
                a2: "[::1]:8082".parse().expect("Valid SocketAddr"),
                want_res: Ordering::Equal,
                name: "Loopback v4 and v6 equal".to_string(),
            },
            TestCase {
                a1: "192.168.1.1:8082".parse().expect("Valid SocketAddr"),
                a2: "[2002:2::%2]:8082".parse().expect("Valid SocketAddr"),
                want_res: Ordering::Equal,
                name: "Non local v4 and non local v6 local equal".to_string(),
            },
            TestCase {
                // Local
                a1: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                // Non Local
                a2: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0x2607, 0xf8b0, 0x4005, 0x805, 0, 0, 0, 0x200e),
                    8080,
                    0,
                    1,
                )),
                want_res: Ordering::Less,
                name: "local v6 and non v6 local less".to_string(),
            },
            TestCase {
                // Local
                a1: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                // Local
                a2: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0xfe80, 0, 0, 0, 2, 6, 7, 8),
                    8080,
                    0,
                    1,
                )),
                want_res: Ordering::Equal,
                name: "local v6 and local v6 local equal".to_string(),
            },
        ];

        for case in cases {
            let got = TargetAddr::compare_socket_addrs_by_priority(&case.a1, &case.a2);
            assert_eq!(
                got, case.want_res,
                "TestCase: {0}. Results differ: {1:?}, {2:?}",
                case.name, got, case.want_res
            );
        }
    }

    fn get_addr_and_spec() -> (SocketAddr, TargetInfoQuery) {
        let addr = "127.0.0.1:123".to_string();
        let addr_spec =
            TargetInfoQuery::Addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 123));
        let sa = addr.parse::<SocketAddr>().unwrap();
        (sa, addr_spec)
    }

    fn make_target_handle_for_product(name: &str, sa: SocketAddr) -> TargetHandle {
        let state = discovery::TargetState::Product { addrs: vec![sa.into()], serial: None };
        let th = TargetHandle { node_name: Some(String::from(name)), state, manual: false };
        th
    }

    #[fuchsia::test]
    async fn test_expect_single_target_empty() {
        let query = TargetInfoQuery::NodenameOrId("foo".to_string());
        let handles: Vec<TargetHandle> = vec![];
        let res = expect_single_target(&query, handles, None);
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::TargetNotFound, .. }
        ));
    }

    #[fuchsia::test]
    async fn test_expect_single_target_single() {
        let query = TargetInfoQuery::NodenameOrId("foo".to_string());
        let handle = make_target_handle_for_product("foo", "127.0.0.1:8080".parse().unwrap());
        let handles = vec![handle.clone()];
        let res = expect_single_target(&query, handles, None);
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), handle);
    }

    #[fuchsia::test]
    async fn test_expect_single_target_multiple() {
        let query = TargetInfoQuery::NodenameOrId("foo".to_string());
        let handle1 = make_target_handle_for_product("foo", "127.0.0.1:8080".parse().unwrap());
        let handle2 = make_target_handle_for_product("bar", "127.0.0.1:8081".parse().unwrap());
        let handles = vec![handle1, handle2];
        let res = expect_single_target(&query, handles, None);
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::QueryAmbiguous, .. }
        ));
    }

    #[fuchsia::test]
    async fn test_get_target_info_serial() {
        let test_env = ffx_config::test_init().unwrap();
        let sa: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let addr = TargetAddr::Net(sa.clone());
        let resolution = Resolution::from_addr(sa);
        let mut identify = IdentifyHostResponse::default();
        identify.nodename = Some("test-node".to_string());
        identify.serial_number = Some("test_serial_123".to_string());
        *resolution.identify_host_response.lock().await = Some(identify);

        let info = resolution.get_target_info(addr, &test_env.context).await.unwrap();

        assert_eq!(info.serial_number, Some("test_serial_123".to_string()));
    }

    #[fuchsia::test]
    async fn test_resolution_tracks_failed_connection() {
        let test_env = ffx_config::test_init().unwrap();
        let resolution = Resolution::from_target(ResolutionTarget::TestMock(Box::new(|| {
            Err(anyhow::anyhow!("Mock connection failure"))
        })));

        // Initially it is usable
        assert!(resolution.is_usable().await);

        // Attempt to connect, which fails
        let res = resolution.get_connection(&test_env.context).await;
        assert!(res.is_err());

        // Now it should not be usable
        assert!(!resolution.is_usable().await);
    }

    #[fuchsia::test]
    async fn test_successful_connection_clears_from_cache() {
        #[derive(Debug)]
        struct PlaceholderConnector;
        impl crate::TransportConnector for PlaceholderConnector {
            const CONNECTION_TYPE: &'static str = "placeholder";
            async fn connect(
                &mut self,
            ) -> Result<crate::TargetConnection, crate::TargetConnectionError> {
                Ok(crate::TargetConnection::FDomain(crate::FDomainConnection::invalid()))
            }
        }

        let test_env = ffx_config::test_env().build().unwrap();
        let conn =
            std::sync::Mutex::new(Some(Connection::new(PlaceholderConnector).await.unwrap()));
        let resolution = Resolution::mock(move || Ok(conn.lock().unwrap().take().unwrap()));
        resolution.from_cache.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(resolution.from_cache());

        resolution.get_connection(&test_env.context).await.unwrap();
        assert!(!resolution.from_cache());
    }

    #[fuchsia::test]
    async fn test_cache_miss_falls_back_to_discovery() {
        use crate::cache::{Cache, get_discovery_cache_file};

        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir_path = temp_dir.path().to_path_buf();

        let test_env = ffx_config::test_env()
            .user_config(
                ffx_config::keys::DISCOVERY_CACHE_DIR_CONFIG,
                serde_json::json!(cache_dir_path.to_str().unwrap().to_string()),
            )
            .build()
            .unwrap();

        // Write an empty cache file (contains no targets)
        let cache_file = get_discovery_cache_file(&test_env.context).unwrap();
        let cache = Cache::new(vec![]);
        cache.save(&cache_file).unwrap();

        let mut resolver = MockTargetResolver::new();
        let name = "foobar".to_string();
        let name_spec = TargetInfoQuery::NodenameOrId(name.clone());
        let (sa, _) = get_addr_and_spec();
        let th = make_target_handle_for_product(&name, sa);

        // Even though cache exists (empty), because of cache miss, it must fall back to manual and discovery queries:
        resolver.expect_try_resolve_manual_target().return_once(move |_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(move |_| Ok(vec![th]));

        let resolution = resolver
            .resolve_single_target(&name_spec, true, &test_env.context, Some(TargetSource::Default))
            .await
            .unwrap();
        assert_eq!(resolution.addr().unwrap(), sa);
    }

    #[fuchsia::test]
    async fn test_cache_hit_bypasses_discovery() {
        use crate::cache::{Cache, get_discovery_cache_file};

        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir_path = temp_dir.path().to_path_buf();

        let test_env = ffx_config::test_env()
            .user_config(
                ffx_config::keys::DISCOVERY_CACHE_DIR_CONFIG,
                serde_json::json!(cache_dir_path.to_str().unwrap().to_string()),
            )
            .build()
            .unwrap();

        // Write a cache file containing our target info!
        let cache_file = get_discovery_cache_file(&test_env.context).unwrap();
        let (sa, _) = get_addr_and_spec();

        let mut target_info = crate::TargetInfo::default();
        target_info.nodename = Some("foobar".to_string());
        target_info.target_state = info::TargetState::Product;
        target_info.addresses = vec![sa.into()];
        let cache = Cache::new(vec![target_info]);
        cache.save(&cache_file).unwrap();

        let resolver = MockTargetResolver::new();
        let name_spec = TargetInfoQuery::NodenameOrId("foobar".to_string());

        // Note: we do NOT mock expect_try_resolve_manual_target or expect_discovered_targets.
        // If the resolver attempts to call them, MockTargetResolver will panic,
        // validating that discovery was indeed bypassed on cache hit.

        let resolution = resolver
            .resolve_single_target(&name_spec, true, &test_env.context, Some(TargetSource::Default))
            .await
            .unwrap();
        assert_eq!(resolution.addr().unwrap(), sa);
    }

    // Tests that `build_discovery_from_config` reads and applies the discovery timeout from the
    // configuration key (`discovery.timeout`), in addition to filtering out fastboot sources.
    #[fuchsia::test]
    async fn test_build_discovery_from_config_reads_timeout_from_config() {
        let test_env = ffx_config::test_env()
            .user_config(ffx_config::keys::DISCOVERY_TIMEOUT_MS, 2500)
            .build()
            .unwrap();
        let discovery = build_discovery_from_config(&test_env.context);
        let sources = discovery.sources();
        assert!(!sources.contains(DiscoverySources::USB_FASTBOOT));
        assert!(!sources.contains(DiscoverySources::FASTBOOT_FILE));
        assert_eq!(discovery.timeout(), Some(Duration::from_millis(2500)));
    }

    // Tests that `build_discovery_from_config_with_timeout` overrides any timeout configured
    // in the environment context with the explicitly requested timeout (e.g. `None` for indefinite discovery),
    // and that `DefaultTargetResolver::set_discovery_timeout` can dynamically update the timeout afterwards.
    #[fuchsia::test]
    async fn test_build_discovery_from_config_maybe_indef() {
        let test_env = ffx_config::test_env()
            .user_config(ffx_config::keys::DISCOVERY_TIMEOUT_MS, 2500)
            .build()
            .unwrap();
        let discovery = build_discovery_builder_common(&test_env.context)
            .with_timeout_msecs(None)
            .build(&test_env.context);
        let sources = discovery.sources();
        assert!(!sources.contains(DiscoverySources::USB_FASTBOOT));
        assert!(!sources.contains(DiscoverySources::FASTBOOT_FILE));
        // Overrides configured 2500ms with None (indefinite)
        assert_eq!(discovery.timeout(), None);

        let mut resolver = DefaultTargetResolver::new(discovery);
        assert_eq!(resolver.discovery_timeout(), None);

        resolver.set_discovery_timeout(Some(Duration::from_millis(100)));
        assert_eq!(resolver.discovery_timeout(), Some(Duration::from_millis(100)));
    }

    #[fuchsia::test]
    async fn test_connection_failure_removes_discovery_cache() {
        use crate::cache::{Cache, get_discovery_cache_file};

        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir_path = temp_dir.path().to_path_buf();

        let test_env = ffx_config::test_env()
            .user_config(
                ffx_config::keys::DISCOVERY_CACHE_DIR_CONFIG,
                serde_json::json!(cache_dir_path.to_str().unwrap()),
            )
            .build()
            .unwrap();

        // Write a cache file containing target info
        let cache_file = get_discovery_cache_file(&test_env.context).unwrap();
        let sa: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut target_info = crate::TargetInfo::default();
        target_info.nodename = Some("test-target".to_string());
        target_info.target_state = info::TargetState::Product;
        target_info.addresses = vec![sa.into()];
        let cache = Cache::new(vec![target_info]);
        cache.save(&cache_file).unwrap();

        assert!(cache_file.exists());

        // Create a Resolution that fails to connect
        let resolution = Resolution::from_target(ResolutionTarget::TestMock(Box::new(|| {
            Err(anyhow::anyhow!("Connection error"))
        })));

        let res = resolution.get_connection(&test_env.context).await;
        assert!(res.is_err());

        // Cache file should now have been removed
        assert!(!cache_file.exists());
    }

    #[fuchsia::test]
    async fn test_resolve_target_address_explicit_usb() {
        let test_env = ffx_config::test_init().unwrap();
        let resolver = MockTargetResolver::new();
        let usb_spec = TargetInfoQuery::Usb(42);
        // Note: MockTargetResolver has no expectations configured.
        // If resolution falls back to discovery, this test will panic,
        // verifying that explicit USB resolution returns immediately.
        let res = resolver
            .resolve_target_address(&usb_spec, false, &test_env.context, None)
            .await
            .unwrap();
        assert_eq!(res.usb_cid(), Some(42));
        assert_eq!(res.vsock_cid(), None);
        assert_eq!(res.target_spec(), "usb:cid:42");
        assert!(matches!(
            res.addr().unwrap_err(),
            crate::FfxTargetCrateError::Resolution(
                crate::error::TargetResolutionError::NonNetworkTarget
            )
        ));
    }

    #[fuchsia::test]
    async fn test_resolve_target_address_explicit_vsock() {
        let test_env = ffx_config::test_init().unwrap();
        let resolver = MockTargetResolver::new();
        let vsock_spec = TargetInfoQuery::VSock(12345);
        // Note: MockTargetResolver has no expectations configured.
        // If resolution falls back to discovery, this test will panic,
        // verifying that explicit VSOCK resolution returns immediately.
        let res = resolver
            .resolve_target_address(&vsock_spec, false, &test_env.context, None)
            .await
            .unwrap();
        assert_eq!(res.vsock_cid(), Some(12345));
        assert_eq!(res.usb_cid(), None);
        assert_eq!(res.target_spec(), "vsock:cid:12345");
        assert!(matches!(
            res.addr().unwrap_err(),
            crate::FfxTargetCrateError::Resolution(
                crate::error::TargetResolutionError::NonNetworkTarget
            )
        ));
    }

    #[fuchsia::test]
    async fn test_resolve_target_address_explicit_addr() {
        let test_env = ffx_config::test_init().unwrap();
        let resolver = MockTargetResolver::new();
        let (sa, addr_spec) = get_addr_and_spec();
        // Note: MockTargetResolver has no expectations configured.
        let res = resolver
            .resolve_target_address(&addr_spec, false, &test_env.context, None)
            .await
            .unwrap();
        assert_eq!(res.addr().unwrap(), sa);
        assert_eq!(res.usb_cid(), None);
        assert_eq!(res.vsock_cid(), None);
        assert_eq!(res.target_spec(), format!("{sa}"));
    }

    #[fuchsia::test]
    async fn test_resolution_try_from_env_context_usb_and_vsock() {
        let env = ffx_config::test_init().unwrap();
        let resolver = MockTargetResolver::new();

        let mut usb_ctx = env.context.clone();
        usb_ctx.override_target_specifier(&Some("usb:cid:42".to_string()));
        let res_usb = Resolution::try_from_env_context_with_resolver(&resolver, &usb_ctx, false)
            .await
            .unwrap();
        assert_eq!(res_usb.usb_cid(), Some(42));
        assert_eq!(res_usb.vsock_cid(), None);

        let mut vsock_ctx = env.context.clone();
        vsock_ctx.override_target_specifier(&Some("vsock:cid:12345".to_string()));
        let res_vsock =
            Resolution::try_from_env_context_with_resolver(&resolver, &vsock_ctx, false)
                .await
                .unwrap();
        assert_eq!(res_vsock.vsock_cid(), Some(12345));
        assert_eq!(res_vsock.usb_cid(), None);
    }

    #[fuchsia::test]
    async fn test_resolve_invalid_uart() {
        let test_env = ffx_config::test_env().build().unwrap();
        // 1. Relative path (not allowed)
        let err =
            UartConnector::new("./my-relative-port".to_string(), &test_env.context).unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::TargetNotFound, .. }
        ));

        // 2. Non-existent absolute path
        let err =
            UartConnector::new("/non/existent/port".to_string(), &test_env.context).unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::TargetNotFound, .. }
        ));

        // 3. Regular file (invalid type)
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let err =
            UartConnector::new(temp_file.path().to_string_lossy().to_string(), &test_env.context)
                .unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::TargetNotFound, .. }
        ));
    }

    fn assert_open_target_error(err: ffx_command_error::Error, expected: ffx::OpenTargetError) {
        let ffx_command_error::Error::User(e) = err else {
            panic!("Expected Error::User, got {err:?}");
        };
        let Some(errors::FfxError::OpenTargetError { err, .. }) =
            e.downcast_ref::<errors::FfxError>()
        else {
            panic!("Expected FfxError::OpenTargetError, got {e:?}");
        };
        let Some(target_errors::FfxTargetError::OpenTargetError { err: actual, .. }) =
            err.downcast_ref::<target_errors::FfxTargetError>()
        else {
            panic!("Expected FfxTargetError::OpenTargetError, got {err:?}");
        };
        assert_eq!(*actual, expected);
    }

    #[fuchsia::test]
    async fn test_try_from_env_context_target_not_found() {
        let mut env = ffx_config::test_init().unwrap();
        let mut resolver = MockTargetResolver::new();
        resolver.expect_try_resolve_manual_target().return_once(|_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(|_| Ok(vec![]));

        env.context.override_target_specifier(&Some("foo".to_string()));

        let err = Resolution::try_from_env_context_with_resolver(&resolver, &env.context, false)
            .await
            .unwrap_err();
        assert_open_target_error(err, ffx::OpenTargetError::TargetNotFound);
    }

    #[fuchsia::test]
    async fn test_try_from_env_context_query_ambiguous() {
        let mut env = ffx_config::test_init().unwrap();
        let mut resolver = MockTargetResolver::new();
        resolver.expect_try_resolve_manual_target().return_once(|_, _| Ok(None));
        resolver.expect_discovered_targets().return_once(|_| {
            Ok(vec![
                make_target_handle_for_product("foo", "127.0.0.1:8080".parse().unwrap()),
                make_target_handle_for_product("foo", "127.0.0.1:8081".parse().unwrap()),
            ])
        });

        env.context.override_target_specifier(&Some("foo".to_string()));

        let err = Resolution::try_from_env_context_with_resolver(&resolver, &env.context, false)
            .await
            .unwrap_err();
        assert_open_target_error(err, ffx::OpenTargetError::QueryAmbiguous);
    }

    #[fuchsia::test]
    async fn test_try_from_env_context_explicit_address() {
        let mut env = ffx_config::test_init().unwrap();
        // Skip discovery, because explicit address is used, MockTargetResolver expectations will
        // panic if methods are called.
        let resolver = MockTargetResolver::new();

        env.context.override_target_specifier(&Some("127.0.0.1:8082".to_string()));

        let res = Resolution::try_from_env_context_with_resolver(&resolver, &env.context, false)
            .await
            .unwrap();

        assert_eq!(res.addr().unwrap(), "127.0.0.1:8082".parse::<SocketAddr>().unwrap());
    }

    #[fuchsia::test]
    async fn test_strict_resolves_id_and_nodename_from_targets_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_map =
            [("shared_data".to_owned(), serde_json::json!(temp_dir.path().to_str().unwrap()))]
                .into_iter()
                .collect();
        let strict_ctx = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();

        let sa: SocketAddr = "192.168.1.50:8022".parse().unwrap();
        let mut target_info = crate::TargetInfo::default();
        target_info.nodename = Some("fuchsia-node".to_string());
        target_info.serial_number = Some("04012C12D45E".to_string());
        target_info.target_state = info::TargetState::Product;
        target_info.addresses = vec![sa.into()];
        crate::registry::register_targets(&strict_ctx, &[target_info]).unwrap();

        let resolver = MockTargetResolver::new();

        // 1. Resolve by `id:<serial-number>` with `use_cache = false` (simulating reconnect in strict mode).
        let id_query = TargetInfoQuery::try_from("id:04012C12D45E").unwrap();
        let res_id =
            resolver.resolve_single_target(&id_query, false, &strict_ctx, None).await.unwrap();
        assert_eq!(res_id.addr().unwrap(), sa);

        // 2. Resolve by bare nodename in strict mode.
        let name_query = TargetInfoQuery::try_from("fuchsia-node").unwrap();
        let res_name =
            resolver.resolve_single_target(&name_query, false, &strict_ctx, None).await.unwrap();
        assert_eq!(res_name.addr().unwrap(), sa);
    }

    #[fuchsia::test]
    async fn test_strict_miss_fails_without_discovery_or_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let shared_dir = temp_dir.path().join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        let config_map = [
            ("shared_data".to_owned(), serde_json::json!(shared_dir.to_str().unwrap())),
            (
                "target".to_owned(),
                serde_json::json!({"discovery_cache_dir": cache_dir.to_str().unwrap()}),
            ),
        ]
        .into_iter()
        .collect();
        let strict_ctx = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();

        // Populate the interactive discovery cache, and write an empty $SHARED_DATA/targets.json.
        let cache_file = crate::cache::get_discovery_cache_file(&strict_ctx).unwrap();
        let (sa, _) = get_addr_and_spec();
        let mut target_info = crate::TargetInfo::default();
        target_info.nodename = Some("cached-only-node".to_string());
        target_info.target_state = info::TargetState::Product;
        target_info.addresses = vec![sa.into()];
        crate::cache::Cache::new(vec![target_info]).save(&cache_file).unwrap();
        crate::registry::register_targets(&strict_ctx, &[]).unwrap();

        // MockTargetResolver has no expectations: if strict mode attempts active discovery, it will panic.
        let resolver = MockTargetResolver::new();
        let query = TargetInfoQuery::try_from("cached-only-node").unwrap();
        let err =
            resolver.resolve_single_target(&query, true, &strict_ctx, None).await.unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::TargetNotFound, .. }
        ));
    }

    #[fuchsia::test]
    async fn test_strict_missing_targets_file_falls_back_to_discovery_bypassing_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let shared_dir = temp_dir.path().join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        let config_map = [
            ("shared_data".to_owned(), serde_json::json!(shared_dir.to_str().unwrap())),
            (
                "target".to_owned(),
                serde_json::json!({"discovery_cache_dir": cache_dir.to_str().unwrap()}),
            ),
        ]
        .into_iter()
        .collect();
        let strict_ctx = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();

        // Populate the interactive discovery cache with a stale address, while leaving
        // $SHARED_DATA/targets.json missing.
        let cache_file = crate::cache::get_discovery_cache_file(&strict_ctx).unwrap();
        let stale_sa: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let mut cached_info = crate::TargetInfo::default();
        cached_info.nodename = Some("usb-node".to_string());
        cached_info.serial_number = Some("c86314704820".to_string());
        cached_info.target_state = info::TargetState::Product;
        cached_info.addresses = vec![stale_sa.into()];
        crate::cache::Cache::new(vec![cached_info]).save(&cache_file).unwrap();

        let discovered_sa: SocketAddr = "127.0.0.1:8022".parse().unwrap();
        let th = make_target_handle_for_product("usb-node", discovered_sa);
        let mut resolver = MockTargetResolver::new();
        resolver.expect_discovered_targets().return_once(move |_| Ok(vec![th]));

        let query = TargetInfoQuery::try_from("id:c86314704820").unwrap();
        let res = resolver.resolve_single_target(&query, true, &strict_ctx, None).await.unwrap();
        assert_eq!(res.addr().unwrap(), discovered_sa);
    }

    #[fuchsia::test]
    async fn test_registered_targets_duplicate_id_returns_ambiguous() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_map =
            [("shared_data".to_owned(), serde_json::json!(temp_dir.path().to_str().unwrap()))]
                .into_iter()
                .collect();
        let strict_ctx = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();

        let mut t1 = crate::TargetInfo::default();
        t1.nodename = Some("node-1".to_string());
        t1.serial_number = Some("DUP_ID".to_string());
        t1.target_state = info::TargetState::Product;
        t1.addresses = vec!["192.168.1.10:22".parse::<SocketAddr>().unwrap().into()];

        let mut t2 = crate::TargetInfo::default();
        t2.nodename = Some("node-2".to_string());
        t2.serial_number = Some("DUP_ID".to_string());
        t2.target_state = info::TargetState::Product;
        t2.addresses = vec!["192.168.1.11:22".parse::<SocketAddr>().unwrap().into()];

        crate::registry::register_targets(&strict_ctx, &[t1, t2]).unwrap();

        let resolver = MockTargetResolver::new();
        let query = TargetInfoQuery::try_from("id:DUP_ID").unwrap();
        let err =
            resolver.resolve_single_target(&query, true, &strict_ctx, None).await.unwrap_err();
        assert!(matches!(
            err,
            FfxTargetError::OpenTargetError { err: ffx::OpenTargetError::QueryAmbiguous, .. }
        ));
    }

    #[fuchsia::test]
    async fn test_get_connection_timeout_cancellation_marks_resolution_unusable() {
        let test_env = ffx_config::test_env().build().unwrap();
        let resolution =
            Resolution::mock_async(|| futures::future::pending::<Result<Connection>>());
        assert!(resolution.is_usable().await);

        let res = resolution
            .get_connection(&test_env.context)
            .on_timeout(Duration::from_millis(10), || Err(anyhow::anyhow!("timed out").into()))
            .await;
        assert!(res.is_err());
        assert!(
            !resolution.is_usable().await,
            "Resolution should be marked unusable if get_connection is cancelled by a timeout"
        );
    }
    #[fuchsia::test]
    async fn test_usb_connect_error_to_non_fatal_command_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let missing_socket = temp_dir.path().join("missing_usb.sock");
        let test_env = ffx_config::test_env()
            .user_config(keys::USB_ENABLED, serde_json::json!(true))
            .user_config("connectivity.usb_driver_autostart", serde_json::json!(false))
            .user_config(
                usb_driver_api::CONFIG_USB_SOCKET_PATH,
                serde_json::json!(missing_socket.to_string_lossy()),
            )
            .build()
            .unwrap();
        let handle = TargetHandle {
            node_name: Some("usb-target".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![addr::TargetAddr::UsbCtx(4)],
                serial: None,
            },
            manual: false,
        };
        let resolution = Resolution::from_target_handle(handle).unwrap();
        let err = resolution.get_connection(&test_env.context).await.unwrap_err();
        assert!(err.into_command_error().downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_lookup_discovery_cache_skips_non_product_targets_as_miss() {
        let dir = tempfile::tempdir().unwrap();
        let test_env = ffx_config::test_env()
            .user_config(
                ffx_config::keys::DISCOVERY_CACHE_DIR_CONFIG,
                serde_json::json!(dir.path().to_str().unwrap()),
            )
            .build()
            .unwrap();
        let cache_file = crate::cache::get_discovery_cache_file(&test_env.context).unwrap();

        let mut fastboot_target = TargetInfo::default();
        fastboot_target.nodename = Some("fb-node".to_string());
        fastboot_target.target_state = info::TargetState::Fastboot;
        fastboot_target.addresses = vec!["192.168.1.50:22".parse::<SocketAddr>().unwrap().into()];
        crate::cache::Cache::new(vec![fastboot_target]).save(&cache_file).unwrap();

        let query = TargetInfoQuery::try_from("fb-node").unwrap();
        assert!(lookup_discovery_cache(&query, true, &test_env.context).await.is_none());
    }

    #[fuchsia::test]
    async fn test_short_circuit_stream_waits_for_all_enabled_transports_on_nodename() {
        let mdns_addr: TargetAddr = "[fe80::1%1]:22".parse().unwrap();
        let usb_addr = TargetAddr::UsbCtx(3);
        let mdns_handle = TargetHandle {
            node_name: Some("target-a".to_string()),
            state: discovery::TargetState::Product { addrs: vec![mdns_addr.clone()], serial: None },
            manual: false,
        };
        let usb_handle = TargetHandle {
            node_name: Some("target-a".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![usb_addr.clone()],
                serial: Some("SER123".to_string()),
            },
            manual: false,
        };
        // Add a trailing event after both transports have reported; the stream should
        // short-circuit as soon as both MDNS and USB_VSOCK have emitted a handle.
        let trailing_handle = TargetHandle {
            node_name: Some("target-a".to_string()),
            state: discovery::TargetState::Product { addrs: vec![mdns_addr], serial: None },
            manual: false,
        };
        let inner_stream = futures::stream::iter(vec![
            TargetEvent::Added(mdns_handle.clone()),
            TargetEvent::Added(usb_handle.clone()),
            TargetEvent::Added(trailing_handle),
        ]);
        let sources = DiscoverySources::MDNS | DiscoverySources::USB_VSOCK;
        let query = TargetInfoQuery::NodenameOrId("target-a".to_string());
        let results: Vec<TargetHandle> =
            short_circuit_stream_by_sources(inner_stream, query, sources).collect().await;
        assert_eq!(results, vec![mdns_handle, usb_handle]);
    }

    #[fuchsia::test]
    async fn test_short_circuit_stream_single_transport_short_circuits_immediately() {
        let usb_handle1 = TargetHandle {
            node_name: Some("target-a".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![TargetAddr::UsbCtx(3)],
                serial: Some("SER123".to_string()),
            },
            manual: false,
        };
        let usb_handle2 = TargetHandle {
            node_name: Some("target-a".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![TargetAddr::UsbCtx(4)],
                serial: Some("SER123".to_string()),
            },
            manual: false,
        };
        let inner_stream = futures::stream::iter(vec![
            TargetEvent::Added(usb_handle1.clone()),
            TargetEvent::Added(usb_handle2),
        ]);
        let sources = DiscoverySources::USB_VSOCK;
        let query = TargetInfoQuery::NodenameOrId("target-a".to_string());
        let results: Vec<TargetHandle> =
            short_circuit_stream_by_sources(inner_stream, query, sources).collect().await;
        assert_eq!(results, vec![usb_handle1]);
    }

    #[fuchsia::test]
    async fn test_discover_matching_targets_waits_for_manual_target_when_discovery_empty() {
        struct DelayedManualTargetResolver {
            manual_addr: SocketAddr,
        }

        impl TargetResolver for DelayedManualTargetResolver {
            async fn discovered_targets(
                &self,
                _query: TargetInfoQuery,
            ) -> Result<Vec<TargetHandle>> {
                Ok(vec![])
            }

            async fn try_resolve_manual_target(
                &self,
                name: &str,
                _ctx: &EnvironmentContext,
            ) -> Result<Option<Resolution>> {
                fuchsia_async::Timer::new(Duration::from_millis(10)).await;
                if name == "manual-node" {
                    Ok(Some(Resolution::from_addr(self.manual_addr)))
                } else {
                    Ok(None)
                }
            }
        }

        let test_env = ffx_config::test_env().build().unwrap();
        let sa: SocketAddr = "10.128.16.178:22".parse().unwrap();
        let resolver = DelayedManualTargetResolver { manual_addr: sa };
        let query = TargetInfoQuery::NodenameOrId("manual-node".to_string());
        let res =
            resolver.resolve_single_target(&query, false, &test_env.context, None).await.unwrap();
        assert_eq!(res.addr().unwrap(), sa);
    }

    type FakeIdentity =
        std::result::Result<rcs_fidl::IdentifyHostResponse, rcs_fidl::IdentifyHostError>;

    #[derive(Clone)]
    enum FakeIdentify {
        Respond(FakeIdentity),
        DropResponder,
        Stall,
    }

    fn fake_identity(nodename: &str, serial: &str) -> FakeIdentify {
        FakeIdentify::Respond(Ok(rcs_fidl::IdentifyHostResponse {
            nodename: Some(nodename.into()),
            serial_number: Some(serial.into()),
            ..Default::default()
        }))
    }

    // Returns a resolution of `query` whose target is a fake RCS that answers IdentifyHost
    // according to `identify`, and the number of IdentifyHost calls the fake has received.
    fn fake_rcs_resolution(
        query: Option<&str>,
        identify: FakeIdentify,
    ) -> (Resolution, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let client = fdomain_local::local_client(move || {
            let (namespace, mut opens) =
                fidl::endpoints::create_request_stream::<fidl_fuchsia_io::DirectoryMarker>();
            let (identify, counter) = (identify.clone(), Arc::clone(&counter));
            fuchsia_async::Task::local(async move {
                let mut stalled = Vec::new();
                while let Some(Ok(fidl_fuchsia_io::DirectoryRequest::Open { object, .. })) =
                    opens.next().await
                {
                    let mut requests =
                        fidl::endpoints::ServerEnd::<rcs_fidl::RemoteControlMarker>::new(object)
                            .into_stream();
                    while let Some(Ok(rcs_fidl::RemoteControlRequest::IdentifyHost { responder })) =
                        requests.next().await
                    {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        match &identify {
                            FakeIdentify::Respond(res) => {
                                responder.send(res.as_ref().map_err(|e| *e)).unwrap();
                            }
                            FakeIdentify::DropResponder => {}
                            FakeIdentify::Stall => stalled.push(responder),
                        }
                    }
                }
            })
            .detach();
            Ok(namespace)
        });
        let conn = std::sync::Mutex::new(Some(Connection::from_fdomain_client(client)));
        let mut resolution = Resolution::mock(move || Ok(conn.lock().unwrap().take().unwrap()));
        resolution.expected_query = query.map(|q| TargetInfoQuery::try_from(q).unwrap());
        (resolution, calls)
    }

    // Connects a resolution from `fake_rcs_resolution`.
    async fn connect_to_fake_rcs(
        context: &EnvironmentContext,
        query: Option<&str>,
        identity: FakeIdentify,
    ) -> (
        Resolution,
        std::result::Result<Arc<Connection>, crate::FfxTargetCrateError>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (resolution, calls) = fake_rcs_resolution(query, identity);
        let res = resolution.get_connection(context).await;
        (resolution, res, calls)
    }

    #[fuchsia::test]
    async fn test_identity_matches() {
        let identify = |nodename: Option<&str>, serial: Option<&str>| IdentifyHostResponse {
            nodename: nodename.map(Into::into),
            serial_number: serial.map(Into::into),
            ..Default::default()
        };
        let defaults = identify(Some(DEFAULT_NODENAME), Some(DEFAULT_SERIAL_NUMBER));
        let empty = identify(Some(""), Some(""));
        for (query, identify, want) in [
            ("id:SER123", identify(Some("my-node"), Some("SER123")), true),
            ("id:SER123", identify(Some("my-node"), Some("SER999")), false),
            ("id:SER123", identify(Some("my-node"), None), true),
            ("id:SER123", identify(Some("my-node"), Some("")), true),
            ("id:SER123", defaults.clone(), true),
            ("my-node", identify(Some("my-node"), Some("SER123")), true),
            ("my-node", identify(Some("other-node"), None), false),
            ("my-node", identify(None, Some("SER123")), false),
            // A query without the `id:` prefix may also name a serial number.
            ("SER123", identify(Some("my-node"), Some("SER123")), true),
            ("my-node", identify(None, None), true),
            ("my-node", defaults, true),
            ("my-node", empty, true),
        ] {
            let matches = identity_matches(&TargetInfoQuery::try_from(query).unwrap(), &identify);
            assert_eq!(matches, want, "query {query:?}, target reports {identify:?}");
        }
    }

    #[fuchsia::test]
    async fn test_get_connection_rejects_a_different_target() {
        let env = ffx_config::test_init().unwrap();
        let (resolution, res, _) =
            connect_to_fake_rcs(&env.context, Some("id:SER123"), fake_identity("a-node", "SER999"))
                .await;
        let err = res.unwrap_err();
        match &err {
            crate::FfxTargetCrateError::Resolution(
                crate::error::TargetResolutionError::IdentityMismatch {
                    expected_query,
                    target_spec,
                    actual_nodename,
                    actual_serial,
                },
            ) => {
                assert_eq!(expected_query, "id:SER123");
                assert_eq!(target_spec, "mock_target");
                assert_eq!(actual_nodename.as_deref(), Some("a-node"));
                assert_eq!(actual_serial.as_deref(), Some("SER999"));
            }
            other => panic!("expected IdentityMismatch, got {other:?}"),
        }
        assert!(!resolution.is_usable().await);
        // The user has to act on a mismatch: connecting to the same address again cannot fix it.
        let err = err.into_command_error();
        assert!(matches!(err, ffx_command_error::Error::User(_)), "{err:?}");
        assert!(err.downcast_non_fatal().is_err());
    }

    #[fuchsia::test]
    async fn test_get_connection_accepts_the_target_and_remembers_its_identity() {
        let env = ffx_config::test_init().unwrap();
        let (resolution, res, calls) =
            connect_to_fake_rcs(&env.context, Some("id:SER123"), fake_identity("a-node", "SER123"))
                .await;
        res.unwrap();
        let identity = resolution.identify(&env.context).await.unwrap();
        assert_eq!(identity.serial_number.as_deref(), Some("SER123"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[fuchsia::test]
    async fn test_identify_that_connects_asks_only_once() {
        let env = ffx_config::test_init().unwrap();
        let (resolution, calls) =
            fake_rcs_resolution(Some("id:SER123"), fake_identity("a-node", "SER123"));
        let identity = resolution.identify(&env.context).await.unwrap();
        assert_eq!(identity.serial_number.as_deref(), Some("SER123"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[fuchsia::test]
    async fn test_get_connection_does_not_identify_without_a_symbolic_query() {
        let env = ffx_config::test_init().unwrap();
        let (_, res, calls) =
            connect_to_fake_rcs(&env.context, None, fake_identity("a-node", "SER999")).await;
        res.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[fuchsia::test]
    async fn test_get_connection_accepts_a_target_that_cannot_identify_itself() {
        let env = ffx_config::test_init().unwrap();
        let identity =
            FakeIdentify::Respond(Err(rcs_fidl::IdentifyHostError::ListInterfacesFailed));
        let (resolution, res, _) =
            connect_to_fake_rcs(&env.context, Some("id:SER123"), identity).await;
        res.unwrap();
        assert!(resolution.is_usable().await);
    }

    #[fuchsia::test]
    async fn test_get_connection_can_be_retried_when_identify_host_closes_channel() {
        let env = ffx_config::test_init().unwrap();
        let (_, res, _) =
            connect_to_fake_rcs(&env.context, Some("id:SER123"), FakeIdentify::DropResponder).await;
        assert!(res.unwrap_err().into_command_error().downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_get_connection_times_out_when_identify_host_stalls() {
        let env =
            ffx_config::test_env().runtime_config(CONFIG_TARGET_SSH_TIMEOUT, 10).build().unwrap();
        // The timeout also covers opening the RCS proxy, so on a slow or loaded host it may fire
        // before the fake receives IdentifyHost. Only check what holds either way.
        let (_, res, _) =
            connect_to_fake_rcs(&env.context, Some("id:SER123"), FakeIdentify::Stall).await;
        let err = res.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err:?}");
        assert!(err.into_command_error().downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_get_connection_can_be_retried_when_rcs_proxy_fails() {
        let env = ffx_config::test_init().unwrap();
        let conn = std::sync::Mutex::new(Some(Connection::from_fdomain_client(
            fdomain_local::local_client_empty(),
        )));
        let mut resolution = Resolution::mock(move || Ok(conn.lock().unwrap().take().unwrap()));
        resolution.expected_query = Some(TargetInfoQuery::Id("SER123".into()));

        let err = resolution.get_connection(&env.context).await.unwrap_err();
        assert!(err.into_command_error().downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_get_connection_times_out_when_rcs_proxy_stalls() {
        struct PendingReader;
        impl tokio::io::AsyncRead for PendingReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Pending
            }
        }

        #[derive(Debug)]
        struct StallingConnector;
        impl crate::TransportConnector for StallingConnector {
            const CONNECTION_TYPE: &'static str = "stalling";
            async fn connect(
                &mut self,
            ) -> Result<crate::TargetConnection, crate::TargetConnectionError> {
                let (_, errors) = async_channel::unbounded();
                Ok(crate::TargetConnection::FDomain(crate::FDomainConnection {
                    output: Box::new(tokio::io::BufReader::new(PendingReader)),
                    input: Box::new(tokio::io::sink()),
                    errors,
                    main_task: None,
                }))
            }
        }

        let env =
            ffx_config::test_env().runtime_config(CONFIG_TARGET_SSH_TIMEOUT, 10).build().unwrap();
        let conn = std::sync::Mutex::new(Some(Connection::new(StallingConnector).await.unwrap()));
        let mut resolution = Resolution::mock(move || Ok(conn.lock().unwrap().take().unwrap()));
        resolution.expected_query = Some(TargetInfoQuery::Id("SER123".into()));

        let err = resolution.get_connection(&env.context).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err:?}");
        assert!(err.into_command_error().downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_only_symbolic_queries_expect_an_identity() {
        let env = ffx_config::test_init().unwrap();
        let (sa, addr_spec) = get_addr_and_spec();
        for (query, want) in [
            (TargetInfoQuery::Id("SER123".into()), Some(TargetInfoQuery::Id("SER123".into()))),
            (
                TargetInfoQuery::NodenameOrId("foo".into()),
                Some(TargetInfoQuery::NodenameOrId("foo".into())),
            ),
            // An unannotated serial query narrows to `Id(...)` so missing system serials match.
            (
                TargetInfoQuery::NodenameOrId("SER123".into()),
                Some(TargetInfoQuery::Id("SER123".into())),
            ),
            (TargetInfoQuery::First, None),
            (addr_spec, None),
        ] {
            let mut resolver = MockTargetResolver::new();
            resolver.expect_try_resolve_manual_target().returning(|_, _| Ok(None));
            resolver.expect_discovered_targets().returning(move |_| {
                Ok(vec![TargetHandle {
                    node_name: Some("foo".into()),
                    state: discovery::TargetState::Product {
                        addrs: vec![sa.into()],
                        serial: Some("SER123".into()),
                    },
                    manual: false,
                }])
            });
            let resolution =
                resolver.resolve_target_address(&query, false, &env.context, None).await.unwrap();
            assert_eq!(resolution.expected_query, want, "{query:?}");
        }
    }
    #[fuchsia::test]
    async fn test_match_target_infos_to_resolutions_narrows_unannotated_serial() {
        let (sa, _) = get_addr_and_spec();
        let target = TargetInfo {
            nodename: Some("foo".into()),
            serial_number: Some("SER123".into()),
            target_state: info::TargetState::Product,
            addresses: vec![sa.into()],
            ..Default::default()
        };
        let resolutions = match_target_infos_to_resolutions(
            vec![target.clone()],
            &TargetInfoQuery::NodenameOrId("SER123".into()),
        );
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].expected_query, Some(TargetInfoQuery::Id("SER123".into())));
        // Targets whose nodename also matches keep `NodenameOrId`.
        let same_name = TargetInfo { nodename: Some("SER123".into()), ..target };
        let resolutions = match_target_infos_to_resolutions(
            vec![same_name],
            &TargetInfoQuery::NodenameOrId("SER123".into()),
        );
        assert_eq!(
            resolutions[0].expected_query,
            Some(TargetInfoQuery::NodenameOrId("SER123".into()))
        );
    }

    fn unreachable_error() -> crate::FfxTargetCrateError {
        crate::KnockError::NonCritical(crate::KnockNonCriticalError::RcsKnockFailed {
            detail: "timeout".into(),
        })
        .into()
    }

    #[fuchsia::test]
    async fn test_explain_unreachable_names_the_registered_query_in_strict_mode() {
        let strict_ctx =
            EnvironmentContext::strict(ExecutableKind::Test, ffx_config::ConfigMap::new()).unwrap();
        let mut resolution = Resolution::mock(|| unreachable!());
        resolution.expected_query = Some(TargetInfoQuery::try_from("id:SER123").unwrap());

        let err = resolution.explain_unreachable(unreachable_error(), &strict_ctx);
        // Callers that wait for a rebooting target retry on non-critical knock errors.
        assert!(
            matches!(
                &err,
                crate::FfxTargetCrateError::Knock(crate::KnockError::NonCritical(
                    crate::KnockNonCriticalError::RegisteredTargetUnreachable { query, .. }
                )) if query == "id:SER123"
            ),
            "{err:?}"
        );
        let err = err.into_command_error();
        assert!(err.to_string().contains("Cannot reach 'id:SER123' at mock_target"), "{err}");
        assert!(err.downcast_non_fatal().is_ok());
    }

    #[fuchsia::test]
    async fn test_explain_unreachable_keeps_other_errors() {
        let env = ffx_config::test_init().unwrap();
        let strict_ctx =
            EnvironmentContext::strict(ExecutableKind::Test, ffx_config::ConfigMap::new()).unwrap();
        let mut symbolic = Resolution::mock(|| unreachable!());
        symbolic.expected_query = Some(TargetInfoQuery::try_from("id:SER123").unwrap());
        let direct = Resolution::mock(|| unreachable!());

        for (resolution, err, context) in [
            // Non-strict symbolic queries are not resolved from the registry.
            (&symbolic, unreachable_error(), &env.context),
            // Direct addresses were not looked up, so there is nothing to explain.
            (&direct, unreachable_error(), &strict_ctx),
            // Only failures to reach the target are explained.
            (&symbolic, crate::error::TargetResolutionError::UsbDisabled.into(), &strict_ctx),
        ] {
            let err = resolution.explain_unreachable(err, context);
            assert!(
                !matches!(
                    err,
                    crate::FfxTargetCrateError::Knock(crate::KnockError::NonCritical(
                        crate::KnockNonCriticalError::RegisteredTargetUnreachable { .. }
                    ))
                ),
                "{err:?}"
            );
        }
    }
}
