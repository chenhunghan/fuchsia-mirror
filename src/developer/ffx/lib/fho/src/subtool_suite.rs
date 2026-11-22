// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{FfxMain, FfxTool, FhoEnvironment, TargetNeed, TryFromEnv};
use argh::{ArgsInfo, FromArgs, SubCommand};
use ffx_command::{Error, LocalBoxFuture, Result, check_strict_constraints};
use writer::ToolIO;

// We need this wrapper because we want to store the subtool as a dyn
// FfxTool, but FfxTool has associated types, so it doesn't really work the
// way we want it to. For instance, you can't return a Box<dyn FfxTool>,
// since the associated types have to be specified. Instead, we store a
// Subtool<T>, which implements this trait.
pub trait SubtoolBox {
    fn supports_structured_output(&self) -> bool;
    fn has_schema(&self) -> bool;
    fn try_print_schema(
        self: Box<Self>,
        env: FhoEnvironment,
    ) -> LocalBoxFuture<'static, Result<()>>;
    fn run(self: Box<Self>, env: FhoEnvironment) -> LocalBoxFuture<'static, Result<()>>;
}

/// A container for a deferred `FfxTool` -- wrap the subcommand's parsed args in
/// this object in your implementation of `SubtoolSuite::new_subtool()`.
///
/// The underlying tool is not constructed via `FfxTool::from_env` until
/// `SubtoolBox::run` is called, so metadata operations like `--schema` do not
/// trigger environment setup or target discovery.
pub struct Subtool<T: FfxTool<E>, E = Error>
where
    E: Into<Error> + 'static,
{
    cmd: T::Command,
    _marker: std::marker::PhantomData<fn() -> (T, E)>,
}

impl<T: FfxTool<E>, E: Into<Error> + 'static> Subtool<T, E> {
    pub fn new(cmd: T::Command) -> Box<Self> {
        Box::new(Self { cmd, _marker: std::marker::PhantomData })
    }
}

impl<T: FfxTool<E> + 'static, E: Into<Error> + 'static> SubtoolBox for Subtool<T, E> {
    fn supports_structured_output(&self) -> bool {
        <<T as FfxMain>::Writer as ToolIO>::supports_structured_output()
    }
    fn has_schema(&self) -> bool {
        <<T as FfxMain>::Writer as ToolIO>::has_schema()
    }
    fn try_print_schema(
        self: Box<Self>,
        env: FhoEnvironment,
    ) -> LocalBoxFuture<'static, Result<()>> {
        // Intentionally skip the per-subcommand target strictness check:
        // printing a command's schema never connects to a target, and
        // non-target `--strict` constraints were already verified in
        // `FhoTool::build`.
        Box::pin(async move {
            let mut writer = <<T as FfxMain>::Writer as TryFromEnv>::try_from_env(&env)
                .await
                .map_err(|e| Error::Unexpected(anyhow::Error::new(e)))?;
            writer.try_print_schema().map_err(|e| e.into())
        })
    }

    fn run(self: Box<Self>, env: FhoEnvironment) -> LocalBoxFuture<'static, Result<()>> {
        Box::pin(async move {
            check_strict_constraints(
                &env.ffx_command().global,
                Some(env.environment_context()),
                T::target_need(),
            )?;
            let writer = <<T as FfxMain>::Writer as TryFromEnv>::try_from_env(&env)
                .await
                .map_err(|e| Error::Unexpected(anyhow::Error::new(e)))?;
            let tool = Box::pin(T::from_env(env.clone(), self.cmd)).await?;
            env.update_log_file(tool.log_basename())?;
            tool.main(writer).await.map_err(|e| e.into())
        })
    }
}

