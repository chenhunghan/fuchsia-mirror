// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use gtest_test_parser::{ChildGuard, DEFAULT_SIGNAL_GRACE_PERIOD, TestResult, TestStatus};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;

const PARSER_BINARY_PATH: &str = "./gtest_test_parser";
const TEST_CHILD_BINARY_PATH: &str = "./signal_test_child";
const TEST_OUTPUT_SUMMARY_PATH_ENV_KEY: &str = "TEST_OUTPUT_SUMMARY_PATH";
const READY_MARKER: &str = "READY_FOR_SIGNAL";
const SERIALLISTENER_TIMEOUT_LOG: &str = "seriallistener FATAL: timed out before success string";

struct RunningParser {
    guard: ChildGuard,
    stdout: BufReader<ChildStdout>,
    stderr_thread: Option<thread::JoinHandle<String>>,
}

impl RunningParser {
    fn spawn(summary_path: &Path, child_mode: &str) -> Self {
        let mut child = Command::new(PARSER_BINARY_PATH)
            .env(TEST_OUTPUT_SUMMARY_PATH_ENV_KEY, summary_path)
            .args([TEST_CHILD_BINARY_PATH, child_mode])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn gtest_test_parser");

        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut stderr = child.stderr.take().expect("stderr piped");
        let stderr_thread = thread::spawn(move || {
            let mut captured = String::new();
            stderr.read_to_string(&mut captured).expect("failed to read stderr");
            captured
        });

        Self { guard: ChildGuard::new(child), stdout, stderr_thread: Some(stderr_thread) }
    }

    fn pid(&mut self) -> Pid {
        let raw_pid = self
            .guard
            .as_mut()
            .expect("child still running")
            .id()
            .try_into()
            .expect("pid fits in i32");
        Pid::from_raw(raw_pid)
    }

    fn read_until_ready(&mut self) -> String {
        let mut captured = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = self.stdout.read_line(&mut line).expect("failed to read stdout");
            if bytes_read == 0 {
                let stderr = self
                    .stderr_thread
                    .take()
                    .and_then(|handle| handle.join().ok())
                    .unwrap_or_default();
                panic!(
                    "parser exited before printing {READY_MARKER};\nstdout:\n{captured}\nstderr:\n{stderr}"
                );
            }
            captured.push_str(&line);
            if line.contains(READY_MARKER) {
                return captured;
            }
        }
    }

    fn finish(mut self, mut stdout_so_far: String) -> (ExitStatus, String, String) {
        self.stdout.read_to_string(&mut stdout_so_far).expect("failed to drain remaining stdout");
        let status = self.guard.wait().expect("failed to wait for parser");
        let stderr = self
            .stderr_thread
            .take()
            .expect("stderr thread present")
            .join()
            .expect("stderr thread panicked");
        (status, stdout_so_far, stderr)
    }
}

fn read_summary(summary_path: &Path) -> TestResult {
    let contents = fs::read_to_string(summary_path).unwrap_or_else(|err| {
        panic!("expected summary file at {} to exist: {err}", summary_path.display())
    });
    serde_json::from_str(&contents).expect("summary JSON should parse as TestResult")
}

fn summary_path_in(temp_dir: &TempDir) -> PathBuf {
    temp_dir.path().join("summary.json")
}

fn spawn_and_send_signal(
    summary_path: &Path,
    child_mode: &str,
    signal: Signal,
) -> (ExitStatus, String, String, Duration) {
    let mut parser = RunningParser::spawn(summary_path, child_mode);
    let stdout_before_signal = parser.read_until_ready();
    let signal_sent_at = Instant::now();
    kill(parser.pid(), signal).expect("failed to send signal to gtest_test_parser");
    let (status, stdout, stderr) = parser.finish(stdout_before_signal);
    (status, stdout, stderr, signal_sent_at.elapsed())
}

