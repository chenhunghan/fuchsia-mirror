// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Deadline-profile derivation (capacity / period / affinity).
//!
//! * period T: the window the output must land in. Taken from the consumer
//!   (user-supplied) when known, otherwise from the left tail of the measured
//!   inter-arrival distribution. Every thread on one critical path gets the
//!   same T so none can invert another.
//! * capacity C: the upper 95% confidence bound of a right-tail quantile of the
//!   thread's *normalized* work per period window (runtime x processing rate,
//!   which is what the kernel charges). Measured under the heaviest condition
//!   available.
//! * affinity: the CPUs of the loaded role whose parameters the thread runs
//!   with, else the CPU set most loaded deadline roles share; faster domains
//!   when those CPUs cannot host U = C/T at their top rate (checked only when
//!   the device reports which CPUs are in each domain).

use crate::analysis::AnalysisResult;
use crate::attribution::{Attribution, Bucket};
use crate::model::Trace;
use crate::profiles_file::{NS_PER_US, NS_PER_US_F64};
use crate::stats;
use crate::tasks::Job;
use crate::topology::Topology;
use indexmap::IndexMap;

/// A proposed deadline profile for one thread.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileProposal {
    pub utid: i64,
    pub label: String,
    pub period_ns: i64,
    pub capacity_ns: i64,
    pub affinity: Vec<i64>,
    pub utilization: f64,
    /// p50/p90/p99/max of the normalized work per period.
    pub work_per_period_ns: IndexMap<String, f64>,
    /// (capacity_ns, period_ns) currently in effect.
    pub existing: Option<(i64, i64)>,
    pub rate_source: String,
    pub notes: Vec<String>,
    /// The normalized work of each period window measured in this trace, so
    /// other captures can pool it (see [`DeriveOptions::pooled_work`]).
    pub work_ns: Vec<f64>,
}

impl ProfileProposal {
    /// Whether the proposal differs from the profile in effect, as a trace
    /// shows both (see [`same_traced`]). A placeholder without a period or
    /// capacity (no period could be determined) is no change: it cannot be
    /// applied.
    pub fn changed(&self) -> bool {
        if self.period_ns <= 0 || self.capacity_ns <= 0 {
            return false;
        }
        match self.existing {
            None => true,
            Some(e) => !(same_traced(self.capacity_ns, e.0) && same_traced(self.period_ns, e.1)),
        }
    }

    /// The role's line for a `.profiles` file. An empty affinity is left out:
    /// RoleManager reads `"affinity": []` as a mask with no CPUs, not as "any
    /// CPU".
    pub fn profiles_json(&self) -> String {
        let aff = if self.affinity.is_empty() {
            String::new()
        } else {
            let cpus = self.affinity.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ");
            format!(", \"affinity\": [{cpus}]")
        };
        format!(
            "{{ \"capacity\": \"{}us\", \"period\": \"{}us\"{aff} }}",
            self.capacity_ns.div_euclid(NS_PER_US),
            self.period_ns.div_euclid(NS_PER_US)
        )
    }
}

/// The step capacities are proposed in, and the smallest capacity proposed.
/// Zircon writes deadline parameters into traces in 10 us units, clamped to at
/// least one unit (`get_weight_or_packed_deadline_params` in the kernel's
/// `scheduler.rs`), so 10 us steps are the finest a trace can show: proposals
/// then compare exactly with the capacities read back from traces. The kernel
/// itself keeps nanoseconds and only requires 0 < capacity <= period.
const CAPACITY_STEP_NS: i64 = 10 * NS_PER_US;

/// The longest capacity or period proposed. Zircon rejects deadline
/// parameters above `SchedCompactDuration::max()` (i32::MAX ns, about 2.147 s;
/// `validate_and_create_profile` in the kernel's `profile_dispatcher.rs`);
/// this is that limit rounded down to the capacity step.
const MAX_PARAM_NS: i64 = (i32::MAX as i64 / CAPACITY_STEP_NS) * CAPACITY_STEP_NS;

