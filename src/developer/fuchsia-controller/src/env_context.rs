// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{LibContext, logging};
use anyhow::Result;
use async_lock::Mutex;
use camino::Utf8PathBuf;
use discovery::query::TargetInfoQuery;
use errors::ffx_error;
use fdomain_client::HandleBased;
use fdomain_client::fidl::Proxy;
use fdomain_fuchsia_device::ControllerMarker;
use ffx_config::environment::ExecutableKind;
use ffx_config::{ConfigError, ConfigLevel, EnvironmentContext};
use ffx_target::connection::Connection;
use rcs;
use serde_json::{Map, Value};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;
use zx_types;

fn unspecified_target() -> anyhow::Error {
    anyhow::anyhow!(concat!(
        "no device has been specified for this `Context`. ",
        "A device must be specified in order to connect to the remote control proxy"
    ))
}

fn fxe<E: std::fmt::Debug>(e: E) -> anyhow::Error {
    ffx_error!("{e:?}").into()
}

fn merge_json_values(dest: &mut Value, src: Value, path: &str) -> Result<()> {
    match (dest, src) {
        (Value::Object(dest_map), Value::Object(src_map)) => {
            for (k, v) in src_map {
                if let Some(dest_val) = dest_map.get_mut(&k) {
                    let curr_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    merge_json_values(dest_val, v, &curr_path)?;
                } else {
                    dest_map.insert(k, v);
                }
            }
            Ok(())
        }
        (d, s) => {
            if d.is_object() != s.is_object() {
                anyhow::bail!(
                    "Conflicting configuration for key '{path}': cannot merge object and non-object values"
                );
            }
            *d = s;
            Ok(())
        }
    }
}

pub(crate) fn unflatten_json(val: Value) -> Result<Value> {
    match val {
        Value::Object(map) => {
            let mut res = Map::new();
            for (key, value) in map {
                let value = unflatten_json(value)?;
                let parts: Vec<&str> = key.split('.').collect();
                let mut curr = &mut res;
                for (idx, &part) in parts[..parts.len() - 1].iter().enumerate() {
                    if !curr.contains_key(part) {
                        curr.insert(part.to_string(), Value::Object(Map::new()));
                    }
                    let v = curr.get_mut(part).unwrap();
                    if !v.is_object() {
                        let err_path = parts[..=idx].join(".");
                        anyhow::bail!(
                            "Conflicting configuration for key '{err_path}': cannot treat scalar as dictionary"
                        );
                    }
                    curr = v.as_object_mut().unwrap();
                }
                let last_part = *parts.last().unwrap();
                if let Some(existing) = curr.get_mut(last_part) {
                    merge_json_values(existing, value, &key)?;
                } else {
                    curr.insert(last_part.to_string(), value);
                }
            }
            Ok(Value::Object(res))
        }
        other => Ok(other),
    }
}

/// Execution mode for an [`EnvContext`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvMode {
    /// Runs in ffx strict mode using only explicit runtime configuration.
    Strict,
    /// Runs in an isolated ffx environment rooted at the given directory.
    Isolated(PathBuf),
    /// Autodetects the ambient ffx environment.
    Detect,
}

#[derive(Debug)]
pub struct EnvContext {
    lib_ctx: Weak<LibContext>,
    target_spec: TargetInfoQuery,
    device_connection: Mutex<Option<Arc<Connection>>>,
    pub(crate) context: EnvironmentContext,
}

async fn new_device_connection(
    ctx: &EnvironmentContext,
    target_spec: &TargetInfoQuery,
) -> Result<Arc<Connection>> {
    // We pass use_cache=false because in Fuchsia Controller, we don't want to
    // scripts to use potentially stale cache data, and the caller can make sure
    // to pass an address directly if they don't want to wait for discovery.
    let resolution = ffx_target::resolve_target_address(target_spec, false, ctx).await?;
    resolution.get_connection(ctx).await.map_err(anyhow::Error::from)
}

