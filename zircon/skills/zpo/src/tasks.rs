// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Task specifications and job extraction.
//!
//! A *task* is what the user wants to make faster. Three ways to name one, all
//! normalized to a list of [`Job`]s (a window [t0, t1] on an *anchor thread*,
//! which the attribution engine explains):
//!
//! ```text
//! thread:<process>/<thread>      jobs = transactional epochs of that thread
//!                                (wake -> terminal block)
//! fidl:<library/Protocol.Method> jobs = one per request: from the client's
//!                                write to the client's wake by the server
//!                                (the reply), anchored on the client thread
//! events:<start>-><end>          jobs = one per end event, paired with the
//!                                latest preceding start event; anchored on the
//!                                end event's thread. Event forms:
//!                                  irq:<n>            interrupt number
//!                                  ipc:<Method>       FIDL write instant
//!                                  ipc-in:<Method>    FIDL read instant
//!                                  slice:<name>       any trace slice begin
//!                                  slice-end:<name>   any trace slice end
//! ```

use crate::model::{StateKind, ThreadInfo, Trace};
use anyhow::bail;
use std::collections::HashSet;

/// One window to explain.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The anchor thread.
    pub utid: i64,
    pub t0: i64,
    pub t1: i64,
    /// When the job's triggering event happened (IRQ, message write), when
    /// known; t0 otherwise.
    pub event_ts: i64,
}

impl Job {
    pub fn dur(&self) -> i64 {
        self.t1 - self.t0
    }
}

/// The kind of task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Thread,
    Fidl,
    Events,
}

/// A parsed task specification.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSpec {
    pub kind: TaskKind,
    /// The normalized spec, e.g. `thread:proc/name`.
    pub text: String,
    /// The threads the task resolved to.
    pub threads: Vec<ThreadInfo>,
    pub method: String,
    pub start: String,
    pub end: String,
}

impl TaskSpec {
    fn new(kind: TaskKind, text: String) -> Self {
        TaskSpec {
            kind,
            text,
            threads: vec![],
            method: String::new(),
            start: String::new(),
            end: String::new(),
        }
    }

    pub fn label(&self) -> &str {
        &self.text
    }
}

/// Parses a task spec; bare names are guessed to be FIDL methods or threads.
pub fn parse(spec: &str) -> anyhow::Result<TaskSpec> {
    if spec.starts_with("thread:") {
        return Ok(TaskSpec::new(TaskKind::Thread, spec.to_string()));
    }
    if let Some(m) = spec.strip_prefix("fidl:") {
        let mut t = TaskSpec::new(TaskKind::Fidl, spec.to_string());
        t.method = m.to_string();
        return Ok(t);
    }
    if let Some(body) = spec.strip_prefix("events:") {
        let Some((s, e)) = body.split_once("->") else {
            bail!("events spec must be events:<start>-><end>");
        };
        let (start, end) = (s.trim(), e.trim());
        for ev in [start, end] {
            let (kind, name) = ev.split_once(':').unwrap_or((ev, ""));
            if matches!(kind, "slice" | "slice-end" | "ipc" | "ipc-in") && name.trim().is_empty() {
                bail!("the event {ev:?} needs a name, e.g. {kind}:Present");
            }
        }
        // A job is attributed on the thread its end event happens on, and an
        // interrupt has none.
        if end.starts_with("irq:") {
            bail!("an irq: event can only start a job (an interrupt has no thread to end it on)");
        }
        let mut t = TaskSpec::new(TaskKind::Events, spec.to_string());
        t.start = start.to_string();
        t.end = end.to_string();
        return Ok(t);
    }
    // Bare names: guess.
    if !spec.contains('(')
        && spec.split_once('/').is_some_and(|(lib, method)| {
            !method.contains('/')
                && lib.contains('.')
                && lib.starts_with(char::is_lowercase)
                && method.starts_with(char::is_uppercase)
        })
    {
        let mut t = TaskSpec::new(TaskKind::Fidl, format!("fidl:{spec}"));
        t.method = spec.to_string();
        return Ok(t);
    }
    Ok(TaskSpec::new(TaskKind::Thread, format!("thread:{spec}")))
}

/// The longest wait for a reply (or terminal block) that is still part of a
/// job. A longer wait is idle time, even when the peer's message ends it.
/// Like [`MAX_EVENT_SPAN_NS`], a pairing bound only: well above a 60 Hz frame
/// (16.7ms), so it never cuts off a real response.
pub const MAX_REPLY_NS: i64 = 50_000_000;
/// The longest start-to-end span an events task pairs into one job.
pub const MAX_EVENT_SPAN_NS: i64 = 200_000_000;

