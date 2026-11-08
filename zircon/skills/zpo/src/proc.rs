// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Running host commands with a timeout.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// What a finished command printed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    /// The exit code; None when killed by a signal or the timeout.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    /// True when the command exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout and stderr together.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Runs commands; tests substitute a fake.
pub trait Runner: Send + Sync {
    /// Runs `args[0]` with the rest as arguments.
    fn run(&self, args: &[String], timeout: Duration) -> io::Result<Output>;
}

/// The real [`Runner`].
#[derive(Clone, Copy, Debug, Default)]
pub struct HostRunner;

impl Runner for HostRunner {
    fn run(&self, args: &[String], timeout: Duration) -> io::Result<Output> {
        let (prog, rest) = args.split_first().ok_or_else(|| io::Error::other("empty command"))?;
        let mut cmd = Command::new(prog);
        cmd.args(rest);
        run_command(cmd, timeout)
    }
}

/// Runs `cmd` with piped output, killing it after `timeout`.
///
/// The command runs in its own process group, so a timeout kills everything it
/// started. Otherwise a grandchild that inherited the pipes (for example a
/// daemon started by `ffx`) would keep them open and the reads would block
/// until it exits.
pub fn run_command(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn()?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let (status, timed_out) = wait_timeout(&mut child, timeout)?;
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out,
    })
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Waits for `child`, killing it after `timeout`. Returns (status, timed_out).
pub fn wait_timeout(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<(Option<ExitStatus>, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok((Some(s), false));
        }
        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.kill();
            let s = child.wait().ok();
            return Ok((s, true));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Kills the process group `pgid` (a child started with `process_group(0)` has
/// its own pid as group id). Uses the shell's `kill` to avoid unsafe code.
pub fn kill_group(pgid: u32) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg("kill -s KILL -- -$1 2>/dev/null || kill -s KILL -$1 2>/dev/null || true")
        .arg("sh")
        .arg(pgid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Quotes an argument for display so that a POSIX shell reads it back as one
/// word: plain words are left alone, anything else is single-quoted.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// A command line for logs: `$ a b 'c d'`.
pub fn display(args: &[String]) -> String {
    format!("$ {}", args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_times_out() {
        let out = HostRunner
            .run(
                &["sh".into(), "-c".into(), "echo hi; echo err >&2; exit 3".into()],
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "err\n");
        let out = HostRunner
            .run(&["sh".into(), "-c".into(), "sleep 5".into()], Duration::from_millis(100))
            .unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote(""), "''");
    }
}
