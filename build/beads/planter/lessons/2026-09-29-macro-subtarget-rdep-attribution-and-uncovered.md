# Macro subtarget rdep attribution and uncovered caller visibility

- **Learned from:** cq failure on [CL 1850715](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850715), patchset 1 (task `pkg-update-fidl`)
- **Date:** 2026-09-29
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`, `tools/manifest.json`

## Root cause

Both find_rdeps.sh and visibility_audit.sh only attributed reverse dependencies
to per_target_rdeps when the depended-on label exactly matched a top-level
target name in BUILD.bazel, dropping all callers that depend on macro-generated
subtargets such as FIDL language bindings (<name>_rust, <name>_cpp,
<name>_hlcpp). In addition, when a target declared a non-empty visibility list,
visibility_audit.sh only checked existing entries for overbroad wildcards or
false-positive rdeps without verifying that every actual reverse-dependency
caller package was covered by the visibility list.

## Why not a one-off fix

Fixing visibility on only a single FIDL package would leave every future
migration of FIDL libraries, bind libraries, and other macro-expanded targets
vulnerable to dropping language-binding callers from
per_target_recommended_visibility, falsely flagging legitimate callers as
false-positive rdeps, and silently omitting out-of-graph or vendor callers until
CQ subbuilds fail.