/// A trait for implementing a suite of subtools. The implementation is likely simply:
/// ```
/// impl SubtoolSuite for MySuite {
///     type Command = MyCommand;
///
///     fn new_subtool(cmd: MyCommand) -> Box<dyn SubtoolBox> {
///         match cmd.subcommand {
///             MySubCommand::Subtool1(cmd) => Subtool::<ffx_my_subtool1::MySubtool1>::new(cmd),
///             MySubCommand::Subtool2(cmd) => Subtool::<ffx_my_subtool2::MySubtool2>::new(cmd),
///             // Nested suites must delegate directly (see note below):
///             // MySubCommand::Nested(cmd) => NestedSuite::new_subtool(cmd),
///         }
///     }
/// }
/// ```
///
/// Nested suites must delegate directly to `NestedSuite::new_subtool(cmd)` rather
/// than wrapping `FfxSubtoolSuite<NestedSuite>` in [`Subtool`]. `Subtool` queries
/// schema and structured-output support from `T::Writer` (which is `NullWriter`
/// for `FfxSubtoolSuite`), so wrapping a suite in `Subtool` would bypass
/// `FfxSubtoolSuite`'s overrides and report `--schema` as unsupported.
pub trait SubtoolSuite {
    type Command: FromArgs + SubCommand + ArgsInfo;
    fn new_subtool(cmd: Self::Command) -> Box<dyn SubtoolBox>;
}

/// The wrapper for the suite. Usually used as:
/// ```
/// pub type MySuiteTool = FfxSubtoolSuite<MySuite>;
/// ...
/// #[fuchsia_async::run_singlethreaded]
/// async fn main() {
///     MySuiteTool::execute_tool().await
/// }
/// ```
pub struct FfxSubtoolSuite<S: SubtoolSuite> {
    env: FhoEnvironment,
    subtool: Box<dyn SubtoolBox>,
    _marker: std::marker::PhantomData<S>,
}

impl<S: SubtoolSuite> FfxTool for FfxSubtoolSuite<S> {
    type Command = S::Command;
    async fn from_env(env: FhoEnvironment, cmd: Self::Command) -> Result<Self> {
        let subtool = S::new_subtool(cmd);
        Ok(Self { subtool, env, _marker: std::marker::PhantomData })
    }

    fn supports_structured_output(&self) -> bool {
        self.subtool.supports_structured_output()
    }
    fn has_schema(&self) -> bool {
        self.subtool.has_schema()
    }
    // Per-subcommand target requirements are checked in `Subtool::run`.
    fn target_need() -> TargetNeed {
        TargetNeed::None
    }
}

