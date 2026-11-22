// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The `zpo` command line tool.

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zpo::attribution::Bucket;
use zpo::derive::{self, DeriveOptions};
use zpo::device::{Device, Log};
use zpo::model::Trace;
use zpo::profiles_file::NS_PER_US_F64;
use zpo::runner::{self, Ctx, LoadDef, Runner, Session};
use zpo::topology::Topology;
use zpo::tp::session::Sessions;
use zpo::{
    analysis, checkout, extensions, profiles_file, report, roles, stats, summary, tasks, ui_daemon,
    verdict,
};

/// How long `zpo ui stop` waits after SIGTERM before SIGKILL: long enough
/// for the server to answer the requests it is answering.
const UI_STOP_WAIT: Duration = Duration::from_secs(5);
/// How long `zpo ui start` waits for the new server to answer: starting
/// reads the checkout and the agent backend.
const UI_START_WAIT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(
    name = "zpo",
    version = zpo::VERSION,
    about = "Zircon Profile Optimizer (zpo)",
    after_help = "--fuchsia-dir PATH picks the Fuchsia checkout; otherwise $FUCHSIA_DIR, the current \
                  directory, the checkout of `fx` on PATH, then ~/fuchsia."
)]
struct Cli {
    /// The Fuchsia checkout to work against.
    #[arg(long, global = true)]
    fuchsia_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct AnalysisArgs {
    /// thread:<proc>/<thread> | fidl:<lib/Protocol.Method> | events:<start>-><end>
    #[arg(long)]
    task: String,
    /// A measurement.json whose saved CPU topology and roles to use; default:
    /// read them from the connected device.
    #[arg(long)]
    topology: Option<PathBuf>,
    /// Consumer window / required response time, e.g. 850us or 16.6ms.
    #[arg(long)]
    window: Option<String>,
    /// Work quantile for capacity.
    #[arg(long, default_value_t = 0.99)]
    quantile: f64,
    /// Response-time tail the verdict is based on.
    #[arg(long, default_value_t = verdict::TAIL_Q)]
    tail: f64,
    #[arg(long, default_value_t = analysis::MAX_JOBS)]
    max_jobs: usize,
    #[arg(long, default_value_t = analysis::MAX_DEPTH)]
    max_depth: usize,
    /// Propose capacities below the current ones.
    #[arg(long)]
    allow_shrink: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Analyze a task in a trace.
    Analyze {
        #[arg(long)]
        trace: PathBuf,
        #[command(flatten)]
        a: AnalysisArgs,
        /// Write a markdown report here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write machine-readable results here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Print markdown instead of text.
        #[arg(long)]
        markdown: bool,
        /// Print the segment timeline of job k (-1 = slowest).
        #[arg(long, allow_hyphen_values = true)]
        dump_job: Option<i64>,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Define a task + load for measurement on hardware.
    SessionInit {
        /// Directory for traces and results.
        #[arg(long)]
        session: PathBuf,
        #[arg(long)]
        task: String,
        #[arg(long)]
        window: Option<String>,
        #[arg(long, default_value = "manual")]
        load_mode: String,
        /// manual: text shown to the user per capture (repeatable).
        #[arg(long)]
        instruction: Vec<String>,
        /// automated: host command that drives the load.
        #[arg(long)]
        load_command: Option<String>,
        /// Seconds per capture.
        #[arg(long, default_value_t = 15.0)]
        duration: f64,
        #[arg(long, default_value_t = 2.0)]
        warmup: f64,
        #[arg(long, default_value_t = 2)]
        repeats: i64,
        /// Skip the full-CPU-load variant.
        #[arg(long)]
        no_stress: bool,
        #[arg(long, default_value_t = 512)]
        buffer_mb: i64,
        /// Also record a CPU profile of the chain per condition (kept out of the comparison).
        #[arg(long)]
        profile: bool,
    },
    /// Capture traces for a session (baseline or candidate).
    Measure {
        #[arg(long)]
        session: PathBuf,
        /// Measurement name, e.g. baseline, candidate-1.
        #[arg(long)]
        name: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        no_analyze: bool,
    },
    /// (Re)analyze a measurement.
    SessionAnalyze {
        #[arg(long)]
        session: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// A/B regression gate between two measurements.
    Gate {
        #[arg(long)]
        session: PathBuf,
        #[arg(long, default_value = "baseline")]
        baseline: String,
        #[arg(long)]
        candidate: String,
    },
    /// Write a role into the board's .profiles file (found through the build
    /// directory).
    ProfileSet {
        #[arg(long)]
        role: String,
        #[arg(long)]
        capacity: String,
        #[arg(long)]
        period: String,
        /// Comma separated cpus; default: an existing role keeps its CPUs, a new
        /// one gets the set most of the device's deadline roles use.
        #[arg(long)]
        affinity: Option<String>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Edit even if the build directory does not look like the device's.
        #[arg(long)]
        yes: bool,
    },
    /// Write a measurement's proposals into the board's .profiles file, as
    /// the UI's Apply does (threads sharing a role are merged).
    ProfileApply {
        #[arg(long)]
        session: PathBuf,
        #[arg(long)]
        name: String,
        /// LABEL=ROLE for a proposal (repeatable); default: the top suggested role.
        #[arg(long)]
        role: Vec<String>,
        /// Apply every proposal, not only the ones the verdict recommends.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
        /// Edit even if the build directory does not look like the device's.
        #[arg(long)]
        yes: bool,
    },
    /// fx build + push the product bundle to the device + verify.
    Ota {
        #[arg(long)]
        no_build: bool,
        /// Build even if the build directory does not look like the device's.
        #[arg(long)]
        yes: bool,
    },
    /// Suggest role names for the threads of an analysis: a measurement's
    /// analysis.json or the output of `analyze --json`.
    Roles {
        #[arg(long)]
        analysis: PathBuf,
        /// A measurement.json whose loaded roles to suggest from; default: the
        /// one next to the analysis, else the connected device's roles.
        #[arg(long)]
        topology: Option<PathBuf>,
    },
    /// Ask the agent to explain a markdown report and suggest fixes.
    Explain {
        /// Markdown report from `analyze --out`.
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
    },
    /// The browser UI server: `zpo ui start`, `zpo ui stop`, `zpo ui restart`.
    ///
    /// `start` runs the server in the background and prints the link to open;
    /// it keeps running after the command returns and after you log out, until
    /// `zpo ui stop` or the red X in the UI. Every request needs the token in
    /// the link, which is kept in ~/.config/zpo/ui_token.
    Ui {
        #[command(subcommand)]
        action: Option<UiAction>,
    },
    /// Write a self-contained HTML report of saved sessions.
    Report {
        /// Only "html" is supported.
        #[arg(long, default_value = "html")]
        format: String,
        /// Output file.
        #[arg(short, long)]
        out: PathBuf,
        /// A session directory (repeatable; default: every session).
        #[arg(long)]
        session: Vec<PathBuf>,
        /// The session shown first.
        #[arg(long)]
        first: Option<String>,
        /// Where sessions live (when --session is not given).
        #[arg(long)]
        sessions: Option<PathBuf>,
    },
}

fn stderr_log(prefix: &'static str) -> Log {
    Arc::new(move |m: &str| eprintln!("{prefix} {m}"))
}

#[derive(Subcommand)]
enum UiAction {
    /// Start the UI server in the background and print its link.
    ///
    /// If one is already running, asks whether to replace it (`--yes`
    /// replaces it without asking); without a terminal it keeps the running
    /// server and prints its link. A replacement keeps the old server's
    /// settings, like `zpo ui restart`.
    Start {
        #[command(flatten)]
        serve: UiServeArgs,
        /// Run in this terminal instead of in the background.
        #[arg(long)]
        foreground: bool,
        /// Replace a running server without asking.
        #[arg(long)]
        yes: bool,
        /// Set by `zpo ui start` for the background server, whose output goes
        /// to a log file: the token link is left out of it.
        #[arg(long, hide = true)]
        detached: bool,
    },
    /// Stop the background UI server.
    Stop,
    /// Stop the running UI server, if any, and start a new one, without
    /// asking.
    ///
    /// The new server keeps the stopped one's sessions dir, port, address
    /// and checkout; flags given here override them. To go back to the
    /// defaults, run `zpo ui stop` and then `zpo ui start`.
    Restart {
        #[command(flatten)]
        serve: UiServeArgs,
    },
}

/// How the UI server runs.
#[derive(Args, Clone)]
struct UiServeArgs {
    /// Address to listen on [default: the replaced server's, else 0.0.0.0,
    /// every interface].
    #[arg(long)]
    host: Option<String>,
    /// Port to listen on [default: the replaced server's, else 8765].
    #[arg(long)]
    port: Option<u16>,
    /// Where sessions live [default: the replaced server's, else
    /// ~/.local/share/zpo/sessions].
    #[arg(long)]
    sessions: Option<PathBuf>,
    /// Make a new token; links with the old one stop working.
    #[arg(long)]
    new_token: bool,
}

impl UiServeArgs {
    /// The settings given on the command line, with the checkout.
    fn flags(&self, fuchsia_dir: Option<&Path>) -> ui_daemon::ServeFlags {
        ui_daemon::ServeFlags {
            bind: self.host.clone(),
            port: self.port,
            sessions: self.sessions.clone(),
            fuchsia_dir: fuchsia_dir.map(Path::to_path_buf),
        }
    }
}

/// Stops the recorded server. 5 s after SIGTERM is long enough for it to
/// answer what it is answering.
fn ui_stop() -> anyhow::Result<ui_daemon::Stopped> {
    Ok(ui_daemon::stop_server(&ui_daemon::state_dir()?, &ui_daemon::SystemProbe, UI_STOP_WAIT))
}

/// `zpo ui start`: starts the server in the background, or points at the
/// running one. `fuchsia_dir` is `--fuchsia-dir`, already in `env`.
fn ui_start(
    serve: &UiServeArgs,
    yes: bool,
    env: &checkout::Env,
    fuchsia_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let (cfg, state) = (ui_daemon::config_dir()?, ui_daemon::state_dir()?);
    if let Some(s) = ui_daemon::running_server(&state, &ui_daemon::SystemProbe) {
        let tty = std::io::stdin().is_terminal();
        let mut asked = false;
        let replace = ui_daemon::replace_existing(yes, tty, || {
            asked = true;
            print!("{}\n\n{}", ui_daemon::already_running(&s), ui_daemon::REPLACE_PROMPT);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        });
        if !replace {
            let token = ui_daemon::ui_token(&cfg, false)?;
            if !tty {
                println!(
                    "{} Leaving it running.\n\nTo replace it, run `zpo ui start --yes` or \
                     `zpo ui restart`.\n",
                    ui_daemon::already_running(&s)
                );
            } else {
                println!();
            }
            println!(
                "Open this link in your browser to use the UI:\n\n    {}",
                ui_daemon::link(&s.host, s.port, &token)
            );
            return Ok(());
        }
        if asked {
            println!();
        }
        let stopped = ui_stop()?;
        println!("{}\n", ui_daemon::stopped_message(&stopped));
        return ui_relaunch(serve, env, fuchsia_dir, &stopped);
    }
    let found = checkout::find(env);
    ui_launch(&serve.flags(fuchsia_dir), found, serve.new_token, None)
}

/// Starts a new server after `stopped`, keeping the stopped server's
/// settings that the flags leave out (`zpo ui restart`, `start --yes`).
fn ui_relaunch(
    serve: &UiServeArgs,
    env: &checkout::Env,
    fuchsia_dir: Option<&Path>,
    stopped: &ui_daemon::Stopped,
) -> anyhow::Result<()> {
    let found = checkout::find(env);
    let (flags, kept) =
        serve.flags(fuchsia_dir).or_recorded(stopped.server(), &checkout::is_checkout);
    let defaults = ui_daemon::Defaults { sessions: sessions_default(), checkout: found.0.clone() };
    let note = ui_daemon::kept_message(&kept, &defaults);
    let checkout = match kept.fuchsia_dir {
        Some(d) => (d, ui_daemon::KEPT_SOURCE.to_string()),
        None => found,
    };
    ui_launch(&flags, checkout, serve.new_token, note)
}

/// Starts a new server in the background against `checkout` (its path and
/// where it came from) and prints its link, after `note` when there is one.
/// The server gets the checkout as an absolute `FUCHSIA_DIR`, with its source
/// for the UI.
fn ui_launch(
    flags: &ui_daemon::ServeFlags,
    checkout: (PathBuf, String),
    new_token: bool,
    note: Option<String>,
) -> anyhow::Result<()> {
    let (root, source) = checkout;
    let root = std::path::absolute(checkout::require_at(root, &source)?)?;
    let (cfg, state) = (ui_daemon::config_dir()?, ui_daemon::state_dir()?);
    let (bind, port) = (flags.bind(), flags.port());
    if ui_daemon::port_in_use(bind, port) {
        bail!("{}", ui_daemon::port_taken_message(&state, bind, port, &ui_daemon::SystemProbe));
    }
    // The server reads the token when it starts, so rotate it first.
    let token = ui_daemon::ui_token(&cfg, new_token)?;
    let exe = std::env::current_exe().context("could not find the zpo binary")?;
    let mut args: Vec<String> = Vec::new();
    for a in ["ui", "start", "--foreground", "--detached", "--host", bind, "--port"] {
        args.push(a.into());
    }
    args.push(port.to_string());
    if let Some(s) = &flags.sessions {
        args.push("--sessions".into());
        args.push(std::path::absolute(s)?.display().to_string());
    }
    ui_daemon::ServerState::remove(&state);
    let log = state.join(ui_daemon::LOG_FILE);
    let envs = [("FUCHSIA_DIR", root.display().to_string()), (checkout::SOURCE_ENV, source)];
    let mut child = ui_daemon::spawn_detached(&exe, &args, &envs, &log)?;
    // Starting reads the checkout and the agent backend: allow 30 s.
    let s = ui_daemon::wait_until_up(&state, &mut child, &ui_daemon::SystemProbe, UI_START_WAIT)
        .with_context(|| format!("see {}:\n{}", log.display(), ui_daemon::tail(&log, 10)))?;
    if let Some(n) = note {
        println!("{n}\n");
    }
    println!("{}", ui_daemon::started_message(&s, &token, &log));
    Ok(())
}

/// `zpo ui` (its help) and `zpo ui stop`, which need no checkout, so they
/// work anywhere; None for every other command.
fn ui_without_checkout(cmd: &Cmd) -> Option<anyhow::Result<()>> {
    match cmd {
        Cmd::Ui { action: None } => Some((|| {
            // Bare `zpo ui`: what the subcommands are.
            let mut cmd = <Cli as clap::CommandFactory>::command();
            cmd.build();
            cmd.find_subcommand_mut("ui").context("no ui command")?.print_help()?;
            Ok(())
        })()),
        Cmd::Ui { action: Some(UiAction::Stop) } => {
            Some(ui_stop().map(|stopped| println!("{}", ui_daemon::stopped_message(&stopped))))
        }
        _ => None,
    }
}

/// `zpo ui start` (in the background) and `zpo ui restart`, which check the
/// checkout themselves: a restart keeps the replaced server's, which this
/// directory need not be in. None for every other command.
fn ui_background(
    cmd: &Cmd,
    env: &checkout::Env,
    fuchsia_dir: Option<&Path>,
) -> Option<anyhow::Result<()>> {
    match cmd {
        Cmd::Ui { action: Some(UiAction::Start { serve, foreground: false, yes, .. }) } => {
            Some(ui_start(serve, *yes, env, fuchsia_dir))
        }
        Cmd::Ui { action: Some(UiAction::Restart { serve }) } => Some((|| {
            let stopped = ui_stop()?;
            println!("{}\n", ui_daemon::restart_preamble(&stopped));
            ui_relaunch(serve, env, fuchsia_dir, &stopped)
        })()),
        _ => None,
    }
}

fn sessions_default() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".local/share/zpo/sessions")
}

fn agent(ctx: &Ctx) -> Option<zpo::agent::Agent> {
    extensions::agent(&ctx.ext_dirs).map(|(a, cmd)| zpo::agent::Agent::new(a.name, cmd))
}

fn params_json(p: Option<(i64, i64)>) -> Value {
    p.map_or(Value::Null, |(a, b)| json!([a, b]))
}

fn summary_json(xs: &[i64]) -> Value {
    serde_json::to_value(stats::summary_i64(xs)).unwrap_or(Value::Null)
}

fn to_json(
    res: &analysis::AnalysisResult,
    tr: &Trace,
    vd: &verdict::Verdict,
    props: &[derive::ProfileProposal],
) -> Value {
    let n_att = res.attributions.len() as f64;
    json!({
        "task": res.spec.label(),
        "trace": res.trace_path,
        "jobs": res.jobs.len(),
        "verdict": {
            "primary": vd.primary, "headline": vd.headline, "recommend_profiles": vd.recommend_profiles,
            "tail_q": vd.tail_q, "shares_tail": runner::shares_json(&vd.shares_tail), "findings": vd.findings,
        },
        "response_ns": summary_json(&res.response_ns),
        "inter_arrival_ns": summary_json(&res.inter_arrival_ns),
        "buckets": Value::Object(res.bucket_per_job.iter().map(|(b, xs)| (b.as_str().to_string(), summary_json(xs))).collect()),
        "threads": Value::Object(res.threads.iter().map(|(u, t)| (u.to_string(), json!({
            "label": t.label, "deadline_params": params_json(t.deadline_params), "fair_weight": t.fair_weight,
            "buckets": Value::Object(t.per_job.iter().map(|(b, xs)| (b.as_str().to_string(), summary_json(xs))).collect()),
            "cpus": Value::Object(t.cpus.iter().map(|(c, ns)| (c.to_string(), json!(ns))).collect()),
        }))).collect()),
        "hops": res.hops.iter().map(|((f, t, m), v)| json!({
            "from": tr.label(*f), "to": tr.label(*t), "method": m,
            "per_job": v.len() as f64 / n_att, "mean_ns": v.iter().sum::<i64>() as f64 / v.len() as f64,
        })).collect::<Vec<_>>(),
        "proposals": props.iter().map(|p| json!({
            "utid": p.utid, "label": p.label, "period_ns": p.period_ns, "capacity_ns": p.capacity_ns,
            "affinity": p.affinity, "utilization": p.utilization, "work_per_period_ns": p.work_per_period_ns,
            "existing": params_json(p.existing), "profiles_json": p.profiles_json(), "notes": p.notes,
        })).collect::<Vec<_>>(),
        "notes": res.notes,
    })
}

fn dump_job(res: &analysis::AnalysisResult, tr: &Trace, k: i64) {
    let k = if k < 0 {
        (0..res.attributions.len())
            .max_by(|&a, &b| {
                res.attributions[a].total().cmp(&res.attributions[b].total()).then(b.cmp(&a))
            })
            .unwrap_or(0)
    } else {
        k as usize
    };
    let Some(a) = res.attributions.get(k) else { return };
    println!(
        "\njob {k}: {} [{} .. {}] {:.1} us",
        tr.label(a.utid),
        a.t0,
        a.t1,
        a.total() as f64 / NS_PER_US_F64
    );
    for s in &a.segments {
        let det = match s.bucket {
            Bucket::BlockedOn => format!("-> {}", tr.thread_label(s.detail)),
            Bucket::HwWait => summary::hw_wait_detail(s.detail),
            _ => String::new(),
        };
        let cpu = if s.cpu >= 0 { format!("cpu{}", s.cpu) } else { String::new() };
        let ind = "  ".repeat(s.depth);
        println!(
            "  {ind}+{:8.1}us {:8.1}us {:<15} {:<50.50} {cpu} {det}",
            (s.t0 - a.t0) as f64 / NS_PER_US_F64,
            s.dur() as f64 / NS_PER_US_F64,
            s.bucket.as_str(),
            tr.label(s.utid)
        );
        if s.bucket == Bucket::Run {
            for ev in tr.ipc_between(s.utid, s.t0, s.t1) {
                let arrow = if ev.outgoing { "->" } else { "<-" };
                println!(
                    "  {ind}      ipc {arrow} {} {arrow} {:.40}",
                    ev.method,
                    tr.thread_label(ev.peer_utid)
                );
            }
        }
    }
}

/// Where `zpo analyze` writes and what it prints.
struct AnalyzeOutput<'a> {
    out: Option<&'a Path>,
    json: Option<&'a Path>,
    markdown: bool,
    dump_job: Option<i64>,
    verbose: bool,
}

