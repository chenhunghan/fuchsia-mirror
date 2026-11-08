// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Warm trace processor sessions: a capture loaded once, then queried many
//! times.
//!
//! Built on `trace_processor_shell`'s own `server unix` sessions (Perfetto
//! v59+): named, daemonized, and reaped by the trace processor after 30 idle
//! minutes, so zpo keeps no pool of its own. The session name is derived from
//! the trace path, so every caller (`zpo ui`, the agent, other zpo processes)
//! shares one session per capture.
//!
//! Loading a capture takes a few seconds and a session holds roughly 2.7x the
//! trace size in memory (measured: 626 MB for a 233 MB capture).

use super::client::Client;
use crate::proc::Runner;
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long an unused session lives.
pub const IDLE_TIMEOUT: &str = "30m";
/// The first trace processor release with `server unix` and `query --remote`.
pub const MIN_VERSION: u32 = 59;

/// Starts, finds and stops warm sessions.
pub struct Sessions {
    runner: Arc<dyn Runner>,
    tp: PathBuf,
    version: Mutex<HashMap<PathBuf, Option<u32>>>,
    started: Mutex<HashSet<String>>,
    chunk_rows: usize,
}

impl Sessions {
    /// Sessions of the trace processor at `tp`.
    pub fn new(runner: Arc<dyn Runner>, tp: PathBuf) -> Self {
        Sessions {
            runner,
            tp,
            version: Mutex::new(HashMap::new()),
            started: Mutex::new(HashSet::new()),
            chunk_rows: super::client::CHUNK_ROWS,
        }
    }

    /// Uses smaller result chunks for every client (tests).
    pub fn with_chunk_rows(mut self, n: usize) -> Self {
        self.chunk_rows = n;
        self
    }

