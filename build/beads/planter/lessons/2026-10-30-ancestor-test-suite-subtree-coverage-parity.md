# Ancestor test suite subtree coverage parity

- **Learned from:** inner loop friction on CL I06e18c8b8aed9fede2fc30ec6b96a07c6a7ca9a7, patchset 3 (task `src-performance-memory-batch-heapdump_snapshot`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/test_suite_layout.sh`

## Root cause

In checks/cq_reachability.sh, ancestor_suite_covers_subtree treated any member
label resolving to a test_suite in another package as a multi-package sub-area
roll-up suite rather than checking whether that target is a package-local
test_suite (with an omitted, empty, or same-package-only tests list). When an
existing area test_suite listed a previously migrated leaf package via its
package-local :tests suite, has_direct_leaf_tests stayed False and
ancestor_suite_covers_subtree returned False for other child subtrees under the
same area, causing cq_reachability to emit unwanted_ancestor_test_suite_hookup
in direct contradiction with test_suite_layout's per_package_bazel_test_suite.

## Why not a one-off fix

Special-casing a single subsystem or package directory would leave the
contradiction intact for every other area BUILD.bazel test_suite that references
a leaf package's local :tests or :host_tests test_suite target, or where a
package defines both device and host tests while the ancestor area suite only
exports one kind.