#[test]
fn test_sigterm_forwarded_to_child_and_writes_partial_summary() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    let (status, stdout, stderr, _elapsed) =
        spawn_and_send_signal(&summary_path, "trap-sigterm", Signal::SIGTERM);

    assert_eq!(status.code(), Some(1), "expected child exit code 1; stderr: {stderr}");
    assert!(
        stderr.contains(SERIALLISTENER_TIMEOUT_LOG),
        "expected seriallistener timeout log in stderr, got: {stderr:?}"
    );
    assert!(
        stdout.contains("post-sigterm detail"),
        "expected stdout emitted during child SIGTERM handler to be mirrored, got: {stdout:?}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 2, "unexpected parsed cases: {summary:?}");
    assert_eq!(summary.cases[0].display_name, "BootSuite.PassCase");
    assert_eq!(summary.cases[0].status, TestStatus::Pass);
    assert_eq!(summary.cases[0].duration, Duration::from_millis(4));

    assert_eq!(summary.cases[1].display_name, "BootSuite.HangingCase");
    assert_eq!(summary.cases[1].status, TestStatus::Fail);
    let failure_message = summary.cases[1]
        .failure_reason
        .as_ref()
        .and_then(|reason| reason.errors.as_ref())
        .and_then(|errors| errors.first())
        .and_then(|err| err.message.as_deref())
        .expect("expected failure reason for unfinished case");
    assert!(
        failure_message.contains(READY_MARKER) && failure_message.contains("post-sigterm detail"),
        "expected both pre- and post-signal logs in failure reason, got: {failure_message:?}"
    );
}

#[test]
fn test_sigint_forwarded_to_child_and_writes_partial_summary() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    let (status, _stdout, stderr, _elapsed) =
        spawn_and_send_signal(&summary_path, "trap-sigint", Signal::SIGINT);

    assert_eq!(status.code(), Some(2), "expected child exit code 2; stderr: {stderr}");
    assert!(
        stderr.contains("child caught SIGINT"),
        "expected SIGINT handler output in stderr, got: {stderr:?}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 2, "unexpected parsed cases: {summary:?}");
    assert_eq!(summary.cases[0].display_name, "IntSuite.Done");
    assert_eq!(summary.cases[0].status, TestStatus::Pass);
    assert_eq!(summary.cases[1].display_name, "IntSuite.Interrupted");
    assert_eq!(summary.cases[1].status, TestStatus::Fail);
}

#[test]
fn test_sigterm_terminates_untrapped_child_and_writes_partial_summary() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    // Child installs no handler; verifies that `exec` resets SIGTERM to SIG_DFL
    // in the child and that gtest_test_parser forwards SIGTERM and still writes
    // partial test results without waiting for the SIGKILL grace period.
    let (status, _stdout, stderr, elapsed) =
        spawn_and_send_signal(&summary_path, "default-sigterm", Signal::SIGTERM);

    assert_eq!(status.code(), Some(1), "expected exit code 1 when child dies by signal; {stderr}");
    assert!(
        elapsed < DEFAULT_SIGNAL_GRACE_PERIOD,
        "expected untrapped child to exit before grace period ({DEFAULT_SIGNAL_GRACE_PERIOD:?}), took {elapsed:?}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 2, "unexpected parsed cases: {summary:?}");
    assert_eq!(summary.cases[0].display_name, "DefaultSuite.Pass");
    assert_eq!(summary.cases[0].status, TestStatus::Pass);
    assert_eq!(summary.cases[1].display_name, "DefaultSuite.Running");
    assert_eq!(summary.cases[1].status, TestStatus::Fail);
}

#[test]
fn test_sigterm_escalates_to_sigkill_for_unresponsive_child() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    // Child absorbs SIGTERM without exiting; gtest_test_parser must wait for
    // DEFAULT_SIGNAL_GRACE_PERIOD before escalating to SIGKILL and still flush
    // partial test results.
    let (status, _stdout, stderr, elapsed) =
        spawn_and_send_signal(&summary_path, "ignore-sigterm", Signal::SIGTERM);

    assert_eq!(status.code(), Some(1), "expected exit code 1 after SIGKILL escalation; {stderr}");
    assert!(
        elapsed >= DEFAULT_SIGNAL_GRACE_PERIOD,
        "expected SIGKILL escalation after at least {DEFAULT_SIGNAL_GRACE_PERIOD:?}, took {elapsed:?}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 2, "unexpected parsed cases: {summary:?}");
    assert_eq!(summary.cases[0].display_name, "StubbornSuite.Pass");
    assert_eq!(summary.cases[0].status, TestStatus::Pass);
    assert_eq!(summary.cases[1].display_name, "StubbornSuite.Hung");
    assert_eq!(summary.cases[1].status, TestStatus::Fail);
}

