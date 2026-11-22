// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, bail};
use argh::FromArgs;
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, Package, PackageId, semver};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use tempfile::TempDir;

#[derive(FromArgs, Debug)]
/// Inspect Cargo dependencies, filter out those already approved in Fuchsia's
/// third_party/rust_crates/Cargo.lock, and print OSRB review blocks.
struct Args {
    /// path(s) to target Cargo.toml or project directory containing Cargo.toml (can be repeated).
    #[argh(option, long = "manifest-path", short = 'm')]
    manifest_path: Vec<Utf8PathBuf>,

    /// crates.io crate specification(s) to inspect (e.g. 'serde_yaml' or 'zerocopy@0.8.63').
    #[argh(option, long = "crate", short = 'c')]
    crates: Vec<String>,

    /// path to Fuchsia's third_party/rust_crates/Cargo.lock (auto-detected if omitted).
    #[argh(option)]
    approved_lockfile: Option<Utf8PathBuf>,

    /// path to cargo binary to invoke. If omitted, uses $CARGO, or auto-detects
    /// Fuchsia's prebuilt cargo binary if present.
    #[argh(option)]
    cargo: Option<Utf8PathBuf>,

    /// value for 'Usage Description:' in OSRB output (default: 'Development Use').
    #[argh(option, short = 'u', default = "String::from(\"Development Use\")")]
    usage: String,

    /// specific workspace package name(s) to inspect. Defaults to all workspace members.
    #[argh(option, short = 'p')]
    package: Vec<String>,

    /// include dev-dependencies (DependencyKind::Development) in the graph traversal.
    #[argh(switch)]
    include_dev_dependencies: bool,

    /// include build-dependencies (DependencyKind::Build) in the graph traversal.
    #[argh(switch)]
    include_build_dependencies: bool,

    /// target platform triple to filter dependencies (e.g. x86_64-unknown-linux-gnu).
    #[argh(option)]
    filter_platform: Option<String>,

    /// pass --all-features to cargo metadata.
    #[argh(switch)]
    all_features: bool,

    /// pass --no-default-features to cargo metadata.
    #[argh(switch)]
    no_default_features: bool,

    /// features to enable (can be specified multiple times).
    #[argh(option)]
    features: Vec<String>,

    /// print summary counts only, suppressing individual OSRB blocks.
    #[argh(switch)]
    summary_only: bool,

    /// emit a Warning section instead of failing when unapproved crates have missing metadata or source files.
    #[argh(switch)]
    ignore_errors: bool,

    /// optional output file path. Defaults to stdout.
    #[argh(option, short = 'o')]
    output: Option<Utf8PathBuf>,
}

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: Option<String>,
    source: Option<String>,
}

