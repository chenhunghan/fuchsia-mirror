# Bazel aspect scalar attr incompatibility and test command shell quoting

- **Learned from:** inner loop friction on a local task (task `starnix-wave-next`)
- **Date:** 2026-09-28
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

First, Fuchsia's global build aspect
//build/bazel/aspects:assert_no_deps.bzl%assert_no_deps_aspect iterates over
getattr(ctx.rule.attr, 'data', []) assuming a label_list, which crashes with
'type Target is not iterable' whenever //<dir>:all selects a
_validate_json_action rule (such as <fidl>_validate_ir_json expanded by
fidl_library or direct validate_json/validate_json5 targets, where data is a
scalar attr.label) as a top-level target. Second, build_verification formatted
dry-run commands with ' '.join instead of shlex.join, emitting unquoted GN
toolchain labels like //pkg:target(//build/toolchain:...) that fail under bash
-c in test_verifier, and did not validate commit Test: footers for bash syntax
errors or unexcluded _validate_json_action targets.

## Why not a one-off fix

Every FIDL library and JSON-validation package across the Fuchsia tree
instantiates _validate_json_action with a scalar data attribute, and any
multi-toolchain GN binary or test in ninja_outputs.json carries a
(//build/toolchain:...) suffix. Excluding _validate_json_action targets from
top-level :all builds in build_verification, shell-quoting dry-run commands with
shlex.join, and deterministically validating commit Test: footers with bash -n
-c prevents both failure modes across all future migrations.
