# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `driver_bind_bytecode()`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageResourcesInfo")
load("//build/bazel/rules/bind:providers.bzl", "DriverBindBytecodeInfo")

visibility("private")

def _driver_bind_bytecode_info_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    info = target[DriverBindBytecodeInfo]
    asserts.equals(
        env,
        [ctx.attr.expected_bind_output_file_name],
        [f.basename for f in target[DefaultInfo].files.to_list()],
    )
    asserts.equals(env, ctx.attr.expected_src_file_name, info.src.basename if info.src else "")
    asserts.equals(
        env,
        ["meta/bind/" + ctx.attr.expected_bind_output_file_name],
        [r.dest for r in target[FuchsiaPackageResourcesInfo].resources],
    )
    return analysistest.end(env)

driver_bind_bytecode_info_test = analysistest.make(
    _driver_bind_bytecode_info_test_impl,
    doc = "Checks the providers of a `driver_bind_bytecode()` target.",
    attrs = {
        "expected_bind_output_file_name": attr.string(
            doc = "The expected file name of the compiled bytecode file.",
            mandatory = True,
        ),
        "expected_src_file_name": attr.string(
            doc = "The expected file name of the bind rules file, or empty if there is none.",
            mandatory = True,
        ),
    },
)
