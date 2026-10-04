# Target compatibility constraints and manifest list sync

- **Learned from:** human review on CL Ib2e9fab824df6ce0bbd2ff226ef41940c510fff5, patchset 1 (task `starnix-line-discipline-pstate`)
- **Date:** 2026-09-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The existing bazel_minimality check did not audit targets that gate all sources
behind a single-branch select() with an empty default and
@bazel2gn:raw_overwrite instead of using target_compatible_with, nor did it
check that GN read_file() manifest lists expanded inline in BUILD.bazel remain
synchronized via LINT.IfChange/LINT.ThenChange or are removed when unused.

## Why not a one-off fix

Architecture-specific assembly/code targets and JSON-driven action input lists
occur across many system, kernel, and test libraries; enforcing
target_compatible_with over empty-default srcs selects and requiring
LINT.IfChange/LINT.ThenChange on expanded read_file() lists in bazel_minimality
prevents both defects across all future migrations.
