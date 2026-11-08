# Commit message bazel migration target footer enforcement

- **Learned from:** human review on CL I1022527aaae5aabdcf40b39105cb8dc87047e9ea, patchset 1 (task `ffx-product-leaves-a`)
- **Date:** 2026-10-30
- **Changed:** `checks/build_only_scope.sh`, `checks/commit_message_format.sh`

## Root cause

The commit was authored before Bazel-Migration-Target footer enforcement landed,
and unresolved git conflict markers in checks/commit_message_format.sh and
checks/build_only_scope.sh caused Python to exit with a SyntaxError before
reporting missing_bazel_migration_target_footer findings.

## Why not a one-off fix

Adding Bazel-Migration-Target footers only to the current commit message leaves
checks/commit_message_format.sh and checks/build_only_scope.sh broken by syntax
errors, preventing deterministic detection of missing migration target footers
across all future migration tasks.
