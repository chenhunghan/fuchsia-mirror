// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `zpo ui start|stop|restart`: the UI server in the background, its access
//! token, the state file that records a running server, and what the commands
//! print.
//!
//! The token lives in zpo's config dir and the state file and log in its state
//! dir (XDG layout), both readable only by the user. The server writes the
//! state file itself once it listens, so the file always names a process that
//! got that far.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// zpo's config dir: `$XDG_CONFIG_HOME/zpo`, else `~/.config/zpo`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    xdg_dir(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"), ".config")
}

/// zpo's state dir: `$XDG_STATE_HOME/zpo`, else `~/.local/state/zpo`.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    xdg_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"), ".local/state")
}

/// `<xdg>/zpo`, else `<home>/<under_home>/zpo`. With neither there is no
/// private place for the token, so it fails rather than use a shared one such
/// as /tmp.
fn xdg_dir(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    under_home: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(v) = xdg.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v).join("zpo"));
    }
    match home.filter(|h| !h.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(under_home).join("zpo")),
        None => bail!("neither $HOME nor the XDG directory variables are set"),
    }
}

/// Creates `dir` (and its parents) and makes it readable only by the user
/// (0700): it holds the token, or the log that names the server.
fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    // An existing dir keeps its mode through `create`.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// How long the running-server check waits to connect and for the answer: a
/// local server answers at once.
const PING_CONNECT: Duration = Duration::from_secs(1);
const PING_READ: Duration = Duration::from_secs(2);
/// How often `zpo ui` checks a server it is starting or stopping.
const POLL: Duration = Duration::from_millis(100);

/// The token file in the config dir.
pub const TOKEN_FILE: &str = "ui_token";
/// The running server's record in the state dir.
pub const STATE_FILE: &str = "ui_server.json";
/// The background server's output, in the state dir.
pub const LOG_FILE: &str = "ui_server.log";

/// 128 random bits as hex, from the OS.
pub fn random_token() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The UI's access token, kept in `dir`: created once and reused, so a link
/// keeps working across restarts until `rotate` replaces it.
pub fn ui_token(dir: &Path, rotate: bool) -> anyhow::Result<String> {
    let path = dir.join(TOKEN_FILE);
    if !rotate && let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim();
        if t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(t.to_string());
        }
    }
    let t = random_token()?;
    write_private(&path, t.as_bytes())?;
    Ok(t)
}

/// Writes `path` readable only by the user (0600), through a temporary file
/// and a rename so a reader never sees half of it.
fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().context("no parent directory")?;
    private_dir(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    f.write_all(contents)?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// This machine's fully qualified name for links (`hostname -f`), else its
/// short name, else "localhost".
pub fn fqdn() -> String {
    let from_cmd = Command::new("hostname")
        .arg("-f")
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    from_cmd
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|h| h.trim().to_string())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// The link that opens the UI. The token is in the fragment, which browsers
/// never send over the network; the page keeps it and sends it as a header.
pub fn link(host: &str, port: u16, token: &str) -> String {
    format!("http://{host}:{port}/#t={token}")
}

/// What a running server records about itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerState {
    pub pid: u32,
    pub port: u16,
    /// The address it listens on (`--host`).
    pub bind: String,
    /// The name links use.
    pub host: String,
    pub sessions: String,
    pub started: String,
    /// A random id the server picks when it starts and sends in every
    /// response ([`INSTANCE_HEADER`]), so a recycled pid or another zpo on the
    /// port is never taken for this server.
    #[serde(default)]
    pub instance: String,
    /// The checkout it works against. None in a record from before this
    /// was recorded.
    #[serde(default)]
    pub fuchsia_dir: Option<String>,
}

/// The address a server listens on unless told otherwise: every interface.
pub const DEFAULT_BIND: &str = "0.0.0.0";
/// The port a server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8765;
/// How a restarted server's UI says where its checkout came from when it kept
/// the replaced server's.
pub const KEPT_SOURCE: &str = "kept from the replaced zpo UI server";

/// The settings `zpo ui start` and `restart` start a server with, each None
/// when not given on the command line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServeFlags {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub sessions: Option<PathBuf>,
    pub fuchsia_dir: Option<PathBuf>,
}

/// Which of the replaced server's settings a new server keeps: those the
/// command line leaves out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kept {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub sessions: Option<PathBuf>,
    pub fuchsia_dir: Option<PathBuf>,
    /// The replaced server's checkout when it is no longer a checkout, so it
    /// was not kept.
    pub gone_checkout: Option<PathBuf>,
}

