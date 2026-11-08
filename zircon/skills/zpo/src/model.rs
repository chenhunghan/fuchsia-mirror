// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-memory model of the parts of a Fuchsia trace the analysis needs.
//!
//! The analysis is driven by a handful of threads and binary searches over
//! per-thread sorted lists, so per-thread tables (thread states, sched slices,
//! IPC events) are loaded lazily from the trace processor the first time a
//! thread is looked at. A [`Trace`] without a source (built by tests) holds
//! everything up front.

use crate::tp::{Query, Rows};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What a thread was doing, from Perfetto's Fuchsia importer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StateKind {
    /// On a CPU.
    Running,
    /// Was running, got preempted; still runnable ("R").
    Preempted,
    /// Woken, waiting for a CPU ("W").
    Woken,
    /// Blocked ("S").
    Blocked,
    /// Anything else the importer reports.
    Other(String),
}

impl StateKind {
    /// Parses `thread_state.state`.
    pub fn parse(s: &str) -> Self {
        match s {
            "Running" => StateKind::Running,
            "R" => StateKind::Preempted,
            "W" => StateKind::Woken,
            "S" => StateKind::Blocked,
            other => StateKind::Other(other.to_string()),
        }
    }

    /// The trace processor's spelling.
    pub fn as_str(&self) -> &str {
        match self {
            StateKind::Running => "Running",
            StateKind::Preempted => "R",
            StateKind::Woken => "W",
            StateKind::Blocked => "S",
            StateKind::Other(s) => s,
        }
    }
}

/// A thread of the trace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadInfo {
    pub utid: i64,
    pub tid: i64,
    pub name: String,
    pub pid: i64,
    pub process: String,
}

impl ThreadInfo {
    /// `process/thread[tid]`, the name zpo shows for a thread.
    pub fn label(&self) -> String {
        format!("{}/{}[{}]", self.process, self.name, self.tid)
    }
}

/// One thread state interval.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub ts: i64,
    /// Negative when the state lasts until the end of the trace.
    pub dur: i64,
    pub state: StateKind,
    /// -1 when unknown.
    pub cpu: i64,
    pub waker_utid: Option<i64>,
}

impl State {
    pub fn end(&self) -> i64 {
        self.ts + self.dur
    }
}

/// The scheduling parameters a thread ran with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulingParams {
    /// A fair profile with this weight.
    Fair { weight: i64 },
    /// A deadline profile.
    Deadline { capacity_ns: i64, deadline_ns: i64 },
    /// A negative value that does not unpack into deadline parameters.
    Unknown,
}

impl SchedulingParams {
    /// Decodes the trace processor's `sched.priority` column: the fair weight
    /// when non-negative, otherwise the deadline parameters packed as
    /// -CCCCDDDD (capacity and deadline in 10 us units).
    ///
    /// Zircon clamps both packed fields to 1..=9999 (10 us to 99.99 ms) when it
    /// writes the trace, so a capacity or period of 100 ms or more decodes as
    /// 99.99 ms, and anything under 10 us as 10 us.
    pub fn from_sched_priority(value: i64) -> Self {
        if value >= 0 {
            return SchedulingParams::Fair { weight: value };
        }
        let Some(packed) = value.checked_neg() else { return SchedulingParams::Unknown };
        let (capacity, deadline) = (packed / 10_000, packed % 10_000);
        if capacity <= 0 || deadline <= 0 {
            return SchedulingParams::Unknown;
        }
        SchedulingParams::Deadline {
            capacity_ns: capacity * 10_000,
            deadline_ns: deadline * 10_000,
        }
    }

    /// (capacity_ns, deadline_ns) for a deadline profile.
    pub fn deadline(&self) -> Option<(i64, i64)> {
        match *self {
            SchedulingParams::Deadline { capacity_ns, deadline_ns } => {
                Some((capacity_ns, deadline_ns))
            }
            _ => None,
        }
    }

    /// The weight of a fair profile.
    pub fn fair_weight(&self) -> Option<i64> {
        match *self {
            SchedulingParams::Fair { weight } => Some(weight),
            _ => None,
        }
    }
}

/// One stretch of a thread on a CPU, with the scheduling parameters it ran
/// with.
#[derive(Clone, Debug, PartialEq)]
pub struct SchedSlice {
    pub ts: i64,
    pub dur: i64,
    pub cpu: i64,
    pub scheduling_params: SchedulingParams,
}

impl SchedSlice {
    pub fn end(&self) -> i64 {
        self.ts + self.dur
    }

