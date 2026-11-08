# Ancestor test suite discovery and tests json parity restoration

- **Learned from:** inner loop friction on CL I675c7334abab3dc5ec6183ac6ae11b3e3cfc8864, patchset 2 (task `src-performance-memory-batch-heapdump_vmo`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/tests_json_parity.sh`, `tools/run_checks.sh`

## Root cause

First, cq_reachability's ancestor_suite_covers_subtree unconditionally returned
False whenever any sibling directory under an intermediate parent still
contained a legacy per-package bazel_test_suite in BUILD.gn, even when an
ancestor BUILD.bazel test_suite (exported by a wired ancestor bazel_test_suite)
already existed and aggregated tests from the same subtree. This caused
cq_reachability to emit unwanted_ancestor_test_suite_hookup and demand a
per-package bazel_test_suite in BUILD.gn, directly contradicting
test_suite_layout/per_package_bazel_test_suite. Second, tests_json_parity did
not advance PLANTER_CHANGE_BASE across git rebases, ran concurrently with
build_verification and static checks in run_checks.sh while reverting
working-tree build files in place, and treated files already updated by
subsequent working-tree edits or rebases as persistent RestoreConflict errors
instead of cleaning up the journal.

## Why not a one-off fix

Exempting a single package or subsystem directory would leave the contradiction
between cq_reachability and test_suite_layout active for every partially
migrated subsystem where some older leaf packages still have per-package
bazel_test_suite targets while an ancestor area test_suite already exists, and
would leave tests_json_parity vulnerable to concurrent working-tree revert races
and post-rebase journal lockouts across all future migrations.
