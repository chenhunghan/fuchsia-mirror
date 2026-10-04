# Dangling parent gn package dep base target verification

- **Learned from:** inner loop friction on a local task (task `time_pretty`)
- **Date:** 2026-10-02
- **Changed:** `checks/migration_sanity.sh`

## Root cause

In checks/migration_sanity.sh, the dangling_parent_gn_package_dep check ran
whenever any fx_package (including a test fx_package with test_components) was
present in BUILD.bazel and flagged any parent BUILD.gn reference to the child
directory whose target name was not in the current child BUILD.gn, without
checking whether that target name actually existed in the child BUILD.gn at
PLANTER_CHANGE_BASE and was deleted by the migration.

## Why not a one-off fix

Exempting a single directory or parent group would leave migration_sanity
vulnerable to false positives across any dual-build migration that adds a test
fx_package when a parent BUILD.gn has a pre-existing unconfigured shorthand
reference to a target that never existed in the child BUILD.gn.