    /// (capacity_ns, deadline_ns) if this slice ran under a deadline profile.
    pub fn deadline_params(&self) -> Option<(i64, i64)> {
        self.scheduling_params.deadline()
    }
}

/// One side of a FIDL message (a kernel:ipc instant).
#[derive(Clone, Debug, PartialEq)]
pub struct IpcEvent {
    pub ts: i64,
    pub utid: i64,
    pub method: String,
    /// True on the write side (sender), false on the read side (receiver).
    pub outgoing: bool,
    pub peer_utid: Option<i64>,
    pub peer_ts: Option<i64>,
}

/// One interrupt handler run.
#[derive(Clone, Debug, PartialEq)]
pub struct Irq {
    pub ts: i64,
    pub dur: i64,
    pub cpu: i64,
    pub irq: i64,
}

impl Irq {
    pub fn end(&self) -> i64 {
        self.ts + self.dur
    }
}

/// Where a lazily loaded [`Trace`] gets its per-thread tables.
pub trait TraceSource {
    /// Runs a query against the trace.
    fn query(&self, q: &Query) -> anyhow::Result<Rows>;
}

/// A trace with per-thread indexes.
#[derive(Default)]
pub struct Trace {
    pub path: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub threads: IndexMap<i64, ThreadInfo>,
    /// cpu -> IRQs sorted by ts, in the order the CPUs first appear.
    pub irqs: IndexMap<i64, Vec<Irq>>,
    /// cpu -> sorted (ts, rate); 1.0 is the reference core at its max OPP,
    /// from kernel:power `Rate:CPU:n`.
    pub rates: IndexMap<i64, Vec<(i64, f64)>>,
    pub has_rates: bool,
    pub ncpu: i64,
    states: RefCell<HashMap<i64, Rc<Vec<State>>>>,
    sched: RefCell<HashMap<i64, Rc<Vec<SchedSlice>>>>,
    ipc: RefCell<HashMap<i64, Rc<Vec<IpcEvent>>>>,
    source: Option<Box<dyn TraceSource>>,
    lazy: bool,
    loaded_states: RefCell<HashSet<i64>>,
    loaded_ipc: RefCell<HashSet<i64>>,
    error: RefCell<Option<anyhow::Error>>,
}

impl Trace {
    /// An empty trace whose tables are filled in by the caller (tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// A trace that loads per-thread tables from `source` on demand.
    pub fn lazy(source: Box<dyn TraceSource>) -> Self {
        Trace { source: Some(source), lazy: true, ..Self::default() }
    }

    /// Sets where [`Trace::query`] goes, without loading tables from it
    /// (tests that fill the tables themselves).
    pub fn set_source(&mut self, source: Box<dyn TraceSource>) {
        self.source = Some(source);
        self.lazy = false;
    }

    /// Runs a query against the trace's source.
    pub fn query(&self, q: &Query) -> anyhow::Result<Rows> {
        match &self.source {
            Some(s) => s.query(q),
            None => anyhow::bail!("this trace has no trace processor behind it"),
        }
    }

