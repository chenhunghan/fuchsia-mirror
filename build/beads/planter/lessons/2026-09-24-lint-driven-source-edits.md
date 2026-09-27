# Build-only change scope violation: lint-driven source edits caused by GN configs-append vs Bazel lint config-replace semantic drift

- **Learned from:** human review on [CL 1845546](https://fuchsia-review.googlesource.com/c/fuchsia/+/1845546), patchset 1 (task `starnix-page_buf`)
- **Date:** 2026-09-24
- **Changed:** `checks/lint_config_parity.sh`, `checks/manifest.json`, `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`

## Root cause

The coder converted GN `configs += [<lint config>]` into Bazel/bazel2gn
`lint_config`, which changes lint semantics (GN appends to default lints, Bazel
replaces the macro default). A clippy finding (redundant_clone) in a
#[cfg(test)] module then surfaced, and the coder 'fixed' it by editing the Rust
source. The machinery only enforced build-only scope at file-name granularity;
nothing explained the lint_config replace/append drift, and no check gave hunk-
level file:line evidence of lint-silencing edits or warned about non-standard
lint_config labels. In addition, the previous evolution plan that added build-
only rules was rejected (invalid 'add_or_update' action), so those guards never
became authoritative.

## Why not a one-off fix

Reverting the one-line edit in the page_buf lib.rs fixes only this CL. Every
Rust package that converts a GN lint config (for example starnix, netstack, or
driver lint configs) into Bazel lint_config will hit the same lint-set drift and
tempt the coder into source edits, especially in with_unit_tests test code. The
prevention has to sit in the coder rules, the playbooks, the reviewer checklists
and a deterministic check that runs on every migration.
