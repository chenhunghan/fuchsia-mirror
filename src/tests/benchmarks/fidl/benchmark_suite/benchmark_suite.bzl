# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Definitions and helpers for the FIDL GIDL benchmark suite."""

load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")

visibility([
    "//src/tests/benchmarks/fidl/...",
])

# LINT.IfChange
_BENCHMARK_SUITE_DECLARATIONS = [
    {
        "name": "bool_array",
        "gidl": ["bool_array.gen.gidl"],
        "fidl": ["bool_array.gen.test.fidl"],
    },
    {
        "name": "byte_array",
        "gidl": ["byte_array.gen.gidl"],
        "fidl": ["byte_array.gen.test.fidl"],
    },
    {
        "name": "byte_vector",
        "gidl": ["byte_vector.gen.gidl"],
        "fidl": ["byte_vector.test.fidl"],
        "contains_drivers": True,
    },
    {
        "name": "byte_vector_limit",
        "gidl": ["byte_vector_limit.gen.gidl"],
        "fidl": ["byte_vector_limit.gen.test.fidl"],
    },
    {
        "name": "enum_array",
        "gidl": ["enum_array.gen.gidl"],
        "fidl": ["enum_array.gen.test.fidl"],
    },
    {
        "name": "enum_array_non_dense",
        "gidl": ["enum_array_non_dense.gen.gidl"],
        "fidl": ["enum_array_non_dense.gen.test.fidl"],
    },
    {
        "name": "float_array",
        "gidl": ["float_array.gen.gidl"],
        "fidl": ["float_array.gen.test.fidl"],
    },
    {
        "name": "gpu_magma",
        "gidl": ["gpu_magma.gen.gidl"],
        "fidl": ["gpu_magma.test.fidl"],
    },
    {
        "name": "handle_array_event",
        "gidl": ["handle_array_event.gen.gidl"],
        "fidl": [
            "handle_array_event.gen.test.fidl",
            "handle_array_event_protocol.test.fidl",
        ],
        "contains_drivers": True,
    },
    {
        "name": "handle_array_plain",
        "gidl": ["handle_array_plain.gen.gidl"],
        "fidl": ["handle_array_plain.gen.test.fidl"],
    },
    {
        "name": "hardware_display",
        "gidl": ["hardware_display.gen.gidl"],
        "fidl": ["hardware_display.test.fidl"],
    },
    {
        "name": "inline",
        "gidl": ["inline.gidl"],
        "fidl": ["inline.test.fidl"],
    },
    {
        "name": "int32_array",
        "gidl": ["int32_array.gen.gidl"],
        "fidl": ["int32_array.gen.test.fidl"],
    },
    {
        "name": "int32_vector",
        "gidl": ["int32_vector.gen.gidl"],
        "fidl": ["int32_vector.test.fidl"],
    },
    {
        "name": "media",
        "gidl": ["media.gidl"],
        "fidl": ["media.test.fidl"],
        "contains_drivers": True,
    },
    {
        "name": "optional_struct_tree",
        "gidl": ["optional_struct_tree.gen.gidl"],
        "fidl": ["optional_struct_tree.test.fidl"],
    },
    {
        "name": "padded_struct_tree",
        "gidl": ["padded_struct_tree.gen.gidl"],
        "fidl": ["padded_struct_tree.gen.test.fidl"],
    },
    {
        "name": "optional_union_tree",
        "gidl": ["optional_union_tree.gen.gidl"],
        "fidl": ["optional_union_tree.test.fidl"],
    },
    {
        "name": "result",
        "gidl": ["result.gidl"],
        "fidl": ["result.test.fidl"],
    },
    {
        "name": "socket",
        "gidl": ["socket.gidl"],
        "fidl": ["socket.test.fidl"],
    },
    {
        "name": "string",
        "gidl": ["string.gidl"],
        "fidl": ["string.test.fidl"],
    },
    {
        "name": "struct_array",
        "gidl": ["struct_array.gen.gidl"],
        "fidl": ["struct_array.gen.test.fidl"],
    },
    {
        "name": "struct_field",
        "gidl": ["struct_field.gen.gidl"],
        "fidl": ["struct_field.gen.test.fidl"],
    },
    {
        "name": "struct_tree",
        "gidl": ["struct_tree.gen.gidl"],
        "fidl": ["struct_tree.gen.test.fidl"],
    },
    {
        "name": "struct_vector",
        "gidl": ["struct_vector.gen.gidl"],
        "fidl": ["struct_vector.test.fidl"],
    },
    {
        "name": "table_all_set",
        "gidl": ["table_all_set.gen.gidl"],
        "fidl": ["table.gen.test.fidl"],
    },
    {
        "name": "table_all_unset",
        "gidl": ["table_all_unset.gen.gidl"],
        "fidl": ["table.gen.test.fidl"],
    },
    {
        "name": "table_single_set_gen",
        "gidl": ["table_single_set.gen.gidl"],
        "fidl": ["table.gen.test.fidl"],
    },
    {
        "name": "table_reserved_last_set",
        "gidl": ["table_reserved_last_set.gen.gidl"],
        "fidl": ["table_reserved.gen.test.fidl"],
    },
    {
        "name": "union",
        "gidl": ["union.gen.gidl"],
        "fidl": ["union.gen.test.fidl"],
    },
]

