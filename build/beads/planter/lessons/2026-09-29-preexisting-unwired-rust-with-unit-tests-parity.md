# Preexisting unwired rust with unit tests parity

- **Learned from:** inner loop friction on CL I92731fa0c2e39ad4a9cff53a171a29ed615f021c, patchset 1 (task `devices-fidl-ir`)
- **Date:** 2026-09-29
- **Changed:** `checks/cq_reachability.sh`

## Root cause

checks/cq_reachability.sh required every rustc_*(with_host_unit_tests = True)
target in a dual-build package either to be referenced above the BAZEL2GN
SENTINEL in BUILD.gn (or parent BUILD.gn) or exported via
bazel_test_suite(host_tests = [...]). However, when a pre-migration BUILD.gn
declared with_unit_tests = true on a rustc_* library without any group("tests"),
test package, or parent reference to :<name>_test, setting with_host_unit_tests
= True in BUILD.bazel is required for bazel2gn to emit with_unit_tests = true in
BUILD.gn while keeping Fuchsia target builds passing without adding unrequested
test suites or groups.

## Why not a one-off fix

Many Rust libraries across the Fuchsia tree declare with_unit_tests = true in
BUILD.gn as boilerplate without wiring the generated :<name>_test target into
any test group or package. Updating cq_reachability.sh to check whether
with_unit_tests = true was already unwired in the pre-migration BUILD.gn
prevents false-positive unexported_bazel_test failures across all dual-build
Rust library migrations while still catching regressions when a previously wired
test is dropped.
