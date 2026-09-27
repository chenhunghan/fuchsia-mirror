// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use argh::FromArgs;
use driver_debug_lib::{connect_to_debug_protocol, execute_command, list_commands};
use std::process::ExitCode;

/// Debug CLI tool to interact with drivers via fuchsia.driver.debug.Debug.
///
/// This tool is typically invoked inside an explore shell on a driver component:
///   `ffx component explore <driver_moniker>`
/// or non-interactively via:
///   `ffx component explore <driver_moniker> --command "debug ..."`
///
/// Inside `component explore`, the driver's outgoing directory is mounted at `/out`,
/// allowing `debug` to connect to `/out/svc/fuchsia.driver.debug.Debug` by default.
#[derive(FromArgs, Debug, PartialEq)]
struct Args {
    /// path to the fuchsia.driver.debug.Debug service node.
    /// Defaults to `/out/svc/fuchsia.driver.debug.Debug` or `/svc/fuchsia.driver.debug.Debug`.
    #[argh(option, short = 's')]
    service: Option<String>,

    /// list all available debug commands supported by the driver.
    #[argh(switch)]
    list_commands: bool,

    /// command and arguments to execute on the driver.
    #[argh(positional, greedy)]
    args: Vec<String>,
}

impl Args {
    fn should_list_commands(&self) -> bool {
        self.list_commands
            || (self.args.len() == 1
                && (self.args[0] == "list-commands"
                    || self.args[0] == "--list-commands"
                    || self.args[0] == "commands"))
    }
}

fn clone_as_socket(fd: impl std::os::fd::AsFd, name: &str) -> Result<zx::Socket> {
    let handle = fdio::clone_fd(fd).with_context(|| format!("Failed to clone {name} fd"))?;
    if handle.is_invalid() {
        anyhow::bail!("{name} is an invalid handle");
    }
    let basic_info =
        handle.basic_info().with_context(|| format!("Failed to query basic info for {name}"))?;
    if basic_info.object_type != zx::ObjectType::SOCKET {
        anyhow::bail!(
            "{name} file descriptor is not backed by a socket (got {:?})",
            basic_info.object_type
        );
    }
    Ok(zx::Socket::from(handle))
}

#[fuchsia::main]
async fn main() -> Result<ExitCode> {
    let args: Args = argh::from_env();

    let should_list = args.should_list_commands();

    if !should_list && args.args.is_empty() {
        eprintln!("Error: No command specified.\n");
        eprintln!("{}", Args::from_args(&["debug"], &["--help"]).unwrap_err().output);
        return Ok(ExitCode::FAILURE);
    }

    let proxy = connect_to_debug_protocol(args.service.as_deref())?;

    if should_list {
        let table = list_commands(&proxy).await?;
        print!("{table}");
        return Ok(ExitCode::SUCCESS);
    }

    let stdout = clone_as_socket(&std::io::stdout(), "stdout")?;
    let stderr = clone_as_socket(&std::io::stderr(), "stderr")?;

    let exit_code = execute_command(&proxy, &args.args, stdout, stderr).await?;
    let exit_u8 = (exit_code & 0xff) as u8;
    Ok(ExitCode::from(exit_u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ping() {
        let args = Args::from_args(&["debug"], &["ping"]).expect("parse args");
        assert_eq!(args.service, None);
        assert!(!args.list_commands);
        assert_eq!(args.args, vec!["ping".to_string()]);
        assert!(!args.should_list_commands());
    }

    #[test]
    fn test_parse_reset_with_flags() {
        let args = Args::from_args(&["debug"], &["reset", "--hard"]).expect("parse args");
        assert_eq!(args.service, None);
        assert!(!args.list_commands);
        assert_eq!(args.args, vec!["reset".to_string(), "--hard".to_string()]);
        assert!(!args.should_list_commands());
    }

    #[test]
    fn test_parse_custom_service() {
        let args =
            Args::from_args(&["debug"], &["-s", "/custom/path", "ping"]).expect("parse args");
        assert_eq!(args.service.as_deref(), Some("/custom/path"));
        assert!(!args.list_commands);
        assert_eq!(args.args, vec!["ping".to_string()]);
        assert!(!args.should_list_commands());

        let args2 = Args::from_args(&["debug"], &["--service", "/other/path", "status"])
            .expect("parse args");
        assert_eq!(args2.service.as_deref(), Some("/other/path"));
        assert_eq!(args2.args, vec!["status".to_string()]);
    }

    #[test]
    fn test_parse_list_commands_switch() {
        let args = Args::from_args(&["debug"], &["--list-commands"]).expect("parse args");
        assert!(args.list_commands);
        assert!(args.args.is_empty());
        assert!(args.should_list_commands());
    }

    #[test]
    fn test_parse_list_commands_positional_aliases() {
        let args1 = Args::from_args(&["debug"], &["list-commands"]).expect("parse args");
        assert!(!args1.list_commands);
        assert_eq!(args1.args, vec!["list-commands".to_string()]);
        assert!(args1.should_list_commands());

        let args2 = Args::from_args(&["debug"], &["commands"]).expect("parse args");
        assert!(!args2.list_commands);
        assert_eq!(args2.args, vec!["commands".to_string()]);
        assert!(args2.should_list_commands());
    }

    #[test]
    fn test_empty_args_behavior() {
        let args = Args::from_args(&["debug"], &[]).expect("parse args");
        assert_eq!(args.service, None);
        assert!(!args.list_commands);
        assert!(args.args.is_empty());
        assert!(!args.should_list_commands());
    }

    #[test]
    fn test_help() {
        let exit = Args::from_args(&["debug"], &["--help"]).unwrap_err();
        assert!(exit.output.contains("Usage: debug [-s <service>] [--list-commands] [args...]"));
        assert!(exit.output.contains("--service"));
        assert!(exit.output.contains("--list-commands"));
    }
}
