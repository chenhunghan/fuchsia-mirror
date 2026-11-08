# Host only gn assert and ancestor test suite subtree coverage

- **Learned from:** inner loop friction on CL I4d77863e5462a881b402992ae5450caffe381a6a, patchset 1 (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/lint_config_parity.sh`, `checks/manifest.json`

## Root cause

First, lint_config_parity unconditionally flagged `# @bazel2gn:skip` on
`target_compatible_with = HOST_OS_CONSTRAINTS` even when the package's BUILD.gn
restricts the entire file to the host toolchain via a top-level `assert(is_host,
...)` without wrapping pre-migration targets in `if (is_host)`, where emitting
`if (is_host)` around a generated test target causes shac
`check_bazel_test_parity.py` to see a polarity transition from unguarded
(device) to `if (is_host)` (host-only) and fail with Class 1 silent orphaning.
Second, `ancestor_suite_covers_subtree` in cq_reachability returned False
whenever any sibling directory had a local `bazel_test_suite` in BUILD.gn before
checking whether the ancestor `test_suite` already aggregated tests from the
target's subtree (`sub_root`), causing `unwanted_ancestor_test_suite_hookup` to
contradict `test_suite_layout/per_package_bazel_test_suite` and
`tests_json_parity`.

## Why not a one-off fix

Fixing only a single package directory would leave every future dual-build
migration of a host-only GN package using top-level `assert(is_host)` or any
subtree where an ancestor `test_suite` coexists with a legacy sibling
`bazel_test_suite` deadlocked between `lint_config_parity`, `cq_reachability`,
`test_suite_layout`, and `shac` test-parity checks.
