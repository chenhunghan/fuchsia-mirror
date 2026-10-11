// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper library for productionized driver debugging.

pub use anyhow;
pub use argh::{self, FromArgs, SubCommands};
pub use fidl;
pub use fidl_fuchsia_driver_debug;
pub use zx;

use core::future::Future;
use fidl_fuchsia_driver_debug as fdebug;
use futures::TryStreamExt as _;

/// Result of executing a driver debug command.
#[derive(Debug, PartialEq, Clone)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Driver debug commands.
#[derive(FromArgs)]
struct TopLevel<C: SubCommands> {
    #[argh(subcommand)]
    cmd: C,
}

/// Returns metadata for all supported subcommands of `C`.
pub fn command_info<C: SubCommands>() -> Vec<fdebug::CommandInfo> {
    C::COMMANDS
        .iter()
        .copied()
        .chain(C::dynamic_commands().iter().copied())
        .map(|info| fdebug::CommandInfo {
            name: Some(info.name.to_string()),
            description: Some(info.description.to_string()),
            ..Default::default()
        })
        .collect()
}

/// Parses a slice of argument strings into the subcommand type `C`.
///
/// On `--help` or syntax error, returns an `Err(ExecutionResult)` ready to be sent back to the
/// caller.
pub fn parse_args<C: SubCommands>(args: &[impl AsRef<str>]) -> Result<C, ExecutionResult> {
    let args_strs: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    match TopLevel::<C>::from_args(&["debug"], &args_strs) {
        Ok(TopLevel { cmd }) => Ok(cmd),
        Err(early_exit) => {
            if early_exit.status.is_ok() {
                Err(ExecutionResult {
                    stdout: early_exit.output,
                    stderr: String::new(),
                    exit_code: 0,
                })
            } else {
                Err(ExecutionResult {
                    stdout: String::new(),
                    stderr: early_exit.output,
                    exit_code: 2,
                })
            }
        }
    }
}

/// Parses arguments into `C` and executes `handler` on the parsed subcommand, returning an
/// [`ExecutionResult`].
pub async fn dispatch<C, F, Fut>(args: &[impl AsRef<str>], handler: F) -> ExecutionResult
where
    C: SubCommands,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<String, anyhow::Error>>,
{
    match parse_args::<C>(args) {
        Ok(cmd) => match handler(cmd).await {
            Ok(stdout) => ExecutionResult { stdout, stderr: String::new(), exit_code: 0 },
            Err(err) => ExecutionResult {
                stdout: String::new(),
                stderr: format!("{err:#}\n"),
                exit_code: 1,
            },
        },
        Err(res) => res,
    }
}

/// Responder for a single `fuchsia.driver.debug.Debug/Execute` request.
pub struct CommandResponder {
    stdout: zx::Socket,
    stderr: zx::Socket,
    responder: fdebug::DebugExecuteResponder,
}

impl CommandResponder {
    /// Writes the command result (`Ok(stdout)` or `Err(error)`) to the appropriate socket and
    /// sends the exit code over the FIDL responder.
    pub fn send(
        self,
        result: Result<impl AsRef<str>, impl std::fmt::Display>,
    ) -> Result<(), fidl::Error> {
        let exec_result = match result {
            Ok(stdout) => ExecutionResult {
                stdout: stdout.as_ref().to_string(),
                stderr: String::new(),
                exit_code: 0,
            },
            Err(err) => ExecutionResult {
                stdout: String::new(),
                stderr: format!("{err:#}\n"),
                exit_code: 1,
            },
        };
        self.send_execution_result(exec_result)
    }

    /// Writes an [`ExecutionResult`] to the stdout/stderr sockets and sends the exit code over the
    /// FIDL responder.
    pub fn send_execution_result(self, result: ExecutionResult) -> Result<(), fidl::Error> {
        let Self { stdout, stderr, responder } = self;
        if !result.stdout.is_empty() {
            let _ = stdout.write(result.stdout.as_bytes());
        }
        drop(stdout);
        if !result.stderr.is_empty() {
            let _ = stderr.write(result.stderr.as_bytes());
        }
        drop(stderr);
        responder.send(Ok(result.exit_code))
    }
}

