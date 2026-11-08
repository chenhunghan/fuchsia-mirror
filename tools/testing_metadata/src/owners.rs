// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// A single resolved owner entry for a directory, annotated with proximity metadata so downstream
/// dashboards and alerts can distinguish direct owners from inherited fallback owners.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OwnerEntry {
    /// Developer or team email address declared in `owners_file`. Used by dashboards and alerting
    /// pipelines to filter and route test failures by owner.
    pub(crate) email: String,
    /// 1-based sequential ownership tier (`1` = closest direct owners) that increments each time
    /// the `(owners_step, include_depth)` proximity level advances. Exists to give SQL queries and
    /// dashboards a single integer column for filtering (e.g. `tier = 1` for primary owners)
    /// without reconstructing the traversal order.
    pub(crate) tier: u32,
    /// 1-based index of the ancestor `OWNERS` file that contributed this owner when walking up the
    /// directory tree (`1` = the nearest `OWNERS` file in the directory or an ancestor, `2` = the
    /// next ancestor directory with an `OWNERS` file, etc.). Exists to distinguish owners defined
    /// at the nearest governing `OWNERS` file from fallback owners inherited from parent
    /// directories.
    pub(crate) owners_step: u32,
    /// Breadth-first `include` / `file:` hop count from the ancestor `OWNERS` file at
    /// `owners_step` (`0` = email listed directly in that `OWNERS` file, `1` = listed in a
    /// directly included file, `2` = listed in a transitively included file, etc.). Exists so
    /// direct team members in an `OWNERS` file can be separated from umbrella approvers pulled in
    /// via `include` directives.
    pub(crate) include_depth: u32,
    /// Number of directory hops up from the directory to the ancestor `OWNERS` file at
    /// `owners_step` (`0` = the directory itself). Exists to measure physical directory tree
    /// distance when intermediate directories have no `OWNERS` file (whereas `owners_step` only
    /// counts ancestors that contain an `OWNERS` file).
    pub(crate) dir_distance: u32,
    /// Repository-relative path of the `OWNERS` file where `email` was declared (e.g.
    /// `"src/sys/test_manager/OWNERS"`). Exists for provenance and debugging so engineers can see
    /// which `OWNERS` file or `include` target attributed a test to them.
    pub(crate) owners_file: String,
}

/// Per-directory ownership and component metadata emitted in
/// [`DirectoryOwnersOutput::directories`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub(crate) struct DirectoryOwnershipOutput {
    /// Buganizer issue tracker component ID resolved from the nearest `METADATA.textproto` file in
    /// this directory or an ancestor. Exists so automated test policy tooling can file or route
    /// bugs for failing tests in this directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) component_id: Option<i64>,
    /// Proximity-ordered list of resolved [`OwnerEntry`] records for this directory. Omitted when
    /// no non-root `OWNERS` file applies (or when `set noparent` stops inheritance with no
    /// owners).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) owners: Vec<OwnerEntry>,
}

/// Root schema for `directory_owners.json`, mapping each repository-relative directory path to its
/// resolved `component_id` and proximity-ranked `owners`.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct DirectoryOwnersOutput {
    /// Maps every repository-relative directory path to its [`DirectoryOwnershipOutput`]
    /// (`component_id` and proximity-ordered `owners`). Exists so any test's GN label directory
    /// can be resolved in O(1) by downstream tools like `testsharder` and BigQuery pipelines.
    pub(crate) directories: BTreeMap<String, DirectoryOwnershipOutput>,
    /// Schema version number (currently `1`). Exists so downstream consumers and BigQuery
    /// ingestion pipelines can validate compatibility before parsing.
    pub(crate) version: u32,
}

/// Parsed representation of a single `OWNERS` file before `include` expansion or parent traversal.
#[derive(Debug, Clone, Default)]
struct ParsedOwnersFile {
    /// Email addresses declared directly in this `OWNERS` file (excluding `per-file` rules).
    direct_owners: Vec<String>,
    /// Normalized repository-relative paths of files referenced via `include` or `file:`
    /// directives.
    includes: Vec<PathBuf>,
    /// Whether this `OWNERS` file contains a `set noparent` directive stopping ancestor directory
    /// inheritance.
    noparent: bool,
}

