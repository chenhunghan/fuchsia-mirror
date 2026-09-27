// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ptrace::PtraceEvent;
use crate::signals::SignalInfo;
use starnix_uapi::{CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Exit(u8),
    Kill(SignalInfo),
    CoreDump(SignalInfo),
    // The second field for Stop and Continue contains the type of ptrace stop
    // event that made it stop / continue, if applicable (PTRACE_EVENT_STOP,
    // PTRACE_EVENT_FORK, etc)
    Stop(SignalInfo, PtraceEvent),
    Continue(SignalInfo, PtraceEvent),
}

impl ExitStatus {
    /// Converts the given exit status to a status code suitable for returning from wait syscalls.
    pub fn wait_status(&self) -> i32 {
        match self {
            ExitStatus::Exit(status) => (*status as i32) << 8,
            ExitStatus::Kill(siginfo) => siginfo.signal.number() as i32,
            ExitStatus::CoreDump(siginfo) => (siginfo.signal.number() as i32) | 0x80,
            ExitStatus::Continue(siginfo, trace_event) => {
                let trace_event_val = *trace_event as u32;
                if trace_event_val != 0 {
                    (siginfo.signal.number() as i32) | (trace_event_val << 16) as i32
                } else {
                    0xffff
                }
            }
            ExitStatus::Stop(siginfo, trace_event) => {
                let trace_event_val = *trace_event as u32;
                (0x7f + ((siginfo.signal.number() as i32) << 8)) | (trace_event_val << 16) as i32
            }
        }
    }

    pub fn signal_info_code(&self) -> i32 {
        match self {
            ExitStatus::Exit(_) => CLD_EXITED as i32,
            ExitStatus::Kill(_) => CLD_KILLED as i32,
            ExitStatus::CoreDump(_) => CLD_DUMPED as i32,
            ExitStatus::Stop(_, _) => CLD_STOPPED as i32,
            ExitStatus::Continue(_, _) => CLD_CONTINUED as i32,
        }
    }

    pub fn signal_info_status(&self) -> i32 {
        match self {
            ExitStatus::Exit(status) => *status as i32,
            ExitStatus::Kill(siginfo)
            | ExitStatus::CoreDump(siginfo)
            | ExitStatus::Continue(siginfo, _)
            | ExitStatus::Stop(siginfo, _) => siginfo.signal.number() as i32,
        }
    }
}

impl Default for ExitStatus {
    fn default() -> Self {
        Self::Exit(0)
    }
}