/// The spec of a task given as the UI's form fields: `events:<start>-><end>`,
/// `fidl:<method>` or `thread:<process>/<thread>`; `kind` is "events",
/// "fidl" or "thread". Empty when the kind's fields are empty.
pub fn compose(kind: &str, start: &str, end: &str, method: &str, thread: &str) -> String {
    let (start, end, method, thread) = (start.trim(), end.trim(), method.trim(), thread.trim());
    match kind {
        "events" if !start.is_empty() || !end.is_empty() => format!("events:{start}->{end}"),
        "fidl" if !method.is_empty() => format!("fidl:{method}"),
        "thread" if !thread.is_empty() => format!("thread:{thread}"),
        _ => String::new(),
    }
}

/// The form fields of a spec, the inverse of [`compose`]:
/// `{"kind", "start", "end", "fidl", "thread"}`.
pub fn form_fields(spec: &str) -> serde_json::Value {
    let (kind, start, end, method, thread) = match parse(spec) {
        Ok(t) if t.kind == TaskKind::Events => {
            ("events", t.start, t.end, String::new(), String::new())
        }
        Ok(t) if t.kind == TaskKind::Fidl => {
            ("fidl", String::new(), String::new(), t.method, String::new())
        }
        Ok(t) => {
            let thread = t.text.strip_prefix("thread:").unwrap_or(&t.text).to_string();
            ("thread", String::new(), String::new(), String::new(), thread)
        }
        // events:<start> without "->": keep what was typed.
        Err(_) => {
            let start = spec.strip_prefix("events:").unwrap_or(spec).to_string();
            ("events", start, String::new(), String::new(), String::new())
        }
    };
    serde_json::json!({"kind": kind, "start": start, "end": end, "fidl": method, "thread": thread})
}

/// Splits a thread's timeline into epochs.
///
/// An epoch starts when the thread is woken and ends at its next *terminal*
/// block. A block is *internal* (does not end the epoch) when the wake that
/// ends it comes from a thread this thread sent a FIDL message to during the
/// current run turn (it was waiting for that peer's reply), and it is shorter
/// than `min_terminal_block_ns`; every other block is terminal. A reply the
/// thread writes to a peer it just read from answers that peer, so it does
/// not make the thread wait for it, and each waking reply is used once.
pub fn thread_jobs(tr: &Trace, utid: i64, min_terminal_block_ns: i64) -> Vec<Job> {
    let states = tr.thread_states(utid);
    let mut jobs = Vec::new();
    let n = states.len();
    let mut i = 0;
    while i < n {
        let s = &states[i];
        if s.state != StateKind::Woken {
            i += 1;
            continue;
        }
        let t0 = s.ts;
        let mut sent_to: HashSet<i64> = HashSet::new();
        let mut turn = t0;
        let mut j = i;
        let mut end_ts = None;
        while j < n {
            let st = &states[j];
            if st.state == StateKind::Blocked {
                let nxt = states.get(j + 1);
                let mut read_from: HashSet<i64> = HashSet::new();
                for ev in tr.ipc_between(utid, turn, st.ts) {
                    match (ev.outgoing, ev.peer_utid) {
                        (false, Some(p)) => {
                            read_from.insert(p);
                        }
                        (true, Some(p)) if !read_from.contains(&p) => {
                            sent_to.insert(p);
                        }
                        _ => {}
                    }
                }
                let waker =
                    nxt.filter(|nx| nx.state == StateKind::Woken).and_then(|nx| nx.waker_utid);
                let internal = waker.is_some_and(|w| sent_to.contains(&w))
                    && st.dur >= 0
                    && (min_terminal_block_ns <= 0 || st.dur < min_terminal_block_ns);
                if !internal {
                    end_ts = Some(st.ts);
                    break;
                }
                if let (Some(w), Some(nx)) = (waker, nxt) {
                    sent_to.remove(&w);
                    turn = nx.ts;
                }
            }
            j += 1;
        }
        let Some(end) = end_ts else { break };
        if end > t0 {
            jobs.push(Job { utid, t0, t1: end, event_ts: t0 });
        }
        i = j + 1;
    }
    jobs
}