/// What a new server gets when neither the command line nor a replaced
/// server sets it. The restart line leaves these values out.
#[derive(Clone, Debug, PartialEq)]
pub struct Defaults {
    pub sessions: PathBuf,
    /// The checkout this command found, as `zpo ui start` would.
    pub checkout: PathBuf,
}

impl ServeFlags {
    /// These flags, with what they leave out taken from the server `prev`
    /// that was just stopped, so a restart keeps its settings; and what was
    /// taken. A recorded checkout is taken only while `is_checkout` still
    /// says it is one.
    pub fn or_recorded(
        &self,
        prev: Option<&ServerState>,
        is_checkout: &dyn Fn(&Path) -> bool,
    ) -> (ServeFlags, Kept) {
        let Some(p) = prev else { return (self.clone(), Kept::default()) };
        let mut kept = Kept {
            bind: self.bind.is_none().then(|| p.bind.clone()),
            port: self.port.is_none().then_some(p.port),
            sessions: (self.sessions.is_none() && !p.sessions.is_empty())
                .then(|| PathBuf::from(&p.sessions)),
            ..Kept::default()
        };
        if let (None, Some(d)) = (&self.fuchsia_dir, &p.fuchsia_dir) {
            let d = PathBuf::from(d);
            if is_checkout(&d) {
                kept.fuchsia_dir = Some(d);
            } else {
                kept.gone_checkout = Some(d);
            }
        }
        let flags = ServeFlags {
            bind: self.bind.clone().or_else(|| kept.bind.clone()),
            port: self.port.or(kept.port),
            sessions: self.sessions.clone().or_else(|| kept.sessions.clone()),
            fuchsia_dir: self.fuchsia_dir.clone().or_else(|| kept.fuchsia_dir.clone()),
        };
        (flags, kept)
    }

    pub fn bind(&self) -> &str {
        self.bind.as_deref().unwrap_or(DEFAULT_BIND)
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }
}