// Validates runtime configuration for a strict context so invalid or missing
// settings fail at context creation rather than on first connection.
fn check_strict_config(context: &EnvironmentContext, target_spec: &TargetInfoQuery) -> Result<()> {
    // Reject empty or malformed runtime path settings even when the target does
    // not need the registry (an empty `shared_data` would expand `$SHARED_DATA`
    // to the filesystem root).
    for key in ["shared_data", ffx_config::keys::REGISTERED_TARGETS_CONFIG] {
        match context.query(key).level(Some(ConfigLevel::Runtime)).build().get::<PathBuf>(context) {
            Ok(path) if path.as_os_str().is_empty() => {
                anyhow::bail!("A strict context requires a non-empty `{key}` path");
            }
            Ok(_) | Err(ConfigError::KeyNotFound | ConfigError::NoValueSet(_)) => {}
            Err(e) => {
                anyhow::bail!("Invalid `{key}` in strict context config: {e}");
            }
        }
    }

    // Symbolic target queries (nodenames and `id:<serial>`) resolve from the
    // target registry (`target.registered_targets`, defaulting to
    // `$SHARED_DATA/targets.json`). Explicit transport addresses and local
    // contexts (`TargetInfoQuery::First`) do not consult the registry.
    let needs_registry =
        !matches!(target_spec, TargetInfoQuery::First) && target_spec.get_target_addr().is_none();
    if needs_registry {
        ffx_target::targets_file_path(context).map_err(|e| {
            anyhow::anyhow!(
                "A strict context with a symbolic target requires `shared_data` or \
                 `target.registered_targets` in its config: {e}"
            )
        })?;
    }

    // Validate `ssh.priv` if present in runtime config. Non-SSH transports,
    // symbolic targets (which may resolve to a non-SSH transport), and local
    // contexts do not require `ssh.priv` at creation time.
    let has_ssh_keys = match context
        .query(ffx_config::keys::SSH_PRIVATE_KEY)
        .level(Some(ConfigLevel::Runtime))
        .build()
        .get::<Vec<PathBuf>>(context)
    {
        Ok(keys) => {
            if keys.iter().any(|key| key.as_os_str().is_empty()) {
                anyhow::bail!("A strict context cannot have an empty path in `ssh.priv`");
            }
            !keys.is_empty()
        }
        Err(ConfigError::KeyNotFound | ConfigError::NoValueSet(_)) => false,
        Err(e) => {
            anyhow::bail!("Invalid `ssh.priv` in strict context config: {e}");
        }
    };

    // An explicit IP address (`TargetInfoQuery::Addr`) always connects via SSH.
    // Strict mode cannot expand the default `ssh.priv` macros (`$FUCHSIA_SSH_KEY`,
    // `$HOME/.ssh/fuchsia_ed25519`), and `ffx_ssh` requires at least one key
    // path even when `ssh.auth-sock` is set, so require a non-empty `ssh.priv`
    // at runtime.
    if matches!(target_spec, TargetInfoQuery::Addr(_)) && !has_ssh_keys {
        anyhow::bail!(
            "A strict context with an address target requires a non-empty `ssh.priv` in its config"
        );
    }
    Ok(())
}

impl EnvContext {
    pub(crate) fn write_err<T: std::fmt::Display>(&self, thread_id: std::thread::ThreadId, err: T) {
        let lib = self.lib_ctx.upgrade().expect("library context instance deallocated early");
        lib.write_err(thread_id, err)
    }

    pub(crate) fn lib_ctx(&self) -> Arc<LibContext> {
        self.lib_ctx.upgrade().expect("library context instance deallocated early")
    }

