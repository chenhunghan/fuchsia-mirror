// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use argh::{ArgsInfo, FromArgs, SubCommand};
use fho::subtool::{MetadataCmd, StandaloneFhoHandler, StandaloneToolCommand};
use fho::{FfxContext, Result};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use usb_driver_impl::UnixListener;

/// Number of log file rotations to keep.
const LOG_ROTATIONS: usize = 5;

/// Default SDK version reported to analytics when no SDK is configured.
const UNKNOWN_SDK: &str = "Unknown SDK";

// [START command_struct]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "usb-driver")]
/// Drive USB devices to/from ffx
pub struct UsbDriverCommand {
    #[argh(switch)]
    /// whether to fork the driver process into the background rather than run
    background: bool,

    #[argh(option)]
    /// directory where log file should be placed
    log_dir: Option<String>,

    #[argh(option)]
    /// only allow this driver daemon to see the device with the given serial
    /// number
    serial: Option<String>,
}
// [END command_struct]

#[derive(Debug)]
pub enum Action {
    ExitStatus(ExitStatus),
    MetadataCommand(MetadataCmd),
    RunDriver(UnixListener, String, Option<String>),
}

/// Runs the USB driver tool.
///
/// # Safety
///
/// This function may daemonize the process, and thus can only be safely called
/// when it is known to be safe to fork the process without violating Rust's
/// invariants (e.g. no background threads or async executors are running that
/// would be invalidated by a fork).
pub unsafe fn run() {
    let mut env_context = None;
    let mut analytics_config = None;
    let mut logging_enabled = false;
    let result = match ffx_command::init_cmd(ffx_config::environment::ExecutableKind::Subtool) {
        Ok(c) => {
            let enabled = c.context.analytics_enabled();
            let path = c.context.get_analytics_path();
            let build_info = c.context.build_info();
            let invoker = c.context.get("fuchsia.analytics.ffx_invoker").unwrap_or(None);
            let sdk_version = if enabled {
                c.context
                    .get_sdk()
                    .ok()
                    .and_then(|sdk| sdk.get_version_string())
                    .unwrap_or_else(|| UNKNOWN_SDK.to_string())
            } else {
                UNKNOWN_SDK.to_string()
            };
            analytics_config = Some((enabled, path, build_info, invoker, sdk_version));
            env_context = Some(c.context.clone());
            // SAFETY:
            // Obligation: `implementation` requires that no background threads or
            // async executors are running (because it may fork).
            // Artifact facts:
            // - A1: `run` is marked unsafe with the precondition that the caller
            //   guarantees no background threads or async executors are running.
            // Semantic premises:
            // - S1: Operations yielding `analytics_config` (`analytics_enabled`,
            //   `get_sdk`, etc.) are synchronous context lookups that do not spawn
            //   threads.
            // Derivation:
            // - A1 establishes the invariant on entry.
            // - S1 establishes the invariant is preserved until `implementation`.
            // Result:
            // - The process fulfills the safe-to-fork precondition of `implementation`.
            unsafe { implementation(c, &mut logging_enabled) }
        }
        Err(e) => Err(e),
    };
    let should_format = match fho::FfxCommandLine::from_env() {
        Ok(cli) => cli.global.should_format(),
        Err(e) => {
            if logging_enabled {
                log::warn!("Received error getting command line: {}", e);
            } else {
                eprintln!("Received error getting command line: {}", e);
            }
            match e {
                fho::Error::Help { .. } => false,
                _ => true,
            }
        }
    };

    // We have to defer launching the Tokio runtime until here otherwise
    // daemonizing might break its thread pools.
    fuchsia_async::LocalExecutorBuilder::new().build().run_singlethreaded(async move {
        let result = match result {
            Ok(Action::ExitStatus(status)) => Ok(status),
            Ok(Action::MetadataCommand(cmd)) => cmd.run(UsbDriverCommand::COMMAND).await,
            Ok(Action::RunDriver(listener, log_path, serial)) => {
                if let Some((enabled, path, build_info, invoker, sdk_version)) = analytics_config {
                    ffx_metrics::init_metrics_svc(path, build_info, invoker, sdk_version).await;
                    if !enabled {
                        if let Err(e) = analytics::opt_out_for_this_invocation().await {
                            log::warn!("Could not opt out of analytics for this invocation: {e}");
                        }
                    }
                }
                usb_driver_impl::HostDriver::run(listener, log_path, serial).await;
                Ok(ExitStatus::from_raw(0))
            }
            Err(e) => Err(e),
        };
        ffx_command::exit(env_context, result, should_format).await;
    })
}

