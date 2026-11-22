// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::git::GitRepo;

/// Path component marking an `fx-worktree` checkout inside a parent Jiri root.
const WORKTREES_MARKER: &str = ".jiri_root/worktrees/";

/// Location of the `jiri` binary relative to a Jiri root.
const JIRI_BIN_REL: &str = ".jiri_root/bin/jiri";

/// `jiri project` template producing tab-separated `name`, `path`, and `remote` columns (paths
/// may contain spaces).
const PROJECT_TEMPLATE: &str = "{{.Name}}\t{{.Path}}\t{{.Remote}}";

/// Metadata for a project managed by Jiri.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiriProject {
    pub name: String,
    pub path: PathBuf,
    pub remote: String,
}

/// Lookup map for all Jiri projects in the workspace.
#[derive(Debug, Clone)]
pub struct JiriProjectMap {
    pub projects: Vec<JiriProject>,
    pub fuchsia_dir: PathBuf,
}

/// If `fuchsia_dir` is an `fx-worktree` (`<root>/.jiri_root/worktrees/<name>`), returns `<root>`.
fn worktree_parent_root(fuchsia_dir: &Path) -> Option<PathBuf> {
    let s = fuchsia_dir.to_str()?;
    let idx = s.find(WORKTREES_MARKER)?;
    let root = s[..idx].trim_end_matches('/');
    if root.is_empty() { None } else { Some(PathBuf::from(root)) }
}

/// Rebases a project path reported relative to a parent checkout onto the worktree, if the
/// corresponding path exists in the worktree.
fn rebase_onto_worktree(path: &Path, fuchsia_dir: &Path, parent_root: Option<&Path>) -> PathBuf {
    let Some(root) = parent_root else {
        return path.to_path_buf();
    };
    if path.starts_with(fuchsia_dir) {
        return path.to_path_buf();
    }
    match path.strip_prefix(root) {
        Ok(rel) => {
            let candidate = fuchsia_dir.join(rel);
            if candidate.exists() { candidate } else { path.to_path_buf() }
        }
        Err(_) => path.to_path_buf(),
    }
}

/// Converts a Gerrit review host (e.g. `fuchsia-review.googlesource.com`) to the name of its
/// git host (e.g. `fuchsia`).
fn git_host_name(review_host: &str) -> Option<String> {
    let first_label = review_host.split('.').next()?;
    let name = first_label.strip_suffix("-review").unwrap_or(first_label);
    if name.is_empty() { None } else { Some(name.to_string()) }
}

/// Returns true if `remote` is served by the git host named `host_name`.
fn remote_matches_host(remote: &str, host_name: &str) -> bool {
    remote.contains(&format!("://{host_name}.googlesource.com/"))
        || remote.contains(&format!("://{host_name}.git.corp.google.com/"))
        || remote.contains(&format!("sso://{host_name}/"))
}

/// Git hosts for which projects explicitly served by a different known host are never
/// considered a match.
const STRICT_HOSTS: &[&str] = &["fuchsia", "turquoise-internal", "partner-fuchsia"];

/// Returns the name of the git host serving `remote` if it is a well-known host
/// (`*.googlesource.com`, `*.git.corp.google.com`, or `sso://*`).
fn remote_host_name(remote: &str) -> Option<&str> {
    let name = if let Some(rest) = remote.strip_prefix("sso://") {
        rest.split('/').next()?
    } else {
        let (_, rest) = remote.split_once("://")?;
        let authority = rest.split('/').next()?;
        let name = authority
            .strip_suffix(".googlesource.com")
            .or_else(|| authority.strip_suffix(".git.corp.google.com"))?;
        // Review hosts (e.g. `fuchsia-review.googlesource.com`) serve the same repositories.
        name.strip_suffix("-review").unwrap_or(name)
    };
    if name.is_empty() || name.contains('.') { None } else { Some(name) }
}

/// Returns true if `remote` points at a repository named `project`.
fn remote_matches_project(remote: &str, project: &str) -> bool {
    let normalized_remote = remote.trim_end_matches('/').trim_end_matches(".git");
    normalized_remote.ends_with(&format!("/{project}"))
        || normalized_remote.ends_with(&format!(":{project}"))
}

