// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! End-to-end tests against the real `trace_processor_shell`, on a small
//! trace written by [`fxt_writer`] with hand-chosen values, so every expected
//! number below can be worked out by hand.
//!
//! These catch Perfetto rolls that rename tables or columns, change the SQL
//! standard library, change how FXT is imported, or change the shell's
//! command line output.

mod core_tests;
mod fxt_writer;

use fxt_writer::{BLOCKED, IDLE, Writer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zpo::attribution::Bucket;
use zpo::proc::HostRunner;
use zpo::tp::queries;
use zpo::tp::session::Sessions;

const DRV: u64 = 100;
const APP: u64 = 200;
const KERNEL: u64 = 1;
const IRQ_T: u64 = 101; // drv/irq-thread, deadline profile
const APP_T: u64 = 201; // app/main, fair
const CPU2: u64 = 11; // kernel/cpu-2, where IRQ slices go
const JOBS: u64 = 25;
const PERIOD: u64 = 1_000_000;
/// 50 us capacity / 1 ms period, packed -CCCCDDDD in 10 us units.
const DL: i32 = -(5 * 10_000 + 100);

/// One job per millisecond: IRQ 42 at +10us (handler until +15) wakes the irq
/// thread (+12), which runs from +13, sends Echo.Ping to app at +18 and wakes
/// it at +19; app runs from +20 and emits "Present" at +29. The app's wake
/// is after the handler ends, so it is a thread wake rather than an interrupt
/// wake.
fn fixture() -> Vec<u8> {
    fixture_with_ipc_name("Echo.Ping")
}

/// [`fixture`] with every kernel:ipc slice and flow named `ipc` (e.g.
/// "ChannelMessage", as in a trace `ffx trace` could not symbolize).
fn fixture_with_ipc_name(ipc: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.process(KERNEL, "kernel");
    w.process(DRV, "drv");
    w.process(APP, "app");
    w.thread(CPU2, KERNEL, "cpu-2");
    w.thread(IRQ_T, DRV, "irq-thread");
    w.thread(APP_T, APP, "main");
    // Both threads run once and block, so they have a state before job 0.
    w.switch(0, 2, 0, IDLE, BLOCKED, IRQ_T, DL);
    w.switch(100, 2, IRQ_T, DL, BLOCKED, 0, IDLE);
    w.switch(0, 3, 0, IDLE, BLOCKED, APP_T, 20);
    w.switch(50, 3, APP_T, 20, BLOCKED, 0, IDLE);
    w.counter(0, KERNEL, CPU2, "kernel:power", "Rate", "CPU", 2, 871);
    w.counter(0, KERNEL, CPU2, "kernel:power", "Rate", "CPU", 3, 1000);
    for k in 0..JOBS {
        let b = 10_000 + k * PERIOD;
        w.complete(
            b,
            b + 5000,
            KERNEL,
            CPU2,
            "kernel:irq",
            "irq",
            &[fxt_writer::Arg::I32("irq", 42)],
        );
        w.wakeup(b + 2000, 2, IRQ_T, DL, None);
        w.switch(b + 3000, 2, 0, IDLE, BLOCKED, IRQ_T, DL);
        w.complete(b + 8000, b + 8100, DRV, IRQ_T, "kernel:ipc", ipc, &[]);
        w.flow_begin(b + 8050, DRV, IRQ_T, "kernel:ipc", ipc, k + 1);
        w.wakeup(b + 9000, 3, APP_T, 20, Some(IRQ_T));
        w.switch(b + 10000, 2, IRQ_T, DL, BLOCKED, 0, IDLE);
        w.switch(b + 10000, 3, 0, IDLE, BLOCKED, APP_T, 20);
        w.complete(b + 10000, b + 10100, APP, APP_T, "kernel:ipc", ipc, &[]);
        w.flow_end(b + 10050, APP, APP_T, "kernel:ipc", ipc, k + 1);
        w.complete(b + 19000, b + 19500, APP, APP_T, "gfx", "Present", &[]);
        w.switch(b + 20000, 3, APP_T, 20, BLOCKED, 0, IDLE);
    }
    w.instant(JOBS * PERIOD, APP, APP_T, "gfx", "end");
    w.bytes()
}

/// The prebuilt trace processor, staged as test data next to the test (or
/// $ZPO_TEST_TP when running the test binary directly).
fn tp() -> PathBuf {
    if let Ok(p) = std::env::var("ZPO_TEST_TP") {
        return PathBuf::from(p);
    }
    let host = if cfg!(target_arch = "aarch64") { "host_arm64" } else { "host_x64" };
    let p =
        std::path::absolute(Path::new(host).join("test_data/zpo/trace_processor_shell")).unwrap();
    assert!(p.exists(), "no trace processor at {} (set ZPO_TEST_TP)", p.display());
    p
}

struct Env {
    _dir: tempfile::TempDir,
    trace: PathBuf,
    sessions: Sessions,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.sessions.kill_started();
    }
}

