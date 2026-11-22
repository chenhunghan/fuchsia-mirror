// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `zpo ui start`: a small HTTP server around sessions.
//!
//! No framework and no build step: one HTML page (`ui/index.html`, embedded in
//! the binary) talks JSON to this server. Long operations (measure, build +
//! OTA, agent questions) run on a background thread and stream their log
//! through `/api/jobs/<id>`. Manual-load prompts are surfaced in the page and
//! acknowledged with a button.

use crate::agent::{Agent, Models};
use crate::device::{Device, Log, UserFacingError};
use crate::runner::{self, Cancelled, Ctx, LoadDef, Runner, Session};
use crate::topology::{NO_CORE_IDS_TITLE, NO_CORE_IDS_WARNING};
use crate::ui_daemon::{ServerState, link};
use crate::{profiles_file, summary, trace_context};
use anyhow::{Context as _, bail};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The page, embedded at build time.
pub const INDEX_HTML: &str = include_str!("../ui/index.html");

/// The page's service worker (see the file), with `__ZPO_SHELL_VERSION__`
/// replaced by [`shell_version`].
const SERVICE_WORKER_JS: &str = include_str!("../ui/sw.js");

/// Names the page and service worker this binary serves: a hash of both and
/// of zpo's version. The service worker keys its cache by it, so a new build
/// with a changed page replaces the cached copy.
pub fn shell_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let mut h = Sha256::new();
        for part in [crate::VERSION, INDEX_HTML, SERVICE_WORKER_JS] {
            h.update(part.as_bytes());
            h.update([0u8]);
        }
        h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
    })
}

/// A background operation with a log and an optional pending prompt.
#[derive(Debug)]
pub struct Job {
    pub id: String,
    pub kind: String,
    session: Mutex<Option<String>>,
    log: Mutex<Vec<(String, Instant)>>,
    state: Mutex<String>,
    result: Mutex<Option<Value>>,
    error: Mutex<Option<String>>,
    error_info: Mutex<Option<Value>>,
    prompt: Mutex<Option<String>>,
    acked: Mutex<bool>,
    ack_cv: Condvar,
    started: Instant,
    cancelled: AtomicBool,
}

fn new_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix =
        (n ^ (n >> 17) ^ ((std::process::id() as u128) << 40)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", (mix >> 32) as u32)
}

impl Job {
    fn new(kind: &str, session: Option<String>) -> Arc<Job> {
        Arc::new(Job {
            id: new_id(),
            kind: kind.to_string(),
            session: Mutex::new(session),
            log: Mutex::new(Vec::new()),
            state: Mutex::new("running".into()),
            result: Mutex::new(None),
            error: Mutex::new(None),
            error_info: Mutex::new(None),
            prompt: Mutex::new(None),
            acked: Mutex::new(false),
            ack_cv: Condvar::new(),
            started: Instant::now(),
            cancelled: AtomicBool::new(false),
        })
    }

    /// Appends a timestamped log line.
    pub fn logf(&self, msg: &str) {
        let t = chrono::Local::now().format("%H:%M:%S");
        lock(&self.log).push((format!("{t} {msg}"), Instant::now()));
    }

    fn state(&self) -> String {
        lock(&self.state).clone()
    }

    fn session(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.logf("STATUS: Cancelling…");
        self.ack(); // release a pending prompt
    }

    pub fn check_cancel(&self) -> anyhow::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Blocks until the user acknowledges in the UI (manual load prompts).
    pub fn ask(&self, msg: &str) -> anyhow::Result<()> {
        self.check_cancel()?;
        if msg.starts_with("Done") {
            self.logf(msg);
            return Ok(());
        }
        let mut acked = lock(&self.acked);
        *acked = false;
        *lock(&self.prompt) = Some(msg.to_string());
        self.logf(&format!("PROMPT: {msg}"));
        while !*acked && !self.cancelled.load(Ordering::SeqCst) {
            acked = self.ack_cv.wait(acked).unwrap_or_else(PoisonError::into_inner);
        }
        drop(acked);
        *lock(&self.prompt) = None;
        self.check_cancel()
    }

    pub fn ack(&self) {
        *lock(&self.acked) = true;
        self.ack_cv.notify_all();
    }

    fn log_fn(self: &Arc<Self>) -> Log {
        let j = self.clone();
        Arc::new(move |m: &str| j.logf(m))
    }

    pub fn to_json(&self, last: Option<usize>) -> Value {
        let log = lock(&self.log);
        let lines: Vec<&String> = log.iter().map(|(l, _)| l).collect();
        let mut progress = Value::Null;
        for l in lines.iter().rev() {
            if let Some((_, rest)) = l.split_once("PROGRESS: ") {
                let (frac, label) = rest.split_once(' ').unwrap_or((rest, ""));
                if let Some((k, n)) = frac.split_once('/') {
                    if let (Ok(k), Ok(n)) = (k.parse::<i64>(), n.parse::<i64>()) {
                        progress = json!({"k": k, "n": n, "label": label});
                    }
                }
                break;
            }
        }
        let (mut status, mut age, mut seq) = (Value::Null, Value::Null, Value::Null);
        for (i, (l, t)) in log.iter().enumerate().rev() {
            if let Some((_, rest)) = l.split_once("STATUS: ") {
                status = json!(rest);
                age = json!(t.elapsed().as_secs_f64());
                // Tells repeated statuses apart (one "Recording for 15 s" per capture).
                seq = json!(i);
                break;
            }
        }
        let mut caps = Map::new();
        for l in &lines {
            if let Some((_, rest)) = l.split_once("CAPTURE: ") {
                if let Some((tag, st)) = rest.split_once(' ') {
                    caps.insert(tag.to_string(), json!(st));
                }
            }
        }
        let n = last.unwrap_or(400);
        let tail: Vec<&String> = lines[lines.len().saturating_sub(n)..].to_vec();
        json!({
            "id": self.id,
            "kind": self.kind,
            "state": self.state(),
            "log": tail,
            "status": status,
            "status_age_s": age,
            "status_seq": seq,
            "captures": caps,
            "session": self.session(),
            "progress": progress,
            "result": lock(&self.result).clone(),
            "error": lock(&self.error).clone(),
            "error_info": lock(&self.error_info).clone(),
            "prompt": lock(&self.prompt).clone(),
            "elapsed_s": self.started.elapsed().as_secs(),
        })
    }
}

/// Locks `m`. A job thread that panicked while holding a lock leaves its
/// data in a usable state (strings and flags), so poisoning is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server's state.
#[derive(Debug)]
pub struct State {
    pub sessions_dir: PathBuf,
    pub ctx: Ctx,
    pub agent: Option<Agent>,
    jobs: Mutex<IndexMap<String, Arc<Job>>>,
    models_cache: Mutex<Option<(Instant, Value)>>,
    /// Set by `POST /api/shutdown`; [`serve`] then exits.
    stop: AtomicBool,
    /// Requests being answered, so a shutdown can let them finish.
    inflight: AtomicUsize,
    /// This machine's name as links use it (set by [`serve`]), for the page's
    /// banners and dialogs: behind a proxy the page cannot tell it.
    pub host: std::sync::OnceLock<String>,
    /// This run's id (set by [`serve`]), sent in every response so `zpo ui`
    /// knows the recorded server answers (see [`crate::ui_daemon::ServerState`]).
    instance: std::sync::OnceLock<String>,
}

/// How often the server's waits (for jobs to stop, for a shutdown) check.
const POLL: Duration = Duration::from_millis(100);
/// How long the agent's model list is cached: the page asks often, the
/// backend answers slowly.
const MODELS_CACHE: Duration = Duration::from_secs(60);
/// A client that stops reading or writing for this long is gone.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// `ffx trace upload` sends the whole trace.
const TRACE_UPLOAD: Duration = Duration::from_secs(600);
/// How long a shutdown lets the requests in flight finish, and how often it
/// checks.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// Finished jobs kept for the page's job list and polls; older ones are
/// dropped so a server that runs for weeks does not grow without bound. The
/// page only looks at running jobs and the last few finished ones.
const KEEP_FINISHED_JOBS: usize = 50;

/// How long a request waits for a session's cancelled jobs to stop before it
/// answers 409: a capture stops within a few seconds of a cancel.
const STOP_WAIT: Duration = Duration::from_secs(30);

/// The answer when a session's jobs did not stop within [`STOP_WAIT`].
fn still_stopping() -> Response {
    Response::Json(
        409,
        json!({"error": "a job on this session is still stopping; try again in a moment"}),
    )
}

/// The answer to explain or resolve-roles while another job runs on the
/// session. They do not cancel it: a measurement would be thrown away.
fn job_running() -> Response {
    Response::Json(
        409,
        json!({"error": "a job is running on this session; wait for it to finish or cancel it first"}),
    )
}

impl State {
    pub fn new(sessions_dir: PathBuf, ctx: Ctx, agent: Option<Agent>) -> anyhow::Result<Self> {
        // Absolute, since the server records it for a restart run elsewhere.
        let sessions_dir = std::path::absolute(&sessions_dir)?;
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(State {
            sessions_dir,
            ctx,
            agent,
            jobs: Mutex::new(IndexMap::new()),
            models_cache: Mutex::new(None),
            stop: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            host: std::sync::OnceLock::new(),
            instance: std::sync::OnceLock::new(),
        })
    }

    fn sdir(&self, name: &str) -> PathBuf {
        self.sessions_dir.join(name)
    }

    pub fn cancel_session_jobs(&self, session: &str) -> usize {
        let jobs: Vec<Arc<Job>> = lock(&self.jobs).values().cloned().collect();
        let mut n = 0;
        for j in jobs {
            if j.session().as_deref() == Some(session) && j.state() == "running" {
                j.cancel();
                n += 1;
            }
        }
        n
    }

