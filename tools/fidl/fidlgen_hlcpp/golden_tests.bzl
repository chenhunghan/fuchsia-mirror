# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Golden file verification targets for fidlgen_hlcpp."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:compiled_action.bzl", "compiled_action")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load("//tools/fidl/fidlc/testdata:info.bzl", "fidl_testdata_info")

visibility("private")

# LINT.IfChange(golden_tests)
_HLCPP_GOLDEN_SUFFIXES = [
    (".h", "fidl.h"),
    (".cc", "fidl.cc"),
    ("_test_base.h", "fidl_test_base.h"),
    ("_tables.c", "tables.c"),
]

def fidlgen_hlcpp_golden_tests(*, name, visibility = None):
    """Defines golden file verification targets for fidlgen_hlcpp.

    Args:
        name: Name of the `verify_golden_files` target.
        visibility: Visibility of the `verify_golden_files` target.
    """
    candidate_files = []
    golden_files = []

    for info in fidl_testdata_info:
        if "fidlgen_hlcpp" in info["denylist"]:
            continue

        gen_name = "generate_golden_" + info["name"]
        include_dir = info["library"].replace(".", "/")
        outs = []
        for golden_suffix, candidate_relpath in _HLCPP_GOLDEN_SUFFIXES:
            outs.append(
                "golden_%s/%s/cpp/%s" % (
                    info["name"],
                    include_dir,
                    candidate_relpath,
                ),
            )
            golden_files.append(
                "goldens/%s%s.golden" % (info["name"], golden_suffix),
            )

        compiled_action(
            name = gen_name,
            testonly = True,
            srcs = [
                info["target"],
                "@fuchsia_clang//:bin/clang-format",
            ],
            outs = outs,
            args = [
                "--json",
                "$(location %s)" % info["target"],
                "--clang-format-path",
                "$(location @fuchsia_clang//:bin/clang-format)",
                "--root",
                "$(RULEDIR)/golden_%s" % info["name"],
            ],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":fidlgen_hlcpp",
            visibility = ["//visibility:private"],
        )
        candidate_files.append(":" + gen_name)

    verify_golden_files(
        name = name,
        testonly = True,
        candidate_files = candidate_files,
        golden_files = golden_files,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
    )

# LINT.ThenChange(BUILD.gn:golden_tests)
