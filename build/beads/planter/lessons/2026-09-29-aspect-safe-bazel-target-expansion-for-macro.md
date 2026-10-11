# Aspect safe bazel target expansion for macro validation subtargets

- **Learned from:** inner loop friction on [CL 1850715](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850715), patchset 1 (task `pkg-update-fidl`)
- **Date:** 2026-09-29
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

Fuchsia globally enables
//build/bazel/aspects:assert_no_deps.bzl%assert_no_deps_aspect on all fx bazel
build invocations, and that aspect iterates over ctx.rule.attr.data assuming
attr.label_list. Macros such as fidl_library, fidl_ir, validate_json, and
validate_json5 instantiate private _validate_json_action subtargets (e.g.
<name>_validate_ir_json) whose data attribute is a single Target (attr.label).
When build_verification.sh and prompts/coder.md unconditionally passed
//<dir>:all to fx bazel build, Bazel selected the private
<name>_validate_ir_json subtarget as a top-level target and applied
assert_no_deps_aspect directly to it, crashing analysis with 'Error: type Target
is not iterable'.

## Why not a one-off fix

Special-casing only the current task's FIDL directories would leave every future
migration of a package declaring fidl_library, fidl_ir, validate_json,
validate_json5, or zither_library (including mixed packages containing both
rustc_library/cc_library and test FIDL libraries) broken in
build_verification.sh and test_verifier whenever //<dir>:all is invoked.
