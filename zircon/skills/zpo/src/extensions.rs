// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Extension directories: where the optional agent backend and the optional
//! lesson links come from.
//!
//! The board itself (CPU domains, rates and roles) is read from the device,
//! not from an extension. An agent backend is found in, in order:
//! 1. the directories in `$ZPO_EXTENSIONS` (colon-separated),
//! 2. `$FUCHSIA_DIR/vendor/*/tools/zpo/`,
//! 3. zpo's own directory, `$FUCHSIA_DIR/zircon/skills/zpo/`.
//!
//! An extension directory may have a `zpo_extension.json`:
//!
//! ```json
//! {
//!   "agent": { "name": "MyAgent", "command": "zpo-myagent" },
//!   "lessons": {
//!     "url": "https://example.com/scheduler-lessons.html",
//!     "anchors": { "throttled": "#quota", "capacity": "#demand" }
//!   }
//! }
//! ```
//!
//! `command` is a path relative to the extension directory, an executable next
//! to the `zpo` binary, or a name on `PATH`. See [`crate::agent`] for what it
//! must implement.
//!
//! `lessons` points the "why" links of the verdict at a document that explains
//! the scheduling rules zpo applies, with one anchor per [`LESSON_TOPICS`]
//! entry. Without it, findings have no links.

use crate::checkout::is_executable;
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// zpo's directory in the checkout.
pub const SELF_DIR: &str = "zircon/skills/zpo";

/// The optional file in an extension directory that declares its agent backend
/// and lesson links.
pub const MANIFEST: &str = "zpo_extension.json";

#[derive(Clone, Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    agent: Option<AgentEntry>,
    #[serde(default)]
    lessons: Option<Lessons>,
}

/// The topics a verdict links to, with what the linked section should explain.
pub const LESSON_TOPICS: &[(&str, &str)] = &[
    ("own_work", "execution variability"),
    ("period", "periods/demand"),
    ("scheduling", "EDF vs priority"),
    ("capacity", "quantile demand/epochs"),
    ("throttled", "quota vs slack"),
    ("hardware", "blocking"),
    ("affinity", "placement/affinity"),
    ("hops", "event-loop head-of-line blocking"),
];

/// Where the verdict's "why" links point, declared by an extension.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Lessons {
    /// The document as a whole. Empty when no extension declares one.
    pub url: String,
    /// Topic (see [`LESSON_TOPICS`]) -> anchor or suffix appended to `url`.
    #[serde(default)]
    pub anchors: IndexMap<String, String>,
}

impl Lessons {
    /// Topic -> full link, in the extension's order.
    pub fn links(&self) -> IndexMap<String, String> {
        if self.url.is_empty() {
            return IndexMap::new();
        }
        self.anchors.iter().map(|(k, a)| (k.clone(), format!("{}{a}", self.url))).collect()
    }

    /// The link for a topic, or "" when there is none.
    pub fn link(&self, topic: &str) -> String {
        match self.anchors.get(topic) {
            Some(a) if !self.url.is_empty() => format!("{}{a}", self.url),
            _ => String::new(),
        }
    }
}

/// An agent backend declared by an extension.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct AgentEntry {
    /// The name the UI shows, e.g. "MyAgent".
    pub name: String,
    /// The backend executable.
    pub command: String,
}

/// The extension directories, in priority order.
pub fn dirs(fuchsia_dir: &Path, env_value: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(v) = env_value {
        out.extend(std::env::split_paths(v).filter(|p| !p.as_os_str().is_empty()));
    }
    if let Ok(rd) = std::fs::read_dir(fuchsia_dir.join("vendor")) {
        let mut vendors: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        vendors.sort();
        for v in vendors {
            let d = v.join("tools/zpo");
            if d.is_dir() {
                out.push(d);
            }
        }
    }
    out.push(fuchsia_dir.join(SELF_DIR));
    out.dedup();
    out
}

/// [`dirs`] with `$ZPO_EXTENSIONS` from the environment.
pub fn dirs_from_env(fuchsia_dir: &Path) -> Vec<PathBuf> {
    dirs(fuchsia_dir, std::env::var("ZPO_EXTENSIONS").ok().as_deref())
}

/// The first agent backend any extension declares, with its executable
/// resolved.
pub fn agent(dirs: &[PathBuf]) -> Option<(AgentEntry, PathBuf)> {
    for d in dirs {
        let Ok(text) = std::fs::read_to_string(d.join(MANIFEST)) else { continue };
        let Ok(m) = serde_json::from_str::<Manifest>(&text) else { continue };
        if let Some(a) = m.agent {
            if let Some(cmd) = resolve_command(d, &a.command) {
                return Some((a, cmd));
            }
        }
    }
    None
}

/// The lesson links of the first extension that declares them.
pub fn lessons(dirs: &[PathBuf]) -> Lessons {
    for d in dirs {
        let Ok(text) = std::fs::read_to_string(d.join(MANIFEST)) else { continue };
        let Ok(m) = serde_json::from_str::<Manifest>(&text) else { continue };
        if let Some(l) = m.lessons.filter(|l| !l.url.is_empty()) {
            return l;
        }
    }
    Lessons::default()
}

fn resolve_command(dir: &Path, command: &str) -> Option<PathBuf> {
    let p = Path::new(command);
    if p.is_absolute() {
        return is_executable(p).then(|| p.to_path_buf());
    }
    let in_dir = dir.join(p);
    if is_executable(&in_dir) {
        return Some(in_dir);
    }
    if let Some(next) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(p))) {
        if is_executable(&next) {
            return Some(next);
        }
    }
    crate::checkout::which(command, std::env::var("PATH").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_vendor_dirs_and_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let ext = root.join("vendor/acme/tools/zpo");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::create_dir_all(root.join("vendor/other/tools")).unwrap();
        let extra = root.join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let d = dirs(root, Some(&extra.display().to_string()));
        assert_eq!(d, vec![extra.clone(), ext.clone(), root.join(SELF_DIR)]);

        assert!(agent(&d).is_none());
        std::fs::write(ext.join("backend.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            ext.join("zpo_extension.json"),
            r#"{"agent": {"name": "Helper", "command": "backend.sh"}}"#,
        )
        .unwrap();
        // A backend without an execute bit could not run, so it is not found.
        assert!(agent(&d).is_none());
        std::fs::set_permissions(ext.join("backend.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let (a, cmd) = agent(&d).unwrap();
        assert_eq!(a.name, "Helper");
        assert_eq!(cmd, ext.join("backend.sh"));
        assert_eq!(lessons(&d), Lessons::default());
    }

    #[test]
    fn lessons_come_from_the_first_extension_that_has_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("zpo_extension.json"), r#"{"agent": {"name": "X", "command": "x"}}"#)
            .unwrap();
        std::fs::write(
            b.join("zpo_extension.json"),
            r##"{"lessons": {"url": "https://l/s.html", "anchors": {"hops": "#h", "capacity": "#c"}}}"##,
        )
        .unwrap();
        let l = lessons(&[a, b]);
        assert_eq!(l.link("hops"), "https://l/s.html#h");
        assert_eq!(l.link("period"), "");
        let links: Vec<(String, String)> = l.links().into_iter().collect();
        assert_eq!(
            links,
            vec![
                ("hops".to_string(), "https://l/s.html#h".to_string()),
                ("capacity".to_string(), "https://l/s.html#c".to_string())
            ]
        );
        assert_eq!(Lessons::default().link("hops"), "");
        assert!(Lessons::default().links().is_empty());
    }
}
