# Rust test clippy warn all and ifttt source sync scope

- **Learned from:** inner loop friction on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 3 (task `src-diagnostics-triage`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`, `checks/lint_config_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

When a human-review machinery evolution runs in parallel with the remediation
coding agent, run_checks.sh skipped waiting for the in-progress evolution during
--skip-build runs and timed out after 180s on warm incremental builds, causing
the coder to observe pre-evolution build_only_scope and lint_config_parity
failures and record them in disputed_findings; additionally, build_only_scope
and lint_config_parity silently skipped allowed LINT.IfChange/LINT.ThenChange
comments (missing multi-line LINT.ThenChange blocks and failing when
PLANTER_TARGET_DIRS was unset) and clippy_warn_all test lint fixes without
emitting authoritative INFO coverage findings for downstream review seats, and
lint_config_parity did not advance change_base to HEAD~1 when HEAD was already
the migration commit.

## Why not a one-off fix

Any migrated Rust package that applies //build/config/rust/lints:clippy_warn_all
to test targets or pairs host test data paths across BUILD.bazel, BUILD.gn, and
test sources with LINT.IfChange/LINT.ThenChange comments encounters the same
interaction between parallel machinery evolution, build_only_scope, and
lint_config_parity, so run_checks.sh and both static checks must handle parallel
evolution synchronization, multi-line LINT.ThenChange blocks, and authoritative
INFO coverage across all packages.
