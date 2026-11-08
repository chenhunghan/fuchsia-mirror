# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis and build tests for //build/bazel/aspects:pyrefly.bzl."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("//build/bazel/aspects:pyrefly.bzl", "pyrefly_aspect")
load("//build/bazel/rules:stamp_group.bzl", "STAMP_GROUP_NON_DEPS_ATTRS")

visibility("private")

def _pyrefly_analysis_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    pyrefly_actions = [a for a in target.actions if a.mnemonic == "Pyrefly"]
    has_pyrefly_group = (
        OutputGroupInfo in target and
        hasattr(target[OutputGroupInfo], "pyrefly_checks")
    )
    has_validation_group = (
        OutputGroupInfo in target and
        hasattr(target[OutputGroupInfo], "_validation")
    )

    if ctx.attr.expect_pyrefly:
        asserts.equals(env, 1, len(pyrefly_actions), "Expected 1 Pyrefly action.")
        asserts.true(env, has_pyrefly_group, "Expected pyrefly_checks output group.")
        asserts.true(env, has_validation_group, "Expected _validation output group.")
        if pyrefly_actions and ctx.attr.expected_search_paths:
            argv = pyrefly_actions[0].argv
            for sp in ctx.attr.expected_search_paths:
                asserts.true(env, sp in argv, "Expected search path %s in %s." % (sp, argv))
    else:
        asserts.equals(env, 0, len(pyrefly_actions), "Expected 0 Pyrefly actions.")
        asserts.false(env, has_pyrefly_group, "Expected no pyrefly_checks output group.")

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

pyrefly_analysis_test = analysistest.make(
    _pyrefly_analysis_test_impl,
    extra_target_under_test_aspects = [pyrefly_aspect],
    attrs = {
        "expect_pyrefly": attr.bool(mandatory = True),
        "expected_search_paths": attr.string_list(default = []),
    },
)

def _pyrefly_validation_files_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    validation_files = []
    if OutputGroupInfo in target and hasattr(target[OutputGroupInfo], "_validation"):
        validation_files = sorted([f.basename for f in target[OutputGroupInfo]._validation.to_list()])
    asserts.equals(env, sorted(ctx.attr.expected_validation_files), validation_files)
    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

pyrefly_validation_files_test = analysistest.make(
    _pyrefly_validation_files_test_impl,
    extra_target_under_test_aspects = [pyrefly_aspect],
    attrs = {
        "expected_validation_files": attr.string_list(mandatory = True),
    },
)

def _pyrefly_build_only_tests_group_impl(ctx):
    output = ctx.outputs.stamp
    dep_files = [dep[DefaultInfo].files for dep in ctx.attr.deps]
    pyrefly_files = [
        dep[OutputGroupInfo].pyrefly_checks
        for dep in ctx.attr.deps
        if OutputGroupInfo in dep and hasattr(dep[OutputGroupInfo], "pyrefly_checks")
    ]
    ctx.actions.run(
        outputs = [output],
        inputs = depset(transitive = dep_files + pyrefly_files),
        executable = ctx.file._script,
        arguments = [output.path],
    )
    return [DefaultInfo(files = depset([output]))]

pyrefly_build_only_tests_group = rule(
    implementation = _pyrefly_build_only_tests_group_impl,
    doc = "Builds `deps` and runs their `pyrefly_aspect` checks, writing a stamp file.",
    attrs = {
        "deps": attr.label_list(
            aspects = [pyrefly_aspect],
            mandatory = True,
        ),
    } | STAMP_GROUP_NON_DEPS_ATTRS,
)

def pyrefly_test_suite(name):
    """Defines analysis tests for pyrefly_aspect targets in BUILD.bazel.

    Args:
      name: Name of the generated test_suite target.
    """
    cases = [
        ("lib_default", ":pyrefly_valid_lib", True, []),
        ("pyi_lib_default", ":pyrefly_valid_pyi_lib", True, []),
        ("lib_disabled", ":pyrefly_type_error_optout", False, []),
        ("bin_default", ":pyrefly_valid_bin", True, ["build/bazel/aspects/tests/testdata"]),
        ("host_py_test_default", ":pyrefly_host_test_default_bin", True, ["build/bazel/aspects/tests/testdata"]),
        ("host_py_test_disabled", ":pyrefly_host_test_disabled_bin", False, []),
    ]
    tests = []
    for case_name, target_under_test, expect_pyrefly, expected_search_paths in cases:
        test_name = case_name + "_pyrefly_analysis_test"
        pyrefly_analysis_test(
            name = test_name,
            target_under_test = target_under_test,
            expect_pyrefly = expect_pyrefly,
            expected_search_paths = expected_search_paths,
        )
        tests.append(":" + test_name)

    # Checks are transitive over `deps` and `pyi_deps`, and host_test() forwards
    # the checks of its binary, which Bazel would not otherwise request.
    validation_cases = [
        ("bin_transitive", ":pyrefly_valid_bin", [
            "pyrefly_valid_bin.pyrefly.ok",
            "pyrefly_valid_lib.pyrefly.ok",
            "pyrefly_valid_pyi_lib.pyrefly.ok",
        ]),
        ("host_py_test_forwarded", ":pyrefly_host_test_default", [
            "pyrefly_host_test_default_bin.pyrefly.ok",
            "pyrefly_valid_lib.pyrefly.ok",
        ]),
        ("host_py_test_disabled_forwarded", ":pyrefly_host_test_disabled", []),
    ]
    for case_name, target_under_test, expected_validation_files in validation_cases:
        test_name = case_name + "_pyrefly_validation_test"
        pyrefly_validation_files_test(
            name = test_name,
            target_under_test = target_under_test,
            expected_validation_files = expected_validation_files,
        )
        tests.append(":" + test_name)

    native.test_suite(
        name = name,
        visibility = ["//visibility:private"],
        tests = tests,
    )