    /// Creates a context configured by `config_json` in the given [`EnvMode`].
    /// With [`EnvMode::Strict`], the context runs in ffx strict mode, as `ffx --strict` does:
    /// it uses only the given configuration, and it resolves target names and serial numbers
    /// from the target registry instead of discovering them.
    pub fn new(
        lib_ctx: Weak<LibContext>,
        config_json: Option<String>,
        mode: EnvMode,
    ) -> Result<Self> {
        let config_json = match config_json.filter(|s| !s.is_empty()) {
            Some(json_str) => {
                let parsed: Value = serde_json::from_str(&json_str)?;
                let unflattened = unflatten_json(parsed)?;
                Some(serde_json::to_string(&unflattened)?)
            }
            None => None,
        };
        let runtime_args = ffx_config::runtime::populate_runtime(&[], config_json)?;
        let env_path = None;
        let is_strict = matches!(mode, EnvMode::Strict);
        let context = match mode {
            EnvMode::Strict => {
                EnvironmentContext::strict(ExecutableKind::Test, runtime_args).map_err(fxe)?
            }
            EnvMode::Isolated(dir) => {
                let current_dir = std::env::current_dir()?;
                EnvironmentContext::isolated(
                    ExecutableKind::Test,
                    dir,
                    std::collections::HashMap::from_iter(std::env::vars()),
                    runtime_args,
                    env_path,
                    Utf8PathBuf::try_from(current_dir).ok().as_deref(),
                    false,
                )
                .map_err(fxe)?
            }
            EnvMode::Detect => {
                let current_dir = std::env::current_dir()?;
                EnvironmentContext::detect(
                    ExecutableKind::Test,
                    runtime_args,
                    &current_dir,
                    env_path,
                    false,
                )
                .map_err(fxe)?
            }
        };
        let target_spec: TargetInfoQuery =
            TargetInfoQuery::try_from(ffx_target::get_target_specifier(&context)?)?;
        if is_strict {
            check_strict_config(&context, &target_spec)?;
        }
        logging::init_logging(&context);
        logging::LOG_SINK.add_log_output(&context)?;
        log::info!("Logging setup for EnvContext instance: {}", logging::log_id(&context));

        let device_connection = if matches!(target_spec, TargetInfoQuery::First) {
            log::info!("No target specified. Creating local/testing FDomain.");
            Mutex::new(Some(Arc::new(Connection::from_fdomain_client(
                fdomain_local::local_client_empty(),
            ))))
        } else {
            Mutex::new(None)
        };
        // A strict context has no cache directory: `get_cache_path()` fails on a
        // strict Test-kind context because `$CACHE` can't be resolved.
        if !is_strict {
            let cache_path = context.get_cache_path()?;
            std::fs::create_dir_all(&cache_path)?;
        }
        Ok(Self { context, device_connection, target_spec, lib_ctx })
    }

    async fn invariant_check(&self) -> Result<Arc<Connection>> {
        log::debug!(
            "Checking connectivity invariant for EnvContext: {}",
            logging::log_id(&self.context)
        );
        let mut device_connection = self.device_connection.lock().await;
        // This is a race condition here. It is possible that the connection
        // will have been terminated between here and when this function completes even if
        // `is_terminated` returns `false`, meaning we would end up hitting the timeout in
        // functions like `connect_remote_control_proxy`.
        let device_connection_is_terminated =
            device_connection.as_ref().map(|c| c.is_terminated()).unwrap_or(false);
        if device_connection_is_terminated {
            log::warn!(
                "connection has been interrupted. Attempting to reconnect. Any closed FIDL proxies seen will have been related to this EnvContext's connection having been lost. This is for EnvContext: {}",
                logging::log_id(&self.context)
            );
        }
        let conn = match &*device_connection {
            Some(conn) if !device_connection_is_terminated => Arc::clone(conn),
            _ => {
                let conn = new_device_connection(&self.context, &self.target_spec).await?;
                *device_connection = Some(Arc::clone(&conn));
                conn
            }
        };
        log::debug!("Invariant check successful: {}", logging::log_id(&self.context));
        Ok(conn)
    }