/// What a restart says about the settings it kept: the ones that differ
/// from `defaults`, and a replaced server's checkout that is gone. None when
/// there is nothing to say.
pub fn kept_message(kept: &Kept, defaults: &Defaults) -> Option<String> {
    let mut same = Vec::new();
    if let Some(s) = kept.sessions.as_ref().filter(|s| **s != defaults.sessions) {
        same.push(format!("sessions: {}", s.display()));
    }
    if let Some(p) = kept.port.filter(|p| *p != DEFAULT_PORT) {
        same.push(format!("port {p}"));
    }
    if let Some(b) = kept.bind.as_ref().filter(|b| *b != DEFAULT_BIND) {
        same.push(format!("host {b}"));
    }
    if let Some(d) = kept.fuchsia_dir.as_ref().filter(|d| **d != defaults.checkout) {
        same.push(format!("checkout: {}", d.display()));
    }
    let mut lines = Vec::new();
    if !same.is_empty() {
        lines.push(format!("Restarted with the same settings ({}).", same.join(", ")));
    }
    if let Some(d) = &kept.gone_checkout {
        lines.push(format!(
            "The replaced server's checkout {} is no longer a Fuchsia checkout; using {} \
             instead.",
            d.display(),
            defaults.checkout.display()
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The response header that carries a server's [`ServerState::instance`].
pub const INSTANCE_HEADER: &str = "X-Zpo-Instance";

/// Whether `pid` can be a server's: `kill` reads 0 as this process group and
/// -1 as every process, and pid 1 is init. Pids fit in an i32.
fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

impl ServerState {
    pub fn read(dir: &Path) -> Option<ServerState> {
        let s: ServerState =
            serde_json::from_str(&std::fs::read_to_string(dir.join(STATE_FILE)).ok()?).ok()?;
        valid_pid(s.pid).then_some(s)
    }

    pub fn write(&self, dir: &Path) -> anyhow::Result<()> {
        write_private(&dir.join(STATE_FILE), serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn remove(dir: &Path) {
        let _ = std::fs::remove_file(dir.join(STATE_FILE));
    }
}

/// What zpo asks the OS about a server; tests substitute a fake.
pub trait Probe {
    /// Whether process `pid` exists.
    fn alive(&self, pid: u32) -> bool;
    /// The instance of the zpo UI server that answers on `bind`:`port`
    /// ("" for a zpo without one), None when no zpo answers.
    fn instance_at(&self, bind: &str, port: u16) -> Option<String>;
    /// Sends `signal` ("TERM" or "KILL") to `pid`.
    fn signal(&self, signal: &str, pid: u32);
}

/// The real OS. Signals go through the `kill` command, so zpo needs no
/// unsafe code.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn alive(&self, pid: u32) -> bool {
        valid_pid(pid)
            && Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    }

    fn instance_at(&self, bind: &str, port: u16) -> Option<String> {
        ping(bind, port)
    }

    fn signal(&self, signal: &str, pid: u32) {
        if !valid_pid(pid) {
            return;
        }
        let _ = Command::new("kill")
            .args(["-s", signal, &pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }
}

/// The address to reach a server bound to `bind` from this machine.
fn probe_addr(bind: &str) -> &str {
    match bind {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" => "::1",
        b => b,
    }
}

/// The instance header of the zpo UI server on `bind`:`port` ("" without
/// one), None when no zpo answers. Every zpo response has an `X-Zpo: 1`
/// header, even a refusal, so no token is sent.
fn ping(bind: &str, port: u16) -> Option<String> {
    let sa = (probe_addr(bind), port).to_socket_addrs().ok()?.next()?;
    let mut c = TcpStream::connect_timeout(&sa, PING_CONNECT).ok()?;
    let _ = c.set_read_timeout(Some(PING_READ));
    c.write_all(b"GET /api/ping HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut buf = String::new();
    let _ = c.take(4096).read_to_string(&mut buf);
    let head = buf.split("\r\n\r\n").next().unwrap_or("");
    if !buf.starts_with("HTTP/1.1 ") || !head.split("\r\n").any(|l| l == "X-Zpo: 1") {
        return None;
    }
    let prefix = format!("{INSTANCE_HEADER}: ");
    Some(head.split("\r\n").find_map(|l| l.strip_prefix(&prefix)).unwrap_or("").to_string())
}

/// Whether the server `s` records is the one answering on its port.
fn answers_as(probe: &dyn Probe, s: &ServerState) -> bool {
    !s.instance.is_empty() && probe.instance_at(&s.bind, s.port).as_deref() == Some(&s.instance)
}

/// Whether something already listens on `bind`:`port`.
pub fn port_in_use(bind: &str, port: u16) -> bool {
    matches!(
        std::net::TcpListener::bind((bind, port)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// The server the state file in `dir` records, if it still runs and answers.
/// A record whose process is gone is removed.
pub fn running_server(dir: &Path, probe: &dyn Probe) -> Option<ServerState> {
    let s = ServerState::read(dir)?;
    if !probe.alive(s.pid) {
        ServerState::remove(dir);
        return None;
    }
    answers_as(probe, &s).then_some(s)
}

/// Whether to replace a running server: `--yes` says yes; on a terminal the
/// user is asked (`ask`, default no); without one zpo leaves it running.
pub fn replace_existing(yes: bool, tty: bool, ask: impl FnOnce() -> bool) -> bool {
    yes || (tty && ask())
}

/// How [`stop_server`] ended.
#[derive(Debug, PartialEq)]
pub enum Stopped {
    /// No server was recorded (or its process was already gone).
    NotRunning,
    /// The recorded process exists but the server on its port is not the
    /// recorded one (the pid may belong to another program now), so it was
    /// not signalled.
    NotZpo(ServerState),
    /// The recorded process exists and a zpo answers on its port, but with
    /// another instance id, or none: a server from a different zpo build. It
    /// was not signalled, and the record (in this file) is kept.
    OtherBuild(ServerState, PathBuf),
    /// Stopped by SIGTERM.
    Terminated(ServerState),
    /// Still running `wait` after SIGTERM, so killed.
    Killed(ServerState),
}

impl Stopped {
    /// The server that was stopped, whose settings a restart reuses; None
    /// when none was (no server, a stale record, or another build's).
    pub fn server(&self) -> Option<&ServerState> {
        match self {
            Stopped::Terminated(s) | Stopped::Killed(s) => Some(s),
            Stopped::NotRunning | Stopped::NotZpo(_) | Stopped::OtherBuild(..) => None,
        }
    }
}

/// Stops the server recorded in `dir`: SIGTERM, up to `wait` for it to exit,
/// then SIGKILL. The record is removed, except for a server from a different
/// zpo build, which is left alone.
pub fn stop_server(dir: &Path, probe: &dyn Probe, wait: Duration) -> Stopped {
    let Some(s) = ServerState::read(dir) else { return Stopped::NotRunning };
    if probe.alive(s.pid) && probe.instance_at(&s.bind, s.port).is_some() && !answers_as(probe, &s)
    {
        return Stopped::OtherBuild(s, dir.join(STATE_FILE));
    }
    ServerState::remove(dir);
    if !probe.alive(s.pid) {
        return Stopped::NotRunning;
    }
    if !answers_as(probe, &s) {
        return Stopped::NotZpo(s);
    }
    probe.signal("TERM", s.pid);
    let t0 = Instant::now();
    while probe.alive(s.pid) {
        if t0.elapsed() >= wait {
            probe.signal("KILL", s.pid);
            return Stopped::Killed(s);
        }
        std::thread::sleep(POLL);
    }
    Stopped::Terminated(s)
}

/// Why `bind`:`port` cannot be used, for `zpo ui start` and the server: a
/// zpo from a different build that `dir` records, a zpo with no record, or
/// another program. Each says what happened, a blank line, then what to do.
pub fn port_taken_message(dir: &Path, bind: &str, port: u16, probe: &dyn Probe) -> String {
    let pick = "pick another port: `zpo ui start --port <other>`";
    if probe.instance_at(bind, port).is_none() {
        return format!(
            "port {port} is in use by another program.\n\nPick a different one: `zpo ui start --port <other>`"
        );
    }
    match ServerState::read(dir).filter(|s| s.port == port && probe.alive(s.pid)) {
        Some(s) => format!(
            "{}\n\nStop it with `kill {}`, or {pick}",
            other_build(&s, &dir.join(STATE_FILE)),
            s.pid
        ),
        None => format!(
            "another zpo UI server, not recorded in {}, already listens on port {port}.\n\n\
             Stop it in the terminal that started it, or {pick}",
            dir.display()
        ),
    }
}

/// The first line about a server from a different zpo build.
fn other_build(s: &ServerState, file: &Path) -> String {
    format!(
        "a zpo UI server from a different zpo build is listening on port {} (pid {} in {}).",
        s.port,
        s.pid,
        file.display()
    )
}

/// Starts `exe args` in its own session (`setsid`), detached from this
/// terminal, with `envs` added to its environment: no input, output appended
/// to `log`. It keeps running after the caller exits and after the user logs
/// out.
pub fn spawn_detached(
    exe: &Path,
    args: &[String],
    envs: &[(&str, String)],
    log: &Path,
) -> anyhow::Result<Child> {
    if let Some(d) = log.parent() {
        private_dir(d)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .with_context(|| format!("could not open {}", log.display()))?;
    // `mode` only applies to a new file.
    out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let err = out.try_clone()?;
    Command::new("setsid")
        .arg(exe)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .context("could not run setsid to start the zpo UI server")
}

/// Waits up to `timeout` for the server started as `child` to record itself
/// in `dir` and answer.
pub fn wait_until_up(
    dir: &Path,
    child: &mut Child,
    probe: &dyn Probe,
    timeout: Duration,
) -> anyhow::Result<ServerState> {
    let t0 = Instant::now();
    loop {
        if let Some(s) = ServerState::read(dir)
            && probe.alive(s.pid)
            && answers_as(probe, &s)
        {
            return Ok(s);
        }
        // `setsid` may fork and exit at once; only a failure means the server
        // did not start.
        if let Some(status) = child.try_wait()?
            && !status.success()
        {
            bail!("the zpo UI server exited ({status}) before it answered");
        }
        if t0.elapsed() >= timeout {
            bail!("the zpo UI server did not answer within {} s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// What `zpo ui start` and `restart` print once the server answers.
pub fn started_message(s: &ServerState, token: &str, log: &Path) -> String {
    format!(
        "The zpo UI server is running in the background (pid {}, port {}).\n\n\
         It keeps running after this command returns and after you log out. Only \
         `zpo ui stop` or the red X in the UI stops it.\n\n\
         Open this link in your browser to use the UI:\n\n    {}\n\nLog: {}",
        s.pid,
        s.port,
        link(&s.host, s.port, token),
        log.display()
    )
}

/// The first line about a server that already runs.
pub fn already_running(s: &ServerState) -> String {
    format!(
        "A zpo UI server is already running on this machine (pid {}, port {}, started {}).",
        s.pid, s.port, s.started
    )
}

/// Asked on a terminal after [`already_running`], on its own line.
pub const REPLACE_PROMPT: &str = "Stop it and start a new one? [y/N] ";

/// What `zpo ui stop` prints.
pub fn stopped_message(stopped: &Stopped) -> String {
    match stopped {
        Stopped::NotRunning => "No zpo UI server is running.".into(),
        Stopped::Terminated(s) => {
            format!("Stopped the zpo UI server (pid {}, port {}).", s.pid, s.port)
        }
        Stopped::Killed(s) => format!(
            "The zpo UI server (pid {}, port {}) did not stop within 5 s of SIGTERM, so it was \
             killed.",
            s.pid, s.port
        ),
        Stopped::NotZpo(s) => format!(
            "The recorded zpo UI server (pid {}, port {}) no longer answers, and the pid may \
             belong to another program now, so nothing was stopped.\n\nThe stale record was \
             removed.",
            s.pid, s.port
        ),
        Stopped::OtherBuild(s, file) => {
            let first = other_build(s, file);
            format!("A{}\n\nNothing was stopped. Stop it with `kill {}`.", &first[1..], s.pid)
        }
    }
}

/// What `zpo ui restart` prints before the new server's message.
pub fn restart_preamble(stopped: &Stopped) -> String {
    match stopped {
        Stopped::NotRunning => "No zpo UI server was running.".into(),
        other => stopped_message(other),
    }
}

/// The last `n` lines of `path`, for errors.
pub fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    #[test]
    fn the_token_is_created_once_private_and_rotated_on_request() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("cfg/zpo");
        let t = ui_token(&dir, false).unwrap();
        assert_eq!(t.len(), 32);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        let mode = std::fs::metadata(dir.join(TOKEN_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Reused across restarts.
        assert_eq!(ui_token(&dir, false).unwrap(), t);
        // Rotated on request, and the new one is reused.
        let t2 = ui_token(&dir, true).unwrap();
        assert_ne!(t2, t);
        assert_eq!(ui_token(&dir, false).unwrap(), t2);
        // A damaged file is replaced.
        std::fs::write(dir.join(TOKEN_FILE), "short").unwrap();
        assert_ne!(ui_token(&dir, false).unwrap(), "short");
    }

    fn state(pid: u32) -> ServerState {
        ServerState {
            pid,
            port: 8765,
            bind: "0.0.0.0".into(),
            host: "ws.example.com".into(),
            sessions: "/tmp/s".into(),
            started: "2026-11-06 23:00:00".into(),
            instance: "i1".into(),
            fuchsia_dir: None,
        }
    }

    #[test]
    fn the_started_message_is_laid_out_in_blocks() {
        let m = started_message(&state(42), "ab12", Path::new("/s/ui_server.log"));
        assert_eq!(
            m,
            "The zpo UI server is running in the background (pid 42, port 8765).\n\n\
             It keeps running after this command returns and after you log out. Only \
             `zpo ui stop` or the red X in the UI stops it.\n\n\
             Open this link in your browser to use the UI:\n\n    \
             http://ws.example.com:8765/#t=ab12\n\nLog: /s/ui_server.log"
        );
    }

    #[test]
    fn stop_and_restart_say_what_happened() {
        assert_eq!(stopped_message(&Stopped::NotRunning), "No zpo UI server is running.");
        assert_eq!(restart_preamble(&Stopped::NotRunning), "No zpo UI server was running.");
        let t = Stopped::Terminated(state(7));
        assert_eq!(stopped_message(&t), "Stopped the zpo UI server (pid 7, port 8765).");
        assert_eq!(restart_preamble(&t), stopped_message(&t));
        assert!(stopped_message(&Stopped::Killed(state(7))).contains("so it was killed"));
        assert_eq!(
            already_running(&state(7)),
            "A zpo UI server is already running on this machine (pid 7, port 8765, started \
             2026-11-06 23:00:00)."
        );
    }

    fn defaults() -> Defaults {
        Defaults {
            sessions: "/home/u/.local/share/zpo/sessions".into(),
            checkout: "/src/here".into(),
        }
    }

    #[test]
    fn a_restart_keeps_the_stopped_servers_settings() {
        let d = tempfile::tempdir().unwrap();
        let mut s = record(d.path(), 7, 9000);
        s.bind = "127.0.0.1".into();
        s.sessions = "/tmp/zpo-ui-test".into();
        s.fuchsia_dir = Some("/src/fuchsia".into());
        s.write(d.path()).unwrap();
        let stopped = stop_server(d.path(), &Fake::new(&[7], &[9000], "TERM"), Duration::ZERO);
        let prev = stopped.server();
        assert_eq!(prev, Some(&s));
        let any = |_: &Path| true;
        // Nothing given: everything recorded.
        let (f, kept) = ServeFlags::default().or_recorded(prev, &any);
        assert_eq!((f.bind(), f.port()), ("127.0.0.1", 9000));
        assert_eq!(f.sessions, Some(PathBuf::from("/tmp/zpo-ui-test")));
        assert_eq!(f.fuchsia_dir, Some(PathBuf::from("/src/fuchsia")));
        assert_eq!(kept.fuchsia_dir, Some(PathBuf::from("/src/fuchsia")));
        assert_eq!(
            kept_message(&kept, &defaults()).unwrap(),
            "Restarted with the same settings (sessions: /tmp/zpo-ui-test, port 9000, host \
             127.0.0.1, checkout: /src/fuchsia)."
        );
        // Flags win, and the line names only what was kept.
        let given = ServeFlags {
            port: Some(8800),
            sessions: Some("/tmp/other".into()),
            fuchsia_dir: Some("/src/given".into()),
            ..Default::default()
        };
        let (f, kept) = given.or_recorded(prev, &any);
        assert_eq!((f.bind(), f.port()), ("127.0.0.1", 8800));
        assert_eq!(f.sessions, Some(PathBuf::from("/tmp/other")));
        assert_eq!(f.fuchsia_dir, Some(PathBuf::from("/src/given")));
        assert_eq!((kept.port, &kept.sessions, &kept.fuchsia_dir), (None, &None, &None));
        assert_eq!(
            kept_message(&kept, &defaults()).unwrap(),
            "Restarted with the same settings (host 127.0.0.1)."
        );
    }

    #[test]
    fn the_restart_line_leaves_out_defaults() {
        let d = tempfile::tempdir().unwrap();
        let mut s = record(d.path(), 7, DEFAULT_PORT);
        s.sessions = "/home/u/.local/share/zpo/sessions".into();
        s.fuchsia_dir = Some("/src/here".into());
        s.write(d.path()).unwrap();
        let stopped =
            stop_server(d.path(), &Fake::new(&[7], &[DEFAULT_PORT], "TERM"), Duration::ZERO);
        let (f, kept) = ServeFlags::default().or_recorded(stopped.server(), &|_| true);
        // Everything is kept, but it is what a plain start would pick.
        assert_eq!(f.fuchsia_dir, Some(PathBuf::from("/src/here")));
        assert_eq!(kept_message(&kept, &defaults()), None);
        // A kept checkout other than the one found here is named.
        let elsewhere = Defaults { checkout: "/src/other".into(), ..defaults() };
        assert_eq!(
            kept_message(&kept, &elsewhere).unwrap(),
            "Restarted with the same settings (checkout: /src/here)."
        );
    }

    #[test]
    fn a_checkout_that_is_gone_is_not_kept() {
        let d = tempfile::tempdir().unwrap();
        let mut s = record(d.path(), 7, DEFAULT_PORT);
        s.fuchsia_dir = Some("/src/removed".into());
        s.write(d.path()).unwrap();
        let stopped =
            stop_server(d.path(), &Fake::new(&[7], &[DEFAULT_PORT], "TERM"), Duration::ZERO);
        let (f, kept) = ServeFlags::default().or_recorded(stopped.server(), &|_| false);
        assert_eq!((&f.fuchsia_dir, &kept.fuchsia_dir), (&None, &None));
        assert_eq!(kept.gone_checkout, Some(PathBuf::from("/src/removed")));
        assert_eq!(
            kept_message(&kept, &defaults()).unwrap(),
            "Restarted with the same settings (sessions: /tmp/s).\nThe replaced server's \
             checkout /src/removed is no longer a Fuchsia checkout; using /src/here instead."
        );
    }

    #[test]
    fn a_restart_without_a_server_uses_the_flags_or_defaults() {
        let d = tempfile::tempdir().unwrap();
        let flags = ServeFlags { sessions: Some("/tmp/s2".into()), ..Default::default() };
        let any = |_: &Path| true;
        // No record.
        let stopped = stop_server(d.path(), &Fake::new(&[], &[], "TERM"), Duration::ZERO);
        assert_eq!(restart_preamble(&stopped), "No zpo UI server was running.");
        assert_eq!(stopped.server(), None);
        let (f, kept) = flags.or_recorded(stopped.server(), &any);
        assert_eq!((f.bind(), f.port()), (DEFAULT_BIND, DEFAULT_PORT));
        assert_eq!(f.sessions, Some(PathBuf::from("/tmp/s2")));
        assert_eq!(kept, Kept::default());
        assert_eq!(kept_message(&kept, &defaults()), None);
        // A stale record (its pid is not a zpo now) is not reused either.
        record(d.path(), 7, 9000);
        let stopped = stop_server(d.path(), &Fake::new(&[7], &[], "TERM"), Duration::ZERO);
        assert!(matches!(stopped, Stopped::NotZpo(_)));
        assert_eq!(flags.or_recorded(stopped.server(), &any).0.port(), DEFAULT_PORT);
        // A record from before `fuchsia_dir` was recorded still reads.
        let old = r#"{"pid": 7, "port": 8765, "bind": "0.0.0.0", "host": "h", "sessions": "/s",
                      "started": "t", "instance": "i1"}"#;
        std::fs::write(d.path().join(STATE_FILE), old).unwrap();
        assert_eq!(ServerState::read(d.path()).unwrap().fuchsia_dir, None);
    }

    #[test]
    fn links_carry_the_token_in_the_fragment() {
        assert_eq!(link("ws.example.com", 8765, "ab12"), "http://ws.example.com:8765/#t=ab12");
    }

    /// A fake OS: which pids exist, which ports answer as zpo, and the
    /// signals sent. `dies_on` is the signal that ends a process.
    struct Fake {
        alive: RefCell<HashSet<u32>>,
        /// Ports a zpo answers on, with the instance "i1".
        zpo_ports: HashSet<u16>,
        dies_on: &'static str,
        signals: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(alive: &[u32], zpo_ports: &[u16], dies_on: &'static str) -> Fake {
            Fake {
                alive: RefCell::new(alive.iter().copied().collect()),
                zpo_ports: zpo_ports.iter().copied().collect(),
                dies_on,
                signals: RefCell::new(vec![]),
            }
        }
    }

    impl Probe for Fake {
        fn alive(&self, pid: u32) -> bool {
            self.alive.borrow().contains(&pid)
        }
        fn instance_at(&self, _bind: &str, port: u16) -> Option<String> {
            self.zpo_ports.contains(&port).then(|| "i1".to_string())
        }
        fn signal(&self, signal: &str, pid: u32) {
            self.signals.borrow_mut().push(format!("{signal} {pid}"));
            if signal == self.dies_on || signal == "KILL" {
                self.alive.borrow_mut().remove(&pid);
            }
        }
    }

    fn record(dir: &Path, pid: u32, port: u16) -> ServerState {
        let s = ServerState {
            pid,
            port,
            bind: "0.0.0.0".into(),
            host: "ws.example.com".into(),
            sessions: "/tmp/s".into(),
            started: "2026-11-06T23:00:00".into(),
            instance: "i1".into(),
            fuchsia_dir: None,
        };
        s.write(dir).unwrap();
        s
    }

    #[test]
    fn finds_a_running_server_and_drops_stale_records() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(running_server(d.path(), &Fake::new(&[], &[], "TERM")), None);
        let s = record(d.path(), 42, 8765);
        assert_eq!(running_server(d.path(), &Fake::new(&[42], &[8765], "TERM")), Some(s.clone()));
        // Alive but not answering (still starting, or another program): not
        // running, but the record stays.
        assert_eq!(running_server(d.path(), &Fake::new(&[42], &[], "TERM")), None);
        assert!(d.path().join(STATE_FILE).exists());
        // Its process is gone: the record is removed silently.
        assert_eq!(running_server(d.path(), &Fake::new(&[], &[8765], "TERM")), None);
        assert!(!d.path().join(STATE_FILE).exists());
    }

    #[test]
    fn replaces_only_when_told_to() {
        assert!(replace_existing(true, false, || panic!("not asked with --yes")));
        assert!(replace_existing(false, true, || true));
        assert!(!replace_existing(false, true, || false));
        assert!(!replace_existing(false, false, || panic!("no terminal to ask on")));
    }

    #[test]
    fn stops_with_term_then_kill() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            stop_server(d.path(), &Fake::new(&[], &[], "TERM"), Duration::ZERO),
            Stopped::NotRunning
        );
        let s = record(d.path(), 7, 8765);
        let f = Fake::new(&[7], &[8765], "TERM");
        assert_eq!(
            stop_server(d.path(), &f, Duration::from_secs(5)),
            Stopped::Terminated(s.clone())
        );
        assert_eq!(*f.signals.borrow(), vec!["TERM 7"]);
        assert!(!d.path().join(STATE_FILE).exists());
        // Ignores SIGTERM: killed after the wait.
        record(d.path(), 7, 8765);
        let f = Fake::new(&[7], &[8765], "none");
        assert_eq!(
            stop_server(d.path(), &f, Duration::from_millis(200)),
            Stopped::Killed(s.clone())
        );
        assert_eq!(*f.signals.borrow(), vec!["TERM 7", "KILL 7"]);
        // A recorded pid that no longer answers as zpo is not signalled.
        record(d.path(), 7, 8765);
        let f = Fake::new(&[7], &[], "TERM");
        assert_eq!(stop_server(d.path(), &f, Duration::ZERO), Stopped::NotZpo(s));
        assert!(f.signals.borrow().is_empty());
        // A record whose process is gone.
        record(d.path(), 7, 8765);
        assert_eq!(
            stop_server(d.path(), &Fake::new(&[], &[], "TERM"), Duration::ZERO),
            Stopped::NotRunning
        );
        assert!(!d.path().join(STATE_FILE).exists());
    }

    #[test]
    fn only_the_recorded_instance_with_a_real_pid_counts() {
        let d = tempfile::tempdir().unwrap();
        // Another zpo (another instance) on the recorded port: not ours, and
        // the recycled pid is not signalled.
        let mut s = record(d.path(), 7, 8765);
        s.instance = "other".into();
        s.write(d.path()).unwrap();
        let f = Fake::new(&[7], &[8765], "TERM");
        assert_eq!(running_server(d.path(), &f), None);
        let file = d.path().join(STATE_FILE);
        let other = stop_server(d.path(), &f, Duration::ZERO);
        assert_eq!(other, Stopped::OtherBuild(s, file.clone()));
        assert!(f.signals.borrow().is_empty());
        assert!(file.exists());
        assert_eq!(
            stopped_message(&other),
            format!(
                "A zpo UI server from a different zpo build is listening on port 8765 (pid 7 in \
                 {}).\n\nNothing was stopped. Stop it with `kill 7`.",
                file.display()
            )
        );
        // Pids 0, 1 and those past i32 are never read back.
        for pid in [0, 1, u32::MAX] {
            record(d.path(), pid, 8765);
            assert_eq!(ServerState::read(d.path()), None, "{pid}");
        }
    }

    #[test]
    fn the_dirs_and_the_log_are_private_and_never_shared() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("state/zpo").join(LOG_FILE);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "old\n").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        spawn_detached(Path::new("/bin/true"), &[], &[], &log).unwrap().wait().unwrap();
        assert_eq!((mode(log.parent().unwrap()), mode(&log)), (0o700, 0o600));
        let home = Some(d.path().as_os_str().to_owned());
        assert_eq!(xdg_dir(None, home, ".config").unwrap(), d.path().join(".config/zpo"));
        assert!(xdg_dir(None, None, ".config").is_err());
    }

    #[test]
    fn a_taken_port_says_what_happened_then_what_to_do() {
        let d = tempfile::tempdir().unwrap();
        let msg = |f: &Fake| port_taken_message(d.path(), "0.0.0.0", 8765, f);
        assert_eq!(
            msg(&Fake::new(&[], &[], "TERM")),
            "port 8765 is in use by another program.\n\nPick a different one: \
             `zpo ui start --port <other>`"
        );
        assert_eq!(
            msg(&Fake::new(&[], &[8765], "TERM")),
            format!(
                "another zpo UI server, not recorded in {}, already listens on port 8765.\n\n\
                 Stop it in the terminal that started it, or pick another port: \
                 `zpo ui start --port <other>`",
                d.path().display()
            )
        );
        // A record from a build without instance ids, its pid still serving.
        let mut s = record(d.path(), 7, 8765);
        s.instance = String::new();
        s.write(d.path()).unwrap();
        assert_eq!(
            msg(&Fake::new(&[7], &[8765], "TERM")),
            format!(
                "a zpo UI server from a different zpo build is listening on port 8765 (pid 7 in \
                 {}).\n\nStop it with `kill 7`, or pick another port: `zpo ui start --port <other>`",
                d.path().join(STATE_FILE).display()
            )
        );
    }
}
