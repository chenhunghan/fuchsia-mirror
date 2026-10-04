# Build verification attributing failures from an out-of-sync multi-repository checkout (targets in other git repositories that do not depend on the change) to the migration

- **Learned from:** inner loop friction on CL Id44453fc247d9e3c722eb3a42fd00e7a3604b15d, patchset 2 (task `devices-cqhci-spec`)
- **Date:** 2026-09-30
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`

## Root cause

build_verification turned any non-zero `fx build` exit into a blocking ERROR. It
never checked whether the failing Bazel targets were in the change's repository
or had any dependency path to a changed package. So a sub-repository still at an
older revision, which uses a FIDL type the main repository has since removed,
sent a correct build-only migration back again and again with a finding the
coder could not fix within scope.

## Why not a one-off fix

Any migration task run while the checkout's repositories are out of sync hits
the same issue. Fixing it in one target directory, or allowlisting that one
failing target, does nothing for the next unrelated failure. Scoping failures by
repository ownership plus a Bazel somepath dependency query covers every package
and every out-of-sync failure, and a failure the change really causes still
blocks.
