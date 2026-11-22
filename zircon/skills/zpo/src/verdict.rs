// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Verdict: is this a code / dependency / hardware problem, or a scheduling
//! one?
//!
//! The decision is made on the *tail* (p90 by default), because that is what a
//! deadline profile is bought for, and in this order:
//!
//! 1. throttled: a deadline thread ran out of capacity -> capacity too small
//! 2. scheduling: runnable-but-not-running (W + R) dominates -> profile
//! 3. hardware: waiting on an interrupt / timer dominates -> HW/protocol
//! 4. own work: the chain's own instructions dominate -> optimize code, and if
//!    the work is split over many IPC hops, the hops
//! 5. fine: nothing dominates and the response meets the window
//!
//! A profile proposal is only *recommended* under (1) and (2). Under (3)/(4)
//! the proposals are still computed but reported as "not yet: fix the work
//! first", with the measured numbers so the user can see why.

use crate::analysis::AnalysisResult;
use crate::attribution::{Attribution, Bucket, LEAF_BUCKETS};
use crate::derive::{self, DeriveOptions, ProfileProposal};
use crate::extensions::Lessons;
use crate::model::Trace;
use crate::profiles_file::NS_PER_US_F64;
use crate::stats;
use crate::topology::Topology;
use indexmap::IndexMap;
use serde::Serialize;

/// One finding of the verdict.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    /// throttled | scheduling | hardware | own_work | hops | fine | window | info
    pub kind: String,
    /// high | medium | low | info
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub utids: Vec<i64>,
    pub lesson: String,
}

impl Finding {
    fn new(
        kind: &str,
        severity: &str,
        title: String,
        detail: String,
        utids: Vec<i64>,
        lesson: String,
    ) -> Self {
        Finding { kind: kind.into(), severity: severity.into(), title, detail, utids, lesson }
    }
}

/// The verdict on an analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// throttled | scheduling | hardware | own_work | fine
    pub primary: String,
    pub headline: String,
    pub findings: Vec<Finding>,
    pub recommend_profiles: bool,
    pub profile_utids: Vec<i64>,
    pub tail_q: f64,
    /// Leaf bucket -> share of the tail.
    pub shares_tail: IndexMap<Bucket, f64>,
    /// The document the findings' lesson links point into ("" without one).
    pub lessons_url: String,
}

/// The response-time tail the verdict is decided on. The p90 tail (150 of 1500
/// jobs) keeps bucket shares stable to about 3 points between repeats on the
/// recorded touch traces; a p99 tail of 15 jobs does not.
pub const TAIL_Q: f64 = 0.90;

/// What one analyzed capture concludes. Every front end (the `analyze`
/// command, each capture of a measurement, the measurement's summary) gets
/// its proposals and verdict here.
#[derive(Clone, Debug)]
pub struct Conclusion {
    /// A proposal for every thread on the critical path (other captures pool
    /// and cover from these).
    pub proposals: Vec<ProfileProposal>,
    /// The proposals worth showing: the verdict's culprits, the anchor
    /// threads and the six threads with the most attributed time, so a report
    /// stays readable.
    pub shown: Vec<ProfileProposal>,
    pub verdict: Verdict,
}

/// Derives profiles, decides the verdict on the `tail_q` tail and picks the
/// proposals to show.
pub fn conclude(
    res: &AnalysisResult,
    tr: &Trace,
    topo: &Topology,
    opts: &DeriveOptions,
    tail_q: f64,
    lessons: &Lessons,
) -> Conclusion {
    let proposals = derive::derive(tr, res, topo, opts);
    let verdict = decide(res, tr, &proposals, tail_q, opts.consumer_window_ns, lessons);
    let mut keep: std::collections::HashSet<i64> =
        verdict.profile_utids.iter().copied().chain(res.anchor_utids()).collect();
    keep.extend(res.threads_by_total().iter().take(6).map(|&(_, u)| u));
    let shown = proposals.iter().filter(|p| keep.contains(&p.utid)).cloned().collect();
    Conclusion { proposals, shown, verdict }
}