fn env(chunk_rows: usize) -> Env {
    env_with(chunk_rows, fixture())
}

fn env_with(chunk_rows: usize, fxt: Vec<u8>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("zpo_mini.fxt");
    std::fs::write(&trace, fxt).unwrap();
    let sessions = Sessions::new(Arc::new(HostRunner), tp()).with_chunk_rows(chunk_rows);
    Env { _dir: dir, trace, sessions }
}

#[test]
fn fixture_is_deterministic() {
    assert_eq!(fixture(), fixture());
    assert_eq!(&fixture()[..8], &0x0016_5478_4604_0010u64.to_le_bytes());
}

#[test]
fn every_query_runs() {
    let e = env(100_000);
    let client = e.sessions.open(&e.trace, &|_| {}).unwrap();
    for q in queries::all_for_test() {
        if let Err(err) = client.query(&q) {
            panic!("query {} failed: {err:#}", q.name);
        }
    }
}

#[test]
fn queries_return_the_hand_computed_rows() {
    let e = env(100_000);
    let c = e.sessions.open(&e.trace, &|_| {}).unwrap();
    let threads = c.query(&queries::threads()).unwrap();
    let utid_of = |tid: i64| {
        threads.iter().find(|r| r.get::<i64>(1) == tid).map(|r| r.get::<i64>(0)).unwrap()
    };
    let irq_u = utid_of(IRQ_T as i64);
    let app_u = utid_of(APP_T as i64);
    let names: Vec<(String, String)> = threads
        .iter()
        .filter(|r| r.get::<i64>(1) == APP_T as i64)
        .map(|r| (r.get::<String>(4), r.get::<String>(2)))
        .collect();
    assert_eq!(names, vec![("app".to_string(), "main".to_string())]);

    let irqs = c.query(&queries::irqs()).unwrap();
    assert_eq!(irqs.len(), JOBS as usize);
    let r = irqs.first().unwrap();
    assert_eq!(
        (r.get::<i64>(0), r.get::<i64>(1), r.get::<String>(2), r.get::<i64>(3)),
        (10_000, 5_000, "cpu-2".to_string(), 42)
    );

    let rates = c.query(&queries::rates()).unwrap();
    let rv: Vec<(String, f64)> =
        rates.iter().map(|r| (r.get::<String>(0), r.get::<f64>(2))).collect();
    assert_eq!(rv, vec![("Rate:CPU:2".to_string(), 871.0), ("Rate:CPU:3".to_string(), 1000.0)]);

    let sched = c.query(&queries::thread_sched(irq_u)).unwrap();
    assert_eq!(sched.len(), JOBS as usize + 1);
    let s1 = sched.iter().nth(1).unwrap();
    assert_eq!(
        (s1.get::<i64>(0), s1.get::<i64>(1), s1.get::<i64>(2), s1.get::<i64>(3)),
        (13_000, 7_000, 2, DL as i64)
    );

    let states = c.query(&queries::thread_states(app_u)).unwrap();
    // Running, S, then per job W, Running, S.
    let first: Vec<(i64, String, Option<i64>)> = states
        .iter()
        .take(5)
        .map(|r| (r.get::<i64>(0), r.get::<String>(2), r.get::<Option<i64>>(4)))
        .collect();
    assert_eq!(first[0].1, "Running");
    assert_eq!((first[1].0, first[1].1.as_str()), (50, "S"));
    assert_eq!((first[2].0, first[2].1.as_str(), first[2].2), (19_000, "W", Some(irq_u)));
    assert_eq!((first[3].0, first[3].1.as_str()), (20_000, "Running"));
    assert_eq!((first[4].0, first[4].1.as_str()), (30_000, "S"));

    let ipc = c.query(&queries::ipc_for_method("Echo.Ping")).unwrap();
    let out: Vec<(i64, i64, i64, Option<i64>, Option<i64>)> = ipc
        .iter()
        .filter(|r| r.get::<i64>(3) == 1)
        .map(|r| {
            (
                r.get::<i64>(0),
                r.get::<i64>(1),
                r.get::<i64>(3),
                r.get::<Option<i64>>(4),
                r.get::<Option<i64>>(5),
            )
        })
        .collect();
    assert_eq!(out.len(), JOBS as usize);
    assert_eq!(out[0], (irq_u, 18_000, 1, Some(app_u), Some(20_000)));

    let present = c.query(&queries::slices_named("Present")).unwrap();
    assert_eq!(present.len(), JOBS as usize);
    assert_eq!(
        (present.first().unwrap().get::<i64>(0), present.first().unwrap().get::<i64>(2)),
        (29_000, app_u)
    );

    // The irq thread has 26 deadline slices: a resident deadline thread.
    let dl = c.query(&queries::deadline_threads()).unwrap();
    let dl: Vec<(i64, i64)> = dl.iter().map(|r| (r.get::<i64>(0), r.get::<i64>(2))).collect();
    assert_eq!(dl, vec![(irq_u, DL as i64)]);
}