/// One job per request: client write -> client woken by the server (reply).
///
/// If the client never blocks on the server (one-way / async), the job is the
/// server's: from the write to the end of the server's handling run.
///
/// A request and its reply carry the same method, so a server's write back to
/// its client inside an answered call is that call's reply, not a request.
pub fn fidl_jobs(tr: &Trace, method: &str, max_reply_ns: i64) -> Vec<Job> {
    let mut jobs = Vec::new();
    // (client, server, write, reply wake) of each answered call.
    let mut calls: Vec<(i64, i64, i64, i64)> = Vec::new();
    for ev in tr.ipc_for_method(method) {
        let client = ev.utid;
        let Some(server) = ev.peer_utid else { continue };
        if client == server
            || calls
                .iter()
                .any(|&(c, s, a, b)| client == s && server == c && a < ev.ts && ev.ts <= b)
        {
            continue;
        }
        let mut end = None;
        // First wake of the client by the server after the write.
        for (_, st) in tr.states_overlapping(client, ev.ts, ev.ts + max_reply_ns) {
            if st.state == StateKind::Woken && st.ts > ev.ts && st.waker_utid == Some(server) {
                end = Some(st.ts);
                break;
            }
        }
        if let Some(e) = end {
            calls.push((client, server, ev.ts, e));
        }
        let mut anchor = client;
        if end.is_none() {
            // One-way: end when the server finishes the running segment
            // covering the read; the client does not wait, so the job is the
            // server's.
            anchor = server;
            let read_ts = match ev.peer_ts {
                Some(t) if t != 0 => t,
                _ => ev.ts,
            };
            for (_, st) in tr.states_overlapping(server, read_ts, read_ts + max_reply_ns) {
                if st.state == StateKind::Running && st.end() > read_ts {
                    end = Some(st.end());
                    break;
                }
            }
        }
        match end {
            Some(e) if e > ev.ts => {
                jobs.push(Job { utid: anchor, t0: ev.ts, t1: e, event_ts: ev.ts })
            }
            _ => {}
        }
    }
    jobs
}

/// (ts, utid or None) of every instance of an event spec.
fn event_instances(tr: &Trace, spec: &str) -> anyhow::Result<Vec<(i64, Option<i64>)>> {
    let (kind, arg) = spec.split_once(':').unwrap_or((spec, ""));
    Ok(match kind {
        "irq" => {
            let n: i64 =
                arg.trim().parse().map_err(|_| anyhow::anyhow!("bad irq number in {spec:?}"))?;
            tr.irq_instances(n).iter().map(|i| (i.ts, None)).collect()
        }
        "ipc" => tr.ipc_for_method(arg).iter().map(|e| (e.ts, Some(e.utid))).collect(),
        "ipc-in" => tr
            .ipc_for_method(arg)
            .iter()
            .filter_map(|e| e.peer_ts.map(|t| (t, e.peer_utid)))
            .collect(),
        "slice" => tr.slices_named(arg).iter().map(|&(ts, _, u)| (ts, Some(u))).collect(),
        "slice-end" => tr
            .slices_named(arg)
            .iter()
            .filter(|&&(_, d, _)| d >= 0)
            .map(|&(ts, d, u)| (ts + d, Some(u)))
            .collect(),
        _ => bail!("unknown event spec {spec:?}"),
    })
}

/// One job per end event, paired with the latest start event before it.
pub fn event_jobs(
    tr: &Trace,
    start: &str,
    end: &str,
    max_span_ns: i64,
) -> anyhow::Result<Vec<Job>> {
    let mut starts = event_instances(tr, start)?;
    let mut ends = event_instances(tr, end)?;
    starts.sort();
    ends.sort();
    let sts: Vec<i64> = starts.iter().map(|s| s.0).collect();
    let mut jobs = Vec::new();
    let mut last_used: isize = -1;
    for (e_ts, e_utid) in ends {
        let Some(e_utid) = e_utid else { continue };
        let k = sts.partition_point(|&t| t < e_ts) as isize - 1;
        if k < 0 || k == last_used {
            continue;
        }
        let s_ts = sts[k as usize];
        if e_ts - s_ts > max_span_ns {
            continue;
        }
        last_used = k;
        jobs.push(Job { utid: e_utid, t0: s_ts, t1: e_ts, event_ts: s_ts });
    }
    Ok(jobs)
}

fn threads_of(tr: &Trace, jobs: &[Job]) -> Vec<ThreadInfo> {
    let mut seen = HashSet::new();
    let mut out: Vec<ThreadInfo> = jobs
        .iter()
        .filter_map(|j| tr.threads.get(&j.utid))
        .filter(|t| seen.insert(t.utid))
        .cloned()
        .collect();
    out.sort_by_key(|t| t.label());
    out
}

