# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining an in-tree Fuchsia devicetree visitor."""

load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/packages:fx_packaged_binary.bzl", "fx_packaged_binary")

visibility([
    "//boards/...",
    "//build/bazel/rules/devicetree/...",
    "//examples/...",
    "//sdk/...",
    "//src/...",
    "//vendor/...",
    "//zircon/...",
])

_VISITOR_LD = Label("@fuchsia_rules_common//devicetree:visitor.ld")

def _devicetree_visitor_impl(
        name,
        visibility,
        output_name,
        additional_linker_inputs,
        linkopts,
        local_defines,
        features,
        build_flags,
        disable_build_flags,
        implementation_deps,
        non_sdk_deps,
        enable_sdk_source_set,  # buildifier: disable=unused-variable - For GN conversion only.
        category,  # buildifier: disable=unused-variable - For GN conversion only.
        testonly,
        tags,
        target_compatible_with,
        **kwargs):
    lib_name = name + ".lib"
    static_name = name + ".static"
    shared_lib_name = name + ".so"

    base_output_name = output_name or name.split("/")[-1]
    bin_name = base_output_name if base_output_name.endswith(".so") else base_output_name + ".so"

    internal_tags = (tags or []) + ["manual"]
    merged_impl_deps = (
        implementation_deps if not non_sdk_deps else (non_sdk_deps if implementation_deps == None else implementation_deps + non_sdk_deps)
    )

    # Always-link the library object files so REGISTER_DEVICETREE_VISITOR(...)
    # registration symbols and static constructors are retained when linked into
    # the visitor DSO or into static visitor consumers.
    fx_cc_library(
        name = lib_name,
        alwayslink = True,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        features = features,
        implementation_deps = merged_impl_deps,
        local_defines = local_defines,
        tags = internal_tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = ["//visibility:private"],
        **kwargs
    )

    fx_cc_library(
        name = static_name,
        alwayslink = True,
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        features = features,
        implementation_deps = merged_impl_deps,
        local_defines = local_defines + ["FDF_DEVICETREE_STATIC_VISITOR"],
        tags = tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
        **kwargs
    )

    fx_cc_binary(
        name = shared_lib_name,
        additional_linker_inputs = additional_linker_inputs + [_VISITOR_LD],
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        features = features,
        linkopts = linkopts + [
            "-Wl,--version-script,$(location {})".format(_VISITOR_LD),
        ],
        linkshared = True,
        tags = internal_tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
        deps = [":" + lib_name],
    )

    fx_packaged_binary(
        name = name,
        binary = ":" + shared_lib_name,
        binary_name = bin_name,
        install_root = "lib/visitors/",
        tags = tags,
        target_compatible_with = target_compatible_with,
        testonly = testonly,
        visibility = visibility,
    )

devicetree_visitor = macro(
    doc = """Defines a devicetree visitor target and its static library counterpart.

    Equivalent to GN `devicetree_visitor()` in //build/devicetree/devicetree_visitor.gni.
    For a visitor named `<name>`, it defines:

    * `:<name>`: An `fx_packaged_binary()` wrapping the loadable visitor `.so`
      installed at `lib/visitors/<output_name or name>.so` for use in
      `fx_package(resources = ...)` and `fx_unittest_package(resources = ...)`.
    * `:<name>.static`: An `fx_cc_library()` compiled with
      `FDF_DEVICETREE_STATIC_VISITOR` for statically linking the visitor into
      board drivers or unit tests.
    """,
    implementation = _devicetree_visitor_impl,
    inherit_attrs = fx_cc_library,
    attrs = {
        "output_name": attr.string(
            doc = "Optional basename (without `.so`) of the packaged visitor `.so` under `lib/visitors/`. Defaults to `name`.",
            default = "",
            configurable = False,
        ),
        "additional_linker_inputs": attr.label_list(
            doc = "Additional files passed to the linker when linking the visitor `.so`.",
            allow_files = True,
            default = [],
        ),
        "linkopts": attr.string_list(
            doc = "Additional linker flags passed when linking the visitor `.so`.",
            default = [],
        ),
        "local_defines": attr.string_list(
            doc = "Preprocessor definitions applied only when compiling this visitor's sources.",
            default = [],
        ),
        # `alwayslink` is unconditionally True on the internal libraries so
        # visitor registration symbols/constructors are never stripped.
        "alwayslink": None,
        # Bazel's `inherit_attrs` skips undocumented built-in `BaseRuleClasses`
        # attributes such as `aspect_hints`, so declare it explicitly.
        "aspect_hints": attr.label_list(
            doc = "Hints for aspects.",
        ),
        "non_sdk_deps": attr.label_list(
            doc = "Non-SDK implementation dependencies linked into both the visitor `.so` and `.static` targets.",
            default = [],
        ),
        "enable_sdk_source_set": attr.bool(
            doc = "Unused in Bazel, for GN conversion only.",
            default = False,
            configurable = False,
        ),
        "category": attr.string(
            doc = "Unused in Bazel, for GN conversion only.",
            default = "",
            configurable = False,
        ),
    },
)