#[test]
fn chunks_keep_rows_and_order() {
    let big = env(100_000);
    let small = env(2);
    let q = queries::irqs();
    let a = big.sessions.open(&big.trace, &|_| {}).unwrap().query(&q).unwrap();
    let b = small
        .sessions
        .client(&big.sessions.open(&big.trace, &|_| {}).unwrap().session().to_string())
        .query(&q)
        .unwrap();
    assert_eq!(a.len(), JOBS as usize);
    assert_eq!(a, b);
}

#[test]
fn analysis_matches_the_hand_computed_attribution() {
    let e = env(100_000);
    let tr = zpo::trace_db::load(&e.sessions, &e.trace, &|_| {}).unwrap();
    assert!(tr.has_rates);
    let spec = zpo::tasks::parse("events:irq:42->slice:Present").unwrap();
    let res =
        zpo::analysis::analyze(&tr, spec, None, zpo::analysis::MAX_JOBS, zpo::analysis::MAX_DEPTH)
            .unwrap();
    assert_eq!(res.jobs.len(), JOBS as usize);
    assert!(res.response_ns.iter().all(|&r| r == 19_000));
    // Per job: app blocked on the irq thread [10, 19]: the irq thread waits
    // for IRQ 42 (2), waits for a CPU (1) and runs (6); then app waits for a
    // CPU (1) and runs (9).
    let bb = res.attributions[0].by_bucket();
    assert_eq!(bb.get(&Bucket::Run), Some(&15_000));
    assert_eq!(bb.get(&Bucket::WakeLatency), Some(&2_000));
    assert_eq!(bb.get(&Bucket::HwWait), Some(&2_000));
    let hops: Vec<&String> = res.hops.keys().map(|k| &k.2).collect();
    assert_eq!(hops, vec!["<- Echo.Ping"]);
    let irq_u = tr.threads.values().find(|t| t.tid == IRQ_T as i64).unwrap().utid;
    assert_eq!(res.threads[&irq_u].deadline_params, Some((50_000, 1_000_000)));
    assert_eq!(res.hw_waits.keys().next().map(|k| k.1), Some(42));
    tr.check().unwrap();
}