    /// Whether a job is running on the session.
    fn session_busy(&self, session: &str) -> bool {
        lock(&self.jobs)
            .values()
            .any(|j| j.session().as_deref() == Some(session) && j.state() == "running")
    }

    /// Cancels a session's running jobs and waits up to `timeout` for them to
    /// stop, so deleting or moving its directory does not race a job that is
    /// still writing into it. Returns false if a job is still running.
    pub fn stop_session_jobs(&self, session: &str, timeout: Duration) -> bool {
        self.cancel_session_jobs(session);
        let deadline = Instant::now() + timeout;
        loop {
            if !self.session_busy(session) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    pub fn rename_session(&self, old: &str, new: &str) -> anyhow::Result<()> {
        let (old, new) = (old.trim(), new.trim());
        if new.is_empty() || new == old {
            return Ok(());
        }
        if !valid_segment(old) || !valid_segment(new) {
            bail!("invalid session name");
        }
        let (od, nd) = (self.sdir(old), self.sdir(new));
        if !od.is_dir() {
            bail!("no such session: {old}");
        }
        if nd.exists() {
            bail!("session '{new}' already exists");
        }
        std::fs::rename(&od, &nd)?;
        let (os, ns) = (od.display().to_string(), nd.display().to_string());
        rewrite_paths(&nd, &os, &ns);
        for j in lock(&self.jobs).values() {
            let mut s = lock(&j.session);
            if s.as_deref() == Some(old) {
                *s = Some(new.to_string());
            }
        }
        Ok(())
    }

    /// Runs `f` on a background thread as a job.
    pub fn start<F>(self: &Arc<Self>, kind: &str, session: Option<String>, f: F) -> Arc<Job>
    where
        F: FnOnce(Arc<Job>) -> anyhow::Result<Value> + Send + 'static,
    {
        let job = Job::new(kind, session);
        {
            let mut jobs = lock(&self.jobs);
            let finished: Vec<String> =
                jobs.values().filter(|j| j.state() != "running").map(|j| j.id.clone()).collect();
            // Oldest first: the map keeps insertion order.
            for id in finished.iter().take(finished.len().saturating_sub(KEEP_FINISHED_JOBS)) {
                jobs.shift_remove(id);
            }
            jobs.insert(job.id.clone(), job.clone());
        }
        let j = job.clone();
        std::thread::spawn(move || match f(j.clone()) {
            Ok(v) => {
                *lock(&j.result) = Some(v);
                *lock(&j.state) = "done".into();
            }
            Err(e) if e.downcast_ref::<Cancelled>().is_some() => {
                *lock(&j.state) = "cancelled".into();
                j.logf("STATUS: Cancelled.");
            }
            Err(e) => {
                if let Some(u) = e.downcast_ref::<UserFacingError>() {
                    // A known failure with its own fix-it text.
                    let text = u.to_string();
                    *lock(&j.error_info) = Some(Value::Object(u.info.clone()));
                    j.logf(&format!("ERROR: {}", text.lines().next().unwrap_or("")));
                    *lock(&j.error) = Some(text);
                } else {
                    j.logf(&format!("ERROR: {e}"));
                    *lock(&j.error) = Some(format!("{e:#}"));
                }
                *lock(&j.state) = "error".into();
            }
        });
        job
    }

    pub fn list_sessions(&self) -> Value {
        let mut names: Vec<String> = std::fs::read_dir(&self.sessions_dir)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let d = self.sdir(&name);
            let Ok(s) = Session::load(&d) else { continue };
            let mut ms: Vec<String> = std::fs::read_dir(&d)
                .map(|rd| {
                    rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
                })
                .unwrap_or_default();
            ms.sort();
            let meas: Vec<Value> = ms
                .iter()
                .filter(|m| d.join(m).is_dir() && d.join(m).join("measurement.json").exists())
                .map(|m| {
                    json!({"name": m, "analyzed": d.join(m).join("analysis.json").exists(),
                           "report": d.join(m).join("report.md").exists()})
                })
                .collect();
            out.push(json!({
                "name": name, "dir": d.display().to_string(), "task": s.task, "load": s.load,
                "task_fields": crate::tasks::form_fields(&s.task), "window_ns": s.window_ns(),
                "window": s.window, "stress": s.stress, "repeats": s.repeats,
                "profile": s.profile, "trace_buffer_mb": s.trace_buffer_mb,
                "measurements": meas,
                "report_available": crate::report_html::session_has_report(&d),
            }));
        }
        json!(out)
    }

    /// `/api/ping` and `/api/info`: who the server is.
    pub fn info(&self) -> Value {
        json!({"ok": true, "host": self.host.get(), "version": crate::VERSION,
               "shell": shell_version()})
    }

    pub fn device(&self, log: Log) -> Device {
        Device::new(self.ctx.fuchsia_dir.clone(), log)
    }

    fn prefs_path(&self) -> PathBuf {
        let abs =
            std::path::absolute(&self.sessions_dir).unwrap_or_else(|_| self.sessions_dir.clone());
        abs.parent().unwrap_or(Path::new("/")).join("prefs.json")
    }

    pub fn prefs(&self) -> Value {
        std::fs::read_to_string(self.prefs_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(json!({}))
    }

    pub fn save_pref(&self, key: &str, value: Value) {
        let mut p = self.prefs();
        if !p.is_object() {
            p = json!({});
        }
        p[key] = value;
        if let Ok(s) = serde_json::to_string_pretty(&p) {
            let _ = std::fs::write(self.prefs_path(), s);
        }
    }

    /// {"default", "groups", "names", "live"}, cached for a minute.
    pub fn llm_models(&self) -> Value {
        let mut cache = lock(&self.models_cache);
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed() < MODELS_CACHE {
                return v.clone();
            }
        }
        let m: Option<Models> = self.agent.as_ref().and_then(|a| a.models().ok());
        let v = match m {
            Some(m) => {
                json!({"default": m.default, "groups": m.groups, "names": m.names(), "live": m.live})
            }
            None => json!({"default": "", "groups": [], "names": {}, "live": false}),
        };
        *cache = Some((Instant::now(), v.clone()));
        v
    }

    fn agent_json(&self) -> Value {
        match &self.agent {
            None => {
                json!({"available": false, "name": "", "hint": "No agent backend is configured (see the zpo README)."})
            }
            Some(a) => {
                let (ok, hint) = a.info();
                json!({"available": ok, "name": a.name, "hint": hint})
            }
        }
    }
}

/// Replaces the old session directory in every .json/.md file under `dir`.
/// `text` with every occurrence of the path `old` replaced by `new`, where the
/// occurrence is a whole path prefix: followed by `/`, a quote or the end, so
/// renaming `.../touch` does not touch `.../touch2`.
fn replace_path(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(old) {
        let after = &rest[i + old.len()..];
        let boundary = after.is_empty() || after.starts_with(['/', '"', '\'', '\n', ' ', ')']);
        out.push_str(&rest[..i]);
        out.push_str(if boundary { new } else { old });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn rewrite_paths(dir: &Path, old: &str, new: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rewrite_paths(&p, old, new);
        } else if p.extension().is_some_and(|x| x == "json" || x == "md") {
            if let Ok(t) = std::fs::read_to_string(&p) {
                if t.contains(old) {
                    let _ = std::fs::write(&p, replace_path(&t, old, new));
                }
            }
        }
    }
}

// The HTTP server: request parsing, routing and responses.

/// A parsed request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

/// A response: status, content type, body, extra headers.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Json(u16, Value),
    Html(&'static str),
    File {
        path: PathBuf,
        filename: String,
    },
    /// Any body, with extra header lines (a cookie, a download's file name).
    Bytes {
        code: u16,
        ctype: &'static str,
        body: Vec<u8>,
        headers: Vec<String>,
    },
    NotFound,
}

/// Whether `s` is safe as one path segment (a session or measurement name).
/// Quotes and control characters are rejected too, since names end up in a
/// `Content-Disposition` header.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.chars().any(|c| matches!(c, '/' | '\\' | '"') || c.is_control())
}

