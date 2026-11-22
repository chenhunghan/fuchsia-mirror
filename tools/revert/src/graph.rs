// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::message::format_pure_revert_message;

/// `(host prefix, Change-Id)` identity of a change as used in `Depends-on` footers (e.g.
/// `("turquoise-internal", "I1234...")`).
pub type DependsOnKey = (String, String);

/// Represents a change that is planned to be reverted.
#[derive(Debug, Clone)]
pub struct PlannedRevert {
    pub original_change_id: String,
    pub original_change_number: u64,
    pub submission_id: Option<String>,
    pub original_commit_sha: String,
    pub project: String,
    pub host: String,
    pub repo_path: PathBuf,
    /// Short name of the branch the change landed on (e.g. `main` or `releases/f30`).
    pub branch: String,
    pub subject: String,
    pub raw_message: String,
    pub original_depends_on: Vec<String>,
    /// `Depends-on` keys of changes that formed an atomic (mutual `Depends-on`) pair with
    /// this change. The reverts of those changes must be linked via `Depends-on` footers.
    pub atomic_partners: Vec<DependsOnKey>,
}

/// Node representing a change in the dependency graph.
#[derive(Debug, Clone)]
pub struct DependencyNode {
    pub change_id: String,
    pub change_number: u64,
    pub submission_id: Option<String>,
    pub commit_sha: String,
    pub project: String,
    pub host: String,
    pub repo_path: PathBuf,
    /// Short name of the branch the change landed on (e.g. `main` or `releases/f30`).
    pub branch: String,
    pub subject: String,
    pub raw_message: String,
    pub original_depends_on: Vec<String>,
    pub timestamp: i64,
}

/// A cluster of changes belonging to the same repository, branch, and submission.
#[derive(Debug, Clone)]
pub struct SubmissionCluster {
    pub repo_path: PathBuf,
    pub project: String,
    pub host: String,
    pub submission_id: Option<String>,
    pub changes: Vec<PlannedRevert>,
}

/// A wave of independent submission clusters that can be dispatched in parallel.
#[derive(Debug, Clone)]
pub struct RevertWave {
    pub wave_index: usize,
    pub clusters: Vec<SubmissionCluster>,
}

/// Builds and inverts the dependency graph for a collection of changes.
#[derive(Debug, Default)]
pub struct RevertGraphBuilder {
    nodes: Vec<DependencyNode>,
    /// Intra-repo parent-child ancestry: sha -> parent_shas
    commit_parents: HashMap<String, Vec<String>>,
}

/// Returns the `Depends-on` host prefix (e.g. `turquoise-internal`) for a Gerrit host/project.
pub fn host_prefix(host: &str, project: &str) -> &'static str {
    if host.contains("turquoise-internal") || project.contains("vendor/google") {
        "turquoise-internal"
    } else if host.contains("partner-fuchsia") || project.contains("partner-fuchsia") {
        "partner-fuchsia"
    } else {
        "fuchsia"
    }
}

/// Returns the `Depends-on` key of the change `change_id` in `project` on Gerrit `host`.
pub fn depends_on_key(host: &str, project: &str, change_id: &str) -> DependsOnKey {
    (host_prefix(host, project).to_string(), change_id.trim().to_string())
}

/// Parses a `Depends-on` footer value (`[<host-prefix>:]<Change-Id>`, with whitespace tolerated
/// around both parts). Values without a prefix refer to a change on the host of the depending
/// change (`default_host` / `default_project`).
pub fn parse_depends_on_key(dep: &str, default_host: &str, default_project: &str) -> DependsOnKey {
    match dep.split_once(':') {
        Some((prefix, change_id)) => {
            let prefix = if prefix.contains("turquoise-internal") {
                "turquoise-internal"
            } else if prefix.contains("partner-fuchsia") {
                "partner-fuchsia"
            } else {
                "fuchsia"
            };
            (prefix.to_string(), change_id.trim().to_string())
        }
        None => depends_on_key(default_host, default_project, dep),
    }
}

/// Minimal union-find over cluster indices.
struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        Self { parent: (0..n).collect() }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// The fields of a change that determine its dependency edges.
struct EdgeItem<'a> {
    host: &'a str,
    project: &'a str,
    change_id: &'a str,
    commit_sha: &'a str,
    repo_path: &'a Path,
    branch: &'a str,
    depends_on: &'a [String],
}

impl EdgeItem<'_> {
    /// Returns the `Depends-on` key of this item.
    fn key(&self) -> DependsOnKey {
        depends_on_key(self.host, self.project, self.change_id)
    }
}

impl<'a> From<&'a DependencyNode> for EdgeItem<'a> {
    fn from(n: &'a DependencyNode) -> Self {
        Self {
            host: &n.host,
            project: &n.project,
            change_id: &n.change_id,
            commit_sha: &n.commit_sha,
            repo_path: &n.repo_path,
            branch: &n.branch,
            depends_on: &n.original_depends_on,
        }
    }
}

impl<'a> From<&'a PlannedRevert> for EdgeItem<'a> {
    fn from(p: &'a PlannedRevert) -> Self {
        Self {
            host: &p.host,
            project: &p.project,
            change_id: &p.original_change_id,
            commit_sha: &p.original_commit_sha,
            repo_path: &p.repo_path,
            branch: &p.branch,
            depends_on: &p.original_depends_on,
        }
    }
}

