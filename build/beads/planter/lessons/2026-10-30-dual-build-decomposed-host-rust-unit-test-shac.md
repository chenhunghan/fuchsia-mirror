# Dual build decomposed host rust unit test shac parity

- **Learned from:** inner loop friction on CL Ib37092a0900fa5e7c7d1e451ffde020d395dfbd1, patchset 3 (task `ffx-lib-fastboot-flash-manifest`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_attr_parity.sh`

## Root cause

When a dual-build Rust package whose base BUILD.gn defined `with_unit_tests =
true` and `group("tests")` without an in-file `if (is_host)` wrapper is migrated
outside the `//build/bazel/rules/rust:generate_unit_tests.bzl`
`_ALLOWED_PACKAGES` allowlist, `with_host_unit_tests = True` is rejected by
Bazel while `host_rustc_test` causes `bazel2gn` to wrap the generated
`rustc_test` in `if (is_host)`, triggering a false-positive `shac`
`bazel_test_parity` Class 1 removed-device-leg error. `gn_attr_parity.sh`
previously only recommended `with_host_unit_tests = True` without checking
`_ALLOWED_PACKAGES`, and `bazel_minimality.sh` rejected `# @bazel2gn:skip` on
`target_compatible_with = HOST_OS_CONSTRAINTS` on a decomposed `rustc_test`
paired with `wrap_host_rust_test`.

## Why not a one-off fix

Any non-allowlisted host-only dual-build Rust library in the tree whose base
BUILD.gn relied on caller/ancestor `assert(is_host)` rather than an in-file `if
(is_host)` block will hit the exact same contradiction between
`generate_unit_tests.bzl` disallowing `with_host_unit_tests` and `shac`
`bazel_test_parity` rejecting `if (is_host)` around the generated `rustc_test`.
Updating `gn_attr_parity.sh` and `bazel_minimality.sh` to prescribe and allow
`rustc_test(..., target_compatible_with = HOST_OS_CONSTRAINTS, #
@bazel2gn:skip)` paired with `wrap_host_rust_test` resolves this class of
conflict across all such packages.
