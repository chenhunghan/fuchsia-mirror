# Host binary exclude toolchain tags and bazel host tool parity

- **Learned from:** inner loop friction on a local task (task `src-diagnostics-triage`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

gn_attr_parity only exempted exclude_toolchain_tags for *_binary_host_tool
wrapper macros and only recorded extra_attrs alignment notes when
exclude_toolchain_tags remained in BUILD.gn below the sentinel. When a host
binary using a standard rule (such as rustc_binary or cc_binary) with tags =
['exclude_toolchain_tags=<tag>'] is marked # @bazel2gn:skip and wrapped by
bazel_host_tool in BUILD.gn (as required by
migration_sanity/dual_built_host_tool), BUILD.gn has no generated binary
carrying exclude_toolchain_tags, causing gn_attr_parity to report
gn_only_attr_dropped and review_diff to warn on attribute_alignment.

## Why not a one-off fix

Any GN host binary or non-sentinel Bazel target that carries
exclude_toolchain_tags via Bazel tags or is bridged to GN via bazel_host_tool
will trigger the same false-positive gn_only_attr_dropped and review_diff
attribute_alignment findings unless gn_attr_parity inspects Bazel
exclude_toolchain_tags tags and recognizes standard binary rules paired with
bazel_host_tool across all packages.