    async fn connect_remote_control_helper<F, Fut>(&self, func: F) -> Result<zx_types::zx_handle_t>
    where
        F: Fn(fdomain_fuchsia_developer_remotecontrol::RemoteControlProxy) -> Fut,
        Fut: Future<Output = Result<zx_types::zx_handle_t>>,
    {
        if matches!(self.target_spec, TargetInfoQuery::First) {
            return Err(unspecified_target());
        }
        // For a bit of history: originally this was written to deal with a race condition in
        // Overnet. What is rare but possible is for us to establish a connection successfully
        // (usually SSH) and at some point afterward drop the connection.
        //
        // The race condition works like follows:
        // 1. We enter a loop that waits for a RemoteControlProxy advertisement to show up in
        //    Overnet.
        // 2. We lose a connection before this advertisement shows up.
        // 3. Because of this if we don't time out we will loop forever given the way the logic for
        //    this is written (see `locate_remote_control_node` in
        //    //src/developer/ffx/lib/target/src/connection.rs if it still exists at the time of
        //    reading this). That code just waits for Overnet to announce that it sees something
        //    advertising the remote control protocol.
        // 4. Once we timeout we run the `invariant_check` again to check if we've somehow
        //    disconnected and that's the reason we've timed out.
        //
        // All that is to say, we might not need this code so much with FDomain, as the method for
        // connecting to RemoteControlProxy really just requires a connection to the device, and
        // there's not a signal we're expecting to surface from a black box. It probably doesn't
        // hurt to keep this here, but we may want to re-examine its usefulness in the future.
        const MAX_RECONNECT_ATTEMPTS: u32 = 1;
        for attempt in 0..=MAX_RECONNECT_ATTEMPTS {
            let conn = self.invariant_check().await?;
            let t = Duration::from_secs_f64(self.context.get(ffx_config::keys::PROXY_TIMEOUT)?);
            let rcs_res = timeout::timeout(t, conn.rcs_proxy_fdomain())
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "Timed out attempting to get remote control proxy. This happened after \
                         verifying that we can connect to the device, so the device has likely \
                         disconnected in the interim."
                    )
                })
                .and_then(|res| res.map_err(anyhow::Error::from));
            match rcs_res {
                Ok(rcs_proxy) => {
                    log::debug!(
                        "Acquired remote_control_proxy for EnvContext instance: {}",
                        logging::log_id(&self.context)
                    );
                    return func(rcs_proxy).await;
                }
                Err(e) => {
                    let mut guard = self.device_connection.lock().await;
                    if guard.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                        *guard = None;
                    }
                    drop(guard);
                    if attempt < MAX_RECONNECT_ATTEMPTS {
                        log::warn!("{e} Attempting to connect once more");
                    } else {
                        log::warn!("{e} Max attempts reached. Giving up and returning error.");
                        return Err(e);
                    }
                }
            }
        }
        // The above will always return eventually.
        unreachable!();
    }

    pub async fn fdomain_client(&self) -> Result<Arc<fdomain_client::Client>> {
        // While this may attempt to reconnect, we may hit similar race conditions that motivated
        // the original `connect_remote_control_helper` function in the future, so it's possible
        // there will need to be future work done here. However, unlike Overnet, there isn't a loop
        // in which we have to wait for something like remote-control-proxy to announce itself,
        // which is what led to the timeouts motivating `connect_remote_control_helper` in the
        // first place.
        //
        // See said function for the explanation of what it's doing and a "bit of history."
        let conn = self.invariant_check().await?;
        let (fdomain, _) = conn.fdomain_client().await?;
        Ok(fdomain)
    }

    pub async fn connect_remote_control_proxy(&self) -> Result<zx_types::zx_handle_t> {
        log::debug!(
            "Entering connect_remote_control_proxy for EnvContext instance: {}",
            logging::log_id(&self.context)
        );
        self.connect_remote_control_helper(|proxy| async move {
            let hdl = proxy.into_channel().map_err(fxe)?.into_handle();
            let res = self.lib_ctx().fdomain_state().await.register(hdl);
            Ok(res)
        })
        .await
    }

    pub async fn connect_device_proxy(
        &self,
        moniker: String,
        capability_name: String,
    ) -> Result<zx_types::zx_handle_t> {
        log::debug!(
            "Entering connect_device_proxy for EnvContext instance: {}",
            logging::log_id(&self.context)
        );
        self.connect_remote_control_helper(|rcs_proxy| {
            let capability_name_clone = capability_name.clone();
            let moniker_clone = moniker.clone();
            async move {
                let proxy_timeout =
                    Duration::from_secs_f64(self.context.get(ffx_config::keys::PROXY_TIMEOUT)?);
                let proxy = rcs::connect_with_timeout_at::<ControllerMarker>(
                    proxy_timeout,
                    &moniker_clone,
                    &capability_name_clone,
                    &rcs_proxy,
                )
                .await?;
                log::debug!(
                    "Successfully connected to {moniker_clone}:{capability_name_clone} via RCS"
                );
                let hdl = proxy
                    .into_channel()
                    .map_err(fxe)?
                    .into_handle_based::<fdomain_client::Channel>()
                    .into_handle();
                let res = self.lib_ctx().fdomain_state().await.register(hdl);
                Ok(res)
            }
        })
        .await
    }

    pub async fn target_wait(&self, timeout: u64, offline: bool) -> Result<()> {
        log::debug!(
            "Executing target_wait for EnvContext instance: {}",
            logging::log_id(&self.context)
        );
        if matches!(self.target_spec, TargetInfoQuery::First) {
            return Err(unspecified_target());
        }
        let state = if offline {
            ffx_wait_args::TargetStateOption::Down
        } else {
            ffx_wait_args::TargetStateOption::Up
        };
        let cmd = ffx_wait_args::WaitOptions { timeout, down: offline, state: None };
        let tool = ffx_wait::WaitOperation {
            cmd,
            env: self.context.clone(),
            waiter: ffx_wait::DeviceWaiterImpl,
        };
        tool.wait_impl(state).await.map_err(Into::into)
    }
}

