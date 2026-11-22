// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_config::EnvironmentContext;
use ffx_target::TargetConnector;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// FHO environment interface holding the lazily initialized [`TargetConnector`] for a subtool
/// invocation and wrapping subtool errors with target connection diagnostics.
#[derive(Clone, Default)]
pub struct FhoTargetEnvironment(Arc<OnceLock<TargetConnector>>);

impl FhoTargetEnvironment {
    /// Sets the target connector for test use. Panics if a connector is already initialized.
    pub fn set_connector_for_test(&self, connector: TargetConnector) {
        self.0.set(connector).expect("OnceLock::set(connector)")
    }

    /// Returns the shared [`TargetConnector`], initializing it with the discovery timeout from
    /// `context` if not already initialized. If a connector has already been initialized, the
    /// existing instance is returned without updating its timeout.
    pub fn init(&self, context: &EnvironmentContext) -> &TargetConnector {
        self.0.get_or_init(|| TargetConnector::new(context))
    }

    /// Returns the shared [`TargetConnector`], initializing it with `timeout` (`None` for
    /// indefinite) if not already initialized. If a connector has already been initialized, the
    /// existing instance is returned without updating its timeout.
    pub fn init_with_timeout(
        &self,
        context: &EnvironmentContext,
        timeout: Option<Duration>,
    ) -> &TargetConnector {
        self.0.get_or_init(|| TargetConnector::new_with_timeout(context, timeout))
    }

    /// This attempts to wrap errors around a potential failure in the underlying connection being
    /// used to facilitate FIDL protocols. This should NOT be used by developers, this is intended
    /// to be used outside of the scope of an ffx subtool (outside of the `main` function).
    fn maybe_wrap_connection_errors(&self, err: fho::Error) -> fho::Error {
        if let Some(connector) = self.0.get() {
            if let Some(conn) = connector.get_connection_if_already_established() {
                match err {
                    fho::Error::User(e) => {
                        return fho::Error::User(conn.wrap_connection_errors(e).into());
                    }
                    fho::Error::Unexpected(e) => {
                        return fho::Error::Unexpected(conn.wrap_connection_errors(e).into());
                    }
                    _ => (),
                }
            }
        }
        err
    }
}

impl fho::EnvironmentInterface for FhoTargetEnvironment {
    fn wrap_main_errors(&self, err: fho::Error) -> fho::Error {
        self.maybe_wrap_connection_errors(err)
    }
}

/// Gets or inserts the [`FhoTargetEnvironment`] registered in `env`.
pub fn target_interface(env: &fho::FhoEnvironment) -> FhoTargetEnvironment {
    if env.get_interface::<FhoTargetEnvironment>().is_none() {
        let target_interface = FhoTargetEnvironment::default();
        env.set_interface(target_interface);
    }
    env.get_interface::<FhoTargetEnvironment>().expect("No target interface in FhoEnvironment??")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_config::environment::ExecutableKind;
    use ffx_config::keys::TARGET_DEFAULT_KEY;
    use ffx_config::test_env;

    #[fuchsia::test]
    async fn test_init_in_strict() {
        let runtime_args =
            serde_json::json!({"target": {"default" : "127.0.0.1"}}).as_object().unwrap().clone();
        let ctx = EnvironmentContext::strict(ExecutableKind::Test, runtime_args).unwrap();
        let target_env = FhoTargetEnvironment::default();
        let connector = target_env.init(&ctx);
        assert!(connector.resolution().await.is_ok());
    }

    #[fuchsia::test]
    async fn test_init_in_non_strict() {
        let env = test_env().runtime_config(TARGET_DEFAULT_KEY, "127.0.0.1").build().unwrap();
        let target_env = FhoTargetEnvironment::default();
        let connector = target_env.init(&env.context);
        assert!(connector.resolution().await.is_ok());
    }

    #[fuchsia::test]
    async fn init_succeeds_when_called_twice() {
        let env = test_env().build().unwrap();
        let target_env = FhoTargetEnvironment::default();
        let connector = target_env.init(&env.context);
        let connector2 = target_env.init(&env.context);
        assert!(std::ptr::eq(connector, connector2));
    }

    #[fuchsia::test]
    async fn target_interface_persists() {
        let env = test_env().build().unwrap();
        let fho_env = fho::FhoEnvironment::new_with_args(&env.context, &["some", "test"]);
        let target_env = target_interface(&fho_env);
        let _connector = target_env.init_with_timeout(&env.context, None);
        let target_env = target_interface(&fho_env);
        assert_eq!(target_env.init(&env.context).discovery_timeout().await, None);
    }

    // Tests that `FhoTargetEnvironment` can initialize a `TargetConnector`
    // with an explicit indefinite discovery timeout (`timeout: None`), and that `set_discovery_timeout`
    // can be called on the resulting `TargetConnector` to update its resolver's discovery timeout.
    #[fuchsia::test]
    async fn test_init_with_timeout() {
        let env = test_env().build().unwrap();
        let target_env = FhoTargetEnvironment::default();
        let connector = target_env.init_with_timeout(&env.context, None);
        assert_eq!(connector.discovery_timeout().await, None);
        connector.set_discovery_timeout(Some(Duration::from_millis(500))).await;
        assert_eq!(connector.discovery_timeout().await, Some(Duration::from_millis(500)));
    }
}