#[derive(Default, Debug)]
struct FuchsiaCratesMetadata {
    approved: BTreeSet<String>,
    locked_versions: BTreeMap<String, BTreeSet<String>>,
    empty_stubs: BTreeMap<String, BTreeSet<String>>,
    forks: BTreeMap<String, BTreeSet<String>>,
    ask2patch: BTreeMap<String, BTreeSet<String>>,
    intree: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StubbedCallout {
    name: String,
    version: String,
    stub_paths: Vec<String>,
    locked_versions: Vec<String>,
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SkippableCallout {
    name: String,
    version: String,
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ForkedCallout {
    name: String,
    version: String,
    fork_paths: Vec<String>,
    locked_versions: Vec<String>,
    path: String,
}

#[derive(Default, Debug)]
struct Callouts {
    stubbed: Vec<StubbedCallout>,
    skippable: Vec<SkippableCallout>,
    forked: Vec<ForkedCallout>,
}

impl Callouts {
    fn is_empty(&self) -> bool {
        self.stubbed.is_empty() && self.skippable.is_empty() && self.forked.is_empty()
    }

    fn format(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str("=== Fuchsia Stubbed, Skippable, and Forked Dependency Callouts ===\n\n");

        if !self.stubbed.is_empty() {
            out.push_str(
                "Stubbed in //third_party/rust_crates/empty/ (already stubbed out in Fuchsia):\n",
            );
            for item in &self.stubbed {
                let stubs = item.stub_paths.join(", ");
                let locked = if item.locked_versions.is_empty() {
                    String::from("none")
                } else {
                    item.locked_versions.join(", ")
                };
                out.push_str(&format!(
                    "* {} ({}) [stub: {}, locked: {}]\n  Path: {}\n",
                    item.name, item.version, stubs, locked, item.path
                ));
            }
            out.push('\n');
        }

        if !self.skippable.is_empty() {
            out.push_str("Unapproved dependencies only reachable via empty stubs or non-Fuchsia targets (candidates to skip or stub):\n");
            for item in &self.skippable {
                out.push_str(&format!(
                    "* {} ({})\n  Path: {}\n",
                    item.name, item.version, item.path
                ));
            }
            out.push('\n');
        }

        if !self.forked.is_empty() {
            out.push_str("Forked or patched in //third_party/rust_crates/forks/ or ask2patch/:\n");
            for item in &self.forked {
                let forks = item.fork_paths.join(", ");
                let locked = if item.locked_versions.is_empty() {
                    String::from("none")
                } else {
                    item.locked_versions.join(", ")
                };
                out.push_str(&format!(
                    "* {} ({}) [path: {}, locked: {}]\n  Path: {}\n",
                    item.name, item.version, forks, locked, item.path
                ));
            }
            out.push('\n');
        }

        out.push_str("==================================================================\n");
        out
    }
}

struct DependencyGraph<'a> {
    graph: DiGraph<&'a PackageId, Vec<cargo_metadata::DepKindInfo>>,
    pkg_to_node: HashMap<&'a PackageId, NodeIndex>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OsrbEntry {
    name: String,
    version: String,
    urls: Vec<String>,
    license: String,
    license_urls: Vec<String>,
    usage: String,
}

impl OsrbEntry {
    fn format(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Name: {} ({})\n\n", self.name, self.version));
        out.push_str("URL:\n");
        for url in &self.urls {
            out.push_str(&format!("* {}\n", url));
        }
        out.push('\n');
        out.push_str(&format!("LICENSE: {}\n", self.license));
        for url in &self.license_urls {
            out.push_str(&format!("* {}\n", url));
        }
        out.push('\n');
        out.push_str(&format!("Usage Description: {}\n\n", self.usage));
        out.push_str("---\n");
        out
    }
}

fn find_fuchsia_dir() -> Option<Utf8PathBuf> {
    if let Ok(dir) = std::env::var("FUCHSIA_DIR") {
        let path = Utf8PathBuf::from(dir);
        if path.join("third_party/rust_crates/Cargo.lock").is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Ok(mut curr) = Utf8PathBuf::from_path_buf(exe)
    {
        while curr.pop() {
            if curr.join(".jiri_root").is_dir()
                || curr.join("third_party/rust_crates/Cargo.lock").is_file()
            {
                return Some(curr);
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(mut curr) = Utf8PathBuf::from_path_buf(cwd)
    {
        loop {
            if curr.join(".jiri_root").is_dir()
                || curr.join("third_party/rust_crates/Cargo.lock").is_file()
            {
                return Some(curr);
            }
            if !curr.pop() {
                break;
            }
        }
    }
    None
}

fn find_prebuilt_cargo(fuchsia_dir: &Utf8Path) -> Option<Utf8PathBuf> {
    if let Some(cargo_rel) = option_env!("FUCHSIA_CARGO_PATH") {
        let candidate = fuchsia_dir.join(cargo_rel);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn parse_cargo_toml_patches(toml_content: &str, meta: &mut FuchsiaCratesMetadata) -> Result<()> {
    let doc: toml::Value =
        toml::from_str(toml_content).context("Failed to parse Cargo.toml as TOML")?;
    let mut check_table = |table: &toml::map::Map<String, toml::Value>| {
        for (key, val) in table {
            if let Some(entry_table) = val.as_table()
                && let Some(path_val) = entry_table.get("path").and_then(toml::Value::as_str)
            {
                let path_clean = path_val.trim_start_matches("./");
                let crate_name =
                    entry_table.get("package").and_then(toml::Value::as_str).unwrap_or(key);
                if path_clean.starts_with("empty/") {
                    meta.empty_stubs
                        .entry(crate_name.to_string())
                        .or_default()
                        .insert(path_clean.to_string());
                    meta.approved.insert(crate_name.to_string());
                } else if path_clean.starts_with("forks/") {
                    meta.forks
                        .entry(crate_name.to_string())
                        .or_default()
                        .insert(path_clean.to_string());
                    meta.approved.insert(crate_name.to_string());
                } else if path_clean.starts_with("ask2patch/") {
                    meta.ask2patch
                        .entry(crate_name.to_string())
                        .or_default()
                        .insert(path_clean.to_string());
                    meta.approved.insert(crate_name.to_string());
                } else if path_clean.starts_with("intree/") {
                    meta.intree
                        .entry(crate_name.to_string())
                        .or_default()
                        .insert(path_clean.to_string());
                    meta.approved.insert(crate_name.to_string());
                }
            }
        }
    };

    if let Some(patch) = doc.get("patch").and_then(toml::Value::as_table)
        && let Some(crates_io) = patch.get("crates-io").and_then(toml::Value::as_table)
    {
        check_table(crates_io);
    }
    if let Some(deps) = doc.get("dependencies").and_then(toml::Value::as_table) {
        check_table(deps);
    }
    if let Some(ws) = doc.get("workspace").and_then(toml::Value::as_table)
        && let Some(deps) = ws.get("dependencies").and_then(toml::Value::as_table)
    {
        check_table(deps);
    }
    if let Some(target) = doc.get("target").and_then(toml::Value::as_table) {
        for (_cfg, val) in target {
            if let Some(deps) = val.get("dependencies").and_then(toml::Value::as_table) {
                check_table(deps);
            }
        }
    }
    Ok(())
}

fn load_fuchsia_crates_metadata(lockfile_path: &Utf8Path) -> Result<FuchsiaCratesMetadata> {
    let content = fs::read_to_string(lockfile_path)
        .with_context(|| format!("Failed to read approved lockfile at {lockfile_path}"))?;
    let lockfile: Lockfile = toml::from_str(&content)
        .with_context(|| format!("Failed to parse Cargo.lock from {lockfile_path}"))?;
    let mut meta = FuchsiaCratesMetadata::default();
    for pkg in lockfile.package {
        if let Some(version) = pkg.version {
            meta.locked_versions.entry(pkg.name.clone()).or_default().insert(version);
        }
        if pkg.source.is_some() {
            meta.approved.insert(pkg.name);
        }
    }

    let manifest_path = lockfile_path.with_file_name("Cargo.toml");
    if manifest_path.is_file() {
        let manifest_content = fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read {manifest_path}"))?;
        parse_cargo_toml_patches(&manifest_content, &mut meta)?;
    }

    Ok(meta)
}

fn is_non_fuchsia_target(target_str: &str) -> bool {
    let target_lower = target_str.to_ascii_lowercase();
    if target_lower.contains("not(")
        || target_lower.contains("fuchsia")
        || target_lower.contains("linux")
        || target_lower.contains("unix")
    {
        return false;
    }
    const NON_FUCHSIA_TOKENS: &[&str] = &[
        "windows",
        "msvc",
        "wasm",
        "wasi",
        "macos",
        "ios",
        "tvos",
        "watchos",
        "visionos",
        "redox",
        "haiku",
        "freebsd",
        "openbsd",
        "netbsd",
        "dragonfly",
        "solaris",
        "illumos",
        "android",
        "nto",
        "aix",
        "hurd",
        "uefi",
        "cygwin",
        "vxworks",
        "emscripten",
        "sgx",
    ];
    NON_FUCHSIA_TOKENS.iter().any(|tok| target_lower.contains(tok))
}

fn is_non_fuchsia_crate_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "windows"
        || n.starts_with("windows-")
        || n.starts_with("windows_")
        || n.starts_with("winapi")
        || n == "winsafe"
        || n.starts_with("wasm-bindgen")
        || n == "web-sys"
        || n == "js-sys"
        || n == "wit-bindgen-rt"
        || n == "anstyle-wincon"
        || n == "redox_syscall"
        || n == "output_vt100"
}

fn edge_target_annotation(
    kinds: &[cargo_metadata::DepKindInfo],
    include_dev: bool,
    include_build: bool,
    filter_platform: Option<&str>,
) -> Option<String> {
    let active_targets: Vec<String> = kinds
        .iter()
        .filter(|k| {
            if !include_dev && k.kind == DependencyKind::Development {
                return false;
            }
            if !include_build && k.kind == DependencyKind::Build {
                return false;
            }
            if let (Some(target_triple), Some(platform)) = (filter_platform, &k.target)
                && !platform.matches(target_triple, &[])
            {
                return false;
            }
            true
        })
        .filter_map(|k| k.target.as_ref().map(|t| t.to_string()))
        .collect();

    let matching_kinds_count = kinds
        .iter()
        .filter(|k| {
            if !include_dev && k.kind == DependencyKind::Development {
                return false;
            }
            if !include_build && k.kind == DependencyKind::Build {
                return false;
            }
            if let (Some(target_triple), Some(platform)) = (filter_platform, &k.target)
                && !platform.matches(target_triple, &[])
            {
                return false;
            }
            true
        })
        .count();

    if matching_kinds_count > 0 && active_targets.len() == matching_kinds_count {
        let mut uniq = active_targets;
        uniq.sort();
        uniq.dedup();
        Some(uniq.join(" | "))
    } else {
        None
    }
}

fn format_dep_path(
    target_idx: NodeIndex,
    dep_graph: &DependencyGraph<'_>,
    id_to_pkg: &HashMap<&PackageId, &Package>,
    predecessor: &HashMap<NodeIndex, (NodeIndex, Option<String>)>,
) -> String {
    let mut chain = Vec::new();
    let mut curr = target_idx;
    while let Some(&(prev, ref edge_target)) = predecessor.get(&curr) {
        let pkg_id = dep_graph.graph[curr];
        let pkg = id_to_pkg.get(&pkg_id);
        let name = pkg.map(|p| p.name.as_str()).unwrap_or("unknown");
        let version = pkg.map(|p| p.version.to_string());
        chain.push((name, version, edge_target.clone()));
        curr = prev;
    }
    let root_pkg_id = dep_graph.graph[curr];
    let root_name = id_to_pkg.get(&root_pkg_id).map(|p| p.name.as_str()).unwrap_or("root");

    let mut out = root_name.to_string();
    for (i, (name, version, edge_target)) in chain.iter().rev().enumerate() {
        if let Some(target) = edge_target {
            out.push_str(&format!(" -[{target}]-> "));
        } else {
            out.push_str(" -> ");
        }
        if i == chain.len() - 1 {
            if let Some(v) = version {
                out.push_str(&format!("{name} ({v})"));
            } else {
                out.push_str(name);
            }
        } else {
            out.push_str(name);
        }
    }
    out
}

fn build_dep_graph<'a>(metadata: &'a Metadata) -> Result<DependencyGraph<'a>> {
    let resolve = metadata
        .resolve
        .as_ref()
        .context("Missing resolve in cargo metadata. Run without --no-deps.")?;
    let mut graph = DiGraph::new();
    let mut pkg_to_node = HashMap::new();

    for node in &resolve.nodes {
        let node_idx = graph.add_node(&node.id);
        pkg_to_node.insert(&node.id, node_idx);
    }

    for node in &resolve.nodes {
        let source_idx =
            *pkg_to_node.get(&node.id).with_context(|| format!("Node {} not in graph", node.id))?;
        if !node.deps.is_empty() {
            for dep in &node.deps {
                if let Some(&target_idx) = pkg_to_node.get(&dep.pkg) {
                    graph.add_edge(source_idx, target_idx, dep.dep_kinds.clone());
                }
            }
        } else {
            for dep_id in &node.dependencies {
                if let Some(&target_idx) = pkg_to_node.get(dep_id) {
                    graph.add_edge(source_idx, target_idx, Vec::new());
                }
            }
        }
    }

    Ok(DependencyGraph { graph, pkg_to_node })
}

fn filter_edge(
    kinds: &[cargo_metadata::DepKindInfo],
    include_dev: bool,
    include_build: bool,
    filter_platform: Option<&str>,
) -> bool {
    if kinds.is_empty() {
        return true;
    }
    kinds.iter().any(|kind_info| {
        if !include_dev && kind_info.kind == DependencyKind::Development {
            return false;
        }
        if !include_build && kind_info.kind == DependencyKind::Build {
            return false;
        }
        if let (Some(target_triple), Some(platform)) = (filter_platform, &kind_info.target)
            && !platform.matches(target_triple, &[])
        {
            return false;
        }
        true
    })
}

fn find_local_git_info(manifest_path: &Utf8Path) -> Option<(Utf8PathBuf, String, String)> {
    let mut dir_opt = manifest_path.parent();
    while let Some(dir) = dir_opt {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            let (git_dir, common_dir) = if dot_git.is_dir() {
                (dot_git.clone(), dot_git)
            } else if let Ok(content) = fs::read_to_string(&dot_git)
                && let Some(rest) = content.trim().strip_prefix("gitdir:")
            {
                let gitdir_path = Utf8PathBuf::from(rest.trim());
                let resolved =
                    if gitdir_path.is_absolute() { gitdir_path } else { dir.join(gitdir_path) };
                let common =
                    if let Ok(commondir_content) = fs::read_to_string(resolved.join("commondir")) {
                        let cd = Utf8PathBuf::from(commondir_content.trim());
                        if cd.is_absolute() { cd } else { resolved.join(cd) }
                    } else {
                        resolved.clone()
                    };
                (resolved, common)
            } else {
                return None;
            };

            let repo_url = if let Ok(config_content) = fs::read_to_string(common_dir.join("config"))
            {
                let mut url = None;
                let mut in_origin_section = false;
                for line in config_content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        in_origin_section = trimmed == "[remote \"origin\"]";
                    } else if let Some((k, v)) = trimmed.split_once('=')
                        && k.trim() == "url"
                    {
                        let raw_url = v.trim();
                        if in_origin_section || url.is_none() {
                            url = Some(raw_url.to_string());
                        }
                    }
                }
                url.map(|u| {
                    let u = u.trim_end_matches(".git");
                    if let Some(rest) = u.strip_prefix("git@github.com:") {
                        format!("https://github.com/{rest}")
                    } else if let Some(rest) = u.strip_prefix("ssh://git@github.com/") {
                        format!("https://github.com/{rest}")
                    } else {
                        u.to_string()
                    }
                })
            } else {
                None
            };

            let repo_url = repo_url?;

            let commit = if let Ok(head_content) = fs::read_to_string(git_dir.join("HEAD")) {
                let head = head_content.trim();
                if let Some(ref_path) = head.strip_prefix("ref: ") {
                    let ref_path = ref_path.trim();
                    if let Ok(c) = fs::read_to_string(git_dir.join(ref_path))
                        .or_else(|_| fs::read_to_string(common_dir.join(ref_path)))
                    {
                        c.trim().to_string()
                    } else if let Ok(packed) = fs::read_to_string(common_dir.join("packed-refs")) {
                        let mut found = None;
                        for line in packed.lines() {
                            let trimmed = line.trim();
                            if !trimmed.starts_with('#')
                                && !trimmed.starts_with('^')
                                && let Some((sha, name)) = trimmed.split_once(' ')
                                && name == ref_path
                            {
                                found = Some(sha.to_string());
                                break;
                            }
                        }
                        found.unwrap_or_else(|| {
                            ref_path.strip_prefix("refs/heads/").unwrap_or("HEAD").to_string()
                        })
                    } else {
                        ref_path.strip_prefix("refs/heads/").unwrap_or("HEAD").to_string()
                    }
                } else if !head.is_empty() {
                    head.to_string()
                } else {
                    String::from("HEAD")
                }
            } else {
                String::from("HEAD")
            };

            return Some((dir.to_path_buf(), repo_url, commit));
        }
        dir_opt = dir.parent();
    }
    None
}

fn find_license_files(pkg: &Package) -> Vec<String> {
    let mut files = BTreeSet::new();
    let manifest_path = Utf8Path::new(&pkg.manifest_path);
    let Some(pkg_dir) = manifest_path.parent() else {
        return Vec::new();
    };

    if let Some(custom_file) = &pkg.license_file {
        let custom_path = pkg_dir.join(custom_file);
        if custom_path.is_file() {
            files.insert(custom_file.to_string());
        }
    }

    if let Ok(entries) = fs::read_dir(pkg_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type()
                && file_type.is_file()
            {
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                let upper = name_str.to_ascii_uppercase();
                if upper.starts_with("LICENSE")
                    || upper.starts_with("LICENCE")
                    || upper.starts_with("COPYING")
                    || upper.starts_with("COPYRIGHT")
                    || upper.starts_with("UNLICENSE")
                {
                    files.insert(name_str.to_string());
                }
            }
        }
    }

    if files.is_empty() {
        if let Some(source) = &pkg.source
            && source.repr.starts_with("git+")
        {
            let mut dir_opt = pkg_dir.parent();
            while let Some(dir) = dir_opt {
                if dir.join(".git").exists() {
                    if let Ok(entries) = fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            if let Ok(file_type) = entry.file_type()
                                && file_type.is_file()
                            {
                                let name = entry.file_name();
                                let name_str = name.to_string_lossy();
                                let upper = name_str.to_ascii_uppercase();
                                if (upper.starts_with("LICENSE")
                                    || upper.starts_with("LICENCE")
                                    || upper.starts_with("COPYING")
                                    || upper.starts_with("COPYRIGHT")
                                    || upper.starts_with("UNLICENSE"))
                                    && let Ok(rel) = entry.path().strip_prefix(dir)
                                {
                                    files.insert(rel.to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                    break;
                }
                dir_opt = dir.parent();
            }
        } else if pkg.source.is_none()
            && let Some((git_root, _, _)) = find_local_git_info(manifest_path)
            && let Ok(entries) = fs::read_dir(&git_root)
        {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type()
                    && file_type.is_file()
                {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    let upper = name_str.to_ascii_uppercase();
                    if (upper.starts_with("LICENSE")
                        || upper.starts_with("LICENCE")
                        || upper.starts_with("COPYING")
                        || upper.starts_with("COPYRIGHT")
                        || upper.starts_with("UNLICENSE"))
                        && let Ok(rel) = entry.path().strip_prefix(&git_root)
                    {
                        files.insert(rel.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    files.into_iter().collect()
}

fn format_license_urls(pkg: &Package, license_files: &[String]) -> Result<Vec<String>> {
    let name = &pkg.name;
    let version = &pkg.version;

    let is_crates_io = pkg.source.as_ref().is_some_and(|s| s.is_crates_io());
    if is_crates_io {
        if license_files.is_empty() {
            return Ok(vec![format!("https://docs.rs/crate/{name}/{version}/source/Cargo.toml")]);
        }
        return Ok(license_files
            .iter()
            .map(|f| format!("https://docs.rs/crate/{name}/{version}/source/{f}"))
            .collect());
    }

    if let Some(source) = &pkg.source
        && source.repr.starts_with("git+")
    {
        let repr = source.repr.strip_prefix("git+").unwrap_or(&source.repr);
        let (repo_part, commit) = repr.split_once('#').unwrap_or((repr, "HEAD"));
        let repo_base = repo_part.split('?').next().unwrap_or(repo_part);
        let repo_url = repo_base.trim_end_matches(".git");
        if license_files.is_empty() {
            return Ok(vec![format!("{repo_url}/blob/{commit}/Cargo.toml")]);
        }
        return Ok(license_files.iter().map(|f| format!("{repo_url}/blob/{commit}/{f}")).collect());
    }

    if let Some(repo) = &pkg.repository
        && !repo.trim().is_empty()
    {
        let repo_url = repo.trim_end_matches(".git");
        if license_files.is_empty() {
            return Ok(vec![format!("{repo_url}/blob/HEAD/Cargo.toml")]);
        }
        return Ok(license_files.iter().map(|f| format!("{repo_url}/blob/HEAD/{f}")).collect());
    }

    if pkg.source.is_none()
        && let Some((git_root, repo_url, commit)) =
            find_local_git_info(Utf8Path::new(&pkg.manifest_path))
    {
        if license_files.is_empty() {
            let rel_manifest = Utf8Path::new(&pkg.manifest_path)
                .strip_prefix(&git_root)
                .map(|p| p.as_str())
                .unwrap_or("Cargo.toml");
            return Ok(vec![format!("{repo_url}/blob/{commit}/{rel_manifest}")]);
        }
        return Ok(license_files
            .iter()
            .map(|f| {
                let file_path = Utf8Path::new(f);
                if file_path.is_relative() && git_root.join(file_path).is_file() {
                    format!("{repo_url}/blob/{commit}/{f}")
                } else if let Ok(rel) = Utf8Path::new(&pkg.manifest_path)
                    .parent()
                    .unwrap_or_else(|| Utf8Path::new(""))
                    .join(f)
                    .strip_prefix(&git_root)
                {
                    format!("{repo_url}/blob/{commit}/{rel}")
                } else {
                    format!("{repo_url}/blob/{commit}/{f}")
                }
            })
            .collect());
    }

    bail!(
        "could not determine license URL for {} ({}) (not a crates.io crate and no repository or git remote URL found for {})",
        pkg.name,
        pkg.version,
        pkg.manifest_path
    )
}

fn format_project_urls(pkg: &Package) -> Result<Vec<String>> {
    let mut urls = Vec::new();
    let is_crates_io = pkg.source.as_ref().is_some_and(|s| s.is_crates_io());

    if is_crates_io {
        urls.push(format!("https://docs.rs/crate/{}/{}", pkg.name, pkg.version));
    }

    let upstream = if let Some(repo) = &pkg.repository
        && !repo.is_empty()
    {
        Some(repo.trim_end_matches(".git").to_string())
    } else if let Some(homepage) = &pkg.homepage
        && !homepage.is_empty()
    {
        Some(homepage.clone())
    } else if let Some(source) = &pkg.source
        && source.repr.starts_with("git+")
    {
        let repr = source.repr.strip_prefix("git+").unwrap_or(&source.repr);
        let (repo_part, _) = repr.split_once('#').unwrap_or((repr, ""));
        let repo_base = repo_part.split('?').next().unwrap_or(repo_part);
        Some(repo_base.trim_end_matches(".git").to_string())
    } else if pkg.source.is_none()
        && let Some((_, repo_url, _)) = find_local_git_info(Utf8Path::new(&pkg.manifest_path))
    {
        Some(repo_url)
    } else {
        None
    };

    if let Some(u) = upstream
        && !urls.contains(&u)
    {
        urls.push(u);
    }

    if urls.is_empty() {
        bail!(
            "could not determine project URL for {} ({}) (not a crates.io crate and no repository, homepage, or git remote URL found for {})",
            pkg.name,
            pkg.version,
            pkg.manifest_path
        );
    }

    Ok(urls)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SummaryCounts {
    total_inspected: usize,
    approved_count: usize,
    unapproved_count: usize,
    direct_unapproved_count: usize,
    indirect_unapproved_count: usize,
    skippable_count: usize,
    warning_count: usize,
}

fn format_summary(
    manifest_paths: &[Utf8PathBuf],
    crates: &[String],
    approved_lockfile: &Utf8Path,
    counts: SummaryCounts,
) -> String {
    let mut out = String::new();
    out.push_str("=== Cargo OSRB Dependency Summary ===\n");
    if !manifest_paths.is_empty() {
        let paths: Vec<String> = manifest_paths.iter().map(|p| p.to_string()).collect();
        out.push_str(&format!("Manifest path(s): {}\n", paths.join(", ")));
    }
    if !crates.is_empty() {
        out.push_str(&format!("Target crate(s): {}\n", crates.join(", ")));
    }
    out.push_str(&format!("Approved lockfile: {approved_lockfile}\n"));
    out.push_str(&format!(
        "Total reachable external/candidate crates: {}\n",
        counts.total_inspected
    ));
    out.push_str(&format!("Already approved crates in Fuchsia: {}\n", counts.approved_count));
    out.push_str(&format!(
        "New dependencies needing OSRB approval: {} ({} direct, {} indirect; {} candidates to skip or stub)\n",
        counts.unapproved_count,
        counts.direct_unapproved_count,
        counts.indirect_unapproved_count,
        counts.skippable_count
    ));
    if counts.warning_count > 0 {
        out.push_str(&format!(
            "Unapproved crates with metadata errors (see warnings below): {}\n",
            counts.warning_count
        ));
    }
    out
}

fn format_warnings(validation_errors: &[String]) -> String {
    if validation_errors.is_empty() {
        return String::new();
    }
    format!(
        "=== Warning: Failed to resolve required OSRB metadata for {} unapproved crate(s) ===\n\n{}\n\n===================================================================================\n",
        validation_errors.len(),
        validation_errors.join("\n\n")
    )
}

fn assemble_output(
    summary_str: &str,
    warnings: &[String],
    callouts: &Callouts,
    direct_unapproved: &[OsrbEntry],
    indirect_unapproved: &[OsrbEntry],
    summary_only: bool,
) -> String {
    let mut sections: Vec<String> = vec![summary_str.trim_end().to_string()];

    if !warnings.is_empty() {
        sections.push(format_warnings(warnings).trim_end().to_string());
    }

    if !callouts.is_empty() {
        sections.push(callouts.format().trim_end().to_string());
    }

    if !summary_only {
        if !direct_unapproved.is_empty() {
            let mut direct_section =
                String::from("=== New direct dependencies that require OSRB approval ===\n\n");
            for (i, entry) in direct_unapproved.iter().enumerate() {
                if i > 0 {
                    direct_section.push('\n');
                }
                direct_section.push_str(&entry.format());
            }
            sections.push(direct_section.trim_end().to_string());
        }

        if !indirect_unapproved.is_empty() {
            let mut indirect_section =
                String::from("=== New indirect dependencies that require OSRB approval ===\n\n");
            for (i, entry) in indirect_unapproved.iter().enumerate() {
                if i > 0 {
                    indirect_section.push('\n');
                }
                indirect_section.push_str(&entry.format());
            }
            sections.push(indirect_section.trim_end().to_string());
        }
    }

    let mut out = sections.join("\n\n");
    out.push('\n');
    out
}

fn parse_crate_spec(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("Crate specification cannot be empty");
    }
    let (name, version) = if let Some((n, v)) =
        spec.split_once('@').or_else(|| spec.split_once(':')).or_else(|| spec.split_once('='))
    {
        let n = n.trim();
        let v = v.trim();
        if v.is_empty() {
            bail!("Empty version in crate specification '{spec}'");
        }
        (n, v)
    } else {
        (spec, "*")
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("Invalid crate name '{name}' in specification '{spec}'");
    }
    Ok((name.to_string(), version.to_string()))
}

fn create_synthetic_manifest(
    dir: &Utf8Path,
    crates: &[String],
    no_default_features: bool,
    features: &[String],
) -> Result<Utf8PathBuf> {
    if crates.len() > 1 && !features.is_empty() {
        bail!("--features cannot be used when multiple --crate arguments are provided");
    }
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).with_context(|| format!("Failed to create {src_dir}"))?;
    fs::write(src_dir.join("lib.rs"), "")
        .with_context(|| format!("Failed to write {src_dir}/lib.rs"))?;

    let mut manifest = String::from(
        "[package]\nname = \"cargo-osrb-synthetic-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\n",
    );

    let parsed_features: Vec<&str> = features
        .iter()
        .flat_map(|f| f.split(','))
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();

    for spec in crates {
        let (name, version) = parse_crate_spec(spec)?;
        let mut dep_table = toml::map::Map::new();
        dep_table.insert("version".to_string(), toml::Value::String(version));
        if no_default_features {
            dep_table.insert("default-features".to_string(), toml::Value::Boolean(false));
        }
        if !parsed_features.is_empty() {
            let feat_vals =
                parsed_features.iter().map(|&f| toml::Value::String(f.to_string())).collect();
            dep_table.insert("features".to_string(), toml::Value::Array(feat_vals));
        }
        let dep_inline =
            toml::to_string(&dep_table).context("Failed to serialize dependency table")?;
        manifest.push_str(&format!("\n[dependencies.{name}]\n{dep_inline}"));
    }

    let manifest_path = dir.join("Cargo.toml");
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("Failed to write synthetic manifest to {manifest_path}"))?;
    Ok(manifest_path)
}

fn validate_and_build_osrb_entry(
    pkg: &Package,
    usage: &str,
) -> std::result::Result<OsrbEntry, Vec<String>> {
    let mut errors = Vec::new();

    if pkg.source.is_none() {
        if !pkg.manifest_path.is_file() {
            errors.push(format!("manifest file not found on disk: {}", pkg.manifest_path));
        }
        for target in &pkg.targets {
            if !target.src_path.is_file() {
                errors.push(format!(
                    "target '{}' source file not found on disk: {} (is a git submodule or generated source missing?)",
                    target.name, target.src_path
                ));
            }
        }
    }

    let license_name = match pkg
        .license
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"))
    {
        Some(l) => l,
        None => {
            errors.push(format!(
                "missing `license` field in Cargo.toml for {} ({}) at {}",
                pkg.name, pkg.version, pkg.manifest_path
            ));
            ""
        }
    };

    let project_urls = match format_project_urls(pkg) {
        Ok(urls) => urls,
        Err(e) => {
            errors.push(e.to_string());
            Vec::new()
        }
    };

    let license_files = find_license_files(pkg);
    let license_urls = match format_license_urls(pkg, &license_files) {
        Ok(urls) => urls,
        Err(e) => {
            errors.push(e.to_string());
            Vec::new()
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(OsrbEntry {
        name: pkg.name.to_string(),
        version: pkg.version.to_string(),
        urls: project_urls,
        license: license_name.to_string(),
        license_urls,
        usage: usage.to_string(),
    })
}

fn execute(args: Args) -> Result<()> {
    if args.manifest_path.is_empty() && args.crates.is_empty() {
        bail!("At least one --manifest-path (-m) or --crate (-c) must be provided.");
    }

    let mut resolved_manifest_paths = Vec::new();
    for p in &args.manifest_path {
        let mut path = p.clone();
        if path.is_dir() {
            path.push("Cargo.toml");
        }
        if !path.is_file() {
            bail!("Cargo manifest not found at {path}");
        }
        resolved_manifest_paths.push(path);
    }

    let fuchsia_dir = find_fuchsia_dir();

    let approved_lockfile = if let Some(path) = &args.approved_lockfile {
        path.clone()
    } else if let Some(ref dir) = fuchsia_dir {
        let lockfile = dir.join("third_party/rust_crates/Cargo.lock");
        if !lockfile.is_file() {
            bail!(
                "Auto-detected Fuchsia dir at {dir}, but {lockfile} does not exist. Specify --approved-lockfile."
            );
        }
        lockfile
    } else {
        bail!(
            "Could not auto-detect Fuchsia directory or third_party/rust_crates/Cargo.lock. Please provide --approved-lockfile."
        );
    };

    let fuchsia_meta = load_fuchsia_crates_metadata(&approved_lockfile)?;

    let cargo_bin = if let Some(bin) = &args.cargo {
        Some(bin.clone())
    } else if let Ok(env_cargo) = std::env::var("CARGO") {
        if !env_cargo.is_empty() { Some(Utf8PathBuf::from(env_cargo)) } else { None }
    } else if let Some(ref dir) = fuchsia_dir {
        find_prebuilt_cargo(dir)
    } else {
        None
    };

    let synth_temp = if !args.crates.is_empty() {
        let temp = TempDir::new().context("Failed to create temporary directory")?;
        let temp_path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf())
            .map_err(|p| anyhow::anyhow!("Invalid UTF-8 temporary directory path: {:?}", p))?;
        let synth_path = create_synthetic_manifest(
            &temp_path,
            &args.crates,
            args.no_default_features,
            &args.features,
        )?;
        Some((temp, synth_path))
    } else {
        None
    };

    struct TargetRun {
        manifest_path: Utf8PathBuf,
        is_synthetic: bool,
    }

    let mut target_runs = Vec::new();
    for p in &resolved_manifest_paths {
        target_runs.push(TargetRun { manifest_path: p.clone(), is_synthetic: false });
    }
    if let Some((_, ref synth_path)) = synth_temp {
        target_runs.push(TargetRun { manifest_path: synth_path.clone(), is_synthetic: true });
    }

    let mut all_packages: BTreeMap<(String, semver::Version), Package> = BTreeMap::new();
    let mut pkg_paths: BTreeMap<(String, semver::Version), String> = BTreeMap::new();
    let mut direct_keys: BTreeSet<(String, semver::Version)> = BTreeSet::new();
    let mut active_keys: BTreeSet<(String, semver::Version)> = BTreeSet::new();

    let mut stubbed_map: BTreeMap<(String, String), StubbedCallout> = BTreeMap::new();
    let mut skippable_map: BTreeMap<(String, String), SkippableCallout> = BTreeMap::new();
    let mut forked_map: BTreeMap<(String, String), ForkedCallout> = BTreeMap::new();

    for run in &target_runs {
        let mut cmd = MetadataCommand::new();
        cmd.manifest_path(&run.manifest_path);
        if let Some(ref bin) = cargo_bin {
            cmd.cargo_path(bin);
        }

        if !run.is_synthetic {
            if args.all_features {
                cmd.features(cargo_metadata::CargoOpt::AllFeatures);
            }
            if args.no_default_features {
                cmd.features(cargo_metadata::CargoOpt::NoDefaultFeatures);
            }
            if !args.features.is_empty() {
                cmd.features(cargo_metadata::CargoOpt::SomeFeatures(args.features.clone()));
            }
        }
        if let Some(triple) = &args.filter_platform {
            cmd.other_options(vec!["--filter-platform".to_string(), triple.clone()]);
        }

        let metadata = cmd
            .exec()
            .with_context(|| format!("Failed to run cargo metadata on {}", run.manifest_path))?;
        let dep_graph = build_dep_graph(&metadata)?;
        let id_to_pkg: HashMap<&PackageId, &Package> =
            metadata.packages.iter().map(|p| (&p.id, p)).collect();

        let root_ids: Vec<&PackageId> = if run.is_synthetic {
            let probe_root_id = metadata
                .workspace_members
                .first()
                .context("Missing root package in synthetic probe metadata")?;
            let probe_node_idx = *dep_graph
                .pkg_to_node
                .get(probe_root_id)
                .context("Synthetic root not found in graph")?;
            let mut direct_ids = Vec::new();
            for edge in dep_graph.graph.edges(probe_node_idx) {
                let kinds = edge.weight();
                if filter_edge(
                    kinds,
                    args.include_dev_dependencies,
                    args.include_build_dependencies,
                    args.filter_platform.as_deref(),
                ) {
                    direct_ids.push(dep_graph.graph[edge.target()]);
                }
            }
            direct_ids
        } else if !args.package.is_empty() {
            let mut matched = Vec::new();
            for member_id in &metadata.workspace_members {
                if let Some(pkg) = id_to_pkg.get(member_id)
                    && args.package.iter().any(|name| name.as_str() == pkg.name.as_str())
                {
                    matched.push(member_id);
                }
            }
            if matched.is_empty() {
                bail!(
                    "No workspace packages matched the provided --package filter(s): {:?}",
                    args.package
                );
            }
            matched
        } else if let Some(root_pkg) = metadata.root_package() {
            vec![&root_pkg.id]
        } else {
            metadata.workspace_members.iter().collect()
        };

        for &root_id in &root_ids {
            if let Some(pkg) = id_to_pkg.get(root_id) {
                direct_keys.insert((pkg.name.to_string(), pkg.version.clone()));
            }
        }

        let mut visited_nodes = BTreeSet::new();
        let mut predecessor: HashMap<NodeIndex, (NodeIndex, Option<String>)> = HashMap::new();
        let mut all_queue = VecDeque::new();

        for root_id in &root_ids {
            if let Some(&root_idx) = dep_graph.pkg_to_node.get(root_id)
                && visited_nodes.insert(root_idx)
            {
                all_queue.push_back(root_idx);
            }
        }

        while let Some(u) = all_queue.pop_front() {
            for edge in dep_graph.graph.edges(u) {
                let v = edge.target();
                let kinds = edge.weight();
                if !filter_edge(
                    kinds,
                    args.include_dev_dependencies,
                    args.include_build_dependencies,
                    args.filter_platform.as_deref(),
                ) {
                    continue;
                }

                if visited_nodes.insert(v) {
                    let edge_target = edge_target_annotation(
                        kinds,
                        args.include_dev_dependencies,
                        args.include_build_dependencies,
                        args.filter_platform.as_deref(),
                    );
                    predecessor.insert(v, (u, edge_target));
                    all_queue.push_back(v);
                }
            }
        }

        let mut active_nodes = BTreeSet::new();
        let mut active_queue = VecDeque::new();
        for root_id in &root_ids {
            if let Some(&root_idx) = dep_graph.pkg_to_node.get(root_id)
                && active_nodes.insert(root_idx)
            {
                active_queue.push_back(root_idx);
            }
        }

        while let Some(u) = active_queue.pop_front() {
            let pkg_id_u = dep_graph.graph[u];
            let pkg_u = id_to_pkg.get(&pkg_id_u);
            let u_name = pkg_u.map(|p| p.name.as_str()).unwrap_or("");

            if fuchsia_meta.empty_stubs.contains_key(u_name) || is_non_fuchsia_crate_name(u_name) {
                continue;
            }

            for edge in dep_graph.graph.edges(u) {
                let v = edge.target();
                let kinds = edge.weight();
                if !filter_edge(
                    kinds,
                    args.include_dev_dependencies,
                    args.include_build_dependencies,
                    args.filter_platform.as_deref(),
                ) {
                    continue;
                }

                let pkg_id_v = dep_graph.graph[v];
                let pkg_v = id_to_pkg.get(&pkg_id_v);
                let v_name = pkg_v.map(|p| p.name.as_str()).unwrap_or("");

                let has_fuchsia_applicable_kind = kinds.is_empty()
                    || kinds.iter().any(|k| {
                        if !args.include_dev_dependencies && k.kind == DependencyKind::Development {
                            return false;
                        }
                        if !args.include_build_dependencies && k.kind == DependencyKind::Build {
                            return false;
                        }
                        k.target.as_ref().is_none_or(|t| !is_non_fuchsia_target(&t.to_string()))
                    });

                if !has_fuchsia_applicable_kind || is_non_fuchsia_crate_name(v_name) {
                    continue;
                }

                if active_nodes.insert(v) {
                    active_queue.push_back(v);
                }
            }
        }

        for &node_idx in &visited_nodes {
            let pkg_id = dep_graph.graph[node_idx];
            let Some(pkg) = id_to_pkg.get(&pkg_id) else {
                continue;
            };

            let pkg_key = (pkg.name.to_string(), pkg.version.clone());
            all_packages.entry(pkg_key.clone()).or_insert_with(|| (*pkg).clone());
            pkg_paths
                .entry(pkg_key.clone())
                .or_insert_with(|| format_dep_path(node_idx, &dep_graph, &id_to_pkg, &predecessor));

            if active_nodes.contains(&node_idx) {
                active_keys.insert(pkg_key);
            }

            let is_approved = fuchsia_meta.approved.contains(pkg.name.as_str());

            // 1. Stubbed crates in Fuchsia (empty/...)
            if let Some(stub_paths) = fuchsia_meta.empty_stubs.get(pkg.name.as_str()) {
                let locked_versions: Vec<String> = fuchsia_meta
                    .locked_versions
                    .get(pkg.name.as_str())
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                let path = format_dep_path(node_idx, &dep_graph, &id_to_pkg, &predecessor);
                stubbed_map.entry((pkg.name.to_string(), pkg.version.to_string())).or_insert_with(
                    || StubbedCallout {
                        name: pkg.name.to_string(),
                        version: pkg.version.to_string(),
                        stub_paths: stub_paths.iter().cloned().collect(),
                        locked_versions,
                        path,
                    },
                );
            }

            // 2. Forked or patched crates in Fuchsia (forks/... or ask2patch/...)
            let has_fork = fuchsia_meta.forks.contains_key(pkg.name.as_str());
            let has_ask2patch = fuchsia_meta.ask2patch.contains_key(pkg.name.as_str());
            if has_fork || has_ask2patch {
                let mut fork_paths: Vec<String> = Vec::new();
                if let Some(paths) = fuchsia_meta.forks.get(pkg.name.as_str()) {
                    fork_paths.extend(paths.iter().cloned());
                }
                if let Some(paths) = fuchsia_meta.ask2patch.get(pkg.name.as_str()) {
                    fork_paths.extend(paths.iter().cloned());
                }
                let locked_versions: Vec<String> = fuchsia_meta
                    .locked_versions
                    .get(pkg.name.as_str())
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                let path = format_dep_path(node_idx, &dep_graph, &id_to_pkg, &predecessor);
                forked_map.entry((pkg.name.to_string(), pkg.version.to_string())).or_insert_with(
                    || ForkedCallout {
                        name: pkg.name.to_string(),
                        version: pkg.version.to_string(),
                        fork_paths,
                        locked_versions,
                        path,
                    },
                );
            }

            // 3. Unapproved dependencies only reachable via empty stubs or non-Fuchsia targets
            if !is_approved
                && !active_nodes.contains(&node_idx)
                && !fuchsia_meta.empty_stubs.contains_key(pkg.name.as_str())
            {
                let path = format_dep_path(node_idx, &dep_graph, &id_to_pkg, &predecessor);
                skippable_map
                    .entry((pkg.name.to_string(), pkg.version.to_string()))
                    .or_insert_with(|| SkippableCallout {
                        name: pkg.name.to_string(),
                        version: pkg.version.to_string(),
                        path,
                    });
            }
        }
    }

    skippable_map.retain(|(name, version), _| {
        if let Ok(sem_ver) = semver::Version::parse(version) {
            !active_keys.contains(&(name.clone(), sem_ver))
                && !fuchsia_meta.approved.contains(name.as_str())
        } else {
            !fuchsia_meta.approved.contains(name.as_str())
        }
    });

    let mut callouts = Callouts {
        stubbed: stubbed_map.into_values().collect(),
        skippable: skippable_map.into_values().collect(),
        forked: forked_map.into_values().collect(),
    };
    callouts.stubbed.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    callouts.skippable.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    callouts.forked.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));

    let mut approved_count = 0;
    let mut direct_unapproved: Vec<OsrbEntry> = Vec::new();
    let mut indirect_unapproved: Vec<OsrbEntry> = Vec::new();
    let mut validation_errors: Vec<String> = Vec::new();

    for ((name, semver_version), pkg) in &all_packages {
        if fuchsia_meta.approved.contains(name.as_str()) {
            approved_count += 1;
            continue;
        }

        match validate_and_build_osrb_entry(pkg, &args.usage) {
            Ok(entry) => {
                if direct_keys.contains(&(name.clone(), semver_version.clone())) {
                    direct_unapproved.push(entry);
                } else {
                    indirect_unapproved.push(entry);
                }
            }
            Err(reasons) => {
                let source_desc = match &pkg.source {
                    Some(s) => format!("{s} (manifest: {})", pkg.manifest_path),
                    None => format!("local path dependency (manifest: {})", pkg.manifest_path),
                };
                let dep_path = pkg_paths
                    .get(&(name.clone(), semver_version.clone()))
                    .map(String::as_str)
                    .unwrap_or(name.as_str());
                let reasons_str = reasons
                    .into_iter()
                    .map(|r| format!("    - {r}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                validation_errors.push(format!(
                    "* {name} ({semver_version}) [{source_desc}]\n  Path: {dep_path}\n{reasons_str}"
                ));
            }
        }
    }

    if !validation_errors.is_empty() && !args.ignore_errors {
        bail!(
            "Failed to resolve required OSRB metadata for {} unapproved crate(s) (pass --ignore-errors to emit warnings instead):\n\n{}",
            validation_errors.len(),
            validation_errors.join("\n\n")
        );
    }

    direct_unapproved.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });

    indirect_unapproved.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });

    let warning_count = validation_errors.len();
    let unapproved_count = direct_unapproved.len() + indirect_unapproved.len();
    let total_inspected = approved_count + unapproved_count + warning_count;

    let summary_str = format_summary(
        &resolved_manifest_paths,
        &args.crates,
        &approved_lockfile,
        SummaryCounts {
            total_inspected,
            approved_count,
            unapproved_count,
            direct_unapproved_count: direct_unapproved.len(),
            indirect_unapproved_count: indirect_unapproved.len(),
            skippable_count: callouts.skippable.len(),
            warning_count,
        },
    );

    let output_str = assemble_output(
        &summary_str,
        &validation_errors,
        &callouts,
        &direct_unapproved,
        &indirect_unapproved,
        args.summary_only,
    );

    if let Some(out_path) = &args.output {
        fs::write(out_path, &output_str)
            .with_context(|| format!("Failed to write output to {out_path}"))?;
        let warning_suffix = if warning_count > 0 {
            format!(" ({warning_count} warning(s))")
        } else {
            String::new()
        };
        if args.summary_only {
            eprintln!("Wrote summary{warning_suffix} to {out_path}");
        } else {
            eprintln!(
                "Wrote {} new OSRB dependency entries ({} direct, {} indirect){} to {}",
                unapproved_count,
                direct_unapproved.len(),
                indirect_unapproved.len(),
                warning_suffix,
                out_path
            );
        }
    } else {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(output_str.as_bytes())?;
    }

    drop(synth_temp);
    Ok(())
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    execute(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::visit::{Bfs, EdgeFiltered};
    use pretty_assertions::assert_eq;

    #[fuchsia::test]
    fn test_load_fuchsia_crates_metadata() {
        let temp = TempDir::new().unwrap();
        let lock_path = Utf8PathBuf::from_path_buf(temp.path().join("Cargo.lock")).unwrap();
        let toml_path = Utf8PathBuf::from_path_buf(temp.path().join("Cargo.toml")).unwrap();

        let lock_content = r#"
# This file is automatically `@generated` by Cargo.
# It is not intended for manual editing.
version = 3

[[package]]
name = "approved-crate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "123456"

[[package]]
name = "in-tree-crate"
version = "0.1.0"

[[package]]
name = "git-approved"
version = "2.0.0"
source = "git+https://github.com/foo/bar#abcdef1234567890"

[[package]]
name = "windows-sys"
version = "0.60.2"

[[package]]
name = "petgraph"
version = "0.8.3"
"#;
        fs::write(&lock_path, lock_content).unwrap();

        let toml_content = r#"
[patch.crates-io]
windows-sys = { path = "empty/windows-sys-0.60.2" }
petgraph = { path = "forks/petgraph-0.8.3" }
aho-corasick = { path = "ask2patch/aho-corasick" }
bssl-sys = { path = "intree/bssl-sys" }
"#;
        fs::write(&toml_path, toml_content).unwrap();

        let meta = load_fuchsia_crates_metadata(&lock_path).unwrap();
        assert!(meta.approved.contains("approved-crate"));
        assert!(meta.approved.contains("git-approved"));
        assert!(meta.approved.contains("windows-sys"));
        assert!(meta.approved.contains("petgraph"));
        assert!(meta.approved.contains("aho-corasick"));
        assert!(meta.approved.contains("bssl-sys"));
        assert!(!meta.approved.contains("in-tree-crate"));
        assert!(!meta.approved.contains("non-existent"));

        assert_eq!(
            meta.empty_stubs.get("windows-sys").unwrap().iter().collect::<Vec<_>>(),
            vec!["empty/windows-sys-0.60.2"]
        );
        assert_eq!(
            meta.forks.get("petgraph").unwrap().iter().collect::<Vec<_>>(),
            vec!["forks/petgraph-0.8.3"]
        );
        assert_eq!(
            meta.ask2patch.get("aho-corasick").unwrap().iter().collect::<Vec<_>>(),
            vec!["ask2patch/aho-corasick"]
        );
        assert_eq!(
            meta.intree.get("bssl-sys").unwrap().iter().collect::<Vec<_>>(),
            vec!["intree/bssl-sys"]
        );
    }

    #[fuchsia::test]
    fn test_stubbed_and_forked_callouts() {
        let callouts = Callouts {
            stubbed: vec![StubbedCallout {
                name: "windows-sys".to_string(),
                version: "0.60.2".to_string(),
                stub_paths: vec!["empty/windows-sys-0.60.2".to_string()],
                locked_versions: vec!["0.52.0".to_string(), "0.60.2".to_string()],
                path: "charon -> anstream -> windows-sys (0.60.2)".to_string(),
            }],
            skippable: vec![SkippableCallout {
                name: "windows-targets".to_string(),
                version: "0.53.3".to_string(),
                path: "charon -> anstream -> windows-sys -> windows-targets (0.53.3)".to_string(),
            }],
            forked: vec![ForkedCallout {
                name: "petgraph".to_string(),
                version: "0.8.2".to_string(),
                fork_paths: vec!["forks/petgraph-0.8.3".to_string()],
                locked_versions: vec!["0.8.3".to_string()],
                path: "charon -> petgraph (0.8.2)".to_string(),
            }],
        };

        let formatted = callouts.format();
        assert!(formatted.contains("Stubbed in //third_party/rust_crates/empty/"));
        assert!(formatted.contains(
            "* windows-sys (0.60.2) [stub: empty/windows-sys-0.60.2, locked: 0.52.0, 0.60.2]"
        ));
        assert!(formatted.contains("Path: charon -> anstream -> windows-sys (0.60.2)"));
        assert!(formatted.contains(
            "Unapproved dependencies only reachable via empty stubs or non-Fuchsia targets (candidates to skip or stub):"
        ));
        assert!(formatted.contains("* windows-targets (0.53.3)"));
        assert!(
            formatted
                .contains("Path: charon -> anstream -> windows-sys -> windows-targets (0.53.3)")
        );
        assert!(
            formatted
                .contains("Forked or patched in //third_party/rust_crates/forks/ or ask2patch/:")
        );
        assert!(
            formatted.contains("* petgraph (0.8.2) [path: forks/petgraph-0.8.3, locked: 0.8.3]")
        );
        assert!(formatted.contains("Path: charon -> petgraph (0.8.2)"));
    }

    #[fuchsia::test]
    fn test_callouts_empty_format() {
        let empty_callouts = Callouts::default();
        let formatted = empty_callouts.format();
        assert_eq!(formatted, "");
    }

    #[fuchsia::test]
    fn test_osrb_entry_format() {
        let entry = OsrbEntry {
            name: "zerocopy".to_string(),
            version: "0.8.63".to_string(),
            urls: vec![
                "https://docs.rs/crate/zerocopy/0.8.63".to_string(),
                "https://github.com/google/zerocopy".to_string(),
            ],
            license: "Apache-2.0 OR MIT".to_string(),
            license_urls: vec![
                "https://docs.rs/crate/zerocopy/0.8.63/source/LICENSE-APACHE".to_string(),
                "https://docs.rs/crate/zerocopy/0.8.63/source/LICENSE-MIT".to_string(),
            ],
            usage: "Development Use".to_string(),
        };

        let formatted = entry.format();
        let expected = "\
Name: zerocopy (0.8.63)

URL:
* https://docs.rs/crate/zerocopy/0.8.63
* https://github.com/google/zerocopy

LICENSE: Apache-2.0 OR MIT
* https://docs.rs/crate/zerocopy/0.8.63/source/LICENSE-APACHE
* https://docs.rs/crate/zerocopy/0.8.63/source/LICENSE-MIT

Usage Description: Development Use

---
";
        assert_eq!(formatted, expected);
    }

    #[fuchsia::test]
    fn test_find_license_files() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let manifest_path = pkg_dir.join("Cargo.toml");

        fs::write(&manifest_path, "[package]\nname = \"foo\"\nversion = \"0.1.0\"").unwrap();
        fs::write(pkg_dir.join("LICENSE-APACHE"), "Apache License").unwrap();
        fs::write(pkg_dir.join("LICENSE-MIT"), "MIT License").unwrap();
        fs::write(pkg_dir.join("README.md"), "# Foo").unwrap();

        let json_pkg = format!(
            r#"{{
            "name": "foo",
            "version": "0.1.0",
            "id": "foo 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
            "license": "Apache-2.0 OR MIT",
            "license_file": null,
            "description": null,
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "dependencies": [],
            "targets": [],
            "features": {{}},
            "manifest_path": "{}",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }}"#,
            manifest_path
        );

        let pkg: Package = serde_json::from_str(&json_pkg).unwrap();
        let files = find_license_files(&pkg);
        assert_eq!(files, vec!["LICENSE-APACHE", "LICENSE-MIT"]);

        let urls = format_license_urls(&pkg, &files).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://docs.rs/crate/foo/0.1.0/source/LICENSE-APACHE",
                "https://docs.rs/crate/foo/0.1.0/source/LICENSE-MIT"
            ]
        );
    }

    #[fuchsia::test]
    fn test_format_license_urls_git() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let manifest_path = pkg_dir.join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname = \"bar\"\nversion = \"0.2.0\"").unwrap();

        let json_pkg = format!(
            r#"{{
            "name": "bar",
            "version": "0.2.0",
            "id": "bar 0.2.0 (git+https://github.com/org/repo?branch=main#abcdef1234567890)",
            "license": "MIT",
            "license_file": null,
            "description": null,
            "source": "git+https://github.com/org/repo?branch=main#abcdef1234567890",
            "dependencies": [],
            "targets": [],
            "features": {{}},
            "manifest_path": "{}",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": "https://github.com/org/repo",
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }}"#,
            manifest_path
        );

        let pkg: Package = serde_json::from_str(&json_pkg).unwrap();
        let files = vec!["LICENSE".to_string()];
        let urls = format_license_urls(&pkg, &files).unwrap();
        assert_eq!(urls, vec!["https://github.com/org/repo/blob/abcdef1234567890/LICENSE"]);
    }

    #[fuchsia::test]
    fn test_format_project_urls() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let manifest_path = pkg_dir.join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname = \"baz\"\nversion = \"1.0.0\"").unwrap();

        let json_pkg = format!(
            r#"{{
            "name": "baz",
            "version": "1.0.0",
            "id": "baz 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
            "license": "MIT",
            "license_file": null,
            "description": null,
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "dependencies": [],
            "targets": [],
            "features": {{}},
            "manifest_path": "{}",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": "https://github.com/example/baz.git",
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }}"#,
            manifest_path
        );

        let pkg: Package = serde_json::from_str(&json_pkg).unwrap();
        let urls = format_project_urls(&pkg).unwrap();
        assert_eq!(
            urls,
            vec![
                "https://docs.rs/crate/baz/1.0.0".to_string(),
                "https://github.com/example/baz".to_string(),
            ]
        );
    }

    #[fuchsia::test]
    fn test_format_project_urls_git() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let manifest_path = pkg_dir.join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname = \"bar\"\nversion = \"0.2.0\"").unwrap();

        let json_pkg = format!(
            r#"{{
            "name": "bar",
            "version": "0.2.0",
            "id": "bar 0.2.0 (git+https://github.com/org/repo?branch=main#abcdef1234567890)",
            "license": "MIT",
            "license_file": null,
            "description": null,
            "source": "git+https://github.com/org/repo?branch=main#abcdef1234567890",
            "dependencies": [],
            "targets": [],
            "features": {{}},
            "manifest_path": "{}",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": "https://github.com/org/repo",
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }}"#,
            manifest_path
        );

        let pkg: Package = serde_json::from_str(&json_pkg).unwrap();
        let urls = format_project_urls(&pkg).unwrap();
        assert_eq!(urls, vec!["https://github.com/org/repo".to_string()]);
    }

    #[fuchsia::test]
    fn test_validate_and_build_osrb_entry_errors() {
        let temp = TempDir::new().unwrap();
        let pkg_dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let manifest_path = pkg_dir.join("Cargo.toml");
        fs::write(&manifest_path, "[package]\nname = \"missing-crate\"\nversion = \"0.1.0\"\n")
            .unwrap();

        let json_pkg = format!(
            r#"{{
            "name": "missing-crate",
            "version": "0.1.0",
            "id": "missing-crate 0.1.0 (path+file://{})",
            "license": null,
            "license_file": null,
            "description": null,
            "source": null,
            "dependencies": [],
            "targets": [
                {{
                    "name": "missing-lib",
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "required_features": [],
                    "src_path": "{}/non_existent/lib.rs",
                    "edition": "2024",
                    "doctest": false,
                    "test": false
                }}
            ],
            "features": {{}},
            "manifest_path": "{}",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }}"#,
            pkg_dir, pkg_dir, manifest_path
        );

        let pkg: Package = serde_json::from_str(&json_pkg).unwrap();
        let errs = validate_and_build_osrb_entry(&pkg, "Development Use").unwrap_err();
        assert!(errs.iter().any(|e| e.contains("missing `license` field in Cargo.toml")));
        assert!(errs.iter().any(|e| e.contains("source file not found on disk")));
        assert!(errs.iter().any(|e| e.contains("could not determine project URL")));
        assert!(errs.iter().any(|e| e.contains("could not determine license URL")));
    }

    #[fuchsia::test]
    fn test_dep_graph_filtering() {
        let metadata_json = r#"{
            "packages": [
                {
                    "name": "root",
                    "version": "0.1.0",
                    "id": "root 0.1.0 (path+file:///root)",
                    "dependencies": [],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/root/Cargo.toml",
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "edition": "2024"
                },
                {
                    "name": "normal-dep",
                    "version": "0.1.0",
                    "id": "normal-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                    "dependencies": [],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/cargo/registry/normal-dep/Cargo.toml",
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "edition": "2024"
                },
                {
                    "name": "dev-dep",
                    "version": "0.1.0",
                    "id": "dev-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                    "dependencies": [],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/cargo/registry/dev-dep/Cargo.toml",
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "edition": "2024"
                },
                {
                    "name": "build-dep",
                    "version": "0.1.0",
                    "id": "build-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                    "dependencies": [],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/cargo/registry/build-dep/Cargo.toml",
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "edition": "2024"
                }
            ],
            "workspace_members": [
                "root 0.1.0 (path+file:///root)"
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "root 0.1.0 (path+file:///root)",
                        "dependencies": [
                            "normal-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                            "dev-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                            "build-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)"
                        ],
                        "deps": [
                            {
                                "name": "normal-dep",
                                "pkg": "normal-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                                "dep_kinds": [
                                    {
                                        "kind": null,
                                        "target": null
                                    }
                                ]
                            },
                            {
                                "name": "dev-dep",
                                "pkg": "dev-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                                "dep_kinds": [
                                    {
                                        "kind": "dev",
                                        "target": null
                                    }
                                ]
                            },
                            {
                                "name": "build-dep",
                                "pkg": "build-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                                "dep_kinds": [
                                    {
                                        "kind": "build",
                                        "target": null
                                    }
                                ]
                            }
                        ]
                    },
                    {
                        "id": "normal-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                        "dependencies": [],
                        "deps": []
                    },
                    {
                        "id": "dev-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                        "dependencies": [],
                        "deps": []
                    },
                    {
                        "id": "build-dep 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                        "dependencies": [],
                        "deps": []
                    }
                ],
                "root": "root 0.1.0 (path+file:///root)"
            },
            "target_directory": "/root/target",
            "version": 1,
            "workspace_root": "/root"
        }"#;

        let metadata: Metadata = serde_json::from_str(metadata_json).unwrap();
        let dep_graph = build_dep_graph(&metadata).unwrap();
        let root_id = &metadata.workspace_members[0];
        let root_idx = *dep_graph.pkg_to_node.get(root_id).unwrap();

        // 1. Default (normal dependencies only, dev and build excluded)
        let edge_filtered = EdgeFiltered::from_fn(&dep_graph.graph, |edge| {
            filter_edge(edge.weight(), false, false, None)
        });
        let mut bfs = Bfs::new(&edge_filtered, root_idx);
        let mut visited = BTreeSet::new();
        while let Some(nx) = bfs.next(&edge_filtered) {
            visited.insert(dep_graph.graph[nx].repr.as_str());
        }
        assert_eq!(visited.len(), 2);
        assert!(visited.iter().any(|s| s.starts_with("root")));
        assert!(visited.iter().any(|s| s.starts_with("normal-dep")));

        // 2. Include dev dependencies only
        let edge_filtered = EdgeFiltered::from_fn(&dep_graph.graph, |edge| {
            filter_edge(edge.weight(), true, false, None)
        });
        let mut bfs = Bfs::new(&edge_filtered, root_idx);
        let mut visited = BTreeSet::new();
        while let Some(nx) = bfs.next(&edge_filtered) {
            visited.insert(dep_graph.graph[nx].repr.as_str());
        }
        assert_eq!(visited.len(), 3);
        assert!(visited.iter().any(|s| s.starts_with("dev-dep")));
        assert!(!visited.iter().any(|s| s.starts_with("build-dep")));

        // 3. Include build dependencies only
        let edge_filtered = EdgeFiltered::from_fn(&dep_graph.graph, |edge| {
            filter_edge(edge.weight(), false, true, None)
        });
        let mut bfs = Bfs::new(&edge_filtered, root_idx);
        let mut visited = BTreeSet::new();
        while let Some(nx) = bfs.next(&edge_filtered) {
            visited.insert(dep_graph.graph[nx].repr.as_str());
        }
        assert_eq!(visited.len(), 3);
        assert!(visited.iter().any(|s| s.starts_with("build-dep")));
        assert!(!visited.iter().any(|s| s.starts_with("dev-dep")));

        // 4. Include both dev and build dependencies
        let edge_filtered = EdgeFiltered::from_fn(&dep_graph.graph, |edge| {
            filter_edge(edge.weight(), true, true, None)
        });
        let mut bfs = Bfs::new(&edge_filtered, root_idx);
        let mut visited = BTreeSet::new();
        while let Some(nx) = bfs.next(&edge_filtered) {
            visited.insert(dep_graph.graph[nx].repr.as_str());
        }
        assert_eq!(visited.len(), 4);
    }

    #[fuchsia::test]
    fn test_parse_crate_spec() {
        assert_eq!(
            parse_crate_spec("serde_yaml").unwrap(),
            ("serde_yaml".to_string(), "*".to_string())
        );
        assert_eq!(
            parse_crate_spec("zerocopy@0.8.63").unwrap(),
            ("zerocopy".to_string(), "0.8.63".to_string())
        );
        assert_eq!(
            parse_crate_spec("syn:2.0.98").unwrap(),
            ("syn".to_string(), "2.0.98".to_string())
        );
        assert_eq!(
            parse_crate_spec("quote=1.0.38").unwrap(),
            ("quote".to_string(), "1.0.38".to_string())
        );
        assert!(parse_crate_spec("").is_err());
        assert!(parse_crate_spec("  ").is_err());
        assert!(parse_crate_spec("foo@").is_err());
        assert!(parse_crate_spec("foo/bar").is_err());
    }

    #[fuchsia::test]
    fn test_create_synthetic_manifest() {
        let temp = TempDir::new().unwrap();
        let temp_path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let crates = vec!["serde@1.0".to_string()];
        let features = vec!["derive,alloc".to_string()];
        let manifest_path =
            create_synthetic_manifest(&temp_path, &crates, true, &features).unwrap();

        assert!(manifest_path.is_file());
        assert!(temp_path.join("src/lib.rs").is_file());

        let content = fs::read_to_string(&manifest_path).unwrap();
        assert!(content.contains("[package]"));
        assert!(content.contains("name = \"cargo-osrb-synthetic-probe\""));
        assert!(content.contains("[dependencies.serde]"));
        assert!(content.contains("version = \"1.0\""));
        assert!(content.contains("default-features = false"));
        assert!(content.contains("derive"));
        assert!(content.contains("alloc"));

        // Multi-crate without features should succeed
        let multi_crates = vec!["serde@1.0".to_string(), "anyhow".to_string()];
        let multi_manifest =
            create_synthetic_manifest(&temp_path, &multi_crates, false, &[]).unwrap();
        let multi_content = fs::read_to_string(&multi_manifest).unwrap();
        assert!(multi_content.contains("[dependencies.anyhow]"));
        assert!(multi_content.contains("version = \"*\""));

        // Multi-crate with features should fail
        assert!(create_synthetic_manifest(&temp_path, &multi_crates, false, &features).is_err());
    }

    #[fuchsia::test]
    fn test_format_summary() {
        let manifest_paths = vec![Utf8PathBuf::from("/path/to/Cargo.toml")];
        let crates = vec!["serde".to_string(), "tokio@1.0".to_string()];
        let approved_lockfile = Utf8Path::new("/path/to/approved/Cargo.lock");
        let counts = SummaryCounts {
            total_inspected: 15,
            approved_count: 5,
            unapproved_count: 10,
            direct_unapproved_count: 2,
            indirect_unapproved_count: 8,
            skippable_count: 3,
            warning_count: 0,
        };
        let summary = format_summary(&manifest_paths, &crates, approved_lockfile, counts);
        assert!(summary.contains("=== Cargo OSRB Dependency Summary ==="));
        assert!(summary.contains("Manifest path(s): /path/to/Cargo.toml"));
        assert!(summary.contains("Target crate(s): serde, tokio@1.0"));
        assert!(summary.contains("Approved lockfile: /path/to/approved/Cargo.lock"));
        assert!(summary.contains("Total reachable external/candidate crates: 15"));
        assert!(summary.contains("Already approved crates in Fuchsia: 5"));
        assert!(summary.contains("New dependencies needing OSRB approval: 10 (2 direct, 8 indirect; 3 candidates to skip or stub)"));
        assert!(!summary.contains("Unapproved crates with metadata errors"));

        let counts_with_warnings = SummaryCounts { warning_count: 2, ..counts };
        let summary_with_warnings =
            format_summary(&manifest_paths, &crates, approved_lockfile, counts_with_warnings);
        assert!(
            summary_with_warnings
                .contains("Unapproved crates with metadata errors (see warnings below): 2")
        );
    }

    #[fuchsia::test]
    fn test_find_local_git_info() {
        let temp = TempDir::new().unwrap();
        let temp_path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let dot_git = temp_path.join(".git");
        fs::create_dir_all(&dot_git).unwrap();

        let config_content = "[remote \"origin\"]\n\turl = git@github.com:AeneasVerif/charon.git\n";
        fs::write(dot_git.join("config"), config_content).unwrap();

        let head_content = "ref: refs/heads/main\n";
        fs::write(dot_git.join("HEAD"), head_content).unwrap();

        let refs_dir = dot_git.join("refs/heads");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join("main"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();

        let sub_manifest = temp_path.join("crate/Cargo.toml");
        fs::create_dir_all(sub_manifest.parent().unwrap()).unwrap();
        fs::write(&sub_manifest, "[package]\nname = \"example-crate\"\nversion = \"0.1.0\"\n")
            .unwrap();

        let res = find_local_git_info(&sub_manifest);
        assert!(res.is_some());
        let (root, repo_url, commit) = res.unwrap();
        assert_eq!(root, temp_path);
        assert_eq!(repo_url, "https://github.com/AeneasVerif/charon");
        assert_eq!(commit, "0123456789abcdef0123456789abcdef01234567");
    }

    #[fuchsia::test]
    fn test_args_multiple_manifests_and_crates() {
        let args = Args::from_args(
            &["cargo-osrb"],
            &[
                "-m",
                "foo/Cargo.toml",
                "-m",
                "bar/Cargo.toml",
                "-c",
                "serde_json",
                "-c",
                "tokio@1.0",
                "--summary-only",
            ],
        )
        .unwrap();
        assert_eq!(args.manifest_path.len(), 2);
        assert_eq!(args.manifest_path[0], Utf8PathBuf::from("foo/Cargo.toml"));
        assert_eq!(args.manifest_path[1], Utf8PathBuf::from("bar/Cargo.toml"));
        assert_eq!(args.crates.len(), 2);
        assert_eq!(args.crates[0], "serde_json");
        assert_eq!(args.crates[1], "tokio@1.0");
        assert!(args.summary_only);
        assert!(!args.ignore_errors);
    }

    #[fuchsia::test]
    fn test_format_warnings_and_ignore_errors_flag() {
        let args = Args::from_args(&["cargo-osrb"], &["-c", "serde", "--ignore-errors"]).unwrap();
        assert!(args.ignore_errors);

        assert_eq!(format_warnings(&[]), "");

        let warnings = vec![
            "* minirust-rs (0.1.0) [local path dependency]\n  Path: charon -> minirust-rs (0.1.0)\n    - could not determine project URL".to_string(),
        ];
        let formatted = format_warnings(&warnings);
        assert!(formatted.contains(
            "=== Warning: Failed to resolve required OSRB metadata for 1 unapproved crate(s) ==="
        ));
        assert!(formatted.contains("* minirust-rs (0.1.0)"));
        assert!(formatted.contains("could not determine project URL"));
    }

    #[fuchsia::test]
    fn test_assemble_output_omits_empty_sections() {
        let summary = "=== Cargo OSRB Dependency Summary ===\nApproved lockfile: Cargo.lock\n";
        let empty_callouts = Callouts::default();

        // When everything is empty, only the summary is emitted.
        let minimal = assemble_output(summary, &[], &empty_callouts, &[], &[], false);
        assert_eq!(minimal, format!("{}\n", summary.trim_end()));
        assert!(!minimal.contains("None detected"));
        assert!(!minimal.contains("New direct dependencies"));
        assert!(!minimal.contains("New indirect dependencies"));
        assert!(!minimal.contains("Warning:"));

        // When direct, indirect, callouts, and warnings are all populated.
        let sample_entry = |name: &str| OsrbEntry {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            urls: vec![format!("https://docs.rs/crate/{name}/0.1.0")],
            license: "MIT".to_string(),
            license_urls: vec![format!("https://docs.rs/crate/{name}/0.1.0/source/LICENSE")],
            usage: "Development Use".to_string(),
        };
        let callouts = Callouts {
            skippable: vec![SkippableCallout {
                name: "winapi".to_string(),
                version: "0.3.9".to_string(),
                path: "root -> winapi (0.3.9)".to_string(),
            }],
            ..Default::default()
        };
        let warnings = vec!["* bad-crate (0.1.0)\n    - missing license".to_string()];
        let direct = vec![sample_entry("direct-crate")];
        let indirect = vec![sample_entry("indirect-crate")];

        let full = assemble_output(summary, &warnings, &callouts, &direct, &indirect, false);
        let warn_idx = full.find("=== Warning:").unwrap();
        let callout_idx = full
            .find("=== Fuchsia Stubbed, Skippable, and Forked Dependency Callouts ===")
            .unwrap();
        let direct_idx =
            full.find("=== New direct dependencies that require OSRB approval ===").unwrap();
        let indirect_idx =
            full.find("=== New indirect dependencies that require OSRB approval ===").unwrap();
        assert!(warn_idx < callout_idx);
        assert!(callout_idx < direct_idx);
        assert!(direct_idx < indirect_idx);

        // In summary_only mode, direct and indirect sections are omitted while warnings and callouts remain.
        let summary_only = assemble_output(summary, &warnings, &callouts, &direct, &indirect, true);
        assert!(summary_only.contains("=== Warning:"));
        assert!(
            summary_only
                .contains("=== Fuchsia Stubbed, Skippable, and Forked Dependency Callouts ===")
        );
        assert!(!summary_only.contains("=== New direct dependencies"));
        assert!(!summary_only.contains("=== New indirect dependencies"));
    }
}
