# Workspace write error cascading commit and test footer findings

- **Learned from:** inner loop friction on CL I461efaa803aff813001a6cc1b027ad2b68c1007b, patchset 1 (task `ffx-guest-subplugins-b`)
- **Date:** 2026-10-31
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/tests_json_parity.sh`, `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

When a degraded FUSE/casfs workspace mount rejects non-empty file writes
(`workspace_write_error`), `build_verification` and `tests_json_parity`
instructed the coder to stop retrying `git commit --amend`, yet
`commit_message_format` (and `build_verification`'s pre-probe `Test:` footer
checks) had no `.git` writability guard during full runs and continued emitting
blocking commit-message findings instructing the coder to amend `HEAD`. In
addition, `workspace_write_error` remediations and `prompts/coder.md` only told
the coder to set `disputed_findings` to `[]` without clearing
`CoderReport.test_footers`, causing `test_verifier` to run `fx bazel query` on
unexecuted `test_footers` and emit spurious `missing_bazel_test_target` errors
when `main_build.py` failed to write to the build directory.

## Why not a one-off fix

Degraded FUSE/casfs mount write failures can occur during any migration task
across the repository after an initial commit exists; suppressing unamendable
commit-message findings when `.git` rejects writes during full check runs and
instructing coders to set `test_footers` to `[]` under `workspace_write_error`
eliminates contradictory `git commit --amend` instructions and cascading
`test_verifier` query failures across all future tasks.