fn cmd_analyze(
    ctx: &Ctx,
    trace: &Path,
    a: &AnalysisArgs,
    o: AnalyzeOutput<'_>,
) -> anyhow::Result<()> {
    let AnalyzeOutput { out, json: json_out, markdown: md, dump_job: dump, verbose } = o;
    let tr = ctx.load_trace(trace, &|m| eprintln!("[load] {m}"))?;
    let topo = match &a.topology {
        Some(p) => Topology::from_measurement(&read_json(p)?)?,
        None => Device::new(ctx.fuchsia_dir.clone(), stderr_log("[dev]")).topology()?,
    };
    let res = analysis::analyze(&tr, tasks::parse(&a.task)?, Some(&topo), a.max_jobs, a.max_depth)?;
    let window = summary::check_analysis_args(a.window.as_deref(), a.quantile, a.tail)?;
    let opts = DeriveOptions {
        consumer_window_ns: window,
        quantile: a.quantile,
        allow_shrink: a.allow_shrink,
        ..Default::default()
    };
    let c = verdict::conclude(&res, &tr, &topo, &opts, a.tail, &ctx.lessons);
    let (vd, props) = (&c.verdict, &c.shown);
    println!("{}", report::render(&res, &tr, vd, props, md, verbose));
    if let Some(o) = out {
        std::fs::write(o, report::render(&res, &tr, vd, props, true, true))?;
        println!("\nwrote {}", o.display());
    }
    if let Some(j) = json_out {
        std::fs::write(j, serde_json::to_string_pretty(&to_json(&res, &tr, vd, props))?)?;
        println!("wrote {}", j.display());
    }
    if let Some(k) = dump {
        dump_job(&res, &tr, k);
    }
    tr.check()
}

