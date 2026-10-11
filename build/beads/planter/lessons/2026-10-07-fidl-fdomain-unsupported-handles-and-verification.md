# Fidl fdomain unsupported handles and verification gni scope

- **Learned from:** inner loop friction on a local task (task `starnix-bootreason`)
- **Date:** 2026-10-07
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_verification.sh`

## Root cause

Two deterministic check gaps caused inner-loop friction when migrating a FIDL
library dependency under //sdk/fidl/: (1) build_verification.sh built
//<dir>:all without excluding <fidl>_rust_fdomain and <fidl>_rust_fdomain_flex
when a fidl_library's .fidl sources use zx.Handle subtypes (such as
zx.Handle:LOG) that fidlgen_rust maps to types missing from fdomain_client, and
fidl_library unconditionally defines those targets without manual tags; (2)
bazel_minimality.sh omitted sdk/fidl/bazel2gn_verification_targets.gni from its
allowed centralized registration files even though migration_sanity.sh requires
registering //sdk/fidl/<pkg>:verify_bazel2gn there.

## Why not a one-off fix

Hardcoding a single FIDL package would fail on every future FIDL migration under
//sdk/fidl/ that registers verify_bazel2gn in
sdk/fidl/bazel2gn_verification_targets.gni or uses zx.Handle subtypes
unsupported by fdomain_client in its .fidl files.
