// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fmt, io};

use fuchsia_sync::{Condvar, Mutex};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalsHandle, Signals};

/// Default time to wait for a child process to exit cleanly after forwarding
/// `SIGTERM`/`SIGINT` before escalating to `SIGKILL`.
///
/// This is long enough for wrapped tools such as `seriallistener` (100 ms poll
/// interval) to log timeout diagnostics and exit cleanly, while staying well
/// under `testrunner`'s 10 s `subprocess.cleanupGracePeriod` so
/// `gtest_test_parser` still has time to write `$TEST_OUTPUT_SUMMARY_PATH`.
pub const DEFAULT_SIGNAL_GRACE_PERIOD: Duration = Duration::from_secs(5);

const INITIAL_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A guard that wraps a running `std::process::Child` and ensures it is terminated
/// and reaped if the guard is dropped before `wait` is explicitly called.
///
/// This prevents leaking orphan subprocesses when returning early (e.g. via `?` operator)
/// or panicking during I/O streaming.
#[derive(Debug)]
pub struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    /// Creates a new `ChildGuard` wrapping the provided `Child`.
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Provides a mutable reference to the underlying `Child` if still managed.
    pub fn as_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Waits for the subprocess to finish naturally and returns its `ExitStatus`.
    ///
    /// This consumes the guard and disarms it so `kill` will not be called on drop.
    pub fn wait(mut self) -> io::Result<ExitStatus> {
        let mut child = self.child.take().expect("child is present");
        child.wait()
    }

    /// Disarms the guard and returns the underlying `Child`.
    pub fn into_inner(mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Kill the child process to prevent it from leaking in the background.
            let _ = child.kill();
            // Wait for the child to be reaped by the OS to avoid zombie processes.
            let _ = child.wait();
        }
    }
}

/// Registered `SIGTERM` and `SIGINT` handlers ready to be attached to a
/// [`SupervisedChild`].
///
/// Construct this before spawning the child process so termination signals
/// cannot kill `gtest_test_parser` before forwarding is active. Because
/// `signal-hook` installs a `sigaction` handler (not `SIG_IGN`) without masking
/// signals, POSIX resets the child's signal dispositions to `SIG_DFL` across
/// `exec`.
#[derive(Debug)]
pub struct TerminationSignals {
    signals: Signals,
}

impl TerminationSignals {
    /// Registers process-wide handlers for `SIGTERM` and `SIGINT`.
    ///
    /// Note that `signal-hook` keeps its low-level signal handler installed for
    /// the remainder of the process lifetime and does not restore `SIG_DFL`
    /// when `TerminationSignals` or [`SupervisedChild`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying OS signal handlers or wakeup pipe
    /// cannot be registered.
    pub fn register() -> io::Result<Self> {
        let signals = Signals::new([SIGTERM, SIGINT])?;
        Ok(Self { signals })
    }
}

/// Result of waiting for a [`SupervisedChild`] to exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitOutcome {
    /// The exit status reported by the child process.
    pub status: ExitStatus,
    /// Whether a termination signal (`SIGTERM` or `SIGINT`) was forwarded to
    /// the child process while it was running.
    pub signaled: bool,
}

struct ChildState {
    child: Option<ChildGuard>,
    exited: bool,
}

