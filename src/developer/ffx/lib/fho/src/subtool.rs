// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{FhoEnvironment, TryFromEnv};
use argh::{ArgsInfo, CommandInfo, FromArgs, SubCommand, SubCommands};
use ffx_command::{
    Error, FfxCommandLine, FfxContext, LocalBoxFuture, MetricsSession, Result, TargetNeed,
    ToolRunner, ToolSuite, analytics_command, check_strict_constraints, send_enhanced_analytics,
    user_error,
};
use ffx_config::EnvironmentContext;
use ffx_config::environment::ExecutableKind;
use fho_metadata::FhoToolMetadata;
use std::fs::File;
use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use traceable_error::TraceableError;
use writer::ToolIO;

/// The main trait for defining an ffx tool. This is not intended
/// to be implemented directly by the user, but instead derived via
/// `#[derive(FfxTool)]`. Tools that have subcommands ("suites") should
/// instead use `FfxSubtoolSuite`.
// An FfxTool must be sized, but we enforce it only in certain methods, not
// in the trait itself. This is required for FfxTool to be "dyn compatible"
// (https://doc.rust-lang.org/std/keyword.dyn.html). It means that methods such
// as from_env() that need to know the size can still work, while allowing the
// dynamically-sized trait objects to be referred to in a dyn context.
pub trait FfxTool<E = Error>: FfxMain<Error = E>
where
    E: Into<Error> + 'static,
{
    type Command: FromArgs + SubCommand + ArgsInfo;

    fn supports_structured_output(&self) -> bool;
    fn has_schema(&self) -> bool;
    fn target_need() -> TargetNeed;

    fn from_env(env: FhoEnvironment, cmd: Self::Command) -> impl Future<Output = Result<Self>>
    where
        Self: Sized;

    /// Executes the tool. This is intended to be invoked by the user in main.
    fn execute_tool() -> impl Future<Output = ()>
    where
        Self: Sized,
    {
        async {
            let mut env_context = None;
            let result = match ffx_command::init_cmd(ExecutableKind::Subtool) {
                Ok(c) => {
                    env_context = Some(c.context.clone());
                    Box::pin(ffx_command::run::<FhoSuite<Self, E>>(c)).await
                }
                Err(e) => Err(e),
            };
            let should_format = match FfxCommandLine::from_env() {
                Ok(cli) => cli.global.should_format(),
                Err(e) => {
                    log::warn!("Received error getting command line: {}", e);
                    match e {
                        Error::Help { .. } => false,
                        _ => true,
                    }
                }
            };
            ffx_command::exit(env_context, result, should_format).await;
        }
    }
}

pub trait FfxMain {
    type Writer: TryFromEnv + ToolIO + 'static;
    type Error: Into<Error> + 'static;