_BENCHMARK_SUITE_PKG = "//src/tests/benchmarks/fidl/benchmark_suite:"

def _compute_benchmark_suites():
    gidl_files = {}
    fidl_files = {}
    suites = []
    unique_fidl_suites = {}

    for decl in _BENCHMARK_SUITE_DECLARATIONS:
        decl_gidl_labels = [_BENCHMARK_SUITE_PKG + g for g in decl["gidl"]]
        for g in decl["gidl"]:
            gidl_files[g] = True
        for f in decl["fidl"]:
            fidl_files[f] = True

        fidl_library_suffix = decl["fidl"][0].split(".")[0].replace("_", "")
        fidl_target_name = "benchmark_suite_fidl_" + fidl_library_suffix
        fidl_target = _BENCHMARK_SUITE_PKG + fidl_target_name

        suite = {
            "name": decl["name"],
            "fidl": decl["fidl"],
            "contains_drivers": decl.get("contains_drivers", False),
            "gidl": decl_gidl_labels,
            "fidl_library_suffix": fidl_library_suffix,
            "fidl_target_name": fidl_target_name,
            "fidl_target": fidl_target,
        }
        suites.append(suite)
        if fidl_target_name not in unique_fidl_suites:
            unique_fidl_suites[fidl_target_name] = suite

    return (
        gidl_files.keys(),
        fidl_files.keys(),
        suites,
        unique_fidl_suites.values(),
    )

(
    BENCHMARK_SUITE_GIDL_FILES,
    BENCHMARK_SUITE_FIDL_FILES,
    BENCHMARK_SUITES,
    UNIQUE_BENCHMARK_FIDL_SUITES,
) = _compute_benchmark_suites()

BENCHMARK_SUITE_FIDL_TARGET = "//src/tests/benchmarks/fidl/benchmark_suite:benchmark_suite_fidl"
# LINT.ThenChange(//src/tests/benchmarks/fidl/benchmark_suite/benchmark_suite.gni)

def _copy_replace_fidl_library_impl(ctx):
    outputs = []
    for src in ctx.files.srcs:
        out = ctx.actions.declare_file("%s_%s" % (ctx.attr.library_name, src.basename))
        outputs.append(out)
        ctx.actions.run(
            executable = ctx.executable._tool,
            arguments = [src.path, out.path, ctx.attr.library_name],
            inputs = [src],
            outputs = [out],
            mnemonic = "CopyReplaceFidlLibrary",
            progress_message = "Copying FIDL file %s to library %s" % (src.short_path, ctx.attr.library_name),
        )
    return [DefaultInfo(files = depset(outputs))]

_copy_replace_fidl_library = rule(
    doc = "Copies `.fidl` files and replaces `library test.benchmarkfidl;` with `library <library_name>;`.",
    implementation = _copy_replace_fidl_library_impl,
    attrs = {
        "srcs": attr.label_list(
            allow_files = [".fidl"],
            mandatory = True,
        ),
        "library_name": attr.string(
            mandatory = True,
        ),
        "_tool": attr.label(
            default = "//src/tests/benchmarks/fidl/benchmark_suite:copy_replace_fidl_library",
            executable = True,
            cfg = "exec",
        ),
    },
)

def benchmark_fidl(*, name, fidl_library_suffix, srcs, contains_drivers = False, visibility = None):
    """Declares a per-suite FIDL library with `library test.benchmarkfidl<suffix>;`.

    Args:
      name: Name of the generated `fidl_library` target.
      fidl_library_suffix: Suffix appended to `test.benchmarkfidl`.
      srcs: List of `.fidl` source files.
      contains_drivers: Whether the FIDL library uses the driver transport.
      visibility: Visibility of the generated `fidl_library`.
    """
    library_name = "test.benchmarkfidl" + fidl_library_suffix
    copy_target = name + "__copy_replace"

    _copy_replace_fidl_library(
        name = copy_target,
        testonly = True,
        srcs = srcs,
        library_name = library_name,
        visibility = ["//visibility:private"],
    )

    fidl_library(
        name = name,
        testonly = True,
        srcs = [":" + copy_target],
        contains_drivers = contains_drivers,
        enable_hlcpp = True,
        library_name = library_name,
        visibility = visibility,
        deps = ["//zircon/vdso/zx"],
    )
