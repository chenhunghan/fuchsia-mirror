# Stacked bazel dependency package and diffengine helper resolution

- **Learned from:** cq failure on CL I68edac7a5dea423969a428cb60e957eb3613258c, patchset 1 (task `ffx-product-pbms-subtools`)
- **Date:** 2026-10-29
- **Changed:** `checks/cq_reachability.sh`, `checks/gn_attr_parity.sh`

## Root cause

First, checks/gn_attr_parity.sh called find_planter_bin() inside
collect_diffengine_alignment_findings() without defining find_planter_bin(),
causing gn_attr_parity to crash with NameError on every run and blocking stacked
CQ-failure recovery turns. Second, checks/cq_reachability.sh's
stacked_bazel_dependency_package remediation ended with conflicting advice ('or
include <dep>/BUILD.bazel in this change if the parent CL is not stacked in
Gerrit') and ran unbounded git log --grep searches across the full origin/main
history when checking unmerged Depends on: Change-Ids.

## Why not a one-off fix

Fixing only a single task's commit or workspace state would leave
gn_attr_parity.sh crashing with NameError on every migration task and leave
cq_reachability.sh giving conflicting remediation or stalling on full-history
git log scans across all future stacked GN-to-Bazel migrations.
