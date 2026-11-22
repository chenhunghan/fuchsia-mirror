// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Report rendering (text and markdown share one structure).

use crate::analysis::AnalysisResult;
use crate::attribution::{Bucket, LEAF_BUCKETS};
use crate::derive::ProfileProposal;
use crate::model::Trace;
use crate::profiles_file::{NS_PER_US, NS_PER_US_F64};
use crate::stats;
use crate::verdict::Verdict;

/// Microseconds with one decimal and comma thousands separators.
fn us(ns: f64) -> String {
    let s = format!("{:.1}", ns / NS_PER_US_F64);
    let (sign, digits) = s.strip_prefix('-').map_or(("", s.as_str()), |d| ("-", d));
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}{grouped}.{frac}")
}

fn meaning(b: Bucket) -> &'static str {
    match b {
        Bucket::Run => "own instructions -> optimize code",
        Bucket::Preempted => "runnable, CPU taken by another thread -> profile",
        Bucket::WakeLatency => "woken, waiting for a CPU -> profile / affinity",
        Bucket::Throttled => "deadline budget exhausted -> raise capacity",
        Bucket::HwWait => "waiting for interrupt/timer -> HW/protocol",
        Bucket::BlockedUnknown => "unattributed",
        Bucket::BlockedOn => "",
    }
}