/// A candidate's verdict once the gate found it improved on the baseline:
/// (primary, headline, recommend_profiles).
///
/// When the candidate's own verdict recommends no profile change
/// (`recommends_any` is false), the improvement is the whole story: "fine",
/// and the remaining time is the chain's normal execution. Otherwise the
/// candidate still needs a change, so its own verdict stands and the headline
/// says it improved first. `improvements` are the gate's improved checks.
pub fn after_improvement(
    own_primary: &str,
    own_headline: &str,
    recommend_profiles: bool,
    recommends_any: bool,
    improvements: &[String],
) -> (String, String, bool) {
    let imp = improvements.join("; ");
    if !recommends_any {
        let h = format!(
            "Optimization succeeded vs. baseline ({imp}). Remaining time is the chain's normal execution."
        );
        return ("fine".into(), h, false);
    }
    let still = match own_primary {
        "throttled" => "throttled",
        "scheduling" => "waiting for a CPU",
        "hardware" => "waiting on hardware",
        "own_work" => "spending the tail in its own code",
        other => other,
    };
    let h = format!("Improved vs. baseline ({imp}), but still {still}: {own_headline}");
    (own_primary.into(), h, recommend_profiles)
}

/// The headline of each primary verdict.
pub fn headline(primary: &str) -> &'static str {
    match primary {
        "throttled" => {
            "Scheduler: capacity is undersized for the measured work -- raise it (profile proposed below)."
        }
        "scheduling" => {
            "Scheduler: the work is done but waits for a CPU -- deadline profile proposed below."
        }
        "hardware" => {
            "Scheduler profiles are adequate for this code; the tail is spent waiting on hardware. \
             Any further gains are in the protocol or hardware path."
        }
        "own_work" => {
            "Scheduler profiles are already adequate for this code; the tail is the chain's own \
             execution. Any further gains are in the code itself."
        }
        _ => {
            "Nothing dominates the tail; the task is in good shape. No profile change recommended."
        }
    }
}

/// Bucket shares among the jobs in the top (1-q) tail of response time, the
/// tail's mean response time, and the indices of the tail's attributions.
fn share_at_tail(res: &AnalysisResult, q: f64) -> (IndexMap<Bucket, f64>, f64, Vec<usize>) {
    let mut pairs: Vec<(i64, usize)> =
        res.attributions.iter().enumerate().map(|(i, a)| (a.total(), i)).collect();
    pairs.sort();
    let k = ((pairs.len() as f64 * (1.0 - q)) as usize).max(1);
    let tail_idx: Vec<usize> =
        pairs[pairs.len().saturating_sub(k)..].iter().map(|&(_, i)| i).collect();
    let mut sums: IndexMap<Bucket, f64> = LEAF_BUCKETS.iter().map(|&b| (b, 0.0)).collect();
    let mut tot = 0.0;
    for &i in &tail_idx {
        for (b, v) in res.attributions[i].by_bucket() {
            *sums.entry(b).or_insert(0.0) += v as f64;
        }
        tot += res.attributions[i].total() as f64;
    }
    let shares =
        sums.into_iter().map(|(b, v)| (b, if tot != 0.0 { v / tot } else { 0.0 })).collect();
    (shares, tot / tail_idx.len().max(1) as f64, tail_idx)
}

/// The lower 95% bootstrap bound of the tail's share of `buckets`,
/// resampling tail jobs.
fn share_lower_bound(atts: &[Attribution], tail_idx: &[usize], buckets: &[Bucket]) -> f64 {
    let (mut num, mut den) = (Vec::new(), Vec::new());
    for &i in tail_idx {
        let bb = atts[i].by_bucket();
        num.push(buckets.iter().map(|b| bb.get(b).copied().unwrap_or(0)).sum::<i64>() as f64);
        den.push(atts[i].total() as f64);
    }
    stats::bootstrap_ratio_ci(&num, &den).0
}

