// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::gerrit::{ChangeDetail, CurlGerritFetcher, GerritFetcher, GerritUrl};
use crate::git::{GitRepo, MergeTreeResult, normalize_branch};
use crate::graph::{DependencyNode, PlannedRevert, RevertGraphBuilder, RevertWave};
use crate::jiri::JiriProjectMap;
use crate::message::ParsedCommitMessage;

pub const RUBBER_STAMPER_REVIEWER: &str = "rubber-stamper@appspot.gserviceaccount.com";
pub const DEFAULT_REASON: &str = "Reverting to unblock tree";
/// Gerrit label voted +1 when `--auto-submit` is passed.
pub const AUTO_SUBMIT_LABEL: &str = "Fuchsia-Auto-Submit";

const STATUS_MERGED: &str = "MERGED";

/// Simulation base commit of each `(repository, branch)` (the up-to-date remote branch tip
/// when reachable).
type BaseTips = HashMap<(PathBuf, String), String>;

#[derive(Debug, Clone)]
pub struct RevertOptions {
    pub gerrit_url: String,
    pub reason: Option<String>,
    pub failure_link: Option<String>,
    pub bug: Option<String>,
    pub upload: bool,
    pub auto_submit: bool,
    pub dry_run: bool,
    pub fuchsia_dir: Option<PathBuf>,
}

/// Prefix of the reason line in a Gerrit revert reason.
const REASON_PREFIX: &str = "Reason for revert:";
/// Variant of `REASON_PREFIX` with a space before the colon, also accepted as already formatted.
const REASON_SPACED_PREFIX: &str = "Reason for revert :";
/// Prefix of the failure link line in a Gerrit revert reason.
const FAILURE_LINK_PREFIX: &str = "Failure Link:";
/// Prefix of the bug line in a Gerrit revert reason.
const BUG_PREFIX: &str = "Bug:";

/// Formats the revert reason payload for Gerrit REST API, ensuring standard "Reason for revert:"
/// and "Failure Link:" / "Bug:" sections are formatted consistently.
/// Note: `Bug:` lines in the reason are emitted in the bottom git trailer block by
/// `format_pure_revert_message` (while remaining in the reason body for `revert_submission`).
pub fn format_gerrit_revert_reason(
    reason: Option<&str>,
    failure_link: Option<&str>,
    bug: Option<&str>,
) -> String {
    let raw_reason = reason.unwrap_or(DEFAULT_REASON).trim();
    let mut parts = Vec::new();

    if raw_reason.starts_with(REASON_PREFIX) || raw_reason.starts_with(REASON_SPACED_PREFIX) {
        parts.push(raw_reason.to_string());
    } else {
        parts.push(format!("{REASON_PREFIX} {raw_reason}"));
    }

    if let Some(link) = failure_link {
        let link = link.trim();
        if !link.is_empty() && !parts[0].contains(FAILURE_LINK_PREFIX) {
            parts.push(format!("{FAILURE_LINK_PREFIX} {link}"));
        }
    }

    if let Some(b) = bug {
        let b = b.trim();
        if !b.is_empty() && !parts[0].contains(BUG_PREFIX) {
            parts.push(format!("{BUG_PREFIX} {b}"));
        }
    }

    parts.join("\n\n")
}

#[derive(Debug, Clone)]
pub struct RevertPlan {
    /// Gerrit topic applied to all created revert CLs.
    pub topic: String,
    /// Formatted "Reason for revert:" block.
    pub reason: String,
    pub reverts: Vec<PlannedRevert>,
    pub waves: Vec<RevertWave>,
}

/// A Gerrit change together with the host it lives on.
#[derive(Debug, Clone)]
struct HostedChange {
    host: String,
    detail: ChangeDetail,
}

/// Returns the identifier used to address `ch` in Gerrit REST calls (number preferred, since
/// a Change-Id can be ambiguous across branches).
fn change_ref(ch: &ChangeDetail) -> String {
    if ch.number != 0 { ch.number.to_string() } else { ch.change_id.clone() }
}

#[derive(Debug)]
pub struct RevertPlanner<'a, F: GerritFetcher> {
    fetcher: &'a F,
    jiri_map: &'a JiriProjectMap,
}

impl<'a, F: GerritFetcher> RevertPlanner<'a, F> {
    pub fn new(fetcher: &'a F, jiri_map: &'a JiriProjectMap) -> Self {
        Self { fetcher, jiri_map }
    }

