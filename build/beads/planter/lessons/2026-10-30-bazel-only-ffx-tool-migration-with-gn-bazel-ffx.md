# Bazel-only ffx tool migration with GN bazel ffx tool bridge and partial-directory commit subject detection

- **Learned from:** human review on CL I1e367ae421e1a4944abdea68edd2885d392205a7, patchset 2 (task `ffx-tools-monitor`)
- **Date:** 2026-10-30
- **Changed:** `checks/commit_message_format.sh`, `checks/migration_sanity.sh`

## Root cause

While migration_sanity.sh already caught stale ffx_tool() blocks below the
BAZEL2GN SENTINEL once bazel2gn mapped ffx_tool to noGNEquivalent, it did not
flag hand-written ffx_tool() targets kept in BUILD.gn without a sentinel, and
when a sentinel was present it treated a pre-sentinel ffx_tool() as a
bazel2gn-convertible target with a misleading remediation telling the coder to
let fx bazel2gn generate it below the sentinel instead of replacing it with
bazel_ffx_tool(). Additionally, commit_message_format.sh omitted bazel_ffx_tool,
bazel_fuchsia_package, host_test_data, and sdk_atom from gn_meta_constructs,
causing fully migrated ffx_tool packages retaining a bazel_ffx_tool bridge to be
misclassified as partially migrated directories.

## Why not a one-off fix

Every ffx subtool package across //src/developer/ffx/tools/* and
//src/developer/ffx/plugins/* defines an ffx_tool() in BUILD.gn that must
migrate to Bazel-only ffx_tool or idk_ffx_tool with a GN bazel_ffx_tool()
bridge. Enforcing this deterministically in migration_sanity.sh (both with and
without a sentinel) and recognizing bazel_ffx_tool bridges in
commit_message_format.sh prevents hand-written GN ffx_tool() targets and
false-positive partial-directory commit subject errors across all future ffx
subtool migrations.