/// Returns the entries of `candidates` other than `self_idx`, preferring those for which
/// `on_same_branch` holds when the key is ambiguous (e.g. a change cherry-picked to several
/// branches).
fn select_candidates(
    candidates: &[usize],
    self_idx: usize,
    on_same_branch: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let others: Vec<usize> = candidates.iter().copied().filter(|&c| c != self_idx).collect();
    if others.len() <= 1 {
        return others;
    }
    let same_branch: Vec<usize> = others.iter().copied().filter(|&c| on_same_branch(c)).collect();
    if same_branch.is_empty() { others } else { same_branch }
}

/// Forward dependency edges between items. Edge `(u, v)` means `u` landed after (or declared a
/// `Depends-on` on) `v`, so Revert(u) must be applied before Revert(v).
///
/// Physical git ancestry (`git`) and `Depends-on` footers (`dep`) are kept separate so that a
/// `Depends-on` footer can never drop or invert git ancestry.
struct Edges {
    git: HashSet<(usize, usize)>,
    dep: HashSet<(usize, usize)>,
}

impl Edges {
    fn compute(items: &[EdgeItem<'_>], commit_parents: &HashMap<String, Vec<String>>) -> Self {
        let mut sha_to_idx: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut key_to_idx: HashMap<DependsOnKey, Vec<usize>> = HashMap::new();
        for (i, item) in items.iter().enumerate() {
            sha_to_idx.entry(item.commit_sha).or_default().push(i);
            key_to_idx.entry(item.key()).or_default().push(i);
        }

        // Git ancestry only orders changes in the same repository and branch.
        let mut git = HashSet::new();
        for (child_sha, parent_shas) in commit_parents {
            for &u in sha_to_idx.get(child_sha.as_str()).into_iter().flatten() {
                for p_sha in parent_shas {
                    for &v in sha_to_idx.get(p_sha.as_str()).into_iter().flatten() {
                        if u != v
                            && items[u].repo_path == items[v].repo_path
                            && items[u].branch == items[v].branch
                        {
                            git.insert((u, v));
                        }
                    }
                }
            }
        }

        let mut dep = HashSet::new();
        for (u, item) in items.iter().enumerate() {
            for d in item.depends_on {
                let key = parse_depends_on_key(d, item.host, item.project);
                let Some(candidates) = key_to_idx.get(&key) else { continue };
                let targets = select_candidates(candidates, u, |c| items[c].branch == item.branch);
                dep.extend(targets.into_iter().map(|v| (u, v)));
            }
        }
        Self { git, dep }
    }

    /// Returns true if the `Depends-on` edge `e` points against git ancestry (git wins).
    fn contradicts_git(&self, &(u, v): &(usize, usize)) -> bool {
        self.git.contains(&(v, u))
    }

    /// `Depends-on` edges that do not contradict git ancestry.
    fn consistent_dep(&self) -> impl Iterator<Item = &(usize, usize)> {
        self.dep.iter().filter(|e| !self.contradicts_git(e))
    }

    /// Atomic pairs `(u, v)` with `u < v`: mutual `Depends-on` without any git ancestry
    /// between them.
    fn atomic_pairs(&self) -> HashSet<(usize, usize)> {
        self.dep
            .iter()
            .filter(|&&(u, v)| {
                u < v
                    && self.dep.contains(&(v, u))
                    && !self.git.contains(&(u, v))
                    && !self.git.contains(&(v, u))
            })
            .copied()
            .collect()
    }
}

impl PlannedRevert {
    /// Formats the pure revert commit message with the given `Depends-on` footers.
    pub fn revert_message(&self, reason: &str, depends_on: &[String]) -> String {
        format_pure_revert_message(
            &self.original_commit_sha,
            &self.subject,
            &self.raw_message,
            reason,
            depends_on,
            None,
        )
    }

    /// Returns the `(host prefix, Change-Id)` key other changes use to refer to this change.
    pub fn depends_on_key(&self) -> DependsOnKey {
        depends_on_key(&self.host, &self.project, &self.original_change_id)
    }
}

impl RevertGraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `node` unless the same change is already present. Changes are identified by
    /// `(host, project, change number)` (or `(host, project, commit SHA)`), never by bare
    /// Change-Id: cherry-picks and mirrors on other hosts or branches share Change-Ids.
    pub fn add_node(&mut self, node: DependencyNode) {
        let same_change = |n: &DependencyNode| {
            n.host == node.host
                && n.project == node.project
                && ((node.change_number != 0 && n.change_number == node.change_number)
                    || n.commit_sha == node.commit_sha)
        };
        if !self.nodes.iter().any(same_change) {
            self.nodes.push(node);
        }
    }

    pub fn add_parent_relation(&mut self, child_sha: &str, parent_sha: &str) {
        self.commit_parents.entry(child_sha.to_string()).or_default().push(parent_sha.to_string());
    }

