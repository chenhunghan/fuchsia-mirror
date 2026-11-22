# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Golden file verification targets for fidlgen_rust_next."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:golden_files.bzl", "verify_golden_files")
load("//tools/fidl/fidlc/testdata:fidlgen_rustfmt_golden.bzl", "fidlgen_rustfmt_golden")
load("//tools/fidl/fidlc/testdata:info.bzl", "fidl_testdata_info")

visibility("private")

# LINT.IfChange(golden_tests)
def fidlgen_rust_next_golden_tests(*, name, visibility = None):
    """Defines golden file verification targets for fidlgen_rust_next.

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
        crate = "fidl_next_" + lib_underscored
        common_crate = "fidl_next_common_" + lib_underscored
        gen_name = "generate_golden_" + info["name"]
        gen_common_name = gen_name + "_common"

        fidlgen_rustfmt_golden(
            name = gen_name,
            testonly = True,
            out = "golden_candidates/%s.rs" % crate,
            config = ":configs/fuchsia.json",
            extra_args = [
                "--common-lib",
                common_crate,
            ],
            fidl_ir_json = info["target"],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":fidlgen_rust_next",
            visibility = ["//visibility:private"],
        )
        fidlgen_rustfmt_golden(
            name = gen_common_name,
            testonly = True,
            out = "golden_candidates/%s.rs" % common_crate,
            config = ":configs/common.json",
            fidl_ir_json = info["target"],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            tool = ":fidlgen_rust_next",
            visibility = ["//visibility:private"],
        )

        candidate_files.append(":" + gen_name)
        candidate_files.append(":" + gen_common_name)
        golden_files.append("goldens/%s.rs.golden" % info["name"])
        golden_files.append("goldens/%s_common.rs.golden" % info["name"])

    verify_golden_files(
        name = name,
        testonly = True,
        candidate_files = candidate_files,
        formatter_args = [
            "--config-path",
            "$(location //:rustfmt.toml)",
        ],
        formatter_executable = "@fuchsia_prebuilt_rust//:bin/rustfmt",
        formatter_inputs = [
            "//:rustfmt.toml",
            "@fuchsia_prebuilt_rust//:rustc_lib",
        ],
        golden_files = golden_files,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
    )

# LINT.ThenChange(BUILD.gn:golden_tests)
