# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `bind_library()`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@fuchsia_rules_common//bind:providers.bzl", "FuchsiaBindLibraryInfo")

visibility("private")

def _bind_library_info_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    info = target[FuchsiaBindLibraryInfo]
    asserts.equals(env, ctx.attr.expected_name, info.name)
    asserts.equals(
        env,
        sorted([f.short_path for f in ctx.files.expected_transitive_sources]),
        sorted([f.short_path for f in info.transitive_sources.to_list()]),
    )

    return analysistest.end(env)

bind_library_info_test = analysistest.make(
    _bind_library_info_test_impl,
    doc = "Checks the `FuchsiaBindLibraryInfo` of a `bind_library()`.",
    attrs = {
        "expected_name": attr.string(
            doc = "The expected library name.",
            mandatory = True,
        ),
        "expected_transitive_sources": attr.label_list(
            doc = "The expected `.bind` sources of the library and of the libraries it includes.",
            mandatory = True,
            allow_files = [".bind"],
        ),
    },
)