    /// Fails with the first error a lazy load ran into, if any.
    ///
    /// Lookups cannot fail (they return what they have), so analyses call this
    /// once at the end.
    pub fn check(&self) -> anyhow::Result<()> {
        match self.error.borrow_mut().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn note_error(&self, e: anyhow::Error) {
        let mut slot = self.error.borrow_mut();
        if slot.is_none() {
            *slot = Some(e);
        }
    }

    // Construction, used by tests and by the loader to fill in tables that
    // are not loaded lazily.

    /// Sets a thread's states, sorting them by ts.
    pub fn set_states(&mut self, utid: i64, mut states: Vec<State>) {
        states.sort_by_key(|s| s.ts);
        self.states.get_mut().insert(utid, Rc::new(states));
    }

    /// Sets a thread's sched slices, sorting them by ts.
    pub fn set_sched(&mut self, utid: i64, mut sched: Vec<SchedSlice>) {
        sched.sort_by_key(|s| s.ts);
        self.sched.get_mut().insert(utid, Rc::new(sched));
    }

    /// Sets a thread's IPC events, sorting them by ts.
    pub fn set_ipc(&mut self, utid: i64, mut ipc: Vec<IpcEvent>) {
        ipc.sort_by_key(|e| e.ts);
        self.ipc.get_mut().insert(utid, Rc::new(ipc));
    }

    /// Sorts the IRQ and rate tables (filled in directly through the public
    /// fields) and derives `has_rates` and `ncpu` from them.
    pub fn finalize(&mut self) {
        for lst in self.irqs.values_mut() {
            lst.sort_by_key(|i| i.ts);
        }
        for lst in self.rates.values_mut() {
            lst.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        }
        self.has_rates = self.rates.values().any(|v| !v.is_empty());
        let mut ncpu = self.ncpu;
        for &c in self.irqs.keys().chain(self.rates.keys()) {
            ncpu = ncpu.max(c + 1);
        }
        self.ncpu = ncpu;
    }

    // Lazy loading of per-thread tables from the trace processor.

    fn ensure_states(&self, utid: i64) {
        let Some(src) = self.source.as_ref().filter(|_| self.lazy) else { return };
        if !self.loaded_states.borrow_mut().insert(utid) {
            return;
        }
        let states = src.query(&crate::tp::queries::thread_states(utid)).map(|rows| {
            rows.iter()
                .map(|r| State {
                    ts: r.get::<i64>(0),
                    dur: r.get::<i64>(1),
                    state: StateKind::parse(&r.get::<String>(2)),
                    cpu: r.get::<i64>(3),
                    waker_utid: r.get::<Option<i64>>(4),
                })
                .collect::<Vec<_>>()
        });
        let sched = src.query(&crate::tp::queries::thread_sched(utid)).map(|rows| {
            rows.iter()
                .map(|r| SchedSlice {
                    ts: r.get::<i64>(0),
                    dur: r.get::<i64>(1),
                    cpu: r.get::<i64>(2),
                    scheduling_params: SchedulingParams::from_sched_priority(r.get::<i64>(3)),
                })
                .collect::<Vec<_>>()
        });
        match (states, sched) {
            (Ok(st), Ok(sc)) => {
                self.states.borrow_mut().insert(utid, Rc::new(st));
                self.sched.borrow_mut().insert(utid, Rc::new(sc));
            }
            (Err(e), _) | (_, Err(e)) => self.note_error(e),
        }
    }

    fn ensure_ipc(&self, utid: i64) {
        let Some(src) = self.source.as_ref().filter(|_| self.lazy) else { return };
        if !self.loaded_ipc.borrow_mut().insert(utid) {
            return;
        }
        match src.query(&crate::tp::queries::ipc_for_thread(utid)) {
            Ok(rows) => {
                let mut evs: Vec<IpcEvent> =
                    rows.iter().map(ipc_from_row).filter(|e| e.utid == utid).collect();
                evs.sort_by_key(|e| e.ts);
                self.ipc.borrow_mut().insert(utid, Rc::new(evs));
            }
            Err(e) => self.note_error(e),
        }
    }

    // Lookups. These never fail; errors from lazy loads surface through
    // `Trace::check`.

    /// The thread's label, `<none>` for no thread, `utid:N` for an unknown one.
    pub fn thread_label(&self, utid: Option<i64>) -> String {
        match utid {
            None => "<none>".to_string(),
            Some(u) => match self.threads.get(&u) {
                Some(t) => t.label(),
                None => format!("utid:{u}"),
            },
        }
    }

    /// [`Trace::thread_label`] of a known utid.
    pub fn label(&self, utid: i64) -> String {
        self.thread_label(Some(utid))
    }

    /// Threads matching `process/thread` shell-style globs (either part
    /// optional), sorted by process, thread name and utid.
    pub fn find_threads(&self, pattern: &str) -> Vec<ThreadInfo> {
        let (pp, tp) = match pattern.split_once('/') {
            Some((p, t)) => (p, t),
            None => ("*", pattern),
        };
        let (pm, tm) = (glob_pattern(pp), glob_pattern(tp));
        let mut out: Vec<ThreadInfo> = self
            .threads
            .values()
            .filter(|t| pm.matches(&t.process) && tm.matches(&t.name))
            .cloned()
            .collect();
        out.sort_by(|a, b| (&a.process, &a.name, a.utid).cmp(&(&b.process, &b.name, b.utid)));
        out
    }

    /// The thread's states, sorted by ts.
    pub fn thread_states(&self, utid: i64) -> Rc<Vec<State>> {
        self.ensure_states(utid);
        self.states.borrow().get(&utid).cloned().unwrap_or_default()
    }

    /// The thread's sched slices, sorted by ts.
    pub fn sched(&self, utid: i64) -> Rc<Vec<SchedSlice>> {
        self.ensure_states(utid);
        self.sched.borrow().get(&utid).cloned().unwrap_or_default()
    }

    /// The thread's IPC events, sorted by ts.
    pub fn ipc(&self, utid: i64) -> Rc<Vec<IpcEvent>> {
        self.ensure_ipc(utid);
        self.ipc.borrow().get(&utid).cloned().unwrap_or_default()
    }

    /// Index of the state covering ts (or the last one starting before ts),
    /// -1 if none.
    pub fn state_index_at(&self, utid: i64, ts: i64) -> isize {
        let st = self.thread_states(utid);
        st.partition_point(|s| s.ts <= ts) as isize - 1
    }

    /// (index, state) of the thread's states overlapping [t0, t1).
    pub fn states_overlapping(&self, utid: i64, t0: i64, t1: i64) -> Vec<(usize, State)> {
        let lst = self.thread_states(utid);
        let mut i = lst.partition_point(|s| s.ts <= t0).saturating_sub(1);
        let mut out = Vec::new();
        while i < lst.len() {
            let s = &lst[i];
            if s.ts >= t1 {
                break;
            }
            if s.end() > t0 || s.dur < 0 {
                out.push((i, s.clone()));
            }
            i += 1;
        }
        out
    }

    /// The sched slice running at ts, if any.
    pub fn sched_at(&self, utid: i64, ts: i64) -> Option<SchedSlice> {
        let lst = self.sched(utid);
        let i = lst.partition_point(|s| s.ts <= ts);
        if i == 0 {
            return None;
        }
        let s = &lst[i - 1];
        if (s.ts <= ts && ts < s.end()) || s.dur < 0 { Some(s.clone()) } else { None }
    }

    /// The thread's last sched slice starting before ts, however long ago.
    pub fn sched_before(&self, utid: i64, ts: i64) -> Option<SchedSlice> {
        let lst = self.sched(utid);
        let i = lst.partition_point(|s| s.ts < ts);
        if i == 0 { None } else { Some(lst[i - 1].clone()) }
    }

    /// The thread's sched slices overlapping [t0, t1).
    pub fn sched_overlapping(&self, utid: i64, t0: i64, t1: i64) -> Vec<SchedSlice> {
        let lst = self.sched(utid);
        let mut i = lst.partition_point(|s| s.ts <= t0).saturating_sub(1);
        let mut out = Vec::new();
        while i < lst.len() && lst[i].ts < t1 {
            if lst[i].end() > t0 || lst[i].dur < 0 {
                out.push(lst[i].clone());
            }
            i += 1;
        }
        out
    }

    /// The thread's IPC events in [t0, t1].
    pub fn ipc_between(&self, utid: i64, t0: i64, t1: i64) -> Vec<IpcEvent> {
        let lst = self.ipc(utid);
        let mut i = lst.partition_point(|e| e.ts < t0);
        let mut out = Vec::new();
        while i < lst.len() && lst[i].ts <= t1 {
            out.push(lst[i].clone());
            i += 1;
        }
        out
    }

    /// The outgoing side of every message of a FIDL method, sorted by ts.
    pub fn ipc_for_method(&self, method: &str) -> Vec<IpcEvent> {
        match self.query(&crate::tp::queries::ipc_for_method(method)) {
            Ok(rows) => {
                let mut out: Vec<IpcEvent> =
                    rows.iter().map(ipc_from_row).filter(|e| e.outgoing).collect();
                out.sort_by_key(|e| e.ts);
                out
            }
            Err(e) => {
                self.note_error(e);
                Vec::new()
            }
        }
    }

    /// Non-IPC trace slices with this exact name: (ts, dur, utid).
    pub fn slices_named(&self, name: &str) -> Vec<(i64, i64, i64)> {
        match self.query(&crate::tp::queries::slices_named(name)) {
            Ok(rows) => {
                rows.iter().map(|r| (r.get::<i64>(0), r.get::<i64>(1), r.get::<i64>(2))).collect()
            }
            Err(e) => {
                self.note_error(e);
                Vec::new()
            }
        }
    }

    /// Every run of one interrupt, sorted by ts.
    pub fn irq_instances(&self, irq: i64) -> Vec<Irq> {
        let mut out: Vec<Irq> =
            self.irqs.values().flatten().filter(|i| i.irq == irq).cloned().collect();
        out.sort_by_key(|i| i.ts);
        out
    }

    /// The most recent IRQ on cpu that started within window_ns before ts.
    pub fn irq_before(&self, cpu: i64, ts: i64, window_ns: i64) -> Option<&Irq> {
        let lst = self.irqs.get(&cpu)?;
        let i = lst.partition_point(|x| x.ts <= ts);
        if i == 0 {
            return None;
        }
        let irq = &lst[i - 1];
        if ts - irq.ts <= window_ns { Some(irq) } else { None }
    }

    /// The latest IRQ on any CPU that started within window_ns before ts and
    /// was still running at ts.
    pub fn irq_any_cpu_containing(&self, ts: i64, window_ns: i64) -> Option<&Irq> {
        let mut best: Option<&Irq> = None;
        for &cpu in self.irqs.keys() {
            if let Some(irq) = self.irq_before(cpu, ts, window_ns).filter(|i| ts <= i.end()) {
                if best.is_none_or(|b| irq.ts > b.ts) {
                    best = Some(irq);
                }
            }
        }
        best
    }

    /// The CPU's processing rate at ts, from the trace's counters.
    pub fn rate_at(&self, cpu: i64, ts: i64, default: f64) -> f64 {
        let Some(lst) = self.rates.get(&cpu) else { return default };
        if lst.is_empty() {
            return default;
        }
        let i = lst.partition_point(|&(t, _)| t <= ts);
        if i == 0 { lst[0].1 } else { lst[i - 1].1 }
    }
}

/// A shell-style pattern (`*`, `?`, `[abc]`, `[!abc]`); one that does not
/// parse as a pattern matches itself literally.
fn glob_pattern(pattern: &str) -> glob::Pattern {
    // `glob` reserves `**` for whole path components; here any run of `*`
    // means the same as one.
    let mut collapsed = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if !(c == '*' && collapsed.ends_with('*')) {
            collapsed.push(c);
        }
    }
    glob::Pattern::new(&collapsed)
        .unwrap_or_else(|_| glob::Pattern::new(&glob::Pattern::escape(pattern)).unwrap())
}

