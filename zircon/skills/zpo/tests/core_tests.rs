// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Core analysis tests (attribution, derive, stats) on hand-built traces.

use zpo::attribution::{Attributor, Bucket};
use zpo::derive::{RateModel, normalized_work_per_period};
use zpo::model::StateKind::{Blocked, Preempted, Running, Woken};
use zpo::model::{Irq, SchedSlice, SchedulingParams, State, StateKind, ThreadInfo, Trace};
use zpo::profiles_file::{read_role, set_role};
use zpo::stats;
use zpo::tasks::Job;
use zpo::topology::{Domain, Topology};

/// A sample device's CPUs: little (0-1), a mid domain (2-6) and big (7).
fn topology() -> Topology {
    let d = |id, cpus: &[i64], max_rate| Domain {
        id,
        cpus: cpus.to_vec(),
        core_count: cpus.len(),
        max_rate,
    };
    Topology {
        domains: vec![d(0, &[0, 1], 0.202), d(1, &[2, 3, 4, 5, 6], 0.805), d(2, &[7], 1.0)],
        roles: Default::default(),
    }
}

fn st(ts: i64, dur: i64, state: StateKind, cpu: i64, waker: Option<i64>) -> State {
    State { ts, dur, state, cpu, waker_utid: waker }
}

/// A sched slice whose parameters are given as the trace's `sched.priority`.
fn sl(ts: i64, dur: i64, cpu: i64, sched_priority: i64) -> SchedSlice {
    SchedSlice {
        ts,
        dur,
        cpu,
        scheduling_params: SchedulingParams::from_sched_priority(sched_priority),
    }
}

fn ti(utid: i64, tid: i64, name: &str, pid: i64, process: &str) -> ThreadInfo {
    ThreadInfo { utid, tid, name: name.into(), pid, process: process.into() }
}

/// Two threads: A blocks on B, B blocks on an IRQ wake. CPU 0 only.
fn mk_trace() -> Trace {
    let mut tr = Trace::new();
    tr.threads.insert(1, ti(1, 11, "A", 100, "procA"));
    tr.threads.insert(2, ti(2, 22, "B", 200, "procB"));
    tr.threads.insert(9, ti(9, 99, "noise", 900, "loadgen"));
    // A runs 0-10us, blocks 10-40 (woken by B at 40), W 40-42, runs 42-50, S after.
    tr.set_states(
        1,
        vec![
            st(0, 10_000, Running, 0, None),
            st(10_000, 30_000, Blocked, 0, None),
            st(40_000, 2_000, Woken, 0, Some(2)),
            st(42_000, 8_000, Running, 0, None),
            st(50_000, 1_000_000, Blocked, 0, None),
        ],
    );
    // B: blocked until an IRQ at 20 (waker reported = noise thread 9, running
    // on cpu0), W 20-25, R 25-30 (preempted), runs 30-40 (wakes A), blocks.
    tr.set_states(
        2,
        vec![
            st(0, 20_000, Blocked, 0, None),
            st(20_000, 5_000, Woken, 0, Some(9)),
            st(25_000, 5_000, Preempted, 0, None),
            st(30_000, 10_000, Running, 0, None),
            st(40_000, 1_000_000, Blocked, 0, None),
        ],
    );
    tr.set_states(9, vec![st(0, 2_000_000, Running, 0, None)]);
    tr.set_sched(9, vec![sl(0, 2_000_000, 0, 10)]);
    tr.set_sched(1, vec![sl(0, 10_000, 0, -150085), sl(42_000, 8_000, 0, -150085)]);
    tr.set_sched(2, vec![sl(30_000, 10_000, 0, 10)]);
    tr.irqs.insert(0, vec![Irq { ts: 19_000, dur: 3_000, cpu: 0, irq: 716 }]);
    tr.finalize();
    tr
}

#[test]
fn buckets_sum_to_window() {
    let tr = mk_trace();
    let a = Attributor::new(&tr, 6, None).attribute(1, 0, 50_000);
    let bb = a.by_bucket();
    assert_eq!(bb.values().sum::<i64>(), 50_000);
    assert_eq!(bb[&Bucket::Run], 10_000 + 8_000 + 10_000); // A's runs + B's run
    assert_eq!(bb[&Bucket::WakeLatency], 2_000 + 5_000);
    assert_eq!(bb[&Bucket::Preempted], 5_000);
    assert_eq!(bb[&Bucket::HwWait], 10_000); // B blocked 10..20 inside the window
    assert!(!bb.contains_key(&Bucket::BlockedUnknown));
}

