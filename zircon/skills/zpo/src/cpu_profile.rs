// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! CPU profile captures: which processes to sample, and where the chain's run
//! time goes.
//!
//! A profile capture is a normal trace plus an `ffx profiler` capture of the
//! processes on the task's critical path, concatenated into one .fxt. The
//! trace processor reads both; the profiler's samples land in `perf_sample`
//! with utids shared with the trace's threads.
//!
//! - The profiler's default 10 ms period gives ~0.1 samples per ms of chain
//!   run time, useless for jobs of a few hundred us. 100 us on the chain's
//!   processes gives ~9 per ms, a median of 3-4 per job.
//! - `ffx profiler attach --background` returns once sampling is live, so
//!   starting it before the trace keeps its 2-12 s of setup out of the capture.
//! - System-wide at 100 us is not practical: 16x the samples (minutes to
//!   download) for the same view of the chain.

use crate::analysis::AnalysisResult;
use crate::attribution::Bucket;
use crate::model::Trace;
use crate::profiles_file::{NS_PER_MS_F64, NS_PER_US};
use crate::stats;
use crate::tp::queries;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// The sampling period.
pub const PERIOD_US: i64 = 100;
/// Leaf frames in the vDSO are syscall stubs: the thread is in the kernel.
pub const VDSO: &str = "libzircon.so";
pub const UNMAPPED: &str = "[unmapped]";

/// (function, binary basename).
pub type Frame = (String, String);

/// "driver-host-#input/fdf-dispatcher-thread-0:fuchsia[41307]" -> "driver-host-#input".
pub fn process_of(label: &str) -> Option<String> {
    if label.is_empty()
        || !label.contains('/')
        || label.starts_with('<')
        || label.starts_with("utid:")
    {
        return None;
    }
    let p = label.split('/').next().unwrap_or("");
    (!p.is_empty()).then(|| p.to_string())
}

/// Distinct process names of thread labels, in first-seen order.
pub fn chain_processes<'a>(labels: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        if let Some(p) = process_of(l) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// The chain's processes from an analysis.json (tag -> packed capture).
pub fn processes_from_analysis(analysis: &Value) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    if let Some(m) = analysis.as_object() {
        for v in m.values() {
            if let Some(t) = v.get("threads").and_then(Value::as_object) {
                labels.extend(t.keys().cloned());
            }
        }
    }
    chain_processes(labels.iter().map(String::as_str))
}

/// Pids per process name from the device's `ps` output, which prints
/// processes as `p: <koid> <PSS> <PRIVATE> <SHARED> NAME` (the STATE column is
/// empty on process lines). The whole NAME column must match, so "bar" never
/// matches a process named "foo bar". Names can repeat (several instances);
/// names not running map to [].
pub fn parse_ps(text: &str, names: &[String]) -> IndexMap<String, Vec<i64>> {
    let mut out: IndexMap<String, Vec<i64>> =
        names.iter().map(|n| (n.clone(), Vec::new())).collect();
    let re = regex_lite::Regex::new(r"^\s*p:\s+(\d+)\s+\S+\s+\S+\s+\S+\s+(.*)$")
        .expect("the pattern is a valid regex");
    for line in text.lines() {
        let Some(c) = re.captures(line.trim_end()) else { continue };
        let Ok(koid) = c[1].parse::<i64>() else { continue };
        if let Some(pids) = out.get_mut(c[2].trim()) {
            pids.push(koid);
        }
    }
    out
}

/// Removes every top-level open...close group, keeping "open…close".
fn strip_balanced(s: &str, open: char, close: char) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in s.chars() {
        if ch == open {
            if depth == 0 {
                out.push(open);
                out.push('…');
            }
            depth += 1;
        } else if ch == close && depth > 0 {
            depth -= 1;
            if depth == 0 {
                out.push(close);
            }
        } else if depth == 0 {
            out.push(ch);
        }
    }
    out
}

