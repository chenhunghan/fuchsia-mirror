# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Definitions and helpers for the FIDL dangerous identifiers test suite."""

load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")

visibility([
    "//src/tests/fidl/dangerous_identifiers/...",
])

# List of contexts where dangerous identifiers can be used.
USES = [
    "constants",
    "enums",
    "event.names",
    "method.event.arguments",
    "method.names",
    "method.request.arguments",
    "method.response.arguments",
    "protocol.names",
    "service.names",
    "service.member.types",
    "service.member.names",
    "struct.names",
    "struct.types",
    "table.fields",
    "table.names",
    "union.names",
    "union.types",
    "using",
]

# List of identifier styles to test.
STYLES = [
    "lower",
    "upper",
    "camel",
]

# How to shard identifiers.
SHARDS = [
    "1",
    "2",
    "3",
    "4",
]

# Prefix for generated FIDL libraries.
FIDL_LIBRARY_PREFIX = "fidl.test.dangerous"

DANGEROUS_TESTS = [
    {
        "use": use,
        "style": style,
        "shard": shard,
        "fidl_library": "%s.%s.%s%s" % (FIDL_LIBRARY_PREFIX, use, style, shard),
        "fidl_target": "%s.%s.%s%s" % (FIDL_LIBRARY_PREFIX, use, style, shard),
        "cpp_target": "cpp_%s_%s_%s" % (use, style, shard),
        "hlcpp_target": "hlcpp_%s_%s_%s" % (use, style, shard),
        # Deny list of libraries we can compile in Rust.
        # [BindingsDenyList] on struct members doesn't work in Rust.
        "rust_target": use != "struct.names" and use != "struct.types",
    }
    for use in USES
    for style in STYLES
    for shard in SHARDS
]

def _generate_dangerous_fidl_impl(ctx):
    out = ctx.actions.declare_file("%s.test.fidl" % ctx.attr.fidl_library)
    ctx.actions.run(
        executable = ctx.executable._tool,
        arguments = [
            "--use=%s" % ctx.attr.use,
            "--style=%s" % ctx.attr.style,
            "--shards=%s" % ",".join(SHARDS),
            "--shard=%s" % ctx.attr.shard,
            "--fidl-library=%s" % ctx.attr.fidl_library,
            "--out=%s" % out.path,
        ],
        outputs = [out],
        mnemonic = "GenerateDangerousFidl",
        progress_message = "Generating dangerous FIDL %s" % ctx.attr.fidl_library,
    )
    return [DefaultInfo(files = depset([out]))]

_generate_dangerous_fidl = rule(
    doc = "Generates a `.test.fidl` file containing dangerous identifiers.",
    implementation = _generate_dangerous_fidl_impl,
    attrs = {
        "use": attr.string(mandatory = True),
        "style": attr.string(mandatory = True),
        "shard": attr.string(mandatory = True),
        "fidl_library": attr.string(mandatory = True),
        "_tool": attr.label(
            default = "//src/tests/fidl/dangerous_identifiers:generate_fidl",
            executable = True,
            cfg = "exec",
        ),
    },
)

def dangerous_fidl(*, name, use, style, shard, library_name, enable_rust = True):
    """Generates a dangerous-identifiers `.test.fidl` file and declares its `fidl_library`.

    Args:
      name: Name of the generated `fidl_library` target.
      use: The identifier use context.
      style: The identifier naming style.
      shard: The shard identifier.
      library_name: The FIDL library name.
      enable_rust: Whether to generate Rust bindings for this library.
    """
    generate_target_name = "%s_generate" % name

    _generate_dangerous_fidl(
        name = generate_target_name,
        testonly = True,
        use = use,
        style = style,
        shard = shard,
        fidl_library = library_name,
        visibility = ["//visibility:private"],
    )

    fidl_library(
        name = name,
        testonly = True,
        srcs = [":" + generate_target_name],
        enable_cpp = True,
        enable_hlcpp = True,
        enable_rust = enable_rust,
        enable_rust_next = False,
        library_name = library_name,
        visibility = ["//visibility:private"],
    )
