// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Device interaction: ffx/fx wrappers for tracing, stress, OTA and
//! verification.
//!
//! Everything here shells out to the Fuchsia checkout's `ffx`/`fx`. The device
//! is whatever `ffx target default` points at.

use crate::proc::{self, Output};
use crate::topology::{self, Topology};
use anyhow::{Context, bail};
use regex_lite::Regex;
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// Most `fx shell` and `ffx` calls answer within seconds; a minute covers a
/// slow device or USB link.
const DEVICE_CMD: Duration = Duration::from_secs(60);
/// The build-info check runs before every measurement and must fail fast
/// when the device is not there.
const BUILD_INFO: Duration = Duration::from_secs(10);
/// A Bazel query over the product bundle may first load the build graph.
const BAZEL_QUERY: Duration = Duration::from_secs(600);
/// ffx prints "Tracing categories" after about 2 s of setup; if it never
/// does, the countdown starts after this long anyway.
const TRACE_START_FALLBACK: Duration = Duration::from_secs(10);
/// What a capture may take beyond its duration: ffx's setup and the trace's
/// download and symbolization.
const TRACE_DOWNLOAD: Duration = Duration::from_secs(300);
/// The tunnel ports the repository server rotates through (see
/// [`Device::ensure_repo_server`]): unprivileged, and few enough to stay clear
/// of ephemeral ports.
const REPO_PORT_BASE: u64 = 18000;
const REPO_PORT_COUNT: u64 = 900;
/// How often a running capture checks for output, cancel and its timeout.
const POLL: Duration = Duration::from_millis(100);
/// Attaching the profiler symbolizes nothing yet but can wait on ffx's
/// daemon.
const PROFILER_ATTACH: Duration = Duration::from_secs(120);
/// Stopping the profiler downloads and symbolizes every sample (about 30 s,
/// minutes on a large capture).
const PROFILER_STOP: Duration = Duration::from_secs(900);
/// How long loadgen gets to exit after its newline before it is killed.
const STRESS_STOP: Duration = Duration::from_secs(15);
/// Starting a repository server indexes the product bundle.
const REPO_SERVER_START: Duration = Duration::from_secs(120);
/// An OTA install downloads and writes the whole system image.
const INSTALL: Duration = Duration::from_secs(1800);
/// `ffx target reboot` returns once the device acknowledges.
const REBOOT_CMD: Duration = Duration::from_secs(120);
/// The device drops off the network a few seconds after a reboot; waiting
/// this long keeps `ffx target wait` from seeing it before it went down.
const REBOOT_SETTLE: Duration = Duration::from_secs(15);
/// `update wait-for-commit` waits for the new image to be marked healthy.
const COMMIT: Duration = Duration::from_secs(300);

/// Locks `m`. A panic while it was held leaves the data still usable here
/// (an optional child process), so poisoning is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A log sink for progress lines.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// Where the README explains how to set up the checkout and the device.
pub const PREREQUISITES_URL: &str = "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/zircon/skills/zpo/README.md#prerequisites";

/// The trace categories zpo records.
pub const DEFAULT_CATEGORIES: &str =
    "kernel:meta,kernel:sched,kernel:sched-demand,kernel:ipc,kernel:irq,kernel:power,input,gfx";

/// An expected failure that says what happened and what to try.
///
/// The UI shows it without a backtrace. `info` carries the same content in
/// parts so the UI can lay it out: title, reason, cause, fix_lead, fix
/// (commands), fix_join ("or" by default), help_url, output (lines),
/// output_label.
#[derive(Clone, Debug, PartialEq)]
pub struct UserFacingError {
    pub info: Map<String, Value>,
}

impl UserFacingError {
    pub fn new(info: Value) -> Self {
        UserFacingError { info: info.as_object().cloned().unwrap_or_default() }
    }
}

impl std::fmt::Display for UserFacingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&error_text(&self.info))
    }
}

impl std::error::Error for UserFacingError {}

fn s<'a>(info: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    info.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// Plain-text form of a [`UserFacingError`]'s info, for the CLI and logs.
pub fn error_text(info: &Map<String, Value>) -> String {
    let title = s(info, "title").unwrap_or("Error");
    let mut out = vec![match s(info, "reason") {
        Some(r) => format!("{title}: {r}"),
        None => format!("{title}."),
    }];
    if let Some(c) = s(info, "cause") {
        out.push(format!("Likely cause: {c}"));
    }
    if let Some(lead) = s(info, "fix_lead") {
        let fix: Vec<&str> = info
            .get("fix")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let join = s(info, "fix_join").unwrap_or("or");
        if fix.is_empty() {
            out.push(lead.to_string());
        } else {
            out.push(format!("{lead} {}", fix.join(&format!(" {join} "))));
        }
    }
    if let Some(u) = s(info, "help_url") {
        out.push(format!("See Prerequisites in the README: {u}"));
    }
    out.join("\n")
}

/// A command that exited non-zero.
#[derive(Clone, Debug)]
pub struct CommandFailed {
    pub code: Option<i32>,
    pub output: String,
    pub cmd: String,
}

impl std::fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} exited with {:?}:\n{}", self.cmd, self.code, self.output.trim())
    }
}

impl std::error::Error for CommandFailed {}

/// (reason, hint, lines): ffx's last error line, the `ffx log ...` command it
/// suggested, and its output without the generic progress banner or repeats.
pub fn ffx_summary(output: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let mut lines: Vec<String> = Vec::new();
    let mut skip = false;
    for raw in output.lines() {
        let l = raw.trim();
        if l.is_empty() || l == "Caused by:" {
            continue;
        }
        if l.starts_with("Until the update process") {
            skip = true;
        }
        if skip {
            skip = !l.starts_with("for more detail");
            continue;
        }
        if !lines.iter().any(|x| x == l) {
            lines.push(l.to_string());
        }
    }
    let is_error = |l: &str| {
        let l = l.to_lowercase();
        l.contains("fail") || l.contains("error")
    };
    let mut reason = lines.iter().rev().find(|l| is_error(l)).or(lines.last()).cloned();
    let mut hint = None;
    if let Some(r) = &reason {
        let re = Regex::new(r"\s*Inspect target logs with `([^`]+)` for details\.?")
            .expect("the pattern is a valid regex");
        if let Some((c, all)) = re.captures(r).and_then(|c| c.get(0).map(|all| (c, all))) {
            hint = Some(c[1].to_string());
            reason = Some(r[..all.start()].trim().to_string());
        }
    }
    (reason, hint, lines)
}