/// A readable function name: no argument list, generics collapsed, e.g.
/// "core::result::Result<…>::unwrap<…>".
pub fn short_name(f: &str) -> String {
    if f.is_empty() {
        return "?".into();
    }
    let mut s: String = f.trim().to_string();
    // Drop a trailing argument list (C++/Rust demangled), balanced.
    if s.ends_with(')') {
        let chars: Vec<char> = s.chars().collect();
        let mut depth = 0;
        for i in (0..chars.len()).rev() {
            if chars[i] == ')' {
                depth += 1;
            } else if chars[i] == '(' {
                depth -= 1;
                if depth == 0 {
                    s = chars[..i].iter().collect();
                    break;
                }
            }
        }
    }
    let s = strip_balanced(&s, '<', '>');
    let t = s.trim();
    if t.is_empty() { f.to_string() } else { t.to_string() }
}

/// "CODE_SYSCALL_zx_port_wait" / "_zx_port_wait" -> "zx_port_wait".
pub fn syscall_name(f: &str) -> String {
    let re = regex_lite::Regex::new(r"(zx_\w+)").expect("the pattern is a valid regex");
    match re.captures(f) {
        Some(c) => c[1].to_string(),
        None if f.is_empty() => "?".into(),
        None => f.to_string(),
    }
}

/// Library code that says little about *which* work ran: an inlined unwrap,
/// an atomic, the async executor, FIDL plumbing, libc. The first frame below
/// the leaf that is not one of these is the row's context.
const GENERIC: &[&str] = &[
    "core::",
    "std::",
    "alloc::",
    "mmio::",
    "futures",
    "fuchsia_async::",
    "fidl_next",
    "zx::",
    "scudo::",
    "hashbrown::",
    "λ",
    "__",
    "mtx_",
    "cnd_",
    "memcpy",
    "memset",
    "memmove",
    "malloc",
    "free",
    "operator ",
];

fn is_generic(name: &str) -> bool {
    GENERIC.iter().any(|g| name.starts_with(g)) || name == "?" || name.is_empty()
}

/// The nearest non-generic, non-vDSO frame at or below the leaf, short name.
pub fn context_of(frames: &[Frame]) -> Option<String> {
    for (f, b) in frames {
        if b == VDSO || b == UNMAPPED {
            continue;
        }
        let s = short_name(f);
        if !is_generic(&s) {
            return Some(s);
        }
    }
    None
}

/// A counter that remembers insertion order.
#[derive(Default)]
struct Counter<K: std::hash::Hash + Eq + Clone>(IndexMap<K, i64>);

