// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Analysis driver: turns one trace and a task description into everything the
//! verdict, the capacity derivation and the reports need.
//!
//! 1. The task spec finds the jobs: one time window per occurrence of the task
//!    on its anchor thread (`tasks`).
//! 2. Each job's time is attributed along its critical path into buckets (run,
//!    preempted, wake latency, throttled, hardware wait, ...), following waker
//!    edges into the threads the job waited on (`attribution`).
//! 3. The attributions are aggregated per bucket, per thread, per blocking IPC
//!    hop and per hardware wait, and each thread on the path gets the profile
//!    it normally runs with. Inter-arrival times of the jobs feed the period.

use crate::attribution::{Attribution, Attributor, Bucket, LEAF_BUCKETS};
use crate::model::{SchedSlice, SchedulingParams, Trace};
use crate::profiles_file::NS_PER_US_F64;
use crate::stats;
use crate::tasks::{self, Job, TaskSpec};
use crate::topology::Topology;
use indexmap::IndexMap;

/// The task did not run during the capture.
#[derive(Debug)]
pub struct NoJobs(pub String);

impl std::fmt::Display for NoJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoJobs {}

/// One thread's share of the attributed time.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadBreakdown {
    pub utid: i64,
    pub label: String,
    /// bucket -> per-job ns, only for jobs the thread appears in and buckets it
    /// spent time in (so vectors of different buckets can differ in length).
    pub per_job: IndexMap<Bucket, Vec<i64>>,
    /// (capacity_ns, period_ns) of the thread's resident deadline profile, if
    /// any (see [`resident_profile`]).
    pub deadline_params: Option<(i64, i64)>,
    pub fair_weight: Option<i64>,
    /// cpu -> run ns.
    pub cpus: IndexMap<i64, i64>,
}

impl ThreadBreakdown {
    pub fn total(&self, b: Bucket) -> i64 {
        self.per_job.get(&b).map_or(0, |v| v.iter().sum())
    }

    /// Total over every leaf bucket.
    pub fn total_leaf(&self) -> i64 {
        LEAF_BUCKETS.iter().map(|&b| self.total(b)).sum()
    }
}

/// A hop on the critical path: (from utid, to utid, "-> Method" or "<- Method").
pub type HopKey = (i64, i64, String);

/// Everything an analysis found.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisResult {
    pub spec: TaskSpec,
    pub trace_path: String,
    /// The attributed jobs (evenly sampled beyond `max_jobs`).
    pub jobs: Vec<Job>,
    pub attributions: Vec<Attribution>,
    /// bucket -> per-job ns, summed over all threads on the chain.
    pub bucket_per_job: IndexMap<Bucket, Vec<i64>>,
    pub threads: IndexMap<i64, ThreadBreakdown>,
    /// Every job's response time (all jobs, not just the attributed ones).
    pub response_ns: Vec<i64>,
    pub inter_arrival_ns: Vec<i64>,
    pub has_rates: bool,
    pub notes: Vec<String>,
    /// Per-occurrence blocked ns of each hop.
    pub hops: IndexMap<HopKey, Vec<i64>>,
    /// (utid, irq) -> per-occurrence ns.
    pub hw_waits: IndexMap<(i64, i64), Vec<i64>>,
}