/// The string value of `key` in args.gn, from an assignment at the start of
/// a line, so a `# key = "..."` comment is never read.
fn args_gn_value<'a>(args_gn: &'a str, key: &str) -> Option<&'a str> {
    let re = Regex::new(&format!(r#"(?m)^\s*{}\s*=\s*"([^"]+)""#, regex_lite::escape(key))).ok()?;
    re.captures(args_gn).and_then(|c| c.get(1)).map(|m| m.as_str())
}

/// What a build directory builds, from its args.gn: the main product bundle,
/// else product.board.
pub fn describe_build_dir(args_gn: Option<&str>) -> String {
    let Some(a) = args_gn else { return "nothing (it has no args.gn)".into() };
    if let Some(pb) = args_gn_value(a, "main_pb_label") {
        return pb.to_string();
    }
    format!(
        "{}.{}",
        args_gn_value(a, "build_info_product").unwrap_or("?"),
        args_gn_value(a, "build_info_board").unwrap_or("?")
    )
}

/// Best guess, from args.gn, whether a build directory makes an image for a
/// board: the main product bundle's name or the build's board mentions the
/// board as a whole word (e.g. `myproduct_eng.myboard` for "myboard"). The
/// board is the one the device reports. Only drives a warning before
/// `fx build`.
pub fn looks_like_board(args_gn: Option<&str>, board: &str) -> bool {
    // An unknown board (the device did not answer) matches nothing.
    if board.is_empty() {
        return false;
    }
    let Some(a) = args_gn.filter(|a| !a.is_empty()) else { return false };
    let pb =
        args_gn_value(a, "main_pb_label").map(|m| m.rsplit(':').next().unwrap_or(m)).unwrap_or("");
    let brd = args_gn_value(a, "build_info_board").unwrap_or("");
    let pattern = format!(r"(^|[._-]){}($|[._-])", regex_lite::escape(board));
    Regex::new(&pattern).is_ok_and(|re| [pb, brd].iter().any(|n| re.is_match(n)))
}

/// The whole seconds `ffx trace start --duration` gets for `duration_s`: ffx
/// takes whole seconds, rounding up never records less than asked, and at
/// least 1 s, since `--duration 0` is not a timed trace. A non-finite
/// duration is 1 s.
fn trace_secs(duration_s: f64) -> u64 {
    if duration_s.is_finite() { duration_s.ceil().max(1.0) as u64 } else { 1 }
}

/// The result of [`Device::build_dir_check`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BuildDirCheck {
    pub dir: String,
    pub dir_rel: String,
    pub builds: String,
    pub board: String,
    pub looks_ok: bool,
    pub warning: Option<String>,
    pub help_url: String,
}

/// The device's build, from /config/build-info.
pub type BuildInfo = indexmap::IndexMap<String, String>;

/// The board config of a build's main product bundle, as a Bazel query.
fn board_query(main_pb_label: &str) -> String {
    format!("labels(board_config, labels(product_assembly, labels(main, {main_pb_label})))")
}

/// The query from a build's main product bundle to the roles files its board
/// installs: product bundle -> product assembly -> board config -> board input
/// bundles -> their thread roles. Board input bundles can sit on the board,
/// in its board input bundle sets, or, for a hybrid board, in the
/// replacements and the board it is based on (`board_configuration`).
pub fn roles_query(main_pb_label: &str) -> String {
    format!(
        "let b = {} in \
         let boards = $b + labels(board_configuration, $b) in \
         let sets = labels(board_input_bundle_sets, $boards) + \
         labels(replacement_board_input_bundle_sets, $b) in \
         labels(thread_roles, labels(board_input_bundles, $boards + $sets) + \
         labels(replacement_board_input_bundles, $b))",
        board_query(main_pb_label)
    )
}

/// [`roles_query`] for boards laid out some other way: every thread roles
/// attribute under the board config.
pub fn roles_fallback_query(main_pb_label: &str) -> String {
    format!("labels(thread_roles, deps({}))", board_query(main_pb_label))
}

/// A product bundle label from `product_bundles.json` without its GN
/// toolchain suffix (`//p:pb(//build/toolchain:x)` -> `//p:pb`).
fn strip_toolchain(label: &str) -> &str {
    label.split_once('(').map_or(label, |(l, _)| l)
}

/// Whether a `product_bundles.json` label names `label`, with or without a
/// toolchain suffix (so `//p:pb` never matches `//p:pb_eng`).
fn same_bundle(entry_label: &str, label: &str) -> bool {
    strip_toolchain(entry_label) == label
}

/// The checkout path of a Bazel label in the main repository
/// (`//a/b:c.profiles` -> `a/b/c.profiles`); None for other repositories.
pub fn label_path(label: &str) -> Option<String> {
    let l = label.trim().trim_start_matches('@').trim_start_matches('@');
    let rest = l.strip_prefix("//")?;
    let (pkg, name) = rest.split_once(':')?;
    Some(if pkg.is_empty() { name.to_string() } else { format!("{pkg}/{name}") })
}

/// The roles file to edit for `roles` among the board's roles files: the one
/// that already defines one of them, else the only file.
pub fn pick_roles_file(files: &[(String, String)], roles: &[&str]) -> anyhow::Result<String> {
    for (path, text) in files {
        if roles.iter().any(|r| crate::profiles_file::read_role(text, r).is_some()) {
            return Ok(path.clone());
        }
    }
    match files {
        [(only, _)] => Ok(only.clone()),
        [] => bail!("the build installs no roles file for the board"),
        _ => bail!(
            "the build installs several roles files ({}) and none defines {}; add the role to one \
             of them by hand",
            files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", "),
            roles.join(", ")
        ),
    }
}

/// An edit to a roles file (see [`Device::edit_roles`]).
#[derive(Clone, Debug, PartialEq)]
pub struct RolesEdit {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
    /// One line per role changed.
    pub log: Vec<String>,
}

/// What zpo prints, and the UI asks about, when applying a profile needs a
/// build for the device that the checkout does not have.
pub const NEEDS_BUILD: &str = "Applying needs a build of the image for the device (`fx set …`, \
                               `fx build`), which takes a while.";

/// ffx and fx of one checkout.
pub struct Device {
    pub fuchsia_dir: PathBuf,
    pub ffx: PathBuf,
    pub fx: PathBuf,
    log: Log,
    stress: Mutex<Option<Child>>,
}

impl std::fmt::Debug for Device {
    /// The log sink is a closure, which is not `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("fuchsia_dir", &self.fuchsia_dir)
            .field("ffx", &self.ffx)
            .field("fx", &self.fx)
            .field("stress", &self.stress)
            .finish_non_exhaustive()
    }
}

impl Drop for Device {
    /// Stops a stress load left running by a capture that failed or panicked,
    /// so `loadgen` does not keep loading the device through later captures.
    fn drop(&mut self) {
        if self.stress.get_mut().map(|s| s.is_some()).unwrap_or(false) {
            self.stop_stress();
        }
    }
}

/// A package repository as `pkgctl` names it: `fuchsia-pkg://` URLs are kept,
/// bare names become `fuchsia-pkg://<name>.fuchsia.com`.
fn repo_url(repo: &str) -> String {
    if repo.starts_with("fuchsia-pkg://") {
        repo.to_string()
    } else {
        format!("fuchsia-pkg://{repo}.fuchsia.com")
    }
}

impl Device {
    pub fn new(fuchsia_dir: PathBuf, log: Log) -> Self {
        let bin = fuchsia_dir.join(".jiri_root/bin");
        Device {
            ffx: bin.join("ffx"),
            fx: bin.join("fx"),
            fuchsia_dir,
            log,
            stress: Mutex::new(None),
        }
    }

    fn log(&self, m: &str) {
        (self.log)(m)
    }

    fn argv(&self, prog: &Path, args: &[&str]) -> Vec<String> {
        std::iter::once(prog.display().to_string())
            .chain(args.iter().map(|a| a.to_string()))
            .collect()
    }

    /// Runs a command in the checkout. Its stdout and stderr stay apart, so a
    /// warning on stderr never ends up in a value read from stdout.
    pub fn run(&self, cmd: &[String], check: bool, timeout: Duration) -> anyhow::Result<Output> {
        let Some(prog) = cmd.first() else {
            bail!("empty command");
        };
        self.log(&proc::display(cmd));
        let mut c = Command::new(prog);
        c.args(&cmd[1..]).current_dir(&self.fuchsia_dir);
        let out = proc::run_command(c, timeout).with_context(|| format!("could not run {prog}"))?;
        if check && !out.success() {
            return Err(CommandFailed {
                code: out.code,
                output: out.combined(),
                cmd: cmd.join(" "),
            }
            .into());
        }
        Ok(out)
    }

    /// Runs a command on the device with `fx shell`; returns what it printed.
    pub fn shell(&self, cmd: &str, timeout: Duration) -> String {
        self.run(&self.argv(&self.fx, &["shell", cmd]), false, timeout)
            .map(|o| o.stdout)
            .unwrap_or_default()
    }