    /// The entrypoint of the tool. Once FHO has set up the environment for the tool, this is
    /// invoked. Should not be invoked directly unless for testing.
    fn main(
        self,
        writer: Self::Writer,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>>;

    /// Given the writer, print the output schema. This is exposed to allow
    /// traversing the subtool adapters which combine more than one subtool which
    /// probably have different writers since they will have different output.
    fn try_print_schema(self, mut writer: Self::Writer) -> impl Future<Output = Result<()>>
    where
        Self: Sized,
    {
        async move { writer.try_print_schema().map_err(|e| e.into()) }
    }

    /// Returns the basename of the log file to use with this tool. With the exception
    /// of long running tools, subtools are strongly encouraged to use the default basename.
    fn log_basename(&self) -> Option<String> {
        None
    }
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub(crate) enum FhoHandler<M: FfxTool<E>, E = Error>
where
    E: Into<Error> + 'static,
{
    //FhoVersion1(M),
    /// Run the tool as if under ffx
    Standalone(M::Command),
    /// Print out the subtool's metadata json
    Metadata(MetadataCmd),
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum StandaloneFhoHandler<M: SubCommand> {
    //FhoVersion1(M),
    /// Run the tool as if under ffx
    Standalone(M),
    /// Print out the subtool's metadata json
    Metadata(MetadataCmd),
}

#[derive(Debug, FromArgs)]
#[argh(subcommand, name = "metadata", description = "Print out this subtool's FHO metadata json")]
pub struct MetadataCmd {
    #[argh(positional)]
    output_path: Option<PathBuf>,
}

impl MetadataCmd {
    #[allow(clippy::unused_async)]
    pub async fn run(self, info: &'static CommandInfo) -> Result<ExitStatus> {
        let meta = FhoToolMetadata::new(info.name, info.description);
        match &self.output_path {
            Some(path) => serde_json::to_writer_pretty(
                &File::create(path).with_user_message(|| {
                    format!("Failed to create metadata file {}", path.display())
                })?,
                &meta,
            ),
            None => serde_json::to_writer_pretty(&std::io::stdout(), &meta),
        }
        .user_message("Failed writing metadata")?;
        Ok(ExitStatus::from_raw(0))
    }
}

#[derive(FromArgs)]
/// Fuchsia Host Objects Runner for standalone commands.
pub struct StandaloneToolCommand<M: SubCommand> {
    #[argh(subcommand)]
    pub subcommand: StandaloneFhoHandler<M>,
}

#[derive(FromArgs)]
/// Fuchsia Host Objects Runner
pub(crate) struct ToolCommand<M: FfxTool<E>, E = Error>
where
    E: Into<Error> + 'static,
{
    #[argh(subcommand)]
    pub subcommand: FhoHandler<M, E>,
}

pub(crate) struct FhoSuite<M, E = Error> {
    context: EnvironmentContext,
    _p: std::marker::PhantomData<fn(M, E) -> ()>,
}

impl<M, E> Clone for FhoSuite<M, E> {
    fn clone(&self) -> Self {
        Self { context: self.context.clone(), _p: self._p.clone() }
    }
}

struct FhoTool<M: FfxTool<E>, E = Error>
where
    E: Into<Error> + 'static,
{
    env: FhoEnvironment,
    redacted_args: Vec<String>,
    subcommand_chain: Vec<String>,
    enhanced_args: Option<Vec<String>>,
    main: M,
    _p: std::marker::PhantomData<E>,
}

struct MetadataRunner {
    cmd: MetadataCmd,
    info: &'static CommandInfo,
}

impl ToolRunner for MetadataRunner {
    fn run<'a>(self: Box<Self>, _metrics: MetricsSession) -> LocalBoxFuture<'a, Result<ExitStatus>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            // We don't ever want to emit metrics for a metadata query, it's a tool-level
            // command
            self.cmd.run(self.info).await
        })
    }
}

impl<T: FfxTool<E>, E: Into<Error> + 'static> ToolRunner for FhoTool<T, E> {
    fn run<'a>(self: Box<Self>, metrics: MetricsSession) -> LocalBoxFuture<'a, Result<ExitStatus>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            if !analytics_command(&self.redacted_args.join(" ")) {
                metrics.print_notice(&mut std::io::stderr()).await?;
            }
            let writer = <<T as FfxMain>::Writer as TryFromEnv>::try_from_env(&self.env)
                .await
                .map_err(|e| Error::Unexpected(anyhow::Error::new(e)))?;
            let res: Result<ExitStatus> = if self.env.ffx_command().global.schema {
                if self.main.has_schema() {
                    self.main
                        .try_print_schema(writer)
                        .await
                        .map(|_| ExitStatus::from_raw(0))
                        .map_err(|e| e.into())
                } else {
                    Err(user_error!("--schema is not supported for this command (subtool)."))
                }
            } else {
                let r = self.main.main(writer).await;
                match r {
                    Ok(()) => Ok(ExitStatus::from_raw(0)),
                    Err(e) => {
                        let fho_err: Error = e.into();
                        log::error!("[{}]: {:?}", fho_err.diagnostic_code(), fho_err);
                        Err(fho_err)
                    }
                }
            };
            let res = metrics
                .command_finished(
                    &res,
                    &self.redacted_args,
                    &self.subcommand_chain,
                    false,
                    self.enhanced_args.as_deref(),
                )
                .await
                .and(res);
            self.env.wrap_main_result(res)
        })
    }
}

impl<T: FfxTool<E>, E: Into<Error> + 'static> FhoTool<T, E> {
    async fn build(
        context: &EnvironmentContext,
        ffx: FfxCommandLine,
        tool: T::Command,
    ) -> Result<Box<Self>> {
        check_strict_constraints(&ffx.global, Some(context), T::target_need())?;

        let env = FhoEnvironment::new(context, &ffx);
        ffx_diagnostics_analytics_state::set_command_line_context(env.ffx_command(), &tool);
        let redacted_args = ffx.redact_subcmd(&tool);
        let subcommand_chain = ffx.redact_subcmd_from_args_info::<T::Command>();
        let enhanced_args = match send_enhanced_analytics().await {
            false => None,
            true => Some(ffx.unredacted_args_for_analytics()),
        };
        let main = Box::pin(T::from_env(env.clone(), tool)).await?;

        let found = FhoTool {
            env,
            redacted_args,
            subcommand_chain,
            enhanced_args,
            main,
            _p: std::marker::PhantomData,
        };
        Ok(Box::new(found))
    }
}