fn read_json(p: &Path) -> anyhow::Result<Value> {
    let text =
        std::fs::read_to_string(p).with_context(|| format!("could not read {}", p.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Writes role changes into the board's roles file through the same library
/// function as the UI's Apply, or prints the diff. Like the UI, it first
/// checks that the build directory is for the device, since the build finds
/// the roles file and Apply needs it to put the change on the device.
fn apply_changes(
    ctx: &Ctx,
    changes: &[profiles_file::RoleChange],
    default_affinity: &[i64],
    comment: Option<&str>,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let dev = Device::new(ctx.fuchsia_dir.clone(), stderr_log("[dev]"));
    let check = dev.build_dir_check();
    if !check.looks_ok && !yes {
        bail!(
            "{}\n{}\n(pass --yes to edit with this build directory anyway)",
            zpo::device::NEEDS_BUILD,
            check.warning.unwrap_or_default()
        );
    }
    let e = dev.edit_roles(changes, default_affinity, comment)?;
    if dry_run {
        println!("--- {}", e.path.display());
        // One hunk: everything between the unchanged first and last lines.
        let (a, b): (Vec<&str>, Vec<&str>) = (e.old.lines().collect(), e.new.lines().collect());
        let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let post =
            a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
        if pre < a.len().max(b.len()) - post {
            println!("@@ line {} @@", pre + 1);
            a[pre..a.len() - post].iter().for_each(|l| println!("-{l}"));
            b[pre..b.len() - post].iter().for_each(|l| println!("+{l}"));
        }
        return Ok(());
    }
    std::fs::write(&e.path, &e.new)?;
    for l in e.log {
        println!("{l} in {}", e.path.display());
    }
    Ok(())
}

/// Analyzes a measurement exactly as the UI does and prints its conclusion,
/// then where the run time goes in any CPU profile captures.
fn analyze_and_print(ctx: &Ctx, session: &Path, name: &str) -> anyhow::Result<()> {
    let summ = summary::analyze_measurement(
        ctx,
        session,
        name,
        Arc::new(|m: &str| eprintln!("{m}")),
        None,
    )?;
    println!("analysis written to {}/{name}/summary.json", session.display());
    println!("{}", summary::conclusion_text(&summ));
    let out = summ.get("run_functions").and_then(Value::as_object).cloned().unwrap_or_default();
    for (cond, rf) in &out {
        let n = rf.get("samples").and_then(Value::as_i64).unwrap_or(0);
        let what = rf
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{n} samples"));
        println!("\nwhere the run time goes ({cond}): {what}");
        if n > 0 {
            for f in rf.get("functions").and_then(Value::as_array).into_iter().flatten().take(10) {
                let ctx = f
                    .get("caller")
                    .and_then(Value::as_str)
                    .or_else(|| f.get("context").and_then(Value::as_str));
                println!(
                    "  {:5.1}%  {}  ({}){}",
                    100.0 * f["self"].as_f64().unwrap_or(0.0) / n as f64,
                    f["name"].as_str().unwrap_or(""),
                    f["binary"].as_str().unwrap_or(""),
                    ctx.map(|c| format!("  in {c}")).unwrap_or_default()
                );
            }
        }
    }
    if !out.is_empty() {
        println!("profile tables written to {}/{name}/profile.json", session.display());
    }
    Ok(())
}

fn runner_for(ctx: &Ctx, session: &Path, with_device: bool) -> anyhow::Result<Runner> {
    let s = Session::load(session)?;
    let dev: Option<Arc<dyn runner::DeviceOps>> = with_device.then(|| {
        Arc::new(Device::new(ctx.fuchsia_dir.clone(), stderr_log("[dev]")))
            as Arc<dyn runner::DeviceOps>
    });
    Ok(Runner::new(
        s,
        ctx.clone(),
        dev,
        Arc::new(|m| {
            println!("\n>>> {m}");
            Ok(())
        }),
        Arc::new(|m: &str| eprintln!("{m}")),
        None,
    ))
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let mut env = checkout::Env::current();
    if let Some(d) = &cli.fuchsia_dir {
        env.vars.insert("FUCHSIA_DIR".into(), d.display().to_string());
        env.vars.insert(checkout::SOURCE_ENV.into(), checkout::FLAG_SOURCE.into());
    }
    if let Some(r) = ui_without_checkout(&cli.cmd) {
        return r;
    }
    if let Some(r) = ui_background(&cli.cmd, &env, cli.fuchsia_dir.as_deref()) {
        return r;
    }
    let root = match checkout::require(&env) {
        Ok(r) => r,
        // `zpo report` only reads saved sessions, so it works without a
        // checkout (and without an extension's lesson links).
        Err(_) if matches!(cli.cmd, Cmd::Report { .. }) => PathBuf::new(),
        Err(e) => return Err(e),
    };
    let ext_dirs =
        if root.as_os_str().is_empty() { vec![] } else { extensions::dirs_from_env(&root) };
    let ctx = Ctx {
        sessions: Arc::new(Sessions::new(Arc::new(zpo::proc::HostRunner), zpo::tp::default_bin())),
        ext_dirs: ext_dirs.clone(),
        lessons: extensions::lessons(&ext_dirs),
        checkout: checkout::describe(&env),
        fuchsia_dir: root,
    };
    match cli.cmd {
        Cmd::Analyze { trace, a, out, json, markdown, dump_job, verbose } => cmd_analyze(
            &ctx,
            &trace,
            &a,
            AnalyzeOutput {
                out: out.as_deref(),
                json: json.as_deref(),
                markdown,
                dump_job,
                verbose,
            },
        ),
        Cmd::SessionInit {
            session,
            task,
            window,
            load_mode,
            instruction,
            load_command,
            duration,
            warmup,
            repeats,
            no_stress,
            buffer_mb,
            profile,
        } => {
            let load = LoadDef {
                mode: load_mode,
                instructions: instruction,
                command: load_command.unwrap_or_default(),
                duration_s: duration,
                warmup_s: warmup,
            };
            let mut s = Session::new(&session, &task, load);
            s.window = window;
            s.stress = !no_stress;
            s.repeats = repeats;
            s.trace_buffer_mb = buffer_mb;
            s.profile = profile;
            let problems = s.problems();
            if !problems.is_empty() {
                bail!(
                    "invalid session: {}",
                    problems
                        .iter()
                        .map(|(f, m)| format!("{f}: {m}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            s.save()?;
            println!("session written to {}", s.path().display());
            Ok(())
        }
        Cmd::Measure { session, name, note, no_analyze } => {
            let r = runner_for(&ctx, &session, true)?;
            let info = r.measure(&name, note.as_deref().unwrap_or(""))?;
            println!(
                "captured {} traces under {}/{name}",
                info["captures"].as_array().map_or(0, |a| a.len()),
                session.display()
            );
            if !no_analyze {
                analyze_and_print(&ctx, &session, &name)?;
            }
            Ok(())
        }
        Cmd::SessionAnalyze { session, name } => analyze_and_print(&ctx, &session, &name),
        Cmd::Gate { session, baseline, candidate } => {
            let g = runner::gate_files(&session, &baseline, &candidate)?;
            println!("{} {}", g.status.to_uppercase(), if g.passed { "(PASS)" } else { "" });
            for x in &g.improvements {
                println!("  + improved: {x}");
            }
            for x in &g.regressions {
                println!("  - REGRESSED: {x}");
            }
            for x in &g.neutral {
                println!("  = neutral: {x}");
            }
            // A non-zero exit, so scripts can gate on it.
            if !g.passed {
                bail!("the candidate did not pass the gate ({})", g.status);
            }
            Ok(())
        }
        Cmd::ProfileSet { role, capacity, period, affinity, comment, dry_run, yes } => {
            let affinity: Vec<i64> = match affinity {
                Some(a) => a
                    .split(',')
                    .map(|x| x.trim().parse())
                    .collect::<Result<_, _>>()
                    .context("bad --affinity")?,
                None => vec![],
            };
            let change = profiles_file::RoleChange {
                role,
                capacity_us: profiles_file::parse_us(&capacity)?,
                period_us: profiles_file::parse_us(&period)?,
                affinity,
                label: String::new(),
            };
            // Without --affinity, a new role takes the default from the roles
            // the device loaded, as the UI's proposals do.
            let default = if change.affinity.is_empty() {
                Device::new(ctx.fuchsia_dir.clone(), stderr_log("[dev]"))
                    .topology()?
                    .default_affinity()
                    .0
            } else {
                vec![]
            };
            let comment = Some(comment.as_deref().unwrap_or(""));
            apply_changes(&ctx, &[change], &default, comment, dry_run, yes)
        }
        Cmd::ProfileApply { session, name, role, all, dry_run, yes } => {
            let topo = Topology::from_measurement(&read_json(
                &session.join(&name).join("measurement.json"),
            )?)?;
            let summ: Value = serde_json::from_str(
                &std::fs::read_to_string(session.join(&name).join("summary.json"))
                    .context("the measurement has no summary.json; run session-analyze first")?,
            )?;
            let overrides: Vec<(String, String)> = role
                .iter()
                .map(|r| r.split_once('=').map(|(l, r)| (l.to_string(), r.to_string())))
                .collect::<Option<_>>()
                .context("--role takes LABEL=ROLE")?;
            let proposals =
                summ.get("proposals").and_then(Value::as_array).cloned().unwrap_or_default();
            summary::check_role_overrides(&proposals, &overrides)?;
            let mut changes = Vec::new();
            for p in summ.get("proposals").and_then(Value::as_array).into_iter().flatten() {
                let label = p["label"].as_str().unwrap_or("");
                let chosen = overrides.iter().find(|(l, _)| l == label).map(|(_, r)| r.clone());
                if !(all || chosen.is_some() || p["recommended"].as_bool() == Some(true)) {
                    continue;
                }
                let Some(role) = chosen.or_else(|| {
                    p.pointer("/roles/0/role").and_then(Value::as_str).map(str::to_string)
                }) else {
                    bail!("no role suggested for {label}; pass --role '{label}=<role>'");
                };
                let mut c = profiles_file::RoleChange::from_json(p);
                c.role = role;
                changes.push(c);
            }
            if changes.is_empty() {
                bail!("nothing to apply: no proposal is recommended (pass --all or --role)");
            }
            apply_changes(&ctx, &changes, &topo.default_affinity().0, None, dry_run, yes)
        }
        Cmd::Ota { no_build, yes } => {
            let dev = Device::new(ctx.fuchsia_dir, stderr_log("[dev]"));
            if !yes {
                let c = dev.build_dir_check();
                if !c.looks_ok {
                    eprintln!("zpo ota: warning: {}", c.warning.unwrap_or_default());
                    let mut answer = String::new();
                    if std::io::stdin().is_terminal() {
                        eprint!("Build {} anyway? [y/N] ", c.builds);
                        let _ = std::io::stdin().read_line(&mut answer);
                    }
                    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                        bail!("zpo ota: stopped before building (pass --yes to build anyway)");
                    }
                }
            }
            if !no_build {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                dev.build(&Path::new(&home).join(".cache/zpo-fx-build.log"))?;
            }
            println!("{}", dev.ota(600)?);
            Ok(())
        }
        Cmd::Roles { analysis, topology } => {
            let sibling = analysis.with_file_name("measurement.json");
            let topo = match topology.or_else(|| sibling.exists().then_some(sibling)) {
                Some(p) => Topology::from_measurement(&read_json(&p)?)?,
                None => Device::new(ctx.fuchsia_dir.clone(), stderr_log("[dev]")).topology()?,
            };
            let rl = topo.role_params();
            for (label, params) in summary::analysis_threads(&read_json(&analysis)?) {
                println!("{label}:");
                for s in roles::suggest(&label, params, &rl, 5) {
                    println!("    {:4.1}  {}   ({})", s.score, s.role, s.why);
                }
            }
            Ok(())
        }
        Cmd::Explain { report, task, out, model } => {
            let a = agent(&ctx).context("no agent backend is configured (see the zpo README)")?;
            let md = std::fs::read_to_string(&report)?;
            let model = model.or_else(|| a.models().ok().map(|m| m.default)).unwrap_or_default();
            let text = zpo::llm::explain(
                &a,
                &md,
                &ctx.fuchsia_dir.display().to_string(),
                task.as_deref().unwrap_or("(see report)"),
                &model,
                None,
                false,
                None,
                &ctx.lessons,
            )?;
            println!("{text}");
            if let Some(o) = out {
                std::fs::write(o, &text)?;
            }
            Ok(())
        }
        Cmd::Ui {
            action:
                None
                | Some(UiAction::Stop | UiAction::Restart { .. })
                | Some(UiAction::Start { foreground: false, .. }),
        } => {
            unreachable!("handled before the checkout")
        }
        Cmd::Ui { action: Some(UiAction::Start { serve, foreground: true, detached, .. }) } => {
            let st = Arc::new(zpo::server::State::new(
                serve.sessions.clone().unwrap_or_else(sessions_default),
                ctx.clone(),
                agent(&ctx),
            )?);
            let opts = zpo::server::ServeOptions {
                bind: serve.host.unwrap_or_else(|| ui_daemon::DEFAULT_BIND.into()),
                port: serve.port.unwrap_or(ui_daemon::DEFAULT_PORT),
                token: ui_daemon::ui_token(&ui_daemon::config_dir()?, serve.new_token)?,
                host: ui_daemon::fqdn(),
                state_dir: ui_daemon::state_dir()?,
                print_link: !detached,
            };
            zpo::server::serve(opts, st)
        }
        Cmd::Report { format, out, session, first, sessions } => {
            if format != "html" {
                bail!("only --format html is supported");
            }
            let dirs = if session.is_empty() {
                let root = sessions.unwrap_or_else(sessions_default);
                let mut v: Vec<PathBuf> = std::fs::read_dir(&root)
                    .with_context(|| format!("could not read {}", root.display()))?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.join("session.json").exists())
                    .collect();
                v.sort();
                v
            } else {
                session
            };
            let html = zpo::report_html::render(&ctx, &dirs, first.as_deref())?;
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            std::fs::write(&out, html)
                .with_context(|| format!("could not write {}", out.display()))?;
            println!("wrote {} ({} sessions)", out.display(), dirs.len());
            Ok(())
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("zpo: {e:#}");
        std::process::exit(1);
    }
}