    /// Waits for the device to come up.
    pub fn wait(&self, timeout_s: u64) -> anyhow::Result<()> {
        let t = timeout_s.to_string();
        self.run(
            &self.argv(&self.ffx, &["target", "wait", "-t", &t]),
            true,
            Duration::from_secs(timeout_s + 10),
        )?;
        Ok(())
    }

    /// The system image merkle the device runs ("" when it cannot be read).
    pub fn system_image_merkle(&self) -> String {
        let cmd = self.argv(&self.fx, &["shell", "read ver < /system/meta; echo $ver"]);
        self.run(&cmd, false, DEVICE_CMD)
            .ok()
            .filter(Output::success)
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default()
    }

    /// The device's CPU topology (`cpuctl describe`) and the roles it loaded
    /// (`/boot/config/profiles/`). `cpuctl` is in eng images, which zpo needs
    /// anyway for `fx shell` and `loadgen`.
    pub fn topology(&self) -> anyhow::Result<Topology> {
        let describe = self.cpuctl_describe()?;
        let profiles = self
            .run(&self.argv(&self.fx, &["shell", topology::PROFILES_DUMP_CMD]), true, DEVICE_CMD)
            .context("could not read /boot/config/profiles on the device")?
            .stdout;
        Topology::parse(&describe, &topology::split_profiles_dump(&profiles))
    }

    fn cpuctl_describe(&self) -> anyhow::Result<String> {
        Ok(self
            .run(&self.argv(&self.fx, &["shell", "cpuctl describe"]), true, DEVICE_CMD)
            .context("could not run cpuctl on the device")?
            .stdout)
    }

    /// Whether the device's `cpuctl describe` reports which CPUs are in each
    /// performance domain (see [`Topology::knows_cpus`]): the check before a
    /// measurement, without reading the profiles files.
    pub fn reports_core_ids(&self) -> anyhow::Result<bool> {
        Ok(Topology::parse(&self.cpuctl_describe()?, &[])?.knows_cpus())
    }

    /// The board the device reports ("" when it cannot be read).
    pub fn device_board(&self) -> String {
        self.build_info().ok().and_then(|b| b.get("board").cloned()).unwrap_or_default()
    }

    /// The build directory's main product bundle label: `main_pb_label` in
    /// args.gn, else the only bundle in `product_bundles.json` (single-product
    /// `fx set <product>.<board>` builds do not set `main_pb_label`).
    fn main_pb_label(out: &str) -> Option<String> {
        let args = Self::args_gn(out).unwrap_or_default();
        if let Some(pb) = args_gn_value(&args, "main_pb_label") {
            return Some(pb.to_string());
        }
        let text = std::fs::read_to_string(Path::new(out).join("product_bundles.json")).ok()?;
        match serde_json::from_str::<Value>(&text).ok()?.as_array()?.as_slice() {
            [only] => only.get("label").and_then(Value::as_str).map(|l| strip_toolchain(l).into()),
            _ => None,
        }
    }

    /// The roles files the build directory's board installs, as checkout
    /// paths, from a Bazel query over its main product bundle.
    pub fn roles_files(&self, out: &str) -> anyhow::Result<Vec<String>> {
        let Some(pb) = Self::main_pb_label(out) else {
            bail!("{} has no main product bundle (main_pb_label)", self.rel(out));
        };
        let query = |q: &str, keep_going: bool| -> anyhow::Result<Vec<String>> {
            let mut args = vec!["--dir", out, "bazel", "query"];
            if keep_going {
                args.push("--keep_going");
            }
            args.push(q);
            let o = self.run(&self.argv(&self.fx, &args), !keep_going, BAZEL_QUERY)?;
            // Labels are the lines that parse as main-repository labels.
            let mut files: Vec<String> = Vec::new();
            for f in o.stdout.lines().filter_map(label_path) {
                if !files.contains(&f) {
                    files.push(f);
                }
            }
            Ok(files)
        };
        let files = query(&roles_query(&pb), false)?;
        if !files.is_empty() {
            return Ok(files);
        }
        // `deps()` reaches targets that may not be loadable before the first
        // build, so it runs with --keep_going and keeps what it found.
        query(&roles_fallback_query(&pb), true)
    }

    /// The edit `changes` make to the board's roles file in the checkout,
    /// found through the build directory (see [`Device::roles_files`]).
    /// Nothing is written. The UI's Apply and `zpo profile-apply` /
    /// `profile-set` all edit through this.
    pub fn edit_roles(
        &self,
        changes: &[crate::profiles_file::RoleChange],
        default_affinity: &[i64],
        comment: Option<&str>,
    ) -> anyhow::Result<RolesEdit> {
        let out = self.build_dir();
        let mut files = Vec::new();
        for f in self.roles_files(&out)? {
            let text = std::fs::read_to_string(self.fuchsia_dir.join(&f))
                .with_context(|| format!("could not read {f}"))?;
            files.push((f, text));
        }
        let roles: Vec<&str> = changes.iter().map(|c| c.role.as_str()).collect();
        let rel = pick_roles_file(&files, &roles)?;
        let path = self.fuchsia_dir.join(&rel);
        let (old, new, log) =
            crate::profiles_file::apply_role_changes(&path, changes, default_affinity, comment)?;
        Ok(RolesEdit { path, old, new, log })
    }

