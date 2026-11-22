# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `fx_sharded_test_package`, `testshard_config`, and `testshard_offer_shard`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load(
    "//build/bazel/rules/testing:defs.bzl",
    "analysis_failure_test",
)
load("//build/bazel/rules/testing:fx_test.bzl", "FuchsiaTestInfo")
load(
    "//src/lib/testing/sharding/rules:shard_config.bzl",
    "testshard_config",
    "testshard_offer_shard",
)

visibility("private")

def _testshard_config_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    resources = target[FuchsiaPackageResourcesInfo].resources
    asserts.equals(env, 1, len(resources))
    asserts.equals(env, ctx.attr.expected_dest, resources[0].dest)

    actions = analysistest.target_actions(env)
    asserts.equals(env, 1, len(actions))
    asserts.equals(
        env,
        json.decode(ctx.attr.expected_json),
        json.decode(actions[0].content),
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_testshard_config_test = analysistest.make(
    _testshard_config_test_impl,
    attrs = {
        "expected_dest": attr.string(mandatory = True),
        "expected_json": attr.string(mandatory = True),
    },
)

def _testshard_offer_shard_test_impl(ctx):
    env = analysistest.begin(ctx)
    actions = analysistest.target_actions(env)
    asserts.equals(env, 1, len(actions))

    expected = {
        "offer": [
            {
                "directory": "pkg",
                "from": "framework",
                "as": "testshard",
                "to": "#sharder",
                "subdir": ctx.attr.expected_subdir,
            },
        ],
    }
    asserts.equals(env, expected, json.decode(actions[0].content))

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_testshard_offer_shard_test = analysistest.make(
    _testshard_offer_shard_test_impl,
    attrs = {
        "expected_subdir": attr.string(mandatory = True),
    },
)

def _sharded_fx_test_analysis_test_impl(ctx):
    env = analysistest.begin(ctx)
    test_info = analysistest.target_under_test(env)[FuchsiaTestInfo]

    asserts.equals(
        env,
        ctx.attr.expected_component_names,
        [c.component_name for c in test_info.test_components],
    )
    asserts.equals(
        env,
        ctx.attr.expected_package_urls,
        [c.package_url for c in test_info.test_components],
    )
    if ctx.attr.expected_max_log_severity:
        asserts.equals(
            env,
            ctx.attr.expected_max_log_severity,
            test_info.max_log_severity,
        )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

sharded_fx_test_analysis_test = analysistest.make(
    _sharded_fx_test_analysis_test_impl,
    doc = "Verifies the expanded `test_components` names, URLs, and log severity on an `fx_sharded_test_package()` target.",
    attrs = {
        "expected_component_names": attr.string_list(
            doc = "Expected ordered `component_name` values.",
            mandatory = True,
        ),
        "expected_package_urls": attr.string_list(
            doc = "Expected ordered `package_url` values.",
            mandatory = True,
        ),
        "expected_max_log_severity": attr.string(
            doc = "Optional expected `max_log_severity` value.",
        ),
    },
)

def sharding_rules_test_suite(*, name, visibility = None):
    """Defines host-compatible analysis tests for the sharding helper rules.

    Args:
        name: Name of the `test_suite` target.
        visibility: Visibility of the `test_suite` target.
    """
    testshard_config(
        name = "sample_config_plain",
        testonly = True,
        num_shards = 3,
        shard_component_name = "example-test_shard_1_of_3",
        shard_index = 1,
        tags = ["manual"],
    )

    _testshard_config_test(
        name = "testshard_config_plain_test",
        expected_dest = "data/testshards/example-test_shard_1_of_3_testshard/config.json5",
        expected_json = json.encode({
            "num_shards": 3,
            "shard_index": 1,
        }),
        target_under_test = ":sample_config_plain",
    )

    testshard_config(
        name = "sample_config_with_regex",
        testonly = True,
        num_shards = 3,
        shard_component_name = "example-test_shard_2_of_3",
        shard_index = 2,
        shard_part_regex = r"section_(\d+)::case_.+",
        tags = ["manual"],
    )

    _testshard_config_test(
        name = "testshard_config_with_regex_test",
        expected_dest = "data/testshards/example-test_shard_2_of_3_testshard/config.json5",
        expected_json = json.encode({
            "num_shards": 3,
            "shard_index": 2,
            "shard_part_regex": r"section_(\d+)::case_.+",
        }),
        target_under_test = ":sample_config_with_regex",
    )

    testshard_config(
        name = "too_few_shards_config",
        testonly = True,
        num_shards = 1,
        shard_component_name = "bad",
        shard_index = 0,
        tags = ["manual"],
    )

    analysis_failure_test(
        name = "too_few_shards_config_failure_test",
        expected_message = "`num_shards` must be at least 2, got 1.",
        target_under_test = ":too_few_shards_config",
    )

    testshard_config(
        name = "out_of_range_shard_index_config",
        testonly = True,
        num_shards = 2,
        shard_component_name = "bad",
        shard_index = 2,
        tags = ["manual"],
    )

    analysis_failure_test(
        name = "out_of_range_shard_index_failure_test",
        expected_message = "`shard_index` (2) must be in [0, 2).",
        target_under_test = ":out_of_range_shard_index_config",
    )

    testshard_offer_shard(
        name = "sample_offer_shard",
        testonly = True,
        shard_component_name = "example-test_shard_0_of_3",
        tags = ["manual"],
    )

    _testshard_offer_shard_test(
        name = "testshard_offer_shard_test",
        expected_subdir = "data/testshards/example-test_shard_0_of_3_testshard",
        target_under_test = ":sample_offer_shard",
    )

    native.test_suite(
        name = name,
        visibility = visibility,
        tests = [
            ":out_of_range_shard_index_failure_test",
            ":testshard_config_plain_test",
            ":testshard_config_with_regex_test",
            ":testshard_offer_shard_test",
            ":too_few_shards_config_failure_test",
        ],
    )
