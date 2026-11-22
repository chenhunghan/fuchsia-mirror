// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Finds the Fuchsia checkout zpo works against; the only place that decides
//! it.
//!
//! Order, first match wins:
//! 1. `--fuchsia-dir PATH` (exported as `FUCHSIA_DIR`, with
//!    `ZPO_FUCHSIA_DIR_SOURCE` so the origin can be shown),
//! 2. the `FUCHSIA_DIR` environment variable,
//! 3. the current directory or one of its parents (has `.jiri_root`),
//! 4. the checkout that `fx` on `PATH` belongs to,
//! 5. `~/fuchsia`.
//!
//! The checkout supplies `ffx`, `fx`, the trace processor and the board's
//! `.profiles` file.

use serde::Serialize;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Records where `FUCHSIA_DIR` came from when zpo set it.
pub const SOURCE_ENV: &str = "ZPO_FUCHSIA_DIR_SOURCE";
/// The source of a checkout given with `--fuchsia-dir`.
pub const FLAG_SOURCE: &str = "--fuchsia-dir";
/// The source shown for the fallback.
pub const DEFAULT_SOURCE: &str = "default (~/fuchsia)";

/// What [`find`] looks at; tests build their own.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub vars: HashMap<String, String>,
    pub cwd: PathBuf,
}

impl Env {
    /// The process's environment and current directory.
    pub fn current() -> Env {
        Env { vars: std::env::vars().collect(), cwd: std::env::current_dir().unwrap_or_default() }
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.vars.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }
}

/// Whether `path` is the root of a Fuchsia checkout (or worktree / workspace).
pub fn is_checkout(path: &Path) -> bool {
    path.join(".jiri_root").is_dir() || path.join(".fx-root").is_file()
}

/// The checkout an `fx` executable belongs to. `.jiri_root/bin/fx` is a
/// symlink to `scripts/fx`, so both forms are accepted.
fn fx_root(fx: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(fx).ok();
    for p in std::iter::once(fx.to_path_buf()).chain(real) {
        let Some(parent) = p.parent() else { continue };
        let root = if parent.ends_with(".jiri_root/bin") {
            parent.parent().and_then(Path::parent)
        } else if parent.ends_with("scripts") {
            parent.parent()
        } else {
            None
        };
        if let Some(r) = root.filter(|r| is_checkout(r)) {
            return Some(r.to_path_buf());
        }
    }
    None
}

/// The first executable `name` on `path` (a PATH-style list).
pub fn which(name: &str, path: Option<&str>) -> Option<PathBuf> {
    std::env::split_paths(path?).map(|d| d.join(name)).find(|p| is_executable(p))
}

/// Whether `p` is a regular file with an execute bit set: what the shell
/// requires of a PATH command, so a stray data file is never run.
pub fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn absolute(p: &Path, cwd: &Path) -> PathBuf {
    let p = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    // Like os.path.abspath: normalize without resolving symlinks.
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn expand_home(p: &str, home: Option<&str>) -> PathBuf {
    match (p.strip_prefix("~"), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{h}{rest}"))
        }
        _ => PathBuf::from(p),
    }
}

/// (checkout path, how it was found). `which_fx` finds `fx` on the PATH.
pub fn find_with(
    env: &Env,
    which_fx: &dyn Fn(Option<&str>) -> Option<PathBuf>,
) -> (PathBuf, String) {
    if let Some(d) = env.get("FUCHSIA_DIR") {
        let path = absolute(&expand_home(d, env.get("HOME")), &env.cwd);
        return (path, env.get(SOURCE_ENV).unwrap_or("FUCHSIA_DIR").to_string());
    }
    let mut d = absolute(&env.cwd, &env.cwd);
    loop {
        if is_checkout(&d) {
            return (d, "current directory".into());
        }
        if !d.pop() {
            break;
        }
    }
    if let Some(root) = which_fx(env.get("PATH")).and_then(|fx| fx_root(&fx)) {
        return (root, "fx on PATH".into());
    }
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    (home.join("fuchsia"), DEFAULT_SOURCE.into())
}

/// [`find_with`] using the real PATH lookup.
pub fn find(env: &Env) -> (PathBuf, String) {
    find_with(env, &|path| which("fx", path))
}

/// The checkout, failing when the result is not a Fuchsia checkout.
pub fn require(env: &Env) -> anyhow::Result<PathBuf> {
    let (root, source) = find(env);
    require_at(root, &source)
}

/// `root`, found from `source`, failing when it is not a Fuchsia checkout.
pub fn require_at(root: PathBuf, source: &str) -> anyhow::Result<PathBuf> {
    if !is_checkout(&root) {
        anyhow::bail!(
            "No Fuchsia checkout at {} (from {source}).\nPass --fuchsia-dir /path/to/fuchsia or set FUCHSIA_DIR.",
            root.display()
        );
    }
    Ok(root)
}

/// The checkout as the UI shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Description {
    pub path: String,
    /// The path with `~` for the home directory.
    pub display: String,
    pub source: String,
    pub exists: bool,
}

