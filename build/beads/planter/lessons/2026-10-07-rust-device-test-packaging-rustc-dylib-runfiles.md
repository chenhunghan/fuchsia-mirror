# Rust device test packaging rustc dylib runfiles collision

- **Learned from:** inner loop friction on a local task (task `starnix-bootreason`)
- **Date:** 2026-10-07
- **Changed:** `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh flagged gn_test_package_not_migrated on any GN
fuchsia_unittest_package or fuchsia_test_package whose Rust test target had no
syntactic GN-only attributes in its own package, without checking whether the
test depends directly or transitively on a rustc_dylib (rust_dylib_library). On
Fuchsia, @rules_rust places both the dylib output and its _solib_ symlink into
default_runfiles, and get_runfiles_shared_lib_binary_info in
@fuchsia_rules_common//:utils.bzl emits a FuchsiaUnstrippedBinaryInfo for every
.so in default_runfiles without deduplicating by basename, causing
fx_packaged_binary + fx_package to fail with duplicate destination path
lib/lib<crate>-<hash>.so.

## Why not a one-off fix

Exempting a single package directory would fail on every other Fuchsia Rust
library or test package that depends directly or transitively on a rustc_dylib
target when migrating its unit tests to fx_test. Detecting rustc_dylib /
rust_dylib_library in the test's first-party dependency closure in
checks/migration_sanity.sh prevents false-positive gn_test_package_not_migrated
errors across all packages.
