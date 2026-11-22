# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_migration_bazel2gn_overwrites."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_migration_bazel2gn_overwrites",
)
load("./base_test.star", "run_lint_test")

_EXPLANATION = (
    "Use of bazel2gn overwrites requires approval by //build/bazel/BAZEL_OWNERS. " +
    "Add an entry to //scripts/shac/bazel_lint/allowlists/bazel_migration_bazel2gn_overwrites.json " +
    "with a tracking bug. When GN targets are removed and bazel2gn annotations deleted, " +
    "the allowlist entry must be removed so //build/bazel/BAZEL_OWNERS can review the deleted comment " +
    "to ensure the annotation was not hiding a bug."
)

def _run_migration_bazel2gn_overwrites_test(files):
    return run_lint_test(
        _bazel_migration_bazel2gn_overwrites,
        "bazel_migration_bazel2gn_overwrites",
        files,
    )

def test_migration_bazel2gn_overwrites():
    res = _run_migration_bazel2gn_overwrites_test({
        "foo/BUILD.bazel": """
cc_library(
    name = "my_target",
    srcs = [
        "foo.cc",  # @bazel2gn:path_overwrite:bar.cc
    ],  # @bazel2gn:raw_overwrite:[ "//*" ]
    copts = select({
        "//conditions:default": [
            "-O2",  # @bazel2gn:value_overwrite:-O3
        ],
    }),
)

# @bazel2gn:raw_overwrite:[]
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:path_overwrite:bar.cc`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 5,
            col = 20,
            end_col = 53,
        ),
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:raw_overwrite:[ \"//*\" ]`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 6,
            col = 9,
            end_col = 44,
        ),
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:value_overwrite:-O3`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 9,
            col = 21,
            end_col = 52,
        ),
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:raw_overwrite:[]`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 14,
            col = 1,
            end_col = 29,
        ),
    ))

def test_migration_bazel2gn_overwrites_allowed():
    res = _run_migration_bazel2gn_overwrites_test({
        "foo/BUILD.bazel": """
# Standard comment about bazel2gn
cc_library(
    name = "allowed_target",
    srcs = ["foo.cc"],
    # @bazel2gn:skip
    doc = "# @bazel2gn:path_overwrite:not_a_real_comment",
)
""",
    })
    asserts.eq(res.findings, ())

def test_migration_bazel2gn_overwrites_bzl():
    res = _run_migration_bazel2gn_overwrites_test({
        "foo/defs.bzl": """
def my_macro(name, srcs):
    native.cc_library(
        name = name,
        srcs = srcs,  # @bazel2gn:raw_overwrite:[]
    )
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:raw_overwrite:[]`. " +
                _EXPLANATION
            ),
            filepath = "foo/defs.bzl",
            line = 5,
            col = 23,
            end_col = 51,
        ),
    ))

def test_migration_bazel2gn_overwrites_known_exclusions():
    res = _run_migration_bazel2gn_overwrites_test({
        "bar/BUILD.bazel": """
fx_packaged_binary(
    name = "my_bin_packaged",
)
""",
        "foo/BUILD.bazel": """
cc_library(
    name = "exempt_targets",
    deps = [
        "//src/lib/diagnostics/inspect/rust:fuchsia-inspect",  # @bazel2gn:path_overwrite://src/lib/diagnostics/inspect/rust
        "@com_google_googletest//:gtest",  # @bazel2gn:path_overwrite://third_party/googletest:gmock
        "//third_party/rust_crates/forks/prettytable-rs:prettytable",  # @bazel2gn:path_overwrite://third_party/rust_crates:prettytable-rs
        "//src/zircon/lib/zircon:headers",  # @bazel2gn:path_overwrite://zircon/system/public
        "@fuchsia_perfetto_protos//:protos/perfetto/ipc/consumer_port.proto",  # @bazel2gn:path_overwrite://third_party/perfetto/protos/perfetto/ipc/consumer_port.proto
        "//bar:my_bin_packaged",  # @bazel2gn:path_overwrite://bar:my_bin
        "//third_party/golibs:github.com/google/go-cmp/cmp",  # @bazel2gn:path_overwrite://third_party/golibs:github.com/google/go-cmp
        "//:integration/MILESTONE",  # @bazel2gn:path_overwrite://integration/MILESTONE
    ],
    srcs = [
        "src/lib.rs",  # @bazel2gn:path_overwrite:$target_gen_dir/src/lib.rs
        "schema.h",  # @bazel2gn:path_overwrite:${target_gen_dir}/schema.h
        "scudo.cpp",  # @bazel2gn:path_overwrite:${gwp_asan}/scudo.cpp
    ],
    compile_data = [
        ":dispatcher_cm",  # @bazel2gn:path_overwrite:${dispatcher_cm}
    ],
    copts = [
        "-Wconversion",  # @bazel2gn:raw_overwrite:[ "//build/config:Wno-conversion" ]
    ],
)

idk_cc_source_library(
    name = "sdk_lib",
    visibility = [":__pkg__"],  # @bazel2gn:raw_overwrite:[ "//*" ]
)

fidl_library(
    name = "my_fidl",
    visibility = [":__pkg__"],  # @bazel2gn:raw_overwrite:[ "//*" ]
)

rustc_library(
    name = "my_rust",
    crate_root = "src/main.rs",  # @bazel2gn:raw_overwrite:crate_root = "$target_gen_dir/src/main.rs"
    rustc_env = {
        "CFG": "$(location :cfg)",  # @bazel2gn:value_overwrite:${rebased_config_path}
    },
)

go_test(
    name = "my_go_test",
    srcs = ["test.go"],  # @bazel2gn:raw_overwrite:non_go_deps = [ ":host_tool" ]
)
""",
    })
    asserts.eq(res.findings, ())

def test_migration_bazel2gn_overwrites_unexcluded():
    res = _run_migration_bazel2gn_overwrites_test({
        "foo/BUILD.bazel": """
cc_library(
    name = "foo",
    deps = [
        "//some/unrelated:target",  # @bazel2gn:path_overwrite://another/path
        "//:d3_v3_js",  # @bazel2gn:path_overwrite://scripts/third_party/d3_v3/d3.js
    ],
    copts = [
        "-O2",  # @bazel2gn:raw_overwrite:[ "//build/config:all_source" ]
    ],
)
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:path_overwrite://another/path`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 5,
            col = 37,
            end_col = 78,
        ),
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:path_overwrite://scripts/third_party/d3_v3/d3.js`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 6,
            col = 25,
            end_col = 85,
        ),
        testing.finding(
            level = "error",
            message = (
                "bazel2gn overwrite comment is an exception requiring further review: " +
                "`# @bazel2gn:raw_overwrite:[ \"//build/config:all_source\" ]`. " +
                _EXPLANATION
            ),
            filepath = "foo/BUILD.bazel",
            line = 9,
            col = 17,
            end_col = 74,
        ),
    ))

def run_migration_bazel2gn_overwrites_tests():
    """Runs all tests for bazel_migration_bazel2gn_overwrites."""
    test_migration_bazel2gn_overwrites()
    test_migration_bazel2gn_overwrites_allowed()
    test_migration_bazel2gn_overwrites_bzl()
    test_migration_bazel2gn_overwrites_known_exclusions()
    test_migration_bazel2gn_overwrites_unexcluded()