/// The jobs of a task in a trace (and the threads it resolved to).
pub fn resolve(tr: &Trace, mut spec: TaskSpec) -> anyhow::Result<(TaskSpec, Vec<Job>)> {
    match spec.kind {
        TaskKind::Thread => {
            let pattern = spec.text.strip_prefix("thread:").unwrap_or(&spec.text).to_string();
            spec.threads = tr.find_threads(&pattern);
            let mut jobs = Vec::new();
            for t in &spec.threads {
                jobs.extend(thread_jobs(tr, t.utid, MAX_REPLY_NS));
            }
            jobs.sort_by_key(|j| j.t0);
            Ok((spec, jobs))
        }
        TaskKind::Fidl => {
            let jobs = fidl_jobs(tr, &spec.method, MAX_REPLY_NS);
            spec.threads = threads_of(tr, &jobs);
            Ok((spec, jobs))
        }
        TaskKind::Events => {
            let jobs = event_jobs(tr, &spec.start, &spec.end, MAX_EVENT_SPAN_NS)?;
            spec.threads = threads_of(tr, &jobs);
            Ok((spec, jobs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StateKind::{Blocked, Running, Woken};

    #[test]
    fn form_fields_round_trip() {
        for spec in ["events:irq:716->ipc-in:a.b/C.D", "fidl:a.b/C.D", "thread:proc/name"] {
            let f = form_fields(spec);
            let s = |k: &str| f[k].as_str().unwrap().to_string();
            assert_eq!(compose(&s("kind"), &s("start"), &s("end"), &s("fidl"), &s("thread")), spec);
        }
        assert_eq!(compose("fidl", "", "", " ", ""), "");
        assert_eq!(form_fields("events:irq:1")["start"], "irq:1");
    }

    fn st(ts: i64, dur: i64, state: StateKind, waker: Option<i64>) -> crate::model::State {
        crate::model::State { ts, dur, state, cpu: 0, waker_utid: waker }
    }

    fn ipc(ts: i64, outgoing: bool, peer: i64) -> crate::model::IpcEvent {
        crate::model::IpcEvent {
            ts,
            utid: 1,
            method: "M".into(),
            outgoing,
            peer_utid: Some(peer),
            peer_ts: None,
        }
    }

    #[test]
    fn a_server_reply_does_not_merge_its_idle_time_into_one_job() {
        // Server 1 serves client 2 twice: woken by the request, reads it,
        // writes the reply and blocks until the next request.
        let mut tr = Trace::new();
        tr.set_states(
            1,
            vec![
                st(0, 10, Woken, Some(2)),
                st(10, 90, Running, None),
                st(100, 900, Blocked, None),
                st(1000, 10, Woken, Some(2)),
                st(1010, 90, Running, None),
                st(1100, 900, Blocked, None),
            ],
        );
        tr.set_ipc(
            1,
            vec![ipc(20, false, 2), ipc(90, true, 2), ipc(1020, false, 2), ipc(1090, true, 2)],
        );
        tr.finalize();
        let jobs: Vec<(i64, i64)> =
            thread_jobs(&tr, 1, MAX_REPLY_NS).iter().map(|j| (j.t0, j.t1)).collect();
        assert_eq!(jobs, vec![(0, 100), (1000, 1100)]);
    }

    #[test]
    fn a_reply_ends_one_wait_only() {
        // Client 1 calls server 3 and waits for its reply; the reply is used
        // once, so the next wake from 3 starts a new job.
        let mut tr = Trace::new();
        tr.set_states(
            1,
            vec![
                st(0, 10, Woken, Some(9)),
                st(10, 40, Running, None),
                st(50, 50, Blocked, None),
                st(100, 10, Woken, Some(3)),
                st(110, 40, Running, None),
                st(150, 850, Blocked, None),
                st(1000, 10, Woken, Some(3)),
                st(1010, 40, Running, None),
                st(1050, 100, Blocked, None),
            ],
        );
        tr.set_ipc(1, vec![ipc(40, true, 3)]);
        tr.finalize();
        let jobs: Vec<(i64, i64)> =
            thread_jobs(&tr, 1, MAX_REPLY_NS).iter().map(|j| (j.t0, j.t1)).collect();
        assert_eq!(jobs, vec![(0, 150), (1000, 1050)]);
    }

    #[test]
    fn parses_specs() {
        assert_eq!(parse("thread:a/b").unwrap().kind, TaskKind::Thread);
        let f = parse("fuchsia.input.report/InputReportsReader.ReadInputReports").unwrap();
        assert_eq!(f.kind, TaskKind::Fidl);
        assert_eq!(f.text, "fidl:fuchsia.input.report/InputReportsReader.ReadInputReports");
        let e = parse("events:irq:42 -> slice:Present").unwrap();
        assert_eq!((e.start.as_str(), e.end.as_str()), ("irq:42", "slice:Present"));
        assert!(parse("events:irq:1").is_err());
        // An interrupt has no thread to end a job on, and events need names.
        assert!(parse("events:slice:Present->irq:42").is_err());
        assert!(parse("events:irq:42->slice:").is_err());
        assert!(parse("events:ipc: ->slice:Present").is_err());
        assert_eq!(
            parse("driver-host-#input/fdf*").unwrap().text,
            "thread:driver-host-#input/fdf*"
        );
    }
}
