// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    Connection, DefaultTargetResolver, FfxTargetCrateError, FidlPipe, Resolution, build_discovery,
    build_discovery_from_config,
};
use discovery::DiscoverySources;
use ffx_config::EnvironmentContext;
use ffx_config::environment::ExecutableKind;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

struct TargetConnectorInner {
    context: EnvironmentContext,
    resolution: futures::lock::Mutex<Option<Arc<Resolution>>>,
    resolver: futures::lock::Mutex<DefaultTargetResolver>,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetResolutionError {
    #[error("Failed to resolve target from environment: {0}")]
    Context(#[from] ffx_command_error::Error),
}

impl TargetResolutionError {
    pub fn into_command_error(self) -> ffx_command_error::Error {
        match self {
            TargetResolutionError::Context(e) => e,
        }
    }
}

/// An error from [`TargetConnector::connect`], distinguishing failures to resolve a target from
/// failures to connect to a resolved target, since callers retry the two differently.
#[derive(Debug, thiserror::Error)]
pub enum TargetConnectError {
    #[error(transparent)]
    Resolution(#[from] TargetResolutionError),
    #[error("Failed to connect to target {target_spec}: {err}")]
    Connection {
        target_spec: String,
        #[source]
        err: Box<FfxTargetCrateError>,
    },
}

impl From<TargetConnectError> for ffx_command_error::Error {
    fn from(err: TargetConnectError) -> Self {
        match err {
            TargetConnectError::Resolution(e) => e.into_command_error(),
            TargetConnectError::Connection { err, .. } => (*err).into_command_error(),
        }
    }
}

/// Resolves a target from the environment and establishes a connection to it, caching the
/// resolved target across uses and re-resolving when a connection fails or terminates.
#[derive(Clone)]
pub struct TargetConnector(Arc<TargetConnectorInner>);

// Manually implement Debug because DefaultTargetResolver inside
// TargetConnectorInner does not implement Debug.
impl fmt::Debug for TargetConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetConnector").finish_non_exhaustive()
    }
}

impl TargetConnector {
    fn from_resolver(context: &EnvironmentContext, resolver: DefaultTargetResolver) -> Self {
        TargetConnector(Arc::new(TargetConnectorInner {
            context: context.clone(),
            resolution: futures::lock::Mutex::new(None),
            resolver: futures::lock::Mutex::new(resolver),
        }))
    }

    /// Creates a `TargetConnector` using the discovery timeout configured in `context`.
    pub fn new(context: &EnvironmentContext) -> Self {
        log::info!("Initializing TargetConnector");
        let discovery = build_discovery_from_config(context);
        Self::from_resolver(context, DefaultTargetResolver::new(discovery))
    }

    /// Creates a `TargetConnector` with an explicit discovery timeout (`None` for indefinite).
    pub fn new_with_timeout(context: &EnvironmentContext, timeout: Option<Duration>) -> Self {
        log::info!("Initializing TargetConnector with timeout {:?}", timeout);
        let mut discovery = build_discovery_from_config(context);
        discovery.set_timeout(timeout);
        Self::from_resolver(context, DefaultTargetResolver::new(discovery))
    }

    /// Creates a `TargetConnector` from a target resolution for test use.
    pub fn from_resolution_for_test(resolution: Resolution) -> Self {
        let context =
            EnvironmentContext::no_context(ExecutableKind::Test, Default::default(), None, true)
                .expect("no_context test EnvironmentContext");
        let discovery = build_discovery(DiscoverySources::empty(), &context);
        TargetConnector(Arc::new(TargetConnectorInner {
            context,
            resolution: futures::lock::Mutex::new(Some(Arc::new(resolution))),
            resolver: futures::lock::Mutex::new(DefaultTargetResolver::new(discovery)),
        }))
    }

    pub async fn discovery_timeout(&self) -> Option<Duration> {
        self.0.resolver.lock().await.discovery_timeout()
    }

    pub async fn set_discovery_timeout(&self, timeout: Option<Duration>) {
        self.0.resolver.lock().await.set_discovery_timeout(timeout);
    }

    /// Returns `true` if a target has previously been resolved in this connector.
    pub async fn has_resolved_target(&self) -> bool {
        self.0.resolution.lock().await.is_some()
    }

