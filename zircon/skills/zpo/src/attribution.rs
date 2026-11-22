// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Latency attribution: explains every nanosecond of a window on a thread.
//!
//! Given a thread and a time window [t0, t1], the window is split into
//! segments and each segment is put into one bucket:
//!
//! ```text
//! run             the thread executed instructions (own work)
//! preempted       thread was runnable but another thread had the CPU (state R)
//! wake_latency    thread was woken and waited for a CPU (state W)
//! throttled       deadline thread that had already consumed its capacity in
//!                 the current period (subset of R, detected from packed params)
//! blocked_on      thread was blocked and was woken by another thread: the
//!                 interval is recursively attributed to that thread (depth+1)
//! hw_wait         thread was blocked and was woken by an interrupt / timer
//! blocked_unknown blocked, but the wake is outside the window or the
//!                 recursion limit was hit
//! ```
//!
//! The buckets map onto fixes: run -> optimize code; preempted/wake_latency ->
//! deadline profile; blocked_on -> fix the dependency (colocate / remove hop /
//! profile it); hw_wait -> hardware or protocol change; throttled -> capacity
//! too small.

use crate::model::{Irq, State, StateKind, Trace};
use crate::topology::Topology;
use indexmap::IndexMap;
use std::collections::HashSet;

/// Where a segment of time went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bucket {
    Run,
    Preempted,
    WakeLatency,
    Throttled,
    BlockedOn,
    HwWait,
    BlockedUnknown,
}

impl Bucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Bucket::Run => "run",
            Bucket::Preempted => "preempted",
            Bucket::WakeLatency => "wake_latency",
            Bucket::Throttled => "throttled",
            Bucket::BlockedOn => "blocked_on",
            Bucket::HwWait => "hw_wait",
            Bucket::BlockedUnknown => "blocked_unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Bucket> {
        LEAF_BUCKETS
            .iter()
            .chain(std::iter::once(&Bucket::BlockedOn))
            .copied()
            .find(|b| b.as_str() == s)
    }
}

/// The buckets time is finally attributed to (blocked_on recurses instead).
pub const LEAF_BUCKETS: [Bucket; 6] = [
    Bucket::Run,
    Bucket::Preempted,
    Bucket::WakeLatency,
    Bucket::Throttled,
    Bucket::HwWait,
    Bucket::BlockedUnknown,
];

/// How far before a wake [`Attributor::irq_wake`] looks for the start of the
/// IRQ handler that may contain it. Search bound only: 99.98% of IRQ handlers
/// in the recorded touch traces finish within 50us.
const IRQ_SEARCH_NS: i64 = 50_000;

/// A deadline thread counts as having used up its period once its rate-scaled
/// run time in the trailing period reaches this share of its capacity.
/// Zircon charges rate-scaled run time against capacity. In the recorded touch
/// traces real exhaustion measures 1.0-1.1 of capacity and 0.80-0.94 is a valley, so
/// 0.88 separates throttles from ordinary preemptions with margin for rate
/// error.
const EXHAUSTED_SHARE: f64 = 0.88;

/// Trace timestamp precision when matching a stall's end to a period end.
const PERIOD_END_TOLERANCE_NS: i64 = 10_000;

/// Whether `ts` falls in the IRQ handler's slice. A handler still running
/// when the trace ends has no end (`dur` -1) and contains everything after
/// its start.
fn irq_contains(irq: &Irq, ts: i64) -> bool {
    irq.dur < 0 || ts <= irq.end()
}

/// The latest IRQ handler on any CPU whose slice contains `ts`.
fn irq_on_any_cpu(tr: &Trace, ts: i64) -> Option<&Irq> {
    tr.irqs
        .keys()
        .filter_map(|&cpu| tr.irq_before(cpu, ts, IRQ_SEARCH_NS).filter(|i| irq_contains(i, ts)))
        .max_by_key(|i| i.ts)
}