/// The runnable share of the tail above which the scheduling finding is high
/// severity and primary: above 40% runnable time the wait drives the tail even
/// when own work is larger.
const SCHED_PRIMARY: f64 = 0.4;

/// The most throttled threads a throttled verdict names: a few, so the
/// report stays about the threads that matter.
const THROTTLED_CULPRITS: usize = 3;
/// The most waiting threads a scheduling verdict names.
const SCHED_CULPRITS: usize = 4;
/// The threads the own-work finding names.
const OWN_WORK_THREADS: usize = 3;

/// (total over `buckets`, utid) per thread, biggest first.
fn thread_rank(res: &AnalysisResult, buckets: &[Bucket]) -> Vec<(i64, i64)> {
    let mut rows: Vec<(i64, i64)> =
        res.threads.iter().map(|(&u, t)| (buckets.iter().map(|&b| t.total(b)).sum(), u)).collect();
    rows.sort_by(|a, b| b.cmp(a));
    rows
}

/// A tail over the window is never fine, even when no bucket crosses its
/// finding threshold: adds a finding for the miss and, instead of "fine",
/// returns the primary of the largest of `buckets` (primary, tail share).
fn note_window_miss(
    findings: &mut Vec<Finding>,
    primary: &'static str,
    p99_ns: f64,
    window_ns: i64,
    buckets: &[(&'static str, f64)],
) -> &'static str {
    if p99_ns <= window_ns as f64 {
        return primary;
    }
    let text = format!(
        "p99 response {:.0}us misses the {:.0}us window",
        p99_ns / NS_PER_US_F64,
        window_ns as f64 / NS_PER_US_F64
    );
    let detail =
        "The task misses its window at the tail; the verdict names the largest share of that tail.";
    findings.insert(0, Finding::new("window", "high", text, detail.into(), vec![], String::new()));
    if primary != "fine" {
        return primary;
    }
    buckets.iter().fold(("fine", 0.0), |m, &b| if b.1 > m.1 { b } else { m }).0
}

/// Up to `n` threads with the most time in `buckets`, none with 0.
fn top_threads(res: &AnalysisResult, buckets: &[Bucket], n: usize) -> Vec<i64> {
    thread_rank(res, buckets).into_iter().filter(|&(v, _)| v != 0).map(|(_, u)| u).take(n).collect()
}

/// The threads a recommendation is about: the culprits of the throttled and
/// scheduling findings, else (a window miss made the verdict without such a
/// finding) the top throttled or waiting threads.
fn culprit_utids(findings: &[Finding], primary: &str, res: &AnalysisResult) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for f in findings.iter().filter(|f| f.kind == "throttled" || f.kind == "scheduling") {
        for &u in &f.utids {
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    if out.is_empty() {
        out = if primary == "throttled" {
            top_threads(res, &[Bucket::Throttled], THROTTLED_CULPRITS)
        } else {
            let waits = [Bucket::Preempted, Bucket::WakeLatency, Bucket::Throttled];
            top_threads(res, &waits, SCHED_CULPRITS)
        };
    }
    out
}

/// Why a throttled or scheduling verdict proposes no profile change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoChange {
    /// No period could be determined, so there is no deadline profile to
    /// propose (the proposals are placeholders).
    NoPeriod,
    /// Every culprit already runs under a profile that covers its work: the
    /// wait is contention between deadline threads, not a missing contract.
    Contention,
}

/// [`NoChange`] for proposals none of which changes the culprits' profiles.
fn why_no_change(proposals: &[ProfileProposal], culprits: &[i64]) -> NoChange {
    let theirs: Vec<&ProfileProposal> =
        proposals.iter().filter(|p| culprits.contains(&p.utid)).collect();
    if !theirs.is_empty() && theirs.iter().all(|p| p.period_ns <= 0) {
        NoChange::NoPeriod
    } else {
        NoChange::Contention
    }
}

impl NoChange {
    fn finding(self, lessons: &Lessons) -> Finding {
        let (title, detail, lesson) = match self {
            NoChange::NoPeriod => (
                "No period could be determined, so no deadline profile is proposed",
                "The load looks aperiodic or the capture has too few jobs, and neither --window nor \
                 an existing deadline profile on the path gives a period. Pass --window with the \
                 consumer's deadline, or record a longer capture.",
                "period",
            ),
            NoChange::Contention => (
                "No profile parameter change is derivable from this data",
                "The threads that wait already have deadline profiles whose capacity covers the \
                 measured work. The remaining wait comes from other deadline threads with earlier \
                 deadlines on the same cores, or from wake-time placement. Options: shorten the period \
                 of this path (only if its consumer window allows), spread affinity across more cores, \
                 or reduce competing deadline work (lesson 6.2).",
                "affinity",
            ),
        };
        Finding::new("info", "medium", title.into(), detail.into(), vec![], lessons.link(lesson))
    }

    fn headline(self) -> &'static str {
        match self {
            NoChange::NoPeriod => {
                "Scheduler: the path waits for CPUs, but no period could be determined, so no \
                 deadline profile can be proposed. See findings."
            }
            NoChange::Contention => {
                "Scheduler: the path waits for CPUs, but every thread on it already has an adequate \
                 profile -- this is contention/placement, not a missing contract. See findings."
            }
        }
    }
}