/// Supervises a running child process with RAII cleanup and `SIGTERM`/`SIGINT`
/// forwarding.
///
/// When `SIGTERM` or `SIGINT` is received, the signal is forwarded to the child
/// process while stdout draining continues. If the child does not exit within
/// the configured grace period, `SupervisedChild` sends `SIGKILL` to the child
/// so the caller can finish flushing partial test results.
pub struct SupervisedChild {
    state: Arc<Mutex<ChildState>>,
    condvar: Arc<Condvar>,
    signaled: Arc<AtomicBool>,
    signals_handle: SignalsHandle,
    signal_thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for SupervisedChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisedChild")
            .field("signaled", &self.signaled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SupervisedChild {
    /// Creates a new `SupervisedChild` that forwards signals from `signals` to
    /// `child` and escalates to `SIGKILL` after `grace_period`.
    ///
    /// # Errors
    ///
    /// Returns an error if spawning the signal-forwarding background thread
    /// fails. In that case, `child` is killed and reaped before returning.
    pub fn new(
        child: Child,
        signals: TerminationSignals,
        grace_period: Duration,
    ) -> io::Result<Self> {
        // Wrap `child` in `ChildGuard` immediately so it is killed and reaped
        // even if spawning the signal-forwarding thread fails.
        let guard = ChildGuard::new(child);
        let signals_handle = signals.signals.handle();
        let state = Arc::new(Mutex::new(ChildState { child: Some(guard), exited: false }));
        let condvar = Arc::new(Condvar::new());
        let signaled = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_condvar = Arc::clone(&condvar);
        let thread_signaled = Arc::clone(&signaled);

        let signal_thread =
            thread::Builder::new().name("gtest-signal-forwarder".to_string()).spawn(move || {
                forward_signals(
                    signals.signals,
                    thread_state,
                    thread_condvar,
                    thread_signaled,
                    grace_period,
                );
            })?;

        Ok(Self { state, condvar, signaled, signals_handle, signal_thread: Some(signal_thread) })
    }

    /// Waits for the child process to exit, stops the signal-forwarding thread,
    /// and returns the child's exit status along with whether a termination
    /// signal was forwarded.
    ///
    /// # Errors
    ///
    /// Returns an error if polling the child's exit status fails with an
    /// unrecoverable I/O error.
    pub fn wait(mut self) -> io::Result<WaitOutcome> {
        let mut poll_interval = INITIAL_WAIT_POLL_INTERVAL;
        loop {
            let wait_result = {
                let mut state = self.state.lock();
                match state.child.as_mut().and_then(|guard| guard.as_mut()) {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            // The child has been reaped; disarm `ChildGuard` and
                            // mark exited while holding the lock so the signal
                            // thread cannot signal a recycled PID.
                            if let Some(guard) = state.child.take() {
                                let _ = guard.into_inner();
                            }
                            state.exited = true;
                            self.condvar.notify_all();
                            Some(Ok(status))
                        }
                        Ok(None) => None,
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => None,
                        Err(err) => {
                            state.exited = true;
                            self.condvar.notify_all();
                            Some(Err(err))
                        }
                    },
                    None => Some(Err(io::Error::other("child process already waited on"))),
                }
            };

            if let Some(result) = wait_result {
                self.stop_signal_thread();
                let status = result?;
                let signaled = self.signaled.load(Ordering::SeqCst);
                return Ok(WaitOutcome { status, signaled });
            }

            thread::sleep(poll_interval);
            poll_interval = (poll_interval * 2).min(MAX_WAIT_POLL_INTERVAL);
        }
    }

    fn stop_signal_thread(&mut self) {
        self.signals_handle.close();
        if let Some(handle) = self.signal_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        let child_to_drop = {
            let mut state = self.state.lock();
            state.exited = true;
            let child = state.child.take();
            self.condvar.notify_all();
            child
        };
        // Stop the signal thread before reaping the child so it cannot race
        // with PID reuse after `ChildGuard::drop` calls `wait()`.
        self.stop_signal_thread();
        drop(child_to_drop);
    }
}