    pub fn compute_plan(&self, opts: &RevertOptions) -> Result<RevertPlan> {
        let parsed_url = GerritUrl::parse(&opts.gerrit_url)?;
        let reason = format_gerrit_revert_reason(
            opts.reason.as_deref(),
            opts.failure_link.as_deref(),
            opts.bug.as_deref(),
        );

        println!(
            "==> Querying Gerrit for change {} on {}",
            parsed_url.change_id_or_number, parsed_url.host
        );
        let target_change = self
            .fetcher
            .fetch_change_detail(&parsed_url.host, &parsed_url.change_id_or_number)
            .with_context(|| {
                format!("Failed to fetch details for change {}", parsed_url.change_id_or_number)
            })?;
        if target_change.status != STATUS_MERGED {
            return Err(anyhow!(
                "Change {} on {} has status '{}' (must be MERGED to revert)",
                change_ref(&target_change),
                parsed_url.host,
                target_change.status
            ));
        }

        let target_number = target_change.number;
        let changes = vec![HostedChange { host: parsed_url.host, detail: target_change }];
        let mut base_tips = BaseTips::new();

        // Map changes to local git repositories and extract commit SHAs
        let (nodes, repos_map) = self.build_nodes(&changes, &mut base_tips)?;
        for node in &nodes {
            if !node.original_depends_on.is_empty() {
                eprintln!(
                    "Warning: Change {} declares Depends-on footer(s) ({}); related changes are not reverted automatically.",
                    node.change_id,
                    node.original_depends_on.join(", ")
                );
            }
        }

        // Build Dependency Graph
        let mut builder = RevertGraphBuilder::new();
        for node in &nodes {
            builder.add_node(node.clone());
        }

        // Add intra-repo ancestry relations:
        // For any two nodes u and v in the same repo, if u is ancestor of v (v landed after u),
        // then v was committed after u -> Revert(v) must be applied before Revert(u).
        for (i, ni) in nodes.iter().enumerate() {
            for (j, nj) in nodes.iter().enumerate() {
                if i != j && ni.repo_path == nj.repo_path && ni.branch == nj.branch {
                    let repo = &repos_map[&ni.repo_path];
                    if repo.is_ancestor(&ni.commit_sha, &nj.commit_sha) {
                        builder.add_parent_relation(&nj.commit_sha, &ni.commit_sha);
                    }
                }
            }
        }

        let planned = builder.compute_revert_plan()?;
        // Reject dependency cycles before spending time on (meaningless) merge simulations of
        // an arbitrarily ordered plan.
        let waves = builder.compute_revert_waves(&planned)?;
        simulate_plan(&planned, &repos_map, &base_tips)?;
        println!(
            "==> Simulation succeeded with 0 conflicts! Revert plan is complete ({} total commits).",
            planned.len()
        );

        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let topic = format!("revert-{}-{}", target_number, timestamp);
        Ok(RevertPlan { topic, reason, reverts: planned, waves })
    }

    /// Maps changes to dependency graph nodes backed by local git repositories.
    ///
    /// The simulation base commit of each repository and branch is resolved (and its objects
    /// invisibly fetched) on first use and cached in `base_tips`. Every node's commit is
    /// verified to be contained in its simulation base.
    fn build_nodes(
        &self,
        changes: &[HostedChange],
        base_tips: &mut BaseTips,
    ) -> Result<(Vec<DependencyNode>, HashMap<PathBuf, GitRepo>)> {
        let mut nodes = Vec::new();
        let mut repos_map: HashMap<PathBuf, GitRepo> = HashMap::new();

        for hc in changes {
            let ch = &hc.detail;
            let repo_path =
                self.jiri_map.resolve_project_path(&ch.project, &hc.host).ok_or_else(|| {
                    anyhow!(
                        "Could not resolve local path for project '{}' ({})",
                        ch.project,
                        hc.host
                    )
                })?;

            let repo: &GitRepo =
                repos_map.entry(repo_path.clone()).or_insert_with(|| GitRepo::new(&repo_path));

            let mut commit_sha = latest_revision(ch)?;
            let branch = change_branch(ch).to_string();
            let key = (repo_path.clone(), branch.clone());
            let (mut base_tip, mut fresh) = match base_tips.get(&key) {
                Some(tip) => (tip.clone(), false),
                None => (repo.resolve_remote_base_tip(&branch, &[&commit_sha])?, true),
            };
            // A change may have landed after a cached tip was resolved: re-resolve the tip
            // (fetching any missing objects) before relying on it.
            let mut refresh = |commit_sha: &str, base_tip: &mut String| -> Result<()> {
                if !fresh {
                    *base_tip = repo.resolve_remote_base_tip(&branch, &[commit_sha])?;
                    fresh = true;
                }
                Ok(())
            };

            if !repo.has_commit(&commit_sha) {
                refresh(&commit_sha, &mut base_tip)?;
            }
            // The Gerrit revision may differ from the landed commit (e.g. rebased on submit):
            // find the landed commit on the base tip by Change-Id.
            if !repo.has_commit(&commit_sha) {
                commit_sha = select_landed_commit(repo, &ch.change_id, &base_tip, ch.number)?
                    .ok_or_else(|| {
                        anyhow!(
                            "Commit {} for '{}' was not found in local git history at {:?}.\nPlease run `jiri update` to sync local repository history.",
                            commit_sha,
                            ch.subject,
                            repo_path
                        )
                    })?;
            }
            // Simulating against a base that does not contain the commit would run a
            // backwards merge and report bogus conflicts.
            if !repo.is_ancestor(&commit_sha, &base_tip) {
                refresh(&commit_sha, &mut base_tip)?;
                if !repo.is_ancestor(&commit_sha, &base_tip) {
                    return Err(anyhow!(
                        "Commit {} for '{}' is not an ancestor of the simulation base {} (branch '{}') in {:?}. The local branch tip is behind this change; run `jiri update` or `git fetch origin {}` and retry.",
                        commit_sha,
                        ch.subject,
                        base_tip,
                        branch,
                        repo_path,
                        branch
                    ));
                }
            }
            base_tips.insert(key, base_tip);

            let raw_msg = current_commit_message(ch);
            let parsed_msg = ParsedCommitMessage::parse(&raw_msg);
            let timestamp = repo.commit_timestamp(&commit_sha).unwrap_or(0);

            nodes.push(DependencyNode {
                change_id: ch.change_id.clone(),
                change_number: ch.number,
                submission_id: ch.submission_id.clone(),
                commit_sha,
                project: ch.project.clone(),
                host: hc.host.clone(),
                repo_path,
                branch,
                subject: ch.subject.clone(),
                raw_message: raw_msg,
                original_depends_on: parsed_msg.depends_on,
                timestamp,
            });
        }

        Ok((nodes, repos_map))
    }
}

