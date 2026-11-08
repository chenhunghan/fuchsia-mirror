# Bazel2gn and gn prebuilt version sync verification

- **Learned from:** cq failure on CL If360d6314b1adc3c4597bbbdcaea551770e5b024, patchset 13 (task `src-sys-run-test-suite`)
- **Date:** 2026-10-30
- **Changed:** `checks/build_verification.sh`, `checks/gn_format.sh`

## Root cause

The local checkout's prebuilt GN binary differed from the git_revision pinned in
manifests/toolchain while local origin/main was stale, and static checks only
ran `gn format --dry-run` rather than `bazel2gn --check_only` on touched
BAZEL2GN SENTINEL directories, allowing BUILD.gn files formatted with a
mismatched GN prebuilt or out of sync with BUILD.bazel to pass local checks and
fail CQ `verify_bazel2gn`. Additionally, `build_verification.sh` did not include
the prebuilt `gn` and compiled host `bazel2gn` binary digests in its
result-reuse fingerprint.

## Why not a one-off fix

Re-syncing only the specific directories in this task would not prevent future
dual-build migrations from failing CQ `verify_bazel2gn` whenever upstream rolls
`manifests/toolchain` (`gn/gn`), local `origin/main` is stale, a prebuilt GN
binary in a worktree drifts from `manifests/toolchain`, or `BUILD.gn` drifts
from `BUILD.bazel` while still satisfying `gn format --dry-run`.
