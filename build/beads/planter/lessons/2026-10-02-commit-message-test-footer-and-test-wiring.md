# Commit message Test: footer and test wiring: migrated tests must be reachable from (and verified through) the ancestor Bazel test suite() aggregating the area's tests

- **Learned from:** human review on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

commit_message_format only linted the syntax of Test: footers (config flags, @
labels, bazel2gn form) and never compared them with the build graph, so a footer
running the leaf test directly passed even though an ancestor test_suite()
already aggregated it; nothing reported when migrated tests were in no ancestor
test_suite() at all, so the coder never surfaced that the developer must pick
one.

## Why not a one-off fix

Rewriting this one CL's footer fixes nothing for later migrations: every
Rust/C++ test migration in any area faces the same choice between a leaf label
and an aggregating ancestor test_suite(), and the first migration in an area has
no suite at all, so the rule has to be a deterministic check that resolves
ancestor suites for whatever directory is migrated.
