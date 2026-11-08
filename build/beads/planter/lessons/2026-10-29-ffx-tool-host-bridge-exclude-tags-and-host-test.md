# Ffx tool host bridge exclude tags and host test data runtime dir scope

- **Learned from:** inner loop friction on a local task (task `ffx-product-bundle`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`, `checks/commit_message_format.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

First, gn_attr_parity.sh recognized bazel_host_tool paired with
*_binary_host_tool as automatically satisfying GN exclude_toolchain_tags by
building in toolchain_variant.base, and check_gn_conditions_with_host_tools
already recognized bazel_ffx_tool alongside bazel_host_tool, but
BAZEL_HOST_TOOL_BINARY_RULES and is_host_tool_exclude_tags_exempt omitted
ffx_tool, idk_ffx_tool, and bazel_ffx_tool, causing false-positive
gn_only_attr_dropped errors and downstream review_diff attribute_alignment
warnings whenever a GN ffx_tool with exclude_toolchain_tags migrated to ffx_tool
or idk_ffx_tool bridged via bazel_ffx_tool. Second, build_only_scope.sh rejected
all source edits in packages with built GN library targets even when
#[cfg(test)] or dedicated test files had to adapt GN build-directory-relative
('../../<pkg>/...') or current_exe()-relative paths to Bazel's
'<test>.runtime_dir' working directory where host_test_data_files and
host_test_data_map stage runtime test data, and commit_message_format.sh assumed
every modified source file was an unbuilt-package bitrot fix.

## Why not a one-off fix

Exempting a single ffx subtool target or a single test source file path would
fail on every remaining GN ffx_tool migration that sets exclude_toolchain_tags
(such as asan, coverage, or profile exclusions bridged via bazel_ffx_tool) and
on every host unit or integration test across the tree whose #[cfg(test)] or
test source hardcodes GN's out/<dir> relative paths ('../../<pkg>/...') or
executable-parent lookups when migrating runtime test fixtures to
host_test_data_files or host_test_data_map. Handling ffx_tool, idk_ffx_tool, and
bazel_ffx_tool generally in gn_attr_parity.sh and validating AST/hunk-scoped
test-only runtime_dir path adaptations in build_only_scope.sh and
commit_message_format.sh resolves both classes of friction across all future
migrations without weakening production source-edit protections.
