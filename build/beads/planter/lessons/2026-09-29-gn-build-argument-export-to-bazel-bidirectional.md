# GN build-argument export to Bazel: bidirectional LINT.IfChange/ThenChange pairs into large shared build files must be scoped with named labels

- **Learned from:** human review on CL I9a0baded964f3f9be9166a4c39455469515b69c7, patchset 5 (task `starnix-deps-migrated`)
- **Date:** 2026-09-29
- **Changed:** `checks/shared_config_reuse.sh`, `prompts/coder.md`

## Root cause

shared_config_reuse only checked that a LINT.ThenChange(//build/bazel/BUILD.gn)
marker existed in declare_args(); its regex accepted an unlabeled target, and
both its remediation text and coder.md told the coder to write the unscoped
form. So an imprecise pair pointing at the whole large, unrelated
build/bazel/BUILD.gn passed every check.

## Why not a one-off fix

Every migration that exports a GN declare_args() build argument to Bazel adds
the same LINT pair into the same shared build/bazel/BUILD.gn. Fixing one
args.gni leaves the machinery still recommending and accepting the unscoped
form, so the same review comment would come back on every later package that
exports a build argument.
