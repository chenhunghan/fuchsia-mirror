# Centralized idk and fidl category allowlist registration scope

- **Learned from:** inner loop friction on a local task (task `pkg-realm-proxy-fidl`)
- **Date:** 2026-09-30
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

The bazel_minimality check allowed centralized registration in sdk/BUILD.gn but
did not include sdk/fidl/category_lists.bzl, sdk/fidl/BUILD.gn, or
sdk/atom_lists.bzl in is_allowed_scope(), even though fidl_library and IDK atom
macros call verify_target_is_in_allowlist() in
//build/bazel/rules/idk/private/idk_common.bzl requiring categorized targets
(such as category = 'compat_test', 'host_tool', 'prebuilt', or 'partner') to
register their _idk target in those centralized lists.

## Why not a one-off fix

Exempting only a single FIDL package would fail for any other categorized
fidl_library or IDK atom target outside //sdk/fidl that must register its
<name>_idk target in //sdk/fidl:category_lists.bzl or //sdk:atom_lists.bzl to
pass verify_target_is_in_allowlist() during Bazel analysis.
