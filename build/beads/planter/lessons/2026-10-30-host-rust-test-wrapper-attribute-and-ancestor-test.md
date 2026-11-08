# Host rust test wrapper attribute and ancestor test suite subtree coverage

- **Learned from:** inner loop friction on CL Ib37092a0900fa5e7c7d1e451ffde020d395dfbd1, patchset 1 (task `ffx-lib-fastboot-flash-manifest`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/speculative_unit_test.sh`

## Root cause

speculative_unit_test.sh only inspected `binary` (missing `binary_name`) on
`wrap_host_rust_test` calls, causing the wrapped test name to resolve to an
empty string and fail `group("tests")` host-test detection; simultaneously,
`cq_reachability.sh` called `has_sibling_local_bazel_test_suite` across all
intermediate ancestor directories before checking whether the ancestor
`test_suite` already aggregated packages from those directories, creating a
contradiction with `test_suite_layout.sh` whenever an area `test_suite` had even
one legacy sibling with a local `bazel_test_suite`.

## Why not a one-off fix

Any host-only Rust crate using `wrap_host_rust_test(binary_name = ...)` or
residing in a multi-level subsystem tree with an established ancestor
`test_suite` and a single legacy per-package `bazel_test_suite` sibling will hit
the exact same false-positive errors and check deadlock unless
`wrap_host_rust_test` attribute parsing and ancestor `test_suite` subtree
membership resolution are fixed generally.