    /// Product, board, version, latest-commit-date and system_image of the
    /// device.
    pub fn build_info(&self) -> anyhow::Result<BuildInfo> {
        let script = "echo __zpo_ok=1; \
            for k in product board version latest-commit-date; do \
            printf '%s=' \"$k\"; cat \"/config/build-info/$k\" 2>/dev/null; echo; done; \
            read ver < /system/meta 2>/dev/null; echo \"system_image=$ver\"";
        let out = self.run(&self.argv(&self.fx, &["shell", script]), false, BUILD_INFO)?;
        if !out.success() || !out.stdout.contains("__zpo_ok=1") {
            let all = out.combined();
            let msg = all.trim();
            bail!(
                "{}",
                if msg.is_empty() {
                    format!("fx shell exited with {:?}", out.code)
                } else {
                    msg.to_string()
                }
            );
        }
        let mut d: BuildInfo =
            ["product", "board", "version", "latest-commit-date", "system_image"]
                .iter()
                .map(|k| (k.to_string(), String::new()))
                .collect();
        for line in out.stdout.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if let Some(slot) = d.get_mut(k.trim()) {
                    *slot = v.trim().to_string();
                }
            }
        }
        Ok(d)
    }

    // Tracing: recording a trace on the device and pulling it to the host.

    /// Records `duration_s` of trace to `output`.
    ///
    /// `on_start` is called once when ffx starts tracing ("Tracing categories:
    /// ..."), i.e. after its ~2 s of setup, so a countdown can match the
    /// recording (or after 10 s if ffx never says so). `cancelled` is polled
    /// while recording; when it returns true the capture is stopped.
    pub fn trace(
        &self,
        output: &Path,
        duration_s: f64,
        categories: &str,
        buffer_mb: i64,
        on_start: &mut dyn FnMut(),
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<PathBuf> {
        match self.trace_once(output, duration_s, categories, buffer_mb, on_start, cancelled) {
            // A session left on the device by an interrupted run blocks a new
            // one: abort it and retry once.
            Err(e) if e.to_string().contains("Trace already started") => {
                self.trace_abort();
                self.trace_once(output, duration_s, categories, buffer_mb, on_start, cancelled)
            }
            r => r,
        }
    }

    /// Abandons the device's trace session.
    fn trace_abort(&self) {
        let cmd = self.argv(&self.ffx, &["trace", "stop", "--abort"]);
        let _ = self.run(&cmd, false, DEVICE_CMD);
    }

    fn trace_once(
        &self,
        output: &Path,
        duration_s: f64,
        categories: &str,
        buffer_mb: i64,
        on_start: &mut dyn FnMut(),
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<PathBuf> {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if output.exists() {
            let _ = std::fs::remove_file(output);
        }
        let secs = trace_secs(duration_s);
        let dur = secs.to_string();
        let buf = buffer_mb.to_string();
        let out_s = output.display().to_string();
        let cmd = self.argv(
            &self.ffx,
            &[
                "trace",
                "start",
                "--categories",
                categories,
                "--duration",
                &dur,
                "--buffer-size",
                &buf,
                "--output",
                &out_s,
            ],
        );
        self.log(&proc::display(&cmd));
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exec \"$@\" 2>&1")
            .arg("sh")
            .args(&cmd)
            .current_dir(&self.fuchsia_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .context("could not start ffx trace")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (tx, rx) = mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let t0 = Instant::now();
        let killer = Duration::from_secs(secs).saturating_add(TRACE_DOWNLOAD);
        let mut started = false;
        let mut stopped = None;
        let mut lines: Vec<String> = Vec::new();
        loop {
            match rx.recv_timeout(POLL) {
                Ok(line) => {
                    if line.starts_with("Tracing categories") && !started {
                        started = true;
                        on_start();
                    }
                    lines.push(line);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !started && t0.elapsed() >= TRACE_START_FALLBACK {
                started = true;
                on_start();
            }
            if stopped.is_none() {
                if t0.elapsed() >= killer {
                    stopped = Some("timed out");
                } else if cancelled() {
                    stopped = Some("cancelled");
                }
                if stopped.is_some() {
                    proc::kill_group(child.id());
                    let _ = child.kill();
                }
            }
        }
        let _ = reader.join();
        let status = child.wait()?;
        if let Some(why) = stopped {
            // Killing ffx leaves the device's session running, which blocks
            // the next capture.
            self.trace_abort();
            bail!("trace capture {why}");
        }
        let stdout = lines.join("\n");
        if stdout.contains("dropped records") {
            self.log("WARNING: trace providers dropped records; increase --buffer-mb");
        }
        if stdout.contains("FIDL ordinals not loaded") {
            self.log(
                "WARNING: ffx could not load the FIDL IR listed in the build directory's \
                 all_fidl_json.txt, so FIDL messages in this trace are not named (ipc:, ipc-in: and \
                 fidl: tasks will find no jobs); rebuild the build directory (`fx build`)",
            );
        }
        if !status.success() || !output.exists() {
            bail!("trace capture failed:\n{stdout}");
        }
        Ok(output.to_path_buf())
    }

    // CPU profiling: sampling call stacks alongside a trace.

    /// name -> pids of the device's running processes with those names.
    pub fn process_pids(&self, names: &[String]) -> indexmap::IndexMap<String, Vec<i64>> {
        crate::cpu_profile::parse_ps(&self.shell("ps", DEVICE_CMD), names)
    }

    /// Starts a background profile of `pids`; returns once sampling is live.
    pub fn profiler_start(&self, pids: &[i64], period_us: i64) -> anyhow::Result<()> {
        let period = period_us.to_string();
        let mut args =
            vec!["profiler", "attach", "--background", "--sample-period-us", period.as_str()];
        let pid_s: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
        for p in &pid_s {
            args.push("--pids");
            args.push(p);
        }
        let cmd = self.argv(&self.ffx, &args);
        let ok = |o: &Output| o.success() && o.combined().contains("Background session started");
        let mut out = self.run(&cmd, false, PROFILER_ATTACH)?;
        if !ok(&out) {
            // A session left over from an interrupted run blocks a new one:
            // abort it and retry once.
            self.profiler_abort();
            out = self.run(&cmd, false, PROFILER_ATTACH)?;
        }
        if !ok(&out) {
            bail!("ffx profiler attach failed:\n{}", out.combined().trim());
        }
        Ok(())
    }

    /// Stops the background profile and writes it, symbolized, as .fxt (~30 s).
    pub fn profiler_stop(&self, output: &Path) -> anyhow::Result<PathBuf> {
        // A file left from an earlier run must not pass for this profile.
        if output.exists() {
            std::fs::remove_file(output)?;
        }
        let o = output.display().to_string();
        let out = self.run(
            &self.argv(
                &self.ffx,
                &["profiler", "stop", "--output-format", "fxt", "--print-stats", "--output", &o],
            ),
            false,
            PROFILER_STOP,
        )?;
        if !out.success() || !output.exists() {
            bail!("ffx profiler stop failed:\n{}", out.combined().trim());
        }
        Ok(output.to_path_buf())
    }

    /// Abandons a background profile.
    pub fn profiler_abort(&self) {
        let _ =
            self.run(&self.argv(&self.ffx, &["profiler", "stop", "--abort"]), false, DEVICE_CMD);
    }

    // CPU stress: background load that competes with the task for CPUs.

    /// Runs the device's `loadgen` until [`Device::stop_stress`], sized for a
    /// board with `ncpu` CPUs.
    pub fn start_stress(&self, ncpu: i64) -> anyhow::Result<()> {
        if lock(&self.stress).is_some() {
            self.stop_stress();
        }
        let cmd = self.argv(&self.fx, &["shell", &loadgen_command(ncpu)]);
        self.log(&format!("{}  (background stress)", cmd.join(" ")));
        let child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .current_dir(&self.fuchsia_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own group, so a stop that times out kills all of it.
            .process_group(0)
            .spawn()
            .context("could not start loadgen")?;
        *lock(&self.stress) = Some(child);
        Ok(())
    }

    /// Stops the stress load.
    pub fn stop_stress(&self) {
        let Some(mut child) = lock(&self.stress).take() else { return };
        // A newline asks loadgen to exit. If the pipe is already closed,
        // the wait and the group kill below still clean up.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
        let stopped = proc::wait_timeout(&mut child, STRESS_STOP)
            .is_ok_and(|(status, timed_out)| status.is_some() && !timed_out);
        if !stopped {
            proc::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.kill_stray_stress();
    }

    /// Stops any `loadgen` on the device, including one left running by a
    /// zpo process that died during a stress capture.
    pub fn kill_stray_stress(&self) {
        self.shell("killall loadgen", DEVICE_CMD);
    }

    // Building a system image with new profiles and flashing it over the air.

    /// Runs `fx build`, writing its output to `log_path`.
    pub fn build(&self, log_path: &Path) -> anyhow::Result<()> {
        self.log(&format!("$ fx build  (log: {})", log_path.display()));
        if let Some(p) = log_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let f = std::fs::File::create(log_path)?;
        let mut child = Command::new(&self.fx)
            .arg("build")
            .current_dir(&self.fuchsia_dir)
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f)
            // Its own group, so the timeout kills ninja and Bazel with it.
            .process_group(0)
            .spawn()
            .context("could not run fx build")?;
        let (status, timed_out) = proc::wait_timeout(&mut child, Duration::from_secs(3 * 3600))?;
        if timed_out || !status.is_some_and(|s| s.success()) {
            bail!("fx build failed (see {})", log_path.display());
        }
        Ok(())
    }

    /// `fx get-build-dir` ("" when it fails).
    pub fn build_dir(&self) -> String {
        self.run(&self.argv(&self.fx, &["get-build-dir"]), false, DEVICE_CMD)
            .ok()
            .filter(Output::success)
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default()
    }

    /// `path` relative to the checkout when inside it, else itself ("none"
    /// when empty).
    fn rel(&self, path: &str) -> String {
        let root = format!("{}/", self.fuchsia_dir.display());
        match path.strip_prefix(&root) {
            Some(r) if !path.is_empty() => r.to_string(),
            _ if path.is_empty() => "none".into(),
            _ => path.to_string(),
        }
    }

    fn args_gn(dir: &str) -> Option<String> {
        if dir.is_empty() {
            return None;
        }
        std::fs::read_to_string(Path::new(dir).join("args.gn")).ok()
    }

    /// Whether the build directory seems to make an image for the device's
    /// board: a heuristic for a pre-build warning that the user can override,
    /// never a refusal.
    pub fn build_dir_check(&self) -> BuildDirCheck {
        self.build_dir_check_in(&self.build_dir(), &self.device_board())
    }

    /// [`Device::build_dir_check`] of a given directory.
    pub fn build_dir_check_in(&self, out: &str, board: &str) -> BuildDirCheck {
        let args_gn = Self::args_gn(out);
        let builds = describe_build_dir(args_gn.as_deref());
        let ok = looks_like_board(args_gn.as_deref(), board);
        let rel = self.rel(out);
        let warning = (!ok).then(|| {
            let what = if board.is_empty() {
                "zpo could not read the device's board to check it".to_string()
            } else {
                format!("which does not look like an image for the {board} board")
            };
            format!(
                "{rel} builds {builds}, {what}.\n\
                 To build for the device: fx use out/<dir>, or fx set ... (see {PREREQUISITES_URL})."
            )
        });
        BuildDirCheck {
            dir: out.to_string(),
            dir_rel: rel,
            builds,
            board: board.to_string(),
            looks_ok: ok,
            warning,
            help_url: PREREQUISITES_URL.into(),
        }
    }

    /// `problem` plus what the build directory builds and how to point it at
    /// the device's product.
    pub fn build_dir_error(&self, problem: &str, out: &str) -> UserFacingError {
        let args_gn = Self::args_gn(out);
        UserFacingError::new(json!({
            "title": "Nothing to flash",
            "reason": problem,
            "cause": format!(
                "{} builds {} (zpo builds and flashes whatever `fx get-build-dir` points at).",
                self.rel(out),
                describe_build_dir(args_gn.as_deref())
            ),
            "fix_lead": "Point the checkout at the device's build, then try again:",
            "fix": ["fx use out/<dir>", "fx set …"],
            "help_url": PREREQUISITES_URL,
        }))
    }

    /// The build directory's main product bundle.
    pub fn product_bundle_dir(&self) -> anyhow::Result<PathBuf> {
        self.product_bundle_dir_in(&self.build_dir())
    }

    fn product_bundle_dir_in(&self, out: &str) -> anyhow::Result<PathBuf> {
        let read = || -> std::io::Result<(Value, String)> {
            let entries = std::fs::read_to_string(Path::new(out).join("product_bundles.json"))?;
            let args = std::fs::read_to_string(Path::new(out).join("args.gn"))?;
            Ok((serde_json::from_str(&entries).unwrap_or(Value::Null), args))
        };
        let (entries, args_gn) = match read() {
            Ok(v) => v,
            Err(e) => {
                let why = match e.kind() {
                    std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                    _ => e.to_string(),
                };
                return Err(self
                    .build_dir_error(
                        &format!("the build directory has no product bundles ({why})."),
                        out,
                    )
                    .into());
            }
        };
        let label = args_gn_value(&args_gn, "main_pb_label");
        let list = entries.as_array().cloned().unwrap_or_default();
        for e in &list {
            if let (Some(l), Some(p)) = (label, e.get("path").and_then(Value::as_str)) {
                if same_bundle(e.get("label").and_then(Value::as_str).unwrap_or(""), l) {
                    return Ok(Path::new(out).join(p));
                }
            }
        }
        let first = list.first().and_then(|e| e.get("path")).and_then(Value::as_str);
        match first {
            Some(p) => Ok(Path::new(out).join(p)),
            None => {
                Err(self.build_dir_error("the build directory has no product bundles.", out).into())
            }
        }
    }

    /// The system image merkle in a product bundle's update repository.
    pub fn pb_system_image_merkle(pb_dir: &Path) -> anyhow::Result<String> {
        let t: Value = serde_json::from_str(&std::fs::read_to_string(
            pb_dir.join("repository/targets.json"),
        )?)?;
        if let Some(targets) = t.pointer("/signed/targets").and_then(Value::as_object) {
            for (k, v) in targets {
                if k.starts_with("system_image/") {
                    if let Some(m) = v.pointer("/custom/merkle").and_then(Value::as_str) {
                        return Ok(m.to_string());
                    }
                }
            }
        }
        bail!("no system_image in product bundle targets.json")
    }

    /// Makes sure a repository server on a fresh tunnel port serves `pb_dir`.
    pub fn ensure_repo_server(&self, pb_dir: &Path, name: &str) -> anyhow::Result<()> {
        let servers = || -> Vec<Vec<String>> {
            self.run(&self.argv(&self.ffx, &["repository", "server", "list"]), false, DEVICE_CMD)
                .map(|o| {
                    o.stdout
                        .lines()
                        .map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
                        .filter(|c| c.len() >= 3)
                        .collect()
                })
                .unwrap_or_default()
        };
        // Stop any previous zpo repository server and use a fresh tunnel port
        // so the target's TIME_WAIT never hits EADDRINUSE.
        let prefix = format!("{name}.");
        for cols in servers() {
            if cols[0] != name && !cols[0].starts_with(&prefix) {
                continue;
            }
            let _ = self.run(
                &self.argv(&self.ffx, &["repository", "server", "stop", &cols[0]]),
                false,
                DEVICE_CMD,
            );
            let _ = self.run(
                &self.argv(&self.ffx, &["repository", "server", "stop", name]),
                false,
                DEVICE_CMD,
            );
        }
        // Only zpo's own previous registration is removed. Another repository
        // serving the same packages needs no cleanup: `ffx repository server
        // start` replaces alias conflicts by default, so registering with the
        // fuchsia.com alias points the device's rewrite rules at zpo's repo.
        self.shell(&format!("pkgctl repo rm {}", proc::shell_quote(&repo_url(name))), DEVICE_CMD);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let port = REPO_PORT_BASE + (secs % REPO_PORT_COUNT);
        let addr = format!("127.0.0.1:{port}");
        let pb = pb_dir.display().to_string();
        let out = self.run(
            &self.argv(
                &self.ffx,
                &[
                    "repository",
                    "server",
                    "start",
                    "--background",
                    "--address",
                    &addr,
                    "--tunnel-addr",
                    &addr,
                    "--product-bundle",
                    &pb,
                    "-r",
                    name,
                    "--alias",
                    "fuchsia.com",
                    "--alias",
                    "chromium.org",
                ],
            ),
            false,
            REPO_SERVER_START,
        )?;
        if out.success() {
            return Ok(());
        }
        // A server for the same bundle may still be registered; that is fine.
        for cols in servers() {
            if cols[1].ends_with(&format!(":{port}"))
                && cols[2].trim_end_matches('/').starts_with(pb.trim_end_matches('/'))
            {
                self.log(&format!("using existing repository server {} on {}", cols[0], cols[1]));
                return Ok(());
            }
        }
        bail!("could not start repository server:\n{}", out.combined())
    }

    // The profiles the device actually loaded (from bootfs).

    /// /boot/config/profiles/<name> on the device: role -> normalized params.
    pub fn device_profiles(&self, profiles_file: &str) -> Option<Map<String, Value>> {
        let name = Path::new(profiles_file).file_name()?.to_string_lossy().into_owned();
        let path = format!("/boot/config/profiles/{name}");
        let txt = self.shell(&format!("cat {}", proc::shell_quote(&path)), DEVICE_CMD);
        // The installed file can keep the checkout's comments and trailing
        // commas, so read it the way the checkout's copy is read.
        let roles = crate::roles::parse_roles_full(&txt);
        if roles.is_empty() && crate::profiles_file::profiles_span(&txt).is_none() {
            return None;
        }
        Some(
            roles
                .iter()
                .map(|(k, v)| {
                    (k.clone(), crate::roles::normalize(&crate::roles::role_text_json(v)))
                })
                .collect(),
        )
    }

    /// The checkout's profiles file: role -> normalized params.
    pub fn local_profiles(&self, profiles_file: &str) -> anyhow::Result<Map<String, Value>> {
        let text = std::fs::read_to_string(self.fuchsia_dir.join(profiles_file))?;
        Ok(crate::roles::parse_roles_full(&text)
            .iter()
            .map(|(k, v)| (k.clone(), crate::roles::normalize(&crate::roles::role_text_json(v))))
            .collect())
    }

    /// Whether the device runs the checkout's profiles.
    pub fn profiles_in_sync(&self, profiles_file: &str) -> bool {
        match (self.device_profiles(profiles_file), self.local_profiles(profiles_file)) {
            (Some(d), Ok(l)) => d == l,
            _ => false,
        }
    }

    /// The error for a failed `ffx target update force-install`.
    ///
    /// zpo cannot tell why an install failed (cable, tunnel, device storage,
    /// wrong image...), so it only points at the build directory when its own
    /// pre-build check also says the build does not look like an image for the
    /// board. Otherwise the advice is neutral.
    pub fn install_error(
        &self,
        e: &CommandFailed,
        check: Option<&BuildDirCheck>,
    ) -> UserFacingError {
        let (reason, hint, lines) = ffx_summary(&e.output);
        let output: Vec<String> = if lines.is_empty() {
            vec![format!("(no output; exit status {})", e.code.unwrap_or(-1))]
        } else {
            lines
        };
        let mut m = Map::new();
        m.insert("title".into(), json!("Couldn't install the update on the device"));
        m.insert("reason".into(), json!(reason));
        m.insert("output".into(), json!(output));
        m.insert("output_label".into(), json!("What ffx said"));
        match check {
            Some(c) if !c.looks_ok && !c.board.is_empty() => {
                m.insert(
                    "cause".into(),
                    json!(format!(
                        "{} builds {}, which doesn't look like an image for the {} board.",
                        c.dir_rel, c.builds, c.board
                    )),
                );
                m.insert(
                    "fix_lead".into(),
                    json!("Point the checkout at the device's build, then try again:"),
                );
                m.insert("fix".into(), json!(["fx use out/<dir>", "fx set …"]));
                m.insert("help_url".into(), json!(PREREQUISITES_URL));
            }
            _ => {
                m.insert("fix_join".into(), json!("and"));
                let mut fix = vec!["ffx target list".to_string()];
                fix.extend(hint.clone());
                m.insert("fix".into(), json!(fix));
                m.insert(
                    "fix_lead".into(),
                    json!(if hint.is_some() {
                        "Check that the device shows up and what the updater logged, then try again:"
                    } else {
                        "Check that the device shows up, then try again:"
                    }),
                );
            }
        }
        UserFacingError::new(Value::Object(m))
    }

    /// Pushes the current build through a tunneled repository server, reboots
    /// and verifies.
    ///
    /// "Up to date" means both the system_image merkle matches the product
    /// bundle and the device's bootfs copies of the board's roles files match
    /// the checkout (profiles live in the ZBI, not in system_image).
    pub fn ota(&self, reboot_timeout_s: u64) -> anyhow::Result<Value> {
        let out = self.build_dir();
        let board = self.device_board();
        let roles_files = self.roles_files(&out).unwrap_or_else(|e| {
            self.log(&format!("not checking the roles files: {e:#}"));
            Vec::new()
        });
        let profiles_ok = || roles_files.iter().all(|f| self.profiles_in_sync(f));
        let pb_dir = self.product_bundle_dir_in(&out)?;
        let want = match Self::pb_system_image_merkle(&pb_dir) {
            Ok(w) => w,
            Err(e) => {
                let why = match e.downcast_ref::<std::io::Error>() {
                    Some(io) if io.kind() == std::io::ErrorKind::NotFound => {
                        "No such file or directory".to_string()
                    }
                    _ => e.to_string(),
                };
                return Err(self
                    .build_dir_error(
                        &format!("the product bundle has no update repository ({why})."),
                        &out,
                    )
                    .into());
            }
        };
        let have = self.system_image_merkle();
        // Profiles live in the ZBI, not in system_image, so a matching merkle
        // proves nothing about them: skip only when the roles files are known
        // and match.
        let prof_ok = !roles_files.is_empty() && profiles_ok();
        if have == want && prof_ok {
            self.log(&format!(
                "device already on system_image {} with matching profiles; skipping OTA",
                &want[..want.len().min(12)]
            ));
            return Ok(json!({"updated": false, "merkle": want}));
        }
        self.log("STATUS: Pushing update to device (OTA)");
        self.log("PROGRESS: 2/4 Pushing update package to device");
        self.ensure_repo_server(&pb_dir, "zpo-pb")?;
        let install = self.run(
            &self.argv(
                &self.ffx,
                &[
                    "target",
                    "update",
                    "force-install",
                    "--reboot",
                    "false",
                    "fuchsia-pkg://fuchsia.com/update",
                ],
            ),
            true,
            INSTALL,
        );
        if let Err(e) = install {
            if let Some(cf) = e.downcast_ref::<CommandFailed>() {
                let check = self.build_dir_check_in(&out, &board);
                return Err(self.install_error(cf, Some(&check)).into());
            }
            return Err(e);
        }
        self.log("STATUS: Waiting for the device to reconnect");
        self.log("PROGRESS: 3/4 Waiting for the device to reconnect");
        let _ = self.run(
            &self.argv(&self.ffx, &["target", "reboot", "--reason", "system-update"]),
            false,
            REBOOT_CMD,
        );
        std::thread::sleep(REBOOT_SETTLE);
        self.wait(reboot_timeout_s)?;
        self.log("STATUS: Verifying profiles on device");
        self.run(&self.argv(&self.fx, &["shell", "update", "wait-for-commit"]), true, COMMIT)
            .context("OTA commit failed")?;
        let now = self.system_image_merkle();
        if now != want {
            bail!("OTA verification failed: device {now} != product bundle {want}");
        }
        if !profiles_ok() {
            bail!(
                "OTA verification failed: /boot/config/profiles on the device does not match the checkout"
            );
        }
        self.log(&format!(
            "OTA ok: system_image {}{}",
            &want[..want.len().min(12)],
            if roles_files.is_empty() { "" } else { ", profiles verified on device" }
        ));
        Ok(json!({"updated": true, "merkle": want}))
    }
}

/// The `loadgen` command line for a device with `ncpu` CPUs (threads, then
/// min/max work ms, then min/max sleep ms). Two always-runnable fair threads
/// per CPU. 10-20ms bursts outlast Zircon's 10ms fair period, and 0.1-0.3ms
/// sleeps force frequent wake-time placement.
fn loadgen_command(ncpu: i64) -> String {
    format!("loadgen {} 10 20 0.1 0.3", 2 * ncpu.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadgen_runs_two_threads_per_cpu() {
        assert_eq!(loadgen_command(8), "loadgen 16 10 20 0.1 0.3");
        assert_eq!(loadgen_command(0), "loadgen 2 10 20 0.1 0.3");
    }

    const RISCV_ARGS: &str = "build_info_board = \"riscv64\"\nbuild_info_product = \"bringup\"\n";

    const FFX_OUT: &str = "Until the update process is improved to have more granular reporting, try using
    ffx inspect show 'bootstrap/pkg-cache'
for more detail on the progress of update-related downloads.
Starting install
  0.00% Preparing
Install failed prepare: Internal. Inspect target logs with `ffx log --filter system-updater` for details.
Caused by:
    Install failed prepare: Internal. Inspect target logs with `ffx log --filter system-updater` for details.
";

    fn quiet() -> Log {
        Arc::new(|_| {})
    }

    #[test]
    fn roles_files_come_from_the_product_bundle() {
        let q = roles_query("//p:pb");
        assert!(
            q.starts_with(
                "let b = labels(board_config, labels(product_assembly, labels(main, //p:pb))) in "
            ),
            "{q}"
        );
        for attr in [
            "board_configuration",
            "board_input_bundle_sets",
            "replacement_board_input_bundle_sets",
            "replacement_board_input_bundles",
        ] {
            assert!(q.contains(&format!("labels({attr}, ")), "{attr}: {q}");
        }
        assert_eq!(
            roles_fallback_query("//p:pb"),
            "labels(thread_roles, deps(labels(board_config, labels(product_assembly, \
             labels(main, //p:pb)))))"
        );
        assert_eq!(
            label_path("//vendor/acme/profiles:board.profiles").as_deref(),
            Some("vendor/acme/profiles/board.profiles")
        );
        assert_eq!(label_path("@//a:b").as_deref(), Some("a/b"));
        assert_eq!(label_path("@other//a:b"), None);
        assert_eq!(label_path("Loading: 0 packages loaded"), None);
        let files = vec![
            ("a.profiles".to_string(), r#"{"profiles": {"x.a": {"priority": 1}}}"#.to_string()),
            ("b.profiles".to_string(), r#"{"profiles": {"x.b": {"priority": 1}}}"#.to_string()),
        ];
        assert_eq!(pick_roles_file(&files, &["x.b"]).unwrap(), "b.profiles");
        assert!(pick_roles_file(&files, &["x.new"]).unwrap_err().to_string().contains("several"));
        assert_eq!(pick_roles_file(&files[..1], &["x.new"]).unwrap(), "a.profiles");
        assert!(pick_roles_file(&[], &["x"]).is_err());
    }

    #[test]
    fn looks_like_board_is_a_name_match_not_a_recipe() {
        assert!(!looks_like_board(Some(RISCV_ARGS), "pad"));
        assert!(!looks_like_board(None, "pad"));
        assert!(looks_like_board(
            Some("main_pb_label = \"//vendor/acme/products/handset:handset_eng.pad\"\n"),
            "pad"
        ));
        assert!(looks_like_board(
            Some("build_info_board = \"pad\"\nbuild_info_product = \"anything\"\n"),
            "pad"
        ));
        assert!(!looks_like_board(Some("main_pb_label = \"//x:notepad_demo\"\n"), "pad")); // whole words only
        // An unreadable board matches nothing, even with one name unset.
        assert!(!looks_like_board(Some("build_info_board = \"pad\"\n"), ""));
        assert!(!looks_like_board(Some(RISCV_ARGS), ""));
    }

    #[test]
    fn build_dir_check_warns_when_the_board_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out/pad");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("args.gn"), "build_info_board = \"pad\"\n").unwrap();
        let dev = Device::new(d.path().to_path_buf(), quiet());
        let out = out.display().to_string();
        assert!(dev.build_dir_check_in(&out, "pad").looks_ok);
        let c = dev.build_dir_check_in(&out, "");
        assert!(!c.looks_ok);
        let w = c.warning.unwrap();
        assert!(w.contains("zpo could not read the device's board"), "{w}");
    }

    #[test]
    fn trace_duration_is_whole_seconds_and_never_panics() {
        assert_eq!(trace_secs(0.2), 1);
        assert_eq!(trace_secs(2.5), 3);
        assert_eq!(trace_secs(10.0), 10);
        for bad in [-5.0, 0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(trace_secs(bad), 1, "{bad}");
        }
    }

    #[test]
    fn install_failure_only_blames_the_build_dir_when_it_looks_wrong() {
        let dev = Device::new("/f".into(), quiet());
        let err = CommandFailed { code: Some(1), output: FFX_OUT.into(), cmd: "ffx".into() };
        let looks = BuildDirCheck {
            dir: "/f/out/core.x64-balanced".into(),
            dir_rel: "out/core.x64-balanced".into(),
            builds: "core.x64".into(),
            board: "pad".into(),
            looks_ok: false,
            warning: None,
            help_url: PREREQUISITES_URL.into(),
        };
        let wrong = dev.install_error(&err, Some(&looks)).info;
        let fine = BuildDirCheck { looks_ok: true, ..looks.clone() };
        let right = dev.install_error(&err, Some(&fine)).info;
        for info in [&wrong, &right] {
            assert_eq!(info["reason"], "Install failed prepare: Internal."); // ffx's own reason
        }
        assert!(wrong["cause"].as_str().unwrap().contains("builds core.x64"));
        assert_eq!(wrong["help_url"], PREREQUISITES_URL);
        assert!(right.get("cause").is_none()); // no fx set chase when the build looks fine
        assert!(right.get("help_url").is_none());
        assert_eq!(right["fix"], json!(["ffx target list", "ffx log --filter system-updater"]));
        // When the device's board could not be read, the advice is neutral.
        let unknown = BuildDirCheck { board: String::new(), ..looks.clone() };
        let info = dev.install_error(&err, Some(&unknown)).info;
        assert!(info.get("cause").is_none(), "{info:?}");
        assert_eq!(info["fix"], right["fix"]);
    }

    #[test]
    fn ffx_summary_drops_the_banner_and_repeats() {
        let (reason, hint, lines) = ffx_summary(FFX_OUT);
        assert_eq!(reason.as_deref(), Some("Install failed prepare: Internal."));
        assert_eq!(hint.as_deref(), Some("ffx log --filter system-updater"));
        assert_eq!(lines[0], "Starting install");
        assert_eq!(lines.len(), 3);
        assert_eq!(ffx_summary(""), (None, None, vec![]));
    }

    #[test]
    fn trace_reports_when_ffx_starts_recording() {
        let d = tempfile::tempdir().unwrap();
        let ffx = d.path().join("ffx");
        // Setup, then the line ffx prints right before it records, then the trace.
        crate::checkout::testing::write_test_script(
            &ffx,
            "#!/bin/sh\nsleep 0.3\necho \"Tracing categories: [kernel:sched]...\"\nsleep 0.3\n\
             echo \"Trace completed! Copying trace from device...\"\neval out=\\${$#}\ntouch \"$out\"\n",
        );
        let mut dev = Device::new(d.path().to_path_buf(), quiet());
        dev.ffx = ffx;
        let out = d.path().join("t.fxt");
        let t0 = Instant::now();
        let mut at: Vec<f64> = Vec::new();
        let got = dev
            .trace(
                &out,
                1.0,
                DEFAULT_CATEGORIES,
                512,
                &mut || at.push(t0.elapsed().as_secs_f64()),
                &|| false,
            )
            .unwrap();
        assert_eq!(got, out);
        assert_eq!(at.len(), 1); // once, when ffx says it is tracing
        assert!(at[0] > 0.25, "{at:?}");

        // A subsequent failed run does not mistake the previous output file for success.
        crate::checkout::testing::write_test_script(
            &dev.ffx,
            "#!/bin/sh\necho 'target offline'\nexit 1\n",
        );
        let err = dev.trace(&out, 1.0, DEFAULT_CATEGORIES, 512, &mut || {}, &|| false).unwrap_err();
        assert!(err.to_string().contains("target offline"), "{err}");
    }

    /// A fake `ffx` or `fx` at `dir/name` that appends its arguments to
    /// `dir/name.log` and then runs `body`. `write_test_script` waits out the
    /// ETXTBSY race of running a file just written.
    fn fake(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let script = format!("#!/bin/sh\necho \"$*\" >> \"$0.log\"\n{body}\n");
        crate::checkout::testing::write_test_script(&p, &script);
        p
    }

    fn calls(tool: &Path) -> String {
        std::fs::read_to_string(tool.with_extension("log")).unwrap_or_default()
    }

    #[test]
    fn trace_aborts_a_stale_session_and_a_cancelled_capture() {
        let d = tempfile::tempdir().unwrap();
        let mut dev = Device::new(d.path().to_path_buf(), quiet());
        dev.ffx = fake(
            d.path(),
            "ffx",
            r#"case "$*" in
  "trace stop --abort") touch "$0.aborted";;
  "trace start"*) [ -e "$0.aborted" ] || { echo "Trace already started for target x."; exit 1; }
    eval out=\${$#}; touch "$out";;
esac"#,
        );
        let out = d.path().join("t.fxt");
        dev.trace(&out, 0.2, DEFAULT_CATEGORIES, 64, &mut || {}, &|| false).unwrap();
        let log = calls(&dev.ffx);
        assert_eq!(log.matches("trace start").count(), 2, "{log}");
        assert!(log.contains("--duration 1 "), "{log}"); // rounded up, never 0
        assert!(log.contains("trace stop --abort"), "{log}");

        dev.ffx = fake(d.path(), "ffx2", r#"case "$*" in "trace start"*) sleep 30;; esac"#);
        let t0 = Instant::now();
        let err = dev.trace(&out, 20.0, DEFAULT_CATEGORIES, 64, &mut || {}, &|| true).unwrap_err();
        assert_eq!(err.to_string(), "trace capture cancelled");
        assert!(t0.elapsed() < Duration::from_secs(20));
        assert!(calls(&dev.ffx).contains("trace stop --abort"));
    }

    #[test]
    fn profiler_retries_after_an_abort_and_never_keeps_a_stale_profile() {
        let d = tempfile::tempdir().unwrap();
        let mut dev = Device::new(d.path().to_path_buf(), quiet());
        dev.ffx = fake(
            d.path(),
            "ffx",
            r#"case "$*" in
  "profiler stop --abort") touch "$0.aborted";;
  "profiler attach"*) [ -e "$0.aborted" ] || exit 1; echo "Background session started" >&2;;
  "profiler stop"*) exit 1;;
esac"#,
        );
        dev.profiler_start(&[7], 100).unwrap();
        let log = calls(&dev.ffx);
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            [
                "profiler attach --background --sample-period-us 100 --pids 7",
                "profiler stop --abort",
                "profiler attach --background --sample-period-us 100 --pids 7",
            ]
        );
        let out = d.path().join("p.fxt");
        std::fs::write(&out, "stale").unwrap();
        assert!(dev.profiler_stop(&out).is_err());
        assert!(!out.exists());
        dev.ffx = fake(d.path(), "ffx2", r#"eval out=\${$#}; echo new > "$out""#);
        assert_eq!(dev.profiler_stop(&out).unwrap(), out);
    }

    #[test]
    fn dropping_the_device_stops_the_stress_load() {
        let d = tempfile::tempdir().unwrap();
        let mut dev = Device::new(d.path().to_path_buf(), quiet());
        let fx = fake(d.path(), "fx", r#"case "$2" in loadgen*) read line;; esac"#);
        dev.fx = fx.clone();
        dev.start_stress(2).unwrap();
        drop(dev);
        assert_eq!(calls(&fx), "shell loadgen 4 10 20 0.1 0.3\nshell killall loadgen\n");
    }

    #[test]
    fn ota_skips_only_when_the_image_and_the_known_profiles_match() {
        let d = tempfile::tempdir().unwrap();
        let (root, out) = (d.path().join("f"), d.path().join("f/out/x"));
        std::fs::create_dir_all(out.join("pb/repository")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("b/board.profiles"), r#"{"profiles": {"x.a": {"priority": 1}}}"#)
            .unwrap();
        // A single-product build: no main_pb_label, one bundle.
        std::fs::write(out.join("args.gn"), RISCV_ARGS).unwrap();
        std::fs::write(
            out.join("product_bundles.json"),
            r#"[{"label": "//p:pb(//tc:x)", "path": "pb"}]"#,
        )
        .unwrap();
        std::fs::write(
            out.join("pb/repository/targets.json"),
            r#"{"signed": {"targets": {"system_image/0": {"custom": {"merkle": "abc123"}}}}}"#,
        )
        .unwrap();
        let fx_body = |roles: &str| {
            format!(
                r#"case "$1" in
  get-build-dir) echo "{}";;
  --dir) echo "{roles}";;
  shell) case "$2" in
    *build-info*) echo __zpo_ok=1; echo board=riscv64;;
    "read ver"*) echo abc123;;
    cat*) cat "{}";;
  esac;;
esac"#,
                out.display(),
                root.join("b/board.profiles").display()
            )
        };
        let mut dev = Device::new(root.clone(), quiet());
        dev.fx = fake(d.path(), "fx", &fx_body("//b:board.profiles"));
        dev.ffx = fake(
            d.path(),
            "ffx",
            r#"case "$*" in "target update"*) echo "Install failed"; exit 1;; esac"#,
        );
        let v = dev.ota(5).unwrap();
        assert_eq!(v, json!({"updated": false, "merkle": "abc123"}));
        assert!(calls(&dev.fx).contains("labels(main, //p:pb)"), "{}", calls(&dev.fx));
        assert!(!calls(&dev.ffx).contains("target update"));

        // No roles files found: the profiles can't be checked, so it flashes.
        dev.fx = fake(d.path(), "fx2", &fx_body(""));
        let err = dev.ota(5).unwrap_err();
        assert!(err.downcast_ref::<UserFacingError>().is_some(), "{err}");
        assert!(calls(&dev.fx).contains("--keep_going"));
        assert!(calls(&dev.ffx).contains("target update force-install"));
    }

    #[test]
    fn describe_prefers_the_main_product_bundle() {
        assert_eq!(describe_build_dir(Some(RISCV_ARGS)), "bringup.riscv64");
        // A commented-out assignment is not a value.
        let commented = format!("{RISCV_ARGS}# main_pb_label = \"//p:old\"\n");
        assert_eq!(describe_build_dir(Some(&commented)), "bringup.riscv64");
        assert!(same_bundle("//p:pb(//build/toolchain:x)", "//p:pb"));
        assert!(same_bundle("//p:pb", "//p:pb"));
        assert!(!same_bundle("//p:pb_eng(//build/toolchain:x)", "//p:pb"));
        let with_pb = format!("{RISCV_ARGS}main_pb_label = \"//p:handset_eng.pad\"\n");
        assert_eq!(describe_build_dir(Some(&with_pb)), "//p:handset_eng.pad");
        assert!(describe_build_dir(None).contains("no args.gn"));
    }

    #[test]
    fn missing_product_bundle_says_how_to_fix_it() {
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("args.gn"), RISCV_ARGS).unwrap();
        let dev = Device::new(out.path().to_path_buf(), quiet());
        let e = dev.product_bundle_dir_in(&out.path().display().to_string()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("builds bringup.riscv64"), "{msg}");
        assert!(msg.contains("fx use out/"), "{msg}");
        assert!(msg.contains(PREREQUISITES_URL), "{msg}");
        assert!(e.downcast_ref::<UserFacingError>().is_some());
    }
}
