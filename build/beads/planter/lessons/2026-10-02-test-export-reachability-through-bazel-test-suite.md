# Test export reachability through Bazel test suite aggregation across packages (exporting a test via another package's test suite that a GN bazel test suite lists)

- **Learned from:** inner loop friction on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/cq_reachability.sh`

## Root cause

cq_reachability only expanded Bazel test_suite labels defined in the migrated
package itself: label_target() returned None for any bazel_test_suite label in
another package, so the label was skipped. A test exported through an ancestor's
or sibling's aggregate test_suite (a GN bazel_test_suite host_tests entry
pointing at //area:tests, whose Bazel test_suite lists the migrated test) was
therefore reported as unexported_bazel_test. The check's own fix advice already
says a test_suite label works. The coder could not satisfy the check without
adding a duplicate per-package bazel_test_suite that runs the test twice, so it
got stuck disputing the finding.

## Why not a one-off fix

Many migrated packages share one aggregate Bazel test_suite exported by a single
GN bazel_test_suite in a parent directory. Patching the one package (adding a
redundant per-package bazel_test_suite or ignoring the finding) would export
tests twice and diverge from that convention. Every later package that follows
the shared aggregate pattern would hit the same false positive. The check itself
has to follow test_suite membership across packages.
