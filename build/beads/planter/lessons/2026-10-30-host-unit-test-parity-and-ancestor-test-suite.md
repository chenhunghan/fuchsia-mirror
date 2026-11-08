# Host unit test parity and ancestor test suite scoping

- **Learned from:** inner loop friction on CL I4d77863e5462a881b402992ae5450caffe381a6a, patchset 4 (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_verification.sh`, `checks/gn_attr_parity.sh`

## Root cause

Two deterministic check defects stalled host-library dual-build migrations: (1)
gn_attr_parity (and a companion exception in bazel_minimality) enforced
decomposed_host_unit_test_shac_mismatch for the upstream bazel_test_parity shac
check after that shac check was reverted upstream, demanding either deprecated
allowlist-gated with_host_unit_tests = True or # @bazel2gn:skip on
HOST_OS_CONSTRAINTS; and (2) build_verification appended raw
bazel_test_suite(host_tests) labels from any modified ancestor package in
bazel_dirs (such as an ancestor BUILD.bazel that updated both test_suite and a
subpackage file label) directly into host_test_targets without expanding
test_suite aggregators to changed packages, executing unrelated sibling
packages' host tests inside RBE's network-isolated sandbox.

## Why not a one-off fix

Exempting individual host-library packages or hardcoding specific ancestor
test_suite labels would leave every future host-only Rust library migration with
base with_unit_tests = true blocked by the obsolete shac parity check, and would
re-trigger unrelated sibling RBE test failures whenever any migration updates
both an ancestor test_suite and another target or collateral
visibility/exports_files attribute in an ancestor or dependency package.