fn forward_signals(
    mut signals: Signals,
    state: Arc<Mutex<ChildState>>,
    condvar: Arc<Condvar>,
    signaled: Arc<AtomicBool>,
    grace_period: Duration,
) {
    for raw_signal in signals.forever() {
        let signal = match raw_signal {
            SIGTERM => Signal::SIGTERM,
            SIGINT => Signal::SIGINT,
            _ => continue,
        };

        let mut guard = state.lock();
        if guard.exited {
            break;
        }
        let Some(pid) = guard
            .child
            .as_mut()
            .and_then(|child_guard| child_guard.as_mut())
            .map(|child| child.id())
        else {
            break;
        };

        signaled.store(true, Ordering::SeqCst);
        if let Ok(raw_pid) = i32::try_from(pid)
            && raw_pid > 0
        {
            // Ignore ESRCH in case the child exited before signal delivery.
            let _ = kill(Pid::from_raw(raw_pid), signal);
        }

        // Wait up to `grace_period` for the child to exit cleanly. Track an
        // explicit deadline so spurious wakeups do not extend the wait.
        if let Some(deadline) = Instant::now().checked_add(grace_period) {
            while !guard.exited {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if condvar.wait_for(&mut guard, remaining).timed_out() {
                    break;
                }
            }
        } else {
            while !guard.exited {
                condvar.wait(&mut guard);
            }
        }

        if !guard.exited
            && let Some(child) = guard.child.as_mut().and_then(|child_guard| child_guard.as_mut())
        {
            let _ = child.kill();
        }
        // Once we have forwarded a termination signal and either observed child
        // exit or escalated to SIGKILL, there is nothing left to forward.
        break;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn test_child_guard_normal_wait() {
        let child = Command::new("true")
            .spawn()
            .or_else(|_| Command::new("/bin/true").spawn())
            .expect("failed to spawn true");
        let guard = ChildGuard::new(child);
        let status = guard.wait().expect("wait failed");
        assert!(status.success());
    }

    #[test]
    fn test_child_guard_drop_kills_process() {
        let child = Command::new("sleep")
            .arg("60")
            .spawn()
            .or_else(|_| Command::new("/bin/sleep").arg("60").spawn())
            .expect("failed to spawn sleep");
        let pid = child.id();
        let guard = ChildGuard::new(child);

        // Dropping guard must kill and reap the child process.
        drop(guard);

        // Verify that the child process is dead.
        #[cfg(unix)]
        {
            let res = Command::new("kill").args(["-0", &pid.to_string()]).status();
            assert!(!res.map(|s| s.success()).unwrap_or(false));
        }
    }

    #[test]
    fn test_child_guard_into_inner() {
        let child = Command::new("true")
            .spawn()
            .or_else(|_| Command::new("/bin/true").spawn())
            .expect("failed to spawn true");
        let guard = ChildGuard::new(child);
        let mut inner = guard.into_inner().expect("expected inner child");
        let status = inner.wait().expect("wait failed");
        assert!(status.success());
    }

    #[test]
    fn test_supervised_child_normal_wait() {
        let signals = TerminationSignals::register().expect("register signals");
        let child = Command::new("true")
            .spawn()
            .or_else(|_| Command::new("/bin/true").spawn())
            .expect("failed to spawn true");
        let supervised =
            SupervisedChild::new(child, signals, DEFAULT_SIGNAL_GRACE_PERIOD).expect("supervise");
        let outcome = supervised.wait().expect("wait failed");
        assert!(outcome.status.success());
        assert!(!outcome.signaled);
    }

    #[test]
    fn test_supervised_child_drop_kills_process() {
        let signals = TerminationSignals::register().expect("register signals");
        let child = Command::new("sleep")
            .arg("60")
            .spawn()
            .or_else(|_| Command::new("/bin/sleep").arg("60").spawn())
            .expect("failed to spawn sleep");
        let pid = child.id();
        let supervised =
            SupervisedChild::new(child, signals, DEFAULT_SIGNAL_GRACE_PERIOD).expect("supervise");

        // Dropping `SupervisedChild` must stop the signal thread and kill/reap the child.
        drop(supervised);

        #[cfg(unix)]
        {
            let raw_pid = i32::try_from(pid).expect("pid fits in i32");
            assert_eq!(kill(Pid::from_raw(raw_pid), None), Err(nix::errno::Errno::ESRCH));
        }
    }
}