impl AnalysisResult {
    /// The anchor threads, sorted.
    pub fn anchor_utids(&self) -> Vec<i64> {
        let mut v: Vec<i64> = self.jobs.iter().map(|j| j.utid).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The utids of the threads that matter most: (total leaf ns, utid),
    /// biggest first.
    pub fn threads_by_total(&self) -> Vec<(i64, i64)> {
        let mut rows: Vec<(i64, i64)> =
            self.threads.iter().map(|(&u, t)| (t.total_leaf(), u)).collect();
        rows.sort_by(|a, b| b.cmp(a));
        rows
    }
}

/// True if the task matches FIDL method names on kernel:ipc slices.
fn uses_ipc_names(spec: &TaskSpec) -> bool {
    spec.kind == tasks::TaskKind::Fidl
        || [&spec.start, &spec.end]
            .iter()
            .any(|e| e.starts_with("ipc:") || e.starts_with("ipc-in:"))
}

/// True if the trace has kernel:ipc slices and none is named after a FIDL method:
/// `ffx trace` could not symbolize the ordinals.
pub fn ipc_unsymbolized(tr: &Trace) -> bool {
    let Ok(rows) = tr.query(&crate::tp::queries::facts_methods()) else { return false };
    !rows.is_empty() && rows.iter().all(|r| r.get::<String>(0) == "ChannelMessage")
}

/// Analyzes `spec` in `tr`.
pub fn analyze(
    tr: &Trace,
    spec: TaskSpec,
    topo: Option<&Topology>,
    max_jobs: usize,
    max_depth: usize,
) -> anyhow::Result<AnalysisResult> {
    let mut notes = Vec::new();
    // The kernel charges run time scaled by each CPU's processing rate. Zircon
    // writes a Rate:CPU counter only when a CPU changes operating point, so a
    // capture where every CPU stays at one has none. Charging each CPU at its
    // domain's top rate can only overstate work, which errs toward reporting
    // throttling.
    if !tr.has_rates {
        notes.push(
            "the trace has no kernel:power Rate:CPU counters (Zircon writes them only when a CPU \
             changes operating point); each CPU is charged at its domain's top rate"
                .to_string(),
        );
    }
    let (spec, jobs) = tasks::resolve(tr, spec)?;
    tr.check()?;
    if jobs.is_empty() {
        if uses_ipc_names(&spec) && ipc_unsymbolized(tr) {
            return Err(NoJobs(format!(
                "no jobs found for {}: the trace's FIDL messages are not symbolized (every \
                 kernel:ipc slice is named ChannelMessage), so ipc:, ipc-in: and fidl: tasks cannot \
                 match. `ffx trace` names them from the FIDL IR listed in the build directory's \
                 all_fidl_json.txt and skips that if any listed file is missing (a stale build \
                 directory): rebuild it (`fx build`) and measure again, or name the task with irq: \
                 and slice: events",
                spec.label()
            ))
            .into());
        }
        return Err(NoJobs(format!(
            "no jobs found for {}: the task did not run during the capture \
             (screen off? load not exercised? wrong thread/method name?)",
            spec.label()
        ))
        .into());
    }
    let mut arrivals: Vec<i64> = jobs.iter().map(|j| j.event_ts).collect();
    arrivals.sort_unstable();
    let (bursts, gap_min) = merge_bursts(&arrivals);
    let inter: Vec<i64> = bursts.windows(2).filter(|w| w[1] > w[0]).map(|w| w[1] - w[0]).collect();
    if bursts.len() < arrivals.len() {
        notes.push(format!(
            "{} wakes merged into {} arrivals (bursts < {:.0}us apart)",
            arrivals.len(),
            bursts.len(),
            gap_min / NS_PER_US_F64
        ));
    }
    let all_jobs = jobs;
    let jobs: Vec<Job> = if all_jobs.len() > max_jobs {
        let step = all_jobs.len() as f64 / max_jobs as f64;
        notes.push(format!("attributed {max_jobs} of {} jobs (evenly sampled)", all_jobs.len()));
        (0..max_jobs).map(|i| all_jobs[(i as f64 * step) as usize].clone()).collect()
    } else {
        all_jobs.clone()
    };
    let at = Attributor::new(tr, max_depth, topo);
    let attributions: Vec<Attribution> =
        jobs.iter().map(|j| at.attribute(j.utid, j.t0, j.t1)).collect();

    let mut bucket_per_job: IndexMap<Bucket, Vec<i64>> =
        LEAF_BUCKETS.iter().map(|&b| (b, Vec::new())).collect();
    let mut threads: IndexMap<i64, ThreadBreakdown> = IndexMap::new();
    let mut hops: IndexMap<HopKey, Vec<i64>> = IndexMap::new();
    let mut hw_waits: IndexMap<(i64, i64), Vec<i64>> = IndexMap::new();
    for a in &attributions {
        let bb = a.by_bucket();
        for b in LEAF_BUCKETS {
            bucket_per_job.entry(b).or_default().push(bb.get(&b).copied().unwrap_or(0));
        }
        for ((utid, b), ns) in a.by_thread_bucket() {
            let tbd = threads.entry(utid).or_insert_with(|| ThreadBreakdown {
                utid,
                label: tr.label(utid),
                per_job: IndexMap::new(),
                deadline_params: None,
                fair_weight: None,
                cpus: IndexMap::new(),
            });
            tbd.per_job.entry(b).or_default().push(ns);
        }
        for s in &a.segments {
            match s.bucket {
                Bucket::Run if s.cpu >= 0 => {
                    if let Some(tbd) = threads.get_mut(&s.utid) {
                        *tbd.cpus.entry(s.cpu).or_insert(0) += s.dur();
                    }
                }
                Bucket::BlockedOn => {
                    let Some(peer) = s.detail else { continue };
                    // A request/reply hop: we sent a message to the waker before blocking.
                    // A request is written just before the client blocks. 300us
                    // allows a little client work between the write and the wait
                    // (60% of hops in the recorded touch traces: under 50us). The 2us is timestamp
                    // tolerance.
                    let mut method = String::new();
                    let mut reply = true;
                    for ev in tr.ipc_between(s.utid, s.t0 - 300_000, s.t0 + 2_000) {
                        if ev.outgoing && ev.peer_utid == Some(peer) {
                            method = ev.method.clone();
                        }
                    }
                    if method.is_empty() {
                        // A pipeline hop: the waker handed us new work (its message woke us).
                        // The waking message is read soon after the wake (the recorded
                        // touch traces: 74% within 100us); reads before t1 happened before
                        // the thread blocked, and later reads are more likely
                        // unrelated.
                        reply = false;
                        for ev in tr.ipc_between(s.utid, s.t1, s.t1 + 100_000) {
                            if !ev.outgoing && ev.peer_utid == Some(peer) {
                                method = ev.method.clone();
                                break;
                            }
                        }
                    }
                    let m = if method.is_empty() { "?".to_string() } else { method };
                    let key = (s.utid, peer, format!("{} {m}", if reply { "->" } else { "<-" }));
                    hops.entry(key).or_default().push(s.dur());
                }
                Bucket::HwWait => {
                    // -1 marks a timer or self wake; 0 can be a real interrupt.
                    let irq = match s.detail {
                        Some(d) if d >= 0 => d,
                        _ => -1,
                    };
                    hw_waits.entry((s.utid, irq)).or_default().push(s.dur());
                }
                _ => {}
            }
        }
    }

    for (&utid, tbd) in threads.iter_mut() {
        (tbd.deadline_params, tbd.fair_weight) = resident_profile(&tr.sched(utid));
    }

    let response: Vec<i64> = all_jobs.iter().map(|j| j.dur()).collect();
    tr.check()?;
    Ok(AnalysisResult {
        trace_path: tr.path.clone(),
        spec,
        jobs,
        attributions,
        bucket_per_job,
        threads,
        response_ns: response,
        inter_arrival_ns: inter,
        has_rates: tr.has_rates,
        notes,
        hops,
        hw_waits,
    })
}

/// Merges wakes much closer together than the typical arrival spacing (for
/// example two IRQs per report) into one arrival; returns the merged arrivals
/// and the merge threshold in ns.
///
/// The threshold comes from the arrival gaps, not job duration, so a slow
/// system does not merge more arrivals than a fast one. It is a quarter of the
/// p75 gap, not the median: with two wakes per arrival half the gaps are the
/// short ones inside an arrival, so the median would be one of them. With up
/// to three wakes per arrival at most 2/3 of the gaps are inside an arrival,
/// so the p75 still lands on a real inter-arrival gap. The quantile counts
/// gaps, so long pauses in manual input only move it when they are over 25%
/// of the gaps.
pub fn merge_bursts(sorted_arrivals: &[i64]) -> (Vec<i64>, f64) {
    let gaps: Vec<i64> = sorted_arrivals.windows(2).map(|w| w[1] - w[0]).collect();
    let gap_min = if gaps.is_empty() { 0.0 } else { 0.25 * stats::quantile_i64(&gaps, 0.75) };
    let mut bursts: Vec<i64> = Vec::new();
    for &a in sorted_arrivals {
        if bursts.last().is_none_or(|&l| (a - l) as f64 >= gap_min) {
            bursts.push(a);
        }
    }
    (bursts, gap_min)
}

/// The profile a thread normally runs with, from all of its sched slices:
/// (deadline params, fair weight), at most one of them set.
///
/// While a deadline thread waits on a fair server, Zircon lends the server the
/// client's deadline parameters, and the trace shows those borrowed values.
/// Job windows are exactly when that happens, so a single slice can make a fair
/// server look like it has its own deadline profile. Like the gate's
/// `deadline_threads` query, a thread counts as a deadline thread only when at
/// least 20 of its slices, and more than half of them, run under deadline
/// parameters, and its parameters are the most common ones (ties: more run
/// time, then the larger packed value).
pub fn resident_profile(slices: &[SchedSlice]) -> (Option<(i64, i64)>, Option<i64>) {
    let mut deadline: IndexMap<(i64, i64), (usize, i64)> = IndexMap::new();
    let mut fair: IndexMap<i64, (usize, i64)> = IndexMap::new();
    let mut dl_count = 0;
    for s in slices {
        let dur = s.dur.max(0);
        match s.scheduling_params {
            SchedulingParams::Deadline { capacity_ns, deadline_ns } => {
                dl_count += 1;
                let e = deadline.entry((capacity_ns, deadline_ns)).or_default();
                e.0 += 1;
                e.1 += dur;
            }
            SchedulingParams::Fair { weight } => {
                let e = fair.entry(weight).or_default();
                e.0 += 1;
                e.1 += dur;
            }
            SchedulingParams::Unknown => {}
        }
    }
    if dl_count >= 20 && dl_count * 2 > slices.len() {
        // The trace's packed value is -(capacity * 10_000 + deadline) in 10 us
        // units; "priority ascending" in the query means the larger params win.
        let best = deadline
            .iter()
            .max_by(|(ka, a), (kb, b)| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(ka.cmp(kb)))
            .map(|(&k, _)| k);
        return (best, None);
    }
    let best = fair
        .iter()
        .max_by(|(ka, a), (kb, b)| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(kb.cmp(ka)))
        .map(|(&k, _)| k);
    (None, best)
}

/// Default number of jobs to attribute per capture. 1500 jobs puts 150 jobs in
/// the p90 tail, enough for tail bucket shares to within about 3 percentage
/// points (the recorded touch traces), at bounded analysis time.
pub const MAX_JOBS: usize = 1500;
/// Default recursion limit. Covers the longest blocked-on chains seen (IRQ,
/// driver host, input pipeline, scene manager) with room to spare. Deeper
/// chains show up as unattributed time.
pub const MAX_DEPTH: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{State, StateKind, ThreadInfo};

