# Gn declare args single source of truth

- **Learned from:** human review on CL Ie535654f0534b939408d709df36730ceb70927f5, patchset 2 (task `starnix-leaf-crates`)
- **Date:** 2026-09-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_verification.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `checks/shared_config_reuse.sh`, `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`

## Root cause

The coder prompt, playbooks, and migration_sanity check previously instructed
the agent to either bind a '# @bazel2gn:skip' default in BUILD.bazel or create a
package-local .bzl file when migrating targets conditioned on GN declare_args()
variables, rather than exporting the GN build argument to
@fuchsia_build_info//:args.bzl via //build/bazel:gn_build_variables_for_bazel,
and no check flagged Starlark redefinitions of GN declare_args() variables.

## Why not a one-off fix

Fixing only a single package's build argument would leave the flawed
instructions in prompts/coder.md, prompts/playbooks/*.md, and
checks/migration_sanity.sh intact and would fail to prevent future migrations
across the tree from duplicating GN declare_args() definitions in local .bzl or
BUILD.bazel files instead of exporting them through
@fuchsia_build_info//:args.bzl.
