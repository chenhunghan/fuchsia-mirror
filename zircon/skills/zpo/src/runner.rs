// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Measurement runner: load definition, stress, repeated captures, A/B gate.
//!
//! A *session* lives in a directory and holds a `session.json` describing the
//! task and load, plus one sub-directory per measurement (baseline,
//! candidate-1, ...), each with its traces and analysis JSON.

use crate::analysis::{self, NoJobs};
use crate::derive::{self, DeriveOptions};
use crate::device::{self, Device};
use crate::model::{StateKind, Trace};
use crate::profiles_file::{NS_PER_US_F64, parse_dur_ns};
use crate::topology::{self, Topology};
use crate::tp::queries;
use crate::tp::session::Sessions;
use crate::{cpu_profile, proc, report, roles, stats, tasks, verdict};
use anyhow::Context;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often a wait checks for a cancel.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// How long the stress load runs before a capture starts, so every core is
/// busy by then.
const STRESS_SETTLE: Duration = Duration::from_secs(2);
/// The period assumed for a collateral deadline thread whose key has no
/// parameters: 1 ms.
const FALLBACK_PERIOD_NS: i64 = 1_000_000;

/// The user (or the UI) cancelled a run.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The CPU profile could not be recorded; the measurement goes on without it.
#[derive(Debug)]
pub struct ProfileUnavailable(pub String);

impl std::fmt::Display for ProfileUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileUnavailable {}

/// The captures that count: everything but the CPU profile captures.
pub fn regular_captures(info: &Value) -> Vec<Value> {
    captures(info).into_iter().filter(|c| !truthy(c.get("profile"))).collect()
}

/// The CPU profile captures.
pub fn profile_captures(info: &Value) -> Vec<Value> {
    captures(info).into_iter().filter(|c| truthy(c.get("profile"))).collect()
}

fn captures(info: &Value) -> Vec<Value> {
    info.get("captures").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Analysis steps: one per capture, one for the report, one per profile capture.
pub fn progress_steps(info: &Value) -> usize {
    regular_captures(info).len() + 1 + profile_captures(info).len()
}

/// How to exercise the task while tracing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadDef {
    /// "manual" | "automated"
    pub mode: String,
    /// manual: instructions shown to the user, one per capture.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// automated: a shell command run on the host for the capture (started
    /// right before tracing; exiting early is fine).
    #[serde(default)]
    pub command: String,
    /// Seconds per capture.
    #[serde(default = "default_duration")]
    pub duration_s: f64,
    /// Seconds of settling before the trace starts.
    #[serde(default = "default_warmup")]
    pub warmup_s: f64,
}

fn default_duration() -> f64 {
    15.0
}
fn default_warmup() -> f64 {
    2.0
}
fn default_true() -> bool {
    true
}
fn default_repeats() -> i64 {
    2
}
fn default_buffer() -> i64 {
    512
}
fn default_categories() -> String {
    device::DEFAULT_CATEGORIES.into()
}

impl LoadDef {
    pub fn manual() -> Self {
        LoadDef {
            mode: "manual".into(),
            instructions: vec![],
            command: String::new(),
            duration_s: default_duration(),
            warmup_s: default_warmup(),
        }
    }
}

/// A session's definition (`session.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub dir: String,
    pub task: String,
    pub load: LoadDef,
    #[serde(default)]
    pub window: Option<String>,
    /// Also measure under full CPU load.
    #[serde(default = "default_true")]
    pub stress: bool,
    #[serde(default = "default_repeats")]
    pub repeats: i64,
    #[serde(default = "default_buffer")]
    pub trace_buffer_mb: i64,
    #[serde(default = "default_categories")]
    pub categories: String,
    /// One extra capture per condition with a CPU profile of the chain's
    /// processes, kept out of the comparison (sampling adds overhead).
    #[serde(default)]
    pub profile: bool,
}

impl Session {
    pub fn new(dir: &Path, task: &str, load: LoadDef) -> Self {
        Session {
            dir: dir.display().to_string(),
            task: task.into(),
            load,
            window: None,
            stress: true,
            repeats: default_repeats(),
            trace_buffer_mb: default_buffer(),
            categories: default_categories(),
            profile: false,
        }
    }

    pub fn path(&self) -> PathBuf {
        Path::new(&self.dir).join("session.json")
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.path(), serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }

    pub fn load(d: &Path) -> anyhow::Result<Session> {
        let p = d.join("session.json");
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("could not read {}", p.display()))?;
        let mut s: Session = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a valid session", p.display()))?;
        s.dir = d.display().to_string();
        Ok(s)
    }

    /// The consumer window in ns.
    pub fn window_ns(&self) -> Option<i64> {
        self.window.as_deref().and_then(parse_dur_ns)
    }

    /// What is wrong with this definition, as (field, reason) pairs; empty
    /// when it can be measured. Fields: task, start, end, fidl, thread,
    /// window, cmd, dur, warmup, rep, buf. The command line and the UI both check
    /// sessions here before saving them.
    pub fn problems(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        match tasks::parse(&self.task) {
            Err(e) => out.push(("task", e.to_string())),
            Ok(t) => match t.kind {
                tasks::TaskKind::Events => {
                    if t.start.is_empty() {
                        out.push(("start", "required".into()));
                    }
                    if t.end.is_empty() {
                        out.push(("end", "required".into()));
                    }
                }
                tasks::TaskKind::Fidl if t.method.is_empty() => {
                    out.push(("fidl", "required".into()))
                }
                tasks::TaskKind::Thread if t.text.trim_start_matches("thread:").is_empty() => {
                    out.push(("thread", "required".into()))
                }
                _ => {}
            },
        }
        if let Some(w) = self.window.as_deref().filter(|w| !w.trim().is_empty()) {
            if parse_dur_ns(w).is_none_or(|ns| ns <= 0) {
                out.push((
                    "window",
                    "a duration such as 850us or 16.6ms (a bare number is us)".into(),
                ));
            }
        }
        if self.load.mode == "automated" && self.load.command.trim().is_empty() {
            out.push(("cmd", "required".into()));
        }
        // Both become a `Duration`, which panics on infinity or NaN.
        let d = self.load.duration_s;
        if !(d.is_finite() && d > 0.0) {
            out.push(("dur", "positive number".into()));
        }
        let w = self.load.warmup_s;
        if !(w.is_finite() && w >= 0.0) {
            out.push(("warmup", "0 or more seconds".into()));
        }
        if self.repeats < 1 {
            out.push(("rep", "whole number, 1 or more".into()));
        }
        if self.trace_buffer_mb < 1 {
            out.push(("buf", "positive number".into()));
        }
        out
    }
}