#[test]
fn warm_sessions_start_and_stop() {
    let e = env(100_000);
    let name = e.sessions.ensure(&e.trace, &|_| {}).unwrap();
    assert!(e.sessions.alive(&name));
    assert_eq!(e.sessions.ensure(&e.trace, &|_| {}).unwrap(), name);
    e.sessions.kill(&name);
    assert!(!e.sessions.alive(&name));
}

#[test]
fn unsymbolized_ipc_is_explained() {
    let e = env_with(100_000, fixture_with_ipc_name("ChannelMessage"));
    let tr = zpo::trace_db::load(&e.sessions, &e.trace, &|_| {}).unwrap();
    assert!(zpo::analysis::ipc_unsymbolized(&tr));
    let spec = zpo::tasks::parse("events:irq:42->ipc-in:Echo.Ping").unwrap();
    let Err(err) =
        zpo::analysis::analyze(&tr, spec, None, zpo::analysis::MAX_JOBS, zpo::analysis::MAX_DEPTH)
    else {
        panic!("an ipc-in: task matched on an unsymbolized trace");
    };
    let err = err.to_string();
    assert!(err.contains("not symbolized"), "{err}");
    // Tasks that do not use FIDL names still work.
    let spec = zpo::tasks::parse("events:irq:42->slice:Present").unwrap();
    assert_eq!(
        zpo::analysis::analyze(&tr, spec, None, zpo::analysis::MAX_JOBS, zpo::analysis::MAX_DEPTH)
            .unwrap()
            .jobs
            .len(),
        JOBS as usize
    );
}

// Measurement conclusions (the command line and the UI share one path).

/// A session with one stress capture of the fixture, recorded on a device
/// with one 4-CPU domain whose only role matches the irq thread's profile.
fn mini_session(e: &Env, root: &Path) -> (zpo::runner::Ctx, PathBuf, zpo::topology::Topology) {
    let topo = zpo::topology::Topology::parse(
        "Domain d0\nlogical core count: 4\n + logical core id: 0\n + logical core id: 1\n \
         + logical core id: 2\n + logical core id: 3\nrelative performance: 1000\ndomain_id: 0\n",
        &[(
            "mini.profiles".into(),
            r#"{"scope": "board", "profiles": {"drv.irq": {"capacity": "50us", "period": "1ms", "affinity": [2, 3]}}}"#
                .into(),
        )],
    )
    .unwrap();
    let sdir = root.join("sessions/mini");
    let task = "events:irq:42->slice:Present";
    zpo::runner::Session::new(&sdir, task, zpo::runner::LoadDef::manual()).save().unwrap();
    std::fs::create_dir_all(sdir.join("baseline")).unwrap();
    let info = serde_json::json!({"name": "baseline", "topology": topo, "captures": [
        {"tag": "stress-1", "condition": "stress", "trace": e.trace.display().to_string()}]});
    std::fs::write(sdir.join("baseline/measurement.json"), info.to_string()).unwrap();
    let ctx = zpo::runner::Ctx {
        fuchsia_dir: root.to_path_buf(),
        sessions: Arc::new(Sessions::new(Arc::new(HostRunner), tp())),
        ext_dirs: vec![],
        checkout: Default::default(),
        lessons: Default::default(),
    };
    (ctx, sdir, topo)
}