    // Return a pinned boxed future (LocalBoxFuture) to prevent deep recursion
    // of compiler-generated future types. When this method is called within
    // other complex async contexts (e.g., the ffx log or update plugins),
    // the nested async blocks can otherwise exceed the compiler's recursion limit.
    /// Returns the cached [`Resolution`] if it is still usable, or resolves the target from the
    /// environment (bypassing the discovery cache if a previous resolution became unusable).
    pub fn resolution(
        &self,
    ) -> ffx_futures::LocalBoxFuture<'_, Result<Arc<Resolution>, TargetResolutionError>> {
        Box::pin(async move {
            let mut resolution = self.0.resolution.lock().await;

            let use_cache = if let Some(res) = &*resolution {
                if res.is_usable().await {
                    return Ok(Arc::clone(res));
                }
                false
            } else {
                true
            };

            let resolver = self.0.resolver.lock().await;
            let new = Arc::new(
                Resolution::try_from_env_context_with_resolver(
                    &*resolver,
                    &self.0.context,
                    use_cache,
                )
                .await?,
            );
            *resolution = Some(Arc::clone(&new));

            Ok(new)
        })
    }

    /// Resolves the target and establishes a connection, automatically falling back from a
    /// speculative discovery cache hit to active discovery if the cached address is stale or
    /// unreachable.
    pub async fn connect(
        &self,
        context: &EnvironmentContext,
    ) -> Result<(Arc<Resolution>, Arc<Connection>), TargetConnectError> {
        let mut resolution = self.resolution().await?;
        let mut conn_res = resolution.get_connection(context).await;
        if conn_res.is_err() && resolution.from_cache() {
            log::debug!(
                "Connection to cached target {} failed; re-resolving without cache",
                resolution.target_spec()
            );
            // `get_connection` marks the resolution failed (`is_usable() == false`) and
            // deletes the cache file, so `self.resolution()` re-resolves via active discovery.
            match self.resolution().await {
                Ok(res) => {
                    resolution = res;
                    conn_res = resolution.get_connection(context).await;
                }
                Err(e) => {
                    // The cached target was never verified against a live target, so don't
                    // report it as previously resolved.
                    *self.0.resolution.lock().await = None;
                    return Err(e.into());
                }
            }
        }
        // On failure, `get_connection` marks the resolution unusable, so the next
        // `self.resolution()` call re-resolves without the cache.
        conn_res.map(|conn| (Arc::clone(&resolution), conn)).map_err(|err| {
            TargetConnectError::Connection {
                target_spec: resolution.target_spec(),
                err: Box::new(err),
            }
        })
    }

    /// Returns the active [`Connection`] if a resolution and connection have already been
    /// established and the resolution lock is not currently contended.
    pub fn get_connection_if_already_established(&self) -> Option<Arc<Connection>> {
        self.0.resolution.try_lock()?.as_ref()?.get_connection_if_already_established()
    }

    /// Creates a `TargetConnector` backed by a fake `fdomain_client::Client` for test use.
    pub async fn fake_with_fdomain_client(client: std::sync::Arc<fdomain_client::Client>) -> Self {
        let socket_addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            22,
        );
        let target_handle = discovery::TargetHandle {
            node_name: Some("test-target".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![addr::TargetAddr::Net(socket_addr)],
                serial: None,
            },
            manual: false,
        };
        let resolution = Resolution::from_target_handle(target_handle).unwrap();
        let conn = Connection::from_fdomain_client(client);
        resolution.set_connection_for_test(Some(conn)).await;
        Self::from_resolution_for_test(resolution)
    }
}

