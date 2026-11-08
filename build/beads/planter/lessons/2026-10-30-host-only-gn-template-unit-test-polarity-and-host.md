# Host only gn template unit test polarity and host test wrapper parity

- **Learned from:** inner loop friction on CL I8a38b187e1a6ff2983ccb8f3bcf5b58b4d182c99, patchset 4 (task `src-developer-ffx-plugins-self-test`)
- **Date:** 2026-10-30
- **Changed:** `checks/lint_config_parity.sh`, `checks/manifest.json`, `checks/speculative_unit_test.sh`

## Root cause

The `shac_unconditional_gn_unit_test_polarity_shift` check in
`checks/lint_config_parity.sh` matched `with_unit_tests = true` with a raw
file-level regex rather than scoping via `find_unguarded_gn_rust_unit_tests` to
`GN_RUST_UNITTEST_OWNER_RULES` (`rustc_library`, `rustc_binary`,
`rustc_proc_macro`). As a result, it misclassified host-only GN templates like
`ffx_plugin` (which wrap their inner `rustc_library` targets in `if (is_host)`
inside `ffx_plugin.gni` and are ignored by
`scripts/shac/check_bazel_test_parity.py`) as device-polarity unit tests, and
its remediation omitted `wrap_host_rust_test` (required for
`FuchsiaHostTestInfo` in `bazel_test_suite(host_tests = [...])`). Meanwhile,
`base_gn_had_host_unit_tests` in `checks/speculative_unit_test.sh` only checked
for literal `is_host` or `host_toolchain` strings in the leaf `BUILD.gn` or
`assert(is_host)` in ancestors, misclassifying unit tests of
`ffx_plugin`/`ffx_tool` and unguarded host libraries without Fuchsia test
packages as Fuchsia-only device tests.

## Why not a one-off fix

Hundreds of Fuchsia host subtools, plugins, and helper libraries invoke
`ffx_plugin(with_unit_tests = true)` or declare unguarded host
`rustc_library(with_unit_tests = true)` targets wired into parent
`group("tests")` with `($host_toolchain)` without a local `if (is_host)` block
in the leaf `BUILD.gn`. Scoping `shac_unconditional_gn_unit_test_polarity_shift`
to `GN_RUST_UNITTEST_OWNER_RULES`, requiring `wrap_host_rust_test` in its
remediation, and teaching `speculative_unit_test.sh` to recognize host-only
templates and unpackaged host unit tests prevents false positives and check
deadlocks across all future host plugin and library migrations.