/// The rate-scaled work of running on `cpu` over [a, b), which is what the
/// kernel charges. The CPU's rate can change in the middle of a sched slice,
/// so each `Rate:CPU` step is weighted by how long it lasted. Without
/// counters for the CPU, it runs at `fallback`.
pub fn rate_scaled_work(tr: &Trace, cpu: i64, a: i64, b: i64, fallback: f64) -> f64 {
    if b <= a {
        return 0.0;
    }
    let Some(steps) = tr.rates.get(&cpu).filter(|v| !v.is_empty()) else {
        return (b - a) as f64 * fallback;
    };
    let (mut t, mut rate, mut work) = (a, tr.rate_at(cpu, a, fallback), 0.0);
    for &(ts, r) in &steps[steps.partition_point(|&(ts, _)| ts <= a)..] {
        if ts >= b {
            break;
        }
        work += (ts - t) as f64 * rate;
        (t, rate) = (ts, r);
    }
    work + (b - t) as f64 * rate
}

/// A stretch of time on one thread, in one bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub utid: i64,
    pub t0: i64,
    pub t1: i64,
    pub bucket: Bucket,
    pub depth: usize,
    /// For blocked_on: the utid this thread waited for. For hw_wait: the IRQ
    /// number (-1 for a timer or self wake).
    pub detail: Option<i64>,
    pub cpu: i64,
}

impl Segment {
    fn new(utid: i64, t0: i64, t1: i64, bucket: Bucket, depth: usize) -> Self {
        Segment { utid, t0, t1, bucket, depth, detail: None, cpu: -1 }
    }

    fn with_cpu(mut self, cpu: i64) -> Self {
        self.cpu = cpu;
        self
    }

    fn with_detail(mut self, d: i64) -> Self {
        self.detail = Some(d);
        self
    }

    pub fn dur(&self) -> i64 {
        self.t1 - self.t0
    }
}

/// The attribution of one window.
#[derive(Clone, Debug, PartialEq)]
pub struct Attribution {
    pub utid: i64,
    pub t0: i64,
    pub t1: i64,
    /// Sorted by (t0, depth): [`Attributor::attribute`] sorts them before it
    /// returns, and callers that walk the timeline rely on that order.
    pub segments: Vec<Segment>,
}

impl Attribution {
    pub fn total(&self) -> i64 {
        self.t1 - self.t0
    }

    /// Every segment but blocked_on.
    pub fn leaf_segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.bucket != Bucket::BlockedOn)
    }

    /// Leaf time per bucket, in the order buckets first appear.
    pub fn by_bucket(&self) -> IndexMap<Bucket, i64> {
        let mut out = IndexMap::new();
        for s in self.leaf_segments() {
            *out.entry(s.bucket).or_insert(0) += s.dur();
        }
        out
    }

    /// Leaf time per (thread, bucket), in the order they first appear.
    pub fn by_thread_bucket(&self) -> IndexMap<(i64, Bucket), i64> {
        let mut out = IndexMap::new();
        for s in self.leaf_segments() {
            *out.entry((s.utid, s.bucket)).or_insert(0) += s.dur();
        }
        out
    }

    /// Threads on the critical path, in order of first appearance.
    pub fn chain(&self) -> Vec<i64> {
        let mut seen = Vec::new();
        for s in &self.segments {
            if !seen.contains(&s.utid) {
                seen.push(s.utid);
            }
        }
        seen
    }
}

/// Recursive window attribution over a loaded trace.
pub struct Attributor<'a> {
    tr: &'a Trace,
    max_depth: usize,
    topo: Option<&'a Topology>,
}

impl std::fmt::Debug for Attributor<'_> {
    /// `Trace` is not `Debug` (it holds every event), so it is left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attributor")
            .field("max_depth", &self.max_depth)
            .field("topo", &self.topo)
            .finish_non_exhaustive()
    }
}

impl<'a> Attributor<'a> {
    /// `topo` supplies the rates throttle detection assumes for CPUs without
    /// rate counters: the domain's top rate, else 1.0 (the reference core).
    pub fn new(tr: &'a Trace, max_depth: usize, topo: Option<&'a Topology>) -> Self {
        Attributor { tr, max_depth, topo }
    }

    /// The rate charged on `cpu` when the trace has no rate counter for it.
    fn fallback_rate(&self, cpu: i64) -> f64 {
        // 1.0 is the reference core at its top OPP (lesson 1.3), and a domain's
        // top rate is the most it can be charged: either can only overstate
        // usage, which errs toward reporting throttling. A CPU whose domain is
        // unknown (no topology, or a cpuctl without core IDs) gets 1.0.
        self.topo.map_or(1.0, |t| t.rate(cpu))
    }