    #[test]
    fn a_capture_without_rate_counters_is_analyzed_with_top_rates() {
        let mut tr = Trace::new();
        tr.threads.insert(
            1,
            ThreadInfo { utid: 1, tid: 11, name: "t".into(), pid: 100, process: "p".into() },
        );
        let st = |ts, dur, state, waker| State { ts, dur, state, cpu: 0, waker_utid: waker };
        let mut states = Vec::new();
        for k in 0..30 {
            let t = k * 1_000_000;
            states.push(st(t, 10_000, StateKind::Woken, Some(2)));
            states.push(st(t + 10_000, 100_000, StateKind::Running, None));
            states.push(st(t + 110_000, 890_000, StateKind::Blocked, None));
        }
        tr.set_states(1, states);
        tr.set_sched(
            1,
            (0..30)
                .map(|k| SchedSlice {
                    ts: k * 1_000_000 + 10_000,
                    dur: 100_000,
                    cpu: 0,
                    scheduling_params: SchedulingParams::Fair { weight: 20 },
                })
                .collect(),
        );
        tr.set_ipc(1, vec![]);
        tr.finalize();
        assert!(!tr.has_rates);
        let res =
            analyze(&tr, tasks::parse("thread:p/t").unwrap(), None, MAX_JOBS, MAX_DEPTH).unwrap();
        assert!(res.jobs.len() >= 29, "{}", res.jobs.len());
        assert!(
            res.notes.iter().any(|n| n.contains("no kernel:power Rate:CPU counters")),
            "{:?}",
            res.notes
        );
    }