/// Renders the report as text, or markdown when `md`.
pub fn render(
    res: &AnalysisResult,
    tr: &Trace,
    vd: &Verdict,
    proposals: &[ProfileProposal],
    md: bool,
    verbose: bool,
) -> String {
    let mut l: Vec<String> = Vec::new();
    let h = |s: &str| if md { format!("## {s}") } else { format!("\n== {s} ==") };
    let b = |s: &str| if md { format!("**{s}**") } else { s.to_string() };
    let code = |s: &str| if md { format!("`{s}`") } else { s.to_string() };

    l.push(if md {
        format!("# zpo report: {}", res.spec.label())
    } else {
        format!("zpo report: {}", res.spec.label())
    });
    l.push(format!("trace: {}", code(&res.trace_path)));
    l.push(format!(
        "jobs: {}; anchor thread(s): {}",
        res.jobs.len(),
        res.anchor_utids().iter().map(|&u| tr.label(u)).collect::<Vec<_>>().join(", ")
    ));
    for n in &res.notes {
        l.push(format!("note: {n}"));
    }

    l.push(h("Verdict"));
    l.push(b(&vd.headline));
    if !vd.lessons_url.is_empty() {
        l.push(format!("Why this verdict? {}", vd.lessons_url));
    }
    for f in &vd.findings {
        l.push(String::new());
        l.push(format!("- [{}] {}", f.severity, b(&f.title)));
        l.push(format!("  {}", f.detail));
        if !f.utids.is_empty() {
            l.push(format!(
                "  threads: {}",
                f.utids.iter().map(|&u| tr.label(u)).collect::<Vec<_>>().join(", ")
            ));
        }
        if !f.lesson.is_empty() {
            l.push(format!("  lesson: {}", f.lesson));
        }
    }

    let s = stats::summary_i64(&res.response_ns);
    l.push(h("Measured response time (event -> end)"));
    l.push(format!(
        "p50 {} us  p90 {} us  p99 {} us  max {} us  (n={})",
        us(s.p50()),
        us(s.p90()),
        us(s.p99()),
        us(s.max()),
        s.n
    ));
    if !res.inter_arrival_ns.is_empty() {
        let ia = &res.inter_arrival_ns;
        l.push(format!(
            "inter-arrival: p1 {} us  p10 {} us  p50 {} us",
            us(stats::quantile_i64(ia, 0.01)),
            us(stats::quantile_i64(ia, 0.10)),
            us(stats::quantile_i64(ia, 0.5))
        ));
    }

    l.push(h(&format!(
        "Where the time goes (share of the p{} tail; per-job quantiles in us)",
        (vd.tail_q * 100.0) as i64
    )));
    if md {
        l.push("| bucket | tail share | p50 | p90 | p99 | meaning |".into());
        l.push("|---|---:|---:|---:|---:|---|".into());
    }
    for bkt in LEAF_BUCKETS {
        let xs = &res.bucket_per_job[&bkt];
        if !xs.iter().any(|&x| x != 0) {
            continue;
        }
        let q = stats::summary_i64(xs);
        let share = vd.shares_tail.get(&bkt).copied().unwrap_or(0.0) * 100.0;
        if md {
            l.push(format!(
                "| {} | {share:.1}% | {} | {} | {} | {} |",
                bkt.as_str(),
                us(q.p50()),
                us(q.p90()),
                us(q.p99()),
                meaning(bkt)
            ));
        } else {
            l.push(format!(
                "  {:<16} {share:5.1}%  p50 {:>9}  p90 {:>9}  p99 {:>9}   {}",
                bkt.as_str(),
                us(q.p50()),
                us(q.p90()),
                us(q.p99()),
                meaning(bkt)
            ));
        }
    }

    l.push(h("Threads on the critical path"));
    let total = res.attributions.iter().map(|a| a.total()).sum::<i64>();
    let total = if total == 0 { 1 } else { total } as f64;
    let rows = res.threads_by_total();
    if md {
        l.push("| thread | share | profile in effect | run | preempted | wake | throttled | hw | cpus |".into());
        l.push("|---|---:|---|---:|---:|---:|---:|---:|---|".into());
    }
    for &(tot, u) in rows.iter().take(if verbose { 60 } else { 15 }) {
        let t = &res.threads[&u];
        let prof = match (t.deadline_params, t.fair_weight) {
            (Some((c, p)), _) => {
                format!("deadline {}us/{}us", c.div_euclid(NS_PER_US), p.div_euclid(NS_PER_US))
            }
            (None, Some(w)) if w != 0 => format!("fair w={w}"),
            _ => "none seen".into(),
        };
        let mut cpus: Vec<(i64, i64)> = t.cpus.iter().map(|(&c, &ns)| (c, ns)).collect();
        cpus.sort_by_key(|&(_, ns)| -ns);
        let cpus = cpus.iter().take(5).map(|(c, _)| c.to_string()).collect::<Vec<_>>().join(",");
        let pct = |bk: Bucket| format!("{:.1}%", 100.0 * t.total(bk) as f64 / total);
        let share = 100.0 * tot as f64 / total;
        if md {
            l.push(format!(
                "| {} | {share:.1}% | {prof} | {} | {} | {} | {} | {} | {cpus} |",
                t.label,
                pct(Bucket::Run),
                pct(Bucket::Preempted),
                pct(Bucket::WakeLatency),
                pct(Bucket::Throttled),
                pct(Bucket::HwWait)
            ));
        } else {
            l.push(format!("  {:<60.60} {share:5.1}%  [{prof}] cpus {cpus}", t.label));
            l.push(format!(
                "      run {}  preempted {}  wake {}  throttled {}  hw {}",
                pct(Bucket::Run),
                pct(Bucket::Preempted),
                pct(Bucket::WakeLatency),
                pct(Bucket::Throttled),
                pct(Bucket::HwWait)
            ));
        }
    }

    if !res.hops.is_empty() {
        l.push(h("Blocking IPC hops on the critical path"));
        let mut hop_rows: Vec<(usize, i64, &(i64, i64, String))> =
            res.hops.iter().map(|(k, v)| (v.len(), v.iter().sum::<i64>(), k)).collect();
        hop_rows.sort_by(|a, b| b.cmp(a));
        for (n, ssum, (f, t, m)) in hop_rows.iter().take(10) {
            l.push(format!(
                "- {} -> {} via {}: {:.2}/job, {:.1} us each",
                tr.label(*f),
                tr.label(*t),
                if m.is_empty() { "?" } else { m.as_str() },
                *n as f64 / res.attributions.len() as f64,
                *ssum as f64 / (*n).max(1) as f64 / NS_PER_US_F64
            ));
        }
    }

    l.push(h("Deadline profile derivation"));
    if vd.recommend_profiles {
        l.push(b("Recommended: apply the profile(s) below and verify with `zpo measure`."));
    } else {
        l.push(b(
            "Not recommended yet: the verdict says the work itself should change first. \
             The numbers below show what the profile WOULD be for the work as measured; re-run after the fix.",
        ));
    }
    for p in proposals {
        let tag = if vd.profile_utids.contains(&p.utid) || !vd.recommend_profiles {
            ""
        } else {
            " (not on the culprit list)"
        };
        l.push(String::new());
        l.push(format!("- {}{tag}", b(&p.label)));
        if p.period_ns <= 0 {
            l.push(format!("  no period: {}", p.notes.join("; ")));
            continue;
        }
        let ex = match p.existing {
            Some((c, pe)) => {
                format!("{}us/{}us", c.div_euclid(NS_PER_US), pe.div_euclid(NS_PER_US))
            }
            None => "none".into(),
        };
        let w = |k: &str| p.work_per_period_ns.get(k).copied().unwrap_or(0.0);
        l.push(format!(
            "  work per {}us period (normalized): p50 {}  p90 {}  p99 {}  max {} us   [rates: {}]",
            p.period_ns.div_euclid(NS_PER_US),
            us(w("p50")),
            us(w("p90")),
            us(w("p99")),
            us(w("max")),
            p.rate_source
        ));
        l.push(format!(
            "  proposal: {}   U={:.2}   current: {ex}{}",
            code(&p.profiles_json()),
            p.utilization,
            if p.changed() { "" } else { "   (unchanged)" }
        ));
        for n in &p.notes {
            l.push(format!("  - {n}"));
        }
    }
    l.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microseconds_have_thousands_separators() {
        assert_eq!(us(1_234_567_891.0), "1,234,567.9");
        assert_eq!(us(-1_234_500.0), "-1,234.5");
        assert_eq!(us(999_000.0), "999.0");
        assert_eq!(us(40.0), "0.0");
    }
}