/// Whether two durations look the same in a trace. The existing profile comes
/// from the trace, where Zircon truncates parameters to 10us units and clamps
/// them at 99.99ms, so a 150ms period reads back as 99.99ms.
fn same_traced(a_ns: i64, b_ns: i64) -> bool {
    crate::roles::as_traced_us(a_ns.div_euclid(NS_PER_US))
        == crate::roles::as_traced_us(b_ns.div_euclid(NS_PER_US))
}

/// Processing rate per (cpu, ts), from the trace's `Rate:CPU` counters.
/// Analysis requires them; a CPU the trace has no counter for is charged at
/// its domain's top rate, which can only overstate work.
pub struct RateModel<'a> {
    tr: &'a Trace,
    topo: &'a Topology,
    pub source: String,
}

impl std::fmt::Debug for RateModel<'_> {
    /// `Trace` is not `Debug` (it holds every event), so it is left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateModel")
            .field("topo", &self.topo)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl<'a> RateModel<'a> {
    pub fn new(tr: &'a Trace, topo: &'a Topology) -> Self {
        let source = if tr.has_rates {
            "trace kernel:power Rate:CPU counters"
        } else {
            "the domains' top rates (the trace has no Rate:CPU counters)"
        };
        RateModel { tr, topo, source: source.into() }
    }

    /// The rate-scaled work of running on `cpu` over [a, b), following rate
    /// changes inside the interval.
    pub fn work(&self, cpu: i64, a: i64, b: i64) -> f64 {
        crate::attribution::rate_scaled_work(self.tr, cpu, a, b, self.topo.rate(cpu))
    }
}

/// The consumer window, if it is one: only a positive `--window` counts.
pub fn active_window(window_ns: Option<i64>) -> Option<i64> {
    window_ns.filter(|&w| w > 0)
}

/// The note for a thread whose current profile covers its measured work.
const ALREADY_COVERED_NOTE: &str = "current profile already covers the measured work";

fn round_up(ns: i64, step_ns: i64) -> i64 {
    ((ns as f64 / step_ns as f64).ceil() * step_ns as f64) as i64
}

/// (period_ns, reason).
///
/// Priority: the consumer's window (the only number that is a real
/// requirement) > the shortest period already configured on the critical path
/// (someone chose it for a reason; keep all stages aligned to it) > the left
/// tail of the measured inter-arrival distribution (what the load demands).
pub fn choose_period(
    inter_arrival_ns: &[i64],
    consumer_window_ns: Option<i64>,
    existing_periods: &[i64],
    q: f64,
) -> (i64, String) {
    let (period, mut reason) = if let Some(w) = active_window(consumer_window_ns) {
        (w, "consumer window (--window)".to_string())
    } else if let Some(&m) = existing_periods.iter().min() {
        (m, "shortest period already configured on the critical path".to_string())
    } else if inter_arrival_ns.len() >= 20 {
        (
            stats::quantile_i64(inter_arrival_ns, q) as i64,
            format!("p{} inter-arrival of the measured load", (q * 100.0) as i64),
        )
    } else {
        return (0, "unknown (aperiodic or too few jobs)".to_string());
    };
    if period > MAX_PARAM_NS {
        reason += ", capped at Zircon's 2.147 s limit";
    }
    // Round down (shorter is conservative, lesson 4.1) to 10us, the precision
    // the trace keeps for deadline parameters.
    let step = CAPACITY_STEP_NS;
    (step.max(period.min(MAX_PARAM_NS).div_euclid(step) * step), reason)
}

/// The capacity that covers quantile q of `work` (normalized ns per period).
///
/// Capacity is the upper 95% confidence bound of the work quantile, so it
/// covers sampling error in the measured quantile instead of a fixed margin.
/// Zircon deadline threads have a hard quota, so an underestimate throttles.
pub fn capacity_for(work: &[f64], q: f64) -> i64 {
    let (_, upper) = stats::bootstrap_quantile_ci(work, q);
    // Rounded up first, so the capacity always covers the bound.
    round_up(upper.ceil() as i64, CAPACITY_STEP_NS).max(CAPACITY_STEP_NS)
}

