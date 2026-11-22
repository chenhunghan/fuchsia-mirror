# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for generating test sharding config resources and CML offer shards."""

load(
    "@fuchsia_rules_common//:utils.bzl",
    "make_resource_struct",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)

visibility(["//src/lib/testing/sharding/..."])

# LINT.IfChange

def _testshard_config_impl(ctx):
    if ctx.attr.num_shards < 2:
        fail("`num_shards` must be at least 2, got {}.".format(ctx.attr.num_shards))
    if ctx.attr.shard_index < 0 or ctx.attr.shard_index >= ctx.attr.num_shards:
        fail(
            "`shard_index` ({}) must be in [0, {}).".format(
                ctx.attr.shard_index,
                ctx.attr.num_shards,
            ),
        )

    config_file = ctx.actions.declare_file(
        "{}.json5".format(ctx.label.name),
    )
    contents = {
        "num_shards": ctx.attr.num_shards,
        "shard_index": ctx.attr.shard_index,
    }
    if ctx.attr.shard_part_regex:
        contents["shard_part_regex"] = ctx.attr.shard_part_regex

    ctx.actions.write(
        output = config_file,
        content = json.encode_indent(contents, indent = "  ") + "\n",
    )

    dest = "data/testshards/{}_testshard/config.json5".format(
        ctx.attr.shard_component_name,
    )
    return [
        FuchsiaPackageResourcesInfo(
            resources = [make_resource_struct(src = config_file, dest = dest)],
        ),
        DefaultInfo(files = depset([config_file])),
    ]

testshard_config = rule(
    doc = """Generates the `config.json5` package resource for a single test shard at
    `data/testshards/<shard_component_name>_testshard/config.json5`.""",
    implementation = _testshard_config_impl,
    provides = [FuchsiaPackageResourcesInfo],
    attrs = {
        "shard_component_name": attr.string(
            doc = "The shard component name scoping the `data/testshards/` subdirectory.",
            mandatory = True,
        ),
        "num_shards": attr.int(
            doc = "Total number of shards for the component.",
            mandatory = True,
        ),
        "shard_index": attr.int(
            doc = "Zero-based index of this shard (`0 <= shard_index < num_shards`).",
            mandatory = True,
        ),
        "shard_part_regex": attr.string(
            doc = "Optional regex with a single capture group selecting the portion of each test case name to hash.",
        ),
    },
)

def _testshard_offer_shard_impl(ctx):
    offer_shard = ctx.actions.declare_file(
        "{}.shard.cml".format(ctx.label.name),
    )
    offer_contents = json.encode_indent(
        {
            "offer": [
                {
                    "directory": "pkg",
                    "from": "framework",
                    "as": "testshard",
                    "to": "#sharder",
                    "subdir": "data/testshards/{}_testshard".format(
                        ctx.attr.shard_component_name,
                    ),
                },
            ],
        },
        indent = "  ",
    ) + "\n"
    ctx.actions.write(
        output = offer_shard,
        content = offer_contents,
    )
    return [DefaultInfo(files = depset([offer_shard]))]

testshard_offer_shard = rule(
    doc = "Generates the CML shard offering a shard's `data/testshards/` package subdirectory as `testshard` to `#sharder`.",
    implementation = _testshard_offer_shard_impl,
    attrs = {
        "shard_component_name": attr.string(
            doc = "The shard component name scoping the `data/testshards/` subdirectory.",
            mandatory = True,
        ),
    },
)

# LINT.ThenChange(//src/lib/testing/sharding/fuchsia_sharded_test_package.gni)
