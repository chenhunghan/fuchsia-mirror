# Required upstream dependency co migration and cl scoping

- **Learned from:** inner loop friction on CL If360d6314b1adc3c4597bbbdcaea551770e5b024, patchset 11 (task `src-sys-run-test-suite`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `prompts/coder.md`, `prompts/reviewers/target_parity.md`

## Root cause

checks/bazel_minimality.sh restricted migrated_dependency_packages() to
same-area private helpers without a BAZEL2GN SENTINEL, while also counting
pre-existing BUILD.bazel files with only visibility edits (whose BUILD.gn was
untouched and lacked a sentinel) as migrated packages and firing
cl_scope_exceeded even when every extra migrated directory was an unmigrated
first-party upstream dependency required for the assigned target to build in
Bazel and pass cq_reachability (missing_bazel_dependency_package).

## Why not a one-off fix

Whitelisting only the specific upstream libraries of a single target directory
would leave bazel_minimality and cq_reachability in deadlock whenever any
assigned migration target depends directly or transitively (including via
top-level Starlark dependency variables) on unmigrated first-party packages that
still have other GN callers requiring bazel2gn synchronization and ancestor
test_suite registration.
