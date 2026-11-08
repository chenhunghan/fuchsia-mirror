# Commit message test footer coverage and host test data helpers

- **Learned from:** inner loop friction on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 5 (task `src-diagnostics-triage`)
- **Date:** 2026-10-30
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

After the transient merge conflict in build_only_scope.sh and
commit_message_format.sh was resolved, commit_message_format.sh still treated
synthetic host_test_data_* targets, testonly = True genrule test-data helpers,
and GN-bridged bazel_host_tool targets listed in Bazel-Migration-Target: footers
as uncovered non-test targets in test_footer_missing_migrated_target, and
hardcoded --config=fuchsia_platform in its remediation even for targets
constrained to HOST_OS_CONSTRAINTS.

## Why not a one-off fix

Any GN-to-Bazel migration that introduces host_test_data_files, testonly = True
genrule helpers, or bazel_host_tool bridges alongside dual-build libraries will
trigger false-positive test_footer_missing_migrated_target warnings and invalid
--config=fuchsia_platform build suggestions unless commit_message_format.sh
classifies test-data helpers, GN-bridged targets, and host-constrained targets
by their AST rules and attributes.
