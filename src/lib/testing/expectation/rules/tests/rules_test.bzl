# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `fx_expectations`, `fx_generated_expectations`, and `preprocess_expectations`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component_manifest",
)
load(
    "//build/bazel/rules/testing:defs.bzl",
    "analysis_failure_test",
)
load(
    "//src/lib/testing/expectation/rules:fx_expectations.bzl",
    "fx_expectations",
)
load(
    "//src/lib/testing/expectation/rules:fx_generated_expectations.bzl",
    "fx_generated_expectations",
)
load(
    "//src/lib/testing/expectation/rules:fx_test_component_with_expectations.bzl",
    "fx_component_manifest_with_expectations",
)
load(
    "//src/lib/testing/expectation/rules:preprocess.bzl",
    "FuchsiaExpectationsInfo",
    "preprocess_expectations",
)

visibility("private")

def _expectations_provider_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    asserts.true(
        env,
        FuchsiaExpectationsInfo in target,
        "Expected FuchsiaExpectationsInfo on target under test.",
    )
    info = target[FuchsiaExpectationsInfo]
    asserts.equals(
        env,
        ctx.attr.expected_root_short_path,
        info.root_file.short_path,
    )

    actual_default_files = sorted([
        f.short_path
        for f in target[DefaultInfo].files.to_list()
    ])
    asserts.equals(
        env,
        [ctx.attr.expected_root_short_path],
        actual_default_files,
    )

    actual_includes = sorted([
        f.short_path
        for f in info.includes.to_list()
    ])
    asserts.equals(
        env,
        sorted(ctx.attr.expected_include_short_paths),
        actual_includes,
    )

    if ctx.attr.expected_generated_includes:
        actions = analysistest.target_actions(env)
        asserts.equals(env, 1, len(actions))
        decoded = json.decode(actions[0].content)
        expected_actions = [
            {"include": inc}
            for inc in ctx.attr.expected_generated_includes
        ]
        asserts.equals(env, {"actions": expected_actions}, decoded)

    if env.failures:
        # Fail during analysis so `bazel build` build-only suites also catch
        # regressions.
        fail("\n".join(env.failures))
    return analysistest.end(env)

_expectations_provider_test = analysistest.make(
    _expectations_provider_test_impl,
    attrs = {
        "expected_root_short_path": attr.string(mandatory = True),
        "expected_include_short_paths": attr.string_list(mandatory = True),
        "expected_generated_includes": attr.string_list(),
    },
)

def _preprocess_expectations_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    asserts.true(
        env,
        FuchsiaPackageResourcesInfo in target,
        "Expected FuchsiaPackageResourcesInfo on target under test.",
    )
    resources = target[FuchsiaPackageResourcesInfo].resources
    asserts.equals(env, 1, len(resources))
    asserts.equals(env, ctx.attr.expected_dest, resources[0].dest)

    actions = [
        a
        for a in analysistest.target_actions(env)
        if a.mnemonic == "PreprocessExpectations"
    ]
    asserts.equals(env, 1, len(actions))
    action = actions[0]

    argv = action.argv
    asserts.true(
        env,
        "--cases-to-run" in argv,
        "Expected `--cases-to-run` in action argv: {}".format(argv),
    )
    cases_idx = argv.index("--cases-to-run")
    asserts.equals(env, ctx.attr.expected_cases_to_run, argv[cases_idx + 1])

    input_short_paths = {
        f.short_path: True
        for f in action.inputs.to_list()
    }
    for expected_input in ctx.attr.expected_input_short_paths:
        asserts.true(
            env,
            expected_input in input_short_paths,
            "Expected `{}` in action inputs, got: {}".format(
                expected_input,
                sorted(input_short_paths.keys()),
            ),
        )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

_preprocess_expectations_test = analysistest.make(
    _preprocess_expectations_test_impl,
    attrs = {
        "expected_dest": attr.string(mandatory = True),
        "expected_cases_to_run": attr.string(mandatory = True),
        "expected_input_short_paths": attr.string_list(mandatory = True),
    },
)