impl<M: FfxTool<E>, E: Into<Error> + 'static> ToolSuite for FhoSuite<M, E> {
    fn from_env(context: &EnvironmentContext) -> Result<Self> {
        let context = context.clone();
        Ok(Self { context: context, _p: Default::default() })
    }

    fn global_command_list() -> &'static [&'static argh::CommandInfo] {
        FhoHandler::<M, E>::COMMANDS
    }

    fn subcommand_chain(&self, cmd: &FfxCommandLine) -> Vec<String> {
        cmd.redact_subcmd_from_args_info::<M::Command>()
    }

    async fn get_args_info(&self) -> Result<ffx_command::CliArgsInfo> {
        Ok(M::Command::get_args_info().into())
    }

    async fn try_from_args(
        &self,
        ffx: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>> {
        let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
        let command = ToolCommand::<M, E>::from_args(&Vec::from_iter(ffx.cmd_iter()), &args)
            .map_err(|err| Error::from_early_exit(&ffx.command, err))?;

        let res: Box<dyn ToolRunner> = match command.subcommand {
            FhoHandler::Metadata(cmd) => {
                Box::new(MetadataRunner { cmd, info: M::Command::COMMAND })
            }
            FhoHandler::Standalone(tool) => {
                FhoTool::<M, E>::build(&self.context, ffx.clone(), tool).await?
            }
        };
        Ok(Some(res))
    }

    async fn try_runner_from_name(
        &self,
        ffx: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>> {
        let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
        match ToolCommand::<M, E>::from_args(&Vec::from_iter(ffx.cmd_iter()), &args) {
            Ok(cmd) => {
                let res: Box<dyn ToolRunner> = match cmd.subcommand {
                    FhoHandler::Metadata(cmd) => {
                        Box::new(MetadataRunner { cmd, info: M::Command::COMMAND })
                    }
                    FhoHandler::Standalone(tool) => {
                        FhoTool::<M, E>::build(&self.context, ffx.clone(), tool).await?
                    }
                };
                return Ok(Some(res));
            }
            Err(err) => {
                return Err(Error::from_early_exit(&ffx.command, err));
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tests::SimpleCheck;
    // This keeps the macros from having compiler errors.
    use crate::adapters::tests::{FakeCommand, FakeTool, SIMPLE_CHECK_COUNTER, TestWriter};
    use crate::{self as fho};
    use fho_macro::FfxTool;
    use fho_metadata::{FhoDetails, Only};

    // The main testing part will happen in the `main()` function of the tool.
    #[fuchsia::test]
    async fn test_run_fake_tool() {
        let config_env = ffx_config::test_init().unwrap();
        let ffx = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).expect("test ffx cmd");
        let fho_env = FhoEnvironment::new(&config_env.context, &ffx);
        let writer = TestWriter;
        let fake_tool: FakeTool = build_tool(fho_env).await.expect("build fake tool");
        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            1,
            "tool pre-check should have been called once"
        );
        fake_tool.main(writer).await.unwrap();
    }

    #[fuchsia::test]
    async fn negative_precheck_fails() {
        #[derive(Debug, FfxTool)]
        #[check(SimpleCheck(false))]
        struct FakeToolWillFail {
            #[command]
            _fake_command: FakeCommand,
        }
        impl FfxMain for FakeToolWillFail {
            type Writer = TestWriter;
            type Error = Error;
            async fn main(self, _writer: Self::Writer) -> Result<(), Self::Error> {
                panic!("This should never get called")
            }
        }

        let config_env = ffx_config::test_init().unwrap();
        let ffx = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).expect("test ffx cmd");
        let fho_env = FhoEnvironment::new(&config_env.context, &ffx);

        build_tool::<FakeToolWillFail>(fho_env)
            .await
            .expect_err("Should not have been able to create tool with a negative pre-check");
        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            1,
            "tool pre-check should have been called once"
        );
    }

    #[fuchsia::test]
    async fn present_metadata() {
        let test_env = ffx_config::test_init().expect("Test env initialization failed");
        let tmpdir = tempfile::tempdir().expect("tempdir");

        let output_path = tmpdir.path().join("metadata.json");
        let cmd = MetadataCmd { output_path: Some(output_path.clone()) };
        let tool = Box::new(MetadataRunner { cmd, info: FakeCommand::COMMAND });
        let metrics = MetricsSession::start(&test_env.context).await.expect("Session start");

        tool.run(metrics).await.expect("running metadata command");

        let read_metadata: FhoToolMetadata =
            serde_json::from_reader(File::open(output_path).expect("opening metadata"))
                .expect("parsing metadata");
        assert_eq!(
            read_metadata,
            FhoToolMetadata {
                name: "fake".to_owned(),
                description: "fake command".to_owned(),
                requires_fho: 0,
                fho_details: FhoDetails::FhoVersion0 { version: Only },
            }
        );
    }

    pub async fn build_tool<T: FfxTool>(env: FhoEnvironment) -> Result<T> {
        let tool_cmd = ToolCommand::<T>::from_args(
            &Vec::from_iter(env.ffx_command().cmd_iter()),
            &Vec::from_iter(env.ffx_command().subcmd_iter()),
        )
        .unwrap();
        let fho::subtool::FhoHandler::Standalone(cmd) = tool_cmd.subcommand else {
            panic!("Not testing metadata generation");
        };
        T::from_env(env, cmd).await
    }

    #[fuchsia::test]
    async fn test_fho_suite_subcommand_chain() {
        let config_env = ffx_config::test_init().unwrap();
        let suite = FhoSuite::<FakeTool>::from_env(&config_env.context).unwrap();
        let ffx = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).expect("test ffx cmd");
        assert_eq!(suite.subcommand_chain(&ffx), vec!["fake"]);
    }

    #[fuchsia::test]
    async fn test_inferred_target_need_and_overrides() {
        use crate::{Deferred, TryFromEnvWith, deferred};
        use std::marker::PhantomData;
        use std::rc::Rc;
        use std::sync::Arc;

        struct NeedsTarget;
        impl TryFromEnv for NeedsTarget {
            const TARGET_NEED: TargetNeed = TargetNeed::Required;
            type Error = std::convert::Infallible;
            async fn try_from_env(_: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
                Ok(Self)
            }
        }

        // Generic decorator whose output type is inferred from the field type, like `moniker`.
        struct WithTarget<P>(PhantomData<fn() -> P>);
        fn with_target<P>() -> WithTarget<P> {
            WithTarget(PhantomData)
        }
        impl<P: Default + 'static> TryFromEnvWith for WithTarget<P> {
            const TARGET_NEED: TargetNeed = TargetNeed::Required;
            type Output = P;
            type Error = std::convert::Infallible;
            async fn try_from_env_with(
                self,
                _: &FhoEnvironment,
            ) -> std::result::Result<P, Self::Error> {
                Ok(P::default())
            }
        }

        macro_rules! test_tool {
            ($(#[$attr:meta])* $name:ident { $($body:tt)* }) => {
                #[derive(FfxTool)]
                $(#[$attr])*
                struct $name {
                    #[command]
                    _cmd: FakeCommand,
                    $($body)*
                }
                impl FfxMain for $name {
                    type Writer = TestWriter;
                    type Error = Error;
                    async fn main(self, _: Self::Writer) -> Result<()> {
                        Ok(())
                    }
                }
            };
        }

        test_tool!(NoTarget { _ctx: EnvironmentContext });
        test_tool!(Direct { _t: NeedsTarget });
        test_tool!(DeferredField { _d: Deferred<NeedsTarget, std::convert::Infallible> });
        test_tool!(WithField {
            #[with(with_target())]
            _p: String
        });
        test_tool!(WithDeferredField {
            #[with(deferred(with_target()))]
            _p: Deferred<String, std::convert::Infallible>
        });
        test_tool!(
            #[target(None)]
            OverrideNone { _t: NeedsTarget }
        );
        test_tool!(
            #[target(Optional)]
            OverrideOptional { _t: NeedsTarget }
        );
        test_tool!(
            #[target(Required)]
            OverrideRequired {}
        );

        assert_eq!(<Arc<NeedsTarget> as TryFromEnv>::TARGET_NEED, TargetNeed::Required);
        assert_eq!(<Rc<NeedsTarget> as TryFromEnv>::TARGET_NEED, TargetNeed::Required);
        assert_eq!(<Box<NeedsTarget> as TryFromEnv>::TARGET_NEED, TargetNeed::Required);
        assert_eq!(
            <std::result::Result<NeedsTarget, std::convert::Infallible> as TryFromEnv>::TARGET_NEED,
            TargetNeed::Required,
        );
        assert_eq!(NoTarget::target_need(), TargetNeed::None);
        assert_eq!(Direct::target_need(), TargetNeed::Required);
        assert_eq!(DeferredField::target_need(), TargetNeed::Optional);
        assert_eq!(WithField::target_need(), TargetNeed::Required);
        assert_eq!(WithDeferredField::target_need(), TargetNeed::Optional);
        assert_eq!(OverrideNone::target_need(), TargetNeed::None);
        assert_eq!(OverrideOptional::target_need(), TargetNeed::Optional);
        assert_eq!(OverrideRequired::target_need(), TargetNeed::Required);
    }
}