/// What a [`Runner`] needs from the device; tests substitute a fake.
pub trait DeviceOps: Send + Sync {
    fn fuchsia_dir(&self) -> &Path;
    fn build_info(&self) -> anyhow::Result<device::BuildInfo>;
    /// The CPU topology and loaded roles.
    fn topology(&self) -> anyhow::Result<Topology>;
    /// Starts CPU load sized for a device with `ncpu` CPUs.
    fn start_stress(&self, ncpu: i64) -> anyhow::Result<()>;
    fn stop_stress(&self);
    /// Stops a load left on the device by an earlier zpo process.
    fn kill_stray_stress(&self);
    fn trace(
        &self,
        output: &Path,
        duration_s: f64,
        categories: &str,
        buffer_mb: i64,
        on_start: &mut dyn FnMut(),
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<PathBuf>;
    fn process_pids(&self, names: &[String]) -> IndexMap<String, Vec<i64>>;
    fn profiler_start(&self, pids: &[i64], period_us: i64) -> anyhow::Result<()>;
    fn profiler_stop(&self, output: &Path) -> anyhow::Result<PathBuf>;
    fn profiler_abort(&self);
}

impl DeviceOps for Device {
    fn fuchsia_dir(&self) -> &Path {
        &self.fuchsia_dir
    }
    fn build_info(&self) -> anyhow::Result<device::BuildInfo> {
        Device::build_info(self)
    }
    fn topology(&self) -> anyhow::Result<Topology> {
        Device::topology(self)
    }
    fn start_stress(&self, ncpu: i64) -> anyhow::Result<()> {
        Device::start_stress(self, ncpu)
    }
    fn stop_stress(&self) {
        Device::stop_stress(self)
    }
    fn kill_stray_stress(&self) {
        Device::kill_stray_stress(self)
    }
    fn trace(
        &self,
        output: &Path,
        duration_s: f64,
        categories: &str,
        buffer_mb: i64,
        on_start: &mut dyn FnMut(),
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<PathBuf> {
        Device::trace(self, output, duration_s, categories, buffer_mb, on_start, cancelled)
    }
    fn process_pids(&self, names: &[String]) -> IndexMap<String, Vec<i64>> {
        Device::process_pids(self, names)
    }
    fn profiler_start(&self, pids: &[i64], period_us: i64) -> anyhow::Result<()> {
        Device::profiler_start(self, pids, period_us)
    }
    fn profiler_stop(&self, output: &Path) -> anyhow::Result<PathBuf> {
        Device::profiler_stop(self, output)
    }
    fn profiler_abort(&self) {
        Device::profiler_abort(self)
    }
}

/// Where zpo finds its tools and extensions.
#[derive(Clone)]
pub struct Ctx {
    pub fuchsia_dir: PathBuf,
    pub sessions: Arc<Sessions>,
    /// Extension directories (agent, lesson links).
    pub ext_dirs: Vec<PathBuf>,
    /// How `fuchsia_dir` was picked, for the UI.
    pub checkout: crate::checkout::Description,
    /// Where the verdict's "why" links point (from the extensions).
    pub lessons: crate::extensions::Lessons,
}

impl std::fmt::Debug for Ctx {
    /// The trace processor sessions are left out: they are processes, not
    /// configuration.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("fuchsia_dir", &self.fuchsia_dir)
            .field("ext_dirs", &self.ext_dirs)
            .field("checkout", &self.checkout)
            .field("lessons", &self.lessons)
            .finish_non_exhaustive()
    }
}

impl Ctx {
    /// Loads a trace through its warm session.
    pub fn load_trace(&self, path: &Path, log: &dyn Fn(&str)) -> anyhow::Result<Trace> {
        crate::trace_db::load(&self.sessions, path, log)
    }
}

/// A blocking prompt to the user (manual load instructions).
pub type Prompt = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;
/// Fails with [`Cancelled`] when the run was cancelled.
pub type CheckCancel = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

/// Records and analyzes the measurements of one session.
pub struct Runner {
    pub s: Session,
    pub ctx: Ctx,
    dev: Option<Arc<dyn DeviceOps>>,
    prompt: Prompt,
    log: device::Log,
    check_cancel: CheckCancel,
}

impl std::fmt::Debug for Runner {
    /// The device, the prompt, the log and the cancel check are trait
    /// objects, which are not `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").field("s", &self.s).field("ctx", &self.ctx).finish_non_exhaustive()
    }
}

fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

impl Runner {
    pub fn new(
        s: Session,
        ctx: Ctx,
        dev: Option<Arc<dyn DeviceOps>>,
        prompt: Prompt,
        log: device::Log,
        check_cancel: Option<CheckCancel>,
    ) -> Self {
        Runner {
            s,
            ctx,
            dev,
            prompt,
            log,
            check_cancel: check_cancel.unwrap_or_else(|| Arc::new(|| Ok(()))),
        }
    }

