# Commit message bazel migration target footer enforcement

- **Learned from:** human review on CL I4d77863e5462a881b402992ae5450caffe381a6a, patchset 1 (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-30
- **Changed:** `checks/commit_message_format.sh`, `tools/run_checks.sh`

## Root cause

When a migration commit used a non-standard subject prefix (such as '[bazel]
Migrate ...' instead of '[bazel_migration] ...') or was rebased/amended off a
prior patchset commit, is_task_migration_head in checks/commit_message_format.sh
and tools/run_checks.sh failed to recognize HEAD as the task's migration commit,
skipping commit-message and Bazel-Migration-Target footer checks when
PLANTER_CHANGE_BASE defaulted to HEAD or diffing against a non-ancestor
pre-rebase revision. In addition, base_gn_names only inspected direct top-level
GN target names and post-sentinel generated targets, missing <name>_test unit
test targets produced by with_unit_tests = true on base GN targets or file-local
GN templates when BUILD.gn was removed or the test was skipped in bazel2gn.

## Why not a one-off fix

Adding Bazel-Migration-Target footers only to the single commit message in this
task would leave future single- and multi-directory migrations vulnerable
whenever a commit subject omits the '[bazel_migration]' prefix, after a
rebase/amend invalidates PLANTER_CHANGE_BASE, or when unit tests originate from
with_unit_tests = true or unrolled file-local GN templates.