/// Returns the change number named by the last `Reviewed-on:` footer of `message`, if any.
fn reviewed_on_change_number(message: &str) -> Option<u64> {
    let url = message.lines().rev().find_map(|l| l.trim().strip_prefix("Reviewed-on:"))?;
    GerritUrl::parse(url.trim()).ok()?.change_id_or_number.parse().ok()
}

/// Returns the newest commit on `base_tip` that landed `change_id` as change `number`.
///
/// Commits whose `Reviewed-on:` footer names a different change are skipped: relands and
/// cherry-picks reuse the Change-Id of the original change.
fn select_landed_commit(
    repo: &GitRepo,
    change_id: &str,
    base_tip: &str,
    number: u64,
) -> Result<Option<String>> {
    for sha in repo.find_commits_by_change_id(change_id, base_tip)? {
        if number == 0 {
            return Ok(Some(sha));
        }
        match reviewed_on_change_number(&repo.commit_message(&sha)?) {
            Some(n) if n != number => continue,
            _ => return Ok(Some(sha)),
        }
    }
    Ok(None)
}

/// Simulates `planned` in memory with `git merge-tree` (zero filesystem writes), failing on
/// the first conflict.
///
/// Each repository and branch is simulated on top of its commit in `base_tips` (falling back to
/// the local `base_ref()`).
fn simulate_plan(
    planned: &[PlannedRevert],
    repos_map: &HashMap<PathBuf, GitRepo>,
    base_tips: &BaseTips,
) -> Result<()> {
    println!(
        "==> Simulating planned revert order ({} commits) in-memory to verify conflict-free execution...",
        planned.len()
    );
    let mut sim_trees: HashMap<(PathBuf, String), String> = HashMap::new();

    for (idx, item) in planned.iter().enumerate() {
        let repo = &repos_map[&item.repo_path];
        let short_sha = item.original_commit_sha.get(..8).unwrap_or(&item.original_commit_sha);
        println!(
            "    [{}/{}] Simulating revert [{}] '{}' ({})",
            idx + 1,
            planned.len(),
            item.project,
            item.subject,
            short_sha
        );

        let key = (item.repo_path.clone(), item.branch.clone());
        let current_tree = match sim_trees.get(&key) {
            Some(t) => t.clone(),
            None => base_tips.get(&key).cloned().unwrap_or_else(|| repo.base_ref().to_string()),
        };

        match repo.simulate_revert_merge_tree(&item.original_commit_sha, &current_tree)? {
            MergeTreeResult::Success { tree_sha } => {
                sim_trees.insert(key, tree_sha);
            }
            MergeTreeResult::Conflict { unmerged_files, details: _ } => {
                return Err(anyhow!(
                    "Reverting [{}] '{}' ({}) conflicts with later changes in {:?} on {} file(s): {}. Please revert manually.",
                    item.project,
                    item.subject,
                    short_sha,
                    item.repo_path,
                    unmerged_files.len(),
                    unmerged_files.join(", ")
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct RevertExecutor<'a, F: GerritFetcher> {
    fetcher: &'a F,
}

impl<'a, F: GerritFetcher> RevertExecutor<'a, F> {
    pub fn new(fetcher: &'a F) -> Self {
        Self { fetcher }
    }

    pub fn execute(&self, plan: &RevertPlan, opts: &RevertOptions) -> Result<()> {
        if plan.reverts.is_empty() {
            println!("==> No reverts to execute.");
            return Ok(());
        }
        let [item] = plan.reverts.as_slice() else {
            return Err(anyhow!(
                "Uploading a revert plan with {} changes is not supported yet. Please revert these changes manually.",
                plan.reverts.len()
            ));
        };

        let change_ref = if item.original_change_number != 0 {
            item.original_change_number.to_string()
        } else {
            item.original_change_id.clone()
        };

        println!("\n==> Creating revert of change {} on {}...", change_ref, item.host);
        let message = item.revert_message(&plan.reason, &[]);
        let rev_cl = self
            .fetcher
            .revert_single(&item.host, &change_ref, Some(&message), Some(&plan.topic))
            .with_context(|| format!("Failed to revert change {} on {}", change_ref, item.host))?;

        let project = if !rev_cl.project.is_empty() { &rev_cl.project } else { &item.project };
        let cl_url = if rev_cl.number != 0 {
            format!("https://{}/c/{}/+/{}", item.host, project, rev_cl.number)
        } else {
            format!("https://{}/q/{}", item.host, rev_cl.change_id)
        };
        println!("    + Created Revert CL: {} ({})", cl_url, rev_cl.subject);

        if rev_cl.number == 0 {
            eprintln!(
                "    Warning: Gerrit did not return a change number for revert {}; skipping reviewer assignment.",
                rev_cl.change_id
            );
        } else {
            let mut labels = HashMap::new();
            if opts.auto_submit {
                labels.insert(AUTO_SUBMIT_LABEL.to_string(), 1);
            }
            match self.fetcher.post_review(
                &item.host,
                &rev_cl.number.to_string(),
                &[RUBBER_STAMPER_REVIEWER],
                &labels,
            ) {
                Err(e) => eprintln!(
                    "    Warning: Failed to set Rubber Stamper / labels on CL {}: {:#}",
                    rev_cl.number, e
                ),
                Ok(()) if opts.auto_submit => {
                    println!("      [+] Added Rubber Stamper reviewer and {AUTO_SUBMIT_LABEL} +1")
                }
                Ok(()) => println!("      [+] Added Rubber Stamper reviewer"),
            }
        }

        println!("\n========================================================");
        println!("               REVERT ORCHESTRATION COMPLETE            ");
        println!("========================================================");
        println!("Generated Revert CL: {cl_url}");
        if !opts.auto_submit {
            println!(
                "\nNote: {AUTO_SUBMIT_LABEL} was not set. Ensure Rubber Stamper approves, then click Submit or pass --auto-submit."
            );
        }
        println!("========================================================\n");

        Ok(())
    }
}

#[derive(Debug)]
pub struct RevertEngine<F: GerritFetcher> {
    fetcher: F,
}

impl RevertEngine<CurlGerritFetcher> {
    pub fn new() -> Self {
        Self::with_fetcher(CurlGerritFetcher)
    }
}

impl Default for RevertEngine<CurlGerritFetcher> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: GerritFetcher> RevertEngine<F> {
    pub fn with_fetcher(fetcher: F) -> Self {
        Self { fetcher }
    }

    pub fn run(&self, opts: RevertOptions) -> Result<()> {
        let fuchsia_dir = match &opts.fuchsia_dir {
            Some(d) => d.clone(),
            None => detect_fuchsia_dir()?,
        };

        let jiri_map = JiriProjectMap::load(&fuchsia_dir)
            .with_context(|| "Failed to load Jiri workspace project map")?;

        let planner = RevertPlanner::new(&self.fetcher, &jiri_map);
        let plan = planner.compute_plan(&opts)?;

        let dry_run = opts.dry_run || !opts.upload;
        print_plan_summary(&plan, &opts, dry_run);
        if dry_run {
            if !opts.upload && !opts.dry_run {
                println!("Run with --upload to execute the revert plan on Gerrit.");
            }
            return Ok(());
        }

        let executor = RevertExecutor::new(&self.fetcher);
        executor.execute(&plan, &opts)
    }
}

fn detect_fuchsia_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("FUCHSIA_DIR") {
        let p = PathBuf::from(dir);
        if p.exists() {
            return Ok(p);
        }
    }
    let current = std::env::current_dir()?;
    let mut curr_opt: Option<&Path> = Some(current.as_path());
    while let Some(dir) = curr_opt {
        if dir.join(".jiri_manifest").exists() || dir.join(".jiri_root").exists() {
            return Ok(dir.to_path_buf());
        }
        curr_opt = dir.parent();
    }
    Err(anyhow!(
        "Could not locate Fuchsia root directory. Please ensure $FUCHSIA_DIR is set or run from within the Fuchsia tree."
    ))
}

/// Returns the SHA of the latest revision of `ch`.
fn latest_revision(ch: &ChangeDetail) -> Result<String> {
    if let Some(r) = &ch.current_revision {
        return Ok(r.clone());
    }
    ch.revisions
        .iter()
        .max_by_key(|(_, d)| d.number)
        .map(|(sha, _)| sha.clone())
        .ok_or_else(|| anyhow!("Change {} has no revisions", ch.change_id))
}

/// Returns the short name of the branch `ch` landed on (`main` if Gerrit did not report one).
fn change_branch(ch: &ChangeDetail) -> &str {
    normalize_branch(&ch.branch)
}

fn current_commit_message(ch: &ChangeDetail) -> String {
    if let Ok(rev) = latest_revision(ch)
        && let Some(rev_detail) = ch.revisions.get(&rev)
        && let Some(commit) = &rev_detail.commit
    {
        return commit.message.clone();
    }
    // Fall back to subject
    ch.subject.clone()
}

fn print_plan_summary(plan: &RevertPlan, opts: &RevertOptions, dry_run: bool) {
    println!("\n========================================================");
    if dry_run {
        println!("                   DRY RUN EXECUTION PLAN               ");
    } else {
        println!("                  REVERT EXECUTION PLAN                 ");
    }
    println!("========================================================");
    println!("Topic Name         : {}", plan.topic);
    println!("Input Gerrit Change: {}", opts.gerrit_url);
    println!("Revert Reason Msg  :\n{}", indent_lines(&plan.reason, "  | "));
    println!(
        "Auto-Submit        : {}",
        if opts.auto_submit {
            format!("Enabled ({AUTO_SUBMIT_LABEL} +1)")
        } else {
            "Disabled (Manual)".to_string()
        }
    );
    println!("Reviewer           : {}", RUBBER_STAMPER_REVIEWER);
    println!("Total Waves        : {}", plan.waves.len());
    println!("Total CLs to Revert: {}", plan.reverts.len());
    println!("--------------------------------------------------------");

    for wave in &plan.waves {
        println!("\n[Wave {}] ({} cluster(s)):", wave.wave_index + 1, wave.clusters.len());
        for (c_idx, cluster) in wave.clusters.iter().enumerate() {
            println!(
                "  Cluster {}.{} - Project: '{}' on {} (Submission: {:?}, {} change(s)):",
                wave.wave_index + 1,
                c_idx + 1,
                cluster.project,
                cluster.host,
                cluster.submission_id,
                cluster.changes.len()
            );
            println!("      Local repository: {}", cluster.repo_path.display());
            for item in &cluster.changes {
                let orig_url = if item.original_change_number != 0 {
                    format!(
                        "https://{}/c/{}/+/{}",
                        item.host, item.project, item.original_change_number
                    )
                } else {
                    format!("https://{}/q/{}", item.host, item.original_change_id)
                };
                println!(
                    "    - Reverting [{}] \"{}\"\n      Original Change: {}",
                    item.project, item.subject, orig_url
                );
                println!("      Revert commit message:");
                let message = item.revert_message(&plan.reason, &[]);
                println!("{}", indent_lines(message.trim(), "      | "));
            }
        }
    }
    if dry_run {
        println!("\nNote: This was a dry run. No revert CLs have been created on Gerrit.");
    }
    println!("========================================================\n");
}

fn indent_lines(s: &str, prefix: &str) -> String {
    s.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gerrit::{CommitDetail, RevisionDetail};
    use crate::git::test_util::{commit_files, init_repo, require_git};
    use std::sync::Mutex;

    const FUCHSIA_HOST: &str = "fuchsia-review.googlesource.com";

    /// Recorded (host, change ref, message, topic) of a `revert_single` call.
    type RevertCall = (String, String, String, String);
    /// Recorded (host, change ref, reviewers, labels) of a `post_review` call.
    type ReviewCall = (String, String, Vec<String>, HashMap<String, i32>);

    #[derive(Debug, Default)]
    struct MockFetcher {
        /// (host, number or Change-Id) -> change.
        changes: HashMap<(String, String), ChangeDetail>,
        revert_single_calls: Mutex<Vec<RevertCall>>,
        post_review_calls: Mutex<Vec<ReviewCall>>,
    }

    impl MockFetcher {
        fn add_change(&mut self, host: &str, detail: ChangeDetail) {
            self.changes.insert((host.into(), detail.number.to_string()), detail.clone());
            self.changes.insert((host.into(), detail.change_id.clone()), detail);
        }
    }

    impl GerritFetcher for MockFetcher {
        fn fetch_change_detail(&self, host: &str, change_id: &str) -> Result<ChangeDetail> {
            self.changes
                .get(&(host.into(), change_id.into()))
                .cloned()
                .ok_or_else(|| anyhow!("HTTP 404: change {change_id} not found on {host}"))
        }

        fn revert_single(
            &self,
            host: &str,
            change_id: &str,
            message: Option<&str>,
            topic: Option<&str>,
        ) -> Result<ChangeDetail> {
            self.revert_single_calls.lock().unwrap().push((
                host.into(),
                change_id.into(),
                message.unwrap_or_default().into(),
                topic.unwrap_or_default().into(),
            ));
            let original = self.fetch_change_detail(host, change_id)?;
            Ok(ChangeDetail {
                id: format!("{}~main~Irevert90000", original.project),
                project: original.project.clone(),
                branch: original.branch.clone(),
                change_id: "Irevert90000".into(),
                number: 90000,
                subject: format!("Revert \"{}\"", original.subject),
                status: "NEW".into(),
                current_revision: None,
                revisions: HashMap::new(),
                submitted: None,
                topic: None,
                submission_id: None,
                revert_of: Some(original.number),
            })
        }

        fn post_review(
            &self,
            host: &str,
            change_id: &str,
            reviewers: &[&str],
            labels: &HashMap<String, i32>,
        ) -> Result<()> {
            self.post_review_calls.lock().unwrap().push((
                host.into(),
                change_id.into(),
                reviewers.iter().map(|r| r.to_string()).collect(),
                labels.clone(),
            ));
            Ok(())
        }
    }

    fn change(
        project: &str,
        number: u64,
        change_id: &str,
        sha: &str,
        message: &str,
        submission_id: &str,
    ) -> ChangeDetail {
        let mut revisions = HashMap::new();
        revisions.insert(
            sha.to_string(),
            RevisionDetail {
                number: 1,
                commit: Some(CommitDetail {
                    commit: Some(sha.to_string()),
                    subject: message.lines().next().map(String::from),
                    message: message.to_string(),
                    parents: Vec::new(),
                }),
            },
        );
        ChangeDetail {
            id: format!("{project}~main~{change_id}"),
            project: project.into(),
            branch: "main".into(),
            change_id: change_id.into(),
            number,
            subject: message.lines().next().unwrap_or_default().into(),
            status: STATUS_MERGED.into(),
            current_revision: Some(sha.to_string()),
            revisions,
            submitted: Some("2026-08-18 12:00:00.000000000".into()),
            topic: None,
            submission_id: Some(submission_id.into()),
            revert_of: None,
        }
    }

    fn options(fuchsia_dir: &Path, url: &str, upload: bool) -> RevertOptions {
        RevertOptions {
            gerrit_url: url.into(),
            reason: Some("Flaky tests".into()),
            failure_link: Some("go/bbid/123456".into()),
            bug: Some("789".into()),
            upload,
            auto_submit: true,
            dry_run: !upload,
            fuchsia_dir: Some(fuchsia_dir.to_path_buf()),
        }
    }

    fn compute_plan<F: GerritFetcher>(fetcher: &F, opts: &RevertOptions) -> Result<RevertPlan> {
        let jiri_map =
            JiriProjectMap { projects: Vec::new(), fuchsia_dir: opts.fuchsia_dir.clone().unwrap() };
        RevertPlanner::new(fetcher, &jiri_map).compute_plan(opts)
    }

    fn plan_ids(plan: &RevertPlan) -> Vec<Vec<String>> {
        plan.waves
            .iter()
            .map(|w| {
                w.clusters
                    .iter()
                    .flat_map(|c| c.changes.iter().map(|ch| ch.original_change_id.clone()))
                    .collect()
            })
            .collect()
    }

    /// Sets up a fuchsia checkout with a single change `I_t` (number 123456).
    fn single_change_setup() -> (tempfile::TempDir, MockFetcher, String) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(temp_dir.path());
        commit_files(&repo, &[("README.md", "readme\n")], "Initial commit");
        let msg = "[core] Test commit\n\nChange-Id: I_t";
        let sha = commit_files(&repo, &[("a.txt", "hello\n")], msg);
        let mut fetcher = MockFetcher::default();
        fetcher.add_change(FUCHSIA_HOST, change("fuchsia", 123456, "I_t", &sha, msg, "sub_t"));
        (temp_dir, fetcher, sha)
    }

    const TARGET_URL: &str = "https://fuchsia-review.googlesource.com/c/fuchsia/+/123456";

    #[fuchsia::test]
    fn test_format_gerrit_revert_reason() {
        // Plain string
        let r1 = format_gerrit_revert_reason(Some("DNS, try 3"), None, None);
        assert_eq!(r1, "Reason for revert: DNS, try 3");

        // With failure link and bug
        let r2 = format_gerrit_revert_reason(
            Some("Suspect CL broke core.arm64-debug"),
            Some("go/bbid/8672361213915190673"),
            Some("12345"),
        );
        assert_eq!(
            r2,
            "Reason for revert: Suspect CL broke core.arm64-debug\n\nFailure Link: go/bbid/8672361213915190673\n\nBug: 12345"
        );

        // Already formatted with Reason for revert:
        let r3 = format_gerrit_revert_reason(
            Some("Reason for revert: Custom formatted reason\n\nFailure Link: http://example.com"),
            None,
            None,
        );
        assert_eq!(
            r3,
            "Reason for revert: Custom formatted reason\n\nFailure Link: http://example.com"
        );

        // Default reason
        assert_eq!(
            format_gerrit_revert_reason(None, None, None),
            format!("Reason for revert: {DEFAULT_REASON}")
        );
    }

    #[fuchsia::test]
    fn test_single_change_plan_is_one_wave() {
        require_git!();
        let (temp_dir, fetcher, sha) = single_change_setup();
        let plan = compute_plan(&fetcher, &options(temp_dir.path(), TARGET_URL, false)).unwrap();
        assert_eq!(plan_ids(&plan), vec![vec!["I_t".to_string()]]);
        assert_eq!(plan.reverts.len(), 1);
        let item = &plan.reverts[0];
        assert_eq!(item.original_commit_sha, sha);
        assert_eq!(item.repo_path, temp_dir.path());
        assert_eq!(item.host, FUCHSIA_HOST);
        assert_eq!(item.branch, "main");
        assert!(plan.topic.starts_with("revert-123456-"), "{}", plan.topic);
    }

    #[fuchsia::test]
    fn test_revert_engine_dry_run() {
        require_git!();
        let (temp_dir, fetcher, _) = single_change_setup();
        let engine = RevertEngine::with_fetcher(fetcher);
        engine.run(options(temp_dir.path(), TARGET_URL, false)).unwrap();
        assert!(engine.fetcher.revert_single_calls.lock().unwrap().is_empty());
        assert!(engine.fetcher.post_review_calls.lock().unwrap().is_empty());

        // `--dry-run` overrides `--upload`.
        let mut opts = options(temp_dir.path(), TARGET_URL, true);
        opts.dry_run = true;
        engine.run(opts).unwrap();
        assert!(engine.fetcher.revert_single_calls.lock().unwrap().is_empty());
        assert!(engine.fetcher.post_review_calls.lock().unwrap().is_empty());
    }

    #[fuchsia::test]
    fn test_rejects_non_merged_target() {
        require_git!();
        let (temp_dir, mut fetcher, sha) = single_change_setup();
        let mut detail = change("fuchsia", 123456, "I_t", &sha, "[core] Test commit", "sub_t");
        detail.status = "NEW".into();
        fetcher.add_change(FUCHSIA_HOST, detail);
        let err = compute_plan(&fetcher, &options(temp_dir.path(), TARGET_URL, false)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("has status 'NEW' (must be MERGED to revert)"), "{msg}");
    }

    #[fuchsia::test]
    fn test_rejects_commit_missing_from_local_history() {
        require_git!();
        let (temp_dir, mut fetcher, _) = single_change_setup();
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let msg = "[core] Elsewhere\n\nChange-Id: I_missing";
        fetcher.add_change(FUCHSIA_HOST, change("fuchsia", 123456, "I_missing", missing, msg, "s"));
        let err = compute_plan(&fetcher, &options(temp_dir.path(), TARGET_URL, false)).unwrap_err();
        assert!(format!("{err}").contains("was not found in local git history"), "{err:#}");
    }

    #[fuchsia::test]
    fn test_revert_engine_execution_single_change_uses_revert() {
        require_git!();
        let (temp_dir, fetcher, sha) = single_change_setup();
        let engine = RevertEngine::with_fetcher(fetcher);
        engine.run(options(temp_dir.path(), TARGET_URL, true)).unwrap();

        let calls = engine.fetcher.revert_single_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (host, change_ref, message, topic) = &calls[0];
        assert_eq!(host, FUCHSIA_HOST);
        assert_eq!(change_ref, "123456");
        assert!(topic.starts_with("revert-123456-"), "{topic}");
        assert!(message.starts_with("Revert \"[core] Test commit\"\n\n"), "{message}");
        assert!(message.contains(&format!("This reverts commit {sha}.")), "{message}");
        assert!(message.contains("Reason for revert: Flaky tests"), "{message}");
        assert!(message.contains("Failure Link: go/bbid/123456"), "{message}");
        assert!(message.contains("> [core] Test commit"), "{message}");
        assert!(
            message.ends_with("\nBug: 789\n"),
            "Bug: should be placed at the very end of footers (no Change-Id in dry-run)"
        );
        assert!(!message.contains("\nChange-Id: "), "{message}");

        let reviews = engine.fetcher.post_review_calls.lock().unwrap();
        assert_eq!(reviews.len(), 1);
        let (host, change_ref, reviewers, labels) = &reviews[0];
        assert_eq!(host, FUCHSIA_HOST);
        // The review targets the created revert CL, not the original change.
        assert_eq!(change_ref, "90000");
        assert_eq!(reviewers, &vec![RUBBER_STAMPER_REVIEWER.to_string()]);
        assert_eq!(labels.get(AUTO_SUBMIT_LABEL), Some(&1));
        assert_eq!(labels.get("Auto-Submit"), None);
    }

    #[fuchsia::test]
    fn test_execution_without_auto_submit_sets_no_labels() {
        require_git!();
        let (temp_dir, fetcher, _) = single_change_setup();
        let engine = RevertEngine::with_fetcher(fetcher);
        let mut opts = options(temp_dir.path(), TARGET_URL, true);
        opts.auto_submit = false;
        engine.run(opts).unwrap();

        assert_eq!(engine.fetcher.revert_single_calls.lock().unwrap().len(), 1);
        let reviews = engine.fetcher.post_review_calls.lock().unwrap();
        assert_eq!(reviews.len(), 1);
        let (_, change_ref, reviewers, labels) = &reviews[0];
        assert_eq!(change_ref, "90000");
        assert_eq!(reviewers, &vec![RUBBER_STAMPER_REVIEWER.to_string()]);
        assert!(labels.is_empty(), "{labels:?}");
    }

    /// Sets up a checkout with change 100 (`I_r`), its revert, and its reland as change 200,
    /// which reuses the Change-Id. Returns the SHAs of the original and the reland.
    fn reland_setup() -> (tempfile::TempDir, MockFetcher, String, String) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(temp_dir.path());
        commit_files(&repo, &[("a.txt", "a\n"), ("b.txt", "b\n")], "Initial");
        let url = "https://fuchsia-review.googlesource.com/c/fuchsia/+";
        let original_msg = format!("[core] Feature\n\nReviewed-on: {url}/100\nChange-Id: I_r");
        let original = commit_files(&repo, &[("a.txt", "feature\n")], &original_msg);
        commit_files(&repo, &[("a.txt", "a\n")], "Revert \"[core] Feature\"\n\nChange-Id: I_rr");
        let reland_msg = format!("[core] Feature\n\nReviewed-on: {url}/200\nChange-Id: I_r");
        let reland = commit_files(&repo, &[("b.txt", "feature\n")], &reland_msg);
        // Gerrit's revisions were rebased on submit, so neither is in local history.
        let mut fetcher = MockFetcher::default();
        let gerrit_sha = "d".repeat(40);
        fetcher.add_change(
            FUCHSIA_HOST,
            change("fuchsia", 100, "I_r", &gerrit_sha, &original_msg, "s1"),
        );
        fetcher.add_change(
            FUCHSIA_HOST,
            change("fuchsia", 200, "I_r", &gerrit_sha, &reland_msg, "s2"),
        );
        (temp_dir, fetcher, original, reland)
    }

    #[fuchsia::test]
    fn test_reland_prefers_commit_matching_reviewed_on() {
        require_git!();
        let (temp_dir, fetcher, original, reland) = reland_setup();
        let url = "https://fuchsia-review.googlesource.com/c/fuchsia/+";
        // Change 100 maps to its own (older) commit, not to the newer reland sharing its
        // Change-Id.
        let plan = compute_plan(&fetcher, &options(temp_dir.path(), &format!("{url}/100"), false))
            .unwrap();
        assert_eq!(plan.reverts[0].original_commit_sha, original);
        let plan = compute_plan(&fetcher, &options(temp_dir.path(), &format!("{url}/200"), false))
            .unwrap();
        assert_eq!(plan.reverts[0].original_commit_sha, reland);

        let repo = GitRepo::new(temp_dir.path());
        assert_eq!(select_landed_commit(&repo, "I_r", "HEAD", 0).unwrap(), Some(reland));
        assert_eq!(select_landed_commit(&repo, "I_r", "HEAD", 300).unwrap(), None);
    }

    #[fuchsia::test]
    fn test_reviewed_on_change_number() {
        let url = "https://fuchsia-review.googlesource.com/c/fuchsia/+";
        assert_eq!(reviewed_on_change_number(&format!("x\n\nReviewed-on: {url}/42\n")), Some(42));
        assert_eq!(
            reviewed_on_change_number(&format!(
                "x\n\nOriginal-Reviewed-on: {url}/1\nReviewed-on: {url}/2\nChange-Id: I_x"
            )),
            Some(2)
        );
        assert_eq!(reviewed_on_change_number("x\n\nChange-Id: I_x"), None);
    }

    #[fuchsia::test]
    fn test_rejects_commit_not_on_base_tip() {
        require_git!();
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(temp_dir.path());
        let main_tip = commit_files(&repo, &[("a.txt", "a\n")], "Initial");
        repo.run_git(&["checkout", "-q", "-b", "side"]).unwrap();
        let msg = "[core] Side\n\nChange-Id: I_s";
        let side = commit_files(&repo, &[("b.txt", "b\n")], msg);
        repo.run_git(&["checkout", "-q", "main"]).unwrap();
        // The simulation base (`jiri/head`) lags behind the change.
        repo.run_git(&["update-ref", "refs/remotes/jiri/head", &main_tip]).unwrap();
        let mut fetcher = MockFetcher::default();
        fetcher.add_change(FUCHSIA_HOST, change("fuchsia", 123456, "I_s", &side, msg, "s"));

        let err = compute_plan(&fetcher, &options(temp_dir.path(), TARGET_URL, false)).unwrap_err();
        assert!(format!("{err}").contains("is not an ancestor of the simulation base"), "{err:#}");
    }

    #[fuchsia::test]
    fn test_simulate_plan_keys_by_branch() {
        require_git!();
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(temp_dir.path());
        commit_files(&repo, &[("a.txt", "base\n")], "Initial");
        repo.run_git(&["branch", "releases/f30"]).unwrap();
        let main_change = commit_files(&repo, &[("a.txt", "main\n")], "Main change");
        let main_tip = main_change.clone();
        repo.run_git(&["checkout", "-q", "releases/f30"]).unwrap();
        let release_change = commit_files(&repo, &[("a.txt", "release\n")], "Release change");
        repo.run_git(&["checkout", "-q", "main"]).unwrap();

        let planned_revert = |sha: &str, branch: &str| PlannedRevert {
            original_change_id: format!("I_{branch}"),
            original_change_number: 0,
            submission_id: None,
            original_commit_sha: sha.to_string(),
            project: "fuchsia".into(),
            host: FUCHSIA_HOST.into(),
            repo_path: temp_dir.path().to_path_buf(),
            branch: branch.into(),
            subject: "Change".into(),
            raw_message: "Change".into(),
            original_depends_on: Vec::new(),
            atomic_partners: Vec::new(),
        };
        let planned = vec![
            planned_revert(&main_change, "main"),
            planned_revert(&release_change, "releases/f30"),
        ];
        let repos_map = HashMap::from([(temp_dir.path().to_path_buf(), repo)]);
        let mut base_tips = BaseTips::new();
        base_tips.insert((temp_dir.path().to_path_buf(), "main".into()), main_tip);
        base_tips
            .insert((temp_dir.path().to_path_buf(), "releases/f30".into()), release_change.clone());
        // Each branch is simulated on its own tip; sharing one simulated tree would conflict.
        simulate_plan(&planned, &repos_map, &base_tips).unwrap();
    }

    #[fuchsia::test]
    fn test_executor_rejects_multi_change_plans() {
        require_git!();
        let (temp_dir, fetcher, _) = single_change_setup();
        let opts = options(temp_dir.path(), TARGET_URL, true);
        let mut plan = compute_plan(&fetcher, &opts).unwrap();
        plan.reverts.push(plan.reverts[0].clone());
        let err = RevertExecutor::new(&fetcher).execute(&plan, &opts).unwrap_err();
        assert!(format!("{err}").contains("not supported yet"), "{err:#}");
        assert!(fetcher.revert_single_calls.lock().unwrap().is_empty());
    }

    /// Sets up target I_t whose revert conflicts with a later commit I_l.
    fn conflict_setup() -> (tempfile::TempDir, MockFetcher) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(temp_dir.path());
        commit_files(&repo, &[("a.txt", "l1\nl2\nl3\n"), ("b.txt", "b\n")], "Initial");
        let t_msg = "[core] Target\n\nChange-Id: I_t";
        let t_sha = commit_files(&repo, &[("a.txt", "l1\nX\nl3\n")], t_msg);
        let l_msg = "[core] Later\n\nReviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/202\nChange-Id: I_l";
        commit_files(&repo, &[("a.txt", "l1\nY\nl3\n")], l_msg);
        // Unrelated later commit touching a different file.
        commit_files(&repo, &[("b.txt", "b2\n")], "[core] Unrelated\n\nChange-Id: I_u");

        let mut fetcher = MockFetcher::default();
        fetcher.add_change(FUCHSIA_HOST, change("fuchsia", 123456, "I_t", &t_sha, t_msg, "st"));
        (temp_dir, fetcher)
    }

    #[fuchsia::test]
    fn test_conflicting_revert_fails_simulation() {
        require_git!();
        let (temp_dir, fetcher) = conflict_setup();
        let err = compute_plan(&fetcher, &options(temp_dir.path(), TARGET_URL, false)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("conflicts with later changes"), "{msg}");
        assert!(msg.contains("a.txt"), "{msg}");
        assert!(!msg.contains("b.txt"), "{msg}");
    }

    #[fuchsia::test]
    fn test_change_branch() {
        let mut ch = change("fuchsia", 1, "I_1", "1111", "[core] Change", "sub");
        assert_eq!(change_branch(&ch), "main");
        ch.branch = "refs/heads/releases/f30".into();
        assert_eq!(change_branch(&ch), "releases/f30");
        ch.branch = String::new();
        assert_eq!(change_branch(&ch), "main");
    }

    #[fuchsia::test]
    fn test_current_commit_message_without_current_revision() {
        let mut ch = change("fuchsia", 1, "I_1", "1111", "[core] Old commit", "sub");
        ch.current_revision = None;
        ch.revisions.insert(
            "2222".to_string(),
            crate::gerrit::RevisionDetail {
                number: 2,
                commit: Some(crate::gerrit::CommitDetail {
                    commit: None,
                    subject: None,
                    parents: vec![],
                    message: "[core] New commit".into(),
                }),
            },
        );
        let msg = current_commit_message(&ch);
        assert_eq!(msg, "[core] New commit");
    }
}
