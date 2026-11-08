# Workspace write error test footer remediation

- **Learned from:** inner loop friction on CL I461efaa803aff813001a6cc1b027ad2b68c1007b, patchset 1 (task `ffx-guest-subplugins-b`)
- **Date:** 2026-10-31
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/tests_json_parity.sh`, `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

When a degraded FUSE/casfs mount rejected non-empty file writes
(`workspace_write_error`), `build_verification.sh`, `tests_json_parity.sh`,
`commit_message_format.sh`, `tools/run_checks.sh`, and `prompts/coder.md`
instructed the coding agent to set `CoderReport.test_footers` to `[]` to avoid
`test_verifier` running `fx bazel query` against an unwritable build directory.
However, Planter's `test_verifier` unconditionally rejects an empty
`test_footers` list on non-empty migration CLs with `missing_test_footer`,
creating an unresolvable contradiction across coder rounds. Additionally,
`tests_json_parity.sh` called `os.makedirs(state_dir, exist_ok=True)` before
probing `.git` writability, leaving an empty directory behind when metadata
creation succeeded while file writes failed.

## Why not a one-off fix

Degraded FUSE/casfs workspace mounts (`workspace_write_error`) are
environment-level failures that can affect any Fuchsia GN-to-Bazel migration
task regardless of subsystem or target rule type. Fixing only a single package
directory cannot resolve conflicting `CoderReport.test_footers` instructions in
the check remediations; all check scripts and runner guidance must consistently
prescribe a target-free `Test:` command (`["fx host-tool shac check"]`) that
satisfies `test_verifier` without triggering target queries on an unwritable
mount.