/// Collects `OWNERS` and `METADATA.textproto` files and resolves proximity-ranked directory owners.
#[derive(Default)]
pub(crate) struct OwnershipCollector {
    /// Cache of parsed `OWNERS` files keyed by normalized repository-relative file path.
    owners_files: HashMap<PathBuf, ParsedOwnersFile>,
    /// Buganizer component IDs extracted from `METADATA.textproto`, keyed by repository-relative
    /// directory path.
    component_ids: HashMap<PathBuf, i64>,
    /// Sorted set of repository-relative `OWNERS` and `METADATA.textproto` file paths read during
    /// collection and `include` expansion, emitted into the Ninja depfile.
    dep_paths: BTreeSet<PathBuf>,
}

impl OwnershipCollector {
    /// Inspects a single repository file during directory traversal, caching `OWNERS` and
    /// `METADATA.textproto` contents and recording their paths for the Ninja depfile.
    pub(crate) fn visit_file(&mut self, abs_path: &Path, rel_path: &Path) {
        let Some(file_name) = abs_path.file_name() else {
            return;
        };
        let norm_rel = normalize_rel_path(rel_path);
        if file_name == "OWNERS" {
            self.dep_paths.insert(norm_rel.clone());
            if let Ok(parsed) = parse_owners_file(abs_path, &norm_rel) {
                self.owners_files.insert(norm_rel, parsed);
            }
        } else if file_name == "METADATA.textproto" {
            self.dep_paths.insert(norm_rel.clone());
            let dir_key = norm_rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            if let Some(component_id) = parse_metadata_component_id(abs_path) {
                self.component_ids.insert(dir_key, component_id);
            }
        }
    }

    /// Resolves `component_id` and proximity-ranked `owners` for all directories in `all_dirs` and
    /// returns `(output, relative_dep_paths)`.
    pub(crate) fn finish(
        &mut self,
        all_dirs: &[PathBuf],
        abs_fuchsia_dir: &Path,
    ) -> (DirectoryOwnersOutput, Vec<PathBuf>) {
        let mut directories = BTreeMap::new();
        let mut resolved_cache: HashMap<PathBuf, Vec<OwnerEntry>> = HashMap::new();

        for dir in all_dirs {
            let owners =
                if let Some(odir_path) = self.resolve_owners_directory(dir, abs_fuchsia_dir) {
                    let hop_offset = hop_distance(dir, &odir_path);
                    let base_owners = resolved_cache
                        .entry(odir_path.clone())
                        .or_insert_with(|| self.resolve_owners(&odir_path, abs_fuchsia_dir));
                    base_owners
                        .iter()
                        .map(|entry| {
                            let mut adjusted = entry.clone();
                            adjusted.dir_distance += hop_offset;
                            adjusted
                        })
                        .collect()
                } else {
                    Vec::new()
                };
            let entry =
                DirectoryOwnershipOutput { component_id: self.resolve_component_id(dir), owners };
            directories.insert(dir.to_string_lossy().into_owned(), entry);
        }

        let output = DirectoryOwnersOutput { directories, version: 1 };
        let deps = std::mem::take(&mut self.dep_paths).into_iter().collect();
        (output, deps)
    }

    /// Walks up the directory tree from `dir` to the repository root (`""`) and returns the first
    /// Buganizer `component_id` defined in a `METADATA.textproto` file.
    fn resolve_component_id(&self, dir: &Path) -> Option<i64> {
        let mut curr = dir;
        loop {
            if let Some(&cid) = self.component_ids.get(curr) {
                return Some(cid);
            }
            if curr == Path::new("") || curr == Path::new(".") {
                return self.component_ids.get(Path::new("")).copied();
            }
            curr = curr.parent().unwrap_or_else(|| Path::new(""));
        }
    }

    /// Finds the nearest ancestor directory (or `dir` itself, excluding the repository root `""`)
    /// whose `OWNERS` file expands to at least one owner, stopping early if an ancestor `OWNERS`
    /// file specifies `set noparent`.
    fn resolve_owners_directory(&mut self, dir: &Path, abs_fuchsia_dir: &Path) -> Option<PathBuf> {
        let mut curr = dir;
        while curr != Path::new("") && curr != Path::new(".") {
            let owners_rel = curr.join("OWNERS");
            if self.get_or_load_owners_file(&owners_rel, abs_fuchsia_dir).is_some() {
                let (expanded, noparent) = self.expand_owners_bfs(&owners_rel, abs_fuchsia_dir);
                if !expanded.is_empty() {
                    return Some(curr.to_path_buf());
                }
                if noparent {
                    return None;
                }
            }
            match curr.parent() {
                Some(parent) => curr = parent,
                None => break,
            }
        }
        None
    }