/// Returns the `jiri` binary to run: `<fuchsia_dir>/.jiri_root/bin/jiri`, then
/// `<parent_root>/.jiri_root/bin/jiri` (for `fx-worktree` checkouts), then `jiri` from `$PATH`.
fn find_jiri_binary(fuchsia_dir: &Path, parent_root: Option<&Path>) -> PathBuf {
    [Some(fuchsia_dir), parent_root]
        .into_iter()
        .flatten()
        .map(|root| root.join(JIRI_BIN_REL))
        .find(|bin| bin.is_file())
        .unwrap_or_else(|| PathBuf::from("jiri"))
}

/// Parses `jiri project -template PROJECT_TEMPLATE` output, rebasing project paths onto the
/// worktree when `fuchsia_dir` is an `fx-worktree` of `parent_root`.
fn parse_project_listing(
    stdout: &str,
    fuchsia_dir: &Path,
    parent_root: Option<&Path>,
) -> Vec<JiriProject> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut cols = line.trim_end_matches('\r').splitn(3, '\t');
            let (name, path, remote) = (cols.next()?, cols.next()?, cols.next()?);
            let (name, path) = (name.trim(), path.trim());
            if name.is_empty() || path.is_empty() {
                return None;
            }
            Some(JiriProject {
                name: name.to_string(),
                path: rebase_onto_worktree(Path::new(path), fuchsia_dir, parent_root),
                remote: remote.trim().to_string(),
            })
        })
        .collect()
}