fn window_work(tr: &Trace, rates: &RateModel<'_>, utid: i64, t0_win: i64, t1_win: i64) -> f64 {
    let mut w = 0.0;
    for sl in tr.sched_overlapping(utid, t0_win, t1_win) {
        let t0 = sl.ts.max(t0_win);
        let sl_end = if sl.dur < 0 { t1_win } else { sl.end() };
        let t1 = sl_end.min(t1_win);
        if t1 > t0 {
            w += rates.work(sl.cpu, t0, t1);
        }
    }
    w
}

/// The CPU a run segment without one is charged on: the first of the CPUs new
/// deadline roles get.
fn fallback_cpu(rates: &RateModel<'_>) -> i64 {
    rates.topo.default_affinity().0.first().copied().unwrap_or(0)
}

/// Normalized running work of utid per period window.
///
/// When a thread lacks a deadline profile under load, its execution for a job
/// may be delayed past [event_ts, event_ts + period) or split across multiple
/// waits. Once given a profile with period T, all of its causal work for one
/// job lands inside a single period T, so this takes the max of:
///   1) work in [event_ts, event_ts + period),
///   2) work in [first_run_in_job, first_run_in_job + period),
///   3) causal RUN segments attributed to utid for that job, and
///   4) when throttled during [seg.t0, seg.t1], the active kernel period
///      [seg.t1 - period, seg.t1] with the throttle stall removed:
///      [seg.t1 - period, seg.t0) plus [seg.t1, min(job.t1, seg.t1 + seg.dur)).
pub fn normalized_work_per_period(
    tr: &Trace,
    rates: &RateModel<'_>,
    utid: i64,
    anchors: &[i64],
    period_ns: i64,
    jobs: Option<&[Job]>,
    attributions: Option<&[Attribution]>,
) -> Vec<f64> {
    let mut out = Vec::with_capacity(anchors.len());
    for (idx, &a) in anchors.iter().enumerate() {
        let mut w = window_work(tr, rates, utid, a, a + period_ns);
        let j = jobs.and_then(|js| js.get(idx));
        if let Some(j) = j {
            if let Some(sl) = tr.sched_overlapping(utid, j.t0, j.t1).first() {
                let t_first = sl.ts.max(j.t0);
                w = w.max(window_work(tr, rates, utid, t_first, t_first + period_ns));
            }
        }
        if let Some(at) = attributions.and_then(|ats| ats.get(idx)) {
            let j_t1 = j.map_or(at.t1, |j| j.t1);
            let mut wc = 0.0;
            for seg in &at.segments {
                if seg.utid != utid {
                    continue;
                }
                if seg.bucket == Bucket::Run && seg.dur() > 0 {
                    let cpu = if seg.cpu >= 0 { seg.cpu } else { fallback_cpu(rates) };
                    wc += rates.work(cpu, seg.t0, seg.t1);
                } else if seg.bucket == Bucket::Throttled && 0 < seg.dur() && seg.dur() < period_ns
                {
                    // Assumes the period ends (and the budget replenishes) at
                    // seg.t1. If the CPU was busy at the replenishment, the
                    // stall ends later, the inferred period starts late and
                    // some pre-stall work is missed: an undercount, bounded
                    // below by the other estimates via max().
                    let w_thr = window_work(tr, rates, utid, seg.t1 - period_ns, seg.t0)
                        + window_work(tr, rates, utid, seg.t1, j_t1.min(seg.t1 + seg.dur()));
                    w = w.max(w_thr);
                }
            }
            w = w.max(wc);
        }
        out.push(w);
    }
    out
}

/// Options for [`derive`].
#[derive(Clone, Debug)]
pub struct DeriveOptions {
    pub consumer_window_ns: Option<i64>,
    /// Capacity covers the p99 of work per period: a 1% per-window overrun
    /// budget (lesson 4.1, 4.3). Stricter tasks should pass a higher
    /// --quantile.
    pub quantile: f64,
    pub utids: Option<Vec<i64>>,
    pub allow_shrink: bool,
    /// Work per period measured in other captures (repeats of the same
    /// condition), by (thread label, period ns): pooled with this trace's
    /// work before the capacity is sized.
    pub pooled_work: IndexMap<(String, i64), Vec<f64>>,
    /// Capacities other captures needed; a proposal for the same thread and
    /// period is raised to cover each (see [`Cover`]).
    pub cover: Vec<Cover>,
}

