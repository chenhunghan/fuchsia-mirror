# Rebased commit base resolution and review diff false positive deduplication

- **Learned from:** inner loop friction on CL If360d6314b1adc3c4597bbbdcaea551770e5b024, patchset 14 (task `src-sys-run-test-suite`)
- **Date:** 2026-10-31
- **Changed:** `checks/commit_message_format.sh`, `tools/run_checks.sh`

## Root cause

First, commit_message_format.sh read post-change BUILD.bazel and BUILD.gn files
only from the working tree without falling back to git HEAD when a concurrent
check (such as tests_json_parity) temporarily reverted the working tree during
baseline fx gen, and did not advance a stale pre-amend [bazel_migration] or
parent-ancestor base revision to HEAD~1, causing also_migrated_already_migrated
false positives on co-migrated directories. Second, review_diff (diffengine)
emitted false-positive errors and warnings on unrolled
fx_test_component/fx_package test targets that bazel2gn translates to
fuchsia_test_component/fuchsia_test_package from original
fuchsia_unittest_component/fuchsia_unittest_package targets (template_parity),
unmigrated GN-only child target directories that intentionally have no
BUILD.bazel (gn_unmodified), bazel2gn-synced third_party/rust_crates/vendor
dependencies mapped to //third_party/rust_crates:<crate> below the sentinel
(dep_alignment), and GN /* visibility entries narrowed to :__pkg__ per
find_rdeps/visibility_audit (visibility_narrowed).

## Why not a one-off fix

Fixing or waiving these findings for a single package directory would leave all
future multi-directory migrations, custom-manifest unit test unrollings,
unmigrated child integration-test directories, and dual-build Rust crate
dependency/visibility mappings vulnerable to the same recurring
commit_message_format and review_diff false positives.
