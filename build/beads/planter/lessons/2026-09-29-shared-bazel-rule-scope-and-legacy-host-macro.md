# Shared bazel rule scope and legacy host macro visibility

- **Learned from:** human review on [CL 1850658](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850658), patchset 2 (task `pkg-go-leaves`)
- **Date:** 2026-09-29
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

When migrating a GN host binary to a legacy host-tool macro (go_binary_host_tool
or py_binary_host_tool in //build/bazel/rules/host:defs.bzl) whose GN definition
also carried a redundant output_name equal to name, the coding agent encountered
two errors: (1) legacy macros expand
select({"//build/bazel/versioning:is_api_level_PLATFORM": [], ...}) in the
caller's package, requiring caller package visibility on
//build/bazel/versioning:is_api_level_PLATFORM, and (2) go_binary does not
accept an output_name attribute. Because bazel_minimality.sh previously exempted
the entire build/bazel/ prefix in ALLOWED_GLOBAL_PREFIXES, the check allowed the
agent to rewrite build/bazel/rules/host/defs.bzl into a symbolic macro that
imported go_binary a second time from a private
@io_bazel_rules_go//go/private/rules:binary.bzl path, added an uncommented
linkmode = None workaround, and added output_name = None instead of omitting the
redundant output_name == name attribute and granting visibility in
build/bazel/versioning/BUILD.bazel.

## Why not a one-off fix

Reverting build/bazel/rules/host/defs.bzl or fixing only the single migrated
package would not prevent future tasks from modifying shared rule, macro,
aspect, or toolchain definitions under //build/bazel/, passing redundant
output_name == name attributes to Bazel rules, forgetting to grant caller
package visibility on //build/bazel/versioning:is_api_level_PLATFORM when
invoking legacy host-tool macros, or loading external rules from private .bzl
paths alongside public rule entry points.
