# Host platform constraints bazel2gn parity

- **Learned from:** human review on CL Ib37092a0900fa5e7c7d1e451ffde020d395dfbd1, patchset 3 (task `ffx-lib-fastboot-flash-manifest`)
- **Date:** 2026-10-30
- **Changed:** `checks/bazel_minimality.sh`, `checks/cc_header_deps_and_annotations.sh`, `checks/gn_attr_parity.sh`

## Root cause

When a host-only library or test in a host-only subtree or with host-only
dependencies was migrated to BUILD.bazel with target_compatible_with =
HOST_OS_CONSTRAINTS, gn_attr_parity (check_unrequested_gn_conditions) only
inspected target-local if (is_host) blocks in the package's base BUILD.gn and
ignored ancestor BUILD.gn assert(is_host) guards, host-only dependency
constraints (HOST_OS_CONSTRAINTS), and host-only package test layouts. To
suppress the resulting false-positive unrequested_gn_condition error on the
bazel2gn-generated if (is_host) block, the coder attached # @bazel2gn:skip to
target_compatible_with = HOST_OS_CONSTRAINTS and added a misleading comment
claiming the target was not gated by is_host in GN (prompted by
cc_header_deps_and_annotations undocumented_bazel2gn_directive), while
bazel_minimality did not reject # @bazel2gn:skip on target_compatible_with =
HOST_OS_CONSTRAINTS.

## Why not a one-off fix

Fixing only a single package leaves gn_attr_parity rejecting bazel2gn-generated
if (is_host) blocks across any host-only library whose base BUILD.gn relied on
ancestor assert(is_host) guards or host-only dependencies rather than a local if
(is_host) wrapper, and leaves bazel_minimality and
cc_header_deps_and_annotations permitting (and requesting comments on) #
@bazel2gn:skip on target_compatible_with = HOST_OS_CONSTRAINTS instead of
letting bazel2gn emit if (is_host) in BUILD.gn.
