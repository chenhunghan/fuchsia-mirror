# Rust unit test layout in dual-build packages: implicit with unit tests/with host unit tests generation vs explicit bazel2gn-skipped rustc test plus fx test device test rules

- **Learned from:** human review on CL I414a544b4af26cbb04f58ae5a4408a44c00e4b8f, patchset 8 (task `netstack-src-lib`)
- **Date:** 2026-10-02
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

No check inspected how Rust unit tests are expressed in BUILD.bazel, so
with_host_unit_tests was accepted; worse, migration_sanity treated rustc_test as
a convertible rule and flagged # @bazel2gn:skip on it (and GN rustc_test above
the sentinel) as errors, actively pushing the coder away from the required
explicit skipped rustc_test, and coder.md presented with_unit_tests as the
default with rustc_test only as a lint workaround. Nothing verified that a
non-host-only test had a target-side fx_test.

## Why not a one-off fix

Every Rust library migrated with bazel2gn faces the same choice of test layout;
fixing only one package leaves the checks rejecting skipped rustc_test and
accepting with_unit_tests everywhere else, so the same review comment would
recur on each future Rust migration and target-side tests would keep silently
disappearing.
