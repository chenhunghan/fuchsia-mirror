# Commit message check derives the migrated directory without the layout relocation rules (GN secondary overlay tree) and requires a migration subject for blocked changes that migrate nothing

- **Learned from:** inner loop friction on [CL 1841583](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841583), patchset 6 (task `cl-1841583`)
- **Date:** 2026-10-02
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

commit_message_format took PLANTER_TARGET_DIR literally as the migrated
directory. For a build/secondary/<dir> target, its remediation required
'[bazel_migration] //build/secondary/...', which contradicts gn_secondary_tree:
that check forbids Bazel files in the GN-only overlay tree and requires the
migration to land in //<dir>. The check also demanded a migration subject even
when the commit changed no files because the coder correctly blocked and
reverted. The coder then had to either claim a migration that did not happen or
leave a disputed WARNING that made run_checks exit 1.

## Why not a one-off fix

Rewording this one CL's subject or special-casing its directory leaves the
contradiction in place for every other third-party library still built from the
GN secondary overlay tree, and for any future migration that is blocked and
reverted to an empty change. The fix belongs in the check's general logic:
overlay directories map to the directory they overlay, and empty changes are
exempt from the migration-subject requirement.
