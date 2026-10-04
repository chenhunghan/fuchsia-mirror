# Commit msg test footer consolidation

- **Learned from:** human review on CL I9a0baded964f3f9be9166a4c39455469515b69c7, patchset 1 (task `starnix-deps-migrated`)
- **Date:** 2026-09-29
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

The commit message Test: footer validation in build_verification.sh checked
individual Test: commands for syntax errors, unconfigured GN labels, and missing
validate_json exclusions, and prompts/coder.md instructed the coder to record
executed commands in tests_run and Test: footers, but neither capped the total
number of Test: footer lines nor required consolidating per-package incremental
fx build/fx bazel build/query commands into at most 3 Test: lines.

## Why not a one-off fix

Amending only the single commit message in this change would not prevent future
multi-directory migrations or iterative coding rounds from accumulating many
per-package Test: footer lines in commit messages and CoderReport.tests_run.