/// # Safety
///
/// This function can only be safely called when it is known to be safe to fork
/// the process without violating Rust's invariants.
unsafe fn implementation(
    icmd: ffx_command::InitializedCmd,
    logging_enabled: &mut bool,
) -> Result<Action> {
    let ffx_command::InitializedCmd { cmd: ffx, context: ctx, help_state } = icmd;

    match help_state {
        ffx_command::HelpState::ReturnArgsInfo => {
            let args_info = ffx_command::CliArgsInfo::from(UsbDriverCommand::get_args_info());
            let output = match ffx.global.machine.unwrap() {
                ffx_command::MachineFormat::Json => serde_json::to_string(&args_info),
                ffx_command::MachineFormat::JsonPretty => serde_json::to_string_pretty(&args_info),
                ffx_command::MachineFormat::Raw => Ok(format!("{args_info:#?}")),
            };
            println!("{}", output.bug_context("Error serializing args")?);
            return Ok(Action::ExitStatus(ExitStatus::from_raw(0)));
        }
        ffx_command::HelpState::ReturnHelp { command, output, code } => {
            return Err(fho::Error::Help { command, output, code });
        }
        ffx_command::HelpState::None => (),
    }

    let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
    let command = StandaloneToolCommand::<UsbDriverCommand>::from_args(
        &Vec::from_iter(ffx.cmd_iter()),
        &args,
    )
    .map_err(|err| ffx_command::Error::from_early_exit(&ffx.command, err))?;

    let command = match command.subcommand {
        StandaloneFhoHandler::Metadata(metadata_cmd) => {
            return Ok(Action::MetadataCommand(metadata_cmd));
        }
        StandaloneFhoHandler::Standalone(cmd) => cmd,
    };

    let (socket_path, found_config) = ctx
        .query(usb_driver_api::CONFIG_USB_SOCKET_PATH)
        .level(Some(ffx_config::ConfigLevel::Runtime))
        .build()
        .get::<PathBuf>(&ctx)
        .map(|x| (x, true))
        .or_else(|_| -> fho::Result<_> {
            ctx.query(usb_driver_api::CONFIG_USB_SOCKET_PATH)
                .level(Some(ffx_config::ConfigLevel::Default))
                .build()
                .get::<PathBuf>(&ctx)
                .map(|ret| (ret, false))
                .map_err(|e| fho::Error::Unexpected(e.into()))
        })?;

    let path_sha2 = Sha256::digest(socket_path.as_os_str().as_encoded_bytes());
    let log_id = u64::from_be_bytes(path_sha2[..8].try_into().unwrap());

    let (sink, log_path) = if command.background || command.log_dir.is_some() {
        let mut path = if let Some(log_dir) = &command.log_dir {
            PathBuf::from(log_dir)
        } else {
            let mut path = match ffx_config::get_state_base_path() {
                Ok(p) => p,
                Err(e) => return Err(ffx_command::Error::Config(e.into())),
            };

            path.push("ffx_usb");
            path
        };
        let _: Result<(), _> = std::fs::create_dir_all(&path);

        let usb_path = |rot: usize| path.join(format!("ffx_usb.{log_id:x}.{rot}.log"));

        for rot in (0..LOG_ROTATIONS).rev() {
            if rot + 1 == LOG_ROTATIONS {
                let _: Result<(), _> = std::fs::remove_file(usb_path(rot));
            } else {
                let _: Result<(), _> = std::fs::rename(usb_path(rot), usb_path(rot + 1));
            }
        }

        path.push(usb_path(0));

        let file = match OpenOptions::new().write(true).append(true).create(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                return Err(ffx_command::Error::Config(anyhow::anyhow!(
                    "Could not open log file: {e}"
                )));
            }
        };

        (
            Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(file))))
                as Box<dyn logging::LogSinkTrait>,
            path.to_string_lossy().to_string(),
        )
    } else {
        (
            Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(std::io::stderr()))))
                as Box<dyn logging::LogSinkTrait>,
            "stdout".to_owned(),
        )
    };

    if ffx.global.machine.is_some() {
        return Err(ffx_command::Error::User(anyhow::anyhow!(
            "The machine flag is not supported for this subcommand"
        )));
    }

    if ffx.global.schema {
        return Err(ffx_command::Error::User(anyhow::anyhow!(
            "Schema is not defined for this subcommand"
        )));
    }

    if !found_config
        && let Ok(p) = ctx.get::<PathBuf, _>(usb_driver_api::CONFIG_USB_SOCKET_PATH)
        && p != socket_path
    {
        return Err(fho::Error::User(anyhow::anyhow!(
            "{} must be set on the command line",
            usb_driver_api::CONFIG_USB_SOCKET_PATH
        )));
    }

    let listener = usb_driver_impl::remove_and_bind_socket(socket_path);

    if command.background
        && let Err(usb_driver_impl::RemoveAndBindError::InUse(_)) = listener
    {
        return Ok(Action::ExitStatus(ExitStatus::from_raw(0)));
    }

    let logger = logging::FfxLog::new(
        vec![sink],
        logging::FormatOpts::new(0),
        Filter,
        log::LevelFilter::Debug,
        logging::TargetsFilter::new(vec![]),
    );

    let _ = log::set_boxed_logger(Box::new(logger))
        .map(|()| log::set_max_level(log::LevelFilter::Trace));
    *logging_enabled = true;

    if command.background {
        // daemonize(3) is deprecated on macOS 10.15. The replacement is not
        // yet clear, we may want to replace this with a manual double fork
        // setsid, etc.
        #[allow(deprecated)]
        // First argument: chdir(/)
        // Second argument: close stdio
        //
        // SAFETY: This shouldn't do much of anything to memory state. If it
        // succeeds we've effectively just been shuffled around the process
        // table. If it fails then it likely has no side effects at all, but
        // even if it does we're going to exit as fast as we can anyway. It
        // will, of course, fork the process, which is why we the caller must be
        // marked unsafe.
        match unsafe { libc::daemon(0, 0) } {
            0 => (),
            x => return Err(fho::Error::Unexpected(std::io::Error::from_raw_os_error(x).into())),
        }
    }

    let listener = listener.map_err(anyhow::Error::from)?;

    if let Some(serial) = &command.serial {
        log::info!("Only interacting with devices with serial {serial}");
    }
    Ok(Action::RunDriver(listener, log_path, command.serial))
}

