// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Name of the git remote queried for up-to-date branch tips.
const DEFAULT_REMOTE: &str = "origin";

/// Branch used when a Gerrit change does not report one.
const DEFAULT_BRANCH: &str = "main";

/// Local refs tracking the upstream `main` branch, in order of preference for
/// `GitRepo::base_ref()`.
const UPSTREAM_TRACKING_REFS: [&str; 2] = ["origin/main", "jiri/head"];

/// Environment variables that would redirect git away from the repository at `GitRepo::path`.
const GIT_ENV_OVERRIDES: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// `git merge-tree -z` message kind reported for file/directory conflicts.
const FILE_DIRECTORY_CONFLICT: &str = "CONFLICT (file/directory)";

/// Wrapper around git command operations for a local repository.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub path: PathBuf,
}

/// Result of an in-memory 3-way merge/revert simulation using `git merge-tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTreeResult {
    Success { tree_sha: String },
    Conflict { unmerged_files: Vec<String>, details: String },
}

/// One `<Informational messages>` record of `git merge-tree -z` output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeTreeMessage {
    /// Paths the message is about (e.g. `[foo~<rev>, foo]` for a file/directory conflict).
    paths: Vec<String>,
    /// Message kind, e.g. `Auto-merging` or `CONFLICT (contents)`.
    kind: String,
    /// Human-readable message, including its trailing newline.
    message: String,
}

/// Parsed stdout of `git merge-tree --write-tree --name-only -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeTreeOutput {
    tree_sha: String,
    /// Exact (unquoted, untrimmed) paths listed in the `<Conflicted file info>` section.
    conflicted_paths: Vec<String>,
    messages: Vec<MergeTreeMessage>,
}

/// Parses NUL-delimited `git merge-tree --write-tree --name-only -z` stdout:
///
/// ```text
/// <tree>NUL [<path>NUL]* NUL [<N>NUL <path_1>NUL ... <path_N>NUL <kind>NUL <message>NUL]*
/// ```
///
/// A clean merge prints only `<tree>NUL`.
fn parse_merge_tree_z(stdout: &[u8]) -> Result<MergeTreeOutput> {
    let mut fields = stdout.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let tree_sha = fields.next().unwrap_or_default();
    if tree_sha.is_empty() {
        return Err(anyhow!("git merge-tree printed no tree SHA"));
    }
    let conflicted_paths: Vec<String> = fields.by_ref().take_while(|f| !f.is_empty()).collect();
    let mut messages = Vec::new();
    while let Some(count) = fields.next() {
        if count.is_empty() {
            break;
        }
        let count: usize = count
            .parse()
            .with_context(|| format!("Invalid path count {count:?} in git merge-tree output"))?;
        let paths: Vec<String> = fields.by_ref().take(count).collect();
        let (Some(kind), Some(message)) = (fields.next(), fields.next()) else {
            return Err(anyhow!("Truncated message record in git merge-tree output"));
        };
        if paths.len() != count {
            return Err(anyhow!("Truncated path list in git merge-tree output"));
        }
        messages.push(MergeTreeMessage { paths, kind, message });
    }
    Ok(MergeTreeOutput { tree_sha, conflicted_paths, messages })
}

