// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::{LedgerMode, LedgerNodeGuard, LedgerOutcome};
use ffx_config::EnvironmentContext;
use ffx_ssh::ssh::{
    ControlMasterMode, SSH_AUTH_SOCK, SSH_CONTROLMASTER_DIR, SSH_CONTROLMASTER_PATH,
    is_identities_only,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getuid};
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const SSH_AGENT_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const SSH_CONTROLMASTER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

async fn run_command_with_timeout(
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    envs: &[(&str, &str)],
    timeout_duration: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn()?;
    match timeout(timeout_duration, child.wait()).await {
        Ok(wait_result) => wait_result.map(Some),
        Err(_) => {
            let _ = child.start_kill();
            Ok(None)
        }
    }
}

/// Checks whether the given metadata (obtained via `std::fs::symlink_metadata`) belongs to a
/// Unix domain socket. Because `symlink_metadata` does not follow symlinks, symlinks (even
/// those pointing to valid sockets) report a symlink file type rather than a socket file type
/// and are deliberately ignored to avoid traversing untrusted symlinks.
fn is_controlmaster_socket(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_socket()
}

fn find_active_controlmaster_sockets(ctx: &EnvironmentContext) -> Vec<PathBuf> {
    let Ok(controlmaster_mode) = ControlMasterMode::from_env(ctx) else {
        return Vec::new();
    };

    match controlmaster_mode {
        ControlMasterMode::None => Vec::new(),
        ControlMasterMode::Explicit => {
            let Ok(Some(path)) = ctx.get::<Option<PathBuf>, _>(SSH_CONTROLMASTER_PATH) else {
                return Vec::new();
            };
            if !path.as_os_str().is_empty()
                && std::fs::symlink_metadata(&path).is_ok_and(|meta| is_controlmaster_socket(&meta))
            {
                vec![path]
            } else {
                Vec::new()
            }
        }
        ControlMasterMode::Managed => {
            let Ok(Some(dir)) = ctx.get::<Option<PathBuf>, _>(SSH_CONTROLMASTER_DIR) else {
                return Vec::new();
            };
            if dir.as_os_str().is_empty() {
                return Vec::new();
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return Vec::new();
            };
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    std::fs::symlink_metadata(path).is_ok_and(|meta| is_controlmaster_socket(&meta))
                })
                .collect()
        }
    }
}

fn has_parent_with_comm(pid: i32, expected_parent_comm: &str) -> bool {
    let status_path = format!("/proc/{pid}/status");
    let Ok(status_content) = std::fs::read_to_string(status_path) else {
        return false;
    };
    let Some(ppid_line) = status_content.lines().find(|line| line.starts_with("PPid:")) else {
        return false;
    };
    let Some(ppid_str) = ppid_line.split_whitespace().nth(1) else {
        return false;
    };
    let Ok(ppid) = ppid_str.parse::<i32>() else {
        return false;
    };
    let parent_comm_path = format!("/proc/{ppid}/comm");
    let Ok(parent_comm) = std::fs::read_to_string(parent_comm_path) else {
        return false;
    };
    parent_comm.trim() == expected_parent_comm
}

/// Sends `SIGKILL` to all processes owned by the current user whose `/proc/<pid>/comm`
/// matches `process_name` (and whose parent matches `expected_parent_comm`, if provided).
///
/// Note: This heuristic matches any qualifying process belonging to the current user rather
/// than tracing the exact PID bound to `SSH_AUTH_SOCK`. If the user is running multiple
/// independent `ssh-agent` sessions concurrently, this may also terminate `ssh-sk-helper`
/// processes in those other sessions.
fn kill_wedged_processes_by_name(
    process_name: &str,
    expected_parent_comm: Option<&str>,
) -> Vec<i32> {
    let mut killed_pids = Vec::new();
    let uid = getuid().as_raw();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return killed_pids;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else {
            continue;
        };
        let Ok(pid) = fname_str.parse::<i32>() else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.uid() != uid {
            continue;
        }
        let comm_path = path.join("comm");
        let Ok(comm) = std::fs::read_to_string(comm_path) else {
            continue;
        };
        // Note: `/proc/<pid>/comm` is truncated to 15 characters by the Linux kernel
        // (`TASK_COMM_LEN - 1`). Both `ssh-agent` and `ssh-sk-helper` fit within this
        // 15-character limit.
        if comm.trim() != process_name {
            continue;
        }
        if let Some(expected_parent) = expected_parent_comm {
            if !has_parent_with_comm(pid, expected_parent) {
                continue;
            }
        }
        // Note: There is a small TOCTOU window between inspecting `/proc/<pid>` metadata
        // and sending `SIGKILL` where the PID could theoretically be recycled if the
        // target process exited right after inspection.
        log::warn!("Sending SIGKILL to wedged {process_name} process (PID {pid})");
        if kill(Pid::from_raw(pid), Signal::SIGKILL).is_ok() {
            killed_pids.push(pid);
        }
    }
    killed_pids
}

