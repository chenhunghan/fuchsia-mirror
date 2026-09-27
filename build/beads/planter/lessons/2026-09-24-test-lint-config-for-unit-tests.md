# Lints that only fire on Bazel test code

- **Learned from:** human review on [CL 1845546](https://fuchsia-review.googlesource.com/c/fuchsia/+/1845546), patchset 2 (task `starnix-page_buf`)
- **Date:** 2026-09-24
- **Changed:** `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`,
  `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/target_parity.md`

## Root cause

Bazel `rustc_library(with_unit_tests = True)` applies the library's
`lint_config` to the generated test target, while GN leaves production-only
lints (`redundant_clone`, `perf`, ...) off test code. Area lint configs such as
`//src/starnix/build:kernel_library_config` must include the production lints
because Bazel replaces the defaults, so test code can fail Bazel clippy where
GN passed. After being told not to edit sources, the coder split the tests into
a `rustc_test` that used plain `clippy_warn_default` (dropping the Starnix
lints) and used `# @bazel2gn:raw_overwrite` to give GN a different lint config
than Bazel.

## Why not a one-off fix

About 200 GN targets combine a custom lint config with `with_unit_tests`, so
any of them can hit this. The coder needs one sanctioned pattern (an explicit
`rustc_test` with the area's test lint config, e.g.
`//src/starnix/build:kernel_library_test_config`), and reviewers need to reject
the workarounds (source edits, `raw_overwrite`, dropping area lints).