/// Returns the sorted, deduplicated list of conflicting files in `output`, mapping the synthetic
/// `<path>~<rev>` entries of file/directory conflicts back to the real `<path>`.
fn unmerged_files(output: &MergeTreeOutput) -> Vec<String> {
    let mut files = output.conflicted_paths.clone();
    files.extend(
        output
            .messages
            .iter()
            .filter(|m| m.kind.starts_with("CONFLICT"))
            .flat_map(|m| m.paths.iter().cloned()),
    );
    for m in output.messages.iter().filter(|m| m.kind == FILE_DIRECTORY_CONFLICT) {
        if let [synthetic, real] = m.paths.as_slice()
            && synthetic.strip_prefix(real.as_str()).is_some_and(|rest| rest.starts_with('~'))
        {
            for f in files.iter_mut().filter(|f| *f == synthetic) {
                *f = real.clone();
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Returns true if `sha` looks like an (abbreviated or full) hexadecimal commit SHA.
pub fn is_valid_commit_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// Returns true if `sha` is a full (40 hex digit) commit SHA.
pub fn is_valid_full_commit_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

fn ensure_valid_sha(sha: &str) -> Result<()> {
    if is_valid_commit_sha(sha) { Ok(()) } else { Err(anyhow!("Invalid commit SHA: {sha:?}")) }
}

/// Returns true if `change_id` is `I` followed by ASCII alphanumerics or `_` (Gerrit uses 40
/// hex digits; tests use short ids such as `I_t`), so it is safe to embed in a regex.
pub fn is_valid_change_id(change_id: &str) -> bool {
    change_id.strip_prefix('I').is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn ensure_valid_change_id(change_id: &str) -> Result<()> {
    if is_valid_change_id(change_id) {
        Ok(())
    } else {
        Err(anyhow!("Invalid Change-Id: {change_id:?}"))
    }
}

/// Rejects refs that could be interpreted as command-line options.
fn ensure_safe_ref(git_ref: &str) -> Result<()> {
    if git_ref.is_empty() || git_ref.starts_with('-') {
        Err(anyhow!("Invalid git ref: {git_ref:?}"))
    } else {
        Ok(())
    }
}

/// Rejects branch names that could be interpreted as options or `ls-remote` patterns.
fn ensure_safe_branch(branch: &str) -> Result<()> {
    ensure_safe_ref(branch)?;
    if branch.contains("..")
        || branch.contains("@{")
        || branch.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '*' | '?' | '[' | '\\' | ':' | '^' | '~')
        })
    {
        return Err(anyhow!("Invalid branch name: {branch:?}"));
    }
    Ok(())
}

/// Returns the short name of `branch`: strips `refs/heads/` and maps an empty branch to
/// `main`.
pub fn normalize_branch(branch: &str) -> &str {
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if branch.is_empty() { DEFAULT_BRANCH } else { branch }
}

/// Local refs that may track the normalized `branch` on `origin`, in order of preference.
fn local_tracking_refs(branch: &str) -> Vec<String> {
    if branch == DEFAULT_BRANCH {
        UPSTREAM_TRACKING_REFS.iter().map(|r| r.to_string()).collect()
    } else {
        vec![format!("refs/remotes/{DEFAULT_REMOTE}/{branch}"), format!("refs/heads/{branch}")]
    }
}

impl GitRepo {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the base `git` command for this repository: it runs in `self.path`, ignores
    /// ambient `GIT_*` overrides (e.g. `$GIT_DIR` inside a git hook), and never runs hooks or
    /// fsmonitor.
    fn git_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.path).args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
        for var in GIT_ENV_OVERRIDES {
            cmd.env_remove(var);
        }
        cmd
    }

    /// Returns a `git_command()` for talking to a remote that can never block on an
    /// interactive credential or SSH prompt, and gives up on stalled connections.
    fn remote_git_command(&self) -> Command {
        let mut cmd = self.git_command();
        cmd.args([
            "-c",
            "core.askPass=",
            "-c",
            "http.connectTimeout=10",
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=30",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=10")
        .stdin(Stdio::null());
        cmd
    }

    /// Returns the default local base ref for branching/simulations (origin/main,
    /// jiri/head, or HEAD). Never touches the network.
    pub fn base_ref(&self) -> &'static str {
        UPSTREAM_TRACKING_REFS.into_iter().find(|r| self.ref_exists(r)).unwrap_or("HEAD")
    }

    /// Returns the commit `rev` resolves to, if any.
    fn resolve_commit(&self, rev: &str) -> Option<String> {
        ensure_safe_ref(rev).ok()?;
        let rev_spec = format!("{rev}^{{commit}}");
        let sha = self.run_git(&["rev-parse", "--verify", "--quiet", &rev_spec]).ok()?;
        let sha = sha.trim();
        is_valid_full_commit_sha(sha).then(|| sha.to_string())
    }

    /// Resolves the commit to simulate reverts against for changes on `branch` without
    /// touching the network.
    ///
    /// For `main`, this is the commit `base_ref()` points at (or the ref name itself if it
    /// cannot be resolved, e.g. in an empty repository). For any other branch, it is the
    /// first of `origin/<branch>` and `<branch>` that exists locally; if neither does, this
    /// fails rather than simulating against `main`.
    pub fn local_base_tip_for_branch(&self, branch: &str) -> Result<String> {
        let branch = normalize_branch(branch);
        ensure_safe_branch(branch)?;
        if branch == DEFAULT_BRANCH {
            let base = self.base_ref();
            return Ok(self.resolve_commit(base).unwrap_or_else(|| base.to_string()));
        }
        local_tracking_refs(branch).iter().find_map(|r| self.resolve_commit(r)).ok_or_else(|| {
            anyhow!(
                "Branch '{branch}' is not available locally in {:?} (no {DEFAULT_REMOTE}/{branch} or {branch}). Run `jiri update` or `git fetch {DEFAULT_REMOTE} {branch}` and retry.",
                self.path
            )
        })
    }

    /// Runs a remote-facing `git` command (see `remote_git_command`) and returns its stdout.
    fn run_remote_git(&self, args: &[&str]) -> Result<String> {
        let output = self
            .remote_git_command()
            .args(args)
            .output()
            .with_context(|| format!("Failed to run git {:?} in {:?}", args, self.path))?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed in {:?} (status {}): {}",
                args,
                self.path,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Queries `remote` for the current tip of `branch` and makes sure its objects (and those of
    /// any missing full-SHA `required_commits`) are present locally, returning the tip SHA.
    ///
    /// This is invisible to every user-facing piece of repository state: objects are fetched by
    /// SHA without any refmap, so no ref (`refs/remotes/*`, `refs/heads/*`, `refs/tags/*`,
    /// `HEAD`), reflog, `FETCH_HEAD`, `packed-refs`/reftable, `shallow`, or commit-graph file is
    /// written, and automatic maintenance/gc and hooks are disabled. Only content-addressed
    /// objects are added under `objects/`.
    ///
    /// A required commit that the remote cannot serve does not fail the call; callers must
    /// check `has_commit` for each required commit.
    pub fn fetch_remote_tip(
        &self,
        remote: &str,
        branch: &str,
        required_commits: &[&str],
    ) -> Result<String> {
        let branch = normalize_branch(branch);
        ensure_safe_ref(remote)?;
        ensure_safe_branch(branch)?;
        for c in required_commits {
            ensure_valid_sha(c)?;
        }
        let full_ref = format!("refs/heads/{branch}");

        let listing = self.run_remote_git(&[
            "ls-remote",
            "--refs",
            "--exit-code",
            "--end-of-options",
            remote,
            &full_ref,
        ])?;
        // `ls-remote` patterns match ref name suffixes, so select the exact ref.
        let tip_sha = listing
            .lines()
            .find_map(|line| {
                let mut cols = line.split('\t');
                let sha = cols.next()?.trim();
                let name = cols.next()?.trim();
                (name == full_ref).then_some(sha)
            })
            .ok_or_else(|| anyhow!("{remote} did not report {full_ref}"))?;
        if !is_valid_full_commit_sha(tip_sha) {
            return Err(anyhow!("{remote} reported invalid SHA {tip_sha:?} for {full_ref}"));
        }
        let tip_sha = tip_sha.to_ascii_lowercase();

        // Abbreviated SHAs cannot be requested from a remote, so only full SHAs are fetched.
        let missing: Vec<String> = required_commits
            .iter()
            .filter(|c| is_valid_full_commit_sha(c))
            .map(|c| c.to_ascii_lowercase())
            .filter(|c| *c != tip_sha && !self.has_commit(c))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if self.has_commit(&tip_sha) && missing.is_empty() {
            return Ok(tip_sha);
        }

        let mut args: Vec<String> = [
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "fetch.unpackLimit=1",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-write-commit-graph",
            "--no-update-shallow",
            "--no-write-fetch-head",
            "--refmap=",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // Advertise every local ref that may track the branch (any of which may be the most
        // recently updated one) to keep pack negotiation and the transferred pack small.
        let mut negotiation_tips: Vec<String> =
            UPSTREAM_TRACKING_REFS.iter().map(|r| r.to_string()).collect();
        negotiation_tips.extend(local_tracking_refs(branch));
        negotiation_tips.push("HEAD".to_string());
        let mut seen = BTreeSet::new();
        for r in negotiation_tips {
            if seen.insert(r.clone()) && self.ref_exists(&r) {
                args.push(format!("--negotiation-tip={r}"));
            }
        }
        args.extend(["--end-of-options".to_string(), remote.to_string(), tip_sha.clone()]);
        let tip_only_len = args.len();
        args.extend(missing.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.run_remote_git(&arg_refs) {
            Ok(_) => {}
            // A required commit the remote does not serve must not cost us the fresh tip.
            Err(e) if !missing.is_empty() => {
                eprintln!(
                    "Warning: Could not fetch {} from {remote} for {:?}: {e:#}",
                    missing.join(", "),
                    self.path
                );
                self.run_remote_git(&arg_refs[..tip_only_len])?;
            }
            Err(e) => return Err(e),
        }

        if !self.has_commit(&tip_sha) {
            return Err(anyhow!("Fetched {tip_sha} from {remote} but it is not present locally"));
        }
        Ok(tip_sha)
    }

    /// Returns the commit to simulate reverts against for changes on `branch`: the current tip of
    /// `branch` on `origin` (fetched invisibly via `fetch_remote_tip`), falling back to
    /// `local_base_tip_for_branch` when there is no `origin` remote, or when the remote cannot
    /// be reached.
    pub fn resolve_remote_base_tip(
        &self,
        branch: &str,
        required_commits: &[&str],
    ) -> Result<String> {
        let branch = normalize_branch(branch);
        ensure_safe_branch(branch)?;
        let origin_key = format!("remote.{DEFAULT_REMOTE}.url");
        if self.run_git(&["config", "--get", &origin_key]).is_err() {
            return self.local_base_tip_for_branch(branch);
        }
        match self.fetch_remote_tip(DEFAULT_REMOTE, branch, required_commits) {
            Ok(tip) => Ok(tip),
            Err(e) => {
                let fallback = self.local_base_tip_for_branch(branch).with_context(|| {
                    format!("Could not resolve the tip of '{branch}' on {DEFAULT_REMOTE}: {e:#}")
                })?;
                eprintln!(
                    "Warning: Could not resolve the tip of '{branch}' on {DEFAULT_REMOTE} for {:?}; simulating against the local copy of '{branch}' ({fallback}): {e:#}",
                    self.path
                );
                Ok(fallback)
            }
        }
    }

    /// Returns the parent SHAs of `commit_sha` from its raw object headers, ignoring
    /// replacement refs and shallow-clone grafts.
    fn commit_parents(&self, commit_sha: &str) -> Result<Vec<String>> {
        ensure_valid_sha(commit_sha)?;
        let raw =
            self.run_git_lossy(&["--no-replace-objects", "cat-file", "commit", commit_sha])?;
        let headers = raw.split("\n\n").next().unwrap_or_default();
        Ok(headers
            .lines()
            .filter_map(|l| l.strip_prefix("parent "))
            .map(|p| p.trim().to_string())
            .collect())
    }

    /// Simulates a 3-way revert of `commit_sha` on top of `target_tree_or_ref` in memory using
    /// `git merge-tree`. Does not touch the working directory, index, or create any
    /// branches/commits.
    ///
    /// Merge commits are reverted relative to their first parent (like Gerrit). Initial (root)
    /// commits and commits whose parent is missing from a shallow clone are rejected.
    pub fn simulate_revert_merge_tree(
        &self,
        commit_sha: &str,
        target_tree_or_ref: &str,
    ) -> Result<MergeTreeResult> {
        ensure_valid_sha(commit_sha)?;
        ensure_safe_ref(target_tree_or_ref)?;

        let parents = self.commit_parents(commit_sha)?;
        let Some(first_parent) = parents.first() else {
            return Err(anyhow!(
                "Cannot revert initial (root) commit {commit_sha} in {:?}: it has no parent",
                self.path
            ));
        };
        if !self.has_commit(first_parent) {
            return Err(anyhow!(
                "Commit {commit_sha} in {:?} is at a shallow-clone boundary (its parent {first_parent} is missing). Run `git fetch --unshallow` or `jiri update` and retry.",
                self.path
            ));
        }
        let merge_base_arg = format!("--merge-base={commit_sha}");

        let output = self
            .git_command()
            .args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                &merge_base_arg,
                first_parent,
                target_tree_or_ref,
            ])
            .output()
            .with_context(|| format!("Failed to run git merge-tree in {:?}", self.path))?;
        let stderr = String::from_utf8_lossy(&output.stderr);

        match output.status.code() {
            Some(0) => {
                let parsed = parse_merge_tree_z(&output.stdout)
                    .with_context(|| format!("Invalid git merge-tree output in {:?}", self.path))?;
                Ok(MergeTreeResult::Success { tree_sha: parsed.tree_sha })
            }
            Some(1) => {
                let parsed = parse_merge_tree_z(&output.stdout)
                    .with_context(|| format!("Invalid git merge-tree output in {:?}", self.path))?;
                let messages: String = parsed.messages.iter().map(|m| m.message.as_str()).collect();
                let details = format!("{}\n{}", messages.trim(), stderr.trim());
                Ok(MergeTreeResult::Conflict { unmerged_files: unmerged_files(&parsed), details })
            }
            _ => Err(anyhow!(
                "git merge-tree failed in {:?} (status {}): {}",
                self.path,
                output.status,
                stderr.trim()
            )),
        }
    }

    /// Runs `git` with `args` and returns its raw stdout, failing if it exits unsuccessfully.
    fn run_git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .git_command()
            .args(args)
            .output()
            .with_context(|| format!("Failed to run git {:?} in {:?}", args, self.path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(anyhow!(
                "git {:?} failed in {:?}\nStatus: {}\nStdout: {}\nStderr: {}",
                args,
                self.path,
                output.status,
                stdout.trim(),
                stderr.trim()
            ));
        }
        Ok(output.stdout)
    }

    pub fn run_git(&self, args: &[&str]) -> Result<String> {
        String::from_utf8(self.run_git_bytes(args)?)
            .with_context(|| format!("git output was not valid UTF-8 in {:?}", self.path))
    }

    /// Like `run_git`, but replaces invalid UTF-8 (e.g. legacy-encoded commit messages or
    /// author names) instead of failing.
    fn run_git_lossy(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run_git_bytes(args)?).into_owned())
    }

    /// Returns true if `rev` resolves to a commit in the local repository.
    fn ref_exists(&self, rev: &str) -> bool {
        if ensure_safe_ref(rev).is_err() {
            return false;
        }
        let rev_spec = format!("{rev}^{{commit}}");
        self.run_git(&["rev-parse", "--verify", "--quiet", &rev_spec]).is_ok()
    }

    /// Verifies if a given commit SHA exists in the local git repository.
    pub fn has_commit(&self, sha: &str) -> bool {
        is_valid_commit_sha(sha) && self.ref_exists(sha)
    }

    /// Returns the unix timestamp (seconds since epoch) for a given commit SHA.
    pub fn commit_timestamp(&self, sha: &str) -> Result<i64> {
        ensure_valid_sha(sha)?;
        let output =
            self.run_git(&["log", "-1", "--no-show-signature", "--format=%ct", sha, "--"])?;
        output
            .trim()
            .parse::<i64>()
            .with_context(|| format!("Invalid commit timestamp {:?} for {sha}", output.trim()))
    }

    /// Returns the full commit message of `sha`.
    pub fn commit_message(&self, sha: &str) -> Result<String> {
        ensure_valid_sha(sha)?;
        self.run_git_lossy(&["log", "-1", "--no-show-signature", "--format=%B", sha, "--"])
    }

    /// Checks if `ancestor` is an ancestor of `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        if !is_valid_commit_sha(ancestor) || !is_valid_commit_sha(descendant) {
            return false;
        }
        self.run_git(&["merge-base", "--is-ancestor", ancestor, descendant]).is_ok()
    }

    /// Returns every commit reachable from `target_ref` whose message has an exact
    /// `Change-Id: <change_id>` line, newest first.
    ///
    /// Uses plumbing `rev-list --basic-regexp` so that user configuration such as
    /// `grep.patternType` or `log.showSignature` cannot change the result.
    pub fn find_commits_by_change_id(
        &self,
        change_id: &str,
        target_ref: &str,
    ) -> Result<Vec<String>> {
        ensure_valid_change_id(change_id)?;
        ensure_safe_ref(target_ref)?;
        let grep_arg = format!("--grep=^Change-Id: {change_id}$");
        let output = self.run_git(&["rev-list", "--basic-regexp", &grep_arg, target_ref, "--"])?;
        Ok(output.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    }

    /// Returns the newest commit on `target_ref` whose message has an exact
    /// `Change-Id: <change_id>` line.
    #[cfg(test)]
    pub fn find_commit_by_change_id(
        &self,
        change_id: &str,
        target_ref: &str,
    ) -> Result<Option<String>> {
        Ok(self.find_commits_by_change_id(change_id, target_ref)?.into_iter().next())
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::GitRepo;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    /// Returns whether a `git` binary can be executed from `PATH`.
    ///
    /// Sandboxed test environments (such as the infra host test sandbox) do not provide
    /// `git`, so tests that spawn `git` subprocesses skip themselves via `require_git!`.
    pub fn git_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            Command::new("git")
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    /// Returns early from the calling test if `git` is not available.
    macro_rules! require_git {
        () => {
            if !$crate::git::test_util::git_available() {
                eprintln!("Skipping test: git binary not available in PATH");
                return;
            }
        };
    }
    pub(crate) use require_git;

    /// Writes `contents` to `path` (creating parent directories) and makes it executable.
    ///
    /// Use `wait_until_executable` before executing the file.
    pub fn write_executable(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs the freshly written executable at `path` until it can be executed.
    ///
    /// A child process forked concurrently by another test thread may briefly inherit the
    /// descriptor `path` was written through, making `exec` fail with `ETXTBSY`. Once the file
    /// has been executed successfully, no such descriptor can remain.
    pub fn wait_until_executable(path: &Path) {
        for _ in 0..500 {
            match Command::new(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => return,
            }
        }
        panic!("{path:?} is still busy");
    }

    /// Initializes an empty git repository with a configured test identity.
    ///
    /// Also installs an untracked no-op `.jiri_root/bin/jiri` (a symlink to `true`, which never
    /// needs to be written and thus cannot be busy), so the repository can serve as a hermetic
    /// Fuchsia checkout root for `JiriProjectMap::load`.
    pub fn init_repo(path: &Path) -> GitRepo {
        std::fs::create_dir_all(path).unwrap();
        let repo = GitRepo::new(path);
        repo.run_git(&["init", "-q", "-b", "main"]).unwrap();
        repo.run_git(&["config", "user.email", "tester@example.com"]).unwrap();
        repo.run_git(&["config", "user.name", "Tester"]).unwrap();
        repo.run_git(&["config", "commit.gpgsign", "false"]).unwrap();
        let jiri = path.join(".jiri_root/bin/jiri");
        std::fs::create_dir_all(jiri.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/bin/true", jiri).unwrap();
        repo
    }

    /// Writes `files` (path, content) and commits them with `message`, returning the new SHA.
    pub fn commit_files(repo: &GitRepo, files: &[(&str, &str)], message: &str) -> String {
        for (name, content) in files {
            let full = repo.path.join(name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
            repo.run_git(&["add", "--", name]).unwrap();
        }
        repo.run_git(&["commit", "-q", "--no-verify", "-m", message]).unwrap();
        repo.run_git(&["rev-parse", "HEAD"]).unwrap().trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{commit_files, init_repo, require_git, write_executable};
    use super::*;
    use assert_matches::assert_matches;
    use std::ffi::OsStr;
    use tempfile::tempdir;

    #[fuchsia::test]
    fn test_is_valid_commit_sha() {
        assert!(is_valid_commit_sha("abcdef1"));
        assert!(is_valid_commit_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_valid_commit_sha("abcdef"));
        assert!(!is_valid_commit_sha("0123456789abcdef0123456789abcdef012345678"));
        assert!(!is_valid_commit_sha("-abcdef1"));
        assert!(!is_valid_commit_sha("--output=x"));
        assert!(!is_valid_commit_sha("origin/main"));
        assert!(!is_valid_commit_sha("ghijklm"));
    }

    #[fuchsia::test]
    fn test_is_valid_change_id() {
        assert!(is_valid_change_id("I_t"));
        assert!(is_valid_change_id("Irevert90000"));
        assert!(is_valid_change_id("I0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_valid_change_id(""));
        assert!(!is_valid_change_id("I"));
        assert!(!is_valid_change_id("i123"));
        assert!(!is_valid_change_id("I.*"));
        assert!(!is_valid_change_id("I 1"));
        assert!(!is_valid_change_id("I1$"));
        assert!(!is_valid_change_id("--x"));
    }

    #[fuchsia::test]
    fn test_normalize_branch() {
        assert_eq!(normalize_branch("main"), "main");
        assert_eq!(normalize_branch(""), "main");
        assert_eq!(normalize_branch("refs/heads/releases/f30"), "releases/f30");
        assert_eq!(normalize_branch("releases/f30"), "releases/f30");
    }

    #[fuchsia::test]
    fn test_git_command_isolation() {
        let repo = GitRepo::new("/nonexistent");
        let cmd = repo.git_command();
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        for var in GIT_ENV_OVERRIDES {
            assert!(envs.contains(&(OsStr::new(var), None)), "{var} not cleared: {envs:?}");
        }
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args, ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/nonexistent")));

        let remote = repo.remote_git_command();
        let env =
            |name: &str| remote.get_envs().find(|(k, _)| *k == OsStr::new(name)).map(|(_, v)| v);
        assert_eq!(env("GIT_DIR"), Some(None));
        assert_eq!(env("GIT_TERMINAL_PROMPT"), Some(Some(OsStr::new("0"))));
        assert_eq!(env("GIT_ASKPASS"), Some(Some(OsStr::new(""))));
        assert_eq!(env("SSH_ASKPASS"), Some(Some(OsStr::new(""))));
        assert_matches!(
            env("GIT_SSH_COMMAND"),
            Some(Some(v)) if v.to_string_lossy().contains("BatchMode=yes")
        );
        let remote_args: Vec<String> =
            remote.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            remote_args[..4],
            ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]
        );
        for arg in ["core.askPass=", "http.connectTimeout=10", "http.lowSpeedTime=30"] {
            assert!(remote_args.iter().any(|a| a == arg), "{arg} missing: {remote_args:?}");
        }
    }

    #[fuchsia::test]
    fn test_rejects_invalid_shas_and_refs() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let sha = commit_files(&repo, &[("file.txt", "hello\n")], "Initial commit");

        assert!(repo.has_commit(&sha));
        assert!(!repo.has_commit("--all"));
        assert!(!repo.has_commit("HEAD"));
        assert!(repo.commit_timestamp("-1").is_err());
        assert!(repo.commit_message("--all").is_err());
        assert!(repo.simulate_revert_merge_tree("--foo", "HEAD").is_err());
        assert!(repo.simulate_revert_merge_tree(&sha, "--output=/tmp/x").is_err());
        assert!(!repo.is_ancestor("--all", &sha));
        assert!(repo.find_commit_by_change_id("I1", "--all").is_err());
    }

    #[fuchsia::test]
    fn test_commit_metadata() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let sha1 = commit_files(&repo, &[("file.txt", "hello\n")], "Initial commit");
        let sha2 = commit_files(
            &repo,
            &[("file.txt", "hello world\n")],
            "Update file\n\nReviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/42\nChange-Id: I2222222222222222222222222222222222222222",
        );

        assert_eq!(repo.base_ref(), "HEAD");
        assert!(repo.commit_timestamp(&sha1).unwrap() > 0);
        assert!(repo.is_ancestor(&sha1, &sha2));
        assert!(!repo.is_ancestor(&sha2, &sha1));
        let message = repo.commit_message(&sha2).unwrap();
        assert!(message.starts_with("Update file\n\nReviewed-on: "), "{message}");

        assert_eq!(
            repo.find_commit_by_change_id("I2222222222222222222222222222222222222222", "HEAD")
                .unwrap(),
            Some(sha2.clone())
        );
        assert_eq!(
            repo.find_commit_by_change_id("I3333333333333333333333333333333333333333", "HEAD")
                .unwrap(),
            None
        );
    }

    #[fuchsia::test]
    fn test_find_commit_by_change_id_ignores_user_config() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let id = "I2222222222222222222222222222222222222222";
        let sha1 = commit_files(&repo, &[("a.txt", "1\n")], &format!("First\n\nChange-Id: {id}"));
        let sha2 = commit_files(&repo, &[("a.txt", "2\n")], &format!("Reland\n\nChange-Id: {id}"));
        commit_files(&repo, &[("a.txt", "3\n")], &format!("Mention\n\nSee Change-Id: {id}"));
        // `fixed` patterns would match `^`/`$` literally, and signatures would pollute output.
        repo.run_git(&["config", "grep.patternType", "fixed"]).unwrap();
        repo.run_git(&["config", "log.showSignature", "true"]).unwrap();

        assert_eq!(repo.find_commit_by_change_id(id, "HEAD").unwrap(), Some(sha2.clone()));
        assert_eq!(repo.find_commits_by_change_id(id, "HEAD").unwrap(), vec![sha2.clone(), sha1]);
        assert!(repo.commit_timestamp(&sha2).unwrap() > 0);
        assert!(repo.commit_message(&sha2).unwrap().starts_with("Reland\n"));
        // Regex metacharacters and malformed ids are rejected rather than interpolated.
        assert!(repo.find_commit_by_change_id("I.*", "HEAD").is_err());
        assert!(repo.find_commit_by_change_id("not-an-id", "HEAD").is_err());
    }

    #[fuchsia::test]
    fn test_parse_merge_tree_z() {
        let tree = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            parse_merge_tree_z(format!("{tree}\0").as_bytes()).unwrap(),
            MergeTreeOutput { tree_sha: tree.into(), conflicted_paths: vec![], messages: vec![] }
        );

        let raw = format!(
            "{tree}\0a b.txt\0foo~HEAD\0\0\
             1\0a b.txt\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in a b.txt\n\0\
             2\0foo~HEAD\0foo\0{FILE_DIRECTORY_CONFLICT}\0CONFLICT (file/directory): moved\n\0"
        );
        let parsed = parse_merge_tree_z(raw.as_bytes()).unwrap();
        assert_eq!(parsed.tree_sha, tree);
        assert_eq!(parsed.conflicted_paths, vec!["a b.txt", "foo~HEAD"]);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[1].paths, vec!["foo~HEAD", "foo"]);
        assert_eq!(parsed.messages[1].kind, FILE_DIRECTORY_CONFLICT);
        assert_eq!(unmerged_files(&parsed), vec!["a b.txt", "foo"]);

        assert!(parse_merge_tree_z(b"").is_err());
        assert!(parse_merge_tree_z(format!("{tree}\0\0x\0").as_bytes()).is_err());
        assert!(parse_merge_tree_z(format!("{tree}\0\x002\0only-one\0").as_bytes()).is_err());
    }

    #[fuchsia::test]
    fn test_simulate_revert_clean() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        commit_files(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], "Initial commit");
        let change = commit_files(&repo, &[("a.txt", "a changed\n")], "Change a");
        commit_files(&repo, &[("b.txt", "b changed\n")], "Change b");

        assert_matches!(
            repo.simulate_revert_merge_tree(&change, "HEAD").unwrap(),
            MergeTreeResult::Success { tree_sha } => {
                let a = repo.run_git(&["show", &format!("{tree_sha}:a.txt")]).unwrap();
                let b = repo.run_git(&["show", &format!("{tree_sha}:b.txt")]).unwrap();
                assert_eq!(a, "a\n");
                assert_eq!(b, "b changed\n");
            }
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_conflict() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        commit_files(&repo, &[("a.txt", "line1\nline2\nline3\n")], "Initial commit");
        let change = commit_files(&repo, &[("a.txt", "line1\nCHANGED\nline3\n")], "Change a");
        let later = commit_files(&repo, &[("a.txt", "line1\nCHANGED AGAIN\nline3\n")], "Again");

        assert_matches!(
            repo.simulate_revert_merge_tree(&change, "HEAD").unwrap(),
            MergeTreeResult::Conflict { unmerged_files, details } => {
                assert_eq!(unmerged_files, vec!["a.txt".to_string()]);
                assert!(details.contains("a.txt"));
            }
        );

        // Reverting the later commit first makes the original revert clean.
        let tree = assert_matches!(
            repo.simulate_revert_merge_tree(&later, "HEAD").unwrap(),
            MergeTreeResult::Success { tree_sha } => tree_sha
        );
        assert_matches!(
            repo.simulate_revert_merge_tree(&change, &tree).unwrap(),
            MergeTreeResult::Success { .. }
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_conflict_paths_are_exact() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        // Uppercase names sort before `CONFLICT`/`Auto-merging` messages and lowercase names
        // after them; names with spaces, quotes, and non-ASCII characters must not be quoted
        // or trimmed.
        let names = [
            "BUILD.bazel",
            "CONFLICT_NOTES.md",
            "src/lib.rs",
            "sp ace.txt",
            "quo\"te.txt",
            "caf\u{e9}.txt",
            " lead.txt",
        ];
        let with = |content: &str| -> Vec<(&str, String)> {
            names.iter().map(|n| (*n, content.to_string())).collect()
        };
        let commit = |content: &str, message: &str| {
            let files = with(content);
            let refs: Vec<(&str, &str)> = files.iter().map(|(n, c)| (*n, c.as_str())).collect();
            commit_files(&repo, &refs, message)
        };
        commit("base\n", "Initial");
        let change = commit("changed\n", "Change");
        commit("changed again\n", "Later");

        assert_matches!(
            repo.simulate_revert_merge_tree(&change, "HEAD").unwrap(),
            MergeTreeResult::Conflict { unmerged_files, details } => {
                let mut expected: Vec<String> = names.iter().map(|n| n.to_string()).collect();
                expected.sort();
                assert_eq!(unmerged_files, expected);
                assert!(details.contains("CONFLICT (content)"), "{details}");
            }
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_file_directory_conflict() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        commit_files(&repo, &[("foo", "1\n"), ("other.txt", "o\n")], "Initial");
        let change = commit_files(&repo, &[("foo", "2\n")], "Change foo");
        repo.run_git(&["rm", "-q", "foo"]).unwrap();
        commit_files(&repo, &[("foo/bar", "dir\n")], "Replace foo with a directory");

        assert_matches!(
            repo.simulate_revert_merge_tree(&change, "HEAD").unwrap(),
            MergeTreeResult::Conflict { unmerged_files, .. } => {
                assert_eq!(unmerged_files, vec!["foo".to_string()]);
            }
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_root_commit_is_error() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let root = commit_files(&repo, &[("root.txt", "root\n")], "Root commit");
        commit_files(&repo, &[("other.txt", "other\n")], "Other commit");

        assert_matches!(
            repo.simulate_revert_merge_tree(&root, "HEAD"),
            Err(e) if e.to_string().contains("initial (root) commit")
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_shallow_boundary_is_error() {
        require_git!();
        let tmp = tempdir().unwrap();
        let origin = init_repo(&tmp.path().join("origin"));
        commit_files(&origin, &[("a.txt", "1\n")], "First");
        commit_files(&origin, &[("a.txt", "2\n")], "Second");
        let url = format!("file://{}", origin.path.display());
        GitRepo::new(tmp.path())
            .run_git(&["clone", "-q", "--depth=1", "--end-of-options", &url, "shallow"])
            .unwrap();
        let shallow = GitRepo::new(tmp.path().join("shallow"));
        let head = shallow.run_git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        assert_matches!(
            shallow.simulate_revert_merge_tree(&head, "HEAD"),
            Err(e) if e.to_string().contains("shallow-clone boundary")
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_merge_commit_uses_first_parent() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        commit_files(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], "Initial");
        repo.run_git(&["checkout", "-q", "-b", "side"]).unwrap();
        commit_files(&repo, &[("b.txt", "b from side\n")], "Side change");
        repo.run_git(&["checkout", "-q", "main"]).unwrap();
        commit_files(&repo, &[("a.txt", "a on main\n")], "Main change");
        repo.run_git(&["merge", "-q", "--no-ff", "--no-edit", "side"]).unwrap();
        let merge = repo.run_git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        commit_files(&repo, &[("c.txt", "c\n")], "Later");

        assert_matches!(
            repo.simulate_revert_merge_tree(&merge, "HEAD").unwrap(),
            MergeTreeResult::Success { tree_sha } => {
                let show = |f: &str| repo.run_git(&["show", &format!("{tree_sha}:{f}")]).unwrap();
                assert_eq!(show("b.txt"), "b\n");
                assert_eq!(show("a.txt"), "a on main\n");
                assert_eq!(show("c.txt"), "c\n");
            }
        );
    }

    #[fuchsia::test]
    fn test_non_utf8_commit_message() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(&tmp.path().join("repo"));
        commit_files(&repo, &[("a.txt", "a\n")], "Initial");
        std::fs::write(repo.path.join("a.txt"), "b\n").unwrap();
        repo.run_git(&["add", "a.txt"]).unwrap();
        // A Latin-1 encoded message (`caf\xe9`) that is not valid UTF-8.
        let message_file = tmp.path().join("message");
        std::fs::write(&message_file, b"caf\xe9\n").unwrap();
        let message_arg = message_file.to_str().unwrap();
        repo.run_git(&[
            "-c",
            "i18n.commitEncoding=",
            "commit",
            "-q",
            "--no-verify",
            "-F",
            message_arg,
        ])
        .unwrap();
        let sha = repo.run_git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        assert_eq!(repo.commit_message(&sha).unwrap(), "caf\u{fffd}\n\n");
        assert_matches!(
            repo.simulate_revert_merge_tree(&sha, "HEAD").unwrap(),
            MergeTreeResult::Success { .. }
        );
    }

    #[fuchsia::test]
    fn test_simulate_revert_bad_target_is_error() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        commit_files(&repo, &[("a.txt", "a\n")], "Initial commit");
        let sha = commit_files(&repo, &[("a.txt", "b\n")], "Change");
        // A nonexistent target ref is a hard git failure, not a conflict.
        assert!(repo.simulate_revert_merge_tree(&sha, "does-not-exist").is_err());
    }

    #[fuchsia::test]
    fn test_is_valid_full_commit_sha() {
        assert!(is_valid_full_commit_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(is_valid_full_commit_sha("0123456789ABCDEF0123456789ABCDEF01234567"));
        assert!(!is_valid_full_commit_sha("abcdef1"));
        assert!(!is_valid_full_commit_sha("0123456789abcdef0123456789abcdef0123456"));
        assert!(!is_valid_full_commit_sha("0123456789abcdef0123456789abcdef012345678"));
        assert!(!is_valid_full_commit_sha("--upload-pack=0123456789abcdef0123456789ab"));
        assert!(!is_valid_full_commit_sha("g123456789abcdef0123456789abcdef01234567"));
    }

    #[fuchsia::test]
    fn test_base_ref_prefers_jiri_head_over_head() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let sha1 = commit_files(&repo, &[("a.txt", "1\n")], "First");
        commit_files(&repo, &[("a.txt", "2\n")], "Second");
        assert_eq!(repo.base_ref(), "HEAD");
        repo.run_git(&["update-ref", "refs/remotes/jiri/head", &sha1]).unwrap();
        assert_eq!(repo.base_ref(), "jiri/head");
        repo.run_git(&["update-ref", "refs/remotes/origin/main", &sha1]).unwrap();
        assert_eq!(repo.base_ref(), "origin/main");
    }

    /// Recursively collects `(relative path, contents)` of every file under `.git`, excluding
    /// the content-addressed object store (but including `objects/info`, e.g. commit-graphs).
    fn snapshot_git_dir(repo: &GitRepo) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut std::collections::BTreeMap<String, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                if rel.starts_with("objects/") && !rel.starts_with("objects/info") {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let git_dir = repo.path.join(".git");
        let mut out = std::collections::BTreeMap::new();
        walk(&git_dir, &git_dir, &mut out);
        out
    }

    /// Creates an `origin` repository and a lagging clone of it. Returns the temp dir, origin,
    /// clone, and the SHAs of the two commits that only exist on origin (older first).
    fn lagging_clone_setup() -> (tempfile::TempDir, GitRepo, GitRepo, String, String) {
        let tmp = tempdir().unwrap();
        let origin_path = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_path).unwrap();
        let origin = init_repo(&origin_path);
        commit_files(&origin, &[("a.txt", "a\n")], "Initial");
        let local_path = tmp.path().join("local");
        GitRepo::new(tmp.path())
            .run_git(&["clone", "-q", "--end-of-options", "origin", "local"])
            .unwrap();
        let local = GitRepo::new(&local_path);
        let ahead1 = commit_files(&origin, &[("a.txt", "a\nb\n")], "Ahead 1");
        let ahead2 = commit_files(&origin, &[("b.txt", "b\n")], "Ahead 2");
        // A branch whose name ends in `/main` must not be confused with `main`.
        origin.run_git(&["branch", "feature/main", &ahead1]).unwrap();
        (tmp, origin, local, ahead1, ahead2)
    }

    #[fuchsia::test]
    fn test_fetch_remote_tip_fetches_objects_without_touching_refs() {
        require_git!();
        let (tmp, _origin, local, ahead1, ahead2) = lagging_clone_setup();
        let old_origin_main =
            local.run_git(&["rev-parse", "refs/remotes/origin/main"]).unwrap().trim().to_string();
        assert!(!local.has_commit(&ahead1));
        assert!(!local.has_commit(&ahead2));
        // Hooks (which `fetch` still runs for ref transactions) must never be invoked.
        let marker = tmp.path().join("hook_ran");
        write_executable(
            &local.path.join(".git/hooks/reference-transaction"),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let before = snapshot_git_dir(&local);
        assert!(!before.contains_key("FETCH_HEAD"));

        let tip = local.fetch_remote_tip("origin", "refs/heads/main", &[]).unwrap();
        assert_eq!(tip, ahead2);
        assert!(local.has_commit(&ahead1));
        assert!(local.has_commit(&ahead2));
        assert!(local.is_ancestor(&old_origin_main, &tip));
        assert!(!marker.exists());

        // HEAD, FETCH_HEAD (still absent), packed-refs/reftable, refs/**, logs/**, config,
        // index, etc. are byte-for-byte identical.
        let after = snapshot_git_dir(&local);
        assert_eq!(before, after);
        assert_eq!(
            local.run_git(&["rev-parse", "refs/remotes/origin/main"]).unwrap().trim(),
            old_origin_main
        );
        assert_eq!(local.base_ref(), "origin/main");

        // A second call finds everything locally and still changes nothing.
        assert_matches!(local.fetch_remote_tip("origin", "main", &[&ahead1]), Ok(t) if t == ahead2);
        assert_matches!(local.fetch_remote_tip("origin", "", &[]), Ok(t) if t == ahead2);
        assert_eq!(snapshot_git_dir(&local), before);
    }

    #[fuchsia::test]
    fn test_fetch_remote_tip_fetches_missing_required_commits() {
        require_git!();
        let (_tmp, origin, local, _ahead1, ahead2) = lagging_clone_setup();
        // A commit on origin that is not reachable from the tip of `main`.
        let side = origin
            .run_git(&["commit-tree", "HEAD^{tree}", "-p", "HEAD~1", "-m", "Side"])
            .unwrap()
            .trim()
            .to_string();
        origin.run_git(&["update-ref", "refs/heads/side", &side]).unwrap();

        assert_eq!(local.fetch_remote_tip("origin", "main", &[]).unwrap(), ahead2);
        assert!(!local.has_commit(&side));
        let before = snapshot_git_dir(&local);

        // The tip is already present, but the missing required commit is still fetched.
        assert_eq!(local.fetch_remote_tip("origin", "main", &[&side]).unwrap(), ahead2);
        assert!(local.has_commit(&side));
        assert_eq!(snapshot_git_dir(&local), before);

        // A required commit the remote does not have does not fail the fetch of the tip.
        let unknown = "0".repeat(40);
        assert_eq!(local.fetch_remote_tip("origin", "main", &[&unknown]).unwrap(), ahead2);
        assert!(!local.has_commit(&unknown));
        assert!(local.fetch_remote_tip("origin", "main", &["-x"]).is_err());
    }

    #[fuchsia::test]
    fn test_fetch_remote_tip_selects_exact_branch() {
        require_git!();
        let (_tmp, _origin, local, ahead1, _ahead2) = lagging_clone_setup();
        assert_eq!(local.fetch_remote_tip("origin", "feature/main", &[]).unwrap(), ahead1);
    }

    #[fuchsia::test]
    fn test_fetch_remote_tip_rejects_bad_input_and_missing_refs() {
        require_git!();
        let (_tmp, _origin, local, _ahead1, _ahead2) = lagging_clone_setup();
        let before = snapshot_git_dir(&local);
        assert!(local.fetch_remote_tip("origin", "does-not-exist", &[]).is_err());
        assert!(local.fetch_remote_tip("origin", "-x", &[]).is_err());
        assert!(local.fetch_remote_tip("origin", "--upload-pack=touch /tmp/x", &[]).is_err());
        assert!(local.fetch_remote_tip("origin", "ma*", &[]).is_err());
        assert!(local.fetch_remote_tip("origin", "a b", &[]).is_err());
        assert!(local.fetch_remote_tip("origin", "main@{1}", &[]).is_err());
        assert!(local.fetch_remote_tip("--upload-pack=x", "main", &[]).is_err());
        assert!(local.fetch_remote_tip("no-such-remote", "main", &[]).is_err());
        assert_eq!(snapshot_git_dir(&local), before);
    }

    #[fuchsia::test]
    fn test_resolve_remote_base_tip() {
        require_git!();
        // With an `origin`, the up-to-date remote tip is used.
        let (_tmp, _origin, local, _ahead1, ahead2) = lagging_clone_setup();
        assert_eq!(local.resolve_remote_base_tip("main", &[]).unwrap(), ahead2);

        // Branches that exist neither on origin nor locally never fall back to `main`.
        let err = local.resolve_remote_base_tip("does-not-exist", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("not available locally"), "{err:#}");

        // Without an `origin` remote, the local base ref commit is used.
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let head = commit_files(&repo, &[("a.txt", "a\n")], "Initial");
        assert_eq!(repo.resolve_remote_base_tip("main", &[&head]).unwrap(), head);
    }

    #[fuchsia::test]
    fn test_local_base_tip_for_branch() {
        require_git!();
        let tmp = tempdir().unwrap();
        let repo = init_repo(tmp.path());
        let sha1 = commit_files(&repo, &[("a.txt", "1\n")], "First");
        let sha2 = commit_files(&repo, &[("a.txt", "2\n")], "Second");
        let head = commit_files(&repo, &[("a.txt", "3\n")], "Third");

        assert_eq!(repo.local_base_tip_for_branch("main").unwrap(), head);
        assert_eq!(repo.resolve_remote_base_tip("", &[&head]).unwrap(), head);

        repo.run_git(&["branch", "releases/f30", &sha1]).unwrap();
        assert_eq!(repo.local_base_tip_for_branch("releases/f30").unwrap(), sha1);
        assert_eq!(repo.resolve_remote_base_tip("refs/heads/releases/f30", &[]).unwrap(), sha1);

        // `origin/<branch>` is preferred over the local branch.
        repo.run_git(&["update-ref", "refs/remotes/origin/releases/f30", &sha2]).unwrap();
        assert_eq!(repo.local_base_tip_for_branch("releases/f30").unwrap(), sha2);
        repo.run_git(&["update-ref", "refs/remotes/origin/releases/f31", &sha2]).unwrap();
        assert_eq!(repo.resolve_remote_base_tip("releases/f31", &[]).unwrap(), sha2);

        // A missing release branch is an error instead of a silent fallback to `main`.
        let err = repo.resolve_remote_base_tip("releases/nope", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("'releases/nope' is not available locally"), "{err}");
        assert!(repo.local_base_tip_for_branch("-x").is_err());
        // Reflog selectors would resolve to a stale local tip.
        assert!(repo.local_base_tip_for_branch("releases/f30@{1}").is_err());
        assert!(repo.resolve_remote_base_tip("a b", &[]).is_err());
    }
}