#[test]
fn test_sigterm_with_zero_exit_child_returns_nonzero() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    let (status, _stdout, stderr, _elapsed) =
        spawn_and_send_signal(&summary_path, "trap-sigterm-exit-zero", Signal::SIGTERM);

    assert_eq!(
        status.code(),
        Some(1),
        "parser must not report exit 0 when interrupted by SIGTERM; stderr: {stderr}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 1);
    assert_eq!(summary.cases[0].display_name, "ZeroExitSuite.Case");
    assert_eq!(summary.cases[0].status, TestStatus::Fail);
}

#[test]
fn test_sigterm_forwarded_while_waiting_after_stdout_closed() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let summary_path = summary_path_in(&temp_dir);

    // Child closes stdout before signaling readiness on stderr so
    // gtest_test_parser finishes draining stdout and enters
    // SupervisedChild::wait() before SIGTERM arrives.
    let mut child = Command::new(PARSER_BINARY_PATH)
        .env(TEST_OUTPUT_SUMMARY_PATH_ENV_KEY, &summary_path)
        .args([TEST_CHILD_BINARY_PATH, "closed-stdout-trap-sigterm"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn gtest_test_parser");

    let mut stdout = child.stdout.take().expect("stdout piped");
    let stdout_thread = thread::spawn(move || {
        let mut captured = String::new();
        stdout.read_to_string(&mut captured).expect("failed to read stdout");
        captured
    });
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr piped"));
    let mut guard = ChildGuard::new(child);

    let mut captured_stderr = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = stderr.read_line(&mut line).expect("failed to read stderr");
        assert!(
            bytes_read > 0,
            "parser exited before printing {READY_MARKER} on stderr; got:\n{captured_stderr}"
        );
        captured_stderr.push_str(&line);
        if line.contains(READY_MARKER) {
            break;
        }
    }

    let raw_pid: i32 =
        guard.as_mut().expect("child running").id().try_into().expect("pid fits in i32");
    kill(Pid::from_raw(raw_pid), Signal::SIGTERM).expect("failed to send SIGTERM");

    stderr.read_to_string(&mut captured_stderr).expect("failed to drain stderr");
    let status = guard.wait().expect("failed to wait for parser");
    let _stdout = stdout_thread.join().expect("stdout reader thread panicked");

    assert_eq!(status.code(), Some(1), "expected child exit code 1; stderr: {captured_stderr}");
    assert!(
        captured_stderr.contains(SERIALLISTENER_TIMEOUT_LOG),
        "expected SIGTERM handler output on stderr, got: {captured_stderr:?}"
    );

    let summary = read_summary(&summary_path);
    assert_eq!(summary.cases.len(), 2, "unexpected parsed cases: {summary:?}");
    assert_eq!(summary.cases[0].display_name, "ClosedStdoutSuite.Pass");
    assert_eq!(summary.cases[0].status, TestStatus::Pass);
    assert_eq!(summary.cases[1].display_name, "ClosedStdoutSuite.Waiting");
    assert_eq!(summary.cases[1].status, TestStatus::Fail);
}

#[test]
fn test_normal_execution_preserves_exit_code_and_writes_summary() {
    let temp_dir = TempDir::new().expect("create temp dir");

    // Passing child exits 0.
    let pass_summary_path = temp_dir.path().join("pass.json");
    let pass_child: Child = Command::new(PARSER_BINARY_PATH)
        .env(TEST_OUTPUT_SUMMARY_PATH_ENV_KEY, &pass_summary_path)
        .args([TEST_CHILD_BINARY_PATH, "normal-pass"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn passing gtest_test_parser");
    let pass_output = pass_child.wait_with_output().expect("wait for passing parser");
    assert!(pass_output.status.success());
    let pass_summary = read_summary(&pass_summary_path);
    assert_eq!(pass_summary.cases.len(), 1);
    assert_eq!(pass_summary.cases[0].status, TestStatus::Pass);

    // Failing child preserves specific non-zero exit code (e.g. 40).
    let fail_summary_path = temp_dir.path().join("fail.json");
    let fail_child: Child = Command::new(PARSER_BINARY_PATH)
        .env(TEST_OUTPUT_SUMMARY_PATH_ENV_KEY, &fail_summary_path)
        .args([TEST_CHILD_BINARY_PATH, "normal-fail"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn failing gtest_test_parser");
    let fail_output = fail_child.wait_with_output().expect("wait for failing parser");
    assert_eq!(fail_output.status.code(), Some(40));
    let fail_summary = read_summary(&fail_summary_path);
    assert_eq!(fail_summary.cases.len(), 1);
    assert_eq!(fail_summary.cases[0].status, TestStatus::Fail);
}
