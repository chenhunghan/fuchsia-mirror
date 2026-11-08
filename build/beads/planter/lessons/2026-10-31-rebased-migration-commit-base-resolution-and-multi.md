# Rebased migration commit base resolution and multi-directory per-package check execution parity

- **Learned from:** inner loop friction on CL If360d6314b1adc3c4597bbbdcaea551770e5b024, patchset 14 (task `src-sys-run-test-suite`)
- **Date:** 2026-10-31
- **Changed:** `checks/commit_message_format.sh`, `checks/gn_attr_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/visibility_audit.sh`, `tools/run_checks.sh`

## Root cause

When a multi-directory migration task rebased HEAD onto origin/main while
leaving a child test subpackage unmigrated in BUILD.gn, Planter's
per-target-directory check runner invoked checks with PLANTER_TARGET_DIR set to
the unmigrated child directory and PLANTER_CHANGE_BASE pinned to the pre-rebase
commit SHA. In commit_message_format.sh and run_checks.sh, matches_subject_dir
required an exact directory match against the commit subject, causing
is_task_migration_head to return False for the child directory and preventing
advance_rebased_base from advancing base_rev to HEAD~1 (so commit_message_format
diffed against the pre-rebase patchset and falsely flagged co-migrated packages
as also_migrated_already_migrated). Similarly, gn_target_type_parity.sh,
gn_attr_parity.sh, and visibility_audit.sh used raw PLANTER_CHANGE_BASE without
advancing a pre-rebase or pre-amend base SHA to HEAD~1, preventing them from
emitting INFO deduplication coverage for template_parity, dep_alignment,
visibility_narrowed, and gn_unmodified on an unmigrated child directory.

## Why not a one-off fix

Hardcoding specific package paths or waiving individual findings would only mask
the failure for a single task; any future multi-directory GN-to-Bazel migration
that rebases onto origin/main or leaves a child test subpackage in BUILD.gn
would hit the same stale pre-rebase PLANTER_CHANGE_BASE comparison across
commit_message_format and the parity checks.
