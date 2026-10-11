# Bazel Rust unit test target hygiene in dual-build packages: visibility scoping of test plumbing vs GN-exported test entry points, spurious manual tags, crate dep duplication in rustc test, and host test wrapper naming

- **Learned from:** human review on CL I414a544b4af26cbb04f58ae5a4408a44c00e4b8f, patchset 9 (task `netstack-src-lib`)
- **Date:** 2026-10-02
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `checks/visibility_audit.sh`

## Root cause

The rust_unit_test_layout check only verified that an explicit rustc_test with #
@bazel2gn:skip and an fx_test existed; nothing inspected the test targets'
visibility, tags, deps overlap with the tested crate, or the wrap_host_* naming,
and visibility_audit treated fx_test/wrap_host_* public visibility as overbroad,
so the coder left plumbing unscoped, tagged tests manual, and copied library
deps.

## Why not a one-off fix

Every Rust library migrated with explicit rustc_test + fx_test +
wrap_host_rust_test emits the same set of plumbing targets, so fixing one
package's BUILD.bazel by hand would let the same visibility, manual-tag,
duplicated-deps and naming defects recur in every future Rust test migration; a
deterministic AST check over all such rules prevents the class.