    /// Sleeps `secs`, failing with [`Cancelled`] as soon as the run is
    /// cancelled (checked every 100 ms).
    fn sleep(&self, secs: f64) -> anyhow::Result<()> {
        let end = std::time::Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        loop {
            (self.check_cancel)()?;
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(CANCEL_POLL));
        }
    }

    fn log(&self, m: &str) {
        (self.log)(m)
    }

    fn dev(&self) -> anyhow::Result<&Arc<dyn DeviceOps>> {
        self.dev.as_ref().context("no device")
    }

    fn mdir(&self, name: &str) -> PathBuf {
        Path::new(&self.s.dir).join(name)
    }

    // Measuring: one measurement is N captures, each under idle and stress load.

    /// Records every capture of one measurement and writes measurement.json.
    pub fn measure(&self, name: &str, note: &str) -> anyhow::Result<Value> {
        let build = self.dev()?.build_info()?;
        // Read once per measurement and saved with it, so the measurement is
        // analyzed against the topology and roles it ran with.
        let topo = self.dev()?.topology()?;
        self.log(&format!(
            "CPU topology: {} CPUs in {} performance domains, {} roles loaded",
            topo.ncpu(),
            topo.domains.len(),
            topo.roles.len()
        ));
        if !topo.knows_cpus() {
            // Measuring still works; only the CPU fit of proposals is skipped.
            self.log(&format!(
                "WARNING: {}. {}",
                topology::NO_CORE_IDS_TITLE,
                topology::NO_CORE_IDS_WARNING
            ));
        }
        let ncpu = topo.ncpu();
        // Created only once the device answered, so a failed start leaves
        // no empty measurement behind.
        let mdir = self.mdir(name);
        std::fs::create_dir_all(&mdir)?;
        let mut info = json!({"name": name, "note": note, "started": now_iso(), "build": build,
                              "topology": topo, "captures": []});
        let mut conditions = vec!["idle"];
        if self.s.stress {
            conditions.push("stress");
        }
        // A zpo that died during a stress capture leaves loadgen running,
        // which would load the idle captures.
        self.dev()?.kill_stray_stress();
        for rep in 0..self.s.repeats {
            for cond in &conditions {
                let tag = format!("{cond}-{}", rep + 1);
                let out = mdir.join(format!("{tag}.fxt"));
                (self.check_cancel)()?;
                self.log(&format!("CAPTURE: {tag} start"));
                self.capture(&out, cond, rep as usize, &tag, None, ncpu)?;
                self.log(&format!("CAPTURE: {tag} done"));
                push_capture(
                    &mut info,
                    json!({"tag": tag, "condition": cond, "trace": out.display().to_string()}),
                );
            }
        }
        if self.s.profile {
            self.profile_captures(name, &mdir, &mut info, &conditions, ncpu)?;
        }
        info["finished"] = json!(now_iso());
        write_atomic(
            &mdir.join("measurement.json"),
            serde_json::to_string_pretty(&info)?.as_bytes(),
        )?;
        Ok(info)
    }

    /// One capture per condition with a CPU profile of the chain's processes.
    fn profile_captures(
        &self,
        name: &str,
        mdir: &Path,
        info: &mut Value,
        conditions: &[&str],
        ncpu: i64,
    ) -> anyhow::Result<()> {
        let (procs, src) = self.chain_processes(name, info)?;
        if procs.is_empty() {
            let e = format!("could not tell which processes are on the chain ({src})");
            self.log(&format!("WARNING: no CPU profile: {e}"));
            info["profile_error"] = json!(e);
            return Ok(());
        }
        self.log(&format!(
            "CPU profile: sampling {} every {} us (chain from {src})",
            procs.join(", "),
            cpu_profile::PERIOD_US
        ));
        for cond in conditions {
            let tag = format!("profile-{cond}-1");
            let out = mdir.join(format!("{tag}.fxt"));
            (self.check_cancel)()?;
            self.log(&format!("CAPTURE: {tag} start"));
            match self.capture(&out, cond, 0, &tag, Some(&procs), ncpu) {
                Ok(meta) => {
                    self.log(&format!("CAPTURE: {tag} done"));
                    let mut c = json!({"tag": tag, "condition": cond, "trace": out.display().to_string(), "profile": true});
                    if let (Some(cm), Some(mm)) = (c.as_object_mut(), meta.as_object()) {
                        for (k, v) in mm {
                            cm.insert(k.clone(), v.clone());
                        }
                    }
                    push_capture(info, c);
                }
                Err(e) if e.downcast_ref::<ProfileUnavailable>().is_some() => {
                    info["profile_error"] = json!(e.to_string());
                    self.log(&format!("WARNING: CPU profile skipped: {e}"));
                    self.log(&format!("CAPTURE: {tag} done"));
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Which processes the CPU profile samples: from the session's most recent
    /// analysis; for a first measurement, from a quick analysis of one of the
    /// captures just recorded.
    fn chain_processes(&self, name: &str, info: &Value) -> anyhow::Result<(Vec<String>, String)> {
        let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
        if let Ok(rd) = std::fs::read_dir(&self.s.dir) {
            for e in rd.flatten() {
                let m = e.file_name().to_string_lossy().into_owned();
                let p = e.path().join("analysis.json");
                if m == name {
                    continue;
                }
                if let Ok(t) = p.metadata().and_then(|md| md.modified()) {
                    if best.as_ref().is_none_or(|(_, bt)| t > *bt) {
                        best = Some((p, t));
                    }
                }
            }
        }
        if let Some((p, _)) = best {
            if let Ok(v) = std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| Ok(serde_json::from_str::<Value>(&t)?))
            {
                let procs = cpu_profile::processes_from_analysis(&v);
                if !procs.is_empty() {
                    let m = p
                        .parent()
                        .and_then(Path::file_name)
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    return Ok((procs, format!("the {m} analysis")));
                }
            }
        }
        let topo = Topology::from_measurement(info).ok();
        for cap in regular_captures(info) {
            let tag = s(&cap, "tag");
            self.log(&format!(
                "STATUS: Finding the task's chain in {tag} (to know what to profile)…"
            ));
            let log = self.log.clone();
            let tr = self.ctx.load_trace(Path::new(s(&cap, "trace")), &|m| log(m))?;
            match analysis::analyze(
                &tr,
                tasks::parse(&self.s.task)?,
                topo.as_ref(),
                300,
                analysis::MAX_DEPTH,
            ) {
                Ok(res) => {
                    let labels: Vec<String> =
                        res.threads.values().map(|t| t.label.clone()).collect();
                    return Ok((
                        cpu_profile::chain_processes(labels.iter().map(String::as_str)),
                        format!("capture {tag}"),
                    ));
                }
                Err(e) if e.downcast_ref::<NoJobs>().is_some() => continue,
                Err(e) => return Err(e),
            }
        }
        Ok((vec![], "the task did not run in any capture".into()))
    }

    /// Records one capture, optionally with a CPU profile. A profile capture
    /// starts the profiler before the trace (its setup stays out of the
    /// capture), stops it after, and writes trace + profile as one .fxt.
    /// Returns the profile's metadata ({} without a profile).
    fn capture(
        &self,
        out: &Path,
        cond: &str,
        rep: usize,
        tag: &str,
        profile: Option<&[String]>,
        ncpu: i64,
    ) -> anyhow::Result<Value> {
        let dev = self.dev()?.clone();
        let ld = &self.s.load;
        let mut proc_child: Option<std::process::Child> = None;
        let mut meta = json!({});
        let mut trace_out = out.to_path_buf();
        let mut profiling = false;
        let stress = cond == "stress";
        let result = (|| -> anyhow::Result<()> {
            self.log(&format!(
                "STATUS: Setting up, please wait{}…",
                if stress { " (starting CPU load on all cores)" } else { "" }
            ));
            if stress {
                dev.start_stress(ncpu)?;
                std::thread::sleep(STRESS_SETTLE);
            }
            if let Some(procs) = profile {
                meta = self.start_profiler(procs, tag)?;
                profiling = true;
                trace_out = with_suffix(out, ".trace.fxt");
            }
            if ld.mode == "automated" && !ld.command.is_empty() {
                let child = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&ld.command)
                    .current_dir(dev.fuchsia_dir())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .process_group(0)
                    .spawn()
                    .context("could not start the load command")?;
                proc_child = Some(child);
                self.sleep(ld.warmup_s)?;
            } else {
                let instr = if ld.instructions.is_empty() {
                    "Exercise the task now.".to_string()
                } else {
                    ld.instructions[rep % ld.instructions.len()].clone()
                };
                (self.prompt)(&instr)?;
                (self.check_cancel)()?;
                self.log(&format!("STATUS: Starting in {:.0} s", ld.warmup_s));
                self.sleep(ld.warmup_s)?;
            }
            // ffx needs ~2 s before it records: say so, and start the countdown
            // when it actually records, so the countdown is the duration exactly.
            self.log("STATUS: Starting the trace, keep going");
            let d = ld.duration_s;
            let log = self.log.clone();
            let cancelled = || (self.check_cancel)().is_err();
            let mut on_start = || log(&format!("STATUS: Recording for {d:.0} s"));
            let (buf, cats) = (self.s.trace_buffer_mb, &self.s.categories);
            if let Err(e) = dev.trace(&trace_out, d, cats, buf, &mut on_start, &cancelled) {
                (self.check_cancel)()?;
                return Err(e);
            }
            self.log("STATUS: Done, you can stop.");
            if ld.mode != "automated" {
                // Non-blocking in the UI; printed in the CLI.
                (self.prompt)("Done, you can stop.")?;
            }
            Ok(())
        })();
        if result.is_err() && profiling {
            dev.profiler_abort();
        }
        if let Some(mut c) = proc_child {
            // Kill the whole group even if the shell already exited: a load
            // command can background children that outlive it. The shell has
            // not been waited for yet, so its pid (the group id) is not reused.
            proc::kill_group(c.id());
            let _ = c.kill();
            let _ = c.wait();
        }
        if stress {
            dev.stop_stress();
        }
        result?;
        if profiling {
            self.finish_profile(&trace_out, out)?;
        }
        Ok(meta)
    }

    /// Starts a background profile of the running processes named `procs`.
    fn start_profiler(&self, procs: &[String], tag: &str) -> anyhow::Result<Value> {
        let dev = self.dev()?;
        self.log("STATUS: Starting the CPU profiler…");
        let by_name = dev.process_pids(procs);
        let mut pids: Vec<i64> = by_name.values().flatten().copied().collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            return Err(ProfileUnavailable(format!(
                "none of {} is running on the device",
                procs.join(", ")
            ))
            .into());
        }
        let gone: Vec<&str> = procs
            .iter()
            .filter(|n| by_name.get(*n).is_none_or(|v| v.is_empty()))
            .map(String::as_str)
            .collect();
        if !gone.is_empty() {
            self.log(&format!("{tag}: not running now, not sampled: {}", gone.join(", ")));
        }
        if let Err(e) = dev.profiler_start(&pids, cpu_profile::PERIOD_US) {
            return Err(ProfileUnavailable(e.to_string()).into());
        }
        let running: Vec<&String> =
            procs.iter().filter(|n| by_name.get(*n).is_some_and(|v| !v.is_empty())).collect();
        Ok(json!({"processes": running, "pids": pids, "period_us": cpu_profile::PERIOD_US}))
    }

    /// Downloads the profile and writes trace + profile to `out`.
    pub fn finish_profile(&self, trace_out: &Path, out: &Path) -> anyhow::Result<()> {
        let dev = self.dev()?;
        self.log("STATUS: Downloading the CPU profile (about 30 s)…");
        let prof = with_suffix(out, ".profile.fxt");
        let raw = with_suffix(out, ".profile.raw.fxt");
        let result = (|| -> anyhow::Result<()> {
            if let Err(e) = dev.profiler_stop(&prof) {
                dev.profiler_abort();
                return Err(ProfileUnavailable(e.to_string()).into());
            }
            // FXT allows concatenation; the trace processor reads both as one trace.
            let mut dst = std::fs::File::create(out)?;
            for part in [trace_out, prof.as_path()] {
                let mut src = std::fs::File::open(part)?;
                std::io::copy(&mut src, &mut dst)?;
            }
            Ok(())
        })();
        for tmp in [trace_out, prof.as_path(), raw.as_path()] {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }

    // Analyzing the captures of a measurement.

    fn info(&self, name: &str) -> anyhow::Result<Value> {
        let p = self.mdir(name).join("measurement.json");
        Ok(serde_json::from_str(
            &std::fs::read_to_string(&p)
                .with_context(|| format!("could not read {}", p.display()))?,
        )?)
    }

    /// Analyzes every regular capture of a measurement; writes analysis.json.
    pub fn analyze(&self, name: &str, max_jobs: usize) -> anyhow::Result<IndexMap<String, Value>> {
        let info = self.info(name)?;
        let topo = Topology::from_measurement(&info)?;
        let window_ns = self.s.window_ns();
        let mut results: IndexMap<String, Value> = IndexMap::new();
        let total = progress_steps(&info);
        for (k, cap) in regular_captures(&info).iter().enumerate() {
            (self.check_cancel)()?;
            let tag = s(cap, "tag").to_string();
            self.log(&format!("PROGRESS: {k}/{total} Analyzing {tag}"));
            let log = self.log.clone();
            let tr = self.ctx.load_trace(Path::new(s(cap, "trace")), &|m| log(m))?;
            let spec = tasks::parse(&self.s.task)?;
            let res = match analysis::analyze(&tr, spec, Some(&topo), max_jobs, analysis::MAX_DEPTH)
            {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<NoJobs>().is_some() => {
                    let msg = e.to_string();
                    self.log(&format!("[{tag}] {msg}"));
                    results.insert(
                        tag,
                        json!({
                            "condition": cap["condition"], "trace": cap["trace"], "error": msg,
                            "response_ns": [], "buckets": {}, "threads": {}, "proposals": [],
                            "verdict": {"primary": "none", "headline": msg, "shares_tail": {}},
                            "system": system_metrics(&tr)?,
                        }),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            let opts = DeriveOptions::with_window(window_ns);
            let c = verdict::conclude(&res, &tr, &topo, &opts, verdict::TAIL_Q, &self.ctx.lessons);
            results.insert(tag, pack(&res, &tr, &c.verdict, &c.proposals, cap)?);
            tr.check()?;
        }
        write_atomic(
            &self.mdir(name).join("analysis.json"),
            serde_json::to_string_pretty(&json!(results))?.as_bytes(),
        )?;
        Ok(results)
    }

    /// The measurement's conclusion from its analyzed captures (`results`, as
    /// written by [`Runner::analyze`]). The UI summary and the command line
    /// both come from here.
    ///
    /// The report capture is the first stress capture that found jobs (else
    /// the first capture). Its proposals pool the work of the other repeats of
    /// its condition, and are raised to cover the capacity any capture whose
    /// verdict is throttled or scheduling needed. A candidate the gate finds
    /// improved on the baseline is judged by [`verdict::after_improvement`].
    /// Fails with [`NoJobs`] when the report capture has no jobs.
    pub fn conclude(
        &self,
        name: &str,
        results: &IndexMap<String, Value>,
    ) -> anyhow::Result<MeasurementConclusion> {
        let info = self.info(name)?;
        let regular = regular_captures(&info);
        let mut caps = regular.clone();
        caps.sort_by(|a, b| {
            (s(a, "condition") != "stress", s(a, "tag"))
                .cmp(&(s(b, "condition") != "stress", s(b, "tag")))
        });
        let ok: Vec<Value> = caps
            .iter()
            .filter(|c| results.get(s(c, "tag")).is_none_or(|v| v.get("error").is_none()))
            .cloned()
            .collect();
        let caps = if ok.is_empty() { caps } else { ok };
        let cap = caps.first().cloned().context("the measurement has no captures")?;
        let (tag, cond) = (s(&cap, "tag").to_string(), s(&cap, "condition").to_string());
        self.log(&format!(
            "PROGRESS: {}/{} Building the report from {tag}",
            regular.len(),
            progress_steps(&info)
        ));
        let log = self.log.clone();
        let tr = self.ctx.load_trace(Path::new(s(&cap, "trace")), &|m| log(m))?;
        let topo = Topology::from_measurement(&info)?;
        let window_ns = self.s.window_ns();
        let res = analysis::analyze(
            &tr,
            tasks::parse(&self.s.task)?,
            Some(&topo),
            analysis::MAX_JOBS,
            analysis::MAX_DEPTH,
        )?;
        let opts = pool_and_cover(results, &tag, &cond, window_ns);
        let mut conclusion =
            verdict::conclude(&res, &tr, &topo, &opts, verdict::TAIL_Q, &self.ctx.lessons);
        let vd = &conclusion.verdict;
        let report_md = report::render(&res, &tr, vd, &conclusion.shown, true, true);
        // Suggestions come from the roles the device loaded, not the checkout:
        // those are the roles the threads actually requested from.
        let rl = topo.role_params();
        let shown: Vec<ShownProposal> = conclusion
            .shown
            .iter()
            .map(|p| ShownProposal {
                roles: roles::suggest(&p.label, p.existing, &rl, 5),
                recommended: recommends(vd, p),
            })
            .collect();
        let sdir = Path::new(&self.s.dir);
        let mut vs_baseline = None;
        if name != "baseline" && sdir.join("baseline").join("analysis.json").exists() {
            if let Ok(g) = gate_files(sdir, "baseline", name) {
                if g.status == "improved" {
                    let v = &mut conclusion.verdict;
                    let recommends_any = shown.iter().any(|p| p.recommended);
                    let (primary, headline, recommend) = verdict::after_improvement(
                        &v.primary,
                        &v.headline,
                        v.recommend_profiles,
                        recommends_any,
                        &g.improvements,
                    );
                    (v.primary, v.headline, v.recommend_profiles) = (primary, headline, recommend);
                }
                vs_baseline = Some(g);
            }
        }
        tr.check()?;
        Ok(MeasurementConclusion {
            report_tag: tag,
            condition: cond,
            tr,
            res,
            conclusion,
            shown,
            report_md,
            vs_baseline,
        })
    }

    /// The function tables of a measurement's profile captures (also written
    /// to profile.json): condition -> run_functions() plus tag and processes.
    pub fn analyze_profiles(
        &self,
        name: &str,
        max_jobs: usize,
    ) -> anyhow::Result<IndexMap<String, Value>> {
        let info = self.info(name)?;
        let caps = profile_captures(&info);
        let mut out: IndexMap<String, Value> = IndexMap::new();
        let base = regular_captures(&info).len() + 1;
        let total = progress_steps(&info);
        let topo = Topology::from_measurement(&info).ok();
        for (k, cap) in caps.iter().enumerate() {
            (self.check_cancel)()?;
            let tag = s(cap, "tag");
            self.log(&format!(
                "PROGRESS: {}/{total} Finding where the run time goes in {tag}",
                base + k
            ));
            let path = Path::new(s(cap, "trace"));
            if !path.exists() {
                continue;
            }
            let log = self.log.clone();
            let tr = self.ctx.load_trace(path, &|m| log(m))?;
            let mut rf = match analysis::analyze(
                &tr,
                tasks::parse(&self.s.task)?,
                topo.as_ref(),
                max_jobs,
                analysis::MAX_DEPTH,
            ) {
                Ok(res) => {
                    let mut rf = cpu_profile::run_functions(&tr, &res, 0.9, 20)?;
                    if rf.get("samples").and_then(Value::as_i64).unwrap_or(0) == 0 {
                        rf.insert(
                            "error".into(),
                            json!("the profile has no samples on the chain's run time"),
                        );
                    }
                    rf
                }
                Err(e) if e.downcast_ref::<NoJobs>().is_some() => {
                    let mut m = Map::new();
                    m.insert("error".into(), json!(e.to_string()));
                    m.insert("samples".into(), json!(0));
                    m.insert("functions".into(), json!([]));
                    m
                }
                Err(e) => return Err(e),
            };
            rf.insert("tag".into(), json!(tag));
            rf.insert("processes".into(), cap.get("processes").cloned().unwrap_or(json!([])));
            out.insert(s(cap, "condition").to_string(), Value::Object(rf));
        }
        if !out.is_empty() {
            write_atomic(
                &self.mdir(name).join("profile.json"),
                serde_json::to_string_pretty(&json!(out))?.as_bytes(),
            )?;
        }
        Ok(out)
    }
}

/// Whether the verdict recommends applying `p`: it is one of the verdict's
/// culprits, and a change with a period (only that can be applied).
fn recommends(vd: &verdict::Verdict, p: &derive::ProfileProposal) -> bool {
    vd.recommend_profiles && vd.profile_utids.contains(&p.utid) && p.changed() && p.period_ns > 0
}

/// The report capture `tag`'s derive options: the work of the other repeats
/// of its condition `cond` is pooled, and the capacities that captures of
/// other conditions needed (when their verdict is throttled or scheduling)
/// are covered. Repeats of `cond` are already in the pool, so covering them
/// would undo the pooled bound.
fn pool_and_cover(
    results: &IndexMap<String, Value>,
    tag: &str,
    cond: &str,
    window_ns: Option<i64>,
) -> DeriveOptions {
    let mut opts = DeriveOptions::with_window(window_ns);
    for (t, v) in results {
        let primary = v.pointer("/verdict/primary").and_then(Value::as_str).unwrap_or("");
        let same_cond = s(v, "condition") == cond;
        let other_repeat = t.as_str() != tag && same_cond;
        let needs_cover = !same_cond && (primary == "throttled" || primary == "scheduling");
        for op in v.get("proposals").and_then(Value::as_array).into_iter().flatten() {
            let Some(period) = op.get("period_ns").and_then(Value::as_i64) else { continue };
            if other_repeat {
                let work = op.get("work_ns").and_then(Value::as_array).into_iter().flatten();
                opts.pooled_work
                    .entry((s(op, "label").to_string(), period))
                    .or_default()
                    .extend(work.filter_map(Value::as_f64));
            }
            if needs_cover {
                opts.cover.push(derive::Cover {
                    label: s(op, "label").to_string(),
                    period_ns: period,
                    capacity_ns: op.get("capacity_ns").and_then(Value::as_i64).unwrap_or(0),
                    capture: format!("{t} ({})", s(v, "condition")),
                });
            }
        }
    }
    opts
}

/// Appends `cap` to `info`'s "captures" list (made if missing).
fn push_capture(info: &mut Value, cap: Value) {
    match info.get_mut("captures").and_then(Value::as_array_mut) {
        Some(caps) => caps.push(cap),
        None => info["captures"] = json!([cap]),
    }
}

/// A measurement's conclusion (see [`Runner::conclude`]).
pub struct MeasurementConclusion {
    /// The capture the report is built from, and its condition.
    pub report_tag: String,
    pub condition: String,
    pub tr: Trace,
    pub res: analysis::AnalysisResult,
    /// The verdict (after the comparison with the baseline) and proposals.
    pub conclusion: verdict::Conclusion,
    /// Per entry of `conclusion.shown`: role suggestions, and whether the
    /// verdict recommends applying it. A candidate is only "fine" after the
    /// comparison when none is recommended, so these agree with the verdict.
    pub shown: Vec<ShownProposal>,
    /// The markdown report (the candidate's own verdict, before the
    /// comparison with the baseline).
    pub report_md: String,
    pub vs_baseline: Option<GateResult>,
}

impl std::fmt::Debug for MeasurementConclusion {
    /// `Trace` is not `Debug` (it holds every event), so it is left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeasurementConclusion")
            .field("report_tag", &self.report_tag)
            .field("condition", &self.condition)
            .field("res", &self.res)
            .field("conclusion", &self.conclusion)
            .field("shown", &self.shown)
            .field("report_md", &self.report_md)
            .field("vs_baseline", &self.vs_baseline)
            .finish_non_exhaustive()
    }
}

/// What the conclusion says about one shown proposal.
#[derive(Clone, Debug, PartialEq)]
pub struct ShownProposal {
    pub roles: Vec<roles::Suggestion>,
    pub recommended: bool,
}

fn with_suffix(out: &Path, suffix: &str) -> PathBuf {
    let s = out.display().to_string();
    PathBuf::from(format!("{}{suffix}", s.strip_suffix(".fxt").unwrap_or(&s)))
}

/// A thread's per-bucket arrays as JSON (bucket name -> ns per job).
pub fn buckets_json(m: &IndexMap<crate::attribution::Bucket, Vec<i64>>) -> Value {
    Value::Object(m.iter().map(|(b, v)| (b.as_str().to_string(), json!(v))).collect())
}

/// Leaf bucket shares as JSON.
pub fn shares_json(m: &IndexMap<crate::attribution::Bucket, f64>) -> Value {
    Value::Object(m.iter().map(|(b, v)| (b.as_str().to_string(), json!(v))).collect())
}

fn params_json(p: Option<(i64, i64)>) -> Value {
    match p {
        Some((a, b)) => json!([a, b]),
        None => Value::Null,
    }
}

/// Everything the gate needs, with raw per-job arrays for bootstrap tests.
fn pack(
    res: &analysis::AnalysisResult,
    tr: &Trace,
    vd: &verdict::Verdict,
    props: &[derive::ProfileProposal],
    cap: &Value,
) -> anyhow::Result<Value> {
    let mut threads = Map::new();
    for t in res.threads.values() {
        threads.insert(
            t.label.clone(),
            json!({"buckets": buckets_json(&t.per_job), "deadline_params": params_json(t.deadline_params)}),
        );
    }
    Ok(json!({
        "condition": cap["condition"],
        "trace": cap["trace"],
        "response_ns": res.response_ns,
        "buckets": buckets_json(&res.bucket_per_job),
        "verdict": {"primary": vd.primary, "headline": vd.headline, "shares_tail": shares_json(&vd.shares_tail)},
        "threads": threads,
        "proposals": props.iter().map(|p| json!({
            "label": p.label, "capacity_ns": p.capacity_ns, "period_ns": p.period_ns,
            "affinity": p.affinity, "profiles_json": p.profiles_json(), "existing": params_json(p.existing),
            "work_ns": p.work_ns.iter().map(|w| w.round() as i64).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "system": system_metrics(tr)?,
    }))
}

/// Global health counters used by the regression gate: each resident deadline
/// thread's runnable waits (W+R) and run time across the whole trace, and
/// per-CPU busy time.
pub fn system_metrics(tr: &Trace) -> anyhow::Result<Value> {
    let mut dl = Map::new();
    for r in tr.query(&queries::deadline_threads())?.iter() {
        let utid = r.get::<i64>(0);
        let waits: Vec<i64> = tr
            .thread_states(utid)
            .iter()
            .filter(|s| matches!(s.state, StateKind::Woken | StateKind::Preempted) && s.dur > 0)
            .map(|s| s.dur)
            .collect();
        dl.insert(
            tr.label(utid),
            json!({"run_ns": r.get::<Value>(1), "wait_ns": waits, "packed": r.get::<Value>(2)}),
        );
    }
    tr.check()?;
    let mut busy = Map::new();
    for r in tr.query(&queries::cpu_busy())?.iter() {
        busy.insert(r.get::<i64>(0).to_string(), r.get::<Value>(1));
    }
    Ok(json!({"deadline_threads": dl, "cpu_busy": busy}))
}

/// The A/B gate's decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    /// Improved somewhere and regressed nowhere.
    pub passed: bool,
    pub improvements: Vec<String>,
    pub regressions: Vec<String>,
    pub neutral: Vec<String>,
    /// improved | neutral | regressed
    pub status: String,
}

fn fmt0(x: f64) -> String {
    if x.is_nan() { "nan".into() } else { format!("{x:.0}") }
}

fn strip_tid(label: &str) -> &str {
    label.rsplit_once('[').map_or(label, |(a, _)| a)
}

/// Compares two analysis.json maps (tag -> packed) condition by condition.
///
/// Pass iff the target improves in at least one condition at p50/p90/p99 and
/// nothing regresses anywhere: target response, every collateral deadline
/// thread's wait, and (for automated command loads) the load's throughput.
/// On a manual load, fewer events per capture is just a smaller sample rather
/// than a throughput regression, as long as enough events were recorded.
pub fn gate(base: &Value, cand: &Value, min_rel: f64, manual_load: bool) -> GateResult {
    let mut imp = Vec::new();
    let mut reg = Vec::new();
    let mut neu = Vec::new();
    let by_cond = |d: &Value| -> IndexMap<String, Vec<Value>> {
        let mut m: IndexMap<String, Vec<Value>> = IndexMap::new();
        if let Some(o) = d.as_object() {
            for v in o.values() {
                m.entry(s(v, "condition").to_string()).or_default().push(v.clone());
            }
        }
        m
    };
    let (bc, cc) = (by_cond(base), by_cond(cand));
    let mut conds: Vec<&String> = bc.keys().chain(cc.keys()).collect();
    conds.sort();
    conds.dedup();
    let ints = |v: &Value, k: &str| -> Vec<f64> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default()
    };
    for cond in conds {
        let (Some(bv), Some(cv)) = (bc.get(cond), cc.get(cond)) else {
            neu.push(format!("{cond}: missing on one side"));
            continue;
        };
        let b_resp: Vec<f64> = bv.iter().flat_map(|v| ints(v, "response_ns")).collect();
        let c_resp: Vec<f64> = cv.iter().flat_map(|v| ints(v, "response_ns")).collect();
        let b_caps: Vec<Vec<f64>> = bv.iter().map(|v| ints(v, "response_ns")).collect();
        let c_caps: Vec<Vec<f64>> = cv.iter().map(|v| ints(v, "response_ns")).collect();
        // Relative change of quantile q from b to a in every capture pair; the
        // smallest decrease and the smallest increase.
        let pair_changes = |q: f64| -> (f64, f64) {
            let mut least_drop = f64::INFINITY;
            let mut least_rise = f64::INFINITY;
            // An empty capture has no quantile to compare: it is left out
            // here (automated loads flag it below), so it can neither make
            // nor mask a change in the other pairs.
            let nonempty = |caps: &[Vec<f64>]| {
                caps.iter().filter(|c| !c.is_empty()).cloned().collect::<Vec<_>>()
            };
            for b in &nonempty(&b_caps) {
                for c in &nonempty(&c_caps) {
                    let (bq, cq) = (stats::quantile(b, q), stats::quantile(c, q));
                    let rel = (cq - bq) / bq.max(1e-9);
                    least_drop = least_drop.min(-rel);
                    least_rise = least_rise.min(rel);
                }
            }
            (least_drop, least_rise)
        };
        // Latency is judged at quantiles (lesson 4.1); these match what zpo
        // reports.
        for q in [0.5, 0.9, 0.99] {
            let (bq, cq) = (stats::quantile(&b_resp, q), stats::quantile(&c_resp, q));
            let tag = format!(
                "{cond} target p{}: {} -> {} us",
                (q * 100.0) as i64,
                fmt0(bq / NS_PER_US_F64),
                fmt0(cq / NS_PER_US_F64)
            );
            // Besides the pooled bootstrap test, the change must hold in every
            // baseline/candidate capture pair: captures of one build differ
            // by more than a bootstrap within one capture shows.
            let (least_drop, least_rise) = pair_changes(q);
            if stats::improved(&b_resp, &c_resp, q, min_rel) && least_drop >= min_rel {
                imp.push(tag);
            } else if stats::regressed(&b_resp, &c_resp, q, min_rel) && least_rise >= min_rel {
                reg.push(tag);
            } else {
                neu.push(tag);
            }
        }
        // Throughput of an automated load (jobs per capture). On a manual load,
        // event count reflects how long the user interacted rather than system
        // throughput, so note drops >20% as neutral unless too few events were
        // recorded for bootstrap quantiles.
        //
        // Losing a fifth of the jobs is a throughput loss. Manual input varied
        // 0.79-0.99x between repeats in the recorded touch traces, so only automated
        // loads treat it as a regression. 30 samples is the usual floor for a
        // tail quantile to mean anything; manual event loads produce fewer
        // events, so they use 10 and report with less confidence.
        let jobs = |vs: &[Value]| {
            vs.iter().map(|v| ints(v, "response_ns").len()).sum::<usize>() as f64 / vs.len() as f64
        };
        let (bj, cj) = (jobs(bv), jobs(cv));
        if b_resp.is_empty() || c_resp.is_empty() {
            // Nothing to compare: an empty capture must not pass as "neutral".
            let side = if b_resp.is_empty() { "baseline" } else { "candidate" };
            reg.push(format!("{cond}: the {side} captured no jobs"));
        } else if manual_load {
            if c_resp.len() < 10 {
                reg.push(format!(
                    "{cond}: too few manual events recorded ({} total, vs. {bj:.0}/capture in baseline)",
                    c_resp.len()
                ));
            } else if cj < bj * 0.8 {
                neu.push(format!("{cond}: {cj:.0} vs. {bj:.0} events per capture (manual input)"));
            }
        } else if let Some(side) = [("baseline", &b_caps), ("candidate", &c_caps)]
            .iter()
            .find(|(_, caps)| caps.iter().any(Vec::is_empty))
            .map(|(side, _)| side)
        {
            // An automated load runs the same work every capture, so a capture
            // without jobs means the run broke, not a small sample.
            reg.push(format!("{cond}: a {side} capture recorded no jobs"));
        } else if cj < bj * 0.8 {
            reg.push(format!("{cond}: jobs per capture fell {bj:.0} -> {cj:.0} (load throughput)"));
        }
        // Collateral deadline threads (same name + same profile in both runs):
        // their wait must not blow their period.
        type Key = (String, Option<i64>);
        let collect = |vs: &[Value]| -> IndexMap<Key, Vec<f64>> {
            let mut m: IndexMap<Key, Vec<f64>> = IndexMap::new();
            for v in vs {
                if let Some(dt) = v.pointer("/system/deadline_threads").and_then(Value::as_object) {
                    for (lbl, mm) in dt {
                        let packed = mm.get("packed").and_then(Value::as_i64);
                        m.entry((strip_tid(lbl).to_string(), packed))
                            .or_default()
                            .extend(ints(mm, "wait_ns"));
                    }
                }
            }
            m
        };
        let (bt, ct) = (collect(bv), collect(cv));
        let mut keys: Vec<&Key> = bt.keys().filter(|k| ct.contains_key(*k)).collect();
        keys.sort();
        for key in keys {
            let (bw, cw) = (&bt[key], &ct[key]);
            if bw.len() < 30 || cw.len() < 30 {
                continue;
            }
            // Keys of deadline threads carry their packed parameters; 1 ms is
            // only a fallback for a key without them.
            let period_ns = key
                .1
                .filter(|&p| p != 0)
                .and_then(|p| crate::model::SchedulingParams::from_sched_priority(p).deadline())
                .map_or(FALLBACK_PERIOD_NS, |p| p.1);
            // The slack and the 20% rule: large enough that other threads would
            // notice the change. In the recorded touch traces, repeats of one build
            // stayed within it (p99 of comparisons: about 190us).
            let slack_ns = 200_000.max((0.25 * period_ns as f64) as i64) as f64;
            let (bq99, cq99) = (stats::quantile(bw, 0.99), stats::quantile(cw, 0.99));
            if cq99 - bq99 > slack_ns && stats::regressed(bw, cw, 0.99, 0.20) {
                reg.push(format!(
                    "{cond}: {} p99 runnable wait {} -> {} us",
                    key.0,
                    fmt0(bq99 / NS_PER_US_F64),
                    fmt0(cq99 / NS_PER_US_F64)
                ));
            }
        }
    }
    let status = if !reg.is_empty() {
        "regressed"
    } else if !imp.is_empty() {
        "improved"
    } else {
        "neutral"
    };
    GateResult {
        passed: !imp.is_empty() && reg.is_empty(),
        improvements: imp,
        regressions: reg,
        neutral: neu,
        status: status.into(),
    }
}

/// Runs the gate on two measurements of a session and writes
/// gate-<baseline>-vs-<candidate>.json.
pub fn gate_files(sdir: &Path, baseline: &str, candidate: &str) -> anyhow::Result<GateResult> {
    let read = |m: &str| -> anyhow::Result<Value> {
        let p = sdir.join(m).join("analysis.json");
        Ok(serde_json::from_str(
            &std::fs::read_to_string(&p)
                .with_context(|| format!("could not read {}", p.display()))?,
        )?)
    };
    let manual_load = Session::load(sdir).is_ok_and(|s| s.load.mode == "manual");
    // Smaller latency changes are too small to act on. Repeats of one build
    // moved up to 6% in the recorded touch traces, so the change must also hold in
    // every capture pair.
    let g = gate(&read(baseline)?, &read(candidate)?, 0.05, manual_load);
    write_atomic(
        &sdir.join(format!("gate-{baseline}-vs-{candidate}.json")),
        serde_json::to_string_pretty(&g)?.as_bytes(),
    )?;
    Ok(g)
}

/// Writes `path` through a uniquely named temporary file in the same
/// directory and a rename, so a reader (the UI polls these files) never sees
/// a half-written file and concurrent writers never share a temporary file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::testing;
    use std::sync::Mutex;

    #[test]
    fn profile_captures_stay_out_of_the_comparison() {
        let info = json!({"captures": [
            {"tag": "idle-1", "condition": "idle"},
            {"tag": "stress-1", "condition": "stress"},
            {"tag": "profile-idle-1", "condition": "idle", "profile": true},
        ]});
        let tags = |v: Vec<Value>| v.iter().map(|c| s(c, "tag").to_string()).collect::<Vec<_>>();
        assert_eq!(tags(regular_captures(&info)), vec!["idle-1", "stress-1"]);
        assert_eq!(tags(profile_captures(&info)), vec!["profile-idle-1"]);
        assert_eq!(progress_steps(&info), 4);
    }

    #[test]
    fn old_sessions_load_without_profile() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("session.json"),
            json!({"dir": "/elsewhere", "task": "thread:a/b", "load": {"mode": "manual"}})
                .to_string(),
        )
        .unwrap();
        let s = Session::load(d.path()).unwrap();
        assert!(!s.profile);
        assert!(s.stress);
        assert_eq!(s.repeats, 2);
        assert_eq!(s.dir, d.path().display().to_string());
        assert_eq!(s.load.duration_s, 15.0);
    }

    #[test]
    fn session_file_keeps_its_field_names_and_order() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Session::new(d.path(), "thread:a/b", LoadDef::manual());
        s.window = Some("850us".into());
        s.save().unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(s.path()).unwrap()).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "dir",
                "task",
                "load",
                "window",
                "stress",
                "repeats",
                "trace_buffer_mb",
                "categories",
                "profile"
            ]
        );
        assert_eq!(Session::load(d.path()).unwrap().window_ns(), Some(850_000));
    }

    struct FakeDev {
        dir: PathBuf,
        calls: Mutex<Vec<String>>,
        /// Whether its cpuctl predates core IDs.
        old_cpuctl: std::sync::atomic::AtomicBool,
    }

    impl DeviceOps for FakeDev {
        fn fuchsia_dir(&self) -> &Path {
            &self.dir
        }
        fn build_info(&self) -> anyhow::Result<device::BuildInfo> {
            Ok(IndexMap::new())
        }
        /// What the sample device's cpuctl and profiles files printed.
        fn topology(&self) -> anyhow::Result<Topology> {
            if self.old_cpuctl.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(testing::sample_without_ids());
            }
            Ok(testing::sample())
        }
        fn start_stress(&self, ncpu: i64) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("stress {ncpu}"));
            Ok(())
        }
        fn stop_stress(&self) {
            self.calls.lock().unwrap().push("unstress".into());
        }
        fn kill_stray_stress(&self) {
            self.calls.lock().unwrap().push("killall".into());
        }
        fn trace(
            &self,
            output: &Path,
            _d: f64,
            _c: &str,
            _b: i64,
            on_start: &mut dyn FnMut(),
            _cancelled: &dyn Fn() -> bool,
        ) -> anyhow::Result<PathBuf> {
            on_start();
            std::fs::write(output, b"TRACE")?;
            Ok(output.to_path_buf())
        }
        fn process_pids(&self, names: &[String]) -> IndexMap<String, Vec<i64>> {
            names.iter().map(|n| (n.clone(), vec![])).collect()
        }
        fn profiler_start(&self, _p: &[i64], _period: i64) -> anyhow::Result<()> {
            Ok(())
        }
        fn profiler_stop(&self, out: &Path) -> anyhow::Result<PathBuf> {
            std::fs::write(out, b"PROF")?;
            std::fs::write(with_suffix(out, ".raw.fxt"), b"RAW")?;
            Ok(out.to_path_buf())
        }
        fn profiler_abort(&self) {
            self.calls.lock().unwrap().push("abort".into());
        }
    }

    type TestRunner = (Runner, Arc<FakeDev>, Arc<Mutex<Vec<String>>>);

    fn runner(d: &Path, load: LoadDef) -> TestRunner {
        let dev = Arc::new(FakeDev {
            dir: d.to_path_buf(),
            calls: Mutex::new(vec![]),
            old_cpuctl: Default::default(),
        });
        let mut s = Session::new(d, "thread:a/b", load);
        s.repeats = 1;
        let ctx = Ctx {
            fuchsia_dir: d.to_path_buf(),
            sessions: Arc::new(Sessions::new(
                Arc::new(crate::proc::HostRunner),
                PathBuf::from("/nonexistent/tp"),
            )),
            ext_dirs: vec![],
            checkout: Default::default(),
            lessons: Default::default(),
        };
        let logs = Arc::new(Mutex::new(Vec::new()));
        let l = logs.clone();
        let prompts = logs.clone();
        let r = Runner::new(
            s,
            ctx,
            Some(dev.clone()),
            Arc::new(move |m| {
                prompts.lock().unwrap().push(format!("PROMPT {m}"));
                Ok(())
            }),
            Arc::new(move |m| l.lock().unwrap().push(m.to_string())),
            None,
        );
        (r, dev, logs)
    }

    #[test]
    fn finish_profile_merges_and_cleans_intermediates() {
        let d = tempfile::tempdir().unwrap();
        let (r, _dev, _logs) = runner(d.path(), LoadDef::manual());
        let trace_out = d.path().join("profile-idle-1.trace.fxt");
        let out = d.path().join("profile-idle-1.fxt");
        std::fs::write(&trace_out, b"TRACE-").unwrap();
        r.finish_profile(&trace_out, &out).unwrap();
        let names: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["profile-idle-1.fxt"]);
        assert_eq!(std::fs::read(&out).unwrap(), b"TRACE-PROF");
    }

    #[test]
    fn measure_records_each_condition_and_stops_the_stress() {
        let d = tempfile::tempdir().unwrap();
        let mut load = LoadDef::manual();
        load.warmup_s = 0.0;
        load.instructions = vec!["Swipe".into()];
        let (r, dev, logs) = runner(d.path(), load);
        let info = r.measure("baseline", "n").unwrap();
        let tags: Vec<&str> =
            info["captures"].as_array().unwrap().iter().map(|c| s(c, "tag")).collect();
        assert_eq!(tags, vec!["idle-1", "stress-1"]);
        assert!(d.path().join("baseline/measurement.json").exists());
        // The topology and roles the device reported are saved with it, and
        // the stress load is sized from them (two threads per CPU).
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("baseline/measurement.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(Topology::from_measurement(&saved).unwrap(), crate::topology::testing::sample());
        assert_eq!(*dev.calls.lock().unwrap(), vec!["killall", "stress 8", "unstress"]);
        let logs = logs.lock().unwrap();
        assert!(logs.contains(&"PROMPT Swipe".to_string()));
        assert!(logs.contains(&"STATUS: Recording for 15 s".to_string()));
        assert!(logs.contains(&"CAPTURE: stress-1 done".to_string()));
        assert!(!logs.iter().any(|l| l.starts_with("WARNING")));
    }

    #[test]
    fn measure_warns_and_continues_without_core_ids() {
        let d = tempfile::tempdir().unwrap();
        let mut load = LoadDef::manual();
        load.warmup_s = 0.0;
        let (r, dev, logs) = runner(d.path(), load);
        dev.old_cpuctl.store(true, std::sync::atomic::Ordering::Relaxed);
        r.measure("baseline", "").unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("baseline/measurement.json")).unwrap(),
        )
        .unwrap();
        let topo = Topology::from_measurement(&saved).unwrap();
        assert!(!topo.knows_cpus());
        // The stress load is still sized for every CPU.
        assert_eq!(*dev.calls.lock().unwrap(), vec!["killall", "stress 8", "unstress"]);
        let warning =
            format!("WARNING: {}. {}", topology::NO_CORE_IDS_TITLE, topology::NO_CORE_IDS_WARNING);
        assert!(logs.lock().unwrap().contains(&warning));
    }

    fn packed(cond: &str, resp: Vec<i64>) -> Value {
        json!({"condition": cond, "response_ns": resp, "system": {"deadline_threads": {}}})
    }

    #[test]
    fn gate_passes_only_without_regressions() {
        let base: Vec<i64> = (0..200).map(|i| 100_000 + (i % 7) * 1000).collect();
        let better: Vec<i64> = (0..200).map(|i| 60_000 + (i % 7) * 1000).collect();
        let g = gate(
            &json!({"idle-1": packed("idle", base.clone())}),
            &json!({"idle-1": packed("idle", better.clone())}),
            0.05,
            false,
        );
        assert!(g.passed, "{g:?}");
        assert_eq!(g.status, "improved");
        assert!(
            g.improvements[0].starts_with("idle target p50: 103 -> 63 us"),
            "{:?}",
            g.improvements
        );
        // Automated command load: fewer jobs per capture is a throughput regression.
        let g = gate(
            &json!({"idle-1": packed("idle", base.clone())}),
            &json!({"idle-1": packed("idle", better[..100].to_vec())}),
            0.05,
            false,
        );
        assert_eq!(g.status, "regressed");
        assert!(
            g.regressions.iter().any(|r| r.contains("jobs per capture fell 200 -> 100")),
            "{:?}",
            g.regressions
        );
        // Manual load: fewer events per capture is noted in neutral, not a regression,
        // unless the capture has too few events to compare.
        let g = gate(
            &json!({"idle-1": packed("idle", base.clone())}),
            &json!({"idle-1": packed("idle", better[..100].to_vec())}),
            0.05,
            true,
        );
        assert!(g.passed, "{g:?}");
        assert_eq!(g.status, "improved");
        assert_eq!(g.neutral, vec!["idle: 100 vs. 200 events per capture (manual input)"]);
        let g = gate(
            &json!({"idle-1": packed("idle", base.clone())}),
            &json!({"idle-1": packed("idle", better[..5].to_vec())}),
            0.05,
            true,
        );
        assert_eq!(g.status, "regressed");
        assert!(g.regressions.iter().any(|r| r.contains("too few manual events recorded")));
        let g = gate(
            &json!({"idle-1": packed("idle", base.clone())}),
            &json!({"stress-1": packed("stress", base)}),
            0.05,
            false,
        );
        assert_eq!(g.neutral, vec!["idle: missing on one side", "stress: missing on one side"]);
    }

    #[test]
    fn gate_changes_must_hold_in_every_capture_pair() {
        let caps = |base: i64| -> Vec<i64> { (0..200).map(|i| base + (i % 7) * 1000).collect() };
        let two = |a: i64, b: i64| json!({"idle-1": packed("idle", caps(a)), "idle-2": packed("idle", caps(b))});
        // Pooled, the candidate is 8% faster at p50, but against the first
        // baseline capture it is only 4% faster: not an improvement.
        let g = gate(&two(100_000, 110_000), &two(96_000, 96_000), 0.05, false);
        assert!(g.improvements.is_empty(), "{g:?}");
        assert_eq!(g.status, "neutral");
        // Likewise a slowdown that one pair does not show is not a regression.
        let g = gate(&two(96_000, 96_000), &two(100_000, 110_000), 0.05, false);
        assert!(g.regressions.is_empty(), "{g:?}");
        // A change every pair shows passes.
        let g = gate(&two(100_000, 110_000), &two(60_000, 62_000), 0.05, false);
        assert_eq!(g.status, "improved", "{g:?}");
        assert_eq!(g.improvements.len(), 3);
    }

    #[test]
    fn durations_must_be_finite() {
        let mut s = Session::new(Path::new("/s"), "thread:a/b", LoadDef::manual());
        assert!(s.problems().is_empty());
        s.load.duration_s = f64::INFINITY;
        s.load.warmup_s = -5.0;
        let fields: Vec<&str> = s.problems().iter().map(|p| p.0).collect();
        assert_eq!(fields, ["dur", "warmup"]);
        s.load.duration_s = 1.0;
        s.load.warmup_s = f64::NAN;
        assert_eq!(s.problems()[0].0, "warmup");
    }

    #[test]
    fn a_cancel_stops_the_warmup() {
        let d = tempfile::tempdir().unwrap();
        let mut load = LoadDef::manual();
        load.mode = "automated".into();
        load.command = "true".into();
        load.warmup_s = 30.0;
        let (mut r, dev, _logs) = runner(d.path(), load);
        // The check before the capture passes; the next one, in the warmup,
        // cancels.
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        r.check_cancel = Arc::new(move || {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(Cancelled.into())
            }
        });
        let t0 = std::time::Instant::now();
        let e = r.measure("baseline", "").unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert_eq!(*dev.calls.lock().unwrap(), vec!["killall"]);
    }

    #[test]
    fn only_other_conditions_are_covered() {
        let cap = |cond: &str, primary: &str, cap_ns: i64| {
            json!({"condition": cond, "verdict": {"primary": primary},
                   "proposals": [{"label": "p/t", "period_ns": 1_000_000, "capacity_ns": cap_ns,
                                  "work_ns": [1.0, 2.0]}]})
        };
        let results: IndexMap<String, Value> = [
            ("stress-1".to_string(), cap("stress", "throttled", 100_000)),
            ("stress-2".to_string(), cap("stress", "throttled", 300_000)),
            ("idle-1".to_string(), cap("idle", "scheduling", 200_000)),
        ]
        .into_iter()
        .collect();
        let o = pool_and_cover(&results, "stress-1", "stress", None);
        assert_eq!(o.pooled_work[&("p/t".to_string(), 1_000_000)], vec![1.0, 2.0]);
        let cover: Vec<(i64, &str)> =
            o.cover.iter().map(|c| (c.capacity_ns, c.capture.as_str())).collect();
        assert_eq!(cover, [(200_000, "idle-1 (idle)")]);
    }

    #[test]
    fn only_applicable_changes_are_recommended() {
        let vd = verdict::Verdict {
            primary: "throttled".into(),
            headline: String::new(),
            findings: vec![],
            recommend_profiles: true,
            profile_utids: vec![1],
            tail_q: verdict::TAIL_Q,
            shares_tail: IndexMap::new(),
            lessons_url: String::new(),
        };
        let p = derive::ProfileProposal {
            utid: 1,
            label: "p/t[1]".into(),
            period_ns: 1_000_000,
            capacity_ns: 200_000,
            affinity: vec![],
            utilization: 0.2,
            work_per_period_ns: IndexMap::new(),
            existing: Some((100_000, 1_000_000)),
            rate_source: String::new(),
            notes: vec![],
            work_ns: vec![],
        };
        assert!(recommends(&vd, &p));
        assert!(!recommends(
            &vd,
            &derive::ProfileProposal { existing: Some((200_000, 1_000_000)), ..p.clone() }
        ));
        assert!(!recommends(&vd, &derive::ProfileProposal { period_ns: 0, ..p.clone() }));
        assert!(!recommends(&vd, &derive::ProfileProposal { utid: 2, ..p }));
    }

    #[test]
    fn an_empty_capture_neither_hides_nor_fakes_a_change() {
        let caps = |base: i64| -> Vec<i64> { (0..200).map(|i| base + (i % 7) * 1000).collect() };
        let base = json!({"idle-1": packed("idle", caps(100_000)), "idle-2": packed("idle", caps(100_000))});
        let slower =
            json!({"idle-1": packed("idle", caps(150_000)), "idle-2": packed("idle", vec![])});
        // Manual load: the empty repeat is skipped and the slowdown still shows.
        let g = gate(&base, &slower, 0.05, true);
        assert!(g.regressions.iter().any(|r| r.starts_with("idle target p50")), "{g:?}");
        // Automated load: the empty capture itself is a regression.
        let g = gate(&base, &slower, 0.05, false);
        assert!(
            g.regressions.contains(&"idle: a candidate capture recorded no jobs".to_string()),
            "{g:?}"
        );
    }

    #[test]
    fn concurrent_atomic_writes_never_collide() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("session.json");
        let writers: Vec<_> = (0..4)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        write_atomic(&path, format!("{i}").as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 1);
    }
}