/// A capacity another capture of the measurement needed for one thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Cover {
    pub label: String,
    pub period_ns: i64,
    pub capacity_ns: i64,
    /// The capture, as shown in the note, e.g. "stress-1 (stress)".
    pub capture: String,
}

impl DeriveOptions {
    /// The defaults with a consumer window.
    pub fn with_window(consumer_window_ns: Option<i64>) -> Self {
        DeriveOptions { consumer_window_ns, ..Default::default() }
    }
}

impl Default for DeriveOptions {
    fn default() -> Self {
        DeriveOptions {
            consumer_window_ns: None,
            quantile: 0.99,
            utids: None,
            allow_shrink: false,
            pooled_work: IndexMap::new(),
            cover: Vec::new(),
        }
    }
}

/// Proposes a deadline profile for each thread on the critical path.
pub fn derive(
    tr: &Trace,
    res: &AnalysisResult,
    topo: &Topology,
    opts: &DeriveOptions,
) -> Vec<ProfileProposal> {
    let rates = RateModel::new(tr, topo);
    let existing_periods: Vec<i64> =
        res.threads.values().filter_map(|t| t.deadline_params.map(|p| p.1)).collect();
    // Left tail of inter-arrival (lesson 4.1): a period at p1 means at most 1%
    // of arrivals come early and wait for their window to open.
    let (period, preason) =
        choose_period(&res.inter_arrival_ns, opts.consumer_window_ns, &existing_periods, 0.01);
    let anchors: Vec<i64> = res.jobs.iter().map(|j| j.event_ts).collect();
    let anchor_utids = res.anchor_utids();
    let targets: Vec<i64> = match &opts.utids {
        Some(u) if !u.is_empty() => u.clone(),
        _ => res.threads.keys().copied().collect(),
    };
    let n_stages = res.threads.len();
    let window = active_window(opts.consumer_window_ns);
    let q = opts.quantile;
    let qpct = (q * 100.0) as i64;
    let mut proposals = Vec::new();
    for utid in targets {
        let tbd = res.threads.get(&utid);
        let label = tr.label(utid);
        let mut notes = vec![format!("period from {preason}")];
        if window.is_some() && n_stages > 1 {
            notes.push(format!(
                "the critical path has {n_stages} stages sharing this window; EDF orders each stage by \
                 its own deadline, so the end-to-end guarantee is weaker than one stage's (lesson 1.5)"
            ));
        }
        let existing = tbd.and_then(|t| t.deadline_params);
        if period <= 0 {
            notes.push(
                "no period could be determined; a fair-priority profile is the honest choice"
                    .into(),
            );
            proposals.push(ProfileProposal {
                utid,
                label,
                period_ns: 0,
                capacity_ns: 0,
                affinity: topo.default_affinity().0,
                utilization: 0.0,
                work_per_period_ns: IndexMap::new(),
                existing,
                rate_source: rates.source.clone(),
                notes,
                work_ns: Vec::new(),
            });
            continue;
        }
        let mut my_period = period;
        let wait_ns = tbd.map_or(0, |t| {
            t.total(Bucket::Throttled) + t.total(Bucket::Preempted) + t.total(Bucket::WakeLatency)
        });
        let is_wait_bound = tbd.is_some_and(|t| wait_ns > t.total(Bucket::Run));
        if let Some(ex) = existing {
            if !same_traced(ex.1, period)
                && window.is_none()
                && (anchor_utids.contains(&utid) || !is_wait_bound)
            {
                // Someone chose this thread's period for its own consumer (e.g.
                // a UI thread at 4ms); don't silently re-time it for this path.
                // Keep the period, recompute capacity for it, and flag it.
                my_period = ex.1;
                notes.push(format!(
                    "keeps its own period {}us (the chain's is {}us): shorter periods win EDF ties, so \
                     this thread yields to the others on the path; align only if its consumer allows \
                     (lesson 1.4)",
                    ex.1.div_euclid(NS_PER_US),
                    period.div_euclid(NS_PER_US)
                ));
            }
        }
        let work = normalized_work_per_period(
            tr,
            &rates,
            utid,
            &anchors,
            my_period,
            Some(&res.jobs),
            Some(&res.attributions),
        );
        if work.is_empty() {
            continue;
        }
        let mut pool = work.clone();
        if let Some(extra) = opts.pooled_work.get(&(label.clone(), my_period)) {
            pool.extend_from_slice(extra);
            notes.push(format!(
                "capacity sized from {} periods pooled over the repeats of this condition",
                pool.len()
            ));
        }
        let wq = stats::quantile(&pool, q);
        let mut cap = capacity_for(&pool, q);
        let period_for_thread = my_period;
        if cap > period_for_thread {
            // Zircon rejects a profile whose capacity exceeds its period.
            notes.push(format!(
                "the upper 95% bound of p{qpct} work per period ({}us) exceeds the period ({}us); \
                 capped at the period. One job's work does not fit one period, so a profile alone \
                 cannot meet this window",
                cap.div_euclid(NS_PER_US),
                period_for_thread.div_euclid(NS_PER_US)
            ));
            cap = period_for_thread;
        }
        if let Some(ex) = existing {
            if !opts.allow_shrink && same_traced(ex.1, period_for_thread) && cap < ex.0 {
                notes.push(format!(
                    "measured p{qpct} work ({:.0}us) fits the current capacity; keeping it (pass \
                     --allow-shrink to propose the smaller value)",
                    wq / NS_PER_US_F64
                ));
                cap = ex.0;
            }
        }
        // Affinity: a thread running with a loaded role's parameters most
        // likely has that role, and an existing role keeps its CPUs. A new role
        // gets the CPU set most loaded deadline roles share.
        // An unrestricted role keeps running anywhere: its affinity stays
        // empty, which leaves the role's own as written.
        let (aff, why) = match existing.and_then(|(c, p)| topo.affinity_for_params(c, p)) {
            Some((role, Some(a))) => {
                let why = format!("CPUs {a:?}: kept from {role}, whose parameters are in effect");
                (a, why)
            }
            Some((role, None)) => {
                (Vec::new(), format!("every CPU: kept from {role}, whose parameters are in effect"))
            }
            None => topo.default_affinity(),
        };
        notes.push(why);
        let util = cap as f64 / period_for_thread as f64;
        let aff = fit_affinity(topo, aff, util, &mut notes);
        // Demands on one core must sum to at most 1 (lesson 1.4, Eq. 1.6), and
        // less at lower OPPs (lesson 1.6): two tasks above 0.5 cannot share a
        // core.
        if util > 0.5 {
            notes.push(
                "utilization above 0.5: a single core can host at most one more such task; consider a \
                 longer period if the consumer allows"
                    .into(),
            );
        }
        if let Some(ex) = existing {
            if cap <= ex.0 && same_traced(period_for_thread, ex.1) {
                notes.push(ALREADY_COVERED_NOTE.into());
            } else if cap > ex.0 && same_traced(period_for_thread, ex.1) {
                notes.push(format!(
                    "measured p{qpct} work per period exceeds the current capacity {}us: the thread is \
                     being throttled in the tail",
                    ex.0.div_euclid(NS_PER_US)
                ));
            }
        }
        let s = stats::summary(&work);
        let mut wpp = IndexMap::new();
        for (k, v) in [("p50", s.p50), ("p90", s.p90), ("p99", s.p99), ("max", s.max)] {
            if let Some(v) = v {
                wpp.insert(k.to_string(), v);
            }
        }
        proposals.push(ProfileProposal {
            utid,
            label,
            period_ns: period_for_thread,
            capacity_ns: cap,
            affinity: aff,
            utilization: util,
            work_per_period_ns: wpp,
            existing,
            rate_source: rates.source.clone(),
            notes,
            work_ns: work,
        });
    }
    for p in proposals.iter_mut() {
        raise_to_cover(p, &opts.cover, topo);
    }
    proposals
}

