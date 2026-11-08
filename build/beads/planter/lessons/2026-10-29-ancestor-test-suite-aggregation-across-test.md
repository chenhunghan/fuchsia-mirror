# Ancestor test suite aggregation across test environments

- **Learned from:** human review on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 4 (task `src-diagnostics-triage`)
- **Date:** 2026-10-29
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`

## Root cause

Three interacting defects in checks/cq_reachability.sh prevented wiring migrated
Bazel tests into a newly added environment-specific test_suite (such as adding
host_tests when an ancestor area BUILD.bazel and BUILD.gn already aggregate
target_tests, or vice versa) and instead forced per-package bazel_test_suite
targets in leaf BUILD.gn files: (1) ancestor_suite_covers_subtree only treated a
newly added test_suite with no pre-existing members as valid when the entire
ancestor BUILD.bazel did not exist at the change base (not base_anc_src), rather
than checking whether that specific test_suite name was absent at the change
base (anc_suite not in base_bazel_suites_of(anc_pkg)), causing
unwanted_ancestor_test_suite_hookup to reject adding host_tests to an existing
ancestor BUILD.bazel that already defined target_tests; (2)
preferred_ancestor_area_suite called has_sibling_local_bazel_test_suite(td) with
an empty ancestor bound, walking up to top-level directories like src/ and
matching unrelated subsystem bazel_test_suite targets so it returned None across
src/**; and (3) preferred_ancestor_area_suite returned None whenever the
working-tree leaf BUILD.gn contained a bazel_test_suite or sentinel, making
bypassed_ancestor_test_suite unreachable whenever a leaf package declared a
local bazel_test_suite.

## Why not a one-off fix

Fixing only a single leaf package would leave every other Fuchsia subsystem
whose area-level BUILD.bazel and BUILD.gn currently define only target_tests (or
only host_tests) vulnerable to both failure modes: checks/cq_reachability.sh
would continue to reject adding the companion host_tests or target_tests suite
in //<area>/BUILD.bazel via unwanted_ancestor_test_suite_hookup, while silently
accepting redundant per-package bazel_test_suite bridges in leaf BUILD.gn files
across both Case 1 and Case 2 migrations.
