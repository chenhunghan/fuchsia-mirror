# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for merging component manifest files with `cmc merge`."""

load(
    "@fuchsia_rules_common//components:component_manifest.bzl",
    "merge_component_manifests",
)
load(
    "//build/bazel/rules/components:cml_shard.bzl",
    "FuchsiaCmlShardInfo",
)

visibility(["//..."])

def _cmc_merge_impl(ctx):
    merged_output = merge_component_manifests(
        ctx = ctx,
        cmc_tool = ctx.executable._cmc_tool,
        srcs = ctx.files.srcs,
        output_name = ctx.attr.output_name,
    )
    transitive_shards = depset(
        [merged_output],
        transitive = [
            dep[FuchsiaCmlShardInfo].transitive_shards
            for dep in ctx.attr.srcs
            if FuchsiaCmlShardInfo in dep
        ],
    )

    return [
        DefaultInfo(files = depset([merged_output])),
        FuchsiaCmlShardInfo(
            shard = merged_output,
            transitive_shards = transitive_shards,
        ),
    ]

cmc_merge = rule(
    doc = """Merges component manifest files (`.cml`, `.shard.cml`, or `.json`) via `cmc merge`.

    JSON objects are merged recursively (failing on conflicting scalar keys),
    and JSON arrays are concatenated with duplicate items removed. This is the
    Bazel counterpart of GN's `cmc_merge()` template in `//tools/cmc/build/cmc.gni`.
    """,
    implementation = _cmc_merge_impl,
    provides = [FuchsiaCmlShardInfo],
    attrs = {
        "srcs": attr.label_list(
            doc = "Component manifest files (`cml_shard` targets or `.cml`, `.shard.cml`, or `.json` files) to merge, in order.",
            allow_files = [".cml", ".json"],
            mandatory = True,
        ),
        "output_name": attr.string(
            doc = "Output file name relative to this package's output directory. Defaults to `<name>.cml`.",
        ),
        "_cmc_tool": attr.label(
            doc = "The path to the component manifest compiler (cmc) tool.",
            default = "//tools/cmc:cmc",
            executable = True,
            cfg = "exec",
        ),
    },
)
