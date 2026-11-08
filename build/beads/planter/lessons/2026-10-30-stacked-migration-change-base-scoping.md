# Stacked migration change base scoping

- **Learned from:** inner loop friction on a local task (task `ffx-guest-tool`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

When a migration task included a parent package alongside remaining child
subpackages in target_dirs and ran on top of a stacked parent [bazel_migration]
commit that had migrated sibling child subpackages under that parent directory,
bazel_minimality.sh checked p.startswith(td + '/') against HEAD~1..HEAD files.
Because the sibling child subpackage paths started with the parent target
directory prefix, bazel_minimality.sh falsely treated HEAD as the current task's
commit, stepped change_base back to HEAD~1, and combined the parent commit's
migrated packages with the working tree's packages to trigger a false-positive
cl_scope_exceeded error.

## Why not a one-off fix

Exempting a single directory or raising the directory count threshold would fail
across any multi-wave stacked migration where child subpackages are migrated in
earlier commits before the parent umbrella package and remaining subpackages are
migrated in a subsequent commit.