/// Raises `p`'s capacity to each larger capacity another capture needed for
/// the same thread and period, then re-checks the CPUs fit.
fn raise_to_cover(p: &mut ProfileProposal, cover: &[Cover], topo: &Topology) {
    for c in cover {
        // Capped at the period like any capacity (Zircon rejects more), and
        // compared after the cap, so a raise that changes nothing adds no note.
        let covered = c.capacity_ns.min(p.period_ns);
        if c.label == p.label && c.period_ns == p.period_ns && covered > p.capacity_ns {
            p.capacity_ns = covered;
            // The raise may exceed the current profile after all.
            if p.existing.is_some_and(|ex| p.capacity_ns > ex.0) {
                p.notes.retain(|n| n != ALREADY_COVERED_NOTE);
            }
            p.utilization =
                if p.period_ns != 0 { p.capacity_ns as f64 / p.period_ns as f64 } else { 0.0 };
            p.notes.push(format!(
                "capacity raised to {}us to cover capture {}",
                p.capacity_ns.div_euclid(NS_PER_US),
                c.capture
            ));
            // The higher utilization may no longer fit the chosen CPUs.
            let aff = std::mem::take(&mut p.affinity);
            p.affinity = fit_affinity(topo, aff, p.utilization, &mut p.notes);
        }
    }
}

