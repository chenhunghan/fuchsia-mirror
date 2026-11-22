# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""IR roundtrip validation targets for fidlgen."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:compiled_action.bzl", "compiled_action")
load("//build/json:validate_json.bzl", "validate_json")
load("//tools/fidl/fidlc/testdata:info.bzl", "fidl_testdata_info")

visibility("private")

# LINT.IfChange(roundtrip_tests)
# Perform a round-trip of the IR: valid JSON IR is read into its Go
# representation by fidlgen, marshaled back to JSON, and validated against
# schema.json.
def fidlgen_roundtrip_tests(*, name, visibility = None):
    """Defines FIDL JSON IR roundtrip validation targets.

    Args:
        name: Name of the aggregating `filegroup` target.
        visibility: Visibility of the aggregating `filegroup` target.
    """
    validate_targets = []

    for info in fidl_testdata_info:
        roundtrip_target = "roundtrip." + info["name"]
        validate_target = "validate." + info["name"]
        out_json = info["name"] + ".json"

        compiled_action(
            name = roundtrip_target,
            testonly = True,
            srcs = [info["target"]],
            outs = [out_json],
            args = [
                "-in",
                "$(location %s)" % info["target"],
                "-out",
                "$(location :%s)" % out_json,
            ],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":roundtripper",
            visibility = ["//visibility:private"],
        )
        validate_json(
            name = validate_target,
            testonly = True,
            data = ":" + roundtrip_target,
            schema = "//tools/fidl/fidlc:schema.json",
            target_compatible_with = HOST_OS_CONSTRAINTS,
            visibility = ["//visibility:private"],
        )
        validate_targets.append(":" + validate_target)

    native.filegroup(
        name = name,
        testonly = True,
        srcs = validate_targets,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
    )

# LINT.ThenChange(//tools/fidl/lib/fidlgen/BUILD.gn:roundtrip_tests)
