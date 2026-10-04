# Rust unit-test data dependencies: test-only files must use test data (or host test data), not the runtime data attribute

- **Learned from:** human review on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/manifest.json`, `checks/test_data_attribute.sh`

## Root cause

No check looked at the data attribute of rustc_* targets that generate unit
tests. The coder put test fixtures in runtime data, which made it edit a shared
host-test macro to pass data through, change a source file to find the files,
and drop a GN test_data dependency. No reviewer seat or check covered the data
vs test_data distinction.

## Why not a one-off fix

Any Rust library migrated with with_unit_tests or with_host_unit_tests and
fixture files can make the same mistake. Fixing only this package's BUILD.bazel
would leave future migrations free to ship test fixtures as library runtime data
and to edit shared rule macros to work around it. A deterministic check catches
it in every package.