    /// Walks ancestor directories starting from `dir` (excluding the repository root `""`),
    /// expanding each `OWNERS` file breadth-first and deduplicating emails to produce the
    /// proximity-ranked [`OwnerEntry`] list for `dir`.
    fn resolve_owners(&mut self, dir: &Path, abs_fuchsia_dir: &Path) -> Vec<OwnerEntry> {
        let mut owners = Vec::new();
        let mut seen_emails = HashSet::new();
        let mut curr = dir;
        let mut dir_distance = 0u32;
        let mut owners_step = 0u32;
        let mut current_tier = 0u32;

        // Stop before the repository root ("") so global approvers in //OWNERS are not inherited
        // unless explicitly included by a subdirectory OWNERS file.
        while curr != Path::new("") && curr != Path::new(".") {
            let owners_rel = curr.join("OWNERS");
            if self.get_or_load_owners_file(&owners_rel, abs_fuchsia_dir).is_some() {
                let (expanded, noparent) = self.expand_owners_bfs(&owners_rel, abs_fuchsia_dir);
                let mut step_incremented = false;
                let mut last_include_depth: Option<u32> = None;

                for (email, include_depth, owners_file) in expanded {
                    if !seen_emails.insert(email.clone()) {
                        continue;
                    }
                    if !step_incremented {
                        owners_step += 1;
                        step_incremented = true;
                    }
                    if last_include_depth != Some(include_depth) {
                        current_tier += 1;
                        last_include_depth = Some(include_depth);
                    }
                    owners.push(OwnerEntry {
                        email,
                        tier: current_tier,
                        owners_step,
                        include_depth,
                        dir_distance,
                        owners_file,
                    });
                }

                if noparent {
                    break;
                }
            }

            dir_distance += 1;
            match curr.parent() {
                Some(parent) => curr = parent,
                None => break,
            }
        }

        owners
    }

    /// Returns the cached [`ParsedOwnersFile`] for `rel_owners_path`, or lazily parses and caches
    /// it from disk (for example when an `include` or `file:` directive references a custom-named
    /// file like `OWNERS_EXTRA` that was not visited as `OWNERS` during the initial tree walk).
    fn get_or_load_owners_file(
        &mut self,
        rel_owners_path: &Path,
        abs_fuchsia_dir: &Path,
    ) -> Option<ParsedOwnersFile> {
        let norm_rel = normalize_rel_path(rel_owners_path);
        if let Some(existing) = self.owners_files.get(&norm_rel) {
            return Some(existing.clone());
        }
        let abs_path = abs_fuchsia_dir.join(&norm_rel);
        if !abs_path.is_file() {
            return None;
        }
        self.dep_paths.insert(norm_rel.clone());
        let parsed = parse_owners_file(&abs_path, &norm_rel).ok()?;
        self.owners_files.insert(norm_rel, parsed.clone());
        Some(parsed)
    }

    /// Expands an `OWNERS` file breadth-first so direct owners (`include_depth = 0`) precede
    /// included owners (`include_depth = 1+`), returning `(expanded_owners, start_noparent)`.
    fn expand_owners_bfs(
        &mut self,
        start_owners_rel: &Path,
        abs_fuchsia_dir: &Path,
    ) -> (Vec<(String, u32, String)>, bool) {
        let mut results = Vec::new();
        let mut visited_files = HashSet::new();
        let mut queue = VecDeque::new();
        let mut start_noparent = false;

        let clean_start = normalize_rel_path(start_owners_rel);
        visited_files.insert(clean_start.clone());
        queue.push_back((clean_start, 0u32));

        while let Some((curr_rel, include_depth)) = queue.pop_front() {
            let Some(parsed) = self.get_or_load_owners_file(&curr_rel, abs_fuchsia_dir) else {
                continue;
            };
            if include_depth == 0 {
                start_noparent = parsed.noparent;
            }
            let owners_file_str = curr_rel.to_string_lossy().into_owned();
            for email in &parsed.direct_owners {
                results.push((email.clone(), include_depth, owners_file_str.clone()));
            }
            for inc_path in &parsed.includes {
                let clean_inc = normalize_rel_path(inc_path);
                if visited_files.insert(clean_inc.clone()) {
                    queue.push_back((clean_inc, include_depth + 1));
                }
            }
        }

        (results, start_noparent)
    }
}

/// Computes the number of directory path components separating `dir` from its ancestor directory
/// `ancestor` (`0` if `dir == ancestor`).
fn hop_distance(dir: &Path, ancestor: &Path) -> u32 {
    dir.strip_prefix(ancestor).map(|rel| rel.components().count() as u32).unwrap_or(0)
}

/// Lexically normalizes a repository-relative path by stripping `.` and root prefixes and
/// resolving `..` components without touching the filesystem.
fn normalize_rel_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(c) => {
                normalized.push(c);
            }
        }
    }
    normalized
}

