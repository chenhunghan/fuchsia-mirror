# Rebased or amended change base resolution and missing prebuilt ninja classification

- **Learned from:** inner loop friction on CL If1afbe49d3a4fbf4da55a10044879190b255500e, patchset 3 (task `src-lib-diagnostics-inspect-rust-bench-batch`)
- **Date:** 2026-10-31
- **Changed:** `checks/build_verification.sh`, `checks/gn_attr_parity.sh`

## Root cause

When a migration commit is amended or rebased onto a newer upstream revision
during follow-up rounds, PLANTER_CHANGE_BASE can point to a pre-amend or
pre-rebase SHA that is no longer the commit parent. gn_attr_parity.sh only fell
back to HEAD~1 when base_sha == head_sha with an empty diff, so after an amend
or rebase it missed packages unchanged since the prior patchset and failed to
emit INFO dep_alignment coverage for GN forwarding-group dependencies. In
addition, rebasing fuchsia.git onto a newer origin/main commit without syncing
CIPD packages can leave Ninja referencing a missing prebuilt binary under
prebuilt/ (exit code 127: No such file or directory), which
build_verification.sh did not classify as an out-of-sync prebuilt checkout
failure.

## Why not a one-off fix

Fixing only a single package directory or a single binary target would leave
every future multi-package migration that amends or rebases its commit across
review rounds vulnerable to stale PLANTER_CHANGE_BASE diffing in
gn_attr_parity.sh and false build_failure_gn_build errors when Ninja encounters
an unfetched prebuilt binary after a git rebase.
