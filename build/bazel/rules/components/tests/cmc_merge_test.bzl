# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `cmc_merge`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "//build/bazel/rules/components:cmc_merge.bzl",
    "cmc_merge",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
)
load(
    "//build/bazel/rules/testing:defs.bzl",
    "analysis_failure_test",
)

visibility("private")

def _write_file_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.filename)
    ctx.actions.write(output = out, content = ctx.attr.content)
    return [DefaultInfo(files = depset([out]))]

_write_file = rule(
    implementation = _write_file_impl,
    attrs = {
        "filename": attr.string(mandatory = True),
        "content": attr.string(mandatory = True),
    },
)

def _cmc_merge_action_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    files = target[DefaultInfo].files.to_list()
    asserts.equals(env, 1, len(files))
    asserts.equals(env, ctx.attr.expected_output_short_path, files[0].short_path)

    actions = [
        a
        for a in analysistest.target_actions(env)
        if a.mnemonic == "CmcMerge"
    ]
    asserts.equals(env, 1, len(actions))
    argv = actions[0].argv
    asserts.equals(env, "merge", argv[1])
    asserts.equals(env, "--output", argv[2])
    asserts.equals(env, files[0].path, argv[3])

    actual_tail = argv[4:]
    asserts.equals(env, len(ctx.attr.expected_tail_suffixes), len(actual_tail))
    for expected_suffix, actual_arg in zip(ctx.attr.expected_tail_suffixes, actual_tail):
        asserts.true(
            env,
            actual_arg == expected_suffix or actual_arg.endswith("/" + expected_suffix),
            "Expected arg `{}` to match `{}`.".format(actual_arg, expected_suffix),
        )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_cmc_merge_action_test = analysistest.make(
    _cmc_merge_action_test_impl,
    attrs = {
        "expected_output_short_path": attr.string(mandatory = True),
        "expected_tail_suffixes": attr.string_list(mandatory = True),
    },
)

