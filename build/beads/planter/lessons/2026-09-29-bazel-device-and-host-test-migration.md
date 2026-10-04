# C++ device tests and host tests migrate to Bazel (fx_test, host_*_test) via bazel_test_suite; Rust device tests stay in GN

- **Learned from:** upstream Bazel test support (`//build/bazel/rules/testing:fx_test.bzl`, fuchsia.git 97f4649755d; precedent `//src/developer/build_info`, f35fbebcb96) and CL Ie34a1d76ef63a47f83f6eb5f72ea50a517533c56, patchset 1 (thermal: `with_host_unit_tests` while the GN unittest package stayed)
- **Date:** 2026-09-29
- **Changed:** `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/target_parity.md`, `prompts/reviewers/dual_build_sentinel.md`, `checks/cq_reachability.sh`, `checks/migration_sanity.sh`, `checks/build_only_scope.sh`, `checks/build_verification.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

The machinery predated Bazel device tests: coder.md and both reviewers
required every `fuchsia_unittest_package` and its test executable to stay
above the sentinel in GN, and build_only_scope rejected the hand-written test
`.cml` that `fx_component_manifest` needs. Nothing knew that a Bazel test
(`fx_test`, host test wrappers) is invisible to `fx test` and infra unless a
GN `bazel_test_suite` reachable from `group("tests")` lists it, and
gn_target_type_parity rejected the replacement of a C++ GN test package by a
same-named `bazel_test_suite`. Meanwhile, Bazel does not yet support Rust
unit tests on Fuchsia devices, so Rust packages must keep `with_host_unit_tests = True`
with the GN test package wrapping the bazel2gn-generated `:<name>_test` above
the sentinel, and must not export a host test that GN already runs.

## Why not a one-off fix

Almost every migrated package has tests. Now that the checkout provides
`fx_test`, C++ migrations would either leave test packages in GN (the old
rules demanded it) or migrate them without exporting them to tests.json, while
Rust migrations must not try to move device tests that Bazel cannot run yet.
The checks gate C++ device test migration on `build/bazel/rules/testing/fx_test.bzl`,
so older checkouts keep the GN-only path.
