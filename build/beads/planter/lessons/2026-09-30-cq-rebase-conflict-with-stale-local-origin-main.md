# Cq rebase conflict with stale local origin main ref

- **Learned from:** cq failure on [CL 1850658](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850658), patchset 3 (task `pkg-go-leaves`)
- **Date:** 2026-09-30
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

When resuming a migration change from a Gerrit patchset (`FETCH_HEAD`), the
local `refs/remotes/origin/main` tracking ref is not updated by `git fetch
origin refs/changes/...` and can remain days behind upstream `main` (even behind
`HEAD~1`). As a result, `cq_reachability.sh` computed `git merge-base
origin/main base_sha == origin_main` and skipped `git merge-tree --write-tree`,
while running `git rebase origin/main` without `git fetch origin main` was a
no-op (`Current branch HEAD is up to date`), leaving upstream conflicts in
`build/bazel2gn_verification_targets.gni` undetected until Gerrit CQ failed
across all builders at `checkout|jiri patch` (`Failed to rebase`). Additionally,
after `git rebase --continue` completed, `cq_reachability.sh` checked standalone
`.git/REBASE_HEAD` and used a pre-rebase `$PLANTER_CHANGE_BASE` for
`merge-base`, causing false positives post-rebase.

## Why not a one-off fix

Every Case 2 dual-build migration edits shared sorted verification target lists
(`build/bazel2gn_verification_targets.gni` or
`sdk/fidl/bazel2gn_verification_targets.gni`) that frequently advance on
upstream `main` while a CL is in review or CQ. Refreshing a stale
`refs/remotes/origin/main` ref inside `cq_reachability.sh` before running `git
merge-tree --write-tree` against `HEAD` and instructing the coder to run `git
fetch origin main && git rebase origin/main` prevents `Failed to rebase` CQ
failures across all future migration tasks.
