# Dual build unconditional gn unit test polarity and collateral ancestor test suite scope

- **Learned from:** inner loop friction on CL I1022527aaae5aabdcf40b39105cb8dc87047e9ea, patchset 1 (task `ffx-product-leaves-a`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/cq_reachability.sh`, `checks/lint_config_parity.sh`

## Root cause

First, when a dual-build package's pre-migration BUILD.gn declared a library
with `with_unit_tests = true` without an enclosing `if (is_host)`,
`assert(is_host)`, or a local `group("tests")` `$host_toolchain` edge (because
the parent BUILD.gn selected `$host_toolchain`),
`scripts/shac/check_bazel_test_parity.py` classifies the pre-migration unit test
in `before_gn` as `device` polarity; using `host_rustc_test` or unskipped
`target_compatible_with = HOST_OS_CONSTRAINTS` causes `fx bazel2gn` to wrap the
generated `rustc_test` in `if (is_host)` (`host` polarity), triggering a
false-positive `CLASS 1 SILENT ORPHANING` shac error, while
`lint_config_parity.sh` previously forbade `# @bazel2gn:skip` on
`target_compatible_with = HOST_OS_CONSTRAINTS` unless `BUILD.gn` had
`assert(is_host)`. Second, `bazel_minimality.sh` allowed top-level `BUILD.gn`
files (`build/BUILD.gn`, `tools/BUILD.gn`, `src/BUILD.gn`, `sdk/BUILD.gn`) and
collateral dependency `BUILD.bazel` files (such as exporting test data), but
rejected edits to their ancestor `BUILD.bazel` test suites (such as
`build/BUILD.bazel`) required when touching a collateral `BUILD.bazel` triggers
`shac` test-parity checks on pre-existing unwired tests in that collateral
package.

## Why not a one-off fix

Many host subtool and library subpackages across the tree omit `if (is_host)`
and local `group("tests")` in their leaf `BUILD.gn` because their parent
aggregator `BUILD.gn` attaches `($host_toolchain)`, and many migrations touch
collateral `BUILD.bazel` packages to export files or widen visibility. Updating
`lint_config_parity.sh`, `cq_reachability.sh`, and `bazel_minimality.sh` handles
this general AST and test-suite pattern across all future migrations.
