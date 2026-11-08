# Workspace filesystem write failure handling

- **Learned from:** inner loop friction on CL I461efaa803aff813001a6cc1b027ad2b68c1007b, patchset 1 (task `ffx-guest-subplugins-b`)
- **Date:** 2026-10-31
- **Changed:** `checks/tests_json_parity.sh`

## Root cause

checks/tests_json_parity.sh performed unguarded file creation and shutil.copy2
backups under <git dir>/planter-tests-json-parity and the build directory
without catching OSError, causing degraded FUSE/casfs mount write failures
(ENOENT/EROFS) to crash the check script with an unhandled FileNotFoundError
traceback (check_script_failure) instead of reporting a structured
workspace_write_error finding instructing the agent not to retry build-file
edits.

## Why not a one-off fix

Degraded casfs or FUSE mount write errors in the checkout, .git directory, or
build output directory are workspace-level filesystem failures that can strike
any migration slot regardless of target directory or Bazel rule type, so check
scripts that write backup or baseline files must validate writability and catch
OSError systemically across all tasks.
