# Upstream origin/main rebase conflict detection on shared verification registries (bazel2gn verification targets.gni) before Gerrit CQ jiri patch

- **Learned from:** cq failure on [CL 1850715](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850715), patchset 2 (task `pkg-update-fidl`)
- **Date:** 2026-09-29
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

When an existing migration CL is checked out from Gerrit in a follow-up review
or CQ round, its parent commit remains at the original base commit while
concurrent migrations land on origin/main and modify shared central lists such
as //build/bazel2gn_verification_targets.gni. None of the existing checks
verified via git merge-tree whether the change still merges cleanly onto
origin/main, and run_checks.sh skipped cq_reachability when the working tree
matched the uploaded patchset, allowing conflicting patchsets to be uploaded and
fail jiri patch across all CQ builders.

## Why not a one-off fix

Rebasing only this single FIDL migration CL onto origin/main leaves every future
multi-round or concurrent dual-build migration touching
//build/bazel2gn_verification_targets.gni,
//sdk/fidl/bazel2gn_verification_targets.gni, or //bundles/assembly vulnerable
to uploading patchsets that conflict with origin/main and fail Gerrit CQ with
Failed to rebase.
