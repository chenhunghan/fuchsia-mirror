# Visibility rdep attribution for GN template-generated sub-targets (fidl library binding targets such as <name> rust/<name> cpp/<name> hlcpp)

- **Learned from:** inner loop friction on CL I8a72d619c70f2f7790c6c233a5712af117663649, patchset 4 (task `starnix-wave-next`)
- **Date:** 2026-09-30
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

visibility_audit.sh and find_rdeps.sh attributed a caller to a Bazel target only
on an exact name match (or the default target when the package has a single
target). GN callers of a FIDL library depend on fidl() binding sub-targets like
:<name>_rust, which never match the fidl_library name. So the per-target
reverse-dependency list for the fidl_library was empty. The check then flagged
the correct caller visibility as false_positive_rdep_visibility, contradicting
target_parity's request to keep that visibility.

## Why not a one-off fix

Every dual-built FIDL library whose GN callers use its generated language
bindings (almost all of them) would hit the same contradiction. Accepting or
deleting the visibility entry in one directory either makes the library private
in Bazel or leaves the audit failing on every future FIDL migration. Only fixing
the rdep mapping in the check and the tool resolves the whole class.