impl<S: SubtoolSuite> FfxMain for FfxSubtoolSuite<S> {
    type Writer = crate::null_writer::NullWriter;
    type Error = Error;
    async fn main(self, _writer: Self::Writer) -> Result<()> {
        self.subtool.run(self.env).await
    }
    async fn try_print_schema(self, _writer: Self::Writer) -> Result<()> {
        self.subtool.try_print_schema(self.env).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate as fho;
    use crate::adapters::tests::{SIMPLE_CHECK_COUNTER, SimpleCheck, TestWriter};
    use crate::subtool::FhoSuite;
    use ffx_command::{FfxCommandLine, MetricsSession, ToolSuite};
    use fho_macro::FfxTool;
    use std::cell::Cell;

    thread_local! {
        static SCHEMA_PRINTED: Cell<u32> = const { Cell::new(0) };
        static MAIN_CALLS: Cell<u32> = const { Cell::new(0) };
    }

    fn reset_counters() {
        SIMPLE_CHECK_COUNTER.with(|c| *c.borrow_mut() = 0);
        SCHEMA_PRINTED.with(|c| c.set(0));
        MAIN_CALLS.with(|c| c.set(0));
    }

    struct SchemaWriter;

    impl TryFromEnv for SchemaWriter {
        type Error = std::convert::Infallible;
        async fn try_from_env(_env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
            Ok(SchemaWriter)
        }
    }

    impl std::io::Write for SchemaWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl ToolIO for SchemaWriter {
        type OutputItem = String;

        fn has_schema() -> bool {
            true
        }
        fn try_print_schema(&mut self) -> writer::Result<()> {
            SCHEMA_PRINTED.with(|c| c.set(c.get() + 1));
            Ok(())
        }
        fn is_machine(&self) -> bool {
            false
        }
        fn stderr(&mut self) -> &mut (dyn std::io::Write + Send + Sync) {
            self
        }
        fn item(&mut self, _value: &Self::OutputItem) -> writer::Result<()> {
            Ok(())
        }
    }

    struct NeedsTarget;

    impl TryFromEnv for NeedsTarget {
        const TARGET_NEED: TargetNeed = TargetNeed::Required;
        type Error = std::convert::Infallible;
        async fn try_from_env(_env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
            Ok(Self)
        }
    }

    struct WrapError;

    impl crate::EnvironmentInterface for WrapError {
        fn wrap_main_errors(&self, err: Error) -> Error {
            ffx_command::user_error!("wrapped: {err}")
        }
    }

    struct RegisterWrap();

    impl crate::CheckEnv for RegisterWrap {
        async fn check_env(self, env: &FhoEnvironment) -> Result<()> {
            env.set_interface(WrapError);
            Ok(())
        }
    }

    #[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "with-schema", description = "subcommand with schema")]
    struct WithSchemaCmd {}

    #[derive(FfxTool)]
    #[check(RegisterWrap())]
    #[check(SimpleCheck(false))]
    struct WithSchemaTool {
        #[command]
        _cmd: WithSchemaCmd,
        _target: NeedsTarget,
    }

    impl FfxMain for WithSchemaTool {
        type Writer = SchemaWriter;
        type Error = Error;
        async fn main(self, _writer: Self::Writer) -> Result<()> {
            unreachable!("main should not be called")
        }
    }

    #[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "no-schema", description = "subcommand without schema")]
    struct NoSchemaCmd {}

    #[derive(FfxTool)]
    #[check(SimpleCheck(false))]
    struct NoSchemaTool {
        #[command]
        _cmd: NoSchemaCmd,
    }

    impl FfxMain for NoSchemaTool {
        type Writer = TestWriter;
        type Error = Error;
        async fn main(self, _writer: Self::Writer) -> Result<()> {
            unreachable!("main should not be called")
        }
    }

    #[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "host-only", description = "host-only subcommand")]
    struct HostOnlyCmd {}

    #[derive(Debug)]
    struct CustomToolError;

    impl From<CustomToolError> for Error {
        fn from(_: CustomToolError) -> Self {
            ffx_command::user_error!("custom error")
        }
    }

    #[derive(FfxTool)]
    #[main_error(CustomToolError)]
    #[check(SimpleCheck(true))]
    struct HostOnlyTool {
        #[command]
        _cmd: HostOnlyCmd,
    }

    impl FfxMain for HostOnlyTool {
        type Writer = SchemaWriter;
        type Error = CustomToolError;
        async fn main(self, _writer: Self::Writer) -> std::result::Result<(), CustomToolError> {
            MAIN_CALLS.with(|c| c.set(c.get() + 1));
            Ok(())
        }
    }

    #[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
    #[argh(subcommand)]
    enum TestSubCommand {
        WithSchema(WithSchemaCmd),
        NoSchema(NoSchemaCmd),
        HostOnly(HostOnlyCmd),
    }

    #[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "suite", description = "test suite")]
    struct TestSuiteCmd {
        #[argh(subcommand)]
        subcommand: TestSubCommand,
    }

    struct TestSuite;

    impl SubtoolSuite for TestSuite {
        type Command = TestSuiteCmd;

        fn new_subtool(cmd: TestSuiteCmd) -> Box<dyn SubtoolBox> {
            match cmd.subcommand {
                TestSubCommand::WithSchema(cmd) => Subtool::<WithSchemaTool>::new(cmd),
                TestSubCommand::NoSchema(cmd) => Subtool::<NoSchemaTool>::new(cmd),
                TestSubCommand::HostOnly(cmd) => Subtool::<HostOnlyTool, _>::new(cmd),
            }
        }
    }

    type TestSuiteTool = FfxSubtoolSuite<TestSuite>;

    async fn run_suite(argv: &[&str]) -> Result<()> {
        let config_env = ffx_config::test_init().unwrap();
        let ffx = FfxCommandLine::new(None, argv).unwrap();
        let suite = FhoSuite::<TestSuiteTool>::from_env(&config_env.context).unwrap();
        let runner = suite.try_from_args(&ffx).await?.expect("runner");
        let metrics = MetricsSession::start(&config_env.context).await.unwrap();
        runner.run(metrics).await.map(|_| ())
    }

    #[fuchsia::test]
    async fn schema_skips_subtool_from_env() {
        reset_counters();
        let res = run_suite(&["ffx", "--schema", "suite", "with-schema"]).await;
        assert!(res.is_ok(), "expected --schema to succeed without running from_env: {res:?}");
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 0);
        assert_eq!(SCHEMA_PRINTED.with(Cell::get), 1);
    }