impl JiriProjectMap {
    /// Loads the current Jiri project layout using `jiri project`.
    ///
    /// `fuchsia_dir` is canonicalized (like the paths `jiri project` reports), and failures to
    /// run `jiri` are returned as errors.
    pub fn load(fuchsia_dir: &Path) -> Result<Self> {
        let canonical = std::fs::canonicalize(fuchsia_dir).with_context(|| {
            format!("Failed to resolve Fuchsia checkout directory {fuchsia_dir:?}")
        })?;
        // `fuchsia_dir` itself may be a symlink to a worktree; the parent root must be canonical
        // either way to match the paths reported by `jiri project`.
        let parent_root = worktree_parent_root(&canonical).or_else(|| {
            worktree_parent_root(fuchsia_dir).and_then(|p| std::fs::canonicalize(p).ok())
        });
        let jiri = find_jiri_binary(&canonical, parent_root.as_deref());
        let output = Command::new(&jiri)
            .current_dir(&canonical)
            .args(["project", "-template", PROJECT_TEMPLATE])
            .stdin(Stdio::null())
            .output()
            .with_context(|| {
                format!("Failed to execute `{} project` in {canonical:?}", jiri.display())
            })?;
        if !output.status.success() {
            return Err(anyhow!(
                "`{} project` failed in {:?} (status {}): {}",
                jiri.display(),
                canonical,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let projects = parse_project_listing(
            &String::from_utf8_lossy(&output.stdout),
            &canonical,
            parent_root.as_deref(),
        );
        Ok(Self { projects, fuchsia_dir: canonical })
    }

    /// Finds the repository path on disk for a given Gerrit project name and host.
    ///
    /// Only repositories inside `fuchsia_dir` are returned, and a repository known (from the
    /// Jiri manifest or its `origin` URL) to be served by a different well-known host than
    /// `host` is never returned.
    pub fn resolve_project_path(&self, project_name: &str, host: &str) -> Option<PathBuf> {
        if project_name.contains("..")
            || project_name.contains('\\')
            || project_name.starts_with('-')
        {
            return None;
        }
        let clean_proj = project_name.trim_matches('/');
        if clean_proj.is_empty() {
            return None;
        }

        let host_name = git_host_name(host);
        let strict_host = host_name.as_deref().filter(|h| STRICT_HOSTS.contains(h));
        let on_host = |p: &JiriProject| {
            host_name.as_deref().is_some_and(|h| remote_matches_host(&p.remote, h))
        };
        // Projects explicitly served by a different well-known host must never be used for a
        // change on a strict host (e.g. public `integration` vs. the internal
        // `smart-integration` checked out at `./integration`).
        let cross_host = |p: &JiriProject| {
            strict_host.is_some_and(|h| {
                remote_host_name(&p.remote).is_some_and(|remote_host| remote_host != h)
            })
        };
        let usable = |p: &JiriProject| !cross_host(p) && p.path.starts_with(&self.fuchsia_dir);

        // Try exact match on project name, preferring remotes on the same host.
        let name_matches: Vec<&JiriProject> =
            self.projects.iter().filter(|p| p.name == clean_proj && usable(p)).collect();
        if let Some(p) = name_matches.iter().find(|p| on_host(p)).or_else(|| name_matches.first()) {
            return Some(p.path.clone());
        }

        // Try match on remote URL ending with /<project_name>, preferring the same host.
        let remote_matches: Vec<&JiriProject> = self
            .projects
            .iter()
            .filter(|p| remote_matches_project(&p.remote, clean_proj) && usable(p))
            .collect();
        if let Some(p) =
            remote_matches.iter().find(|p| on_host(p)).or_else(|| remote_matches.first())
        {
            return Some(p.path.clone());
        }

        // Fall back to the checkout root for `fuchsia`, or a relative directory under
        // FUCHSIA_DIR (refusing anything but plain components).
        let is_root = matches!(clean_proj, "fuchsia" | "fuchsia/fuchsia");
        let candidate = if is_root {
            self.fuchsia_dir.clone()
        } else {
            let rel = Path::new(clean_proj);
            if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
                return None;
            }
            self.fuchsia_dir.join(rel)
        };
        let has_git = candidate.join(".git").exists();
        // `jiri project` reports canonical paths, and a symlink must not lead out of the
        // checkout.
        let candidate = if has_git && !is_root {
            let canonical = std::fs::canonicalize(&candidate).ok()?;
            let root = std::fs::canonicalize(&self.fuchsia_dir)
                .unwrap_or_else(|_| self.fuchsia_dir.clone());
            if !canonical.starts_with(&root) {
                return None;
            }
            canonical
        } else {
            candidate
        };
        let known: Vec<&JiriProject> = self
            .projects
            .iter()
            .filter(|p| {
                p.path == candidate || std::fs::canonicalize(&p.path).is_ok_and(|c| c == candidate)
            })
            .collect();
        // Do not fall back to a directory known to be a checkout from a different host.
        if known.iter().any(|p| cross_host(p)) {
            return None;
        }
        // A checkout unknown to the manifest is checked against its own `origin` URL. Checkouts
        // without a recognizable `origin` (e.g. offline or test checkouts) are accepted.
        if known.is_empty()
            && has_git
            && let Some(h) = strict_host
            && let Ok(url) =
                GitRepo::new(&candidate).run_git(&["config", "--get", "remote.origin.url"])
            && remote_host_name(url.trim()).is_some_and(|remote_host| remote_host != h)
        {
            return None;
        }
        if is_root || has_git { Some(candidate) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_util::{init_repo, require_git, wait_until_executable, write_executable};
    use assert_matches::assert_matches;
    use tempfile::tempdir;

    const FUCHSIA_HOST: &str = "fuchsia-review.googlesource.com";
    const INTERNAL_HOST: &str = "turquoise-internal-review.googlesource.com";

    fn project(name: &str, path: &str, remote: &str) -> JiriProject {
        JiriProject { name: name.into(), path: PathBuf::from(path), remote: remote.into() }
    }

    fn test_map(fuchsia_dir: &Path) -> JiriProjectMap {
        JiriProjectMap {
            projects: vec![
                project(
                    "vendor/google",
                    "/src/vendor/google",
                    "sso://turquoise-internal/vendor/google",
                ),
                project(
                    "cobalt",
                    "/src/third_party/cobalt",
                    "https://fuchsia.googlesource.com/cobalt",
                ),
                project(
                    "cobalt-internal",
                    "/src/vendor/cobalt",
                    "https://turquoise-internal.googlesource.com/cobalt",
                ),
                project("dup", "/src/dup-public", "https://fuchsia.googlesource.com/dup"),
                project("dup", "/src/dup-internal", "sso://turquoise-internal/dup"),
            ],
            fuchsia_dir: fuchsia_dir.to_path_buf(),
        }
    }

    #[fuchsia::test]
    fn test_resolve_fuchsia_root() {
        let map = test_map(Path::new("/src"));
        assert_eq!(map.resolve_project_path("fuchsia", FUCHSIA_HOST), Some(PathBuf::from("/src")));
    }

    #[fuchsia::test]
    fn test_resolve_exact_match() {
        let map = test_map(Path::new("/src"));
        assert_eq!(
            map.resolve_project_path("vendor/google", INTERNAL_HOST),
            Some(PathBuf::from("/src/vendor/google"))
        );
        assert_eq!(
            map.resolve_project_path("/vendor/google/", INTERNAL_HOST),
            Some(PathBuf::from("/src/vendor/google"))
        );
        // Duplicate names prefer the project hosted on the same Gerrit host.
        assert_eq!(
            map.resolve_project_path("dup", INTERNAL_HOST),
            Some(PathBuf::from("/src/dup-internal"))
        );
        assert_eq!(
            map.resolve_project_path("dup", FUCHSIA_HOST),
            Some(PathBuf::from("/src/dup-public"))
        );
    }

    #[fuchsia::test]
    fn test_resolve_remote_host_match() {
        let map = test_map(Path::new("/src"));
        // "cobalt" matches by exact name first.
        assert_eq!(
            map.resolve_project_path("cobalt", FUCHSIA_HOST),
            Some(PathBuf::from("/src/third_party/cobalt"))
        );
        let map = JiriProjectMap {
            projects: vec![
                project("a", "/src/public", "https://fuchsia.googlesource.com/shared"),
                project("b", "/src/internal", "https://turquoise-internal.googlesource.com/shared"),
            ],
            fuchsia_dir: PathBuf::from("/src"),
        };
        assert_eq!(
            map.resolve_project_path("shared", INTERNAL_HOST),
            Some(PathBuf::from("/src/internal"))
        );
        assert_eq!(
            map.resolve_project_path("shared", FUCHSIA_HOST),
            Some(PathBuf::from("/src/public"))
        );
        // Unknown host falls back to any matching remote.
        assert_eq!(
            map.resolve_project_path("shared", "other-review.googlesource.com"),
            Some(PathBuf::from("/src/public"))
        );
    }

    #[fuchsia::test]
    fn test_resolve_rejects_cross_host_projects() {
        let tmp = tempdir().unwrap();
        let integration = tmp.path().join("integration");
        std::fs::create_dir_all(integration.join(".git")).unwrap();
        let map = JiriProjectMap {
            projects: vec![project(
                "integration",
                integration.to_str().unwrap(),
                "sso://turquoise-internal/smart-integration",
            )],
            fuchsia_dir: tmp.path().to_path_buf(),
        };
        // The public `integration` repo is not checked out locally; the internal
        // `smart-integration` checkout at `./integration` must not be used, whether matched by
        // name or by the relative directory fallback.
        assert_eq!(map.resolve_project_path("integration", FUCHSIA_HOST), None);
        // The same checkout still resolves for changes on its own host.
        assert_eq!(
            map.resolve_project_path("integration", INTERNAL_HOST),
            Some(integration.clone())
        );
        assert_eq!(
            map.resolve_project_path("smart-integration", INTERNAL_HOST),
            Some(integration.clone())
        );
        assert_eq!(map.resolve_project_path("smart-integration", FUCHSIA_HOST), None);
        // Unknown hosts keep the permissive fallback.
        assert_eq!(
            map.resolve_project_path("integration", "other-review.googlesource.com"),
            Some(integration)
        );
    }

    #[fuchsia::test]
    fn test_remote_host_name() {
        assert_eq!(
            remote_host_name("https://fuchsia.googlesource.com/integration"),
            Some("fuchsia")
        );
        assert_eq!(
            remote_host_name("https://turquoise-internal.git.corp.google.com/x"),
            Some("turquoise-internal")
        );
        assert_eq!(
            remote_host_name("sso://turquoise-internal/smart-integration"),
            Some("turquoise-internal")
        );
        assert_eq!(
            remote_host_name("https://fuchsia-review.googlesource.com/fuchsia"),
            Some("fuchsia")
        );
        assert_eq!(remote_host_name("https://github.com/foo/bar"), None);
        assert_eq!(remote_host_name("/local/path"), None);
    }

    #[fuchsia::test]
    fn test_resolve_relative_directory_fallback() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("third_party/foo/.git")).unwrap();
        let map = JiriProjectMap { projects: Vec::new(), fuchsia_dir: tmp.path().to_path_buf() };
        assert_eq!(
            map.resolve_project_path("third_party/foo", FUCHSIA_HOST),
            Some(tmp.path().join("third_party/foo"))
        );
        assert_eq!(map.resolve_project_path("third_party/bar", FUCHSIA_HOST), None);
    }

    #[fuchsia::test]
    fn test_resolve_rejects_path_traversal() {
        let tmp = tempdir().unwrap();
        let inner = tmp.path().join("checkout");
        std::fs::create_dir_all(inner.join("sub")).unwrap();
        std::fs::create_dir_all(tmp.path().join("escape/.git")).unwrap();
        let map = JiriProjectMap { projects: Vec::new(), fuchsia_dir: inner };
        assert_eq!(map.resolve_project_path("../escape", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("sub/../../escape", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("..\\escape", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("-escape", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("./escape", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("", FUCHSIA_HOST), None);
    }

    #[fuchsia::test]
    fn test_worktree_path_rebasing() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("fuchsia");
        let worktree = root.join(".jiri_root/worktrees/wt");
        std::fs::create_dir_all(worktree.join("vendor/google")).unwrap();

        assert_eq!(worktree_parent_root(&worktree), Some(root.clone()));
        assert_eq!(worktree_parent_root(&root), None);

        // Paths under the parent checkout are rebased onto the worktree when present.
        assert_eq!(
            rebase_onto_worktree(&root.join("vendor/google"), &worktree, Some(&root)),
            worktree.join("vendor/google")
        );
        // Paths that do not exist in the worktree are left untouched.
        assert_eq!(
            rebase_onto_worktree(&root.join("third_party/x"), &worktree, Some(&root)),
            root.join("third_party/x")
        );
        // Paths already inside the worktree are left untouched.
        assert_eq!(
            rebase_onto_worktree(&worktree.join("vendor/google"), &worktree, Some(&root)),
            worktree.join("vendor/google")
        );
        // Non-worktree checkouts are left untouched.
        assert_eq!(
            rebase_onto_worktree(&root.join("vendor/google"), &root, None),
            root.join("vendor/google")
        );
    }

    /// Installs a `jiri` stub in `root` that prints `listing`.
    fn write_jiri_stub(root: &Path, listing: &str) {
        let jiri = root.join(JIRI_BIN_REL);
        write_executable(&jiri, &format!("#!/bin/sh\ncat <<'EOF'\n{listing}EOF\n"));
        wait_until_executable(&jiri);
    }

    #[fuchsia::test]
    fn test_load_parses_tab_separated_output() {
        let tmp = tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let proj = root.join("with space/proj");
        write_jiri_stub(
            &root,
            &format!(
                "with space\t{}\thttps://fuchsia.googlesource.com/with-space\njunk\tline\n\nnoremote\t{}\t\n",
                proj.display(),
                root.join("nr").display()
            ),
        );
        let map = JiriProjectMap::load(&root).unwrap();
        assert_eq!(map.fuchsia_dir, root);
        assert_eq!(
            map.projects,
            vec![
                project(
                    "with space",
                    proj.to_str().unwrap(),
                    "https://fuchsia.googlesource.com/with-space"
                ),
                project("noremote", root.join("nr").to_str().unwrap(), ""),
            ]
        );
    }

    #[fuchsia::test]
    fn test_load_propagates_failures() {
        let tmp = tempdir().unwrap();
        let failing = tmp.path().join("failing");
        write_executable(&failing.join(JIRI_BIN_REL), "#!/bin/sh\necho broken >&2\nexit 3\n");
        wait_until_executable(&failing.join(JIRI_BIN_REL));
        assert_matches!(
            JiriProjectMap::load(&failing),
            Err(e) if format!("{e:#}").contains("status") && format!("{e:#}").contains("broken")
        );

        let not_executable = tmp.path().join("not_executable");
        std::fs::create_dir_all(not_executable.join(".jiri_root/bin")).unwrap();
        std::fs::write(not_executable.join(JIRI_BIN_REL), "#!/bin/sh\nexit 0\n").unwrap();
        assert_matches!(
            JiriProjectMap::load(&not_executable),
            Err(e) if format!("{e:#}").contains("Failed to execute")
        );

        assert_matches!(
            JiriProjectMap::load(&tmp.path().join("does-not-exist")),
            Err(e) if format!("{e:#}").contains("Failed to resolve")
        );
    }

    #[fuchsia::test]
    fn test_load_canonicalizes_symlinked_fuchsia_dir() {
        let tmp = tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("real");
        write_jiri_stub(&real, "");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let map = JiriProjectMap::load(&link).unwrap();
        assert_eq!(map.fuchsia_dir, real);
        assert_eq!(map.resolve_project_path("fuchsia", FUCHSIA_HOST), Some(real.clone()));
    }

    #[fuchsia::test]
    fn test_load_rebases_symlinked_worktree_paths() {
        let tmp = tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let real_root = base.join("real/fuchsia");
        let worktree = real_root.join(".jiri_root/worktrees/wt");
        std::fs::create_dir_all(worktree.join("subproj/.git")).unwrap();
        // Only the parent checkout has a `jiri` binary; it reports parent checkout paths.
        write_jiri_stub(
            &real_root,
            &format!(
                "subproj\t{}\thttps://fuchsia.googlesource.com/subproj\n\
                 missing\t{}\thttps://fuchsia.googlesource.com/missing\n",
                real_root.join("subproj").display(),
                real_root.join("missing").display()
            ),
        );
        std::fs::create_dir_all(real_root.join("missing/.git")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(base.join("real"), &link).unwrap();

        // Through a symlinked parent directory (`~/fuchsia -> /real/fuchsia`), and through a
        // symlink to the worktree itself.
        let wt_link = base.join("wt-link");
        std::os::unix::fs::symlink(&worktree, &wt_link).unwrap();
        for dir in [link.join("fuchsia/.jiri_root/worktrees/wt"), wt_link] {
            let map = JiriProjectMap::load(&dir).unwrap();
            assert_eq!(map.fuchsia_dir, worktree, "{dir:?}");
            assert_eq!(
                map.resolve_project_path("subproj", FUCHSIA_HOST),
                Some(worktree.join("subproj")),
                "{dir:?}"
            );
            // Projects absent from the worktree never resolve to the parent checkout.
            assert_eq!(map.resolve_project_path("missing", FUCHSIA_HOST), None, "{dir:?}");
        }

        // A worktree that is itself a symlink out of `.jiri_root/worktrees/`: the parent root is
        // derived from the (symlinked) path that was passed in, then canonicalized.
        let outside = base.join("elsewhere/wt2");
        std::fs::create_dir_all(outside.join("subproj/.git")).unwrap();
        std::os::unix::fs::symlink(&outside, real_root.join(".jiri_root/worktrees/wt2")).unwrap();
        let map = JiriProjectMap::load(&link.join("fuchsia/.jiri_root/worktrees/wt2")).unwrap();
        assert_eq!(map.fuchsia_dir, outside);
        assert_eq!(
            map.resolve_project_path("subproj", FUCHSIA_HOST),
            Some(outside.join("subproj"))
        );
    }

    #[fuchsia::test]
    fn test_resolve_rejects_paths_outside_checkout() {
        let tmp = tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(parent.join("vendor/google/.git")).unwrap();
        let map = JiriProjectMap {
            projects: vec![project(
                "vendor/google",
                parent.join("vendor/google").to_str().unwrap(),
                "sso://turquoise-internal/vendor/google",
            )],
            fuchsia_dir: tmp.path().join("wt"),
        };
        assert_eq!(map.resolve_project_path("vendor/google", INTERNAL_HOST), None);
        assert_eq!(map.resolve_project_path("google", INTERNAL_HOST), None);
    }

    #[fuchsia::test]
    fn test_resolve_fuchsia_root_respects_host() {
        let map = JiriProjectMap {
            projects: vec![project("fuchsia", "/src", "sso://turquoise-internal/fuchsia")],
            fuchsia_dir: PathBuf::from("/src"),
        };
        assert_eq!(map.resolve_project_path("fuchsia", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("fuchsia/fuchsia", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("fuchsia", INTERNAL_HOST), Some(PathBuf::from("/src")));
    }

    #[fuchsia::test]
    fn test_resolve_fuchsia_root_checks_origin_url() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let map = JiriProjectMap { projects: Vec::new(), fuchsia_dir: tmp.path().to_path_buf() };
        // Without an `origin`, the checkout is accepted for any host.
        assert_eq!(map.resolve_project_path("fuchsia", FUCHSIA_HOST), Some(tmp.path().into()));
        assert_eq!(map.resolve_project_path("fuchsia", INTERNAL_HOST), Some(tmp.path().into()));

        repo.run_git(&["remote", "add", "origin", "https://fuchsia.googlesource.com/fuchsia"])
            .unwrap();
        assert_eq!(map.resolve_project_path("fuchsia", INTERNAL_HOST), None);
        assert_eq!(map.resolve_project_path("fuchsia", FUCHSIA_HOST), Some(tmp.path().into()));
        assert_eq!(
            map.resolve_project_path("fuchsia", "other-review.googlesource.com"),
            Some(tmp.path().into())
        );
    }

    #[fuchsia::test]
    fn test_partner_fuchsia_is_strict() {
        let tmp = tempdir().unwrap();
        let x = tmp.path().join("x");
        std::fs::create_dir_all(x.join(".git")).unwrap();
        let map = JiriProjectMap {
            projects: vec![project("x", x.to_str().unwrap(), "https://fuchsia.googlesource.com/x")],
            fuchsia_dir: tmp.path().to_path_buf(),
        };
        assert_eq!(map.resolve_project_path("x", "partner-fuchsia-review.googlesource.com"), None);
        assert_eq!(map.resolve_project_path("x", FUCHSIA_HOST), Some(x));
    }

    #[fuchsia::test]
    fn test_find_jiri_binary() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        let wt = root.join(".jiri_root/worktrees/wt");
        std::fs::create_dir_all(&wt).unwrap();
        assert_eq!(find_jiri_binary(&wt, Some(&root)), PathBuf::from("jiri"));
        write_jiri_stub(&root, "");
        assert_eq!(find_jiri_binary(&wt, Some(&root)), root.join(JIRI_BIN_REL));
        write_jiri_stub(&wt, "");
        assert_eq!(find_jiri_binary(&wt, Some(&root)), wt.join(JIRI_BIN_REL));
    }

    #[fuchsia::test]
    fn test_resolve_fallback_follows_symlinks_inside_checkout() {
        let tmp = tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let real = root.join("smart_integration");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        std::os::unix::fs::symlink(&real, root.join("integration")).unwrap();
        let map = JiriProjectMap {
            projects: vec![project(
                "smart-integration",
                real.to_str().unwrap(),
                "sso://turquoise-internal/smart-integration",
            )],
            fuchsia_dir: root.clone(),
        };
        // `./integration` is a symlink to a checkout known to be served by another host.
        assert_eq!(map.resolve_project_path("integration", FUCHSIA_HOST), None);
        assert_eq!(map.resolve_project_path("integration", INTERNAL_HOST), Some(real));
    }

    #[fuchsia::test]
    fn test_resolve_fallback_rejects_symlinks_out_of_checkout() {
        let tmp = tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(tmp.path().join("outside/.git")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside"), checkout.join("escape")).unwrap();
        let map = JiriProjectMap { projects: Vec::new(), fuchsia_dir: checkout };
        assert_eq!(map.resolve_project_path("escape", FUCHSIA_HOST), None);
    }
}
