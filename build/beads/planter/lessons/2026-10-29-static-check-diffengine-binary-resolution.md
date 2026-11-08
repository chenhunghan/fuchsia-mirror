# Static check diffengine binary resolution

- **Learned from:** inner loop friction on a local task (task `src-developer-ffx-tools-echo`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

The gn_attr_parity check script invoked find_planter_bin() inside
collect_diffengine_alignment_findings() to query planter diff --local
--check-findings, and needed consistent resolution of the planter executable
across PLANTER_BIN, running daemon /proc/<pid>/exe entries, PATH, and fallback
install locations so the check never crashes or diverges from review_diff.

## Why not a one-off fix

Every GN-to-Bazel migration task executes gn_attr_parity and
collect_diffengine_alignment_findings() during run_checks.sh, so ensuring
find_planter_bin() is defined and resolves the active planter binary
consistently in checks/gn_attr_parity.sh is required across all package
migrations rather than for any single target directory.
