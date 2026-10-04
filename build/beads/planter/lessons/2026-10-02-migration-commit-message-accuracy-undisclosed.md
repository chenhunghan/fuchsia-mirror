# Migration commit message accuracy: undisclosed shared build-rule changes and naming internal rules instead of the macros and targets the BUILD.bazel file actually defines

- **Learned from:** human review on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

commit_message_format only linted the subject line and Test: footers. Nothing
compared the commit body with the actual diff or the migrated BUILD.bazel. So an
edit to a shared .bzl rule outside the migrated directory went undescribed, and
the body named rules (rust_library, rust_test) that the BUILD.bazel never calls
instead of rustc_library() and the targets its attributes generate.

## Why not a one-off fix

Rewording this one commit message would not stop the next migration from editing
a shared .bzl macro without saying so, or from describing targets by rules they
use only indirectly. Every migration that touches shared rules or uses wrapper
macros like rustc_* or fx_cc_* needs a deterministic body check.
