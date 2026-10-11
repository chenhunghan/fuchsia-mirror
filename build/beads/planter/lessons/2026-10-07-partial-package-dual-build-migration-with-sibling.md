# Partial package dual-build migration with sibling targets blocked by GN-only dependency chains

- **Learned from:** inner loop friction on a local task (task `starnix-leaf-libs`)
- **Date:** 2026-10-07
- **Changed:** `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh flagged every convertible GN template above the
BAZEL2GN SENTINEL unless the target itself matched GN_ONLY_ATTR_RE, ignoring
non-duplicated sibling targets in partially migrated packages whose unmigrated
dependencies rely on GN-only attributes, loadable_module targets, or
cross-toolchain dependencies.

## Why not a one-off fix

Exempting specific package paths or target names would fail whenever another
multi-target package migrates leaf libraries to Bazel while keeping higher-level
sibling libraries above the sentinel due to transitive GN-only dependency
blockers.