#[test]
fn cli_and_ui_reach_the_same_conclusion() {
    let e = env(100_000);
    let root = tempfile::tempdir().unwrap();
    let (ctx, sdir, topo) = mini_session(&e, root.path());
    // `zpo session-analyze` and the UI's Analyze both run this.
    let summ =
        zpo::summary::analyze_measurement(&ctx, &sdir, "baseline", Arc::new(|_| {}), None).unwrap();
    let props = summ["proposals"].as_array().unwrap().clone();
    assert!(!props.is_empty(), "{summ}");
    // The UI reads the written summary through the server.
    let st =
        Arc::new(zpo::server::State::new(root.path().join("sessions"), ctx.clone(), None).unwrap());
    let ui = zpo::server::get_json(&st, "/api/session/mini/baseline/summary").unwrap();
    assert_eq!(ui["verdict"], summ["verdict"]);
    assert_eq!(ui["proposals"], summ["proposals"]);
    // The command line prints the same numbers.
    let text = zpo::summary::conclusion_text(&summ);
    assert!(text.contains(summ["verdict"]["headline"].as_str().unwrap()), "{text}");
    for p in &props {
        let row = format!(
            "{}: {}us / {}us",
            p["label"].as_str().unwrap(),
            p["capacity_us"],
            p["period_us"]
        );
        assert!(text.contains(&row), "{row} not in {text}");
    }
    // `zpo analyze` on the same capture, with the options the measurement
    // uses for a stress capture, concludes the same through the same function.
    let tr = zpo::trace_db::load(&ctx.sessions, &e.trace, &|_| {}).unwrap();
    let spec = zpo::tasks::parse("events:irq:42->slice:Present").unwrap();
    let res = zpo::analysis::analyze(
        &tr,
        spec,
        Some(&topo),
        zpo::analysis::MAX_JOBS,
        zpo::analysis::MAX_DEPTH,
    )
    .unwrap();
    let opts = zpo::derive::DeriveOptions::with_window(None);
    let c = zpo::verdict::conclude(&res, &tr, &topo, &opts, zpo::verdict::TAIL_Q, &ctx.lessons);
    assert_eq!(summ["verdict"]["primary"], c.verdict.primary.as_str());
    let caps: Vec<(String, i64, i64)> = c
        .shown
        .iter()
        .map(|p| (p.label.clone(), p.capacity_ns / 1000, p.period_ns / 1000))
        .collect();
    let from_summary: Vec<(String, i64, i64)> = props
        .iter()
        .map(|p| {
            let n = |k: &str| p[k].as_i64().unwrap();
            (p["label"].as_str().unwrap().to_string(), n("capacity_us"), n("period_us"))
        })
        .collect();
    assert_eq!(caps, from_summary);
    // The irq thread runs with drv.irq's parameters, so its proposal keeps
    // that role's CPUs.
    let irq = c.proposals.iter().find(|p| p.label.contains("irq-thread")).unwrap();
    assert_eq!(irq.affinity, vec![2, 3]);
    assert!(irq.notes.iter().any(|n| n.contains("kept from drv.irq")), "{:?}", irq.notes);
    assert!(!irq.notes.iter().any(|n| n == zpo::topology::NO_CORE_IDS_NOTE));
    // The same capture on a device whose cpuctl does not report core IDs:
    // the same capacities and affinities, and a note that the CPU fit was not
    // checked.
    let old = zpo::topology::Topology {
        domains: topo
            .domains
            .iter()
            .map(|d| zpo::topology::Domain { cpus: vec![], ..d.clone() })
            .collect(),
        roles: topo.roles.clone(),
    };
    assert!(!old.knows_cpus());
    let props_new = zpo::derive::derive(&tr, &res, &topo, &opts);
    let props_old = zpo::derive::derive(&tr, &res, &old, &opts);
    assert_eq!(props_old.len(), props_new.len());
    for (p, q) in props_new.iter().zip(&props_old) {
        assert_eq!(
            (p.capacity_ns, p.period_ns, &p.affinity),
            (q.capacity_ns, q.period_ns, &q.affinity)
        );
        assert!(q.notes.iter().any(|n| n == zpo::topology::NO_CORE_IDS_NOTE), "{:?}", q.notes);
    }
    ctx.sessions.kill_started();
}
