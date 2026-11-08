# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for generating a merged test expectations JSON5 file."""

load(
    ":preprocess.bzl",
    "FuchsiaExpectationsInfo",
)

visibility([
    "//src/...",
    "//third_party/gvisor_syscall_tests/...",
    "//vendor/...",
])

# LINT.IfChange

def _fx_generated_expectations_impl(ctx):
    if ctx.label.name.endswith(".json5"):
        fail("`fx_generated_expectations()` target name '{}' must not end with '.json5'.".format(ctx.label.name))

    output_file = ctx.actions.declare_file("{}.json5".format(ctx.label.name))

    actions = [
        {
            "include": "//" + ctx.file._default_expectations.path,
        },
    ]
    direct_includes = [ctx.file._default_expectations]

    for inc in ctx.attr.includes:
        if FuchsiaExpectationsInfo in inc:
            fail(
                "`includes` in `fx_generated_expectations()` only accepts `.json5` files, got target '{}'.".format(
                    inc.label,
                ),
            )
        for f in inc[DefaultInfo].files.to_list():
            actions.append({"include": "//" + f.path})
            direct_includes.append(f)

    content = json.encode_indent(
        {"actions": actions},
        indent = "  ",
    ) + "\n"

    ctx.actions.write(
        output = output_file,
        content = content,
    )

    return [
        DefaultInfo(files = depset([output_file])),
        FuchsiaExpectationsInfo(
            root_file = output_file,
            includes = depset(direct_includes),
        ),
    ]

fx_generated_expectations = rule(
    doc = """Generates an expectations JSON5 file from a list of expectation files to include.

    All test cases without matching expectations will be expected to pass by default.
    The resulting target provides `FuchsiaExpectationsInfo` so that downstream
    `preprocess_expectations()` and `fx_test_component_with_expectations()`
    targets automatically receive all included `.json5` files in the Bazel
    action sandbox.

    Note: `fx_generated_expectations()` and `fx_expectations()` intentionally do
    not compose with each other in either direction (entries in `includes` must
    be standalone `.json5` files without nested `include` directives); this
    matches current in-tree usage and keeps `bazel2gn` translation 1:1 with GN
    `generated_expectations_file()`.
    """,
    implementation = _fx_generated_expectations_impl,
    attrs = {
        "includes": attr.label_list(
            doc = "A list of `.json5` expectation files to include.",
            allow_files = [".json5"],
            mandatory = True,
            allow_empty = False,
        ),
        "_default_expectations": attr.label(
            default = "//src/lib/testing/expectation:default_expectations.json5",
            allow_single_file = [".json5"],
        ),
    },
    provides = [FuchsiaExpectationsInfo],
)

# LINT.ThenChange(//src/lib/testing/expectation/generated_expectations_file.gni)