impl Drop for EnvContext {
    fn drop(&mut self) {
        log::info!("Dropping EnvContext {}", logging::log_id(&self.context));
        logging::LOG_SINK.remove_log_output(&self.context).expect("remove logger safely");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_unflatten_flat_keys() {
        let input = json!({"foo.bar": 1, "foo.baz": 2, "qux": "hello"});
        let result = unflatten_json(input).unwrap();
        assert_eq!(
            result,
            json!({
                "foo": {
                    "bar": 1,
                    "baz": 2
                },
                "qux": "hello"
            })
        );
    }

    #[test]
    fn test_unflatten_nested_dict() {
        let input = json!({"foo": {"bar": 1}});
        let result = unflatten_json(input).unwrap();
        assert_eq!(result, json!({"foo": {"bar": 1}}));
    }

    #[test]
    fn test_unflatten_mixed_keys() {
        let input = json!({"foo": {"bar": {"a": 1}}, "foo.bar": {"b": 2}});
        let result = unflatten_json(input).unwrap();
        assert_eq!(
            result,
            json!({
                "foo": {
                    "bar": {
                        "a": 1,
                        "b": 2
                    }
                }
            })
        );
    }

    #[test]
    fn test_unflatten_conflict_scalar_then_dict() {
        let input = json!({"foo": 1, "foo.bar": 2});
        assert!(unflatten_json(input).is_err());
    }

    #[test]
    fn test_unflatten_conflict_dict_then_scalar() {
        let input = json!({"foo.bar": 2, "foo": 1});
        assert!(unflatten_json(input).is_err());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_connect_remote_control_helper_clears_stale_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let isolate_dir = tempfile::tempdir().unwrap();
        let sock_path = isolate_dir.path().join("nonexistent_usb.sock");
        let config = json!({
            "proxy.timeout_secs": 0.05,
            "connectivity.usb_socket_path": sock_path.to_str().unwrap(),
            "target.default": "usb:cid:999",
        });
        let env_ctx = EnvContext::new(
            Weak::new(),
            Some(config.to_string()),
            EnvMode::Isolated(isolate_dir.path().to_path_buf()),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let stale_conn =
            Arc::new(Connection::from_fdomain_client(fdomain_local::local_client(move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Err(zx_status::Status::PEER_CLOSED)
            })));
        assert!(!stale_conn.is_terminated());
        *env_ctx.device_connection.lock().await = Some(stale_conn);
        let res = env_ctx.connect_remote_control_helper(|_| async { Ok(1) }).await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(env_ctx.device_connection.lock().await.is_none());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_connect_remote_control_helper_preserves_concurrent_reconnect_and_retries() {
        let isolate_dir = tempfile::tempdir().unwrap();
        let sock_path = isolate_dir.path().join("nonexistent_usb.sock");
        let config = json!({
            "proxy.timeout_secs": 0.05,
            "connectivity.usb_socket_path": sock_path.to_str().unwrap(),
            "target.default": "usb:cid:999",
        });
        let env_ctx = Arc::new(
            EnvContext::new(
                Weak::new(),
                Some(config.to_string()),
                EnvMode::Isolated(isolate_dir.path().to_path_buf()),
            )
            .unwrap(),
        );
        let healthy_conn = Arc::new(Connection::from_fdomain_client(
            fdomain_local::local_client_fdomain(|_channel| {}),
        ));
        let env_ctx_clone = Arc::clone(&env_ctx);
        let healthy_conn_clone = Arc::clone(&healthy_conn);
        let stale_conn =
            Arc::new(Connection::from_fdomain_client(fdomain_local::local_client(move || {
                *env_ctx_clone.device_connection.lock_blocking() =
                    Some(Arc::clone(&healthy_conn_clone));
                Err(zx_status::Status::PEER_CLOSED)
            })));
        *env_ctx.device_connection.lock().await = Some(stale_conn);
        let res = env_ctx.connect_remote_control_helper(|_| async { Ok(42) }).await.unwrap();
        assert_eq!(res, 42);
        assert!(
            env_ctx
                .device_connection
                .lock()
                .await
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(c, &healthy_conn))
        );
    }

    #[test]
    fn test_strict_context() {
        let shared_data = tempfile::tempdir().unwrap();
        let config = json!({
            "shared_data": shared_data.path().to_str().unwrap(),
            "ssh.priv": ["/keys/fuchsia_ed25519"],
            "target.default": "id:SERIAL-A",
        });

        let env_ctx =
            EnvContext::new(Weak::new(), Some(config.to_string()), EnvMode::Strict).unwrap();

        assert!(env_ctx.context.is_strict());
        assert_eq!(env_ctx.target_spec, TargetInfoQuery::Id("SERIAL-A".to_string()));
        assert!(!shared_data.path().join("cache").exists());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_strict_context_accepts_valid_configs() {
        let shared_data = tempfile::tempdir().unwrap();
        let shared_data_str = shared_data.path().to_str().unwrap();
        let targets_file = shared_data.path().join("targets.json");
        let targets_file_str = targets_file.to_str().unwrap();

        for config in [
            // Direct `target.registered_targets` without `shared_data`.
            json!({
                "target.registered_targets": targets_file_str,
                "target.default": "fuchsia-emulator",
            }),
            // Empty `ssh.priv` list (used by Honeydew when no local SSH keys exist).
            json!({
                "shared_data": shared_data_str,
                "ssh.priv": [],
                "target.default": "fuchsia-emulator",
            }),
            // Scalar string `ssh.priv`.
            json!({
                "shared_data": shared_data_str,
                "ssh.priv": "/keys/fuchsia_ed25519",
                "target.default": "id:SERIAL-A",
            }),
            // Explicit IP address target requires a non-empty `ssh.priv`, but
            // does not require `shared_data`.
            json!({
                "target.default": "127.0.0.1:8022",
                "ssh.priv": ["/keys/fuchsia_ed25519"],
            }),
            // Explicit non-SSH transport address needs neither `shared_data` nor `ssh.priv`.
            json!({"target.default": "usb:cid:42"}),
            // Local/testing context with no target.
            json!({}),
        ] {
            let env_ctx = EnvContext::new(Weak::new(), Some(config.to_string()), EnvMode::Strict)
                .unwrap_or_else(|e| panic!("expected strict config {config} to succeed: {e}"));
            assert!(env_ctx.context.is_strict());
        }
    }

    #[test]
    fn test_strict_context_rejects_invalid_settings() {
        let shared_data = tempfile::tempdir().unwrap();
        let shared_data_str = shared_data.path().to_str().unwrap();
        for (config, expected_substring) in [
            // Symbolic target without `shared_data` or `target.registered_targets`.
            (json!({"target.default": "id:SERIAL-A"}), "shared_data"),
            (json!({"target.default": "fuchsia-emulator"}), "shared_data"),
            // IP address target missing non-empty `ssh.priv`.
            (json!({"target.default": "127.0.0.1:8022"}), "ssh.priv"),
            (json!({"target.default": "127.0.0.1:8022", "ssh.priv": []}), "ssh.priv"),
            (
                json!({
                    "target.default": "127.0.0.1:8022",
                    "ssh.priv": [],
                    "ssh.auth-sock": "/tmp/agent.sock",
                }),
                "ssh.priv",
            ),
            // Empty or wrong-typed `shared_data`.
            (json!({"shared_data": ""}), "shared_data"),
            (json!({"shared_data": 123}), "shared_data"),
            // Empty or wrong-typed `target.registered_targets`.
            (json!({"target.registered_targets": ""}), "target.registered_targets"),
            (json!({"target.registered_targets": 123}), "target.registered_targets"),
            // Empty path entries or wrong type in `ssh.priv`.
            (json!({"shared_data": shared_data_str, "ssh.priv": ""}), "ssh.priv"),
            (json!({"shared_data": shared_data_str, "ssh.priv": [""]}), "ssh.priv"),
            (json!({"shared_data": shared_data_str, "ssh.priv": 123}), "ssh.priv"),
            // Empty, wrong-typed, or unexpandable `log.dir`.
            (json!({"log.dir": ""}), "log.dir"),
            (json!({"log.dir": 123}), "log.dir"),
            (json!({"log.dir": "$HOME/logs"}), "HOME"),
            (json!({"log.dir": "$SHARED_DATA/logs"}), "log.dir"),
            // Disallowed variable expansion in strict mode.
            (json!({"shared_data": shared_data_str, "ssh.priv": ["$HOME/key"]}), "HOME"),
        ] {
            let err = EnvContext::new(Weak::new(), Some(config.to_string()), EnvMode::Strict)
                .unwrap_err();
            assert!(err.to_string().contains(expected_substring), "{config}: {err}");
        }
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_strict_context_resolves_registered_target() {
        let shared_data = tempfile::tempdir().unwrap();
        let targets_json = json!([
            {
                "nodename": "test-node",
                "id": "SERIAL-A",
                "address": "127.0.0.1:8022",
            }
        ]);
        std::fs::write(shared_data.path().join("targets.json"), targets_json.to_string()).unwrap();
        let config = json!({
            "shared_data": shared_data.path().to_str().unwrap(),
            "target.default": "id:SERIAL-A",
        });

        let env_ctx =
            EnvContext::new(Weak::new(), Some(config.to_string()), EnvMode::Strict).unwrap();
        let resolution =
            ffx_target::resolve_target_address(&env_ctx.target_spec, false, &env_ctx.context)
                .await
                .unwrap();

        assert_eq!(resolution.addr().unwrap().to_string(), "127.0.0.1:8022");
    }
}