    /// Orders `candidates` topologically along `edges` (restricted to `candidates`), picking the
    /// newest ready node first. Nodes on a cycle are left out.
    fn topo_order<'e>(
        &self,
        candidates: &[usize],
        edges: impl Iterator<Item = &'e (usize, usize)>,
    ) -> Vec<usize> {
        let n = self.nodes.len();
        let mut in_set = vec![false; n];
        for &c in candidates {
            in_set[c] = true;
        }
        let mut in_degree = vec![0usize; n];
        let mut adj = vec![Vec::new(); n];
        for &(u, v) in edges {
            if in_set[u] && in_set[v] {
                adj[u].push(v);
                in_degree[v] += 1;
            }
        }
        // Priority queue ordered by timestamp descending (newest commit first).
        let mut heap: BinaryHeap<(i64, usize)> = candidates
            .iter()
            .filter(|&&c| in_degree[c] == 0)
            .map(|&c| (self.nodes[c].timestamp, c))
            .collect();
        let mut ordered = Vec::new();
        while let Some((_, u)) = heap.pop() {
            ordered.push(u);
            for &v in &adj[u] {
                in_degree[v] -= 1;
                if in_degree[v] == 0 {
                    heap.push((self.nodes[v].timestamp, v));
                }
            }
        }
        ordered
    }

    /// Computes the revert plan, returning the planned revert commits in execution order.
    ///
    /// Changes involved in a non-atomic dependency cycle are still ordered (git descendants
    /// before their ancestors) so the plan can be reported, but `compute_revert_waves` rejects
    /// such plans.
    pub fn compute_revert_plan(&self) -> Result<Vec<PlannedRevert>> {
        if self.nodes.is_empty() {
            return Ok(Vec::new());
        }

        let items: Vec<EdgeItem<'_>> = self.nodes.iter().map(EdgeItem::from).collect();
        let edges = Edges::compute(&items, &self.commit_parents);
        let atomic_pairs = edges.atomic_pairs();

        // Revert order is the reverse of submission order. Atomic pairs are reverted together,
        // so their mutual edges do not constrain the order.
        let order_edges: Vec<(usize, usize)> = edges
            .git
            .iter()
            .chain(
                edges
                    .consistent_dep()
                    .filter(|&&(u, v)| !atomic_pairs.contains(&(u.min(v), u.max(v)))),
            )
            .copied()
            .collect();
        let all: Vec<usize> = (0..self.nodes.len()).collect();
        let mut ordered = self.topo_order(&all, order_edges.iter());

        // Nodes on a dependency cycle: order them by git ancestry alone so that intra-repo
        // descendants are always simulated before their ancestors.
        if ordered.len() < self.nodes.len() {
            let placed: HashSet<usize> = ordered.iter().copied().collect();
            let unplaced: Vec<usize> =
                all.iter().copied().filter(|i| !placed.contains(i)).collect();
            let by_git = self.topo_order(&unplaced, edges.git.iter());
            let placed_by_git: HashSet<usize> = by_git.iter().copied().collect();
            ordered.extend(by_git);
            // Git ancestry itself can only be cyclic for inconsistent input; keep those as-is.
            ordered.extend(unplaced.into_iter().filter(|i| !placed_by_git.contains(i)));
        }

        // Atomic partners: Rev(u) and Rev(v) must remain linked via Depends-on footers.
        // One-way dependencies need no footer: waves guarantee Rev(u) merges before Rev(v).
        let mut atomic_partners: HashMap<usize, Vec<DependsOnKey>> = HashMap::new();
        for &(u, v) in &atomic_pairs {
            atomic_partners.entry(u).or_default().push(items[v].key());
            atomic_partners.entry(v).or_default().push(items[u].key());
        }

        Ok(ordered
            .into_iter()
            .map(|idx| {
                let node = &self.nodes[idx];
                let mut partners = atomic_partners.remove(&idx).unwrap_or_default();
                partners.sort();
                partners.dedup();
                PlannedRevert {
                    original_change_id: node.change_id.clone(),
                    original_change_number: node.change_number,
                    submission_id: node.submission_id.clone(),
                    original_commit_sha: node.commit_sha.clone(),
                    project: node.project.clone(),
                    host: node.host.clone(),
                    repo_path: node.repo_path.clone(),
                    branch: node.branch.clone(),
                    subject: node.subject.clone(),
                    raw_message: node.raw_message.clone(),
                    original_depends_on: node.original_depends_on.clone(),
                    atomic_partners: partners,
                }
            })
            .collect())
    }

    /// Partitions the planned reverts into staged execution waves.
    ///
    /// Changes sharing a `(host, repo_path, branch, submission_id)` are grouped into a single
    /// [`SubmissionCluster`]. Clusters linked by atomic (mutual) dependencies, and clusters of
    /// the same Gerrit submission in different repositories, are always scheduled in the same
    /// wave. Returns an error if the cluster graph contains any other cycle.
    pub fn compute_revert_waves(&self, planned: &[PlannedRevert]) -> Result<Vec<RevertWave>> {
        if planned.is_empty() {
            return Ok(Vec::new());
        }

        // 1. Cluster planned reverts by (host, repo_path, branch, submission_id), preserving the
        //    order of first appearance. Changes without a submission_id each form their own
        //    cluster.
        let mut clusters: Vec<SubmissionCluster> = Vec::new();
        let mut cluster_of: Vec<usize> = Vec::with_capacity(planned.len());
        let mut cluster_keys: HashMap<(&str, &Path, &str, &str), usize> = HashMap::new();
        for item in planned {
            let key = item.submission_id.as_deref().map(|sid| {
                (item.host.as_str(), item.repo_path.as_path(), item.branch.as_str(), sid)
            });
            match key.and_then(|k| cluster_keys.get(&k).copied()) {
                Some(c_idx) => {
                    clusters[c_idx].changes.push(item.clone());
                    cluster_of.push(c_idx);
                }
                None => {
                    if let Some(k) = key {
                        cluster_keys.insert(k, clusters.len());
                    }
                    cluster_of.push(clusters.len());
                    clusters.push(SubmissionCluster {
                        repo_path: item.repo_path.clone(),
                        project: item.project.clone(),
                        host: item.host.clone(),
                        submission_id: item.submission_id.clone(),
                        changes: vec![item.clone()],
                    });
                }
            }
        }

        // 2. Build the cluster dependency graph. Edge (u, v): cluster u must be reverted
        //    before cluster v.
        let items: Vec<EdgeItem<'_>> = planned.iter().map(EdgeItem::from).collect();
        let edges = Edges::compute(&items, &self.commit_parents);
        let cluster_revert_edges: HashSet<(usize, usize)> = edges
            .git
            .iter()
            .chain(edges.consistent_dep())
            .map(|&(u, v)| (cluster_of[u], cluster_of[v]))
            .filter(|(cu, cv)| cu != cv)
            .collect();

        // 3. Merge clusters that must share a wave: atomic partners, and clusters of the same
        //    Gerrit submission (reverted together via `revert_submission`).
        let mut groups = DisjointSet::new(clusters.len());
        let mut key_to_items: HashMap<DependsOnKey, Vec<usize>> = HashMap::new();
        for (i, item) in planned.iter().enumerate() {
            key_to_items.entry(item.depends_on_key()).or_default().push(i);
        }
        for (i, item) in planned.iter().enumerate() {
            for partner in &item.atomic_partners {
                let Some(candidates) = key_to_items.get(partner) else { continue };
                for p in select_candidates(candidates, i, |c| planned[c].branch == item.branch) {
                    groups.union(cluster_of[i], cluster_of[p]);
                }
            }
        }
        let mut submission_cluster: HashMap<(&str, &str), usize> = HashMap::new();
        for (c_idx, cluster) in clusters.iter().enumerate() {
            if let Some(sid) = cluster.submission_id.as_deref() {
                match submission_cluster.get(&(cluster.host.as_str(), sid)) {
                    Some(&first) => groups.union(first, c_idx),
                    None => {
                        submission_cluster.insert((cluster.host.as_str(), sid), c_idx);
                    }
                }
            }
        }
        let group_of: Vec<usize> = (0..clusters.len()).map(|i| groups.find(i)).collect();

        // 4. Compute topological waves over groups via Kahn's algorithm with longest-path leveling.
        let mut group_edges: HashSet<(usize, usize)> = HashSet::new();
        for &(u, v) in &cluster_revert_edges {
            let (gu, gv) = (group_of[u], group_of[v]);
            if gu != gv {
                group_edges.insert((gu, gv));
            }
        }
        let n = clusters.len();
        let mut in_degree = vec![0usize; n];
        let mut adj = vec![Vec::new(); n];
        for &(gu, gv) in &group_edges {
            adj[gu].push(gv);
            in_degree[gv] += 1;
        }
        let group_roots: HashSet<usize> = group_of.iter().copied().collect();
        let mut queue: VecDeque<usize> = {
            let mut roots: Vec<usize> =
                group_roots.iter().copied().filter(|&g| in_degree[g] == 0).collect();
            roots.sort_unstable();
            roots.into()
        };
        let mut group_levels = vec![0usize; n];
        let mut processed = 0;
        while let Some(g) = queue.pop_front() {
            processed += 1;
            for &next in &adj[g] {
                group_levels[next] = group_levels[next].max(group_levels[g] + 1);
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }
        if processed != group_roots.len() {
            let stuck: Vec<String> = (0..n)
                .filter(|&i| in_degree[group_of[i]] > 0)
                .flat_map(|i| clusters[i].changes.iter().map(|c| c.original_change_id.clone()))
                .collect();
            return Err(anyhow!(
                "Cyclic dependency detected between revert clusters involving: {}. Please revert these changes manually.",
                stuck.join(", ")
            ));
        }

        let cluster_levels: Vec<usize> = group_of.iter().map(|&g| group_levels[g]).collect();
        let max_level = cluster_levels.iter().copied().max().unwrap_or(0);
        let mut waves = Vec::new();
        for lvl in 0..=max_level {
            let level_clusters: Vec<SubmissionCluster> = clusters
                .iter()
                .enumerate()
                .filter(|(idx, _)| cluster_levels[*idx] == lvl)
                .map(|(_, c)| c.clone())
                .collect();

            if !level_clusters.is_empty() {
                waves.push(RevertWave { wave_index: waves.len(), clusters: level_clusters });
            }
        }

        Ok(waves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUCHSIA_HOST: &str = "fuchsia-review.googlesource.com";
    const INTERNAL_HOST: &str = "turquoise-internal-review.googlesource.com";

    fn node(
        change_id: &str,
        submission_id: &str,
        sha: &str,
        host: &str,
        repo: &str,
        depends_on: &[&str],
        timestamp: i64,
    ) -> DependencyNode {
        DependencyNode {
            change_id: change_id.into(),
            change_number: 0,
            submission_id: Some(submission_id.into()),
            commit_sha: sha.into(),
            project: repo.trim_start_matches('/').into(),
            host: host.into(),
            repo_path: PathBuf::from(repo),
            branch: "main".into(),
            subject: format!("Change {change_id}"),
            raw_message: format!("Change {change_id}\n\nChange-Id: {change_id}"),
            original_depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
            timestamp,
        }
    }

    fn key(prefix: &str, change_id: &str) -> DependsOnKey {
        (prefix.to_string(), change_id.to_string())
    }

    fn wave_change_ids(waves: &[RevertWave]) -> Vec<Vec<Vec<String>>> {
        waves
            .iter()
            .map(|w| {
                w.clusters
                    .iter()
                    .map(|c| c.changes.iter().map(|ch| ch.original_change_id.clone()).collect())
                    .collect()
            })
            .collect()
    }

    fn plan_order(plan: &[PlannedRevert]) -> Vec<&str> {
        plan.iter().map(|p| p.original_change_id.as_str()).collect()
    }

    #[fuchsia::test]
    fn test_compute_revert_plan_single_change() {
        let mut builder = RevertGraphBuilder::new();
        builder.add_node(DependencyNode {
            change_id: "I1111111111111111111111111111111111111111".into(),
            change_number: 1001,
            submission_id: Some("sub_1".into()),
            commit_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            project: "fuchsia".into(),
            host: FUCHSIA_HOST.into(),
            repo_path: PathBuf::from("/path/to/fuchsia"),
            branch: "main".into(),
            subject: "Fix issue A".into(),
            raw_message: "Fix issue A\n\nChange-Id: I1111111111111111111111111111111111111111"
                .into(),
            original_depends_on: Vec::new(),
            timestamp: 100,
        });

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].original_commit_sha, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(plan[0].original_change_number, 1001);
        assert_eq!(plan[0].branch, "main");
        assert!(plan[0].atomic_partners.is_empty());
        assert_eq!(
            plan[0].depends_on_key(),
            key("fuchsia", "I1111111111111111111111111111111111111111")
        );
        let message = plan[0].revert_message("Tree closure", &[]);
        assert!(message.starts_with("Revert \"Fix issue A\"\n\n"), "{message}");
        assert!(!message.contains("\nChange-Id: "), "{message}");

        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 1);
        assert_eq!(waves[0].clusters[0].changes.len(), 1);
    }

    #[fuchsia::test]
    fn test_compute_revert_plan_intra_repo_stack() {
        let mut builder = RevertGraphBuilder::new();
        let base_sha = "1111111111111111111111111111111111111111";
        let top_sha = "2222222222222222222222222222222222222222";

        builder.add_node(node("I_base", "sub_stack", base_sha, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_top", "sub_stack", top_sha, FUCHSIA_HOST, "/f", &[], 200));

        // Top is child of Base
        builder.add_parent_relation(top_sha, base_sha);

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan.len(), 2);
        // Top must be reverted first, then Base
        assert_eq!(plan[0].original_commit_sha, top_sha);
        assert_eq!(plan[1].original_commit_sha, base_sha);

        // Same submission_id -> Single cluster in Wave 0 with both changes
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 1);
        assert_eq!(waves[0].clusters[0].changes.len(), 2);
    }

    #[fuchsia::test]
    fn test_compute_revert_waves_multi_submission() {
        let mut builder = RevertGraphBuilder::new();
        let base_sha = "1111111111111111111111111111111111111111";
        let top_sha = "2222222222222222222222222222222222222222";

        builder.add_node(node("I_base", "sub_1", base_sha, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_top", "sub_2", top_sha, FUCHSIA_HOST, "/f", &[], 200));
        builder.add_parent_relation(top_sha, base_sha);

        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        // Different submission_ids with parent relation -> 2 waves
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].wave_index, 0);
        assert_eq!(waves[1].wave_index, 1);
        assert_eq!(waves[0].clusters[0].changes[0].original_commit_sha, top_sha);
        assert_eq!(waves[1].clusters[0].changes[0].original_commit_sha, base_sha);
    }

    #[fuchsia::test]
    fn test_atomic_cross_repo_pair_stays_in_one_wave() {
        let mut builder = RevertGraphBuilder::new();
        let a_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let x_sha = "cccccccccccccccccccccccccccccccccccccccc";
        // A (fuchsia) <-> B (vendor/google) are an atomic cross-repo pair.
        builder.add_node(node(
            "I_a",
            "sub_a",
            a_sha,
            FUCHSIA_HOST,
            "/f",
            &["turquoise-internal:I_b"],
            100,
        ));
        builder.add_node(node(
            "I_b",
            "sub_b",
            b_sha,
            INTERNAL_HOST,
            "/vendor/google",
            &["fuchsia:I_a"],
            100,
        ));
        // X landed on top of A in fuchsia in a separate submission.
        builder.add_node(node("I_x", "sub_x", x_sha, FUCHSIA_HOST, "/f", &[], 300));
        builder.add_parent_relation(x_sha, a_sha);

        let plan = builder.compute_revert_plan().unwrap();
        let a = plan.iter().find(|p| p.original_change_id == "I_a").unwrap();
        let b = plan.iter().find(|p| p.original_change_id == "I_b").unwrap();
        assert_eq!(a.atomic_partners, vec![key("turquoise-internal", "I_b")]);
        assert_eq!(b.atomic_partners, vec![key("fuchsia", "I_a")]);

        let waves = builder.compute_revert_waves(&plan).unwrap();
        let ids = wave_change_ids(&waves);
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(ids[0], vec![vec!["I_x".to_string()]]);
        let mut second: Vec<String> = ids[1].iter().flatten().cloned().collect();
        second.sort();
        assert_eq!(second, vec!["I_a".to_string(), "I_b".to_string()]);
    }

    #[fuchsia::test]
    fn test_atomic_pair_alone_is_single_wave() {
        let mut builder = RevertGraphBuilder::new();
        builder.add_node(node(
            "I_a",
            "sub_a",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FUCHSIA_HOST,
            "/f",
            &["turquoise-internal:I_b"],
            100,
        ));
        builder.add_node(node(
            "I_b",
            "sub_b",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            INTERNAL_HOST,
            "/vendor/google",
            &["fuchsia:I_a"],
            100,
        ));
        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 2);
    }

    #[fuchsia::test]
    fn test_non_contiguous_submission_forms_single_cluster() {
        let mut builder = RevertGraphBuilder::new();
        let s1 = "1111111111111111111111111111111111111111";
        let y = "2222222222222222222222222222222222222222";
        let s2 = "3333333333333333333333333333333333333333";
        builder.add_node(node("I_s1", "sub_s", s1, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_y", "sub_y", y, INTERNAL_HOST, "/vendor/google", &[], 200));
        builder.add_node(node("I_s2", "sub_s", s2, FUCHSIA_HOST, "/f", &[], 300));
        builder.add_parent_relation(s2, s1);

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan_order(&plan), vec!["I_s2", "I_y", "I_s1"]);

        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        let ids = wave_change_ids(&waves);
        assert_eq!(
            ids[0],
            vec![vec!["I_s2".to_string(), "I_s1".to_string()], vec!["I_y".to_string()],]
        );
    }

    #[fuchsia::test]
    fn test_same_submission_different_hosts_not_merged() {
        let mut builder = RevertGraphBuilder::new();
        builder.add_node(node(
            "I_a",
            "sub",
            "1111111111111111111111111111111111111111",
            FUCHSIA_HOST,
            "/f",
            &[],
            100,
        ));
        builder.add_node(node(
            "I_b",
            "sub",
            "2222222222222222222222222222222222222222",
            INTERNAL_HOST,
            "/f",
            &[],
            200,
        ));
        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 2);
    }

    #[fuchsia::test]
    fn test_out_of_order_cluster_edges_level_correctly() {
        let mut builder = RevertGraphBuilder::new();
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let c = "3333333333333333333333333333333333333333";
        builder.add_node(node("I_a", "sub_a", a, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_b", "sub_b", b, FUCHSIA_HOST, "/f", &[], 200));
        builder.add_node(node("I_c", "sub_c", c, FUCHSIA_HOST, "/f", &[], 300));
        builder.add_parent_relation(b, a);
        builder.add_parent_relation(c, b);

        let mut plan = builder.compute_revert_plan().unwrap();
        // Reverse the plan so every cluster edge (u, v) has u > v.
        plan.reverse();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        let ids = wave_change_ids(&waves);
        assert_eq!(
            ids,
            vec![
                vec![vec!["I_c".to_string()]],
                vec![vec!["I_b".to_string()]],
                vec![vec!["I_a".to_string()]],
            ]
        );
    }

    #[fuchsia::test]
    fn test_cluster_cycle_is_error() {
        let mut builder = RevertGraphBuilder::new();
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let c = "3333333333333333333333333333333333333333";
        builder.add_node(node("I_a", "s1", a, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_b", "s2", b, FUCHSIA_HOST, "/f", &[], 200));
        builder.add_node(node("I_c", "s3", c, FUCHSIA_HOST, "/f", &[], 300));
        builder.add_parent_relation(b, a);
        builder.add_parent_relation(c, b);
        builder.add_parent_relation(a, c);

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan.len(), 3);
        let err = builder.compute_revert_waves(&plan).unwrap_err();
        assert!(format!("{err}").contains("Cyclic dependency"), "{err}");
    }

    #[fuchsia::test]
    fn test_host_prefix() {
        assert_eq!(host_prefix(INTERNAL_HOST, "foo"), "turquoise-internal");
        assert_eq!(host_prefix(FUCHSIA_HOST, "vendor/google"), "turquoise-internal");
        assert_eq!(host_prefix("partner-fuchsia-review.googlesource.com", "x"), "partner-fuchsia");
        assert_eq!(host_prefix(FUCHSIA_HOST, "fuchsia"), "fuchsia");
    }

    #[fuchsia::test]
    fn test_depends_on_key_parsing() {
        assert_eq!(
            parse_depends_on_key("turquoise-internal: I_b", FUCHSIA_HOST, "fuchsia"),
            key("turquoise-internal", "I_b")
        );
        assert_eq!(
            parse_depends_on_key("I_b", INTERNAL_HOST, "vendor/google"),
            key("turquoise-internal", "I_b")
        );
        assert_eq!(parse_depends_on_key(" I_b ", FUCHSIA_HOST, "fuchsia"), key("fuchsia", "I_b"));
        assert_eq!(parse_depends_on_key("fuchsia:I_b ", INTERNAL_HOST, "x"), key("fuchsia", "I_b"));
        assert_eq!(
            parse_depends_on_key("partner-fuchsia:I_c", FUCHSIA_HOST, "fuchsia"),
            key("partner-fuchsia", "I_c")
        );
        assert_eq!(depends_on_key(FUCHSIA_HOST, "fuchsia", " I_a "), key("fuchsia", "I_a"));
    }

    #[fuchsia::test]
    fn test_add_node_identity() {
        let sha_a = "1111111111111111111111111111111111111111";
        let sha_b = "2222222222222222222222222222222222222222";
        let mut builder = RevertGraphBuilder::new();
        // The same Change-Id on another host (e.g. a mirror or cherry-pick) is a distinct change.
        builder.add_node(node("I_x", "s1", sha_a, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_x", "s2", sha_b, INTERNAL_HOST, "/f", &[], 100));
        assert_eq!(builder.nodes.len(), 2);
        // Re-adding the same (host, project, commit) is a no-op.
        builder.add_node(node("I_x", "s1", sha_a, FUCHSIA_HOST, "/f", &[], 100));
        assert_eq!(builder.nodes.len(), 2);

        // Changes with a number are identified by (host, project, number).
        let mut builder = RevertGraphBuilder::new();
        let mut n1 = node("I_y", "s1", sha_a, FUCHSIA_HOST, "/f", &[], 100);
        n1.change_number = 42;
        let mut n2 = node("I_y2", "s1", sha_b, FUCHSIA_HOST, "/f", &[], 100);
        n2.change_number = 42;
        builder.add_node(n1.clone());
        builder.add_node(n2);
        assert_eq!(builder.nodes.len(), 1);
        // The same Change-Id cherry-picked to another branch has another number and SHA.
        let mut n3 = node("I_y", "s3", sha_b, FUCHSIA_HOST, "/f", &[], 100);
        n3.change_number = 43;
        n3.branch = "releases/f30".into();
        builder.add_node(n3);
        assert_eq!(builder.nodes.len(), 2);
    }

    #[fuchsia::test]
    fn test_depends_on_with_space_after_prefix_links() {
        let mut builder = RevertGraphBuilder::new();
        builder.add_node(node(
            "I_a",
            "sub_a",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FUCHSIA_HOST,
            "/f",
            &["turquoise-internal: I_b"],
            100,
        ));
        builder.add_node(node(
            "I_b",
            "sub_b",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            INTERNAL_HOST,
            "/vendor/google",
            &["fuchsia: I_a"],
            200,
        ));
        let plan = builder.compute_revert_plan().unwrap();
        let a = plan.iter().find(|p| p.original_change_id == "I_a").unwrap();
        assert_eq!(a.atomic_partners, vec![key("turquoise-internal", "I_b")]);
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 2);
    }

    #[fuchsia::test]
    fn test_depends_on_is_host_scoped() {
        let mut builder = RevertGraphBuilder::new();
        let a = "1111111111111111111111111111111111111111";
        let x_public = "2222222222222222222222222222222222222222";
        let x_internal = "3333333333333333333333333333333333333333";
        builder.add_node(node("I_a", "s_a", a, FUCHSIA_HOST, "/f", &["fuchsia:I_x"], 300));
        builder.add_node(node("I_x", "s_xp", x_public, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_x", "s_xi", x_internal, INTERNAL_HOST, "/v", &[], 200));

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan.len(), 3);
        let waves = builder.compute_revert_waves(&plan).unwrap();
        let hosts: Vec<Vec<(String, String)>> = waves
            .iter()
            .map(|w| {
                let mut v: Vec<(String, String)> = w
                    .clusters
                    .iter()
                    .flat_map(|c| &c.changes)
                    .map(|ch| (ch.original_change_id.clone(), ch.host.clone()))
                    .collect();
                v.sort();
                v
            })
            .collect();
        assert_eq!(
            hosts,
            vec![
                vec![
                    ("I_a".to_string(), FUCHSIA_HOST.to_string()),
                    ("I_x".to_string(), INTERNAL_HOST.to_string()),
                ],
                vec![("I_x".to_string(), FUCHSIA_HOST.to_string())],
            ]
        );
    }

    #[fuchsia::test]
    fn test_depends_on_cycle_keeps_git_edge() {
        let mut builder = RevertGraphBuilder::new();
        let base = "1111111111111111111111111111111111111111";
        let child = "2222222222222222222222222222222222222222";
        // `I_base` declares a Depends-on on its own git descendant `I_child`.
        builder.add_node(node("I_base", "sub_1", base, FUCHSIA_HOST, "/f", &["I_child"], 100));
        builder.add_node(node("I_child", "sub_2", child, FUCHSIA_HOST, "/f", &[], 200));
        builder.add_parent_relation(child, base);

        let plan = builder.compute_revert_plan().unwrap();
        assert_eq!(plan_order(&plan), vec!["I_child", "I_base"]);
        assert!(plan.iter().all(|p| p.atomic_partners.is_empty()));
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(
            wave_change_ids(&waves),
            vec![vec![vec!["I_child".to_string()]], vec![vec!["I_base".to_string()]]]
        );
    }

    #[fuchsia::test]
    fn test_three_cycle_orders_git_children_first() {
        let mut builder = RevertGraphBuilder::new();
        let base = "1111111111111111111111111111111111111111";
        let top = "2222222222222222222222222222222222222222";
        let v = "3333333333333333333333333333333333333333";
        // base -> V -> top -> base, where top is a git child of base.
        builder.add_node(node(
            "I_base",
            "sub_1",
            base,
            FUCHSIA_HOST,
            "/f",
            &["turquoise-internal:I_v"],
            300,
        ));
        builder.add_node(node("I_top", "sub_2", top, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_v", "sub_v", v, INTERNAL_HOST, "/v", &["fuchsia:I_top"], 200));
        builder.add_parent_relation(top, base);

        let plan = builder.compute_revert_plan().unwrap();
        let order = plan_order(&plan);
        assert_eq!(order.len(), 3);
        let pos = |id: &str| order.iter().position(|o| *o == id).unwrap();
        assert!(pos("I_top") < pos("I_base"), "{order:?}");
        assert!(plan.iter().all(|p| p.atomic_partners.is_empty()));

        let err = builder.compute_revert_waves(&plan).unwrap_err();
        assert!(format!("{err}").contains("Cyclic dependency"), "{err}");
    }

    #[fuchsia::test]
    fn test_interleaved_two_cluster_cycle_is_error() {
        let mut builder = RevertGraphBuilder::new();
        let s1 = "1111111111111111111111111111111111111111";
        let s2 = "2222222222222222222222222222222222222222";
        let b = "3333333333333333333333333333333333333333";
        // s1 <- b <- s2 with s1 and s2 in one submission: neither cluster can go first.
        builder.add_node(node("I_s1", "sub_1", s1, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_b", "sub_2", b, INTERNAL_HOST, "/v", &["fuchsia:I_s1"], 200));
        builder.add_node(node(
            "I_s2",
            "sub_1",
            s2,
            FUCHSIA_HOST,
            "/f",
            &["turquoise-internal:I_b"],
            300,
        ));

        let plan = builder.compute_revert_plan().unwrap();
        assert!(plan.iter().all(|p| p.atomic_partners.is_empty()));
        let err = builder.compute_revert_waves(&plan).unwrap_err();
        assert!(format!("{err}").contains("Cyclic dependency"), "{err}");
    }

    #[fuchsia::test]
    fn test_same_submission_across_repos_shares_wave() {
        let mut builder = RevertGraphBuilder::new();
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        builder.add_node(node("I_a", "sub_x", a, FUCHSIA_HOST, "/f", &[], 100));
        builder.add_node(node("I_b", "sub_x", b, FUCHSIA_HOST, "/integration", &["I_a"], 200));

        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1, "{:?}", wave_change_ids(&waves));
        assert_eq!(waves[0].clusters.len(), 2);
    }

    #[fuchsia::test]
    fn test_cluster_key_includes_branch() {
        let mut builder = RevertGraphBuilder::new();
        let main_sha = "1111111111111111111111111111111111111111";
        let release_sha = "2222222222222222222222222222222222222222";
        builder.add_node(node("I_m", "sub", main_sha, FUCHSIA_HOST, "/f", &[], 100));
        let mut release = node("I_r", "sub", release_sha, FUCHSIA_HOST, "/f", &[], 200);
        release.branch = "releases/f30".into();
        builder.add_node(release);
        // Ancestry across branches does not order reverts on different branches.
        builder.add_parent_relation(release_sha, main_sha);

        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].clusters.len(), 2);
    }

    #[fuchsia::test]
    fn test_ambiguous_depends_on_prefers_same_branch() {
        let mut builder = RevertGraphBuilder::new();
        let a = "1111111111111111111111111111111111111111";
        let x_main = "2222222222222222222222222222222222222222";
        let x_release = "3333333333333333333333333333333333333333";
        let mut a_node = node("I_a", "s_a", a, FUCHSIA_HOST, "/f", &["I_x"], 300);
        a_node.branch = "releases/f30".into();
        builder.add_node(a_node);
        builder.add_node(node("I_x", "s_xm", x_main, FUCHSIA_HOST, "/f", &[], 100));
        let mut x_rel = node("I_x", "s_xr", x_release, FUCHSIA_HOST, "/f", &[], 200);
        x_rel.branch = "releases/f30".into();
        builder.add_node(x_rel);

        let plan = builder.compute_revert_plan().unwrap();
        let waves = builder.compute_revert_waves(&plan).unwrap();
        let shas: Vec<Vec<&str>> = waves
            .iter()
            .map(|w| {
                let mut v: Vec<&str> = w
                    .clusters
                    .iter()
                    .flat_map(|c| &c.changes)
                    .map(|ch| ch.original_commit_sha.as_str())
                    .collect();
                v.sort();
                v
            })
            .collect();
        assert_eq!(shas, vec![vec![a, x_main], vec![x_release]]);
    }
}