#[test]
fn irq_wake_detection() {
    let tr = mk_trace();
    let a = Attributor::new(&tr, 6, None).attribute(1, 0, 50_000);
    let hw: Vec<_> = a.segments.iter().filter(|s| s.bucket == Bucket::HwWait).collect();
    assert_eq!(hw.len(), 1);
    assert_eq!(hw[0].detail, Some(716));
    assert_eq!(hw[0].utid, 2);
}

#[test]
fn chain() {
    let tr = mk_trace();
    assert_eq!(Attributor::new(&tr, 6, None).attribute(1, 0, 50_000).chain(), vec![1, 2]);
}

#[test]
fn deadline_params_unpack() {
    assert_eq!(sl(0, 1, 0, -150085).deadline_params(), Some((150_000, 850_000)));
    assert_eq!(sl(0, 1, 0, 12).deadline_params(), None);
}

fn renderengine() -> Trace {
    let mut tr = Trace::new();
    tr.threads.insert(1, ti(1, 11, "renderengine", 100, "sf"));
    tr
}

#[test]
fn throttled_after_long_sleep() {
    // 4ms / 15ms deadline profile (-4001500). Exhausts 4ms at [0, 4ms], sleeps
    // 9.1ms (> 5ms) at [4ms, 13.1ms], wakes at 13.1ms and is throttled until
    // 15ms when the period replenishes.
    let bb = sleeper(-4001500, 4_000_000, 9_100_000, 1_900_000);
    assert_eq!(bb.get(&Bucket::Throttled).copied().unwrap_or(0), 1_900_000);
    assert_eq!(bb.get(&Bucket::WakeLatency).copied().unwrap_or(0), 0);
}

/// A deadline thread that runs, sleeps, then waits runnable before running 1ms.
fn sleeper(
    prio: i64,
    run_ns: i64,
    sleep_ns: i64,
    runnable_ns: i64,
) -> indexmap::IndexMap<Bucket, i64> {
    let mut tr = renderengine();
    let wake = run_ns + sleep_ns;
    tr.set_states(
        1,
        vec![
            st(0, run_ns, Running, 7, None),
            st(run_ns, sleep_ns, Blocked, -1, None),
            st(wake, runnable_ns, Woken, 7, Some(2)),
            st(wake + runnable_ns, 1_000_000, Running, 7, None),
        ],
    );
    tr.set_sched(1, vec![sl(0, run_ns, 7, prio), sl(wake + runnable_ns, 1_000_000, 7, prio)]);
    tr.finalize();
    Attributor::new(&tr, 6, None).attribute(1, wake, wake + runnable_ns + 1_000_000).by_bucket()
}

#[test]
fn throttled_after_sleep_longer_than_50ms() {
    // 20ms / 99ms profile: uses its 20ms, sleeps 60ms, wakes at 80ms inside the
    // same period and waits until 99ms. The profile is read from the slice
    // before the sleep however long ago it ended.
    let bb = sleeper(-20_009_900, 20_000_000, 60_000_000, 19_000_000);
    assert_eq!(bb.get(&Bucket::Throttled).copied().unwrap_or(0), 19_000_000);
}

#[test]
fn not_throttled_after_sleep_longer_than_period() {
    // 4ms / 15ms profile: uses its 4ms, then sleeps 20ms, past the period's
    // end, so it wakes with a fresh budget and any wait is wake latency.
    let bb = sleeper(-4001500, 4_000_000, 20_000_000, 500_000);
    assert_eq!(bb.get(&Bucket::Throttled).copied().unwrap_or(0), 0);
    assert_eq!(bb.get(&Bucket::WakeLatency).copied().unwrap_or(0), 500_000);
}

/// The work per period of a 4ms / 15ms deadline thread: a prior job ran 3ms
/// at [2ms, 5ms]; the current job starts at 10ms, runs 1ms, is stalled
/// (preempted) for `stall_ns`, then runs 2.5ms.
fn work_for(stall_ns: i64) -> f64 {
    let prio = -4001500;
    let resume = 11_000_000 + stall_ns;
    let job = Job { utid: 1, t0: 10_000_000, t1: resume + 2_500_000, event_ts: 10_000_000 };
    let mut tr = renderengine();
    tr.set_states(
        1,
        vec![
            st(2_000_000, 3_000_000, Running, 7, None),
            st(5_000_000, 5_000_000, Blocked, -1, None),
            st(10_000_000, 1_000_000, Running, 7, None),
            st(11_000_000, stall_ns, Preempted, 7, None),
            st(resume, 2_500_000, Running, 7, None),
        ],
    );
    tr.set_sched(
        1,
        vec![
            sl(2_000_000, 3_000_000, 7, prio),
            sl(10_000_000, 1_000_000, 7, prio),
            sl(resume, 2_500_000, 7, prio),
        ],
    );
    tr.finalize();
    let at = Attributor::new(&tr, 6, None).attribute(1, job.t0, job.t1);
    let t = topology();
    let rates = RateModel::new(&tr, &t);
    normalized_work_per_period(
        &tr,
        &rates,
        1,
        &[job.event_ts],
        15_000_000,
        Some(&[job]),
        Some(&[at]),
    )[0]
}