    /// The trace processor binary.
    pub fn tp_bin(&self) -> &Path {
        &self.tp
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::io::Result<crate::proc::Output> {
        let mut v = vec![self.tp.display().to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        self.runner.run(&v, timeout)
    }

    /// The trace processor's major version (59 for v59.1), or None if unknown.
    pub fn major_version(&self) -> Option<u32> {
        let mut cache = self.version.lock().unwrap();
        if let Some(v) = cache.get(&self.tp) {
            return *v;
        }
        let v = self.run(&["--version"], Duration::from_secs(20)).ok().and_then(|o| {
            let re = regex_lite::Regex::new(r"Perfetto v(\d+)").unwrap();
            re.captures(&o.stdout).and_then(|c| c[1].parse().ok())
        });
        if v.is_some() {
            cache.insert(self.tp.clone(), v);
        }
        v
    }

    /// Whether this trace processor supports warm sessions.
    pub fn supported(&self) -> bool {
        self.major_version().is_some_and(|v| v >= MIN_VERSION)
    }

    /// Whether a warm session with this name answers a query.
    pub fn alive(&self, name: &str) -> bool {
        self.run(
            &["query", "--remote", name, "--no-agent", "--quiet", "SELECT 1"],
            Duration::from_secs(20),
        )
        .is_ok_and(|o| o.success())
    }

    /// The name of a warm session for `trace`, starting one if needed.
    pub fn ensure(&self, trace: &Path, log: &dyn Fn(&str)) -> anyhow::Result<String> {
        let ver = self.major_version();
        if !ver.is_some_and(|v| v >= MIN_VERSION) {
            if ver.is_none() && !self.tp.exists() {
                bail!(
                    "no trace processor at {}; rebuild zpo (`fx build --host //zircon/skills/zpo`), \
                     which installs it next to the binary, or set $ZPO_TRACE_PROCESSOR",
                    self.tp.display()
                );
            }
            bail!(
                "zpo needs trace processor v{MIN_VERSION} or newer ({} reports {}); update the Fuchsia checkout",
                self.tp.display(),
                ver.map_or("no version".to_string(), |v| format!("v{v}"))
            );
        }
        let name = name_for(trace);
        if self.alive(&name) {
            return Ok(name);
        }
        let base = trace.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        log(&format!("loading {base} into warm session {name}"));
        let trace_s = trace.display().to_string();
        let out = self
            .run(
                &[
                    "server",
                    "unix",
                    "--name",
                    &name,
                    "--daemonize",
                    "--idle-timeout",
                    IDLE_TIMEOUT,
                    "--quiet",
                    &trace_s,
                ],
                Duration::from_secs(600),
            )
            .with_context(|| format!("could not run {}", self.tp.display()))?;
        // Another zpo process may have started the same session first, in which
        // case ours fails to bind the name but the session is alive and usable.
        if !self.alive(&name) {
            let msg = out.combined();
            let msg = msg.trim();
            let tail: String =
                msg.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
            bail!("the trace processor could not load {}: {}", trace.display(), tail);
        }
        self.started.lock().unwrap().insert(name.clone());
        Ok(name)
    }

    /// A client for a session.
    pub fn client(&self, name: &str) -> Client {
        Client::new(self.runner.clone(), self.tp.clone(), name.to_string())
            .with_chunk_rows(self.chunk_rows)
    }

    /// Loads `trace` (or reuses its session) and returns a client for it.
    pub fn open(&self, trace: &Path, log: &dyn Fn(&str)) -> anyhow::Result<Client> {
        let name = self.ensure(trace, log)?;
        Ok(self.client(&name))
    }

    /// Stops a session.
    pub fn kill(&self, name: &str) {
        let _ = self.run(&["server", "kill", name], Duration::from_secs(20));
        self.started.lock().unwrap().remove(name);
    }

    /// Stops the sessions this process started (the idle timeout covers crashes).
    pub fn kill_started(&self) {
        let names: Vec<String> = self.started.lock().unwrap().iter().cloned().collect();
        for n in names {
            self.kill(&n);
        }
    }

    /// The sessions this process started.
    pub fn started(&self) -> Vec<String> {
        let mut v: Vec<String> = self.started.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }
}

/// The session name for a trace: `zpo-` and 12 hex digits of the SHA-256 of
/// its real path, size and modification time. Every zpo process finds the same
/// session for the same file, and a file re-recorded at the same path gets a
/// new session instead of the old trace still loaded in memory.
pub fn name_for(trace: &Path) -> String {
    let real = std::fs::canonicalize(trace).unwrap_or_else(|_| absolute(trace));
    let mut key = real.to_string_lossy().into_owned();
    if let Ok(m) = std::fs::metadata(&real) {
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        key.push_str(&format!("\0{}\0{mtime}", m.len()));
    }
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("zpo-{}", &hex[..12])
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::Output;
    use std::sync::Mutex as StdMutex;

    /// Plays `trace_processor_shell`: --version, query, server unix, server kill.
    struct FakeTp {
        version: StdMutex<String>,
        live: StdMutex<HashSet<String>>,
        /// `server unix` fails to bind the name, as if another process got it
        /// first, but the session is up.
        lose_race: StdMutex<bool>,
        calls: StdMutex<Vec<Vec<String>>>,
    }

    impl FakeTp {
        fn new(version: &str) -> Arc<Self> {
            Arc::new(FakeTp {
                version: StdMutex::new(version.into()),
                live: StdMutex::new(HashSet::new()),
                lose_race: StdMutex::new(false),
                calls: StdMutex::new(Vec::new()),
            })
        }
        fn starts(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c[0] == "server" && c[1] == "unix")
                .cloned()
                .collect()
        }
    }

    impl Runner for FakeTp {
        fn run(&self, args: &[String], _timeout: Duration) -> std::io::Result<Output> {
            let a: Vec<String> = args[1..].to_vec();
            self.calls.lock().unwrap().push(a.clone());
            let ok = Output { code: Some(0), ..Default::default() };
            let fail =
                Output { code: Some(1), stderr: "no such session".into(), ..Default::default() };
            Ok(match (a[0].as_str(), a.get(1).map(String::as_str)) {
                ("--version", _) => Output {
                    code: Some(0),
                    stdout: self.version.lock().unwrap().clone(),
                    ..Default::default()
                },
                ("query", _) => {
                    if self.live.lock().unwrap().contains(&a[2]) {
                        ok
                    } else {
                        fail
                    }
                }
                ("server", Some("unix")) => {
                    let i = a.iter().position(|x| x == "--name").unwrap();
                    self.live.lock().unwrap().insert(a[i + 1].clone());
                    if *self.lose_race.lock().unwrap() {
                        Output { code: Some(1), stderr: "name in use".into(), ..Default::default() }
                    } else {
                        ok
                    }
                }
                ("server", Some("kill")) => {
                    self.live.lock().unwrap().remove(&a[2]);
                    ok
                }
                _ => fail,
            })
        }
    }

    fn sessions(fake: &Arc<FakeTp>) -> Sessions {
        Sessions::new(fake.clone(), PathBuf::from("/fake/tp"))
    }

    #[test]
    fn starts_once_then_reuses() {
        let fake = FakeTp::new("Perfetto v59.1-1e9c61e98");
        let s = sessions(&fake);
        let t = Path::new("/traces/stress-1.fxt");
        let name = s.ensure(t, &|_| {}).unwrap();
        assert_eq!(name, name_for(t));
        assert_eq!(s.ensure(t, &|_| {}).unwrap(), name);
        let starts = fake.starts();
        assert_eq!(starts.len(), 1);
        assert!(starts[0].contains(&"--daemonize".to_string()));
        assert!(starts[0].contains(&IDLE_TIMEOUT.to_string()));
    }

    #[test]
    fn restarts_after_the_session_died() {
        let fake = FakeTp::new("Perfetto v59.1-1e9c61e98");
        let s = sessions(&fake);
        let t = Path::new("/traces/idle-1.fxt");
        let name = s.ensure(t, &|_| {}).unwrap();
        fake.live.lock().unwrap().remove(&name); // reaped by the idle timeout
        assert_eq!(s.ensure(t, &|_| {}).unwrap(), name);
        assert_eq!(fake.starts().len(), 2);
    }

    #[test]
    fn kill_started_stops_only_ours() {
        let fake = FakeTp::new("Perfetto v59.1-1e9c61e98");
        let s = sessions(&fake);
        let ours = s.ensure(Path::new("/traces/a.fxt"), &|_| {}).unwrap();
        fake.live.lock().unwrap().insert("someone-elses".into());
        s.kill_started();
        let live = fake.live.lock().unwrap().clone();
        assert_eq!(live, HashSet::from(["someone-elses".to_string()]));
        assert!(!live.contains(&ours));
    }

    #[test]
    fn old_trace_processor_is_an_error() {
        let fake = FakeTp::new("Perfetto v58.3-11fbaed83");
        let s = sessions(&fake);
        let e = s.ensure(Path::new("/traces/a.fxt"), &|_| {}).unwrap_err();
        assert!(e.to_string().contains("v59"), "{e}");
        assert!(fake.starts().is_empty());
    }

    #[test]
    fn unparseable_version_is_not_cached() {
        let fake = FakeTp::new("");
        let s = sessions(&fake);
        assert_eq!(s.major_version(), None);
        *fake.version.lock().unwrap() = "Perfetto v59.1".into();
        assert_eq!(s.major_version(), Some(59));
    }

    #[test]
    fn session_name_is_stable() {
        // A path that does not exist hashes the path alone:
        // hashlib.sha256(b"/traces/stress-1.fxt").hexdigest()[:12]
        assert_eq!(name_for(Path::new("/traces/stress-1.fxt")), "zpo-5c65f4b7ec5a");
    }

    #[test]
    fn rerecorded_trace_gets_a_new_session() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("idle-1.fxt");
        std::fs::write(&t, b"first").unwrap();
        let first = name_for(&t);
        assert_eq!(name_for(&t), first);
        std::fs::write(&t, b"second capture").unwrap();
        assert_ne!(name_for(&t), first);
    }

    #[test]
    fn losing_the_start_race_reuses_the_live_session() {
        let fake = FakeTp::new("Perfetto v59.1-1e9c61e98");
        *fake.lose_race.lock().unwrap() = true;
        let s = sessions(&fake);
        let t = Path::new("/traces/stress-1.fxt");
        assert_eq!(s.ensure(t, &|_| {}).unwrap(), name_for(t));
    }
}