def expectation_rules_test_suite(name, visibility = None):
    """Defines analysis tests for the expectation Bazel rules.

    Args:
        name: Name of the `test_suite` target.
        visibility: Visibility of the `test_suite` target.
    """
    fx_expectations(
        name = "leaf_expectations",
        testonly = True,
        expectations = "//src/lib/testing/expectation:included_expectations.json5",
        tags = ["manual"],
    )

    _expectations_provider_test(
        name = "fx_expectations_leaf_test",
        target_under_test = ":leaf_expectations",
        expected_root_short_path = "src/lib/testing/expectation/included_expectations.json5",
        expected_include_short_paths = [],
    )

    fx_expectations(
        name = "chained_expectations",
        testonly = True,
        expectations = "//src/lib/testing/expectation:example_expectations.json5",
        includes = [
            ":leaf_expectations",
            "//src/lib/testing/expectation:example_expectations_part2.json5",
        ],
        tags = ["manual"],
    )

    _expectations_provider_test(
        name = "fx_expectations_transitive_includes_test",
        target_under_test = ":chained_expectations",
        expected_root_short_path = "src/lib/testing/expectation/example_expectations.json5",
        expected_include_short_paths = [
            "src/lib/testing/expectation/example_expectations_part2.json5",
            "src/lib/testing/expectation/included_expectations.json5",
        ],
    )

    fx_generated_expectations(
        name = "sample_generated_expectations",
        testonly = True,
        includes = [
            "//src/lib/testing/expectation:example_expectations_part1.json5",
            "//src/lib/testing/expectation:example_expectations_part2.json5",
        ],
        tags = ["manual"],
    )

    _expectations_provider_test(
        name = "fx_generated_expectations_test",
        target_under_test = ":sample_generated_expectations",
        expected_root_short_path = "src/lib/testing/expectation/rules/tests/sample_generated_expectations.json5",
        expected_include_short_paths = [
            "src/lib/testing/expectation/default_expectations.json5",
            "src/lib/testing/expectation/example_expectations_part1.json5",
            "src/lib/testing/expectation/example_expectations_part2.json5",
        ],
        expected_generated_includes = [
            "//src/lib/testing/expectation/default_expectations.json5",
            "//src/lib/testing/expectation/example_expectations_part1.json5",
            "//src/lib/testing/expectation/example_expectations_part2.json5",
        ],
    )

    fx_component_manifest_with_expectations(
        name = "sample_manifest_with_expectations",
        component_name = "custom-component-name",
        includes = ["//src/lib/testing/expectation:meta/client.shard"],
        manifest = "//src/lib/testing/expectation:meta/test_root.cml",
        tags = ["manual"],
    )

    preprocess_expectations(
        name = "preprocess_default_cases",
        testonly = True,
        compiled_manifest = ":sample_manifest_with_expectations",
        expectations = ":sample_generated_expectations",
        tags = ["manual"],
    )

    _preprocess_expectations_test(
        name = "preprocess_expectations_default_test",
        target_under_test = ":preprocess_default_cases",
        expected_dest = "data/tests/expectations/custom-component-name/expectations.json5",
        expected_cases_to_run = "All",
        expected_input_short_paths = [
            "src/lib/testing/expectation/default_expectations.json5",
            "src/lib/testing/expectation/example_expectations_part1.json5",
            "src/lib/testing/expectation/example_expectations_part2.json5",
            "src/lib/testing/expectation/rules/tests/sample_generated_expectations.json5",
        ],
    )

    preprocess_expectations(
        name = "preprocess_direct_file",
        testonly = True,
        compiled_manifest = ":sample_manifest_with_expectations",
        expectations = "//src/lib/testing/expectation:included_expectations.json5",
        tags = ["manual"],
    )

    _preprocess_expectations_test(
        name = "preprocess_expectations_direct_file_test",
        target_under_test = ":preprocess_direct_file",
        expected_dest = "data/tests/expectations/custom-component-name/expectations.json5",
        expected_cases_to_run = "All",
        expected_input_short_paths = [
            "src/lib/testing/expectation/included_expectations.json5",
        ],
    )

    preprocess_expectations(
        name = "preprocess_no_err_logs",
        testonly = True,
        cases_to_run = "NoErrLogs",
        compiled_manifest = ":sample_manifest_with_expectations",
        expectations = ":chained_expectations",
        tags = ["manual"],
    )

    _preprocess_expectations_test(
        name = "preprocess_expectations_no_err_logs_test",
        target_under_test = ":preprocess_no_err_logs",
        expected_dest = "data/tests/expectations/custom-component-name/expectations.json5",
        expected_cases_to_run = "NoErrLogs",
        expected_input_short_paths = [
            "src/lib/testing/expectation/example_expectations.json5",
            "src/lib/testing/expectation/example_expectations_part2.json5",
            "src/lib/testing/expectation/included_expectations.json5",
        ],
    )

    preprocess_expectations(
        name = "preprocess_with_err_logs",
        testonly = True,
        cases_to_run = "WithErrLogs",
        compiled_manifest = ":sample_manifest_with_expectations",
        expectations = ":leaf_expectations",
        tags = ["manual"],
    )

    _preprocess_expectations_test(
        name = "preprocess_expectations_with_err_logs_test",
        target_under_test = ":preprocess_with_err_logs",
        expected_dest = "data/tests/expectations/custom-component-name/expectations.json5",
        expected_cases_to_run = "WithErrLogs",
        expected_input_short_paths = [
            "src/lib/testing/expectation/included_expectations.json5",
        ],
    )

    fx_component_manifest(
        name = "plain_manifest",
        testonly = True,
        includes = ["//src/lib/testing/expectation:meta/client.shard"],
        manifest = "//src/lib/testing/expectation:meta/test_root.cml",
        tags = ["manual"],
    )

    preprocess_expectations(
        name = "preprocess_plain_manifest",
        testonly = True,
        compiled_manifest = ":plain_manifest",
        expectations = ":leaf_expectations",
        tags = ["manual"],
    )

    analysis_failure_test(
        name = "preprocess_expectations_plain_manifest_failure_test",
        expected_message = "was not created by `fx_component_manifest_with_expectations()`",
        target_under_test = ":preprocess_plain_manifest",
    )

    native.test_suite(
        name = name,
        visibility = visibility,
        tests = [
            ":fx_expectations_leaf_test",
            ":fx_expectations_transitive_includes_test",
            ":fx_generated_expectations_test",
            ":preprocess_expectations_default_test",
            ":preprocess_expectations_direct_file_test",
            ":preprocess_expectations_no_err_logs_test",
            ":preprocess_expectations_plain_manifest_failure_test",
            ":preprocess_expectations_with_err_logs_test",
        ],
    )
