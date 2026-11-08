# Workspace mount writability and host only macro verification

- **Learned from:** inner loop friction on CL I461efaa803aff813001a6cc1b027ad2b68c1007b, patchset 1 (task `ffx-guest-subplugins-b`)
- **Date:** 2026-10-31
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/manifest.json`, `checks/tests_json_parity.sh`, `tools/run_checks.sh`

## Root cause

Three systemic gaps interacted here: (1) degraded FUSE/casfs mounts allow 0-byte
O_CREAT metadata operations while failing non-empty write() calls with ENOENT,
so tests_json_parity's 0-byte mkstemp writability probe falsely passed and
failed mid-backup, while build_verification had no writability probe at all and
misreported mount write errors across fx build, git index.lock in shac check
(triggering a doomed static_checks rebuild), and fx bazel build as
build_failure_* defects without telling the coder to leave disputed_findings
empty when mount failures also block git commit --amend; (2)
build_verification's host_markers omitted host-only ffx_plugin, ffx_tool, and
idk_ffx_tool macros whose HOST_CONSTRAINTS live inside the .bzl macro rather
than BUILD.bazel, causing --config=fuchsia_platform :all builds to silently skip
all targets without running --config=host; and (3) commit_message_format's :all
and missing-target Test: footer diagnostics recommended
--config=fuchsia_platform (and invalid fx bazel test --config=fuchsia_platform)
even for host-only ffx_plugin/ffx_tool targets.

## Why not a one-off fix

Any migration slot running on a FUSE or casfs mount can experience backing-store
write failures where 0-byte file creation succeeds while data writes fail, and
dozens of ffx subtool and plugin packages across the tree use host-only
ffx_plugin, ffx_tool, or idk_ffx_tool macros without explicit
HOST_OS_CONSTRAINTS in BUILD.bazel. Probing non-empty file writes upfront in
build_verification and tests_json_parity, surfacing clear non-disputable
workspace_write_error guidance in run_checks, and recognizing host-only ffx
macros in build_verification and commit_message_format prevent both false
build_failure cascades on degraded mounts and silently skipped host builds
across all future migrations.