#[test]
fn normalized_work_per_period_throttled_mid_job() {
    // The job hits its 4ms budget at 11ms and is throttled for 6ms at
    // [11ms, 17ms] (period [2ms, 17ms]), then runs 2.5ms at [17ms, 19.5ms].
    let w = work_for(6_000_000);
    // Active period [2ms, 17ms] with the [11ms, 17ms] stall removed has 3ms +
    // 1ms + 2.5ms = 6.5ms of work on big core cpu 7 (rate 1.0).
    assert!((w - 6_500_000.0).abs() < 1e-6, "{w}");
}

#[test]
fn normalized_work_per_period_busy_cpu_after_replenish() {
    // Same, but another thread holds the CPU for 1ms after the budget
    // replenishes at 17ms, so the stall ends at 18ms. The inferred period starts
    // at 3ms and misses 1ms of the prior job: 5.5ms, an undercount of the true
    // 6.5ms but above the 3.5ms the other estimates give.
    let w = work_for(7_000_000);
    assert!((w - 5_500_000.0).abs() < 1e-6, "{w}");
}

#[test]
fn quantiles() {
    let xs: Vec<f64> = (1..=100).map(|x| x as f64).collect();
    assert!((stats::quantile(&xs, 0.5) - 50.5).abs() < 1e-9);
    assert!((stats::quantile(&xs, 0.99) - 99.01).abs() < 1e-9);
}

#[test]
fn improved_regressed() {
    let base: Vec<f64> = (0..200).map(|i| (100 + i % 7) as f64).collect();
    let better: Vec<f64> = (0..200).map(|i| (60 + i % 7) as f64).collect();
    assert!(stats::improved(&base, &better, 0.9, 0.05));
    assert!(!stats::regressed(&base, &better, 0.9, 0.05));
    assert!(stats::regressed(&better, &base, 0.9, 0.05));
    assert!(!stats::improved(&base, &base, 0.9, 0.05));
}

#[test]
fn compensated_sum_is_exact_where_naive_sums_drift() {
    assert_eq!(stats::compensated_sum(&[0.1; 10]), 1.0);
    assert_eq!(stats::compensated_sum(&[1e100, 1.0, -1e100]), 1.0);
}

const PROFILES: &str = r#"{
  "scope": "board",
  "profiles": {
    // keep me
    "a.role": { "capacity": "100us", "period": "1ms", "affinity": [2, 3] },
    "b.role": { "priority": 20, "affinity": [7] }
  },
  "memory": { "x": { "priority": 24 } },
}
"#;

#[test]
fn update_keeps_comment() {
    let (new, act) = set_role(PROFILES, "a.role", 150, 850, &[2, 3, 4], "").unwrap();
    assert_eq!(act, "updated");
    assert!(new.contains("// keep me"));
    assert!(new.contains(
        r#""a.role": { "capacity": "150us", "period": "850us", "affinity": [2, 3, 4] },"#
    ));
    assert_eq!(read_role(&new, "a.role").unwrap().capacity.as_deref(), Some("150us"));
}

#[test]
fn add_appends_with_comma() {
    let (new, act) = set_role(PROFILES, "c.role", 10, 100, &[0], "").unwrap();
    assert_eq!(act, "added");
    assert!(new.contains(r#""b.role": { "priority": 20, "affinity": [7] },"#));
    assert!(
        new.contains(r#""c.role": { "capacity": "10us", "period": "100us", "affinity": [0] },"#)
    );
    // Still parses as JSON once comments and trailing commas are stripped.
    let j = regex_lite::Regex::new(r"//.*").unwrap().replace_all(&new, "").into_owned();
    let j = regex_lite::Regex::new(r",(\s*[}\]])").unwrap().replace_all(&j, "$1").into_owned();
    let d: serde_json::Value = serde_json::from_str(&j).unwrap();
    assert!(d["profiles"].get("c.role").is_some());
}
