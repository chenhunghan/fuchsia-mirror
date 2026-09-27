# Overbroad subpackages visibility and coarse lca rollup

- **Learned from:** human review on [CL 1842102](https://fuchsia-review.googlesource.com/c/fuchsia/+/1842102), patchset 9 (task `cl-1842102`)
- **Date:** 2026-09-24
- **Changed:** `checks/manifest.json`, `checks/visibility_audit.sh`, `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`, `tools/find_rdeps.sh`, `tools/manifest.json`

## Root cause

The previous visibility machinery only forbade '//:__subpackages__' or
enumerating >=5 top-level '//<dir>:__subpackages__' roots, and find_rdeps.sh
returned a flat ungrouped list of package paths (with substring prefix
collisions and GN-only :verify_bazel2gn/:tests/:benchmarks references).
Consequently, when multiple callers existed under '//src/...', '//src/lib/...',
'//src/storage/...', '//src/connectivity/...', or '//src/starnix/...', the coder
collapsed them into coarse depth-1 ('//src:__subpackages__') or depth-2
('//src/lib:__subpackages__', '//src/storage:__subpackages__',
'//src/connectivity:__subpackages__', '//src/starnix:__subpackages__') wildcards
that passed visibility_audit.sh completely unchallenged.

## Why not a one-off fix

Manually narrowing the visibility list in a single BUILD.bazel file leaves the
coder, reviewer seats, find_rdeps tool, and visibility_audit check vulnerable to
repeating coarse depth-1 and depth-2 ':__subpackages__' rollups and
false-positive GN-only rdeps across all other migrated packages in the CL and
future Fuchsia GN-to-Bazel tasks.
