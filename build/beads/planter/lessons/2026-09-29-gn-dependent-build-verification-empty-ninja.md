# Gn dependent build verification empty ninja outputs

- **Learned from:** inner loop friction on a local task (task `devices-io-buffer`)
- **Date:** 2026-09-29
- **Changed:** `checks/build_verification.sh`

## Root cause

load_toolchains() in checks/build_verification.sh indexed all keys present in
<build dir>/ninja_outputs.json without checking whether the mapped Ninja output
list was non-empty. Header-only GN source_set targets are recorded in
ninja_outputs.json with an empty list ([]), and passing those GN labels to fx
build causes ninja to fail with 'ninja: error: empty path'.

## Why not a one-off fix

Any migrated C/C++ library across the tree can have header-only GN source_set
targets or reverse dependencies in ninja_outputs.json with empty output lists.
Filtering out empty output lists in load_toolchains() prevents false
gn_build_dependents failures and invalid Test: footer suggestions across all
migrations.
