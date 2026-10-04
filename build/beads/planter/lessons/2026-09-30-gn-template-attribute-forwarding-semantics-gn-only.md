# GN template attribute-forwarding semantics: GN-only attributes that a Rust template forwards only to its generated unit test, not to the library itself

- **Learned from:** inner loop friction on CL I8a72d619c70f2f7790c6c233a5712af117663649, patchset 4 (task `starnix-wave-next`)
- **Date:** 2026-09-30
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

gn_attr_parity treated exclude_toolchain_tags as a property of every target that
declares it. But rustc_library and rustc_staticlib forward it only to their
with_unit_tests test (rustc_test_internal), not to rustc_artifact, and
rustc_macro ignores it. So a correct migration was flagged: bazel2gn generated
the library and the unit test stayed hand-written in GN with the exclusion. This
contradicted migration_sanity's rule against a target-level skip on convertible
libraries, and the coder could not satisfy both checks.

## Why not a one-off fix

Any Rust library in the tree that sets exclude_toolchain_tags with
with_unit_tests (common for sanitizer-excluded areas) hits the same
contradiction between gn_attr_parity and migration_sanity. Waiving the one
finding would leave every later migration stuck the same way. The check now
models the template's forwarding and still errors when the exclusion is really
lost, for example when the test moves to a Bazel fx_test.
