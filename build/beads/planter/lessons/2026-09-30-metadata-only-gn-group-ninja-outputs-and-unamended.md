# Metadata-Only GN Group Ninja Outputs and Unamended Worktree Commit Message Validation

- **Learned from:** inner loop friction on CL I414a544b4af26cbb04f58ae5a4408a44c00e4b8f, patchset 2 (task `netstack-src-lib`)
- **Date:** 2026-09-30
- **Changed:** `checks/build_verification.sh`

## Root cause

build_verification.sh included every key from ninja_outputs.json in
load_toolchains() without checking that its output path list was non-empty,
causing metadata-only GN groups (such as group("tests") wrapping only
bazel_test_suite) with empty output lists [] to be passed to fx build, which
gn_ninja_outputs.py maps to an empty path string that fails ninja with 'ninja:
error: empty path'. Additionally, build_verification.sh validated HEAD commit
message Test: footers even when the working tree had uncommitted follow-up round
edits (where workspace rules forbid running git commit --amend in-round and
Planter updates the commit message from CoderReport after checks pass).

## Why not a one-off fix

Any dual-build package whose GN group("tests") wraps only a metadata-only target
such as bazel_test_suite produces an empty output list in ninja_outputs.json,
and any multi-round task in a workspace that forbids in-round git commit --amend
leaves uncommitted worktree edits on top of the prior round's HEAD commit
message until the round completes. Filtering out empty ninja_outputs.json
entries and skipping stale HEAD footer checks when the worktree has uncommitted
edits fixes both failure modes across all future migrations.