/// Reads the next parsed subcommand from a `DebugRequestStream`.
///
/// Automatically responds to `ListCommands` requests using `C`'s subcommand metadata, as well as
/// `--help` and syntax errors on `Execute` requests. Returns `Ok(None)` when the stream closes.
pub async fn next_command<C: SubCommands>(
    stream: &mut fdebug::DebugRequestStream,
) -> Result<Option<(C, CommandResponder)>, fidl::Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            fdebug::DebugRequest::ListCommands { responder } => {
                let commands = command_info::<C>();
                responder.send(Ok(&commands))?;
            }
            fdebug::DebugRequest::Execute { args, stdout, stderr, responder } => {
                let cmd_responder = CommandResponder { stdout, stderr, responder };
                match parse_args::<C>(&args) {
                    Ok(cmd) => return Ok(Some((cmd, cmd_responder))),
                    Err(result) => {
                        cmd_responder.send_execution_result(result)?;
                    }
                }
            }
            fdebug::DebugRequest::_UnknownMethod { .. } => {}
        }
    }
    Ok(None)
}

/// Serves a `DebugRequestStream` by parsing commands into `C` and invoking `handler` for each
/// command.
pub async fn serve<C, F, Fut>(
    mut stream: fdebug::DebugRequestStream,
    mut handler: F,
) -> Result<(), fidl::Error>
where
    C: SubCommands,
    F: FnMut(C) -> Fut,
    Fut: Future<Output = Result<String, anyhow::Error>>,
{
    while let Some((cmd, responder)) = next_command::<C>(&mut stream).await? {
        let result = handler(cmd).await;
        responder.send(result)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_async as fasync;

    #[derive(FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "ping")]
    /// Ping the driver.
    pub struct PingArgs {
        #[argh(option, short = 'c', default = "1", description = "ping count")]
        pub count: u32,
    }

    #[derive(FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "reset")]
    /// Reset the driver.
    pub struct ResetArgs {
        #[argh(switch, description = "perform a hard reset")]
        pub hard: bool,
    }

    #[derive(FromArgs, Debug, PartialEq)]
    #[argh(subcommand, name = "fail")]
    /// Trigger a failure.
    pub struct FailArgs {}

    #[derive(FromArgs, Debug, PartialEq)]
    #[argh(subcommand)]
    pub enum TestCommands {
        Ping(PingArgs),
        Reset(ResetArgs),
        Fail(FailArgs),
    }

    async fn handle_cmd(cmd: TestCommands) -> Result<String, anyhow::Error> {
        match cmd {
            TestCommands::Ping(args) => {
                let mut out = String::new();
                for i in 1..=args.count {
                    out.push_str(&format!("ping {i}\n"));
                }
                Ok(out)
            }
            TestCommands::Reset(args) => {
                if args.hard {
                    Ok("hard reset\n".to_string())
                } else {
                    Ok("soft reset\n".to_string())
                }
            }
            TestCommands::Fail(_) => Err(anyhow::anyhow!("device failure")),
        }
    }

    #[test]
    fn test_command_info() {
        let commands = command_info::<TestCommands>();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].name.as_deref(), Some("ping"));
        assert_eq!(commands[0].description.as_deref(), Some("Ping the driver."));
        assert_eq!(commands[1].name.as_deref(), Some("reset"));
        assert_eq!(commands[1].description.as_deref(), Some("Reset the driver."));
        assert_eq!(commands[2].name.as_deref(), Some("fail"));
        assert_eq!(commands[2].description.as_deref(), Some("Trigger a failure."));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dispatch_success() {
        let res = dispatch(&["ping", "-c", "2"], handle_cmd).await;
        assert_eq!(res.exit_code, 0);
        assert_eq!(res.stdout, "ping 1\nping 2\n");
        assert_eq!(res.stderr, "");

        let res = dispatch(&["reset", "--hard"], handle_cmd).await;
        assert_eq!(res.exit_code, 0);
        assert_eq!(res.stdout, "hard reset\n");
        assert_eq!(res.stderr, "");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dispatch_help() {
        let res = dispatch(&["--help"], handle_cmd).await;
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("Usage:"));
        assert_eq!(res.stderr, "");

        let res = dispatch(&["ping", "--help"], handle_cmd).await;
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("Usage:"));
        assert_eq!(res.stderr, "");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dispatch_syntax_error() {
        let res = dispatch(&["unknown-cmd"], handle_cmd).await;
        assert_eq!(res.exit_code, 2);
        assert_eq!(res.stdout, "");
        assert!(res.stderr.contains("Unrecognized") || !res.stderr.is_empty());

        let res = dispatch(&["ping", "--invalid"], handle_cmd).await;
        assert_eq!(res.exit_code, 2);
        assert_eq!(res.stdout, "");
        assert!(res.stderr.contains("Unrecognized") || !res.stderr.is_empty());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_dispatch_error() {
        let res = dispatch(&["fail"], handle_cmd).await;
        assert_eq!(res.exit_code, 1);
        assert_eq!(res.stdout, "");
        assert!(res.stderr.contains("device failure"));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_next_command() {
        use futures::io::AsyncReadExt;

        let (proxy, mut stream) = create_proxy_and_stream::<fdebug::DebugMarker>();

        let task = fasync::Task::spawn(async move {
            while let Some((cmd, responder)) = next_command(&mut stream).await.unwrap() {
                let res = handle_cmd(cmd).await;
                responder.send(res).unwrap();
            }
        });

        let commands = proxy.list_commands().await.unwrap().unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].name.as_deref(), Some("ping"));

        let (stdout_local, stdout_remote) = zx::Socket::create_stream();
        let (stderr_local, stderr_remote) = zx::Socket::create_stream();

        let mut async_stdout = fasync::Socket::from_socket(stdout_local);
        let mut async_stderr = fasync::Socket::from_socket(stderr_local);

        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();

        let stdout_reader = async {
            async_stdout.read_to_end(&mut stdout_bytes).await.unwrap();
        };
        let stderr_reader = async {
            async_stderr.read_to_end(&mut stderr_bytes).await.unwrap();
        };
        let execute_fut = async {
            proxy
                .execute(
                    &["ping".to_string(), "-c".to_string(), "3".to_string()],
                    stdout_remote,
                    stderr_remote,
                )
                .await
                .unwrap()
                .unwrap()
        };

        let (_, _, exit_code) = futures::join!(stdout_reader, stderr_reader, execute_fut);
        assert_eq!(exit_code, 0);
        assert_eq!(String::from_utf8(stdout_bytes).unwrap(), "ping 1\nping 2\nping 3\n");
        assert_eq!(String::from_utf8(stderr_bytes).unwrap(), "");

        drop(proxy);
        task.await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_serve() {
        use futures::io::AsyncReadExt;

        let (proxy, stream) = create_proxy_and_stream::<fdebug::DebugMarker>();

        let task = fasync::Task::spawn(async move {
            serve(stream, handle_cmd).await.expect("serve");
        });

        let (stdout_local, stdout_remote) = zx::Socket::create_stream();
        let (stderr_local, stderr_remote) = zx::Socket::create_stream();

        let mut async_stdout = fasync::Socket::from_socket(stdout_local);
        let mut async_stderr = fasync::Socket::from_socket(stderr_local);

        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();

        let stdout_reader = async {
            async_stdout.read_to_end(&mut stdout_bytes).await.unwrap();
        };
        let stderr_reader = async {
            async_stderr.read_to_end(&mut stderr_bytes).await.unwrap();
        };
        let execute_fut = async {
            proxy
                .execute(&["reset".to_string(), "--hard".to_string()], stdout_remote, stderr_remote)
                .await
                .unwrap()
                .unwrap()
        };

        let (_, _, exit_code) = futures::join!(stdout_reader, stderr_reader, execute_fut);
        assert_eq!(exit_code, 0);
        assert_eq!(String::from_utf8(stdout_bytes).unwrap(), "hard reset\n");
        assert_eq!(String::from_utf8(stderr_bytes).unwrap(), "");

        drop(proxy);
        task.await;
    }
}