    #[fuchsia::test]
    async fn schema_unsupported_skips_subtool_from_env() {
        reset_counters();
        let err = run_suite(&["ffx", "--schema", "suite", "no-schema"]).await.unwrap_err();
        assert!(err.to_string().contains("--schema is not supported"), "unexpected error: {err:?}");
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 0);
    }

    #[fuchsia::test]
    async fn normal_run_builds_and_executes_subtool() {
        reset_counters();
        let res = run_suite(&["ffx", "suite", "host-only"]).await;
        assert!(res.is_ok(), "expected normal run to succeed: {res:?}");
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 1);
        assert_eq!(MAIN_CALLS.with(Cell::get), 1);
    }

    #[fuchsia::test]
    async fn strict_enforces_subcommand_target_need() {
        reset_counters();
        let err = run_suite(&[
            "ffx",
            "--strict",
            "--machine",
            "json",
            "-o",
            "/dev/null",
            "suite",
            "with-schema",
        ])
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("ffx strict requires that the target be explicitly specified"),
            "expected strict target error, got: {err:?}"
        );
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 0);

        reset_counters();
        let res = run_suite(&[
            "ffx",
            "--strict",
            "--machine",
            "json",
            "-o",
            "/dev/null",
            "suite",
            "host-only",
        ])
        .await;
        assert!(res.is_ok(), "expected host-only subcommand to pass strict check: {res:?}");
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 1);
        assert_eq!(MAIN_CALLS.with(Cell::get), 1);
    }

    #[fuchsia::test]
    async fn strict_schema_skips_target_check() {
        reset_counters();
        let res = run_suite(&[
            "ffx",
            "--strict",
            "--schema",
            "--machine",
            "json",
            "-o",
            "/dev/null",
            "suite",
            "with-schema",
        ])
        .await;
        assert!(res.is_ok(), "expected --strict --schema to skip target check: {res:?}");
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 0);
        assert_eq!(SCHEMA_PRINTED.with(Cell::get), 1);

        reset_counters();
        let err = run_suite(&[
            "ffx",
            "--strict",
            "--schema",
            "--machine",
            "json",
            "suite",
            "with-schema",
        ])
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("Log Destination be explicitly specified"),
            "expected non-target strict error, got: {err:?}"
        );
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 0);
        assert_eq!(SCHEMA_PRINTED.with(Cell::get), 0);
    }

    #[fuchsia::test]
    async fn from_env_error_is_wrapped_by_run() {
        reset_counters();
        let err = run_suite(&["ffx", "suite", "with-schema"]).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wrapped:") && msg.contains("SimpleCheck was false"),
            "expected wrapped setup error from wrap_main_errors, got: {msg:?}"
        );
        assert_eq!(SIMPLE_CHECK_COUNTER.with(|c| *c.borrow()), 1);
    }
}
