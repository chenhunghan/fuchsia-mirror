// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use fidl_fuchsia_driver_debug as fdebug;
use fuchsia_component::client::connect_to_protocol_at_path;
use std::path::Path;

pub const DEFAULT_OUT_SVC_PATH: &str = "/out/svc/fuchsia.driver.debug.Debug";
pub const DEFAULT_SVC_PATH: &str = "/svc/fuchsia.driver.debug.Debug";

/// Connects to the `fuchsia.driver.debug.Debug` protocol at the given path or default paths.
pub fn connect_to_debug_protocol(custom_path: Option<&str>) -> Result<fdebug::DebugProxy> {
    if let Some(path) = custom_path {
        return connect_to_protocol_at_path::<fdebug::DebugMarker>(path)
            .with_context(|| format!("Failed to connect to Debug protocol at {path}"));
    }

    if Path::new(DEFAULT_OUT_SVC_PATH).exists() {
        if let Ok(proxy) = connect_to_protocol_at_path::<fdebug::DebugMarker>(DEFAULT_OUT_SVC_PATH)
        {
            return Ok(proxy);
        }
    }

    connect_to_protocol_at_path::<fdebug::DebugMarker>(DEFAULT_SVC_PATH)
        .with_context(|| format!("Failed to connect to Debug protocol at {DEFAULT_SVC_PATH}"))
}

/// Formats a list of `CommandInfo` into a human-readable table.
pub fn format_command_info_table(commands: &[fdebug::CommandInfo]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:<20} {}\n", "COMMAND", "DESCRIPTION"));
    out.push_str(&format!("{:<20} {}\n", "-------", "-----------"));
    for cmd in commands {
        let name = cmd.name.as_deref().unwrap_or("<unknown>");
        let desc = cmd.description.as_deref().unwrap_or("");
        out.push_str(&format!("{:<20} {}\n", name, desc));
    }
    out
}

/// Queries the debug proxy for supported commands and returns the formatted table.
pub async fn list_commands(proxy: &fdebug::DebugProxy) -> Result<String> {
    let commands = proxy
        .list_commands()
        .await
        .context("FIDL error calling ListCommands")?
        .map_err(|s| anyhow!("ListCommands error: {}", zx::Status::err_from_raw(s)))?;
    Ok(format_command_info_table(&commands))
}

/// Executes a debug command with the given arguments over the FIDL proxy.
pub async fn execute_command(
    proxy: &fdebug::DebugProxy,
    args: &[String],
    stdout: zx::Socket,
    stderr: zx::Socket,
) -> Result<i32> {
    proxy
        .execute(args, stdout, stderr)
        .await
        .context("FIDL error calling Execute")?
        .map_err(|s| anyhow!("Execute error: {}", zx::Status::err_from_raw(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_command_info_table() {
        let commands = vec![
            fdebug::CommandInfo {
                name: Some("ping".to_string()),
                description: Some("Ping driver".to_string()),
                ..Default::default()
            },
            fdebug::CommandInfo {
                name: Some("reset".to_string()),
                description: Some("Reset device".to_string()),
                ..Default::default()
            },
        ];
        let table = format_command_info_table(&commands);
        assert!(table.contains("COMMAND"));
        assert!(table.contains("DESCRIPTION"));
        assert!(table.contains("ping"));
        assert!(table.contains("Ping driver"));
        assert!(table.contains("reset"));
        assert!(table.contains("Reset device"));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_execute_command() {
        use futures::StreamExt;
        use futures::io::AsyncReadExt;

        let (proxy, mut stream) = fidl::endpoints::create_proxy_and_stream::<fdebug::DebugMarker>();

        let (local_stdout, remote_stdout) = zx::Socket::create_stream();
        let (local_stderr, remote_stderr) = zx::Socket::create_stream();

        let mut async_stdout = fuchsia_async::Socket::from_socket(local_stdout);
        let mut async_stderr = fuchsia_async::Socket::from_socket(local_stderr);

        let server = async move {
            if let Some(request) = stream.next().await {
                match request.expect("stream request") {
                    fdebug::DebugRequest::Execute { args, stdout, stderr, responder } => {
                        assert_eq!(args, vec!["echo".to_string(), "hello".to_string()]);
                        let _ = stdout.write(b"hello world\n").expect("write to stdout");
                        let _ = stderr.write(b"no errors\n").expect("write to stderr");
                        drop(stdout);
                        drop(stderr);
                        responder.send(Ok(0)).expect("send response");
                    }
                    _ => panic!("unexpected request"),
                }
            }
        };

        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();

        let stdout_reader = async {
            async_stdout.read_to_end(&mut stdout_bytes).await.unwrap();
        };
        let stderr_reader = async {
            async_stderr.read_to_end(&mut stderr_bytes).await.unwrap();
        };

        let client = async move {
            execute_command(
                &proxy,
                &["echo".to_string(), "hello".to_string()],
                remote_stdout,
                remote_stderr,
            )
            .await
            .expect("execute command")
        };

        let (_, _, _, exit_code) = futures::join!(server, stdout_reader, stderr_reader, client);
        assert_eq!(exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&stdout_bytes), "hello world\n");
        assert_eq!(String::from_utf8_lossy(&stderr_bytes), "no errors\n");
    }
}