/// `aff` (empty: every CPU), or the faster domains when utilization `util`
/// does not fit it. A domain at its top rate R finishes R units of work per
/// unit of time, so it can host U only if U <= R (lesson 1.6). That needs to
/// know which domain each CPU is in, which an older cpuctl does not report.
fn fit_affinity(topo: &Topology, aff: Vec<i64>, util: f64, notes: &mut Vec<String>) -> Vec<i64> {
    let cpus = if aff.is_empty() { topo.cpus() } else { aff.clone() };
    let min_rate = topo.min_rate(&cpus);
    if !topo.knows_cpus() {
        if !notes.iter().any(|n| n == crate::topology::NO_CORE_IDS_NOTE) {
            notes.push(crate::topology::NO_CORE_IDS_NOTE.into());
        }
        return aff;
    }
    if util <= min_rate {
        return aff;
    }
    notes.push(format!(
        "U={util:.2} exceeds the top rate {min_rate:.2} of the slowest of these CPUs; the kernel \
         will steer this thread to faster cores or run it over budget"
    ));
    let faster: Vec<i64> = topo
        .domains
        .iter()
        .filter(|d| d.max_rate >= util)
        .flat_map(|d| d.cpus.iter().copied())
        .collect();
    if faster.is_empty() { aff } else { faster }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SchedSlice, SchedulingParams, ThreadInfo};

    #[test]
    fn period_choice() {
        assert_eq!(choose_period(&[], Some(850_000), &[], 0.01).0, 850_000);
        assert_eq!(choose_period(&[], Some(16_600_000), &[], 0.01).0, 16_600_000);
        assert_eq!(choose_period(&[], None, &[4_000_000, 1_000_000], 0.01).0, 1_000_000);
        assert_eq!(choose_period(&[1; 5], None, &[], 0.01).0, 0);
        let ia: Vec<i64> = (0..40).map(|i| 8_333_333 + i * 1000).collect();
        let (p, why) = choose_period(&ia, None, &[], 0.01);
        assert_eq!(p, 8_330_000);
        assert_eq!(why, "p1 inter-arrival of the measured load");
    }

    #[test]
    fn periods_are_capped_and_a_nonpositive_window_is_ignored() {
        let (p, why) = choose_period(&[], Some(5_000_000_000), &[], 0.01);
        assert_eq!(p, MAX_PARAM_NS);
        assert!(why.ends_with("capped at Zircon's 2.147 s limit"), "{why}");
        assert_eq!(choose_period(&[], Some(0), &[2_000_000], 0.01).0, 2_000_000);
        assert_eq!(choose_period(&[], Some(-1), &[2_000_000], 0.01).0, 2_000_000);
    }

    #[test]
    fn existing_profiles_compare_as_the_trace_shows_them() {
        // Traces clamp parameters at 99.99ms, so a 150ms profile reads back
        // as 99.99ms and still matches.
        assert!(same_traced(150_000_000, 99_990_000));
        assert!(same_traced(850_000, 855_000));
        assert!(!same_traced(850_000, 860_000));
    }

    #[test]
    fn affinity_stays_unrestricted_unless_the_cpus_are_too_slow() {
        let t = crate::topology::testing::sample();
        let mut notes = vec![];
        assert_eq!(fit_affinity(&t, vec![], 0.15, &mut notes), Vec::<i64>::new());
        assert!(notes.is_empty());
        // Every CPU includes the 0.202 domain, too slow for U=0.9.
        assert_eq!(fit_affinity(&t, vec![], 0.9, &mut notes), vec![7]);
    }

    #[test]
    fn a_covered_capacity_is_clamped_to_the_period_and_refitted() {
        let t = crate::topology::testing::sample();
        let mut p = ProfileProposal {
            utid: 1,
            label: "p/t[1]".into(),
            period_ns: 1_000_000,
            capacity_ns: 100_000,
            affinity: vec![],
            utilization: 0.1,
            work_per_period_ns: IndexMap::new(),
            existing: None,
            rate_source: String::new(),
            notes: vec![],
            work_ns: vec![],
        };
        let c = |period_ns| Cover {
            label: "p/t[1]".into(),
            period_ns,
            capacity_ns: 3_000_000,
            capture: "stress-1".into(),
        };
        raise_to_cover(&mut p, &[c(2_000_000)], &t);
        assert_eq!(p.capacity_ns, 100_000);
        raise_to_cover(&mut p, &[c(1_000_000)], &t);
        assert_eq!((p.capacity_ns, p.utilization, p.affinity.clone()), (1_000_000, 1.0, vec![7]));
    }

    #[test]
    fn capacity_covers_the_sampling_error_of_the_quantile() {
        let work: Vec<f64> =
            (0..300).map(|i| 100_000.0 + ((i * 7919) % 1013) as f64 * 50.0).collect();
        let cap = capacity_for(&work, 0.99);
        let (_, upper) = stats::bootstrap_quantile_ci(&work, 0.99);
        assert!(cap as f64 >= upper && (cap as f64) < upper + CAPACITY_STEP_NS as f64, "{cap}");
        assert!(cap as f64 >= stats::quantile(&work, 0.99));
        assert_eq!(cap % CAPACITY_STEP_NS, 0);
        // Pooling repeats narrows the interval, so the capacity does not grow.
        let pooled: Vec<f64> = work.iter().chain(work.iter()).copied().collect();
        assert!(capacity_for(&pooled, 0.99) <= cap);
        assert_eq!(capacity_for(&[1.0], 0.99), CAPACITY_STEP_NS);
    }

    #[test]
    fn work_per_period_counts_runs_in_the_window() {
        // A 15 ms period with 5.5 ms of big-core run time in it.
        let mut tr = Trace::new();
        tr.threads.insert(
            1,
            ThreadInfo { utid: 1, tid: 10, name: "t".into(), pid: 1, process: "p".into() },
        );
        tr.set_sched(
            1,
            vec![
                SchedSlice {
                    ts: 1_000_000,
                    dur: 2_000_000,
                    cpu: 7,
                    scheduling_params: SchedulingParams::Fair { weight: 20 },
                },
                SchedSlice {
                    ts: 5_000_000,
                    dur: 3_500_000,
                    cpu: 7,
                    scheduling_params: SchedulingParams::Fair { weight: 20 },
                },
                SchedSlice {
                    ts: 30_000_000,
                    dur: 1_000_000,
                    cpu: 7,
                    scheduling_params: SchedulingParams::Fair { weight: 20 },
                },
            ],
        );
        tr.finalize();
        let t = crate::topology::testing::sample();
        let rates = RateModel::new(&tr, &t);
        let w = normalized_work_per_period(&tr, &rates, 1, &[0], 15_000_000, None, None);
        assert_eq!(w, vec![5_500_000.0]);
    }

    #[test]
    fn profiles_json_line() {
        let p = ProfileProposal {
            utid: 1,
            label: "p/t[1]".into(),
            period_ns: 850_000,
            capacity_ns: 100_000,
            affinity: vec![2, 3],
            utilization: 0.1,
            work_per_period_ns: IndexMap::new(),
            existing: Some((20_000, 850_000)),
            rate_source: String::new(),
            notes: vec![],
            work_ns: vec![],
        };
        assert_eq!(
            p.profiles_json(),
            r#"{ "capacity": "100us", "period": "850us", "affinity": [2, 3] }"#
        );
        assert!(p.changed());
    }
}
