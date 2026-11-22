# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines `bind_library()`, a macro for declaring a bind library."""

load("@fuchsia_rules_common//bind:bindc.bzl", "bindc_generate")
load("@fuchsia_rules_common//bind:providers.bzl", "FuchsiaBindLibraryInfo")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

visibility([
    "//build/bazel/rules/bind/...",
    "//examples/...",
    "//sdk/...",
    "//src/...",
    "//vendor/...",
    "//zircon/...",
])

def _bind_library_rule_impl(ctx):
    return [
        DefaultInfo(files = depset([ctx.file.src])),
        FuchsiaBindLibraryInfo(
            name = ctx.label.name,
            transitive_sources = depset(
                [ctx.file.src],
                transitive = [dep[FuchsiaBindLibraryInfo].transitive_sources for dep in ctx.attr.deps],
            ),
        ),
    ]

_bind_library_rule = rule(
    doc = "Provides the sources of a bind library and of the libraries it includes.",
    implementation = _bind_library_rule_impl,
    attrs = {
        "src": attr.label(
            doc = "The `.bind` source file of the library.",
            mandatory = True,
            allow_single_file = [".bind"],
        ),
        "deps": attr.label_list(
            doc = "Other bind libraries that the library includes.",
            providers = [FuchsiaBindLibraryInfo],
        ),
    },
)

def _bindc_generate_impl(ctx):
    out = bindc_generate(
        ctx,
        bindc = ctx.executable._bindc,
        language = ctx.attr.language,
        library_name = ctx.attr.library_name,
        src = ctx.file.src,
        mnemonic = "BindcGenerate",
    )
    return [DefaultInfo(files = depset([out]))]

_bindc_generate = rule(
    doc = "Generates the C++ header or Rust source for a bind library with `bindc`.",
    implementation = _bindc_generate_impl,
    attrs = {
        "src": attr.label(
            doc = "The `.bind` source file of the library.",
            mandatory = True,
            allow_single_file = [".bind"],
        ),
        "library_name": attr.string(
            doc = "Name of the bind library, e.g. `fuchsia.example`.",
            mandatory = True,
        ),
        "language": attr.string(
            doc = "The language of the generated bindings.",
            mandatory = True,
            values = ["cpp", "rust"],
        ),
        "_bindc": attr.label(
            default = "//tools/bindc:bin",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _bind_library_impl(
        name,
        src,
        deps,
        # TODO(https://fxbug.dev/507577175): Define an `idk_atom()` when `category` is set.
        category,
        api_area,
        source,
        public_deps,
        sdk_category,
        sdk_area,
        testonly,
        visibility):
    if src != None and source != None:
        fail("Specify `src`, not both `src` and `source`.")
    resolved_src = src if src != None else source
    if resolved_src == None:
        fail("Missing required attribute `src`.")

    if deps and public_deps:
        fail("Specify `deps`, not both `deps` and `public_deps`.")
    resolved_deps = deps or public_deps

    if category and sdk_category:
        fail("Specify `category`, not both `category` and `sdk_category`.")
    _resolved_category = category or sdk_category  # buildifier: disable=unused-variable

    if api_area and sdk_area:
        fail("Specify `api_area`, not both `api_area` and `sdk_area`.")
    _resolved_api_area = api_area or sdk_area  # buildifier: disable=unused-variable

    # Keep the target names, include path and crate name in sync with
    # `_bind_library_cpp()` and `_bind_library_rust()` in
    # //build/bind/bind.gni, so that labels resolve to the same targets in both
    # build systems.
    _bind_library_rule(
        name = name,
        src = resolved_src,
        deps = resolved_deps,
        testonly = testonly,
        visibility = visibility,
    )

    cpp_gen_name = name + "_cpp_header_gen"
    _bindc_generate(
        name = cpp_gen_name,
        src = resolved_src,
        library_name = name,
        language = "cpp",
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    # The header is included as `<bind/{library/name/with/slashes}/cpp/bind.h>`.
    fx_cc_library(
        name = name + "_cpp",
        hdrs = [":" + cpp_gen_name],
        strip_include_prefix = cpp_gen_name,
        deps = [dep.same_package_label(dep.name + "_cpp") for dep in resolved_deps],
        testonly = testonly,
        visibility = visibility,
    )

    rust_gen_name = name + "_rust_file_gen"
    _bindc_generate(
        name = rust_gen_name,
        src = resolved_src,
        library_name = name,
        language = "rust",
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    rustc_library(
        name = name + "_rust",
        crate_name = "bind_" + name.replace(".", "_"),
        srcs = [":" + rust_gen_name],
        deps = [dep.same_package_label(dep.name + "_rust") for dep in resolved_deps],
        edition = "2024",
        build_flags = ["//build/config/rust/lints:deny_unused_results"],
        # The code is generated.
        disable_clippy = True,
        testonly = testonly,
        visibility = visibility,
    )

bind_library = macro(
    doc = """Declares a bind library and its C++ and Rust bindings.

    For a library named `fuchsia.example`, it defines:

    * `:fuchsia.example`: The bind library itself, providing
      `FuchsiaBindLibraryInfo`. Use it in the `deps` of other bind libraries
      and bind rules.
    * `:fuchsia.example_cpp`: The C++ bindings. Include them with
      `#include <bind/fuchsia/example/cpp/bind.h>`.
    * `:fuchsia.example_rust`: The Rust bindings, in the `bind_fuchsia_example`
      crate.

    For more details about bind libraries, see //tools/bindc/README.md.
    """,
    implementation = _bind_library_impl,
    attrs = {
        "src": attr.label(
            doc = """The `.bind` source file of the library.
GN equivalent: `source`""",
            mandatory = False,
            allow_single_file = [".bind"],
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = """Other `bind_library()` targets that the library includes.
GN equivalent: `public_deps`""",
            default = [],
            configurable = False,
        ),
        "category": attr.string(
            doc = """Publication level of the library in the IDK.
TODO(https://fxbug.dev/507577175): Define an `idk_atom()` in Bazel when set.
GN equivalent: `sdk_category`""",
            values = ["partner", ""],
            mandatory = False,
            configurable = False,
        ),
        "api_area": attr.string(
            doc = """The API area responsible for maintaining this library.
GN equivalent: `sdk_area`""",
            mandatory = False,
            configurable = False,
        ),
        "source": attr.label(
            doc = "Deprecated alias for `src`. Do not use.",
            mandatory = False,
            allow_single_file = [".bind"],
            configurable = False,
        ),
        "public_deps": attr.label_list(
            doc = "Deprecated alias for `deps`. Do not use.",
            default = [],
            configurable = False,
        ),
        "sdk_category": attr.string(
            doc = "Deprecated alias for `category`. Do not use.",
            values = ["partner", ""],
            mandatory = False,
            configurable = False,
        ),
        "sdk_area": attr.string(
            doc = "Deprecated alias for `api_area`. Do not use.",
            mandatory = False,
            configurable = False,
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)