/// Extracts the target path from an `include <path>` or `file:<path>` directive line in an
/// `OWNERS` file, or returns `None` if `line` is not an include directive.
fn parse_include_target(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("include ").or_else(|| line.strip_prefix("file:"))?.trim();
    (!rest.is_empty() && !rest.contains(char::is_whitespace)).then_some(rest)
}

/// Resolves an `include` or `file:` target string (which may be root-relative starting with `/` or
/// `//`, or relative to `current_owners_rel`) into a normalized repository-relative [`PathBuf`].
fn resolve_include_path(raw_target: &str, current_owners_rel: &Path) -> PathBuf {
    let target = raw_target.split(':').next_back().unwrap_or(raw_target).trim();
    if target.starts_with('/') {
        normalize_rel_path(Path::new(target.trim_start_matches('/')))
    } else {
        let base_dir = current_owners_rel.parent().unwrap_or_else(|| Path::new(""));
        normalize_rel_path(&base_dir.join(target))
    }
}

/// Reads and parses an `OWNERS` file at `abs_path` (with repository-relative path `rel_path`),
/// extracting direct owner emails, `include` / `file:` targets, and `set noparent` while ignoring
/// comments, wildcards (`*`), and `per-file` rules.
fn parse_owners_file(abs_path: &Path, rel_path: &Path) -> Result<ParsedOwnersFile> {
    let content =
        fs::read_to_string(abs_path).with_context(|| format!("failed to read {:?}", abs_path))?;
    let mut parsed = ParsedOwnersFile::default();

    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with("per-file ") || line.starts_with("per-file\t") {
            continue;
        }
        if line == "set noparent" {
            parsed.noparent = true;
        } else if let Some(target) = parse_include_target(line) {
            parsed.includes.push(resolve_include_path(target, rel_path));
        } else if !line.contains(char::is_whitespace) && line.contains('@') && line != "*" {
            parsed.direct_owners.push(line.to_string());
        }
    }

    Ok(parsed)
}

