# Commit message accuracy: conflating Bazel target names with Rust crate name / macro-generated target names

- **Learned from:** human review on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

commit_message_format only checked for underlying rule names that BUILD.bazel
never calls. It never compared names in the commit body against the actual
target names in BUILD.bazel, so a crate_name (and <crate_name>_test) presented
as a target name passed, even though the real targets were "lib" and the
macro-generated "lib_test".

## Why not a one-off fix

Any Rust migration whose rustc_library/rustc_binary sets a crate_name different
from its target name (very common: name = "lib") can make the same mistake.
Rewording one commit message would not stop the next migration from describing
crate names as targets, while a check that parses name/crate_name pairs catches
it in every package.
