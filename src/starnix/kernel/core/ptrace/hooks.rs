// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::Ordering;

use starnix_logging::CATEGORY_STARNIX;
use starnix_uapi::signals::SIGTRAP;

use crate::ptrace::{PtraceOptions, StopState};
use crate::signals::{SignalDetail, SignalInfo};
use crate::task::CurrentTask;

/// Provides access to ptrace hooks for a [`CurrentTask`].
#[derive(Debug)]
pub struct CurrentTaskPtrace<'a> {
    current_task: &'a mut CurrentTask,
}

impl CurrentTask {
    /// Returns a [`CurrentTaskPtrace`] wrapper to invoke ptrace hooks.
    pub fn ptrace(&mut self) -> CurrentTaskPtrace<'_> {
        CurrentTaskPtrace { current_task: self }
    }
}

impl<'a> CurrentTaskPtrace<'a> {
    /// Invoked upon entering a syscall to handle ptrace syscall entry tracing.
    pub fn on_syscall_enter(self) {
        if self.current_task.trace_syscalls.load(Ordering::Relaxed) {
            fuchsia_trace::duration!(CATEGORY_STARNIX, "ptrace:syscall_enter");
            self.syscall_stop(StopState::SyscallEnterStopping, None);
        }
    }

    /// Invoked upon exiting a syscall to handle ptrace syscall exit tracing.
    pub fn on_syscall_exit(self, is_error: bool) {
        if self.current_task.trace_syscalls.load(Ordering::Relaxed) {
            fuchsia_trace::duration!(CATEGORY_STARNIX, "ptrace:syscall_exit");
            self.syscall_stop(StopState::SyscallExitStopping, Some(is_error));
        }
    }

    #[inline(never)]
    fn syscall_stop(self, stop_state: StopState, is_error: Option<bool>) {
        let block = {
            let mut state = self.current_task.write();
            self.current_task.trace_syscalls.store(false, Ordering::Relaxed);
            if let Some(ptrace) = &mut state.ptrace {
                let mut sig = SignalInfo::with_detail(
                    SIGTRAP,
                    (linux_uapi::SIGTRAP | 0x80) as i32,
                    SignalDetail::None,
                );
                if ptrace.has_option(PtraceOptions::TRACESYSGOOD) {
                    sig.signal.set_ptrace_syscall_bit();
                }
                if let Some(is_error) = is_error {
                    ptrace.last_syscall_was_error = is_error;
                }
                state.set_stopped(stop_state, Some(sig), None, None);
                true
            } else {
                false
            }
        };
        if block {
            self.current_task.block_if_stopped();
        }
    }
}