fn gate_mentions(fname: &str, m: &str) -> bool {
    let prefix = format!("gate-{m}-vs-");
    let suffix = format!("-vs-{m}.json");
    (fname.starts_with(&prefix) && fname.ends_with(".json"))
        || (fname.starts_with("gate-") && fname.ends_with(&suffix))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The request headers the server looks at.
#[derive(Debug, Default)]
struct Headers {
    /// `X-Zpo: 1`, which only the UI's own scripts send.
    x_zpo: bool,
    /// `Content-Type: application/json`.
    json: bool,
    /// `X-Zpo-Token`.
    token: String,
}

/// The largest request body accepted.
const MAX_BODY: usize = 1 << 20;

/// Longest request or header line, and most headers, accepted.
const MAX_LINE: u64 = 8 << 10;
const MAX_HEADERS: usize = 100;

fn read_line_capped(r: &mut impl BufRead, line: &mut String) -> anyhow::Result<usize> {
    let n = r.take(MAX_LINE).read_line(line)?;
    if n as u64 == MAX_LINE && !line.ends_with('\n') {
        bail!("request line too long");
    }
    Ok(n)
}

fn read_request(stream: &mut TcpStream) -> anyhow::Result<(Request, Headers)> {
    let mut r = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    read_line_capped(&mut r, &mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().context("empty request")?.to_string();
    let target = parts.next().context("no request target")?.to_string();
    let mut len = 0usize;
    let mut headers = Headers::default();
    for n in 0.. {
        if n == MAX_HEADERS {
            bail!("too many headers");
        }
        let mut h = String::new();
        if read_line_capped(&mut r, &mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "x-zpo" => headers.x_zpo = v == "1",
                "content-type" => {
                    headers.json = v
                        .split(';')
                        .next()
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
                }
                "x-zpo-token" => headers.token = v.to_string(),
                _ => {}
            }
        }
    }
    // Every POST is a small JSON payload; refusing more before reading it
    // keeps a request from making the server allocate much.
    if len > MAX_BODY {
        bail!("request too large");
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Ok((Request { method, path, query, body }, headers))
}

/// Who may talk to the server: one rule on every address.
///
/// The page itself (`/`) holds no data and is served to anyone. Every other
/// request must carry the token in an `X-Zpo-Token` header. The page keeps
/// the token in its origin's storage, so neither another site nor another
/// server on this machine (another port) can read or send it, and no cookie
/// carries it. Requests are not checked against their `Host`: the token
/// already covers DNS rebinding, and behind a proxy the host is the proxy's
/// name. State-changing POSTs must also carry `X-Zpo: 1` and a JSON content
/// type, which a cross-site form or `no-cors` request cannot send.
struct Access {
    token: String,
}

impl Access {
    /// Err((why, whether the request carried the right token)).
    fn check(&self, req: &Request, h: &Headers) -> Result<(), (&'static str, bool)> {
        let signed_in = same_secret(&h.token, &self.token);
        if req.method == "POST" && !(h.x_zpo && h.json) {
            let why = "POST requests need `X-Zpo: 1` and `Content-Type: application/json`";
            return Err((why, signed_in));
        }
        // The page and its service worker hold no data.
        let page = matches!(req.method.as_str(), "GET" | "HEAD")
            && matches!(req.path.as_str(), "/" | "/index.html" | "/sw.js");
        if page {
            return Ok(());
        }
        if !signed_in {
            let why = "this page does not have the zpo server's token; open the link `zpo ui start` \
                       prints";
            return Err((why, false));
        }
        Ok(())
    }
}

/// Compares two secrets without stopping at the first difference.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Writes `resp`; for a HEAD request (`head_only`), the headers alone.
/// `instance` is the server's run id (see [`State`]).
fn write_response(
    stream: &mut TcpStream,
    resp: Response,
    head_only: bool,
    instance: &str,
) -> std::io::Result<()> {
    let head = |code: u16, ctype: &str, len: u64, extra: &str| {
        let reason = match code {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            409 => "Conflict",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        };
        format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nX-Zpo: 1\r\n\
             {}: {instance}\r\n{extra}Connection: close\r\n\r\n",
            crate::ui_daemon::INSTANCE_HEADER
        )
    };
    match resp {
        Response::Json(code, v) => {
            let body = serde_json::to_vec(&v).unwrap_or_default();
            stream.write_all(head(code, "application/json", body.len() as u64, "").as_bytes())?;
            if head_only { Ok(()) } else { stream.write_all(&body) }
        }
        Response::Html(h) => {
            // Always revalidated, so a new zpo build's page replaces the old one.
            let extra = "Cache-Control: no-cache\r\n";
            stream.write_all(
                head(200, "text/html; charset=utf-8", h.len() as u64, extra).as_bytes(),
            )?;
            if head_only { Ok(()) } else { stream.write_all(h.as_bytes()) }
        }
        Response::File { path, filename } => {
            let mut f = std::fs::File::open(&path)?;
            let len = f.metadata()?.len();
            let extra = format!("Content-Disposition: attachment; filename=\"{filename}\"\r\n");
            stream.write_all(head(200, "application/octet-stream", len, &extra).as_bytes())?;
            if head_only { Ok(()) } else { std::io::copy(&mut f, stream).map(|_| ()) }
        }
        Response::Bytes { code, ctype, body, headers } => {
            let extra: String = headers.iter().map(|h| format!("{h}\r\n")).collect();
            stream.write_all(head(code, ctype, body.len() as u64, &extra).as_bytes())?;
            if head_only { Ok(()) } else { stream.write_all(&body) }
        }
        Response::NotFound => {
            let body = b"Not found";
            stream.write_all(head(404, "text/plain", body.len() as u64, "").as_bytes())?;
            if head_only { Ok(()) } else { stream.write_all(body) }
        }
    }
}

/// Counts a request as in flight while it lives.
struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(st: &Arc<State>, access: &Access, mut stream: TcpStream) {
    st.inflight.fetch_add(1, Ordering::SeqCst);
    let _inflight = InFlight(&st.inflight);
    let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
    let Ok((req, headers)) = read_request(&mut stream) else { return };
    let resp = respond(st, access, &req, &headers);
    // A client that hangs up mid-reply (a browser cancelling a poll) is routine.
    let instance = st.instance.get().map_or("", String::as_str);
    let _ = write_response(&mut stream, resp, req.method == "HEAD", instance);
}

/// The answer to one request, after the access check.
fn respond(st: &Arc<State>, access: &Access, req: &Request, headers: &Headers) -> Response {
    if let Err((why, signed_in)) = access.check(req, headers) {
        return Response::Json(403, json!({"error": why, "signed_in": signed_in}));
    }
    match (req.method.as_str(), req.path.as_str()) {
        // Cheap enough for the page's heartbeat; also says who the server is.
        ("GET" | "HEAD", "/api/ping" | "/api/info") => return Response::Json(200, st.info()),
        _ => {}
    }
    match req.method.as_str() {
        "GET" | "HEAD" => get(st, req),
        "POST" => match serde_json::from_slice::<Value>(if req.body.is_empty() {
            b"{}"
        } else {
            &req.body
        }) {
            Ok(body) => post(st, req, &body)
                .unwrap_or_else(|e| Response::Json(500, json!({"error": format!("{e:#}")}))),
            Err(e) => Response::Json(400, json!({"error": format!("bad JSON: {e}")})),
        },
        _ => Response::NotFound,
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn read_json(p: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(
        &std::fs::read_to_string(p).with_context(|| format!("could not read {}", p.display()))?,
    )?)
}

/// The JSON a GET of `path` returns, if any (the static report uses it).
pub fn get_json(st: &Arc<State>, path: &str) -> Option<Value> {
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    match get(st, &Request { method: "GET".into(), path: p.into(), query: q.into(), body: vec![] })
    {
        Response::Json(200, v) => Some(v),
        _ => None,
    }
}

pub(crate) fn get(st: &Arc<State>, req: &Request) -> Response {
    let path = req.path.as_str();
    if path == "/" || path == "/index.html" {
        return Response::Html(INDEX_HTML);
    }
    if path == "/sw.js" {
        return Response::Bytes {
            code: 200,
            ctype: "text/javascript; charset=utf-8",
            body: SERVICE_WORKER_JS.replace("__ZPO_SHELL_VERSION__", shell_version()).into_bytes(),
            // Browsers then check for a new service worker on every load.
            headers: vec!["Cache-Control: no-cache".into()],
        };
    }
    if let Some(r) = report_download(st, path) {
        return r;
    }
    if path == "/api/sessions" {
        return Response::Json(200, st.list_sessions());
    }
    if let Some(id) = path.strip_prefix("/api/jobs/") {
        let job = lock(&st.jobs).get(id).cloned();
        return match job {
            Some(j) => Response::Json(200, j.to_json(None)),
            None => Response::Json(404, json!({"error": "no such job"})),
        };
    }
    if path == "/api/jobs" {
        let jobs: Vec<Value> = lock(&st.jobs).values().map(|j| j.to_json(Some(3))).collect();
        return Response::Json(200, json!(jobs));
    }
    if path.starts_with("/api/session/") {
        let parts: Vec<String> = path.split('/').map(percent_decode).collect();
        let name = parts.get(3).cloned().unwrap_or_default();
        if !valid_segment(&name) {
            return Response::Json(400, json!({"error": "invalid session name"}));
        }
        let sdir = st.sdir(&name);
        if parts.len() >= 6 && parts[5] == "summary" {
            let m = &parts[4];
            if !valid_segment(m) {
                return Response::Json(400, json!({"error": "invalid measurement name"}));
            }
            let p = sdir.join(m).join("summary.json");
            let Ok(mut d) = read_json(&p) else {
                return Response::Json(404, json!({"error": "not analyzed"}));
            };
            let ep = sdir.join(m).join("explain.md");
            if let Ok(md) = std::fs::read_to_string(&ep) {
                d["explain_md"] = json!(md);
                if let Ok(meta) = read_json(&sdir.join(m).join("explain.json")) {
                    d["explain_meta"] = meta;
                }
            }
            d["traces"] = summary::trace_files(&sdir, m);
            d["by_condition"] = summary::by_condition(&sdir, m);
            let lm = st.llm_models();
            d["llm_models"] = lm["names"].clone();
            let pref = st.prefs().get("llm_model").and_then(Value::as_str).map(str::to_string);
            d["llm_model_default"] = match pref {
                Some(p) if lm["names"].get(&p).is_some() => json!(p),
                _ => lm["default"].clone(),
            };
            d["llm_model_groups"] = lm["groups"].clone();
            d["llm_models_live"] = lm["live"].clone();
            d["agent"] = st.agent_json();
            d["agent_trace_access"] = match &st.agent {
                Some(a) => serde_json::to_value(a.query_access(
                    &st.prefs(),
                    &trace_context::allow_rule(st.ctx.sessions.tp_bin()),
                ))
                .unwrap_or(Value::Null),
                None => {
                    json!({"allowed": false, "via": null, "reason": "no agent backend", "rule": ""})
                }
            };
            return Response::Json(200, d);
        }
        if parts.len() >= 7 && parts[5] == "trace" {
            if !valid_segment(&parts[4]) || !valid_segment(&parts[6]) {
                return Response::Json(400, json!({"error": "invalid trace path"}));
            }
            // Raw .fxt download, for opening in ui.perfetto.dev without uploading anywhere.
            return match summary::trace_path(&sdir, &parts[4], &parts[6]) {
                Some(p) => Response::File {
                    path: p,
                    filename: format!("{name}-{}-{}.fxt", parts[4], parts[6]),
                },
                None => Response::Json(404, json!({"error": "no such trace"})),
            };
        }
        if parts.len() >= 5 && (parts[4] == "gates" || (parts.len() >= 6 && parts[5] == "gates")) {
            let mut fns: Vec<String> = std::fs::read_dir(&sdir)
                .map(|rd| {
                    rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
                })
                .unwrap_or_default();
            fns.sort();
            let mut gates = Vec::new();
            for f in fns.iter().filter(|f| f.starts_with("gate-") && f.ends_with(".json")) {
                if let Ok(Value::Object(mut g)) = read_json(&sdir.join(f)) {
                    let mut o = Map::new();
                    o.insert("name".into(), json!(&f[5..f.len() - 5]));
                    o.append(&mut g);
                    gates.push(Value::Object(o));
                }
            }
            return Response::Json(200, json!(gates));
        }
    }
    match path {
        "/api/checkout" => Response::Json(200, json!(st.ctx.checkout)),
        "/api/build-dir" => {
            // Pre-build warning: does the build directory make an image for the
            // device's board? `needs_build` is the question Apply asks first.
            let dev = st.device(Arc::new(|_| {}));
            let mut v = json!(dev.build_dir_check());
            v["needs_build"] = json!(crate::device::NEEDS_BUILD);
            Response::Json(200, v)
        }
        "/api/cpu-fit" => {
            // Asked before each measurement the page starts (see `measure` in
            // the UI).
            let dev = st.device(Arc::new(|_| {}));
            Response::Json(200, cpu_fit_json(dev.reports_core_ids()))
        }
        "/api/device" => {
            let dev = st.device(Arc::new(|_| {}));
            Response::Json(
                200,
                match dev.build_info() {
                    Ok(b) => json!({"ok": true, "build": b}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                },
            )
        }
        "/api/lessons" => {
            Response::Json(200, json!({"url": st.ctx.lessons.url, "links": st.ctx.lessons.links()}))
        }
        "/api/agent" => Response::Json(200, st.agent_json()),
        _ => Response::NotFound,
    }
}

/// `/api/cpu-fit`: `{"checked": true}` when proposals can check the CPU fit;
/// `{"checked": false, "title", "message"}` for the dialog when the device's
/// cpuctl does not report core IDs; `{"error"}` when the device did not
/// answer (the measurement itself then reports why).
fn cpu_fit_json(reports_core_ids: anyhow::Result<bool>) -> Value {
    match reports_core_ids {
        Ok(true) => json!({"checked": true}),
        Ok(false) => {
            json!({"checked": false, "title": NO_CORE_IDS_TITLE, "message": NO_CORE_IDS_WARNING})
        }
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

fn int_of(v: &Value, k: &str, default: i64) -> i64 {
    match v.get(k) {
        Some(Value::Number(n)) => {
            n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default)
        }
        Some(Value::String(x)) => x.trim().parse().unwrap_or(default),
        _ => default,
    }
}

fn float_of(v: &Value, k: &str, default: f64) -> f64 {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(x)) => x.trim().parse().unwrap_or(default),
        _ => default,
    }
}

fn bool_of(v: &Value, k: &str, default: bool) -> bool {
    match v.get(k) {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(x)) => !x.is_empty(),
        Some(_) => true,
    }
}

/// The topology saved with the session's most recently recorded measurement.
fn latest_topology(sdir: &Path) -> Option<crate::topology::Topology> {
    let mut best: Option<(std::time::SystemTime, Value)> = None;
    for e in std::fs::read_dir(sdir).ok()?.flatten() {
        let p = e.path().join("measurement.json");
        let (Ok(t), Ok(v)) = (p.metadata().and_then(|m| m.modified()), read_json(&p)) else {
            continue;
        };
        if v.get("topology").is_some() && best.as_ref().is_none_or(|(bt, _)| t > *bt) {
            best = Some((t, v));
        }
    }
    crate::topology::Topology::from_measurement(&best?.1).ok()
}

/// The session a create/edit form describes, and its invalid fields (field
/// -> reason, the UI's field names). The task comes from `task`, or from
/// the form fields `task_kind`, `start`, `end`, `fidl` and `thread` (see
/// [`crate::tasks::compose`]); everything else is checked by
/// [`Session::problems`], as for `zpo session-init`.
fn session_from_body(st: &Arc<State>, body: &Value) -> (Session, Map<String, Value>) {
    let mut errors = Map::new();
    let name = s(body, "name").trim().to_string();
    let old = s(body, "old_name").trim();
    if name.is_empty() {
        errors.insert("name".into(), json!("required"));
    } else if !valid_segment(&name) {
        errors.insert("name".into(), json!("letters, digits, - _ . only"));
    } else if name != old && st.sdir(&name).join("session.json").exists() {
        errors.insert("name".into(), json!("name already in use"));
    }
    let load = LoadDef {
        mode: body.get("load_mode").and_then(Value::as_str).unwrap_or("manual").to_string(),
        instructions: s(body, "instructions")
            .split('\n')
            .filter(|x| !x.trim().is_empty())
            .map(str::to_string)
            .collect(),
        command: s(body, "load_command").to_string(),
        // A field that is present but does not parse becomes an invalid value,
        // so it is reported instead of silently taking the default.
        duration_s: float_of(
            body,
            "duration",
            if body.get("duration").is_some() { f64::NAN } else { 15.0 },
        ),
        warmup_s: float_of(body, "warmup", 2.0),
    };
    let task = match body.get("task_kind").and_then(Value::as_str) {
        Some(kind) => crate::tasks::compose(
            kind,
            s(body, "start"),
            s(body, "end"),
            s(body, "fidl"),
            s(body, "thread"),
        ),
        None => s(body, "task").trim().to_string(),
    };
    let sdir = if valid_segment(&name) { st.sdir(&name) } else { st.sessions_dir.clone() };
    let mut sess = Session::new(&sdir, &task, load);
    sess.window = body
        .get("window")
        .and_then(Value::as_str)
        .filter(|w| !w.trim().is_empty())
        .map(str::to_string);
    sess.stress = bool_of(body, "stress", true);
    sess.repeats = int_of(body, "repeats", if body.get("repeats").is_some() { 0 } else { 2 });
    sess.trace_buffer_mb =
        int_of(body, "buffer_mb", if body.get("buffer_mb").is_some() { 0 } else { 512 });
    sess.profile = bool_of(body, "profile", false);
    for (field, why) in sess.problems() {
        // A task that does not parse is reported on the kind's first field.
        let field = match (field, body.get("task_kind").and_then(Value::as_str)) {
            ("task", Some("events")) => "start",
            ("task", Some(k @ ("fidl" | "thread"))) => k,
            (f, _) => f,
        };
        errors.entry(field.to_string()).or_insert(json!(why));
    }
    (sess, errors)
}

fn post(st: &Arc<State>, req: &Request, body: &Value) -> anyhow::Result<Response> {
    let path = req.path.as_str();
    if path == "/api/shutdown" {
        // [`serve`] lets the requests in flight (this reply included) finish,
        // removes its state file and exits.
        st.stop.store(true, Ordering::SeqCst);
        return Ok(Response::Json(200, json!({"ok": true})));
    }
    if path == "/api/sessions/validate" {
        let (sess, errors) = session_from_body(st, body);
        return Ok(Response::Json(
            200,
            json!({"task": sess.task, "task_fields": crate::tasks::form_fields(&sess.task),
                   "window_ns": sess.window_ns(), "errors": errors}),
        ));
    }
    if path == "/api/sessions" {
        let (mut sess, errors) = session_from_body(st, body);
        if !errors.is_empty() {
            return Ok(Response::Json(
                400,
                json!({"error": "the session has invalid fields", "errors": errors}),
            ));
        }
        let name = s(body, "name").trim().to_string();
        let old = s(body, "old_name").trim().to_string();
        if !old.is_empty() && !valid_segment(&old) {
            return Ok(Response::Json(400, json!({"error": "invalid session name"})));
        }
        if !old.is_empty() && old != name {
            // A job still writing into the old directory would race the move.
            if !st.stop_session_jobs(&old, STOP_WAIT) {
                return Ok(still_stopping());
            }
            if let Err(e) = st.rename_session(&old, &name) {
                return Ok(Response::Json(400, json!({"error": e.to_string()})));
            }
        }
        let cancelled = st.cancel_session_jobs(&name); // editing the setup aborts any run
        if !st.stop_session_jobs(&name, STOP_WAIT) {
            return Ok(still_stopping());
        }
        sess.dir = st.sdir(&name).display().to_string();
        sess.save()?;
        return Ok(Response::Json(
            200,
            json!({"ok": true, "name": name, "cancelled_jobs": cancelled}),
        ));
    }
    if let Some(rest) = path.strip_prefix("/api/jobs/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        let job = lock(&st.jobs).get(id).cloned();
        match action {
            "cancel" => {
                if let Some(j) = &job {
                    if j.state() == "running" {
                        j.cancel();
                    }
                }
                return Ok(Response::Json(200, json!({"ok": job.is_some()})));
            }
            "ack" => {
                if let Some(j) = &job {
                    j.ack();
                }
                return Ok(Response::Json(200, json!({"ok": job.is_some()})));
            }
            _ => {}
        }
    }
    if path.starts_with("/api/session/") {
        let parts: Vec<String> = path.split('/').map(percent_decode).collect();
        let (name, action) =
            (parts.get(3).cloned().unwrap_or_default(), parts.get(4).cloned().unwrap_or_default());
        return session_action(st, &name, &action, body);
    }
    if path == "/api/prefs" {
        // Only settings the UI owns; values are booleans.
        if s(body, "key") != trace_context::PREF_ALLOWLISTED {
            return Ok(Response::Json(400, json!({"error": "unknown preference"})));
        }
        st.save_pref(trace_context::PREF_ALLOWLISTED, json!(bool_of(body, "value", false)));
        return Ok(Response::Json(200, json!({"ok": true})));
    }
    if path == "/api/discover" {
        // Capture a short trace (the user exercises the task) and ask the agent for a task spec.
        let desc = s(body, "description").to_string();
        let dur = float_of(body, "duration", 10.0);
        // The capture's timeout is a `Duration`, like Session::problems checks.
        if !(dur.is_finite() && dur > 0.0) {
            return Ok(Response::Json(400, json!({"error": "duration must be a positive number"})));
        }
        let model = body.get("model").and_then(Value::as_str).map(str::to_string);
        let st2 = st.clone();
        let job = st.start("discover task", None, move |job| {
            let agent = st2.agent.clone().context("no agent backend is configured")?;
            let dev = st2.device(job.log_fn());
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let out = st2.sessions_dir.join("_discover").join(format!("discover-{secs}.fxt"));
            job.ask(&format!("Exercise the task ('{desc}') for the next {dur:.0}s."))?;
            let cancelled = || job.check_cancel().is_err();
            if let Err(e) =
                dev.trace(&out, dur, crate::device::DEFAULT_CATEGORIES, 256, &mut || {}, &cancelled)
            {
                // A cancelled capture fails too; report it as cancelled.
                job.check_cancel()?;
                return Err(e);
            }
            job.check_cancel()?;
            let j2 = job.clone();
            let tr = st2.ctx.load_trace(&out, &move |m| j2.logf(m))?;
            let facts = summary::trace_facts(&tr)?;
            job.check_cancel()?;
            job.logf("asking the agent for a task spec...");
            let model = model
                .unwrap_or_else(|| st2.llm_models()["default"].as_str().unwrap_or("").to_string());
            let text = crate::llm::discover_task(&agent, &desc, &facts, &model)?;
            Ok(json!({"discover_md": text, "facts": facts, "trace": out.display().to_string()}))
        });
        return Ok(Response::Json(200, json!({"job": job.id})));
    }
    Ok(Response::Json(404, json!({"error": "unknown endpoint"})))
}

fn session_action(
    st: &Arc<State>,
    name: &str,
    action: &str,
    body: &Value,
) -> anyhow::Result<Response> {
    if !valid_segment(name) {
        return Ok(Response::Json(400, json!({"error": "invalid session name"})));
    }
    let sdir = st.sdir(name);
    // A new job on the session waits for the cancelled ones to stop, so their
    // cleanup never touches its files.
    if matches!(action, "measure" | "analyze" | "apply" | "ota" | "rename" | "delete" | "discard")
        && !st.stop_session_jobs(name, STOP_WAIT)
    {
        return Ok(still_stopping());
    }
    // The agent's jobs write into the session too, but they wait their turn
    // rather than cancel a running job.
    if matches!(action, "explain" | "resolve-roles") && st.session_busy(name) {
        return Ok(job_running());
    }
    match action {
        "measure" => {
            let mut mname = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or("baseline")
                .to_string();
            if mname == "baseline" && sdir.join("baseline").join("summary.json").exists() {
                let mut i = 1;
                while sdir.join(format!("candidate-{i}")).join("summary.json").exists() {
                    i += 1;
                }
                mname = format!("candidate-{i}");
            }
            if !valid_segment(&mname) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let note = s(body, "note").to_string();
            let (st2, sdir2, m2) = (st.clone(), sdir.clone(), mname.clone());
            let job =
                st.start(&format!("measure {name}/{mname}"), Some(name.to_string()), move |job| {
                    let sess = Session::load(&sdir2)?;
                    let dev: Arc<dyn runner::DeviceOps> = Arc::new(st2.device(job.log_fn()));
                    let (ja, jc) = (job.clone(), job.clone());
                    let r = Runner::new(
                        sess,
                        st2.ctx.clone(),
                        Some(dev),
                        Arc::new(move |m| ja.ask(m)),
                        job.log_fn(),
                        Some(Arc::new(move || jc.check_cancel())),
                    );
                    let res = r.measure(&m2, &note).and_then(|_| {
                        job.logf("STATUS: Analyzing traces");
                        let jc = job.clone();
                        summary::analyze_measurement(
                            &st2.ctx,
                            &sdir2,
                            &m2,
                            job.log_fn(),
                            Some(Arc::new(move || jc.check_cancel())),
                        )
                    });
                    // A measurement cancelled before its summary was written
                    // is not kept, whether it stopped recording or analyzing.
                    if let Err(e) = &res {
                        if e.downcast_ref::<Cancelled>().is_some()
                            && !sdir2.join(&m2).join("summary.json").exists()
                        {
                            let _ = std::fs::remove_dir_all(sdir2.join(&m2));
                        }
                    }
                    res
                });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        "rename" => {
            let new = s(body, "new_name").trim().to_string();
            match st.rename_session(name, &new) {
                Ok(()) => Ok(Response::Json(200, json!({"ok": true, "name": new}))),
                Err(e) => Ok(Response::Json(400, json!({"error": e.to_string()}))),
            }
        }
        "delete" => {
            let real = std::fs::canonicalize(&sdir).ok();
            let root = std::fs::canonicalize(&st.sessions_dir).ok();
            if let (Some(r), Some(root)) = (real, root) {
                if sdir.is_dir() && r.starts_with(&root) && r != root {
                    std::fs::remove_dir_all(&sdir)?;
                }
            }
            Ok(Response::Json(200, json!({"ok": true})))
        }
        "discard" => {
            let m = s(body, "name");
            if !valid_segment(m) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let target = sdir.join(m);
            if target.is_dir() {
                std::fs::remove_dir_all(&target)?;
            }
            for f in std::fs::read_dir(&sdir).into_iter().flatten().flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if gate_mentions(&fname, m) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            Ok(Response::Json(200, json!({"ok": true})))
        }
        "analyze" => {
            let mname = s(body, "name").to_string();
            if !valid_segment(&mname) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let (st2, sdir2, m2) = (st.clone(), sdir.clone(), mname.clone());
            let job =
                st.start(&format!("analyze {name}/{mname}"), Some(name.to_string()), move |job| {
                    let jc = job.clone();
                    summary::analyze_measurement(
                        &st2.ctx,
                        &sdir2,
                        &m2,
                        job.log_fn(),
                        Some(Arc::new(move || jc.check_cancel())),
                    )
                });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        "upload-trace" => {
            let (mname, tag) = (s(body, "name"), s(body, "tag"));
            if !valid_segment(mname) || !valid_segment(tag) {
                return Ok(Response::Json(400, json!({"error": "invalid trace path"})));
            }
            let r = upload_trace(st, &sdir, mname, tag);
            let code =
                if r.get("ok").and_then(Value::as_bool).unwrap_or(false) { 200 } else { 500 };
            Ok(Response::Json(code, r))
        }
        "gate" => {
            let base = body.get("baseline").and_then(Value::as_str).unwrap_or("baseline");
            let cand = s(body, "candidate");
            if !valid_segment(base) || !valid_segment(cand) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let g = runner::gate_files(&sdir, base, cand)?;
            Ok(Response::Json(200, serde_json::to_value(g)?))
        }
        "apply" => {
            let changes =
                body.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
            let ota = bool_of(body, "ota", true);
            let (st2, sdir2) = (st.clone(), sdir.clone());
            let job = st.start(&format!("apply {name}"), Some(name.to_string()), move |job| {
                job.logf("STATUS: Updating the roles file");
                job.logf("PROGRESS: 0/4 Finding and editing the roles file in the checkout");
                let dev = st2.device(job.log_fn());
                // A new role's default CPUs come from the roles the device
                // loaded for the session's latest measurement.
                let default =
                    latest_topology(&sdir2).map(|t| t.default_affinity().0).unwrap_or_default();
                // The same edit as `zpo profile-apply`.
                let changes: Vec<profiles_file::RoleChange> =
                    changes.iter().map(profiles_file::RoleChange::from_json).collect();
                let e = dev.edit_roles(&changes, &default, None)?;
                for l in &e.log {
                    job.logf(l);
                }
                job.check_cancel()?;
                std::fs::write(&e.path, &e.new)?;
                job.logf(&format!("wrote {}", e.path.display()));
                if ota {
                    job.check_cancel()?;
                    job.logf("STATUS: Building Fuchsia image (fx build)");
                    job.logf("PROGRESS: 1/4 Building Fuchsia image (fx build - takes 1-3 minutes)");
                    dev.build(&sdir2.join("fx-build.log"))?;
                    // Never flash after a cancel.
                    job.check_cancel()?;
                    return dev.ota(600);
                }
                Ok(json!({"written": true}))
            });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        "ota" => {
            let build = bool_of(body, "build", true);
            let (st2, sdir2) = (st.clone(), sdir.clone());
            let job = st.start(&format!("ota {name}"), Some(name.to_string()), move |job| {
                let dev = st2.device(job.log_fn());
                if build {
                    job.logf("STATUS: Building Fuchsia image (fx build)");
                    job.logf("PROGRESS: 1/4 Building Fuchsia image (fx build - takes 1-3 minutes)");
                    dev.build(&sdir2.join("fx-build.log"))?;
                }
                // Never flash after a cancel.
                job.check_cancel()?;
                dev.ota(600)
            });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        "explain" => {
            let mname = s(body, "name").to_string();
            if !valid_segment(&mname) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let lm = st.llm_models();
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .or_else(|| st.prefs().get("llm_model").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| lm["default"].as_str().unwrap_or("").to_string());
            let model_name =
                lm["names"].get(&model).and_then(Value::as_str).unwrap_or(&model).to_string();
            st.save_pref("llm_model", json!(model));
            let (st2, sdir2) = (st.clone(), sdir.clone());
            let job =
                st.start(&format!("explain {name}/{mname}"), Some(name.to_string()), move |job| {
                    let agent = st2.agent.clone().context("no agent backend is configured")?;
                    let sess = Session::load(&sdir2)?;
                    let md = std::fs::read_to_string(sdir2.join(&mname).join("report.md"))?;
                    let summ = read_json(&sdir2.join(&mname).join("summary.json"))?;
                    let access = agent.query_access(
                        &st2.prefs(),
                        &trace_context::allow_rule(st2.ctx.sessions.tp_bin()),
                    );
                    job.logf(&format!("STATUS: Loading the recorded traces for {}", agent.name));
                    job.logf("PROGRESS: 0/2 Loading the recorded traces (a few seconds each)");
                    let j2 = job.clone();
                    let traces =
                        trace_context::build(&st2.ctx, &sdir2, &mname, &summ, &move |m| {
                            j2.logf(m)
                        })?;
                    job.check_cancel()?;
                    let queries = access.allowed && traces["warm"].as_bool().unwrap_or(false);
                    job.logf(&format!(
                        "trace queries {}: {}",
                        if queries { "on" } else { "off" },
                        access.reason
                    ));
                    job.logf(&format!(
                        "STATUS: Asking {} ({model_name}) to explain the verdict",
                        agent.name
                    ));
                    let took = if queries {
                        "up to ~10 minutes with trace queries"
                    } else {
                        "takes 1-2 minutes"
                    };
                    job.logf(&format!(
                        "PROGRESS: 1/2 {} is analyzing the report ({took})",
                        agent.name
                    ));
                    let text = crate::llm::explain(
                        &agent,
                        &md,
                        &st2.ctx.fuchsia_dir.display().to_string(),
                        &sess.task,
                        &model,
                        Some(&traces),
                        queries,
                        summ.get("run_functions"),
                        &st2.ctx.lessons,
                    )?;
                    job.check_cancel()?;
                    runner::write_atomic(&sdir2.join(&mname).join("explain.md"), text.as_bytes())?;
                    let when = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
                    runner::write_atomic(
                        &sdir2.join(&mname).join("explain.json"),
                        serde_json::to_string(
                            &json!({"model": model, "model_name": model_name, "when": when}),
                        )?
                        .as_bytes(),
                    )?;
                    Ok(json!({"explain_md": text, "model": model}))
                });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        "resolve-roles" => {
            let mname = s(body, "name").to_string();
            if !valid_segment(&mname) {
                return Ok(Response::Json(400, json!({"error": "invalid measurement name"})));
            }
            let (st2, sdir2) = (st.clone(), sdir.clone());
            let job = st.start(&format!("resolve roles {name}/{mname}"), Some(name.to_string()), move |job| {
                let agent = st2.agent.clone().context("no agent backend is configured")?;
                let sp = sdir2.join(&mname).join("summary.json");
                let summ = read_json(&sp)?;
                let threads: Vec<Value> = summ
                    .get("proposals")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        let cands: Vec<Value> =
                            p.get("roles").and_then(Value::as_array).into_iter().flatten().map(|r| r["role"].clone()).collect();
                        json!({"label": p["label"], "deadline_params_ns": p["existing"], "candidates": cands})
                    })
                    .collect();
                job.logf(&format!("STATUS: Confirming role names from source with {}", agent.name));
                job.logf("PROGRESS: 0/1 Searching Fuchsia source for role names (takes 1-2 minutes)");
                let fd = st2.ctx.fuchsia_dir.display().to_string();
                // The checkout's roles files, when the build directory can name
                // them; the agent reads the source either way.
                let dev = st2.device(job.log_fn());
                let pp = dev
                    .roles_files(&dev.build_dir())
                    .map(|fs| {
                        fs.iter()
                            .map(|f| st2.ctx.fuchsia_dir.join(f).display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|_| "the board's .profiles file".into());
                job.check_cancel()?;
                let model = st2.llm_models()["default"].as_str().unwrap_or("").to_string();
                let mapping = crate::llm::resolve_roles(&agent, &json!(threads), &fd, &pp, &model)?;
                job.check_cancel()?;
                // Re-read: the summary may have changed during the agent's
                // minutes (a re-analysis, say).
                let mut summ = read_json(&sp)?;
                if let Some(props) = summ.get_mut("proposals").and_then(Value::as_array_mut) {
                    for p in props {
                        let label = s(p, "label").to_string();
                        if let Some(role) = mapping.get(&label) {
                            let mut rs = vec![json!({"role": role, "score": 9.0, "why": "confirmed by agent from source"})];
                            rs.extend(
                                p.get("roles").and_then(Value::as_array).into_iter().flatten().filter(|r| s(r, "role") != role).cloned(),
                            );
                            p["roles"] = json!(rs);
                        }
                    }
                }
                runner::write_atomic(&sp, serde_json::to_string_pretty(&summ)?.as_bytes())?;
                Ok(json!({"mapping": mapping}))
            });
            Ok(Response::Json(200, json!({"job": job.id})))
        }
        _ => Ok(Response::Json(404, json!({"error": "unknown endpoint"}))),
    }
}

/// Uploads one capture with `ffx trace upload` and caches the link.
fn upload_trace(st: &Arc<State>, sdir: &Path, mname: &str, tag: &str) -> Value {
    let Some(path) = summary::trace_path(sdir, mname, tag) else {
        return json!({"error": format!("no trace file for {mname}/{tag}")});
    };
    let dev = st.device(Arc::new(|_| {}));
    let cmd = vec![
        dev.ffx.display().to_string(),
        "trace".into(),
        "upload".into(),
        path.display().to_string(),
    ];
    let out = dev.run(&cmd, false, TRACE_UPLOAD).map(|o| o.combined()).unwrap_or_default();
    let re = regex_lite::Regex::new(r"https://[A-Za-z0-9.-]+/trace/[A-Za-z0-9_./-]+")
        .expect("the pattern is a valid regex");
    let Some(m) = re.find(&out) else {
        let tail: String =
            out.trim().chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        return json!({"error": format!("ffx trace upload did not return a trace viewer link:\n{tail}")});
    };
    let cache = sdir.join(mname).join("trace_urls.json");
    let mut urls = read_json(&cache).unwrap_or(json!({}));
    urls[tag] = json!(m.as_str());
    if let Ok(s) = serde_json::to_string_pretty(&urls) {
        let _ = std::fs::write(&cache, s);
    }
    json!({"ok": true, "url": m.as_str()})
}

/// Serves the UI until killed.
/// How [`serve`] runs. Its `Debug` leaves out the token.
pub struct ServeOptions {
    /// The address to listen on.
    pub bind: String,
    pub port: u16,
    /// The access token (see [`crate::ui_daemon::ui_token`]).
    pub token: String,
    /// This machine's name for the printed link.
    pub host: String,
    /// Where to record the running server (see [`crate::ui_daemon`]).
    pub state_dir: PathBuf,
    /// Whether to print the link with the token. The background server's
    /// output goes to a log file, which must not hold the token; `zpo ui
    /// start` prints the link to the user instead.
    pub print_link: bool,
}

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeOptions")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("host", &self.host)
            .field("state_dir", &self.state_dir)
            .field("print_link", &self.print_link)
            .finish_non_exhaustive()
    }
}

/// Runs the UI server until `POST /api/shutdown`.
pub fn serve(opts: ServeOptions, st: Arc<State>) -> anyhow::Result<()> {
    let listener = match TcpListener::bind((opts.bind.as_str(), opts.port)) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let probe = crate::ui_daemon::SystemProbe;
            bail!(
                "{}",
                crate::ui_daemon::port_taken_message(
                    &opts.state_dir,
                    &opts.bind,
                    opts.port,
                    &probe
                )
            );
        }
        Err(e) => return Err(e.into()),
    };
    let record = ServerState {
        pid: std::process::id(),
        port: opts.port,
        bind: opts.bind.clone(),
        host: opts.host.clone(),
        sessions: st.sessions_dir.display().to_string(),
        started: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        instance: crate::ui_daemon::random_token()?,
        // However it was found, so a restart keeps it like the other settings.
        fuchsia_dir: Some(st.ctx.fuchsia_dir.display().to_string()),
    };
    let _ = st.instance.set(record.instance.clone());
    record.write(&opts.state_dir)?;
    let _ = st.host.set(opts.host.clone());
    let co = &st.ctx.checkout;
    let open = if opts.print_link {
        format!(
            "Open this in your browser:\n\n    {}\n\n\
             Anyone with this link can run commands as you on this machine; keep it private.",
            link(&opts.host, opts.port, &opts.token)
        )
    } else {
        format!("Port {}; open the link `zpo ui start` printed.", opts.port)
    };
    println!(
        "\nzpo UI is running. {open}\nFuchsia checkout: {} (from {})\n(sessions in {})\n",
        co.path,
        co.source,
        st.sessions_dir.display()
    );
    {
        // Exits once a shutdown was requested and the requests in flight are
        // answered, or after 5 s.
        let st = st.clone();
        let state_dir = opts.state_dir.clone();
        std::thread::spawn(move || {
            while !st.stop.load(Ordering::SeqCst) {
                std::thread::sleep(POLL);
            }
            let t0 = Instant::now();
            while st.inflight.load(Ordering::SeqCst) > 0 && t0.elapsed() < SHUTDOWN_DRAIN {
                std::thread::sleep(DRAIN_POLL);
            }
            ServerState::remove(&state_dir);
            // `exit` skips destructors: stop the jobs (a capture stops its
            // stress load and trace on cancel) and the warm trace processors
            // first.
            let jobs: Vec<Arc<Job>> = lock(&st.jobs).values().cloned().collect();
            for j in &jobs {
                j.cancel();
            }
            let t0 = Instant::now();
            while jobs.iter().any(|j| j.state() == "running") && t0.elapsed() < STOP_WAIT {
                std::thread::sleep(POLL);
            }
            st.ctx.sessions.kill_started();
            println!("zpo UI server stopped (POST /api/shutdown).");
            std::process::exit(0);
        });
    }
    let access = Arc::new(Access { token: opts.token });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (st, access) = (st.clone(), access.clone());
        std::thread::spawn(move || handle(&st, &access, stream));
    }
    Ok(())
}

/// `GET /api/session/<name>/report.html`: the session's static HTML report
/// (see [`crate::report_html`]) as a download.
fn report_download(st: &Arc<State>, path: &str) -> Option<Response> {
    let name = percent_decode(path.strip_prefix("/api/session/")?.strip_suffix("/report.html")?);
    if !valid_segment(&name) {
        return Some(Response::NotFound);
    }
    let dir = st.sdir(&name);
    if !crate::report_html::session_has_report(&dir) {
        let why = "this session has no analyzed measurement to report";
        return Some(Response::Json(404, json!({"error": why})));
    }
    Some(match crate::report_html::render(&st.ctx, &[dir], Some(&name)) {
        Ok(html) => Response::Bytes {
            code: 200,
            ctype: "text/html; charset=utf-8",
            body: html.into_bytes(),
            headers: vec![format!("Content-Disposition: attachment; filename=\"zpo-{name}.html\"")],
        },
        Err(e) => Response::Json(500, json!({"error": format!("{e:#}")})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> Arc<State> {
        let ctx = Ctx {
            fuchsia_dir: dir.to_path_buf(),
            sessions: Arc::new(crate::tp::session::Sessions::new(
                Arc::new(crate::proc::HostRunner),
                dir.join("tp"),
            )),
            ext_dirs: vec![],
            checkout: Default::default(),
            lessons: Default::default(),
        };
        Arc::new(State::new(dir.join("sessions"), ctx, None).unwrap())
    }

    fn req(method: &str, path: &str, body: &str) -> Request {
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.into(),
            path: p.into(),
            query: q.into(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn json_of(r: Response) -> (u16, Value) {
        match r {
            Response::Json(c, v) => (c, v),
            _ => panic!("not JSON"),
        }
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn headers(token: &str, post_headers: bool) -> Headers {
        Headers { x_zpo: post_headers, json: post_headers, token: token.into() }
    }

    fn code(r: Response) -> u16 {
        match r {
            Response::Json(c, _) | Response::Bytes { code: c, .. } => c,
            Response::Html(_) | Response::File { .. } => 200,
            Response::NotFound => 404,
        }
    }

    #[test]
    fn every_api_call_needs_the_token_but_the_page_does_not() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let a = Access { token: TOKEN.into() };
        let call = |path: &str, h: &Headers| respond(&st, &a, &req("GET", path, ""), h);
        // The page holds no data.
        assert!(matches!(call("/", &headers("", false)), Response::Html(_)));
        // Everything else: 403 with a reason, unless the header carries the
        // token.
        let (c, v) = json_of(call("/api/sessions", &headers("", false)));
        assert_eq!(c, 403);
        assert_eq!(v["signed_in"], false);
        assert!(v["error"].as_str().unwrap().contains("open the link `zpo ui start` prints"));
        assert_eq!(code(call("/api/sessions", &headers("nope", false))), 403);
        assert_eq!(code(call("/api/ping", &headers("", false))), 403);
        assert_eq!(code(call("/api/sessions", &headers(TOKEN, false))), 200);
        assert_eq!(code(call("/api/ping", &headers(TOKEN, false))), 200);
        // Unknown paths are refused before they are looked up.
        assert_eq!(code(call("/favicon.ico", &headers("", false))), 403);
        // The token is not accepted in the query string.
        assert_eq!(code(call(&format!("/api/sessions?token={TOKEN}"), &headers("", false))), 403);
    }

    #[test]
    fn ping_and_info_name_the_server() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let a = Access { token: TOKEN.into() };
        let call =
            |path: &str| json_of(respond(&st, &a, &req("GET", path, ""), &headers(TOKEN, false)));
        let (c, v) = call("/api/info");
        assert_eq!((c, v["host"].clone()), (200, Value::Null));
        st.host.set("ws.example.com".into()).unwrap();
        for p in ["/api/info", "/api/ping"] {
            let (c, v) = call(p);
            assert_eq!(c, 200);
            assert_eq!(v["host"], "ws.example.com");
            assert_eq!(v["shell"], shell_version());
        }
        // Like every API call, it needs the token.
        let r = respond(&st, &a, &req("GET", "/api/info", ""), &headers("", false));
        assert_eq!(code(r), 403);
    }

    #[test]
    fn serves_the_service_worker_for_this_build() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let a = Access { token: TOKEN.into() };
        // Without a token, like the page.
        let Response::Bytes { code, ctype, body, headers: extra } =
            respond(&st, &a, &req("GET", "/sw.js", ""), &headers("", false))
        else {
            panic!("no service worker");
        };
        assert_eq!((code, ctype), (200, "text/javascript; charset=utf-8"));
        assert_eq!(extra, vec!["Cache-Control: no-cache"]);
        let js = String::from_utf8(body).unwrap();
        assert!(js.contains(&format!("'zpo-shell-{}'", shell_version())), "{js}");
        assert!(!js.contains("__ZPO_SHELL_VERSION__"));
        assert_eq!(shell_version().len(), 12);
    }

    #[test]
    fn every_response_carries_the_zpo_marker() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        let responses = vec![
            Response::Json(200, json!({})),
            Response::Json(403, json!({"error": "x", "signed_in": false})),
            Response::Json(500, json!({"error": "x"})),
            Response::Html("<html></html>"),
            Response::File { path: file.path().to_path_buf(), filename: "f".into() },
            Response::Bytes { code: 404, ctype: "text/plain", body: vec![], headers: vec![] },
            Response::NotFound,
        ];
        for r in responses {
            let mut c = TcpStream::connect(addr).unwrap();
            let (mut s, _) = l.accept().unwrap();
            let html = matches!(r, Response::Html(_));
            write_response(&mut s, r, false, "i1").unwrap();
            drop(s);
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            assert!(out.contains("\r\nX-Zpo: 1\r\nX-Zpo-Instance: i1\r\n"), "{out}");
            assert_eq!(out.contains("\r\nCache-Control: no-cache\r\n"), html, "{out}");
        }
    }

    #[test]
    fn posts_need_the_custom_header_and_json() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let a = Access { token: TOKEN.into() };
        let body = r#"{"name": "touch", "task": "thread:a/b"}"#;
        let r = |h: &Headers| respond(&st, &a, &req("POST", "/api/sessions/validate", body), h);
        // Refused, but the page has the token: no token banner.
        let (c, v) = json_of(r(&headers(TOKEN, false)));
        assert_eq!((c, v["signed_in"].clone()), (403, json!(true)));
        let mut h = headers(TOKEN, true);
        h.json = false;
        assert_eq!(code(r(&h)), 403);
        let mut h = headers(TOKEN, true);
        h.x_zpo = false;
        assert_eq!(code(r(&h)), 403);
        assert_eq!(code(r(&headers(TOKEN, true))), 200);
    }

    #[test]
    fn reads_the_token_header_only_and_caps_the_body() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let send = |text: String| {
            let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
            c.write_all(text.as_bytes()).unwrap();
            let (mut s, _) = l.accept().unwrap();
            (read_request(&mut s), c)
        };
        let (r, _c) = send(format!(
            "GET /api/ping HTTP/1.1\r\nCookie: zpo_token=x\r\nX-Zpo-Token: {TOKEN}\r\n\r\n"
        ));
        let (r, h) = r.unwrap();
        assert_eq!((r.path.as_str(), h.token.as_str()), ("/api/ping", TOKEN));
        let (r, _c) =
            send(format!("POST /api/x HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1));
        assert!(r.is_err());
    }

    #[test]
    fn finished_jobs_are_pruned() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let jobs: Vec<Arc<Job>> =
            (0..KEEP_FINISHED_JOBS + 10).map(|_| st.start("t", None, |_| Ok(json!({})))).collect();
        while jobs.iter().any(|j| j.state() == "running") {
            std::thread::sleep(Duration::from_millis(10));
        }
        let last = st.start("t", None, |_| Ok(json!({})));
        let kept = st.jobs.lock().unwrap();
        assert!(kept.len() <= KEEP_FINISHED_JOBS + 1, "{}", kept.len());
        assert!(kept.contains_key(&last.id));
        assert!(!kept.contains_key(&jobs[0].id));
    }

    #[test]
    fn shutdown_asks_the_server_to_stop() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let a = Access { token: TOKEN.into() };
        assert_eq!(
            code(respond(&st, &a, &req("POST", "/api/shutdown", "{}"), &headers("", true))),
            403
        );
        assert!(!st.stop.load(Ordering::SeqCst));
        let r = respond(&st, &a, &req("POST", "/api/shutdown", "{}"), &headers(TOKEN, true));
        assert_eq!(json_of(r), (200, json!({"ok": true})));
        assert!(st.stop.load(Ordering::SeqCst));
    }

    #[test]
    fn the_cpu_fit_check_carries_the_dialog_text() {
        assert_eq!(cpu_fit_json(Ok(true)), json!({"checked": true}));
        let v = cpu_fit_json(Ok(false));
        assert_eq!(v["checked"], false);
        assert_eq!(v["title"], crate::topology::NO_CORE_IDS_TITLE);
        assert_eq!(v["message"], crate::topology::NO_CORE_IDS_WARNING);
        let v = cpu_fit_json(Err(anyhow::anyhow!("no device")));
        assert_eq!(v, json!({"error": "no device"}));
    }

    #[test]
    fn creates_lists_renames_and_deletes_sessions() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let body = json!({"name": "touch", "task": "thread:a/b", "instructions": "Swipe\n\nTap", "window": "850us"});
        let (c, v) = json_of(post(&st, &req("POST", "/api/sessions", ""), &body).unwrap());
        assert_eq!(c, 200, "{v}");
        let (_, list) = json_of(get(&st, &req("GET", "/api/sessions", "")));
        assert_eq!(list[0]["name"], "touch");
        assert_eq!(list[0]["load"]["instructions"], json!(["Swipe", "Tap"]));
        assert_eq!(list[0]["window"], "850us");
        assert_eq!(list[0]["window_ns"], 850_000);
        assert_eq!(list[0]["task_fields"]["kind"], "thread");
        assert_eq!(list[0]["task_fields"]["thread"], "a/b");
        let (c, _) = json_of(
            post(&st, &req("POST", "/api/session/touch/rename", ""), &json!({"new_name": "swipe"}))
                .unwrap(),
        );
        assert_eq!(c, 200);
        assert!(d.path().join("sessions/swipe/session.json").exists());
        let s = Session::load(&d.path().join("sessions/swipe")).unwrap();
        assert_eq!(s.dir, d.path().join("sessions/swipe").display().to_string());
        json_of(post(&st, &req("POST", "/api/session/swipe/delete", ""), &json!({})).unwrap());
        assert!(!d.path().join("sessions/swipe").exists());
    }

    #[test]
    fn jobs_prompt_ack_and_cancel() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let job = st.start("t", Some("s".into()), |job| {
            job.logf("PROGRESS: 1/3 Doing things");
            job.logf("CAPTURE: idle-1 start");
            job.ask("Swipe now")?;
            job.logf("STATUS: Recording for 15 s");
            Ok(json!({"done": 1}))
        });
        for _ in 0..200 {
            if job.to_json(None)["prompt"] == "Swipe now" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let v = job.to_json(None);
        assert_eq!(v["prompt"], "Swipe now");
        assert_eq!(v["progress"], json!({"k": 1, "n": 3, "label": "Doing things"}));
        assert_eq!(v["captures"], json!({"idle-1": "start"}));
        json_of(
            post(&st, &req("POST", &format!("/api/jobs/{}/ack", job.id), ""), &json!({})).unwrap(),
        );
        for _ in 0..200 {
            if job.state() == "done" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job.state(), "done");
        assert_eq!(job.to_json(None)["status"], "Recording for 15 s");

        let job = st.start("t", Some("s".into()), |job| {
            job.ask("Wait")?;
            Ok(json!({}))
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(st.cancel_session_jobs("s"), 1);
        for _ in 0..200 {
            if job.state() != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job.state(), "cancelled");
    }

    #[test]
    fn errors_with_fix_it_text_keep_their_info() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let job = st.start("t", None, |_| {
            Err(UserFacingError::new(json!({"title": "Nothing to flash", "reason": "no bundle"}))
                .into())
        });
        for _ in 0..200 {
            if job.state() != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let v = job.to_json(None);
        assert_eq!(v["state"], "error");
        assert_eq!(v["error_info"]["title"], "Nothing to flash");
        assert_eq!(v["error"], "Nothing to flash: no bundle");
    }

    #[test]
    fn serves_the_page_and_unknown_paths() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        assert!(matches!(get(&st, &req("GET", "/", "")), Response::Html(_)));
        assert!(matches!(get(&st, &req("GET", "/nope", "")), Response::NotFound));
        let (c, _) = json_of(get(&st, &req("GET", "/api/session/x/baseline/summary", "")));
        assert_eq!(c, 404);
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("%c3%a9"), "é");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("%aé"), "%aé");
    }

    #[test]
    fn sessions_are_validated_by_field() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let body = json!({"name": "t", "task_kind": "events", "start": "irq:1", "end": "",
                          "window": "12 parsecs", "duration": "0", "repeats": 1, "buffer_mb": 64});
        let (c, v) = json_of(post(&st, &req("POST", "/api/sessions/validate", ""), &body).unwrap());
        assert_eq!(c, 200);
        let mut keys: Vec<&String> = v["errors"].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["dur", "end", "window"], "{v}");
        assert_eq!(v["task"], "events:irq:1->");
        let (c, v) = json_of(post(&st, &req("POST", "/api/sessions", ""), &body).unwrap());
        assert_eq!(c, 400);
        assert!(v["errors"]["window"].is_string(), "{v}");
        // A bare number is microseconds, and either micro sign works.
        for w in ["850", "850us", "850\u{b5}s", "850\u{3bc}s"] {
            let ok = json!({"name": "t", "task_kind": "fidl", "fidl": "a.b/C.D", "window": w});
            let (_, v) =
                json_of(post(&st, &req("POST", "/api/sessions/validate", ""), &ok).unwrap());
            assert_eq!(v["errors"], json!({}), "{w}: {v}");
            assert_eq!(v["window_ns"], 850_000, "{w}");
            assert_eq!(v["task"], "fidl:a.b/C.D");
        }
    }

    /// Waits for a job to leave "running".
    fn finish(job: &Job) -> String {
        for _ in 0..500 {
            if job.state() != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        job.state()
    }

    #[test]
    fn discover_needs_a_positive_duration() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        for dur in [json!(-1.0), json!(0)] {
            let body = json!({"description": "swipe", "duration": dur});
            let (c, v) = json_of(post(&st, &req("POST", "/api/discover", ""), &body).unwrap());
            assert_eq!(c, 400, "{dur}: {v}");
            assert!(v["error"].as_str().unwrap().contains("positive"), "{v}");
        }
        assert!(st.jobs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_cancelled_build_never_flashes_the_device() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        // A fake `fx` that logs its calls and takes a while to build; the
        // OTA would start with `fx get-build-dir`.
        let bin = d.path().join(".jiri_root/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script =
            "#!/bin/sh\necho \"$*\" >> \"$0.log\"\n[ \"$1\" = build ] && sleep 1\nexit 0\n";
        crate::checkout::testing::write_test_script(&bin.join("fx"), script);
        let (c, v) = json_of(
            post(&st, &req("POST", "/api/session/s/ota", ""), &json!({"build": true})).unwrap(),
        );
        assert_eq!(c, 200, "{v}");
        let id = v["job"].as_str().unwrap().to_string();
        let job = st.jobs.lock().unwrap()[&id].clone();
        for _ in 0..200 {
            if bin.join("fx.log").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(st.cancel_session_jobs("s"), 1);
        assert_eq!(finish(&job), "cancelled");
        assert_eq!(std::fs::read_to_string(bin.join("fx.log")).unwrap(), "build\n");
    }

    #[test]
    fn agent_jobs_wait_for_a_running_job_instead_of_cancelling_it() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let release = Arc::new(AtomicBool::new(false));
        let r2 = release.clone();
        let job = st.start("t", Some("s".into()), move |job| {
            while !r2.load(Ordering::SeqCst) {
                job.check_cancel()?;
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(json!({}))
        });
        for action in ["explain", "resolve-roles"] {
            let path = format!("/api/session/s/{action}");
            let body = json!({"name": "baseline"});
            let (c, v) = json_of(post(&st, &req("POST", &path, ""), &body).unwrap());
            assert_eq!(c, 409, "{action}: {v}");
            assert!(v["error"].as_str().unwrap().contains("a job is running"), "{v}");
        }
        assert_eq!(st.jobs.lock().unwrap().len(), 1); // nothing started
        assert_eq!(job.state(), "running");
        assert!(job.check_cancel().is_ok());
        release.store(true, Ordering::SeqCst);
        assert_eq!(finish(&job), "done");
    }

    #[test]
    fn rejects_traversal_and_matches_exact_gates() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        for bad in ["..", ".", "a/b", "a\\b", "a\0b"] {
            let (c, _) = json_of(
                post(&st, &req("POST", "/api/sessions", ""), &json!({"name": bad})).unwrap(),
            );
            assert_eq!(c, 400);
        }
        let (c, _) = json_of(get(&st, &req("GET", "/api/session/..%2fx/baseline/summary", "")));
        assert_eq!(c, 400);
        let (c, _) = json_of(
            post(&st, &req("POST", "/api/session/touch/discard", ""), &json!({"name": "../x"}))
                .unwrap(),
        );
        assert_eq!(c, 400);

        assert!(gate_mentions("gate-baseline-vs-candidate-1.json", "candidate-1"));
        assert!(gate_mentions("gate-candidate-1-vs-candidate-2.json", "candidate-1"));
        assert!(!gate_mentions("gate-baseline-vs-candidate-10.json", "candidate-1"));
        assert!(!gate_mentions("gate-candidate-10-vs-candidate-2.json", "candidate-1"));
    }
}