    #[test]
    fn bursts_merge_from_the_arrival_gaps() {
        // Reports every 4 ms, each as two IRQs 50 us apart: half the gaps are
        // the short ones, so the threshold must come from the long ones.
        let mut a = Vec::new();
        for k in 0..20 {
            a.push(k * 4_000_000);
            a.push(k * 4_000_000 + 50_000);
        }
        let (b, gap_min) = merge_bursts(&a);
        assert_eq!(b, (0..20).map(|k| k * 4_000_000).collect::<Vec<_>>());
        assert!(gap_min > 50_000.0 && gap_min < 4_000_000.0, "{gap_min}");
        // Evenly spaced arrivals never merge, however long each job takes.
        let even: Vec<i64> = (0..20).map(|k| k * 2_000_000).collect();
        assert_eq!(merge_bursts(&even).0, even);
        // Three wakes per report still merge.
        let mut t = Vec::new();
        for k in 0..20 {
            t.extend([k * 4_000_000, k * 4_000_000 + 30_000, k * 4_000_000 + 60_000]);
        }
        assert_eq!(merge_bursts(&t).0, (0..20).map(|k| k * 4_000_000).collect::<Vec<_>>());
        assert_eq!(merge_bursts(&[5]).0, vec![5]);
        assert!(merge_bursts(&[]).0.is_empty());
    }
}
