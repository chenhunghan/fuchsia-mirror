# GN formatting of hand-edited or generated GN files outside the migrated package (verification .gni registration lists, parent groups)

- **Learned from:** cq failure on CL Ife8efc4fa12e331ed5c030de98fa22e94eb42a63, patchset 3 (task `devices-io-buffer`)
- **Date:** 2026-10-01
- **Changed:** `checks/gn_format.sh`, `checks/manifest.json`

## Root cause

The machinery only checked Starlark formatting with buildifier in
bazel_minimality. No check ran gn format on GN files, so an unformatted edit to
the shared verification .gni list (a new verify_bazel2gn entry out of order or
misformatted) passed every static check and only failed in the CQ static-checks
gn_format step.

## Why not a one-off fix

Every dual-build migration adds a line to the shared bazel2gn verification .gni
and often edits parent BUILD.gn groups or regenerates BUILD.gn, so any future
task can hit this. Fixing only this task's file would not stop the next
migration from hitting the same CQ failure. A check that runs gn format on every
changed GN file catches it before upload.
