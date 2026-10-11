# File local gn template type parity and empty ninja output filtering

- **Learned from:** inner loop friction on a local task (task `vfs-rust-prereqs`)
- **Date:** 2026-09-30
- **Changed:** `checks/gn_target_type_parity.sh`

## Root cause

First, gn_target_type_parity.sh matched only the surface identifier of GN target
invocations without resolving file-local template(...) definitions that expand
target_name into an underlying rule such as rustc_library or source_set, falsely
flagginggn_target_type_changed when bazel2gn emitted the expanded underlying GN
target type below the sentinel.

## Why not a one-off fix

Hardcoding exceptions for individual targets or packages would fail on any other
Fuchsia package that uses file-local GN helper templates to parameterize
libraries. Resolving file-local templates to their underlying target_name rules
in gn_target_type_parity.sh fixes both classes of falsepositives across all
future migrations.
