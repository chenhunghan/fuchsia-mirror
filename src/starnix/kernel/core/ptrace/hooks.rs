// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use starnix_logging::CATEGORY_STARNIX;
use starnix_types::ownership::Releasable;
use starnix_uapi::signals::{SIGKILL, SIGTRAP};
use starnix_uapi::{CLONE_PTRACE, CLONE_UNTRACED, CLONE_VFORK, SIGCHLD, clone_args};

use crate::ptrace::{
    PtraceEvent, PtraceEventData, PtraceOptions, PtraceStatus, StopState, ptrace_attach_from_state,
};
use crate::signals::syscalls::WaitingOptions;
use crate::signals::{SignalDetail, SignalInfo, send_signal_first};
use crate::task::{
    CurrentTask, ExitStatus, PidTableGuard, ProcessSelector, Task, TaskMutableState, WaitResult,
    ZombieState,
};

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
        {
            let mut state = self.current_task.write();
            self.current_task.trace_syscalls.store(false, Ordering::Relaxed);
            let Some(ptrace) = &mut state.ptrace else {
                return;
            };

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
        }
        self.current_task.block_if_stopped();
    }

    /// If currently being ptraced with the given option, emit the appropriate
    /// event.  PTRACE_EVENTMSG will return the given message.  Also emits the
    /// appropriate event for execve in the absence of TRACEEXEC.
    ///
    /// Note that the Linux kernel has a documented bug where, if TRACEEXIT is
    /// enabled, SIGKILL will trigger an event.  We do not exhibit this
    /// behavior.
    pub fn on_event(self, trace_kind: PtraceOptions, msg: u64) {
        if trace_kind.is_empty() {
            return;
        }
        {
            let mut state = self.current_task.write();
            let Some(ptrace) = &mut state.ptrace else {
                return;
            };

            if !ptrace.has_option(trace_kind) {
                // If this would be a TRACEEXEC, but TRACEEXEC is not
                // turned on, then send a SIGTRAP.
                if trace_kind == PtraceOptions::TRACEEXEC && !ptrace.is_seized() {
                    // Send a SIGTRAP so that the parent can gain control.
                    send_signal_first(self.current_task, state, SignalInfo::kernel(SIGTRAP));
                }

                return;
            }
            let event_data = PtraceEventData::new(trace_kind, msg);
            let siginfo = SignalInfo::with_detail(
                SIGTRAP,
                (((event_data.event as u32) << 8) | SIGTRAP.number()) as i32,
                SignalDetail::None,
            );
            state.set_stopped(
                StopState::PtraceEventStopping,
                Some(siginfo),
                None,
                Some(event_data),
            );
        }
        self.current_task.block_if_stopped();
    }

    /// For traced tasks, attaches `child_task` to the same tracer if required by the clone
    /// flags/ptrace options, and returns the `PtraceOptions` event to emit after spawning.
    pub fn on_clone(self, clone_args: &clone_args, child_task: &Arc<Task>) -> PtraceOptions {
        let (trace_type, ptrace_state) = {
            let state = self.current_task.read();
            let Some(ptrace) = &state.ptrace else {
                return PtraceOptions::empty();
            };

            // ptrace(2): If the tracee calls clone(2) with the CLONE_VFORK flag,
            // PTRACE_EVENT_VFORK will be delivered instead if PTRACE_O_TRACEVFORK
            // is set, otherwise if the tracee calls clone(2) with the exit signal
            // set to SIGCHLD, PTRACE_EVENT_FORK will be delivered if
            // PTRACE_O_TRACEFORK is set.
            let trace_type = if clone_args.flags & (CLONE_UNTRACED as u64) != 0 {
                PtraceOptions::empty()
            } else if clone_args.flags & (CLONE_VFORK as u64) != 0 {
                PtraceOptions::TRACEVFORK
            } else if clone_args.exit_signal != (SIGCHLD as u64) {
                PtraceOptions::TRACECLONE
            } else {
                PtraceOptions::TRACEFORK
            };

            let trace_type = if ptrace.has_option(trace_type) {
                trace_type
            } else if clone_args.flags & (CLONE_PTRACE as u64) != 0 {
                PtraceOptions::empty()
            } else {
                return PtraceOptions::empty();
            };

            (trace_type, ptrace.get_core_state())
        };

        if ptrace_attach_from_state(child_task, ptrace_state).is_err() {
            return PtraceOptions::empty();
        }
        trace_type
    }

    /// For each task traced by this thread group that matches the given
    /// selector, acquire its `TaskMutableState` and `ptracees` lock and execute
    /// the given function.
    pub fn get_ptracees_and(
        &self,
        selector: &ProcessSelector,
        f: &mut dyn FnMut(&Task, &TaskMutableState),
    ) {
        for task_ref in self
            .current_task
            .thread_group()
            .ptracees
            .lock()
            .iter()
            .filter(|info| selector.match_tid(&info.tid))
            .filter_map(|info| info.tid.get_task().ok())
        {
            let task_state = task_ref.write();
            if task_state.ptrace.is_some() {
                f(&task_ref, &task_state);
            }
        }
    }

    /// Returns a tracee whose state has changed, so that waitpid can report on
    /// it. If this returns a value, and the pid is being traced, the tracer
    /// thread is deemed to have seen the tracee ptrace-stop for the purposes of
    /// `PTRACE_LISTEN`.
    pub fn get_waitable_ptracee(
        &self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
        pids: &mut PidTableGuard<'_>,
    ) -> Option<WaitResult> {
        let thread_group = self.current_task.thread_group();
        // This checks to see if the target is a zombie ptracee.
        let waitable_entry =
            thread_group.write().zombie_ptracees.get_waitable_entry(selector, options);
        match waitable_entry {
            None => (),
            Some((zombie, None)) => return Some(zombie.to_wait_result()),
            Some((zombie, Some((tg, z)))) => {
                if let Some(tg) = tg.upgrade() {
                    if !Arc::ptr_eq(&tg, thread_group) {
                        tg.do_zombie_notifications(z, pids);
                    } else {
                        {
                            let mut state = tg.write();
                            state.children.remove(&z.task.pid);
                            state
                                .deferred_zombie_ptracers
                                .retain(|dzp| dzp.tracee_pid != z.task.pid);
                        }

                        z.release(pids);
                    };
                }
                return Some(zombie.to_wait_result());
            }
        }

        let mut tasks = vec![];

        // This checks to see if the target is a running ptracee.
        self.get_ptracees_and(selector, &mut |task: &Task, _| {
            tasks.push(task.weak_self.clone());
        });
        for task in tasks {
            let Some(task_ref) = task.upgrade() else {
                continue;
            };

            let process_state = &mut task_ref.thread_group().write();
            let mut task_state = task_ref.write();
            if task_state
                .ptrace
                .as_ref()
                .is_some_and(|ptrace| ptrace.is_waitable(task_ref.load_stopped(), options))
            {
                // We've identified a potential target.  Need to return either
                // the process's information (if we are in group-stop) or the
                // thread's information (if we are in a different stop).

                // The shared information:
                let uid = task_ref.real_creds().uid;
                let mut exit_status = None;
                let exit_signal = process_state.exit_signal.clone();
                let time_stats =
                    process_state.base.time_stats() + process_state.children_time_stats;
                let task_stopped = task_ref.load_stopped();

                #[derive(PartialEq)]
                enum ExitType {
                    None,
                    Cont,
                    Stop,
                    Kill,
                }
                if process_state.is_waitable() {
                    let ptrace = &mut task_state.ptrace;
                    // The information for processes, if we were in group stop.
                    let process_stopped = process_state.base.load_stopped();
                    let mut fn_type = ExitType::None;
                    if process_stopped == StopState::Awake && options.wait_for_continued {
                        fn_type = ExitType::Cont;
                    }
                    let mut event = ptrace
                        .as_ref()
                        .map_or(PtraceEvent::None, |ptrace| {
                            ptrace.event_data.as_ref().map_or(PtraceEvent::None, |data| data.event)
                        })
                        .clone();
                    // Tasks that are ptrace'd always get stop notifications.
                    if process_stopped == StopState::GroupStopped
                        && (options.wait_for_stopped || ptrace.is_some())
                    {
                        fn_type = ExitType::Stop;
                    }
                    if fn_type != ExitType::None {
                        let signal = if options.keep_waitable_state {
                            process_state.last_signal.as_ref().map(|siginfo| siginfo.signal)
                        } else {
                            process_state.last_signal.take().map(|siginfo| siginfo.signal)
                        };
                        if let Some(signal) = signal {
                            if task_ref.thread_group().load_stopped() == StopState::GroupStopped
                                && ptrace.as_ref().is_some_and(|ptrace| ptrace.is_seized())
                                && event == PtraceEvent::None
                            {
                                event = PtraceEvent::Stop;
                            }
                            if signal == SIGKILL {
                                fn_type = ExitType::Kill;
                            }
                            exit_status = match fn_type {
                                ExitType::Stop => Some((
                                    ExitStatus::Stop(signal, event),
                                    process_state.base.leader.clone(),
                                )),
                                ExitType::Cont => Some((
                                    ExitStatus::Continue(signal, event),
                                    process_state.base.leader.clone(),
                                )),
                                ExitType::Kill => Some((
                                    ExitStatus::Kill(signal),
                                    process_state.base.leader.clone(),
                                )),
                                _ => None,
                            };
                        }
                        // Clear the wait status of the ptrace, because we're
                        // using the tg status instead.
                        ptrace
                            .as_mut()
                            .map(|ptrace| ptrace.get_last_signal(options.keep_waitable_state));
                    }
                }
                if exit_status.is_none() {
                    if let Some(ptrace) = task_state.ptrace.as_mut() {
                        // The information for the task, if we were in a non-group stop.
                        let mut fn_type = ExitType::None;
                        let event = ptrace
                            .event_data
                            .as_ref()
                            .map_or(PtraceEvent::None, |event| event.event);
                        if task_stopped == StopState::Awake {
                            fn_type = ExitType::Cont;
                        }
                        if task_stopped.is_stopping_or_stopped()
                            || ptrace.stop_status == PtraceStatus::Listening
                        {
                            fn_type = ExitType::Stop;
                        }
                        if fn_type != ExitType::None {
                            if let Some(signal) =
                                ptrace.get_last_signal(options.keep_waitable_state)
                            {
                                if signal == SIGKILL {
                                    fn_type = ExitType::Kill;
                                }
                                exit_status = match fn_type {
                                    ExitType::Stop => Some((
                                        ExitStatus::Stop(signal, event),
                                        task_ref.tid.clone(),
                                    )),
                                    ExitType::Cont => Some((
                                        ExitStatus::Continue(signal, event),
                                        task_ref.tid.clone(),
                                    )),
                                    ExitType::Kill => {
                                        Some((ExitStatus::Kill(signal), task_ref.tid.clone()))
                                    }
                                    _ => None,
                                };
                            }
                        }
                    }
                }
                if let Some((exit_status, pid)) = exit_status {
                    return Some(WaitResult {
                        pid,
                        uid,
                        zombie_state: ZombieState { exit_status, time_stats },
                        exit_signal,
                    });
                }
            }
        }
        None
    }
}