def _fx_manifest_debugdata_merge_test_impl(ctx):
    env = analysistest.begin(ctx)

    merge_actions = [
        a
        for a in analysistest.target_actions(env)
        if a.mnemonic == "CmcMerge"
    ]
    if not ctx.attr.expected_shard_short_path:
        asserts.equals(env, 0, len(merge_actions))
        if env.failures:
            fail("\n".join(env.failures))
        return analysistest.end(env)

    asserts.equals(env, 1, len(merge_actions))
    merge_out = merge_actions[0].outputs.to_list()
    asserts.equals(env, 1, len(merge_out))
    asserts.equals(env, ctx.attr.expected_merged_short_path, merge_out[0].short_path)

    argv = merge_actions[0].argv
    asserts.equals(env, "merge", argv[1])
    asserts.equals(env, "--output", argv[2])
    asserts.equals(env, merge_out[0].path, argv[3])
    asserts.equals(env, 6, len(argv))
    asserts.true(
        env,
        argv[4] == ctx.attr.expected_shard_short_path or
        argv[4].endswith("/" + ctx.attr.expected_shard_short_path),
        "Expected shard arg `{}` to match `{}`.".format(
            argv[4],
            ctx.attr.expected_shard_short_path,
        ),
    )
    asserts.true(
        env,
        argv[5] == ctx.attr.expected_manifest_short_path or
        argv[5].endswith("/" + ctx.attr.expected_manifest_short_path),
        "Expected manifest arg `{}` to match `{}`.".format(
            argv[5],
            ctx.attr.expected_manifest_short_path,
        ),
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_DEBUGDATA_TEST_ATTRS = {
    "expected_merged_short_path": attr.string(),
    "expected_shard_short_path": attr.string(),
    "expected_manifest_short_path": attr.string(),
}

_fx_manifest_no_merge_test = analysistest.make(
    _fx_manifest_debugdata_merge_test_impl,
    attrs = _DEBUGDATA_TEST_ATTRS,
)

# Flagless platform with OS + CPU constraints so Fuchsia's platform `flags` do
# not override `is_coverage` / `is_profile` while `//build/bazel:current_platform`
# (used by top-level build aspects) still resolves cleanly.
_TEST_PLATFORM = str(Label("//build/bazel/rules/components/tests:flagless_fuchsia_x64_platform"))
_IS_COVERAGE_FLAG = str(Label("//build/bazel/config/variables:is_coverage"))
_IS_PROFILE_FLAG = str(Label("//build/bazel/config/variables:is_profile"))

_fx_manifest_coverage_merge_test = analysistest.make(
    _fx_manifest_debugdata_merge_test_impl,
    attrs = _DEBUGDATA_TEST_ATTRS,
    config_settings = {
        "//command_line_option:platforms": _TEST_PLATFORM,
        _IS_COVERAGE_FLAG: True,
        _IS_PROFILE_FLAG: False,
    },
)

_fx_manifest_profile_merge_test = analysistest.make(
    _fx_manifest_debugdata_merge_test_impl,
    attrs = _DEBUGDATA_TEST_ATTRS,
    config_settings = {
        "//command_line_option:platforms": _TEST_PLATFORM,
        _IS_COVERAGE_FLAG: False,
        _IS_PROFILE_FLAG: True,
    },
)

def cmc_merge_test_suite(name, visibility = None):
    """Defines analysis tests for `cmc_merge`.

    Args:
        name: Name of the `test_suite` target.
        visibility: Visibility of the `test_suite` target.
    """
    _write_file(
        name = "part_a_cml",
        testonly = True,
        content = '{ "program": { "runner": "elf", "binary": "bin/example" } }\n',
        filename = "part_a.shard.cml",
        tags = ["manual"],
    )

    _write_file(
        name = "part_b_cml",
        testonly = True,
        content = "{}\n",
        filename = "part_b.cml",
        tags = ["manual"],
    )

    cmc_merge(
        name = "merged_default_name",
        testonly = True,
        srcs = [
            ":part_a_cml",
            ":part_b_cml",
        ],
        tags = ["manual"],
    )

    _cmc_merge_action_test(
        name = "cmc_merge_default_name_test",
        target_under_test = ":merged_default_name",
        expected_output_short_path = "build/bazel/rules/components/tests/merged_default_name.cml",
        expected_tail_suffixes = [
            "build/bazel/rules/components/tests/part_a.shard.cml",
            "build/bazel/rules/components/tests/part_b.cml",
        ],
    )

    cmc_merge(
        name = "merged_custom_output",
        testonly = True,
        output_name = "custom/merged.cml",
        srcs = [":part_a_cml"],
        tags = ["manual"],
    )

    _cmc_merge_action_test(
        name = "cmc_merge_custom_output_test",
        target_under_test = ":merged_custom_output",
        expected_output_short_path = "build/bazel/rules/components/tests/custom/merged.cml",
        expected_tail_suffixes = [
            "build/bazel/rules/components/tests/part_a.shard.cml",
        ],
    )

    cmc_merge(
        name = "merged_empty_inputs",
        testonly = True,
        srcs = [],
        tags = ["manual"],
    )

    analysis_failure_test(
        name = "cmc_merge_empty_inputs_failure_test",
        expected_message = "`srcs` must not be empty.",
        target_under_test = ":merged_empty_inputs",
    )

    cmc_merge(
        name = "merged_with_shard",
        testonly = True,
        srcs = [
            ":part_a_cml",
            "//sdk/lib/syslog:client.shard",
        ],
        tags = ["manual"],
    )

    fx_component_manifest(
        name = "compiled_merged_manifest",
        testonly = True,
        manifest = ":merged_with_shard",
    )

    native.filegroup(
        name = "build_only_tests",
        testonly = True,
        srcs = [":compiled_merged_manifest"],
        visibility = visibility,
    )

    native.platform(
        name = "flagless_fuchsia_x64_platform",
        constraint_values = [
            "@platforms//os:fuchsia",
            "@platforms//cpu:x86_64",
        ],
        tags = ["manual"],
    )

    fx_component_manifest(
        name = "example_fx_manifest",
        testonly = True,
        manifest = ":part_b_cml",
        tags = ["manual"],
    )

    _fx_manifest_no_merge_test(
        name = "fx_manifest_no_debugdata_merge_test",
        target_under_test = ":example_fx_manifest",
    )

    _fx_manifest_coverage_merge_test(
        name = "fx_manifest_coverage_merge_test",
        expected_manifest_short_path = "build/bazel/rules/components/tests/part_b.cml",
        expected_merged_short_path = "build/bazel/rules/components/tests/example_fx_manifest_merge.cml",
        expected_shard_short_path = "sdk/lib/sys/testing/coverage.shard.cml",
        target_under_test = ":example_fx_manifest",
    )

    _fx_manifest_profile_merge_test(
        name = "fx_manifest_profile_merge_test",
        expected_manifest_short_path = "build/bazel/rules/components/tests/part_b.cml",
        expected_merged_short_path = "build/bazel/rules/components/tests/example_fx_manifest_merge.cml",
        expected_shard_short_path = "build/config/profile/instrumentation_data.cml",
        target_under_test = ":example_fx_manifest",
    )

    native.test_suite(
        name = name,
        visibility = visibility,
        tests = [
            ":cmc_merge_custom_output_test",
            ":cmc_merge_default_name_test",
            ":cmc_merge_empty_inputs_failure_test",
            ":fx_manifest_coverage_merge_test",
            ":fx_manifest_no_debugdata_merge_test",
            ":fx_manifest_profile_merge_test",
        ],
    )
