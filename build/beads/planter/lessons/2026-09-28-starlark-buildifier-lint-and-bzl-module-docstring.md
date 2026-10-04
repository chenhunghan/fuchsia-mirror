# Starlark buildifier lint and bzl module docstring hygiene

- **Learned from:** human review on CL Ie535654f0534b939408d709df36730ceb70927f5, patchset 1 (task `starnix-leaf-crates`)
- **Date:** 2026-09-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`, `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`

## Root cause

The existing static checks only inspected BUILD.bazel ASTs for redundant default
attributes and never executed buildifier -lint=warn -mode=check or checked .bzl
files for top-level module docstrings, allowing Starlark files without module
docstrings or with buildifier_lint findings to reach Gerrit shac presubmit.

## Why not a one-off fix

Adding a docstring to a single .bzl file in one package does not prevent future
migrations that introduce or edit .bzl files (such as args.bzl for GN
declare_args() migrations or shared constant definitions) from omitting module
docstrings or failing shac buildifier_lint checks across the repository.