impl<K: std::hash::Hash + Eq + Clone> Counter<K> {
    fn add(&mut self, k: K) {
        *self.0.entry(k).or_insert(0) += 1;
    }
    /// The n most common keys: by count, ties in insertion order.
    fn most_common(&self, n: usize) -> Vec<(K, i64)> {
        let mut v: Vec<(K, i64)> = self.0.iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
    fn get(&self, k: &K) -> i64 {
        self.0.get(k).copied().unwrap_or(0)
    }
    fn first(&self) -> Option<K> {
        self.most_common(1).into_iter().next().map(|(k, _)| k)
    }
}

type Key = (String, String);

/// The function table for the samples that fall in the given run segments.
///
/// `samples`: utid -> [(ts, callsite)] sorted by ts; `stacks`: callsite ->
/// frames, leaf first; `segments`: (utid, t0, t1) run segments on the critical
/// path. Each row has name, full, binary, kernel, self, total, thread, caller
/// (the frame that made the syscall, kernel rows only), context (the most
/// common context_of() when it differs from the name) and stack (the most
/// common stack: the 12 frames nearest the leaf, root first, short names).
pub fn aggregate(
    samples: &IndexMap<i64, Vec<(i64, i64)>>,
    stacks: &HashMap<i64, Vec<Frame>>,
    segments: &[(i64, i64, i64)],
    labels: &HashMap<i64, String>,
    top: usize,
) -> Map<String, Value> {
    let mut n = 0i64;
    let mut kernel = 0i64;
    let mut self_c: Counter<Key> = Counter::default();
    let mut total_c: Counter<Key> = Counter::default();
    let mut thread_c: HashMap<Key, Counter<String>> = HashMap::new();
    let mut caller_c: HashMap<Key, Counter<String>> = HashMap::new();
    let mut context_c: HashMap<Key, Counter<String>> = HashMap::new();
    let mut stack_c: HashMap<Key, Counter<Vec<String>>> = HashMap::new();
    let mut full: HashMap<Key, String> = HashMap::new();
    let unmapped: Vec<Frame> = vec![("?".into(), UNMAPPED.into())];

    for &(utid, t0, t1) in segments {
        let Some(v) = samples.get(&utid).filter(|v| !v.is_empty()) else { continue };
        let lo = v.partition_point(|&(t, _)| t < t0);
        let hi = v.partition_point(|&(t, _)| t <= t1);
        for &(_, cs) in &v[lo..hi.max(lo)] {
            let frames = stacks.get(&cs).filter(|f| !f.is_empty()).unwrap_or(&unmapped);
            n += 1;
            let frame_key = |f: &str, b: &str| -> Key {
                if b == VDSO {
                    (format!("in kernel: {}", syscall_name(f)), VDSO.to_string())
                } else if b == UNMAPPED || f.is_empty() || f == "?" {
                    (
                        UNMAPPED.to_string(),
                        if b.is_empty() { UNMAPPED.to_string() } else { b.to_string() },
                    )
                } else {
                    (short_name(f), b.to_string())
                }
            };
            let (f, binary) = &frames[0];
            let key: Key = frame_key(f, binary);
            if binary == VDSO {
                kernel += 1;
                full.insert(key.clone(), f.clone());
                if let Some((cf, cb)) = frames[1..].iter().find(|(_, b)| b != VDSO) {
                    caller_c
                        .entry(key.clone())
                        .or_default()
                        .add(format!("{} ({cb})", short_name(cf)));
                }
            } else if binary == UNMAPPED || f.is_empty() || f == "?" {
                full.insert(key.clone(), if f.is_empty() { "?".into() } else { f.clone() });
            } else {
                full.insert(key.clone(), f.clone());
            }
            self_c.add(key.clone());
            thread_c
                .entry(key.clone())
                .or_default()
                .add(labels.get(&utid).cloned().unwrap_or_else(|| format!("utid:{utid}")));
            if let Some(ctx) = context_of(frames) {
                if ctx != key.0 {
                    context_c.entry(key.clone()).or_default().add(ctx);
                }
            }
            let stack: Vec<String> =
                frames.iter().take(12).rev().map(|(f, _)| short_name(f)).collect();
            stack_c.entry(key.clone()).or_default().add(stack);
            let mut seen: HashSet<Key> = HashSet::new();
            for (f, b) in frames {
                let k = frame_key(f, b);
                if seen.insert(k.clone()) {
                    total_c.add(k);
                }
            }
        }
    }

    let mut rows = Vec::new();
    for (key, sc) in self_c.most_common(top) {
        rows.push(json!({
            "name": key.0,
            "full": full.get(&key).cloned().unwrap_or_else(|| key.0.clone()),
            "binary": key.1,
            "kernel": key.1 == VDSO,
            "self": sc,
            "total": sc.max(total_c.get(&key)),
            "thread": thread_c.get(&key).and_then(Counter::first),
            "caller": caller_c.get(&key).and_then(Counter::first),
            "context": context_c.get(&key).and_then(Counter::first),
            "stack": stack_c.get(&key).and_then(Counter::first).unwrap_or_default(),
        }));
    }
    let mut out = Map::new();
    out.insert("samples".into(), json!(n));
    out.insert("kernel_share".into(), json!(if n != 0 { kernel as f64 / n as f64 } else { 0.0 }));
    out.insert("functions".into(), Value::Array(rows));
    out
}

/// Nanoseconds to add to `perf_sample.ts` to put it on the trace's clock.
///
/// `ffx trace` records ZX_CLOCK_BOOT while the profiler records
/// ZX_CLOCK_MONOTONIC without saying so. Once the device has suspended since
/// boot, the two differ by the time spent suspended, which `clock_snapshot`
/// records. Checking against `sched` keeps this safe once the profiler tags
/// its clock.
pub fn clock_offset_ns(tr: &Trace, samples: &IndexMap<i64, Vec<(i64, i64)>>) -> i64 {
    let off = match tr.query(&queries::clock_offset()) {
        Ok(rows) => rows.first().and_then(|r| r.get::<Option<i64>>(0)).unwrap_or(0),
        Err(_) => return 0,
    };
    // Offsets under one sampling period cannot be told apart by the sched-hit
    // test, so they are ignored.
    if off == 0 || off.abs() < PERIOD_US * NS_PER_US {
        return 0;
    }
    let mut by_count: Vec<(&i64, usize)> = samples.iter().map(|(u, v)| (u, v.len())).collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1));
    let top: Vec<i64> = by_count.iter().take(4).map(|(u, _)| **u).collect();
    let hits = |shift: i64| -> usize {
        let mut h = 0;
        for u in &top {
            for &(ts, _) in &samples[u] {
                let t = ts + shift;
                if tr.start_ts <= t && t <= tr.end_ts && tr.sched_at(*u, t).is_some() {
                    h += 1;
                }
            }
        }
        h
    };
    if hits(off) > hits(0) { off } else { 0 }
}

