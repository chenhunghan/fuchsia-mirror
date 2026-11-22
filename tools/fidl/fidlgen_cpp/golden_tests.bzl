# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Golden file verification targets for fidlgen_cpp."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:compiled_action.bzl", "compiled_action")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load("//tools/fidl/fidlc/testdata:info.bzl", "fidl_testdata_info")

visibility("private")

# LINT.IfChange(golden_tests)
_CPP_GOLDEN_SUFFIXES = [
    ("wire.h", "wire.h"),
    ("wire_test_base.h", "wire_test_base.h"),
    ("markers.h", "markers.h"),
    ("common_types_format.h", "common_types_format.h"),
    ("common_types.h", "common_types.h"),
    ("common_types.cc", "common_types.cc"),
    ("wire_types.h", "wire_types.h"),
    ("wire_types.cc", "wire_types.cc"),
    ("wire_messaging.h", "wire_messaging.h"),
    ("wire_messaging.cc", "wire_messaging.cc"),
    ("natural_types.h", "natural_types.h"),
    ("natural_types.cc", "natural_types.cc"),
    ("natural_ostream.h", "natural_ostream.h"),
    ("natural_ostream.cc", "natural_ostream.cc"),
    ("natural_messaging.h", "natural_messaging.h"),
    ("natural_messaging.cc", "natural_messaging.cc"),
    ("test_base.h", "test_base.h"),
    ("type_conversions.h", "type_conversions.h"),
    ("type_conversions.cc", "type_conversions.cc"),
    ("hlcpp_conversion.h", "hlcpp_conversion.h"),
    ("fidl.h", "fidl.h"),
    ("driver_wire.h", "driver/wire.h"),
    ("driver_wire_test_base.h", "driver/wire_test_base.h"),
    ("driver_wire_messaging.h", "driver/wire_messaging.h"),
    ("driver_wire_messaging.cc", "driver/wire_messaging.cc"),
    ("driver_natural_messaging.h", "driver/natural_messaging.h"),
    ("driver_natural_messaging.cc", "driver/natural_messaging.cc"),
    ("driver_test_base.h", "driver/test_base.h"),
    ("driver_fidl.h", "driver/fidl.h"),
]

def fidlgen_cpp_golden_tests(*, name, visibility = None):
    """Defines golden file verification targets for fidlgen_cpp.

    Args:
        name: Name of the `verify_golden_files` target.
        visibility: Visibility of the `verify_golden_files` target.
    """
    candidate_files = []
    golden_files = []

    for info in fidl_testdata_info:
        if "fidlgen_cpp" in info["denylist"]:
            continue

        gen_name = "generate_golden_" + info["name"]
        outs = []
        for golden_suffix, candidate_relpath in _CPP_GOLDEN_SUFFIXES:
            outs.append(
                "golden_%s/fidl/%s/cpp/%s" % (
                    info["name"],
                    info["library"],
                    candidate_relpath,
                ),
            )
            golden_files.append(
                "goldens/%s_%s.golden" % (info["name"], golden_suffix),
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
                "--experiment",
                "api_coverage",
            ],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":fidlgen_cpp",
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