pub async fn check_ssh_agent<W: Write>(
    ctx: &EnvironmentContext,
    ledger: &mut LedgerNodeGuard<'_, W>,
) {
    check_ssh_agent_with_cmd(
        ctx,
        ledger,
        "ssh-add",
        "ssh",
        "ssh-sk-helper",
        Some("ssh-agent"),
        SSH_AGENT_CHECK_TIMEOUT,
    )
    .await;
}

pub async fn check_ssh_agent_with_cmd<W: Write>(
    ctx: &EnvironmentContext,
    ledger: &mut LedgerNodeGuard<'_, W>,
    ssh_add_cmd: &str,
    ssh_cmd: &str,
    helper_process_name: &str,
    expected_parent_comm: Option<&str>,
    timeout_duration: Duration,
) {
    let auth_sock: Option<String> =
        ctx.get::<String, _>(SSH_AUTH_SOCK).ok().filter(|s| !s.is_empty());
    let Some(auth_sock) = auth_sock else {
        ledger.add_node_with_outcome(
            "SSH agent is not configured (ssh.auth-sock is not set)",
            LedgerMode::Verbose,
            LedgerOutcome::Info,
        );
        return;
    };

    let check_result = run_command_with_timeout(
        ssh_add_cmd,
        &["-l"],
        &[("SSH_AUTH_SOCK", &auth_sock)],
        timeout_duration,
    )
    .await;

    match check_result {
        Ok(Some(status)) if matches!(status.code(), Some(0) | Some(1)) => {
            ledger.add_node_with_outcome(
                &format!("SSH agent at {auth_sock} is responsive"),
                LedgerMode::Verbose,
                LedgerOutcome::Success,
            );
            return;
        }
        Ok(Some(status)) => {
            ledger.add_node_with_outcome(
                &format!("SSH agent at {auth_sock} is not responding ({status})"),
                LedgerMode::Normal,
                LedgerOutcome::Warning,
            );
            return;
        }
        Err(err) => {
            ledger.add_node_with_outcome(
                &format!("SSH agent check failed for {auth_sock}: {err}"),
                LedgerMode::Normal,
                LedgerOutcome::Warning,
            );
            return;
        }
        Ok(None) => {
            ledger.add_node_with_outcome(
                &format!("SSH agent at {auth_sock} timed out (wedged)"),
                LedgerMode::Normal,
                LedgerOutcome::Warning,
            );
        }
    }

    let killed_pids = kill_wedged_processes_by_name(helper_process_name, expected_parent_comm);
    for pid in killed_pids {
        ledger.add_node_with_outcome(
            &format!(
                "Killed wedged {helper_process_name} process (PID {pid}) to unblock ssh-agent"
            ),
            LedgerMode::Normal,
            LedgerOutcome::Success,
        );
    }

    let sockets = find_active_controlmaster_sockets(ctx);
    for socket_path in sockets {
        let _ = run_command_with_timeout(
            ssh_cmd,
            [
                OsStr::new("-O"),
                OsStr::new("exit"),
                OsStr::new("-S"),
                socket_path.as_os_str(),
                OsStr::new("localhost"),
            ],
            &[],
            SSH_CONTROLMASTER_EXIT_TIMEOUT,
        )
        .await;

        let remove_result = std::fs::remove_file(&socket_path).or_else(|err| {
            if err.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(err) }
        });

        match remove_result {
            Ok(()) => {
                ledger.add_node_with_outcome(
                    &format!(
                        "Stopped ControlMaster socket {} so it will be restarted on next connection",
                        socket_path.display()
                    ),
                    LedgerMode::Normal,
                    LedgerOutcome::Success,
                );
            }
            Err(err) => {
                ledger.add_node_with_outcome(
                    &format!(
                        "Failed to remove ControlMaster socket {}: {err}",
                        socket_path.display()
                    ),
                    LedgerMode::Normal,
                    LedgerOutcome::Failure,
                );
            }
        }
    }

    let identities_only = is_identities_only(ctx).ok().flatten().unwrap_or(false);
    if !identities_only {
        ledger.add_node_with_outcome(
            "SSH connections can hang when querying your SSH agent for hardware security keys (e.g. security tokens/YubiKeys). To configure SSH to only use Fuchsia device keys and prevent querying the agent for other keys, run: `ffx config set ssh.identities_only true`",
            LedgerMode::Normal,
            LedgerOutcome::Warning,
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::doctor_ledger::{DoctorLedger, LedgerViewMode};
    use crate::ledger_view::VisualLedgerView;
    use ffx_doctor_test_utils::MockWriter;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[fuchsia::test]
    async fn test_check_ssh_agent_responsive() {
        let temp = tempdir().unwrap();
        let mock_ssh_add = temp.path().join("mock-ssh-add");
        {
            let mut script = fs::File::create(&mock_ssh_add).unwrap();
            write!(script, "#!/bin/sh\nexit 1\n").unwrap();
        }
        let mut perms = fs::metadata(&mock_ssh_add).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_ssh_add, perms).unwrap();

        let cm_dir = temp.path().join("cm_dir");
        fs::create_dir_all(&cm_dir).unwrap();
        let socket_path = cm_dir.join("12345678");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let fake_auth_sock = temp.path().join("auth.sock");
        fs::write(&fake_auth_sock, "").unwrap();

        let test_env = ffx_config::test_env()
            .user_config("ssh.auth-sock", json!(fake_auth_sock))
            .user_config("ssh.controlmaster.mode", json!("managed"))
            .user_config("ssh.controlmaster.dir", json!(cm_dir))
            .build()
            .unwrap();

        let mut writer = MockWriter::new();
        let mut ledger = DoctorLedger::new(
            &mut writer,
            Box::new(VisualLedgerView::new()),
            LedgerViewMode::Verbose,
        );
        check_ssh_agent_with_cmd(
            &test_env.context,
            &mut ledger.root_guard(),
            mock_ssh_add.to_str().unwrap(),
            "true",
            "nonexistent-helper",
            None,
            Duration::from_secs(2),
        )
        .await;

        let output = writer.get_data();
        assert!(output.contains("is responsive"), "Expected responsive output, got:\n{output}");
        assert!(socket_path.exists(), "ControlMaster socket should remain untouched when healthy");
    }

    #[fuchsia::test]
    async fn test_check_ssh_agent_wedged_restarts_managed_controlmaster() {
        struct ChildProcessGuard(std::process::Child);
        impl Drop for ChildProcessGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let temp = tempdir().unwrap();
        let mock_ssh_add = temp.path().join("mock-ssh-add-hang");
        {
            let mut script = fs::File::create(&mock_ssh_add).unwrap();
            write!(script, "#!/bin/sh\nsleep 10\n").unwrap();
        }
        let mut perms = fs::metadata(&mock_ssh_add).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_ssh_add, perms).unwrap();

        let helper_name = "mock-sk-hlpr";
        let mock_helper = temp.path().join(helper_name);
        {
            let mut script = fs::File::create(&mock_helper).unwrap();
            write!(script, "#!/bin/sh\nsleep 30\n").unwrap();
        }
        let mut helper_perms = fs::metadata(&mock_helper).unwrap().permissions();
        helper_perms.set_mode(0o755);
        fs::set_permissions(&mock_helper, helper_perms).unwrap();
        let wedged_helper_guard =
            ChildProcessGuard(std::process::Command::new(&mock_helper).spawn().unwrap());
        let child_pid = wedged_helper_guard.0.id();
        let child_comm_path = format!("/proc/{child_pid}/comm");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(comm) = fs::read_to_string(&child_comm_path) {
                if comm.trim() == helper_name {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Timed out waiting for helper process {child_pid} comm to become {helper_name}"
            );
            fuchsia_async::Timer::new(Duration::from_millis(10)).await;
        }

        let parent_pid = std::process::id();
        let parent_comm =
            fs::read_to_string(format!("/proc/{parent_pid}/comm")).unwrap().trim().to_string();

        let cm_dir = temp.path().join("cm_dir");
        fs::create_dir_all(&cm_dir).unwrap();
        let socket_path = cm_dir.join("87654321");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let unrelated_file = cm_dir.join("unrelated_regular_file");
        fs::write(&unrelated_file, "keep me").unwrap();
        let unrelated_subdir = cm_dir.join("unrelated_subdir");
        fs::create_dir_all(&unrelated_subdir).unwrap();

        let fake_auth_sock = temp.path().join("wedged_auth.sock");
        fs::write(&fake_auth_sock, "").unwrap();

        let test_env = ffx_config::test_env()
            .user_config("ssh.auth-sock", json!(fake_auth_sock))
            .user_config("ssh.controlmaster.mode", json!("managed"))
            .user_config("ssh.controlmaster.dir", json!(cm_dir))
            .build()
            .unwrap();

        let mut writer = MockWriter::new();
        let mut ledger = DoctorLedger::new(
            &mut writer,
            Box::new(VisualLedgerView::new()),
            LedgerViewMode::Verbose,
        );
        check_ssh_agent_with_cmd(
            &test_env.context,
            &mut ledger.root_guard(),
            mock_ssh_add.to_str().unwrap(),
            "true",
            helper_name,
            Some(&parent_comm),
            Duration::from_millis(150),
        )
        .await;

        let output = writer.get_data();
        assert!(
            output.contains("timed out (wedged)"),
            "Expected wedged timeout warning, got:\n{output}"
        );
        assert!(
            output.contains(&format!("Killed wedged {helper_name} process")),
            "Expected killed helper process message, got:\n{output}"
        );
        assert!(
            output.contains("Stopped ControlMaster socket"),
            "Expected ControlMaster restart message, got:\n{output}"
        );
        assert!(
            output.contains("ffx config set ssh.identities_only true"),
            "Expected identities_only recommendation, got:\n{output}"
        );
        assert!(!socket_path.exists(), "ControlMaster socket should be removed when wedged");
        assert!(
            unrelated_file.exists(),
            "Regular non-socket files in controlmaster dir should be ignored"
        );
        assert!(unrelated_subdir.exists(), "Subdirectories in controlmaster dir should be ignored");
    }

    #[fuchsia::test]
    async fn test_check_ssh_agent_immediate_failure_does_not_remediate() {
        let temp = tempdir().unwrap();
        let mock_ssh_add = temp.path().join("mock-ssh-add-fail");
        {
            let mut script = fs::File::create(&mock_ssh_add).unwrap();
            write!(script, "#!/bin/sh\nexit 2\n").unwrap();
        }
        let mut perms = fs::metadata(&mock_ssh_add).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_ssh_add, perms).unwrap();

        let socket_path = temp.path().join("explicit_cm.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let fake_auth_sock = temp.path().join("dead_auth.sock");
        fs::write(&fake_auth_sock, "").unwrap();

        let test_env = ffx_config::test_env()
            .user_config("ssh.auth-sock", json!(fake_auth_sock))
            .user_config("ssh.controlmaster.mode", json!("explicit"))
            .user_config("ssh.controlmaster.path", json!(socket_path))
            .build()
            .unwrap();

        let mut writer = MockWriter::new();
        let mut ledger = DoctorLedger::new(
            &mut writer,
            Box::new(VisualLedgerView::new()),
            LedgerViewMode::Verbose,
        );
        check_ssh_agent_with_cmd(
            &test_env.context,
            &mut ledger.root_guard(),
            mock_ssh_add.to_str().unwrap(),
            "true",
            "nonexistent-helper",
            None,
            Duration::from_secs(2),
        )
        .await;

        let output = writer.get_data();
        assert!(
            output.contains("is not responding"),
            "Expected unresponsive warning, got:\n{output}"
        );
        assert!(
            socket_path.exists(),
            "Immediate ssh-add exit should not aggressively delete ControlMaster sockets"
        );
    }
}