struct Filter;
impl logging::Filter for Filter {
    fn should_emit(&self, record: &log::Metadata<'_>) -> bool {
        // The logs from hyper are very noisy
        !record.target().starts_with("hyper")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use logging::Filter as _;

    #[test]
    fn test_usb_driver_command_from_args() {
        let cmd = UsbDriverCommand::from_args(
            &["usb-driver"],
            &["--background", "--log-dir", "/tmp/usb_logs", "--serial", "SER123"],
        )
        .unwrap();
        assert!(cmd.background);
        assert_eq!(cmd.log_dir.as_deref(), Some("/tmp/usb_logs"));
        assert_eq!(cmd.serial.as_deref(), Some("SER123"));
    }

    #[test]
    fn test_filter_suppresses_hyper_logs() {
        let filter = Filter;
        let hyper_meta =
            log::Metadata::builder().level(log::Level::Debug).target("hyper::proto::h1").build();
        let hyper_util_meta = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("hyper_util::client::legacy")
            .build();
        let driver_meta =
            log::Metadata::builder().level(log::Level::Debug).target("usb_driver_impl").build();

        assert!(!filter.should_emit(&hyper_meta));
        assert!(!filter.should_emit(&hyper_util_meta));
        assert!(filter.should_emit(&driver_meta));
    }
}