fn ipc_from_row(r: crate::tp::Row<'_>) -> IpcEvent {
    IpcEvent {
        utid: r.get::<i64>(0),
        ts: r.get::<i64>(1),
        method: r.get::<String>(2),
        outgoing: r.get::<i64>(3) != 0,
        peer_utid: r.get::<Option<i64>>(4),
        peer_ts: r.get::<Option<i64>>(5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAIR_20: SchedulingParams = SchedulingParams::Fair { weight: 20 };

    #[test]
    fn scheduling_params_from_sched_priority() {
        assert_eq!(
            SchedulingParams::from_sched_priority(-4001500),
            SchedulingParams::Deadline { capacity_ns: 400 * 10_000, deadline_ns: 1500 * 10_000 }
        );
        assert_eq!(
            SchedulingParams::from_sched_priority(20),
            SchedulingParams::Fair { weight: 20 }
        );
        assert_eq!(SchedulingParams::from_sched_priority(-1500), SchedulingParams::Unknown);
        assert_eq!(SchedulingParams::from_sched_priority(i64::MIN), SchedulingParams::Unknown);
    }

    #[test]
    fn thread_patterns_are_shell_globs() {
        let m = |p: &str, s: &str| glob_pattern(p).matches(s);
        assert!(m("driver-host-*", "driver-host-#input"));
        assert!(m("fdf-dispatcher-thread-?", "fdf-dispatcher-thread-0"));
        assert!(!m("a?c", "ac"));
        assert!(m("[!x]y", "ay"));
        assert!(m("#input", "#input"));
        assert!(m("a.b", "a.b") && !m("a.b", "axb"));
        assert!(m("a**b", "axyb"));
        assert!(m("[", "["));
    }

    #[test]
    fn lookups() {
        let mut tr = Trace::new();
        tr.set_states(
            1,
            vec![
                State { ts: 10, dur: 5, state: StateKind::Woken, cpu: 0, waker_utid: Some(2) },
                State { ts: 0, dur: 10, state: StateKind::Blocked, cpu: -1, waker_utid: None },
            ],
        );
        tr.set_sched(
            1,
            vec![
                SchedSlice { ts: 15, dur: 5, cpu: 1, scheduling_params: FAIR_20 },
                SchedSlice { ts: 25, dur: -1, cpu: 1, scheduling_params: FAIR_20 },
            ],
        );
        tr.finalize();
        assert_eq!(tr.state_index_at(1, 12), 1);
        assert_eq!(tr.state_index_at(1, -1), -1);
        assert_eq!(tr.states_overlapping(1, 5, 12).len(), 2);
        assert!(tr.sched_at(1, 16).is_some());
        assert!(tr.sched_at(1, 20).is_none());
        assert_eq!(tr.sched_before(1, 30).map(|s| s.ts), Some(25));
        assert_eq!(tr.sched_overlapping(1, 0, 16).len(), 1);
        assert_eq!(tr.sched_overlapping(1, 30, 40).len(), 1);
    }
}