/// Extracts the first positive integer `component_id` from a `METADATA.textproto` file at `path`.
fn parse_metadata_component_id(path: &Path) -> Option<i64> {
    let content = fs::read_to_string(path).ok()?;
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("");
        if let Some((_, after)) = line.split_once("component_id:") {
            let digits: String =
                after.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<i64>()
                && id > 0
            {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use valico::json_schema;

    const OWNERSHIP_OUTPUT_SCHEMA: &str = include_str!("../directory_owners.schema.json");

    /// Walks `root` with an [`OwnershipCollector`] and resolves ownership for `dirs`.
    fn run_collector(root: &Path, dirs: &[&str]) -> (DirectoryOwnersOutput, Vec<PathBuf>) {
        let mut collector = OwnershipCollector::default();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(root).unwrap();
                collector.visit_file(entry.path(), rel);
            }
        }
        let dir_bufs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        collector.finish(&dir_bufs, root)
    }

    /// Protects against: DFS/BFS ordering bugs where included OWNERS rank ahead of direct owners,
    /// gaps in `owners_step` when duplicate emails appear across files, and incorrect
    /// `dir_distance` calculation for leaf subdirectories.
    #[test]
    fn test_proximity_tiers_and_deduplication() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        fs::create_dir_all(root.join("src/sub"))?;
        fs::create_dir_all(root.join("src/sys/pkg/tests"))?;
        fs::write(root.join("src/OWNERS"), "umbrella@google.com\nleaf@google.com\n")?;
        fs::write(root.join("src/sub/OWNERS"), "include /src/OWNERS\nsub@google.com\n")?;
        fs::write(root.join("src/sys/OWNERS"), "include /src/sub/OWNERS\nleaf@google.com\n")?;

        let (output, _) = run_collector(root, &["src/sys", "src/sys/pkg/tests"]);
        let sys_owners = &output.directories.get("src/sys").unwrap().owners;
        let sys_summary: Vec<(&str, u32, u32, u32, u32)> = sys_owners
            .iter()
            .map(|o| (o.email.as_str(), o.tier, o.owners_step, o.include_depth, o.dir_distance))
            .collect();
        assert_eq!(
            sys_summary,
            vec![
                ("leaf@google.com", 1, 1, 0, 0),
                ("sub@google.com", 2, 1, 1, 0),
                ("umbrella@google.com", 3, 1, 2, 0),
            ]
        );

        let leaf_owners = &output.directories.get("src/sys/pkg/tests").unwrap().owners;
        let leaf_summary: Vec<(&str, u32, u32, u32, u32)> = leaf_owners
            .iter()
            .map(|o| (o.email.as_str(), o.tier, o.owners_step, o.include_depth, o.dir_distance))
            .collect();
        assert_eq!(
            leaf_summary,
            vec![
                ("leaf@google.com", 1, 1, 0, 2),
                ("sub@google.com", 2, 1, 1, 2),
                ("umbrella@google.com", 3, 1, 2, 2),
            ]
        );
        Ok(())
    }

    /// Protects against: inheriting global approvers from `//OWNERS` during parent traversal while
    /// still honoring explicit `include /OWNERS` directives in a subdirectory.
    #[test]
    fn test_root_owners_excluded_unless_explicitly_included() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        fs::create_dir_all(root.join("implicit_sub"))?;
        fs::create_dir_all(root.join("explicit_sub"))?;
        fs::write(root.join("OWNERS"), "global@google.com\n")?;
        fs::write(root.join("explicit_sub/OWNERS"), "include /OWNERS\n")?;

        let (output, _) = run_collector(root, &["implicit_sub", "explicit_sub"]);
        assert!(output.directories["implicit_sub"].owners.is_empty());
        assert_eq!(output.directories["explicit_sub"].owners.len(), 1);
        assert_eq!(output.directories["explicit_sub"].owners[0].email, "global@google.com");
        Ok(())
    }

    /// Protects against: leaking parent `OWNERS` past `set noparent`, and accidentally aborting
    /// `METADATA.textproto` `component_id` inheritance when a child directory sets `noparent`.
    #[test]
    fn test_noparent_stops_owners_without_blocking_component_id() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        fs::create_dir_all(root.join("area/subcomponent"))?;
        fs::write(root.join("area/OWNERS"), "umbrella@google.com\n")?;
        fs::write(
            root.join("area/METADATA.textproto"),
            "trackers: { issue_tracker: { component_id: 1478143 } }\n",
        )?;
        fs::write(root.join("area/subcomponent/OWNERS"), "set noparent\nlocal@google.com\n")?;

        let (output, _) = run_collector(root, &["area/subcomponent"]);
        let entry = &output.directories["area/subcomponent"];
        assert_eq!(entry.component_id, Some(1478143));
        assert_eq!(entry.owners.len(), 1);
        assert_eq!(entry.owners[0].email, "local@google.com");

        // An OWNERS file containing only `per-file` rules should fall through to its parent
        // `OWNERS`, unless `set noparent` is also present (which yields empty `owners`).
        fs::create_dir_all(root.join("area/perfile_only"))?;
        fs::write(root.join("area/perfile_only/OWNERS"), "per-file *.gni = bob@google.com\n")?;
        fs::create_dir_all(root.join("area/noparent_empty"))?;
        fs::write(
            root.join("area/noparent_empty/OWNERS"),
            "set noparent\nper-file *.gni = bob@google.com\n",
        )?;

        let (output, _) = run_collector(root, &["area/perfile_only", "area/noparent_empty"]);
        assert_eq!(output.directories["area/perfile_only"].owners.len(), 1);
        assert_eq!(output.directories["area/perfile_only"].owners[0].email, "umbrella@google.com");
        assert_eq!(output.directories["area/perfile_only"].owners[0].dir_distance, 1);
        assert!(output.directories["area/noparent_empty"].owners.is_empty());
        Ok(())
    }

    /// Protects against: omitting custom-named included files (`file:/src/OWNERS_EXTRA`) from
    /// Ninja depfile inputs (which breaks Fuchsia's hermetic `action_tracer.py`) and schema drift.
    #[test]
    fn test_custom_named_includes_tracked_in_deps_and_schema_valid() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/OWNERS_EXTRA"), "extra@google.com\n")?;
        fs::write(root.join("src/OWNERS"), "file://src/OWNERS_EXTRA\n")?;

        let (output, deps) = run_collector(root, &["src"]);
        assert!(deps.contains(&PathBuf::from("src/OWNERS_EXTRA")));
        assert_eq!(output.directories["src"].owners.len(), 1);
        assert_eq!(output.directories["src"].owners[0].email, "extra@google.com");

        let mut scope = json_schema::Scope::new();
        let schema_val: serde_json::Value = serde_json::from_str(OWNERSHIP_OUTPUT_SCHEMA)?;
        let schema = scope
            .compile_and_return(schema_val, false)
            .map_err(|e| anyhow::anyhow!("invalid schema: {:?}", e))?;
        let serialized = serde_json::to_value(&output)?;
        assert!(schema.validate(&serialized).is_valid());
        Ok(())
    }
}
