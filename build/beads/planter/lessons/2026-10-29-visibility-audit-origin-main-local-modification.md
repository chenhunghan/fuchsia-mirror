# Visibility audit origin main local modification shadowing

- **Learned from:** inner loop friction on a local task (task `ffx-tools-uart`)
- **Date:** 2026-10-29
- **Changed:** `checks/visibility_audit.sh`

## Root cause

When scanning origin/main to discover callers added upstream in lagging
checkouts or sub-repositories, checks/visibility_audit.sh recorded every file
matching on origin/main in origin_main_sources and concatenated
origin/main:<file> onto the working-tree file whenever their contents differed.
For any caller BUILD.gn or BUILD.bazel modified by the current migration change
(such as replacing a direct sub-target test dependency with :tests or removing a
migrated GN target reference), the working-tree file differed from origin/main
precisely because of the local migration edit, causing visibility_audit.sh to
resurrect the pre-migration dependency from origin/main and report a
false-positive uncovered_rdep_visibility error.

## Why not a one-off fix

Hardcoding an exception for a specific package or target name would fail
whenever any multi-package or caller-updating migration modifies a referencing
BUILD.gn, BUILD.bazel, or .gni file to update or remove a dependency label that
previously existed on origin/main. Filtering origin_main_sources via git diff
HEAD...origin/main minus locally modified files ensures across all repositories
and packages that origin/main is only consulted for files with genuine
upstream-only modifications.
