# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for declaring a Fuchsia test expectations JSON5 file and its transitive includes."""

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

def _fx_expectations_impl(ctx):
    if ctx.label.name.endswith(".json5"):
        fail("`fx_expectations()` target name '{}' must not end with '.json5'.".format(ctx.label.name))
    if FuchsiaExpectationsInfo in ctx.attr.expectations:
        fail(
            "`expectations` in `fx_expectations()` must be a `.json5` source file, got target '{}'.".format(
                ctx.attr.expectations.label,
            ),
        )

    direct_includes = []
    transitive_includes = []

    for inc in ctx.attr.includes:
        if FuchsiaExpectationsInfo in inc:
            exp_info = inc[FuchsiaExpectationsInfo]
            if not exp_info.root_file.is_source:
                fail(
                    "`includes` in `fx_expectations()` does not accept `fx_generated_expectations()` target '{}'.".format(
                        inc.label,
                    ),
                )
            direct_includes.append(exp_info.root_file)
            transitive_includes.append(exp_info.includes)
        else:
            transitive_includes.append(inc[DefaultInfo].files)

    return [
        DefaultInfo(files = depset([ctx.file.expectations])),
        FuchsiaExpectationsInfo(
            root_file = ctx.file.expectations,
            includes = depset(
                direct_includes,
                transitive = transitive_includes,
            ),
        ),
    ]

fx_expectations = rule(
    doc = """Declares a test expectations JSON5 file and any `.json5` files it transitively includes.

    Provides `FuchsiaExpectationsInfo` so that downstream `preprocess_expectations()`
    and `fx_test_component_with_expectations()` targets receive both the root
    expectations file and all transitively included `.json5` files in the Bazel
    action sandbox.

    Note: `fx_expectations()` and `fx_generated_expectations()` intentionally do
    not compose with each other in either direction; this matches current
    in-tree usage and keeps `bazel2gn` translation 1:1 with GN.
    """,
    implementation = _fx_expectations_impl,
    attrs = {
        "expectations": attr.label(
            doc = "The `.json5` test expectations source file.",
            allow_single_file = [".json5"],
            mandatory = True,
        ),
        "includes": attr.label_list(
            doc = "Additional `.json5` files or `fx_expectations()` targets included by `expectations`.",
            allow_files = [".json5"],
        ),
    },
    provides = [FuchsiaExpectationsInfo],
)

# LINT.ThenChange(//src/lib/testing/expectation/fuchsia_test_component_with_expectations.gni)
