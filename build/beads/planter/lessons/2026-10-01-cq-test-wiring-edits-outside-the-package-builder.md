# CQ test-wiring edits outside the package: builder bundle groups (tests barrier / bazel target test suite barrier) needed so exported bazel test suite tests reach tests.json only with their product bundle

- **Learned from:** inner loop friction on CL I8a72d619c70f2f7790c6c233a5712af117663649, patchset 6 (task `starnix-wave-next`)
- **Date:** 2026-10-01
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

bazel_minimality's scope allowlist covered bundles/assembly/ but not the rest of
bundles/, although the coder prompt and task scope permit //bundles/** edits to
wire migrated tests into CQ. bazel_test_suite target tests ignore
product_bundle_test_group's tests_barrier, so a builder group needs a
bazel_target_test_suite_barrier. The check flagged that required edit as a
drive-by change, so the check and the scope rules contradicted each other.

## Why not a one-off fix

Any future migration that exports fx_test suites reached by builder bundle
groups without a default product bundle hits the same testsharder failure and
needs a bundles/ build-file edit. Waiving the finding for this one file would
leave the check contradicting the allowed scope on every such task.
