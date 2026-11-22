# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Declares a FIDL library for use in the FIDL toolchain golden testing."""

load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")
load(":info.bzl", "fidl_testdata_info")

visibility([
    "//tools/fidl/fidlc/testdata/...",
])

_TESTDATA_INFO_BY_TARGET_NAME = {
    info["target_name"]: info
    for info in fidl_testdata_info
}

# LINT.IfChange
def fidl_golden_library(
        *,
        name,
        experimental_flags = None,
        enable_cpp = None,
        enable_hlcpp = None,
        enable_rust = None,
        enable_rust_next = None,
        **kwargs):
    """Declares a FIDL library for use in the FIDL toolchain golden testing.

    See //build/bazel/rules/fidl:fidl_library.bzl for parameters.

    Args:
        name: Target name of the FIDL library.
        experimental_flags: Optional list of experimental fidlc flags.
        enable_cpp: Optional boolean to override C++ bindings generation.
        enable_hlcpp: Optional boolean to override HLCPP bindings generation.
        enable_rust: Optional boolean to override Rust bindings generation.
        enable_rust_next: Optional boolean to override Rust Next bindings generation.
        **kwargs: Additional arguments forwarded to `fidl_library()`.
    """
    flags = list(experimental_flags) if experimental_flags else []
    if "output_index_json" not in flags:
        flags.append("output_index_json")

    info = _TESTDATA_INFO_BY_TARGET_NAME.get(name, {})
    build_denylist = info.get("build_denylist", [])

    if enable_cpp == None:
        enable_cpp = "fidlgen_cpp" not in build_denylist
    if enable_hlcpp == None:
        enable_hlcpp = "fidlgen_hlcpp" not in build_denylist
    if enable_rust == None:
        enable_rust = "fidlgen_rust" not in build_denylist
    if enable_rust_next == None:
        enable_rust_next = "fidlgen_rust_next" not in build_denylist

    fidl_library(
        name = name,
        available = [
            "fuchsia:HEAD",
            "test:HEAD",
        ],
        enable_cpp = enable_cpp,
        enable_hlcpp = enable_hlcpp,
        enable_rust = enable_rust,
        enable_rust_next = enable_rust_next,
        experimental_flags = flags,
        testonly = True,
        visibility = [
            "//src/devices/tools/fidlgen_banjo:__subpackages__",
            "//tools/fidl:__subpackages__",
        ],
        **kwargs
    )

# LINT.ThenChange(fidl_golden_library.gni)
