# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Golden file verification targets for fidlgen_rust_next_convert."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load("//tools/fidl/fidlc/testdata:fidlgen_rustfmt_golden.bzl", "fidlgen_rustfmt_golden")
load("//tools/fidl/fidlc/testdata:info.bzl", "fidl_testdata_info")

visibility("private")

# LINT.IfChange(golden_tests)
def fidlgen_rust_next_convert_golden_tests(*, name, visibility = None):
    """Defines golden file verification targets for fidlgen_rust_next_convert.

    Args:
        name: Name of the `verify_golden_files` target.
        visibility: Visibility of the `verify_golden_files` target.
    """
    candidate_files = []
    golden_files = []

    for info in fidl_testdata_info:
        if "fidlgen_rust_next" in info["denylist"]:
            continue

        lib_underscored = info["library"].replace(".", "_")
        gen_name = "generate_golden_" + info["name"]

        fidlgen_rustfmt_golden(
            name = gen_name,
            testonly = True,
            out = "golden_candidates/fidl_next_convert_%s.rs" % lib_underscored,
            extra_args = [
                "--rust-crate",
                "fidl_" + lib_underscored,
                "--rust-next-crate",
                "fidl_next_" + lib_underscored,
            ],
            fidl_ir_json = info["target"],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":fidlgen_rust_next_convert",
            visibility = ["//visibility:private"],
        )

        candidate_files.append(":" + gen_name)
        golden_files.append("goldens/%s.rs.golden" % info["name"])

    verify_golden_files(
        name = name,
        testonly = True,
        candidate_files = candidate_files,
        golden_files = golden_files,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
    )

# LINT.ThenChange(BUILD.gn:golden_tests)
