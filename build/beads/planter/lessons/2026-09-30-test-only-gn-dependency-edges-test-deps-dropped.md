# Test-only GN dependency edges (test deps) dropped when Rust unit tests move from GN with unit tests to Bazel-only tests exported via bazel test suite

- **Learned from:** inner loop friction on CL I8a72d619c70f2f7790c6c233a5712af117663649, patchset 4 (task `starnix-wave-next`)
- **Date:** 2026-09-30
- **Changed:** `checks/gn_dep_parity.sh`, `checks/manifest.json`

## Root cause

gn_dep_parity treated every removed dependency carrying link-affecting settings
as a GN link hazard, regardless of edge kind. It exempted a removed test_deps
label only when another target of the package still depended on it, which is the
case for an explicit GN test target. The machinery also requires unit tests to
move to Bazel, where bazel2gn never generates with_unit_tests. In that case the
test_deps entries have no GN consumer at all, and the check demanded that an
unused dependency be restored. That contradicts the Migrating Tests rules and
blocks the coder.

## Why not a one-off fix

Every dual-build migration of a rustc_library whose GN unit test had test_deps
with rustflags, configs or libs (any first-party Rust library) will hit this
error once the test moves to Bazel. Waiving it for one package leaves the
contradiction between gn_dep_parity and the tests-move-to-Bazel rule in place
for all future tasks. The check must understand that test_deps only reach GN's
generated <name>_test executable.