    pub fn attribute(&self, utid: i64, t0: i64, t1: i64) -> Attribution {
        let mut segs = Vec::new();
        let mut stack = HashSet::new();
        self.attribute_into(utid, t0, t1, 0, &mut segs, &mut stack);
        segs.sort_by_key(|s| (s.t0, s.depth));
        Attribution { utid, t0, t1, segments: segs }
    }

    fn attribute_into(
        &self,
        utid: i64,
        t0: i64,
        t1: i64,
        depth: usize,
        out: &mut Vec<Segment>,
        stack: &mut HashSet<i64>,
    ) {
        if t1 <= t0 {
            return;
        }
        let tr = self.tr;
        let states = tr.thread_states(utid);
        if states.is_empty() {
            out.push(Segment::new(utid, t0, t1, Bucket::BlockedUnknown, depth));
            return;
        }
        let mut covered = t0;
        for (idx, s) in tr.states_overlapping(utid, t0, t1) {
            let a = s.ts.max(t0);
            let b = if s.dur < 0 { t1 } else { s.end().min(t1) };
            if b <= a {
                continue;
            }
            if a > covered {
                // Gap in the state model (trace start / dropped records).
                out.push(Segment::new(utid, covered, a, Bucket::BlockedUnknown, depth));
            }
            covered = b;
            match s.state {
                StateKind::Running => {
                    out.push(Segment::new(utid, a, b, Bucket::Run, depth).with_cpu(s.cpu));
                }
                StateKind::Woken | StateKind::Preempted => {
                    let bucket = if self.is_throttled(utid, &s) {
                        Bucket::Throttled
                    } else if s.state == StateKind::Woken {
                        Bucket::WakeLatency
                    } else {
                        Bucket::Preempted
                    };
                    out.push(Segment::new(utid, a, b, bucket, depth).with_cpu(s.cpu));
                }
                StateKind::Blocked => self.blocked(utid, idx, &s, a, b, depth, out, stack),
                StateKind::Other(_) => {
                    out.push(Segment::new(utid, a, b, Bucket::BlockedUnknown, depth))
                }
            }
        }
        if covered < t1 {
            out.push(Segment::new(utid, covered, t1, Bucket::BlockedUnknown, depth));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blocked(
        &self,
        utid: i64,
        idx: usize,
        s: &State,
        a: i64,
        b: i64,
        depth: usize,
        out: &mut Vec<Segment>,
        stack: &mut HashSet<i64>,
    ) {
        let states = self.tr.thread_states(utid);
        // The wake that ends this block is the next state (W). The block may
        // extend past the window (e.g. the window ends when a message is
        // enqueued for this thread, before it is woken); the waker's activity
        // inside the window is still the critical path, so the eventual waker
        // is used regardless.
        let nxt = states.get(idx + 1);
        let Some(nxt) = nxt.filter(|n| n.state == StateKind::Woken && s.dur >= 0) else {
            out.push(Segment::new(utid, a, b, Bucket::BlockedUnknown, depth));
            return;
        };
        let wake_ts = nxt.ts;
        let waker = nxt.waker_utid;
        if let Some(irq) = self.irq_wake(waker, utid, wake_ts) {
            out.push(Segment::new(utid, a, b, Bucket::HwWait, depth).with_detail(irq.irq));
            return;
        }
        let waker = match waker {
            Some(w) if w != utid && w != 0 => w,
            _ => {
                // Timer / self wake: treat as external.
                out.push(Segment::new(utid, a, b, Bucket::HwWait, depth).with_detail(-1));
                return;
            }
        };
        if depth + 1 >= self.max_depth || stack.contains(&waker) {
            out.push(Segment::new(utid, a, b, Bucket::BlockedUnknown, depth).with_detail(waker));
            return;
        }
        out.push(Segment::new(utid, a, b, Bucket::BlockedOn, depth).with_detail(waker));
        stack.insert(utid);
        self.attribute_into(waker, a, b, depth + 1, out, stack);
        stack.remove(&utid);
    }

    /// Detects wakes performed from an interrupt handler.
    ///
    /// Zircon records the current thread as the waker
    /// (`zircon/kernel/kernel/scheduler.rs`), which in IRQ context is the
    /// thread the handler interrupted, and writes the record from inside the
    /// handler. So an interrupt wake lies inside the handler's slice on the
    /// waker's CPU: `irq.ts <= wake_ts <= irq.end()`. In the recorded touch
    /// traces, wakes just after an IRQ ends are the interrupted thread's own syscalls,
    /// so there is no slack.
    fn irq_wake(&self, waker: Option<i64>, utid: i64, wake_ts: i64) -> Option<&'a Irq> {
        let tr = self.tr;
        let waker = match waker {
            Some(w) if w != utid => w,
            // When the woken thread is reported as its own waker the wake came
            // from IRQ context on some CPU (in the recorded touch traces every such
            // wake sits inside an IRQ slice, a median 8us after it starts), so
            // test containment on every CPU.
            _ => return irq_on_any_cpu(tr, wake_ts),
        };
        let Some(sl) = tr.sched_at(waker, wake_ts) else {
            // Waker not running at the wake instant: cannot have issued a syscall.
            return irq_on_any_cpu(tr, wake_ts);
        };
        tr.irq_before(sl.cpu, wake_ts, IRQ_SEARCH_NS).filter(|irq| irq_contains(irq, wake_ts))
    }

    /// True if a deadline thread had used up its capacity in the period
    /// before this R/W state and stayed runnable until that period ended.
    ///
    /// Zircon gives a thread that used up its capacity a new activation
    /// starting when the current period ends, and does not run it before then
    /// (`update_activation` in `zircon/kernel/kernel/scheduler.rs`). A stall
    /// that ends earlier is ordinary preemption or wake latency.
    fn is_throttled(&self, utid: i64, s: &State) -> bool {
        let tr = self.tr;
        // When the thread was blocked, its profile is only on sched slices, so
        // read it from the last slice before it slept, however long ago. A
        // thread can only be throttled when it wakes inside the period it used
        // up (Zircon does not replenish until the period ends), so a sleep
        // longer than the period finds no usage below.
        let Some(prev) = tr.sched_before(utid, s.ts) else {
            return false;
        };
        let Some((cap_ns, period_ns)) = prev.deadline_params() else { return false };
        let mut used = 0.0f64;
        for sl in tr.sched_overlapping(utid, s.ts - period_ns, s.ts) {
            let a = sl.ts.max(s.ts - period_ns);
            let b = sl.end().min(s.ts);
            if b > a {
                used += rate_scaled_work(tr, sl.cpu, a, b, self.fallback_rate(sl.cpu));
            }
        }
        if used < cap_ns as f64 * EXHAUSTED_SHARE {
            return false;
        }
        // The used-up period started when the thread entered the run queue
        // (a wake, a preemption, or a release from an earlier throttle), less
        // than one period before this state. The earliest state change to a
        // runnable or running state in that span bounds the start from below,
        // so the period cannot end before `earliest + period`.
        let Some(earliest) = tr
            .states_overlapping(utid, s.ts - period_ns, s.ts)
            .into_iter()
            .map(|(_, st)| st)
            .find(|st| st.ts > s.ts - period_ns && st.ts < s.ts && st.state != StateKind::Blocked)
            .map(|st| st.ts)
        else {
            return false;
        };
        let end = if s.dur < 0 { i64::MAX } else { s.end() };
        end >= earliest + period_ns - PERIOD_END_TOLERANCE_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IpcEvent, SchedSlice, SchedulingParams, ThreadInfo};

    fn st(ts: i64, dur: i64, state: StateKind, cpu: i64, waker: Option<i64>) -> State {
        State { ts, dur, state, cpu, waker_utid: waker }
    }

    pub(crate) fn thread(utid: i64, name: &str) -> ThreadInfo {
        ThreadInfo { utid, tid: utid * 10, name: name.into(), pid: 1, process: "proc".into() }
    }

    /// Thread 1 runs, blocks on thread 2 (which runs and wakes it), then runs.
    fn chain_trace() -> Trace {
        let mut tr = Trace::new();
        tr.threads.insert(1, thread(1, "client"));
        tr.threads.insert(2, thread(2, "server"));
        tr.set_states(
            1,
            vec![
                st(0, 100, StateKind::Running, 3, None),
                st(100, 400, StateKind::Blocked, -1, None),
                st(500, 50, StateKind::Woken, 3, Some(2)),
                st(550, 100, StateKind::Running, 3, None),
            ],
        );
        tr.set_states(
            2,
            vec![
                st(0, 150, StateKind::Blocked, -1, None),
                st(150, 50, StateKind::Woken, 4, Some(1)),
                st(200, 300, StateKind::Running, 4, None),
                st(500, 1000, StateKind::Blocked, -1, None),
            ],
        );
        tr.set_ipc(
            1,
            vec![IpcEvent {
                ts: 90,
                utid: 1,
                method: "Echo".into(),
                outgoing: true,
                peer_utid: Some(2),
                peer_ts: Some(160),
            }],
        );
        tr.finalize();
        tr
    }

    #[test]
    fn work_follows_rate_changes_inside_a_slice() {
        let mut tr = Trace::new();
        tr.rates.insert(0, vec![(0, 0.5), (100, 1.0)]);
        // 100 ns at 0.5 and 100 ns at 1.0, not 200 ns at the start rate.
        assert_eq!(rate_scaled_work(&tr, 0, 0, 200, 0.2), 150.0);
        assert_eq!(rate_scaled_work(&tr, 0, 150, 250, 0.2), 100.0);
        // No counters for the CPU: the fallback rate.
        assert_eq!(rate_scaled_work(&tr, 3, 0, 100, 0.25), 25.0);
        assert_eq!(rate_scaled_work(&tr, 0, 10, 10, 1.0), 0.0);
    }

    #[test]
    fn a_wake_inside_an_irq_open_at_trace_end_is_an_interrupt_wake() {
        let mut tr = Trace::new();
        tr.irqs.insert(2, vec![Irq { ts: 1_000, dur: -1, cpu: 2, irq: 42 }]);
        assert_eq!(irq_on_any_cpu(&tr, 1_500).map(|i| i.irq), Some(42));
        assert_eq!(irq_on_any_cpu(&tr, 500), None);
    }

    #[test]
    fn a_cpu_in_an_unknown_domain_is_charged_as_the_fastest() {
        let tr = chain_trace();
        let t = crate::topology::testing::sample();
        let old = crate::topology::testing::sample_without_ids();
        assert_eq!(Attributor::new(&tr, 6, Some(&t)).fallback_rate(0), 0.202);
        assert_eq!(Attributor::new(&tr, 6, Some(&old)).fallback_rate(0), 1.0);
        assert_eq!(Attributor::new(&tr, 6, None).fallback_rate(0), 1.0);
    }

    #[test]
    fn follows_the_waker() {
        let tr = chain_trace();
        let a = Attributor::new(&tr, 6, None).attribute(1, 0, 650);
        assert!(a.segments.windows(2).all(|w| (w[0].t0, w[0].depth) <= (w[1].t0, w[1].depth)));
        let bb = a.by_bucket();
        assert_eq!(bb.get(&Bucket::Run), Some(&(100 + 300 + 100)));
        // Both threads waited 50 ns for a CPU after being woken.
        assert_eq!(bb.get(&Bucket::WakeLatency), Some(&100));
        // Thread 2 was blocked in [100, 150): its own waker is thread 1, which
        // is on the stack, so that part is unknown.
        assert_eq!(bb.get(&Bucket::BlockedUnknown), Some(&50));
        assert_eq!(a.chain(), vec![1, 2]);
        let total: i64 = bb.values().sum();
        assert_eq!(total, a.total());
    }

    /// A deadline thread with 50 us of capacity per 1000 us period runs 60 us
    /// from 0, then waits runnable (`stall` = R or W) until `resume`.
    fn used_up(stall: StateKind, resume: i64) -> Trace {
        let mut tr = Trace::new();
        tr.threads.insert(1, thread(1, "dl"));
        let mut states = vec![st(0, 60_000, StateKind::Running, 7, None)];
        if stall == StateKind::Woken {
            states.push(st(60_000, 10_000, StateKind::Blocked, -1, None));
            states.push(st(70_000, resume - 70_000, StateKind::Woken, 7, Some(2)));
        } else {
            states.push(st(60_000, resume - 60_000, stall, 7, None));
        }
        states.push(st(resume, 10_000, StateKind::Running, 7, None));
        tr.set_states(1, states);
        let params = SchedulingParams::Deadline { capacity_ns: 50_000, deadline_ns: 1_000_000 };
        tr.set_sched(
            1,
            vec![
                SchedSlice { ts: 0, dur: 60_000, cpu: 7, scheduling_params: params },
                SchedSlice { ts: resume, dur: 10_000, cpu: 7, scheduling_params: params },
            ],
        );
        tr.finalize();
        tr
    }

    fn bucket_ns(tr: &Trace, b: Bucket) -> i64 {
        Attributor::new(tr, 6, None)
            .attribute(1, 0, 1_010_000)
            .by_bucket()
            .get(&b)
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn throttled_when_capacity_used_up_until_the_period_ends() {
        // The period started at 0, so it ends at 1000 us.
        let tr = used_up(StateKind::Preempted, 1_000_000);
        assert_eq!(bucket_ns(&tr, Bucket::Throttled), 940_000);
        let tr = used_up(StateKind::Woken, 995_000);
        assert_eq!(bucket_ns(&tr, Bucket::Throttled), 925_000);
    }

    #[test]
    fn a_stall_that_ends_inside_the_used_up_period_is_not_throttling() {
        // Zircon does not run a thread before its used-up period ends, so a
        // thread that runs again at 100 us was not throttled.
        let tr = used_up(StateKind::Preempted, 100_000);
        assert_eq!(bucket_ns(&tr, Bucket::Throttled), 0);
        assert_eq!(bucket_ns(&tr, Bucket::Preempted), 40_000);
        let tr = used_up(StateKind::Woken, 100_000);
        assert_eq!(bucket_ns(&tr, Bucket::Throttled), 0);
        assert_eq!(bucket_ns(&tr, Bucket::WakeLatency), 30_000);
    }

    /// Thread 1 blocks at 0 and is woken at `wake_ts` with `waker`; thread 2
    /// runs on CPU 0 throughout; IRQ 9 runs on CPU 0 over [100, 200) ns and
    /// IRQ 5 on CPU 1 over [1000, 1100) ns.
    fn irq_trace(wake_ts: i64, waker: i64) -> Trace {
        let mut tr = Trace::new();
        tr.threads.insert(1, thread(1, "woken"));
        tr.threads.insert(2, thread(2, "other"));
        tr.set_states(
            1,
            vec![
                st(0, wake_ts, StateKind::Blocked, -1, None),
                st(wake_ts, 10, StateKind::Woken, 0, Some(waker)),
                st(wake_ts + 10, 10, StateKind::Running, 0, None),
            ],
        );
        tr.set_states(2, vec![st(0, 5_000, StateKind::Running, 0, None)]);
        let fair = SchedulingParams::Fair { weight: 20 };
        tr.set_sched(2, vec![SchedSlice { ts: 0, dur: 5_000, cpu: 0, scheduling_params: fair }]);
        tr.irqs.insert(0, vec![Irq { ts: 100, dur: 100, cpu: 0, irq: 9 }]);
        tr.irqs.insert(1, vec![Irq { ts: 1_000, dur: 100, cpu: 1, irq: 5 }]);
        tr.finalize();
        tr
    }

    fn hw_detail(tr: &Trace, t1: i64) -> Option<i64> {
        let a = Attributor::new(tr, 6, None).attribute(1, 0, t1);
        a.segments.iter().find(|s| s.bucket == Bucket::HwWait).and_then(|s| s.detail)
    }

    #[test]
    fn interrupt_wakes_lie_inside_the_handler() {
        // Waker running on CPU 0: inside IRQ 9 is an interrupt wake.
        assert_eq!(hw_detail(&irq_trace(150, 2), 170), Some(9));
        assert_eq!(hw_detail(&irq_trace(200, 2), 220), Some(9));
        // Just after the handler ends it is the waker's own wake.
        let tr = irq_trace(201, 2);
        let a = Attributor::new(&tr, 6, None).attribute(1, 0, 221);
        assert!(a.segments.iter().all(|s| s.bucket != Bucket::HwWait), "{a:?}");
        // A self wake inside IRQ 5 on another CPU names that IRQ, even 50 ns
        // after the handler started.
        assert_eq!(hw_detail(&irq_trace(1_050, 1), 1_070), Some(5));
        // A self wake outside every handler is a timer or self wake.
        assert_eq!(hw_detail(&irq_trace(1_101, 1), 1_121), Some(-1));
    }
}
