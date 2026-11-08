// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host process management utilities for `ffx`.
//!
//! Provides functions to inspect and manage host processes in a portable, robust manner.

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

/// Returns true if the process identified by `pid` is currently running on the host.
///
/// # Behavior & Edge Cases
/// - Returns `false` for `pid == 0` or values exceeding `i32::MAX`.
/// - If `pid` identifies a child process of the current process, this function reaps
///   the child if it is defunct (zombie) via a non-blocking `waitpid` call (`WNOHANG`),
///   consuming its exit status.
/// - **Note for child processes**: Callers holding an active [`std::process::Child`] handle
///   should prefer calling [`std::process::Child::try_wait`] instead of `is_pid_running`,
///   as this function will reap the child and cause subsequent `Child::wait` or `Child::try_wait`
///   calls to fail with `ECHILD`.
/// - If `pid` is not a child process of the current process, queries process existence
///   via signal 0 (`kill(pid, 0)`) and returns `true` if the process exists (even if
///   `EPERM` is returned due to insufficient permissions to signal the process).
/// - Safe for concurrent calls from multiple threads; automatically retries if interrupted
///   by `EINTR`.
///
/// # Examples
/// ```no_run
/// use ffx_process::is_pid_running;
///
/// if is_pid_running(1234) {
///     println!("Process 1234 is alive");
/// }
/// ```
pub fn is_pid_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let target_pid = Pid::from_raw(raw_pid);

    let wait_status = loop {
        match waitpid(target_pid, Some(WaitPidFlag::WNOHANG)) {
            Err(Errno::EINTR) => continue,
            res => break res,
        }
    };

    match wait_status {
        Ok(WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _)) => false,
        Ok(
            WaitStatus::StillAlive
            | WaitStatus::Stopped(_, _)
            | WaitStatus::Continued(_)
            | WaitStatus::PtraceEvent(_, _, _)
            | WaitStatus::PtraceSyscall(_),
        ) => true,
        Err(Errno::ECHILD) => matches!(kill(target_pid, None), Ok(()) | Err(Errno::EPERM)),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct ChildGuard(Option<std::process::Child>);

    impl ChildGuard {
        fn new(child: std::process::Child) -> Self {
            Self(Some(child))
        }

        fn id(&self) -> u32 {
            self.0.as_ref().expect("child exists").id()
        }

        fn take(&mut self) -> std::process::Child {
            self.0.take().expect("child exists")
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[fuchsia::test]
    fn test_zero_pid_is_not_running() {
        assert!(!is_pid_running(0));
    }

    #[fuchsia::test]
    fn test_invalid_pid_is_not_running() {
        assert!(!is_pid_running(u32::MAX));
        assert!(!is_pid_running((i32::MAX as u32) + 1));
    }

    #[fuchsia::test]
    fn test_current_process_is_running() {
        assert!(is_pid_running(std::process::id()));
    }

    #[fuchsia::test]
    fn test_child_process_liveness() {
        let mut child = ChildGuard::new(
            Command::new("sleep").arg("60").spawn().expect("failed to spawn child"),
        );
        let pid = child.id();
        assert!(is_pid_running(pid));

        let mut inner = child.take();
        inner.kill().expect("failed to kill child");
        inner.wait().expect("failed to wait on child");
        assert!(!is_pid_running(pid));
    }

    #[fuchsia::test]
    fn test_defunct_child_collected_signaled() {
        let mut child = ChildGuard::new(
            Command::new("sleep").arg("60").spawn().expect("failed to spawn child"),
        );
        let pid = child.id();
        assert!(is_pid_running(pid));

        let mut inner = child.take();
        inner.kill().expect("failed to kill child");

        // Block until the child has terminated and become a zombie, without reaping it.
        let nix_pid = Pid::from_raw(i32::try_from(pid).unwrap());
        nix::sys::wait::waitid(
            nix::sys::wait::Id::Pid(nix_pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
        )
        .expect("waitid failed");

        // `is_pid_running` must reap the defunct child and return false on the first call (exercising WaitStatus::Signaled).
        assert!(!is_pid_running(pid));
        assert_eq!(waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)), Err(Errno::ECHILD));

        // Subsequent call exercises the Err(Errno::ECHILD) path.
        assert!(!is_pid_running(pid));
    }

    #[fuchsia::test]
    fn test_defunct_child_collected_exited() {
        let mut child =
            ChildGuard::new(Command::new("true").spawn().expect("failed to spawn true"));
        let pid = child.id();

        // Prevent ChildGuard drop from waiting since we test waitpid reaping here.
        let _ = child.take();

        let nix_pid = Pid::from_raw(i32::try_from(pid).unwrap());
        // Block until `true` has exited without reaping it.
        nix::sys::wait::waitid(
            nix::sys::wait::Id::Pid(nix_pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
        )
        .expect("waitid failed");

        // `is_pid_running` must reap the defunct child and return false (exercising WaitStatus::Exited).
        assert!(!is_pid_running(pid));
        assert_eq!(waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)), Err(Errno::ECHILD));

        // Subsequent call exercises the Err(Errno::ECHILD) path.
        assert!(!is_pid_running(pid));
    }
}
