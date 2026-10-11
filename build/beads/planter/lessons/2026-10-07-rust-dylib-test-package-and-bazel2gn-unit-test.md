# Rust dylib test package and bazel2gn unit test parity

- **Learned from:** inner loop friction on a local task (task `starnix-leaf-libs-memory_pinning`)
- **Date:** 2026-10-07
- **Changed:** `checks/migration_sanity.sh`

## Root cause

In migration_sanity.sh, depends_on_rustc_dylib matched visibility labels as
dependencies and capped frontier size len(seen) <= 64 instead of visited
packages, causing traversal to abort after visiting a single dependency with a
large visibility allowlist before reaching direct or transitive rustc_dylib
dependencies. Furthermore, test_generated_by_bazel2gn ran before GN test package
blocker evaluation and unconditionally flagged unskipped with_unit_tests and
test_deps attributes in BUILD.bazel and the generated section of BUILD.gn even
when a blocked GN test package above the sentinel depended on the
bazel2gn-generated :<name>_test target.

## Why not a one-off fix

Any Rust library or binary whose direct or transitive dependencies include a
rustc_dylib target (or whose GN test package has another valid migration
blocker) cannot package its unit test with fx_packaged_binary and fx_package in
Bazel due to duplicate shared library entries in default_runfiles, and must keep
its GN test package above the sentinel depending on the bazel2gn-generated
:<name>_test target.