/// Decides what the tail is spent on.
pub fn decide(
    res: &AnalysisResult,
    tr: &Trace,
    proposals: &[ProfileProposal],
    tail_q: f64,
    window_ns: Option<i64>,
    lessons: &Lessons,
) -> Verdict {
    let (shares, tail_mean, tail_idx) = share_at_tail(res, tail_q);
    let sh = |b: Bucket| shares.get(&b).copied().unwrap_or(0.0);
    let sched = sh(Bucket::Preempted) + sh(Bucket::WakeLatency);
    let thr = sh(Bucket::Throttled);
    let hw = sh(Bucket::HwWait);
    let run = sh(Bucket::Run);
    let unknown = sh(Bucket::BlockedUnknown);
    let mut findings: Vec<Finding> = Vec::new();
    let resp = stats::summary_i64(&res.response_ns);
    let labels = |us: &[i64]| us.iter().map(|&u| tr.label(u)).collect::<Vec<_>>().join(", ");

    // 1. Throttling. Capacity covers p99 work, so about 1% of jobs may overrun.
    // If every overrun landed in the p90 tail, that is 10% of the tail; more
    // than that means capacity is too small, not unlucky.
    if thr >= 0.10 {
        let culprits = top_threads(res, &[Bucket::Throttled], THROTTLED_CULPRITS);
        findings.push(Finding::new(
            "throttled",
            "high",
            format!("{:.0}% of the tail is a deadline thread sitting on an exhausted budget", thr * 100.0),
            "The thread used its whole capacity inside the period and was made ineligible until the \
             next period. Its capacity is too small for the work it actually does per period. \
             Raise capacity; do not touch the period."
                .into(),
            culprits,
            lessons.link("throttled"),
        ));
    }

    // 2. Scheduling
    let sched_threads: Vec<(i64, i64)> =
        thread_rank(res, &[Bucket::Preempted, Bucket::WakeLatency, Bucket::Throttled])
            .into_iter()
            .filter(|&(v, _)| v != 0)
            .collect();
    // A quarter of tail time spent runnable is enough that priority or
    // placement changes can win back more than tuning own work. The lower
    // confidence bound must clear it, so one noisy capture does not raise it.
    let sched_lo =
        share_lower_bound(&res.attributions, &tail_idx, &[Bucket::Preempted, Bucket::WakeLatency]);
    // Absolute floor so a large share of a very short tail is not reported:
    // below 100us of runnable wait per tail job, a placement change saves less
    // than ten 10us trace ticks.
    if sched_lo >= 0.25 || (!sched_threads.is_empty() && sched * tail_mean >= 100_000.0) {
        let culprits: Vec<i64> =
            sched_threads.iter().take(SCHED_CULPRITS).map(|&(_, u)| u).collect();
        let no_profile: Vec<i64> =
            culprits.iter().copied().filter(|u| res.threads[u].deadline_params.is_none()).collect();
        let undersized: Vec<i64> = culprits
            .iter()
            .copied()
            .filter(|u| {
                res.threads[u].deadline_params.is_some()
                    && proposals.iter().any(|p| p.utid == *u && p.changed())
            })
            .collect();
        let mut detail = format!(
            "Runnable-but-not-running time is {:.0}% of the tail ({:.0}us of {:.0}us). ",
            sched * 100.0,
            sched * tail_mean / NS_PER_US_F64,
            tail_mean / NS_PER_US_F64
        );
        if !no_profile.is_empty() {
            detail += &format!(
                "Threads without a deadline profile on the path: {}. A deadline profile is the right \
                 tool here: the work is already done, it just waits for a CPU.",
                labels(&no_profile)
            );
        } else if !undersized.is_empty() {
            detail += &format!(
                "Threads on the path with an undersized deadline profile: {}. Raising their \
                 capacity/period to match the measured work is recommended below.",
                labels(&undersized)
            );
        } else {
            detail += "All threads on the path already run under deadline profiles; the remaining wait \
                       is CPU contention between deadline threads (check per-core utilization and \
                       affinity) or wake-up migration latency.";
        }
        findings.push(Finding::new(
            "scheduling",
            if sched >= SCHED_PRIMARY { "high" } else { "medium" },
            format!("{:.0}% of the tail is spent runnable, waiting for a CPU", sched * 100.0),
            detail,
            culprits,
            lessons.link("scheduling"),
        ));
    }

    // 3. Hardware and external waits. Same quarter-of-the-tail bar as the
    // scheduling finding, so the two external causes are judged alike.
    if hw >= 0.25 {
        let mut rows: Vec<(i64, (i64, i64))> =
            res.hw_waits.iter().map(|(&k, v)| (v.iter().sum::<i64>(), k)).collect();
        rows.sort_by(|a, b| b.cmp(a));
        rows.truncate(3);
        let desc = rows
            .iter()
            .map(|(_, (u, i))| {
                let what = if *i >= 0 { format!("irq {i}") } else { "a timer".to_string() };
                format!("{} waiting on {what}", tr.label(*u))
            })
            .collect::<Vec<_>>()
            .join("; ");
        findings.push(Finding::new(
            "hardware",
            "high",
            format!("{:.0}% of the tail is waiting for hardware (interrupt / timer)", hw * 100.0),
            format!(
                "No scheduler setting shortens a wait for the device. Look at the protocol: fewer \
                 transactions, hardware-assisted signalling (e.g. hardware chip-select instead of GPIO \
                 round trips), DMA, or a shorter timer. NEEDS HARDWARE-CONTEXT REVIEW: a change that looks \
                 safe in these tests can break features the test does not exercise (tap-to-wake). {desc}"
            ),
            rows.iter().map(|(_, (u, _))| *u).collect(),
            lessons.link("hardware"),
        ));
    }

    // 4. Own work and IPC hops
    let mut hop_rows: Vec<(usize, i64, &(i64, i64, String))> =
        res.hops.iter().map(|(k, v)| (v.len(), v.iter().sum::<i64>(), k)).collect();
    hop_rows.sort_by(|a, b| b.cmp(a));
    let n_att = res.attributions.len();
    let hops_per_job =
        res.hops.values().map(|v| v.len()).sum::<usize>() as f64 / n_att.max(1) as f64;
    let run_rows: Vec<(i64, i64)> =
        thread_rank(res, &[Bucket::Run]).into_iter().filter(|&(v, _)| v != 0).collect();
    // Own work is the finding when it is more than half of the tail.
    if run >= 0.50 {
        let total_run: i64 = run_rows.iter().map(|&(x, _)| x).sum();
        let top: Vec<(i64, i64)> = run_rows.iter().take(OWN_WORK_THREADS).copied().collect();
        let desc = top
            .iter()
            .map(|&(v, u)| {
                format!(
                    "{} ({:.0}% of run)",
                    tr.label(u),
                    100.0 * v as f64 / total_run.max(1) as f64
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut var = String::new();
        let runs = &res.bucket_per_job[&Bucket::Run];
        if !runs.is_empty() && stats::quantile_i64(runs, 0.5) > 0.0 {
            let ratio = stats::quantile_i64(runs, 0.99) / stats::quantile_i64(runs, 0.5);
            // Input and cache effects spread one code path by less than 3x (the
            // recorded touch traces: 1.4-1.9x). Beyond that, some jobs take a different,
            // slower path (lesson 1.2).
            if ratio > 3.0 {
                var = format!(
                    " Own work varies {ratio:.1}x between p50 and p99: input/state-dependent paths \
                     (lesson 1.2) -- find what the slow jobs do differently."
                );
            }
        }
        findings.push(Finding::new(
            "own_work",
            if sched < 0.25 { "high" } else { "medium" },
            format!("{:.0}% of the tail is the chain's own instructions", run * 100.0),
            format!(
                "A deadline profile cannot make instructions faster; it can only stop them waiting. \
                 Biggest contributors: {desc}.{var}"
            ),
            top.iter().map(|&(_, u)| u).collect(),
            lessons.link("own_work"),
        ));
    }
    // Each hop adds a wake and a context switch and hides part of the critical
    // path (lesson 6.3). More than one request/reply round trip (2) is worth
    // noting, and twice that (4) is worth restructuring.
    if hops_per_job >= 2.0 && !hop_rows.is_empty() {
        let desc = hop_rows
            .iter()
            .take(4)
            .map(|(n, s, (f, t, m))| {
                format!(
                    "{} -> {} via {} x{:.1}/job, {:.0}us each",
                    tr.label(*f),
                    tr.label(*t),
                    if m.is_empty() { "?" } else { m.as_str() },
                    *n as f64 / n_att as f64,
                    *s as f64 / (*n).max(1) as f64 / NS_PER_US_F64
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        findings.push(Finding::new(
            "hops",
            if hops_per_job >= 4.0 { "medium" } else { "low" },
            format!("{hops_per_job:.1} blocking hops per job on the critical path"),
            format!(
                "Each hop costs a wake-up and context switch on both sides and hides the critical path \
                 from the scheduler (lesson 6.3). Candidates to remove or colocate: {desc}"
            ),
            vec![],
            lessons.link("hops"),
        ));
    }

    // If more than a fifth of tail time cannot be attributed, the other
    // findings rest on too little of the tail to trust.
    if unknown >= 0.2 {
        findings.push(Finding::new(
            "info",
            "info",
            format!("{:.0}% of the tail could not be attributed", unknown * 100.0),
            "Blocked segments whose waker is outside the trace or beyond the recursion limit. \
             Increase --max-depth or record a larger trace buffer."
                .into(),
            vec![],
            String::new(),
        ));
    }

    // The primary verdict.
    let order: Vec<&str> = findings
        .iter()
        .map(|f| f.kind.as_str())
        .filter(|k| ["throttled", "scheduling", "hardware", "own_work"].contains(k))
        .collect();
    let mut primary = if order.contains(&"throttled") {
        "throttled"
    } else if order.contains(&"scheduling") && (sched >= run || sched >= SCHED_PRIMARY) {
        "scheduling"
    } else if order.contains(&"hardware") && hw >= run {
        "hardware"
    } else if order.contains(&"own_work") {
        "own_work"
    } else if order.contains(&"scheduling") {
        "scheduling"
    } else if order.contains(&"hardware") {
        "hardware"
    } else {
        "fine"
    };

    if let Some(w) = crate::derive::active_window(window_ns) {
        if resp.p99.unwrap_or(0.0) <= w as f64 && (primary == "fine" || primary == "own_work") {
            findings.insert(
                0,
                Finding::new(
                    "fine",
                    "info",
                    format!(
                        "p99 response {:.0}us is inside the {:.0}us window",
                        resp.p99() / NS_PER_US_F64,
                        w as f64 / NS_PER_US_F64
                    ),
                    "The task meets its window at the tail; no change is needed.".into(),
                    vec![],
                    String::new(),
                ),
            );
            if primary == "own_work" {
                primary = "fine";
            }
        }
    }
    if let Some(w) = crate::derive::active_window(window_ns) {
        let buckets =
            [("throttled", thr), ("scheduling", sched), ("hardware", hw), ("own_work", run)];
        primary = note_window_miss(&mut findings, primary, resp.p99.unwrap_or(0.0), w, &buckets);
    }

    let mut recommend = primary == "throttled" || primary == "scheduling";
    let profile_utids = if recommend { culprit_utids(&findings, primary, res) } else { vec![] };
    let mut no_change = None;
    if recommend && !proposals.iter().any(|p| profile_utids.contains(&p.utid) && p.changed()) {
        recommend = false;
        let why = why_no_change(proposals, &profile_utids);
        findings.push(why.finding(lessons));
        no_change = Some(why);
    }

    let headline = match no_change {
        Some(why) => why.headline().to_string(),
        None => headline(primary).to_string(),
    };
    Verdict {
        primary: primary.to_string(),
        headline,
        findings,
        recommend_profiles: recommend,
        profile_utids,
        tail_q,
        shares_tail: shares,
        lessons_url: lessons.url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::ThreadBreakdown;
    use crate::attribution::Segment;

    /// A 100 us job with `wait` ns preempted and the rest run.
    fn job(wait: i64) -> Attribution {
        let seg = |t0: i64, t1: i64, bucket: Bucket| Segment {
            utid: 1,
            t0,
            t1,
            bucket,
            depth: 0,
            detail: None,
            cpu: 0,
        };
        Attribution {
            utid: 1,
            t0: 0,
            t1: 100_000,
            segments: vec![seg(0, wait, Bucket::Preempted), seg(wait, 100_000, Bucket::Run)],
        }
    }

    #[test]
    fn an_improved_candidate_is_fine_only_without_recommendations() {
        let imp = vec!["idle target p50: 1102 -> 379 us".to_string()];
        let (p, h, r) = after_improvement("own_work", headline("own_work"), false, false, &imp);
        assert_eq!((p.as_str(), r), ("fine", false));
        assert!(h.starts_with("Optimization succeeded vs. baseline (idle target p50"), "{h}");
        let (p, h, r) = after_improvement("throttled", headline("throttled"), true, true, &imp);
        assert_eq!((p.as_str(), r), ("throttled", true));
        assert_eq!(
            h,
            format!(
                "Improved vs. baseline (idle target p50: 1102 -> 379 us), but still throttled: {}",
                headline("throttled")
            )
        );
        assert!(!h.contains('\u{2014}'));
    }

    #[test]
    fn scheduling_share_uses_its_lower_confidence_bound() {
        let sched = [Bucket::Preempted, Bucket::WakeLatency];
        // Every tail job waits 30%: the bound is 30% and clears 25%.
        let steady: Vec<Attribution> = (0..150).map(|_| job(30_000)).collect();
        let idx: Vec<usize> = (0..150).collect();
        assert!((share_lower_bound(&steady, &idx, &sched) - 0.3).abs() < 1e-9);
        // Same 26% mean, but carried by a few jobs: the bound falls below 25%.
        let noisy: Vec<Attribution> =
            (0..150).map(|i| job(if i % 10 == 0 { 98_000 } else { 18_000 })).collect();
        let lo = share_lower_bound(&noisy, &idx, &sched);
        assert!(lo < 0.25, "{lo}");
    }

    #[test]
    fn a_tail_over_the_window_is_never_fine() {
        let buckets =
            [("throttled", 0.05), ("scheduling", 0.2), ("hardware", 0.1), ("own_work", 0.6)];
        let mut findings = Vec::new();
        assert_eq!(note_window_miss(&mut findings, "fine", 800_000.0, 850_000, &buckets), "fine");
        assert!(findings.is_empty());
        assert_eq!(
            note_window_miss(&mut findings, "fine", 900_000.0, 850_000, &buckets),
            "own_work"
        );
        assert_eq!((findings[0].kind.as_str(), findings[0].severity.as_str()), ("window", "high"));
        assert_eq!(findings[0].title, "p99 response 900us misses the 850us window");
        let mut more = Vec::new();
        assert_eq!(
            note_window_miss(&mut more, "throttled", 900_000.0, 850_000, &buckets),
            "throttled"
        );
        assert_eq!(more.len(), 1);
    }

    /// An analysis whose threads each spent `ns` in one bucket.
    fn result(threads: &[(i64, Bucket, i64)]) -> AnalysisResult {
        let threads = threads
            .iter()
            .map(|&(u, b, ns)| {
                let t = ThreadBreakdown {
                    utid: u,
                    label: format!("p/t[{u}]"),
                    per_job: [(b, vec![ns])].into_iter().collect(),
                    deadline_params: None,
                    fair_weight: None,
                    cpus: IndexMap::new(),
                };
                (u, t)
            })
            .collect();
        AnalysisResult {
            spec: crate::tasks::parse("thread:p/t").unwrap(),
            trace_path: String::new(),
            jobs: vec![],
            attributions: vec![],
            bucket_per_job: IndexMap::new(),
            threads,
            response_ns: vec![],
            inter_arrival_ns: vec![],
            has_rates: true,
            notes: vec![],
            hops: IndexMap::new(),
            hw_waits: IndexMap::new(),
        }
    }

    fn proposal(
        utid: i64,
        period_ns: i64,
        capacity_ns: i64,
        existing: Option<(i64, i64)>,
    ) -> ProfileProposal {
        ProfileProposal {
            utid,
            label: format!("p/t[{utid}]"),
            period_ns,
            capacity_ns,
            affinity: vec![],
            utilization: 0.0,
            work_per_period_ns: IndexMap::new(),
            existing,
            rate_source: String::new(),
            notes: vec![],
            work_ns: vec![],
        }
    }

    #[test]
    fn a_window_miss_without_findings_still_names_the_waiting_threads() {
        let res = result(&[
            (1, Bucket::Run, 90_000),
            (2, Bucket::Preempted, 40_000),
            (3, Bucket::Throttled, 20_000),
        ]);
        // No throttled or scheduling finding: the top threads of the verdict's
        // buckets, never one with no time in them.
        assert_eq!(culprit_utids(&[], "scheduling", &res), vec![2, 3]);
        assert_eq!(culprit_utids(&[], "throttled", &res), vec![3]);
        let f = Finding::new(
            "scheduling",
            "high",
            String::new(),
            String::new(),
            vec![1],
            String::new(),
        );
        assert_eq!(culprit_utids(&[f], "scheduling", &res), vec![1]);
        // With no profile on those threads, a proposal is a change to apply.
        assert!(proposal(2, 1_000_000, 100_000, None).changed());
    }

    #[test]
    fn placeholder_proposals_are_not_recommended() {
        // No period could be determined: 0/0 placeholders are no change.
        let placeholder = proposal(1, 0, 0, None);
        assert!(!placeholder.changed());
        assert_eq!(why_no_change(&[placeholder.clone()], &[1]), NoChange::NoPeriod);
        let h = NoChange::NoPeriod.headline();
        assert!(h.contains("no period could be determined"), "{h}");
        // An adequate profile in effect: contention, as before.
        let adequate = proposal(1, 1_000_000, 100_000, Some((100_000, 1_000_000)));
        assert!(!adequate.changed());
        assert_eq!(why_no_change(&[adequate], &[1]), NoChange::Contention);
        assert_eq!(why_no_change(&[placeholder], &[2]), NoChange::Contention);
    }
}
