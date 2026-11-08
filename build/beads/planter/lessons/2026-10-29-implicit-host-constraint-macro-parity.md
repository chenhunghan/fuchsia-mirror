# Implicit host constraint macro parity

- **Learned from:** inner loop friction on a local task (task `ffx-target-connector-forward`)
- **Date:** 2026-10-29
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

Planter's diffengine (`review_diff`) reports `host_compatibility` errors
whenever a GN target inside an `if (is_host)` block migrates to a Bazel target
without an explicit `target_compatible_with = HOST_OS_CONSTRAINTS` attribute,
and `bazel_minimality` lacked deduplicating `INFO` `host_compatibility` coverage
(as well as static rejection of `target_compatible_with`) for host-only Bazel
symbolic macros such as `ffx_plugin` and `idk_ffx_tool` that set
`HOST_CONSTRAINTS` or `HOST_OS_CONSTRAINTS` internally and reject
`target_compatible_with` on call sites.

## Why not a one-off fix

Pre-migration `BUILD.gn` files across `ffx` plugins, IDK host subtools, host
binaries, and host tests frequently wrap `ffx_plugin`, `ffx_tool`,
`idk_ffx_tool`, `*_binary_host_tool`, and `host_*` targets inside `if (is_host)`
blocks even though their Bazel macros enforce or default host platform
constraints internally; handling implicit host-constrained macros and loaded
`.bzl` `macro()` definitions in `bazel_minimality` and `run_checks.sh`
eliminates both the `review_diff` false positive and invalid
`target_compatible_with` edits across all future host migrations.
