# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `py_data_package()`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_python//python:py_info.bzl", "PyInfo")
load("//build/bazel/rules/python:py_data_package.bzl", "PyDataPackageInfo")

visibility("private")

def _py_data_package_providers_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    py_info = target[PyInfo]
    data_info = target[PyDataPackageInfo]

    asserts.equals(
        env,
        sorted(ctx.attr.expected_transitive_source_short_paths),
        sorted([f.short_path for f in py_info.transitive_sources.to_list()]),
    )
    asserts.equals(
        env,
        sorted(ctx.attr.expected_data_short_paths),
        sorted([f.short_path for f in data_info.files.to_list()]),
    )
    asserts.equals(
        env,
        sorted(ctx.attr.expected_imports),
        sorted(py_info.imports.to_list()),
    )

    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

py_data_package_providers_test = analysistest.make(
    _py_data_package_providers_test_impl,
    doc = "Checks `PyInfo` and `PyDataPackageInfo` emitted by `py_data_package()`.",
    attrs = {
        "expected_transitive_source_short_paths": attr.string_list(
            doc = "Expected `short_path` values in `PyInfo.transitive_sources`.",
            mandatory = True,
        ),
        "expected_data_short_paths": attr.string_list(
            doc = "Expected `short_path` values in `PyDataPackageInfo.files`.",
            mandatory = True,
        ),
        "expected_imports": attr.string_list(
            doc = "Expected entries in `PyInfo.imports`.",
            default = [],
        ),
    },
)
