# Host-tool exclude toolchain tags parity, compile data path overwrite subset source coverage, and unused GN test deps parity on Rust unit tests

- **Learned from:** inner loop friction on a local task (task `ffx-assembly`)
- **Date:** 2026-10-30
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

gn_attr_parity.sh did not include ffx_tool / idk_ffx_tool and bazel_ffx_tool in
its host-tool exclude_toolchain_tags exemption (even though bazel_ffx_tool wraps
bazel_host_tool, which builds in toolchain_variant.base), only emitted the full
GN inputs list in check_compile_data_inputs_info rather than the sorted subset
of inputs not matched literally in compile_data (such as
@bazel2gn:path_overwrite filegroups), and lacked a check in
check_unit_test_parity_info for unused non-library executable or unreferenced
crate entries dropped from GN test_deps.

## Why not a one-off fix

Any ffx_tool or idk_ffx_tool target with exclude_toolchain_tags migrated to
bazel_ffx_tool, any target combining literal compile_data files with
@bazel2gn:path_overwrite filegroups for GN inputs, and any Rust unit test
dropping an unused GN executable or crate from test_deps will hit the same
false-positive gn_only_attr_dropped, missing_sources, attribute_alignment, and
unit_test_parity errors unless gn_attr_parity.sh handles these general patterns.