/// The capture's CPU samples: utid -> [(ts, callsite)] shifted onto the
/// trace's clock, each referenced callsite's stack (leaf first), and the shift.
pub fn load_samples(
    tr: &Trace,
) -> anyhow::Result<(IndexMap<i64, Vec<(i64, i64)>>, HashMap<i64, Vec<Frame>>, i64)> {
    let mut samples: IndexMap<i64, Vec<(i64, i64)>> = IndexMap::new();
    for r in tr.query(&queries::perf_samples())?.iter() {
        samples.entry(r.get::<i64>(1)).or_default().push((r.get::<i64>(0), r.get::<i64>(2)));
    }
    if samples.is_empty() {
        return Ok((samples, HashMap::new(), 0));
    }
    let off = clock_offset_ns(tr, &samples);
    if off != 0 {
        for v in samples.values_mut() {
            for s in v.iter_mut() {
                s.0 += off;
            }
        }
    }
    let mut parent: HashMap<i64, Option<i64>> = HashMap::new();
    let mut frame_of: HashMap<i64, Frame> = HashMap::new();
    for r in tr.query(&queries::callsites())?.iter() {
        let id = r.get::<i64>(0);
        parent.insert(id, r.get::<Option<i64>>(1));
        let mp = r.get::<String>(3);
        let mp = if !mp.is_empty() && !mp.starts_with('[') {
            mp.rsplit('/').next().unwrap_or(&mp).to_string()
        } else if mp.is_empty() {
            UNMAPPED.to_string()
        } else {
            mp
        };
        frame_of.insert(id, (r.get::<String>(2), mp));
    }
    let mut stacks: HashMap<i64, Vec<Frame>> = HashMap::new();
    let callsites: HashSet<i64> = samples.values().flatten().map(|&(_, c)| c).collect();
    for cs in callsites {
        let mut frames = Vec::new();
        let mut cur = Some(cs);
        let mut guard = 0;
        while let Some(c) = cur {
            let Some(f) = frame_of.get(&c) else { break };
            // Loop bound against cycles in the callsite table.
            if guard >= 256 {
                break;
            }
            frames.push(f.clone());
            cur = parent.get(&c).copied().flatten();
            guard += 1;
        }
        stacks.insert(cs, frames);
    }
    Ok((samples, stacks, off))
}