/// Creates a fake [`Resolution`] pre-populated with a fake connection at `127.0.0.1:22` and an
/// optional `host_address` for test use.
pub async fn setup_fake_resolution(host_address: Option<String>) -> Resolution {
    let device_address =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)), 22);
    let target_handle = discovery::TargetHandle {
        node_name: Some("test-target".to_string()),
        state: discovery::TargetState::Product {
            addrs: vec![addr::TargetAddr::Net(device_address)],
            serial: None,
        },
        manual: false,
    };
    let ret = Resolution::from_target_handle(target_handle).unwrap();
    let host_ssh_address = host_address.map(ffx_ssh::parse::HostAddr::from);
    let fidl_pipe = FidlPipe::fake_with_host_ssh_address(Some(device_address), host_ssh_address);
    let conn = Connection::fake(fidl_pipe);
    ret.set_connection_for_test(Some(conn)).await;
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FDomainConnection, TargetConnection, TargetConnectionError, TargetInfo, TransportConnector,
        get_discovery_cache_file, info, register_targets,
    };
    use discovery::{DiscoveryBuilder, TargetEvent, TargetHandle, TargetState};
    use ffx_config::keys::{DISCOVERY_CACHE_DIR_CONFIG, SSH_PRIVATE_KEY, TARGET_DEFAULT_KEY};
    use ffx_config::test_env;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct InvalidConnector;
    impl TransportConnector for InvalidConnector {
        const CONNECTION_TYPE: &'static str = "invalid";
        async fn connect(&mut self) -> Result<TargetConnection, TargetConnectionError> {
            Ok(TargetConnection::FDomain(FDomainConnection::invalid()))
        }
    }

    fn write_test_cache(context: &EnvironmentContext, nodename: &str, ip: &str) {
        let cache_file = get_discovery_cache_file(context).expect("cache file path");
        if let Some(parent) = cache_file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let json = serde_json::json!({
            "version": 2,
            "expires": "2036-06-03T19:50:40Z",
            "targets": [
                {
                    "nodename": nodename,
                    "addresses": [
                        {"Net": ip}
                    ],
                    "rcs_state": "Unknown",
                    "target_state": "Product",
                    "product_config": null,
                    "board_config": null,
                    "serial_number": null,
                    "is_manual": false,
                    "boot_id": null,
                    "is_default": null
                }
            ]
        });
        let file = std::fs::File::create(cache_file).unwrap();
        serde_json::to_writer(file, &json).unwrap();
    }

    #[fuchsia::test]
    async fn test_target_connector_initial_resolution_uses_cache() {
        let mut builder = test_env();
        let cache_dir = builder.isolate_root().join("cache");
        let env = builder
            .runtime_config(TARGET_DEFAULT_KEY, "test-target")
            .runtime_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_str().unwrap())
            .build()
            .unwrap();

        write_test_cache(&env.context, "test-target", "127.0.0.1:8082");

        let discovery = build_discovery(DiscoverySources::all(), &env.context);
        let connector =
            TargetConnector::from_resolver(&env.context, DefaultTargetResolver::new(discovery));

        let res = connector.resolution().await;
        assert!(res.is_ok(), "Expected resolution to succeed using cache, got {:?}", res);
        let res = res.unwrap();
        assert_eq!(res.target_spec(), "127.0.0.1:8082");
    }

    #[fuchsia::test]
    async fn test_target_connector_reconnect_bypasses_cache() {
        let mut builder = test_env();
        let cache_dir = builder.isolate_root().join("cache");
        let env = builder
            .runtime_config(TARGET_DEFAULT_KEY, "test-target")
            .runtime_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_str().unwrap())
            .build()
            .unwrap();

        write_test_cache(&env.context, "test-target", "127.0.0.1:8082");

        let conn = Connection::new(InvalidConnector).await.unwrap();
        fuchsia_async::Timer::new(std::time::Duration::from_millis(10)).await;
        assert!(conn.is_terminated());

        let resolution = Resolution::mock(|| Err(anyhow::anyhow!("reconnect failed").into()));

        resolution.set_connection_for_test(Some(conn)).await;

        let discovery = build_discovery(DiscoverySources::all(), &env.context);
        let resolver = DefaultTargetResolver::new(discovery);
        let connector = TargetConnector(Arc::new(TargetConnectorInner {
            context: env.context.clone(),
            resolution: futures::lock::Mutex::new(Some(Arc::new(resolution))),
            resolver: futures::lock::Mutex::new(resolver),
        }));

        let res = connector.resolution().await;
        assert!(
            res.is_err(),
            "Expected resolution to fail (cache bypassed), but got Ok: {:?}",
            res
        );
    }

    #[fuchsia::test]
    async fn test_target_connector_reresolves_on_terminated_connection() {
        let env = test_env().runtime_config(TARGET_DEFAULT_KEY, "127.0.0.1:8082").build().unwrap();

        let conn = Connection::new(InvalidConnector).await.unwrap();
        fuchsia_async::Timer::new(std::time::Duration::from_millis(10)).await;
        assert!(conn.is_terminated());

        let reconnect_called = Arc::new(AtomicBool::new(false));
        let reconnect_called_clone = reconnect_called.clone();
        let initial_resolution = Resolution::mock(move || {
            reconnect_called_clone.store(true, Ordering::SeqCst);
            Err(anyhow::anyhow!("reconnect failed").into())
        });

        initial_resolution.set_connection_for_test(Some(conn)).await;

        let discovery = build_discovery(DiscoverySources::all(), &env.context);
        let resolver = DefaultTargetResolver::new(discovery);
        let connector = TargetConnector(Arc::new(TargetConnectorInner {
            context: env.context.clone(),
            resolution: futures::lock::Mutex::new(Some(Arc::new(initial_resolution))),
            resolver: futures::lock::Mutex::new(resolver),
        }));

        let res = connector.resolution().await;
        assert!(res.is_ok(), "Expected re-resolution to succeed for IP target, got {:?}", res);
        let res = res.unwrap();
        assert_eq!(res.target_spec(), "127.0.0.1:8082");

        assert!(
            !reconnect_called.load(Ordering::SeqCst),
            "Expected no reconnection attempt on terminated connection"
        );
    }

    #[fuchsia::test]
    async fn test_target_connector_failed_connection_bypasses_cache() {
        let mut builder = test_env();
        let cache_dir = builder.isolate_root().join("cache");
        let env = builder
            .runtime_config(TARGET_DEFAULT_KEY, "test-target")
            .runtime_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_str().unwrap())
            .build()
            .unwrap();

        // 1. Create a mocked resolution that fails to connect
        let resolution = Resolution::mock(|| Err(anyhow::anyhow!("connect failed").into()));

        // Call get_connection to trigger the failure
        let conn_res = resolution.get_connection(&env.context).await;
        assert!(conn_res.is_err());

        // 2. Put it in the connector
        let discovery = build_discovery(DiscoverySources::all(), &env.context);
        let resolver = DefaultTargetResolver::new(discovery);
        let connector = TargetConnector(Arc::new(TargetConnectorInner {
            context: env.context.clone(),
            resolution: futures::lock::Mutex::new(Some(Arc::new(resolution))),
            resolver: futures::lock::Mutex::new(resolver),
        }));

        // 3. Resolve. Since the connection failed, it should bypass the cache.
        // Bypassing the cache will try to resolve "test-target" via discovery,
        // which fails in the test environment, so we expect an Err.
        // If it did NOT bypass the cache, it would return Ok(cached_resolution).
        let res = connector.resolution().await;
        assert!(
            res.is_err(),
            "Expected resolution to fail (cache bypassed), but got Ok: {:?}",
            res
        );
    }

    #[fuchsia::test]
    async fn test_target_connector_reconnect_re_resolves_to_new_address() {
        let mut builder = test_env();
        let cache_dir = builder.isolate_root().join("cache");
        let env = builder
            .runtime_config(TARGET_DEFAULT_KEY, "test-target")
            .runtime_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_str().unwrap())
            // Use `/bin/false` as `ssh.path` and a fake `ssh.priv` key so `SshConnector`
            // fails immediately with a non-fatal exit status (simulating an unreachable
            // target after reboot without hanging on network timeouts).
            .runtime_config("ssh.path", "/bin/false")
            .runtime_config(SSH_PRIVATE_KEY, cache_dir.to_str().unwrap())
            .build()
            .unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Define two target handles representing the device before and after reboot (with different ports)
        let addr1 = addr::TargetAddr::from_str("127.0.0.1:8082").unwrap();
        let handle1 = TargetHandle {
            node_name: Some("test-target".to_string()),
            state: TargetState::Product { addrs: vec![addr1], serial: None },
            manual: false,
        };

        let addr2 = addr::TargetAddr::from_str("127.0.0.1:8083").unwrap();
        let handle2 = TargetHandle {
            node_name: Some("test-target".to_string()),
            state: TargetState::Product { addrs: vec![addr2], serial: None },
            manual: false,
        };

        // 1. Target appears at Address 1
        let (tx1, rx1) = futures::channel::mpsc::unbounded::<TargetEvent>();
        tx1.unbounded_send(TargetEvent::Added(handle1.clone())).unwrap();
        drop(tx1); // Close the stream so discovery terminates immediately
        let discovery1 = DiscoveryBuilder::default().build_with_stream(&env.context, rx1);
        let connector =
            TargetConnector::from_resolver(&env.context, DefaultTargetResolver::new(discovery1));

        // Resolve first time - should find Address 1
        let res1 = connector.resolution().await;
        assert!(res1.is_ok());
        let res1 = res1.unwrap();
        assert_eq!(res1.target_spec(), "127.0.0.1:8082");

        // 2. Mock a successful connection
        let conn = Connection::new(InvalidConnector).await.unwrap();
        res1.set_connection_for_test(Some(conn)).await;

        // Verify we can get the connection
        assert!(res1.get_connection(&env.context).await.is_ok());

        // 3. Simulate device going away (connection lost / rebooted)
        // Clear the cached connection
        res1.set_connection_for_test(None).await;

        // Attempt connection, which fails immediately because ssh.priv is invalid
        let conn_res = res1.get_connection(&env.context).await;
        assert!(conn_res.is_err());

        // 4. Target reappears at Address 2
        let (tx2, rx2) = futures::channel::mpsc::unbounded::<TargetEvent>();
        tx2.unbounded_send(TargetEvent::Added(handle2)).unwrap();
        drop(tx2); // Close the stream so discovery terminates immediately
        let discovery2 = DiscoveryBuilder::default().build_with_stream(&env.context, rx2);
        let resolver2 = DefaultTargetResolver::new(discovery2);
        *connector.0.resolver.lock().await = resolver2;

        // Call resolution again - since the connection failed, it should re-resolve
        // and find the new Address 2
        let res2 = connector.resolution().await;
        assert!(res2.is_ok());
        let res2 = res2.unwrap();
        assert_eq!(res2.target_spec(), "127.0.0.1:8083");
    }

    #[fuchsia::test]
    async fn test_target_connector_strict_mode_reconnect_reloads_targets_json() {
        let mut builder = test_env();
        let temp_dir = builder.isolate_root();
        let fake_key = temp_dir.join("fake_ssh_key");
        std::fs::write(&fake_key, "fake").unwrap();

        let runtime_args = serde_json::json!({
            "shared_data": temp_dir.to_str().unwrap(),
            "target": { "default": "id:04012C12D45E" },
            "ssh": {
                "path": "/bin/false",
                "priv": fake_key.to_str().unwrap()
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let strict_ctx = EnvironmentContext::strict(ExecutableKind::Test, runtime_args).unwrap();

        let mut target_v1 = TargetInfo::default();
        target_v1.nodename = Some("fuchsia-node".to_string());
        target_v1.serial_number = Some("04012C12D45E".to_string());
        target_v1.target_state = info::TargetState::Product;
        target_v1.addresses = vec![addr::TargetAddr::from_str("127.0.0.1:8082").unwrap()];
        register_targets(&strict_ctx, &[target_v1]).unwrap();

        let connector = TargetConnector::new(&strict_ctx);

        // 1. Initial resolution resolves to 127.0.0.1:8082 from $SHARED_DATA/targets.json.
        let res1 = connector.resolution().await.unwrap();
        assert_eq!(res1.target_spec(), "127.0.0.1:8082");

        // 2. Simulate connection failure (non-fatal SSH exit status 1 from /bin/false).
        let conn_err = res1.get_connection(&strict_ctx).await.unwrap_err();
        assert!(conn_err.into_command_error().downcast_non_fatal().is_ok());

        // 3. Orchestrator atomically updates $SHARED_DATA/targets.json with new address after reboot.
        let mut target_v2 = TargetInfo::default();
        target_v2.nodename = Some("fuchsia-node".to_string());
        target_v2.serial_number = Some("04012C12D45E".to_string());
        target_v2.target_state = info::TargetState::Product;
        target_v2.addresses = vec![addr::TargetAddr::from_str("127.0.0.1:8083").unwrap()];
        register_targets(&strict_ctx, &[target_v2]).unwrap();

        // 4. Subsequent `connector.resolution()` re-reads $SHARED_DATA/targets.json and resolves to 127.0.0.1:8083.
        let res2 = connector.resolution().await.unwrap();
        assert_eq!(res2.target_spec(), "127.0.0.1:8083");
    }

    fn setup_stale_cache_connector(
        live_addr: Option<&str>,
    ) -> (ffx_config::TestEnv, TargetConnector) {
        let mut builder = test_env();
        let cache_dir = builder.isolate_root().join("cache");
        let fake_key = builder.isolate_root().join("fake_ssh_key");
        std::fs::write(&fake_key, "fake").unwrap();

        let env = builder
            .runtime_config(TARGET_DEFAULT_KEY, "test-target")
            .runtime_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_str().unwrap())
            .runtime_config("ssh.path", "/bin/false")
            .runtime_config(SSH_PRIVATE_KEY, fake_key.to_str().unwrap())
            .build()
            .unwrap();

        write_test_cache(&env.context, "test-target", "127.0.0.1:8082");

        let (tx, rx) = futures::channel::mpsc::unbounded::<TargetEvent>();
        if let Some(addr_str) = live_addr {
            let addr = addr::TargetAddr::from_str(addr_str).unwrap();
            let handle = TargetHandle {
                node_name: Some("test-target".to_string()),
                state: TargetState::Product { addrs: vec![addr], serial: None },
                manual: false,
            };
            tx.unbounded_send(TargetEvent::Added(handle)).unwrap();
        }
        drop(tx);
        let discovery = DiscoveryBuilder::default().build_with_stream(&env.context, rx);
        let connector =
            TargetConnector::from_resolver(&env.context, DefaultTargetResolver::new(discovery));
        (env, connector)
    }

    #[fuchsia::test]
    async fn test_target_connector_connect_falls_back_from_stale_cache_to_active_discovery() {
        let (env, connector) = setup_stale_cache_connector(Some("127.0.0.1:8083"));

        // `connect()` tries cached 127.0.0.1:8082, fails with a non-fatal SSH error, and
        // automatically re-resolves via active discovery to 127.0.0.1:8083.
        let err = connector.connect(&env.context).await.unwrap_err();
        let TargetConnectError::Connection { target_spec, .. } = err else {
            panic!("expected TargetConnectError::Connection, got {err:?}");
        };
        assert_eq!(target_spec, "127.0.0.1:8083");
        assert!(connector.has_resolved_target().await);
    }

    #[fuchsia::test]
    async fn test_target_connector_connect_stale_cache_when_offline_clears_resolved_target() {
        let (env, connector) = setup_stale_cache_connector(None);

        // `connect()` tries cached 127.0.0.1:8082, fails, falls back to active discovery which
        // returns `TargetNotFound`, and clears `has_resolved_target()` to `false`.
        let err = connector.connect(&env.context).await.unwrap_err();
        assert!(matches!(err, TargetConnectError::Resolution(_)), "got {err:?}");
        assert!(!connector.has_resolved_target().await);
    }

    #[fuchsia::test]
    async fn test_target_connector_terminated_non_cache_offline_keeps_resolved_target() {
        let env = test_env().runtime_config(TARGET_DEFAULT_KEY, "test-target").build().unwrap();

        let addr = addr::TargetAddr::from_str("127.0.0.1:8082").unwrap();
        let handle = TargetHandle {
            node_name: Some("test-target".to_string()),
            state: TargetState::Product { addrs: vec![addr], serial: None },
            manual: false,
        };
        let (tx1, rx1) = futures::channel::mpsc::unbounded::<TargetEvent>();
        tx1.unbounded_send(TargetEvent::Added(handle)).unwrap();
        drop(tx1);
        let discovery1 = DiscoveryBuilder::default().build_with_stream(&env.context, rx1);
        let connector =
            TargetConnector::from_resolver(&env.context, DefaultTargetResolver::new(discovery1));

        // 1. Initial non-cache resolution succeeds and establishes a connection that terminates.
        let res1 = connector.resolution().await.unwrap();
        assert!(!res1.from_cache());
        let conn = Connection::new(InvalidConnector).await.unwrap();
        fuchsia_async::Timer::new(std::time::Duration::from_millis(10)).await;
        assert!(conn.is_terminated());
        res1.set_connection_for_test(Some(conn)).await;

        // 2. Subsequent discovery finds no targets (e.g. target is rebooting).
        let (tx2, rx2) = futures::channel::mpsc::unbounded::<TargetEvent>();
        drop(tx2);
        let discovery2 = DiscoveryBuilder::default().build_with_stream(&env.context, rx2);
        *connector.0.resolver.lock().await = DefaultTargetResolver::new(discovery2);

        // 3. Re-resolution fails with `TargetNotFound`, but `has_resolved_target()` stays `true`
        // so `Connector::try_connect` continues waiting for the target to come back.
        let err = connector.connect(&env.context).await.unwrap_err();
        assert!(matches!(err, TargetConnectError::Resolution(_)), "got {err:?}");
        assert!(connector.has_resolved_target().await);
    }
}