/// Describes the checkout [`find`] picks.
pub fn describe(env: &Env) -> Description {
    let (path, source) = find(env);
    let p = path.display().to_string();
    let display = match env.get("HOME") {
        Some(h) if p == h => "~".to_string(),
        Some(h) if p.starts_with(&format!("{h}/")) => format!("~{}", &p[h.len()..]),
        _ => p.clone(),
    };
    Description { exists: is_checkout(&path), path: p, display, source }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long [`write_test_script`] retries, and its first and longest
    /// pause: a forked child execs within milliseconds.
    const SCRIPT_READY: Duration = Duration::from_secs(2);
    const FIRST_PAUSE: Duration = Duration::from_millis(5);
    const MAX_PAUSE: Duration = Duration::from_millis(200);

    /// Writes `script` (starting with a `#!` line) to `path` as an executable
    /// for a test, and returns once it can be run. A child that another test
    /// thread forked while the file was open for writing keeps it open until it
    /// execs, and running the file meanwhile fails with ETXTBSY; so the script
    /// is run once here (a line after `#!` makes that run exit at once), retried
    /// with backoff for up to about 2 s. Later runs then find no writer.
    pub(crate) fn write_test_script(path: &Path, script: &str) {
        let (shebang, rest) = script.split_once('\n').unwrap_or((script, ""));
        let text = format!("{shebang}\n[ -n \"$ZPO_SCRIPT_PROBE\" ] && exit 0\n{rest}");
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (t0, mut pause) = (Instant::now(), FIRST_PAUSE);
        loop {
            let run = Command::new(path)
                .env("ZPO_SCRIPT_PROBE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match run {
                Ok(_) => return,
                Err(e)
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && t0.elapsed() < SCRIPT_READY =>
                {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(MAX_PAUSE);
                }
                Err(e) => panic!("could not run {}: {e}", path.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        elsewhere: PathBuf,
        tmp: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src/fuchsia");
        std::fs::create_dir_all(root.join(".jiri_root/bin")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/fx"), "").unwrap();
        std::os::unix::fs::symlink("../../scripts/fx", root.join(".jiri_root/bin/fx")).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        Fixture { tmp: tmp.path().to_path_buf(), _tmp: tmp, root, elsewhere }
    }

    fn env(cwd: &Path, vars: &[(&str, &str)]) -> Env {
        Env {
            cwd: cwd.to_path_buf(),
            vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn no_fx(_: Option<&str>) -> Option<PathBuf> {
        None
    }

    #[test]
    fn env_wins_and_keeps_its_source() {
        let f = fixture();
        let e = f.elsewhere.display().to_string();
        assert_eq!(
            find_with(&env(&f.root, &[("FUCHSIA_DIR", &e), (SOURCE_ENV, FLAG_SOURCE)]), &no_fx),
            (f.elsewhere.clone(), FLAG_SOURCE.to_string())
        );
        assert_eq!(
            find_with(&env(&f.root, &[("FUCHSIA_DIR", &e)]), &no_fx),
            (f.elsewhere.clone(), "FUCHSIA_DIR".to_string())
        );
    }

    #[test]
    fn current_directory_or_parent() {
        let f = fixture();
        let sub = f.root.join("src/devices");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            find_with(&env(&sub, &[]), &no_fx),
            (f.root.clone(), "current directory".to_string())
        );
    }

    #[test]
    fn fx_on_path_through_the_jiri_symlink() {
        let f = fixture();
        let fx = f.root.join(".jiri_root/bin/fx");
        let found = find_with(&env(&f.elsewhere, &[]), &|_| Some(fx.clone()));
        assert_eq!(found, (f.root.clone(), "fx on PATH".to_string()));
    }

    #[test]
    fn fx_outside_a_checkout_is_ignored() {
        let f = fixture();
        let stray = f.elsewhere.join("fx");
        std::fs::write(&stray, "").unwrap();
        let home = f.tmp.display().to_string();
        let found = find_with(&env(&f.elsewhere, &[("HOME", &home)]), &|_| Some(stray.clone()));
        assert_eq!(found, (f.tmp.join("fuchsia"), DEFAULT_SOURCE.to_string()));
    }

    #[test]
    fn require_refuses_a_directory_that_is_not_a_checkout() {
        let f = fixture();
        let e = f.elsewhere.display().to_string();
        let err = require(&env(&f.root, &[("FUCHSIA_DIR", &e)])).unwrap_err();
        assert!(err.to_string().contains("No Fuchsia checkout at"), "{err}");
        assert!(
            require(&env(&f.elsewhere, &[("FUCHSIA_DIR", &f.root.display().to_string())])).is_ok()
        );
    }

    #[test]
    fn describe_uses_tilde() {
        let f = fixture();
        let home = f.tmp.display().to_string();
        let d = describe(&env(&f.root, &[("HOME", &home)]));
        assert_eq!(d.display, "~/src/fuchsia");
        assert!(d.exists);
    }

    #[test]
    fn which_skips_files_without_an_execute_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("tool"), "").unwrap();
        std::fs::write(b.join("tool"), "").unwrap();
        std::fs::set_permissions(b.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(which("tool", path.to_str()), Some(b.join("tool")));
    }

    #[test]
    fn detects_worktree_via_fx_root_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(wt.join("zircon")).unwrap();
        std::fs::write(wt.join(".fx-root"), "").unwrap();
        assert_eq!(
            find_with(&env(&wt.join("zircon"), &[]), &no_fx),
            (wt, "current directory".into())
        );
    }
}