/// Where the run time of the slowest jobs goes, by function: [`aggregate`]
/// plus jobs, jobs_without_samples, run_ms, quantile, period_us,
/// clock_offset_ns and slowest_job ({t0_ns, t1_ns, threads}, for opening it in
/// Perfetto).
pub fn run_functions(
    tr: &Trace,
    res: &AnalysisResult,
    quantile: f64,
    top: usize,
) -> anyhow::Result<Map<String, Value>> {
    let (samples, stacks, offset) = load_samples(tr)?;
    let durs: Vec<i64> = res.jobs.iter().map(|j| j.t1 - j.t0).collect();
    let cut = if durs.is_empty() { 0.0 } else { stats::quantile_i64(&durs, quantile) };
    let mut segments: Vec<(i64, i64, i64)> = Vec::new();
    let mut per_job: Vec<Vec<(i64, i64, i64)>> = Vec::new();
    let mut slowest: Option<(usize, Vec<(i64, i64, i64)>)> = None;
    for (i, (j, a)) in res.jobs.iter().zip(&res.attributions).enumerate() {
        if ((j.t1 - j.t0) as f64) < cut {
            continue;
        }
        let segs: Vec<(i64, i64, i64)> = a
            .leaf_segments()
            .filter(|s| s.bucket == Bucket::Run && s.dur() > 0)
            .map(|s| (s.utid, s.t0, s.t1))
            .collect();
        per_job.push(segs.clone());
        segments.extend(segs.iter().copied());
        let longer =
            slowest.as_ref().is_none_or(|(k, _)| j.t1 - j.t0 > res.jobs[*k].t1 - res.jobs[*k].t0);
        if longer {
            slowest = Some((i, segs));
        }
    }
    let labels: HashMap<i64, String> = segments.iter().map(|s| (s.0, tr.label(s.0))).collect();
    let mut out = aggregate(&samples, &stacks, &segments, &labels, top);
    let count = |segs: &[(i64, i64, i64)]| -> usize {
        segs.iter()
            .map(|&(u, t0, t1)| {
                samples.get(&u).map_or(0, |v| {
                    v.partition_point(|&(t, _)| t <= t1)
                        .saturating_sub(v.partition_point(|&(t, _)| t < t0))
                })
            })
            .sum()
    };
    out.insert("jobs".into(), json!(per_job.len()));
    out.insert(
        "jobs_without_samples".into(),
        json!(per_job.iter().filter(|s| count(s) == 0).count()),
    );
    out.insert(
        "run_ms".into(),
        json!(segments.iter().map(|&(_, t0, t1)| t1 - t0).sum::<i64>() as f64 / NS_PER_MS_F64),
    );
    out.insert("quantile".into(), json!(quantile));
    out.insert("period_us".into(), json!(PERIOD_US));
    out.insert("clock_offset_ns".into(), json!(offset));
    if let Some((k, segs)) = slowest {
        let j = &res.jobs[k];
        let mut threads: Vec<String> =
            segs.iter().map(|s| tr.label(s.0)).collect::<HashSet<_>>().into_iter().collect();
        threads.sort();
        out.insert("slowest_job".into(), json!({"t0_ns": j.t0, "t1_ns": j.t1, "threads": threads}));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SchedSlice, SchedulingParams};
    use crate::tp::{Query, Rows};

    #[test]
    fn parse_ps_matches_names_and_keeps_duplicates() {
        let ps = "TASK                     PSS PRIVATE  SHARED   STATE NAME\nj: 1051              2480.2M 2139.2M                 root\n      p: 11647       2763.0K   2064K   12.3M         driver-host-#gpio\n      p: 17175       2120.2K   1548K   8896K         driver-host-#input\n      p: 17200       2120.2K   1548K   8896K         driver-host-#input\n      p: 79509        960.9K    888K   6652K         scene_manager.cm\n      p: 79600        960.9K    888K   6652K         foo bar\n";
        let names: Vec<String> = ["driver-host-#input", "scene_manager.cm", "gone", "bar"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = parse_ps(ps, &names);
        assert_eq!(got["driver-host-#input"], vec![17175, 17200]);
        assert_eq!(got["scene_manager.cm"], vec![79509]);
        assert_eq!(got["gone"], Vec::<i64>::new());
        assert_eq!(got["bar"], Vec::<i64>::new());
    }

    #[test]
    fn chain_processes_from_thread_labels() {
        let ana = json!({
            "idle-1": {"threads": {
                "driver-host-#input/fdf-dispatcher-thread-0:fuchsia[41307]": {},
                "scene_manager.cm/initial-thread[79511]": {},
            }},
            "stress-1": {"threads": {"driver-host-#input/fdf-dispatcher-thread-1[1]": {}, "utid:7": {}}},
        });
        assert_eq!(processes_from_analysis(&ana), vec!["driver-host-#input", "scene_manager.cm"]);
    }

    #[test]
    fn short_and_syscall_names() {
        assert_eq!(
            short_name(
                "core::result::Result<u32, mmio::mmio::MmioError>::unwrap<u32, mmio::mmio::MmioError>(core::result::Result<u32, mmio::mmio::MmioError>)"
            ),
            "core::result::Result<…>::unwrap<…>"
        );
        assert_eq!(
            short_name(
                "scudo::Allocator<scudo::DefaultConfig, &malloc_postinit>::allocate(unsigned long)"
            ),
            "scudo::Allocator<…>::allocate"
        );
        assert_eq!(short_name("memcpy"), "memcpy");
        assert_eq!(syscall_name("CODE_SYSCALL_zx_port_wait"), "zx_port_wait");
        assert_eq!(syscall_name("zx_channel_write"), "zx_channel_write");
    }

    #[test]
    fn aggregate_counts_only_run_segments_and_names_syscalls() {
        let spi = "dw-spi.so";
        let f = |a: &str, b: &str| (a.to_string(), b.to_string());
        let stacks: HashMap<i64, Vec<Frame>> = [
            (
                1,
                vec![
                    f("CODE_SYSCALL_zx_port_wait", VDSO),
                    f("zx::port::Port::wait", "libvfs_rust.so"),
                    f("Executor::run_singlethreaded", "libvfs_rust.so"),
                ],
            ),
            (
                2,
                vec![
                    f("core::result::Result<u32, E>::unwrap<u32, E>(core::result::Result<u32, E>)", spi),
                    f("mmio::register::RegisterProxy<R>::read<R>()", spi),
                    f("_dw_spi_driver_rustc::spi_device::DwSpiDeviceInner::exchange_pio_loop", spi),
                ],
            ),
        ]
        .into_iter()
        .collect();
        // The last sample is outside the segment.
        let samples: IndexMap<i64, Vec<(i64, i64)>> =
            [(5, vec![(100, 1), (150, 2), (160, 2), (900, 2)])].into_iter().collect();
        let labels: HashMap<i64, String> =
            [(5, "driver-host-#input/t[1]".to_string())].into_iter().collect();
        let out = aggregate(&samples, &stacks, &[(5, 100, 200)], &labels, 20);
        assert_eq!(out["samples"], 3);
        assert!((out["kernel_share"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-12);
        let by: HashMap<String, &Value> = out["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["name"].as_str().unwrap().to_string(), r))
            .collect();
        let unwrap = by["core::result::Result<…>::unwrap<…>"];
        assert_eq!(unwrap["self"], 2);
        assert_eq!(
            unwrap["context"],
            "_dw_spi_driver_rustc::spi_device::DwSpiDeviceInner::exchange_pio_loop"
        );
        let k = by["in kernel: zx_port_wait"];
        assert_eq!(k["kernel"], true);
        assert_eq!(k["caller"], "zx::port::Port::wait (libvfs_rust.so)");
        assert_eq!(k["stack"].as_array().unwrap().last().unwrap(), "CODE_SYSCALL_zx_port_wait");
        assert_eq!(k["thread"], "driver-host-#input/t[1]");
    }

    struct Offset(i64);
    impl crate::model::TraceSource for Offset {
        fn query(&self, _q: &Query) -> anyhow::Result<Rows> {
            Ok(Rows { rows: vec![vec![json!(self.0)]] })
        }
    }

    #[test]
    fn monotonic_samples_are_shifted_to_boottime_only_when_needed() {
        let mut tr = Trace::new();
        tr.set_source(Box::new(Offset(3_000_000_000)));
        tr.start_ts = 9_000_000_000;
        tr.end_ts = 9_010_000_000;
        tr.set_sched(
            5,
            vec![SchedSlice {
                ts: 9_005_000_000,
                dur: 500_000,
                cpu: 0,
                scheduling_params: SchedulingParams::Fair { weight: 20 },
            }],
        );
        tr.finalize();
        // Samples recorded on ZX_CLOCK_MONOTONIC (3 s behind BOOTTIME): shifted.
        let mono: IndexMap<i64, Vec<(i64, i64)>> =
            [(5, vec![(6_005_100_000, 1), (6_005_200_000, 1)])].into_iter().collect();
        assert_eq!(clock_offset_ns(&tr, &mono), 3_000_000_000);
        // Samples already translated to BOOTTIME: left alone.
        let boot: IndexMap<i64, Vec<(i64, i64)>> =
            [(5, vec![(9_005_100_000, 1), (9_005_200_000, 1)])].into_iter().collect();
        assert_eq!(clock_offset_ns(&tr, &boot), 0);
    }
}
