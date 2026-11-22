// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FhoEnvironment;
use ffx_command::TargetNeed;
use ffx_config::EnvironmentContext;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

/// TryFromEnv is used to perform dependency injection on the members of
/// FfxTool.
pub trait TryFromEnv: Sized {
    /// Whether injecting this type requires a target specifier in `--strict` mode.
    ///
    /// Defaults to [`TargetNeed::None`]. Any implementation that resolves or connects
    /// to a target must override this (typically to [`TargetNeed::Required`]) so
    /// strict-mode pre-entry checks catch missing or invalid `--target` flags.
    const TARGET_NEED: TargetNeed = TargetNeed::None;
    type Error: std::error::Error + Send + Sync + 'static;
    fn try_from_env(
        env: &FhoEnvironment,
    ) -> impl Future<Output = std::result::Result<Self, Self::Error>>;
}

/// Decorator trait used by `#[with(expr)]` fields on `#[derive(FfxTool)]` structs.
///
/// Expressions passed to `#[with(...)]` must be standalone (cannot reference locals
/// from `from_env`) because `#[derive(FfxTool)]` also inspects the decorator's type
/// inside the zero-argument `FfxTool::target_need()` method.
pub trait TryFromEnvWith: 'static {
    /// Whether injecting this type requires a target specifier in `--strict` mode.
    ///
    /// Defaults to [`TargetNeed::None`]. Any implementation that resolves or connects
    /// to a target must override this (typically to [`TargetNeed::Required`]).
    const TARGET_NEED: TargetNeed = TargetNeed::None;
    type Output: 'static;
    type Error: std::error::Error + Send + Sync + 'static;
    fn try_from_env_with(
        self,
        env: &FhoEnvironment,
    ) -> impl Future<Output = std::result::Result<Self::Output, Self::Error>>;
}

/// Returns the `TARGET_NEED` of a `#[with(...)]` decorator without executing the expression.
///
/// Taking a never-called closure tied to the field type `O` resolves generic parameters on
/// constructors like `moniker("...")` or `toolbox()` without allocating or running side
/// effects. Because `#[derive(FfxTool)]` emits this inside the zero-argument `target_need()`,
/// `#[with(expr)]` expressions must be standalone and cannot reference `from_env` locals.
pub fn target_need_of<O, D, F>(_: F) -> TargetNeed
where
    D: TryFromEnvWith<Output = O>,
    F: FnOnce() -> D,
{
    D::TARGET_NEED
}

/// This is so that you can use a () somewhere that generically expects something
/// to be TryFromEnv, but there's no meaningful type to put there.
impl TryFromEnv for () {
    type Error = std::convert::Infallible;
    async fn try_from_env(_env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(())
    }
}

impl<T> TryFromEnv for Arc<T>
where
    T: TryFromEnv,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED;
    type Error = T::Error;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        T::try_from_env(env).await.map(Arc::new)
    }
}

impl<T> TryFromEnv for Rc<T>
where
    T: TryFromEnv,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED;
    type Error = T::Error;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        T::try_from_env(env).await.map(Rc::new)
    }
}

impl<T> TryFromEnv for Box<T>
where
    T: TryFromEnv,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED;
    type Error = T::Error;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        T::try_from_env(env).await.map(Box::new)
    }
}

impl<T> TryFromEnv for PhantomData<T> {
    type Error = std::convert::Infallible;
    async fn try_from_env(_env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(PhantomData)
    }
}

impl<T> TryFromEnv for std::result::Result<T, T::Error>
where
    T: TryFromEnv,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED;
    type Error = std::convert::Infallible;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(T::try_from_env(env).await)
    }
}

impl TryFromEnv for EnvironmentContext {
    type Error = std::convert::Infallible;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(env.environment_context().clone())
    }
}

impl TryFromEnv for FhoEnvironment {
    type Error = std::convert::Infallible;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(env.clone())
    }
}

/// Allows you to defer the initialization of an object in your tool struct
/// until you need it (if at all) or apply additional combinators on it (like
/// custom timeout logic or anything like that).
///
/// If you need to defer something that requires a decorator, use the
/// [`deferred`] decorator around it.
///
/// Example:
/// ```rust
/// #[derive(FfxTool)]
/// struct Tool {
///     rcs: fho::Deferred<fidl_fuchsia_developer_remotecontrol::RemoteControlProxy>,
/// }
/// impl fho::FfxMain for Tool {
///     type Writer = fho::SimpleWriter;
///     async fn main(self, _writer: fho::SimpleWriter) -> fho::Result<()> {
///         let rcs = self.rcs.await?;
///         writeln!(writer, "Loaded the remote control proxy!");
///     }
/// }
/// ```
pub struct Deferred<T: 'static, E = ffx_command_error::Error>(
    Pin<Box<dyn Future<Output = std::result::Result<T, E>>>>,
);

impl<T> TryFromEnv for Deferred<T, T::Error>
where
    T: TryFromEnv + 'static,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED.or_optional();
    type Error = std::convert::Infallible;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        let env = env.clone();
        Ok(Self(Box::pin(async move { T::try_from_env(&env).await })))
    }
}

impl<T: 'static, E: 'static> Deferred<T, E> {
    /// Use the value provided to create a test-able Deferred value.
    pub fn from_output(output: std::result::Result<T, E>) -> Self {
        Self(Box::pin(async move { output }))
    }
}

/// The implementation of the decorator returned by [`deferred`]
pub struct WithDeferred<T>(T);

impl<T> TryFromEnvWith for WithDeferred<T>
where
    T: TryFromEnvWith + 'static,
{
    const TARGET_NEED: TargetNeed = T::TARGET_NEED.or_optional();
    type Output = Deferred<T::Output, T::Error>;
    type Error = std::convert::Infallible;
    async fn try_from_env_with(
        self,
        env: &FhoEnvironment,
    ) -> std::result::Result<Self::Output, Self::Error> {
        let env = env.clone();
        Ok(Deferred(Box::pin(async move { self.0.try_from_env_with(&env).await })))
    }
}

/// A decorator for proxy types in [`crate::FfxTool`] implementations so you can
/// specify the moniker for the component exposing the proxy you're loading.
///
/// Example:
///
/// ```rust
/// #[derive(FfxTool)]
/// struct Tool {
///     #[with(fho::deferred(fho::moniker("/core/foo/thing")))]
///     foo_proxy: fho::Deferred<FooProxy>,
/// }
/// ```
pub fn deferred<T: TryFromEnvWith>(inner: T) -> WithDeferred<T> {
    WithDeferred(inner)
}

impl<T, E> Future for Deferred<T, E> {
    type Output = std::result::Result<T, E>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use ffx_command::FfxCommandLine;
    use ffx_command_error::Error;

    use super::*;

    #[derive(Debug)]
    struct AlwaysError;
    impl TryFromEnv for AlwaysError {
        type Error = ffx_command_error::Error;
        async fn try_from_env(_env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
            Err(Error::User(anyhow::anyhow!("boom")))
        }
    }

    #[fuchsia::test]
    async fn test_deferred_err() {
        let config_env = ffx_config::test_init().unwrap();
        let ffx =
            FfxCommandLine::new(None, &["test", "test_deferred_err"]).expect("ffx command line");

        let fho_env = FhoEnvironment::new(&config_env.context, &ffx);

        Deferred::<AlwaysError, ffx_command_error::Error>::try_from_env(&fho_env)
            .await
            .expect("Deferred result should be Ok")
            .await
            .expect_err("Inner AlwaysError should error after second await");
    }
}
