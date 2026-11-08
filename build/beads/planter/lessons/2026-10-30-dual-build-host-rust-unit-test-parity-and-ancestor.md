# Dual build host rust unit test parity and ancestor suite coverage

- **Learned from:** inner loop friction on CL Ib37092a0900fa5e7c7d1e451ffde020d395dfbd1, patchset 3 (task `ffx-lib-fastboot-flash-manifest`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/gn_attr_parity.sh`

## Root cause

First, cq_reachability's has_sibling_local_bazel_test_suite disqualified an
ancestor BUILD.bazel test_suite whenever any package in an intermediate parent
directory still had a legacy per-package bazel_test_suite in BUILD.gn, even when
the ancestor BUILD.bazel at the change base already aggregated tests from that
parent subtree. Second, when a host-only dual-build Rust target whose base
BUILD.gn lacked an in-file if (is_host) wrapper decomposed with_unit_tests =
true into a standalone host_rustc_test (generating if (is_host) { rustc_test }
in BUILD.gn), shac's bazel_test_parity check misclassified the base unit test as
a device leg and failed because explicit rustc_test rules inside if (is_host)
only register host polarity whereas with_unit_tests = true on rustc_library
inside if (is_host) satisfies both host and group("tests") polarity.

## Why not a one-off fix

Hardcoding exemptions for a single subsystem or library directory would leave
all other multi-level subtrees with mixed ancestor test_suite and legacy local
bazel_test_suite packages blocked by false-positive
unwanted_ancestor_test_suite_hookup errors, and would fail to guide future
host-only dual-build Rust library migrations away from decomposing inline unit
tests into standalone host_rustc_test targets when base BUILD.gn relied on
ancestor host assertions.
