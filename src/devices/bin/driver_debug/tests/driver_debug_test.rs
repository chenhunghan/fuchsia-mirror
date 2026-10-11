// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use driver_debug_lib::{execute_command, list_commands};
use fdf_component::testing::harness::TestHarness;
use fidl_fuchsia_driver_debug as fdebug;

mod test_driver;
use test_driver::TestDriver;

struct ExecutionResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

async fn run_command(proxy: &fdebug::DebugProxy, args: &[&str]) -> ExecutionResult {
    let (local_stdout, remote_stdout) = zx::Socket::create_stream();
    let (local_stderr, remote_stderr) = zx::Socket::create_stream();

    let mut async_stdout = fuchsia_async::Socket::from_socket(local_stdout);
    let mut async_stderr = fuchsia_async::Socket::from_socket(local_stderr);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();

    use futures::io::AsyncReadExt;
    let stdout_reader = async {
        async_stdout.read_to_end(&mut stdout_bytes).await.unwrap();
    };
    let stderr_reader = async {
        async_stderr.read_to_end(&mut stderr_bytes).await.unwrap();
    };

    let args_strings: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let execute_fut = async {
        execute_command(proxy, &args_strings, remote_stdout, remote_stderr)
            .await
            .expect("execute command")
    };

    let (_, _, exit_code) = futures::join!(stdout_reader, stderr_reader, execute_fut);
    ExecutionResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
    }
}

#[fuchsia::test]
async fn test_rust_driver_debug_e2e() {
    let mut harness = TestHarness::<TestDriver>::new();
    let dut = harness.start_driver().await.expect("driver start");
    let proxy: fdebug::DebugProxy =
        dut.driver_outgoing().connect_protocol::<fdebug::DebugProxy>().expect("connect");

    // 1. debug list-commands
    let table = list_commands(&proxy).await.expect("list commands");
    assert!(table.contains("COMMAND"));
    assert!(table.contains("DESCRIPTION"));
    assert!(table.contains("ping"));
    assert!(table.contains("Ping the driver"));
    assert!(table.contains("reset"));
    assert!(table.contains("Reset the driver state"));
    assert!(table.contains("fault"));
    assert!(table.contains("Induce driver fault"));

    // 2. debug ping -c 3
    let res = run_command(&proxy, &["ping", "-c", "3"]).await;
    assert_eq!(res.exit_code, 0);
    assert_eq!(res.stdout, "ping 1\nping 2\nping 3\n");
    assert_eq!(res.stderr, "");

    // 3. debug reset --hard
    let res = run_command(&proxy, &["reset", "--hard"]).await;
    assert_eq!(res.exit_code, 0);
    assert_eq!(res.stdout, "hard reset\n");
    assert_eq!(res.stderr, "");

    // 4. debug --help
    let res = run_command(&proxy, &["--help"]).await;
    assert_eq!(res.exit_code, 0);
    assert!(res.stdout.contains("Usage:"));
    assert_eq!(res.stderr, "");

    // 5. syntax error (exit 2)
    let res = run_command(&proxy, &["ping", "--unknown-flag"]).await;
    assert_eq!(res.exit_code, 2);
    assert_eq!(res.stdout, "");
    assert!(!res.stderr.is_empty());

    // 6. driver fault (exit 1)
    let res = run_command(&proxy, &["fault"]).await;
    assert_eq!(res.exit_code, 1);
    assert_eq!(res.stdout, "");
    assert!(res.stderr.contains("driver internal fault"));

    dut.stop_driver().await;
}
