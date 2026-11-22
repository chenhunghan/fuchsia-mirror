# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel equivalent of the GN `fostr_fidl()` template."""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")

visibility([
    "//src/...",
    "//vendor/...",
])

def _get_fidl_ir_json(fidl):
    """Returns the single FIDL IR JSON `File` of a `fidl_library()` target."""
    json_files = [f for f in fidl[DefaultInfo].files.to_list() if f.basename.endswith(".fidl.json")]
    if len(json_files) != 1:
        fail("Expected exactly one `.fidl.json` file in %s, got: %s" % (fidl.label, json_files))
    return json_files[0]

def _fostr_fidl_generate_impl(ctx):
    json_file = _get_fidl_ir_json(ctx.attr.fidl_target)
    template = ctx.file.template_path or ctx.file._default_template
    output_base = ctx.outputs.out_h.path.removesuffix(".h")
    if ctx.outputs.out_cc.path != output_base + ".cc":
        fail("`out_h` and `out_cc` must only differ by their extension.")

    args = ctx.actions.args()
    args.add("--template", template)
    args.add("--json", json_file)
    args.add("--output-base", output_base)
    if ctx.attr.additional_include:
        args.add("--options", "include=" + ctx.attr.additional_include)
    inputs = [json_file, template]
    if ctx.file.amendments_path:
        args.add("--amend", ctx.file.amendments_path)
        inputs.append(ctx.file.amendments_path)

    ctx.actions.run(
        executable = ctx.executable._fidlmerge,
        arguments = [args],
        inputs = inputs,
        outputs = [ctx.outputs.out_h, ctx.outputs.out_cc],
        mnemonic = "Fidlmerge",
        progress_message = "Generating fostr formatting code %{label}",
    )
    return [DefaultInfo(files = depset([ctx.outputs.out_h, ctx.outputs.out_cc]))]

_fostr_fidl_generate = rule(
    doc = "Runs `fidlmerge` to generate the fostr formatting code of a FIDL library.",
    implementation = _fostr_fidl_generate_impl,
    attrs = {
        "fidl_target": attr.label(
            doc = "The `fidl_library()` target to generate formatting code for.",
            mandatory = True,
        ),
        "template_path": attr.label(
            doc = "Optional fidlmerge template. Defaults to `_default_template`.",
            allow_single_file = True,
        ),
        "amendments_path": attr.label(
            doc = "Optional JSON file with amendments to the FIDL IR. See the fidlmerge README.",
            allow_single_file = [".json"],
        ),
        "additional_include": attr.string(
            doc = "Optional file to `#include` in the generated header.",
        ),
        "out_h": attr.output(
            doc = "The generated header.",
            mandatory = True,
        ),
        "out_cc": attr.output(
            doc = "The generated source file.",
            mandatory = True,
        ),
        # A private attribute, so that Bazel checks the visibility of the file
        # against this package.
        "_default_template": attr.label(
            default = "//src/lib/fostr/build:fostr.fidlmerge",
            allow_single_file = True,
        ),
        "_fidlmerge": attr.label(
            default = "//tools/fidl/fidlmerge",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _fostr_fidl_impl(
        name,
        fidl_target,
        hlcpp_dep,
        include_dir,
        template_path,
        amendments_path,
        additional_include,
        deps,
        public_deps,
        testonly,
        visibility):
    # Like in `fidlmerge_cpp()` in //build/cpp/fidlmerge_cpp.gni, the
    # generated header is included as `<include_dir>/formatting.h`, where
    # `include_dir` is the package with `.` replaced by `/`, e.g.
    # "src/lib/fostr/fidl/fuchsia/ui/input".
    gen_dir = name + "_generate"
    out_h = "%s/%s/formatting.h" % (gen_dir, include_dir)
    out_cc = "%s/%s/formatting.cc" % (gen_dir, include_dir)

    _fostr_fidl_generate(
        name = gen_dir,
        fidl_target = fidl_target,
        template_path = template_path,
        amendments_path = amendments_path,
        additional_include = additional_include,
        out_h = out_h,
        out_cc = out_cc,
        testonly = testonly,
        visibility = ["//visibility:private"],
    )

    fx_cc_library(
        name = name,
        srcs = [out_cc],
        hdrs = [out_h],
        strip_include_prefix = gen_dir,
        # The generated source file includes the headers of the fostr
        # libraries of the FIDL libraries that `fidl_target` depends on, so
        # they are public dependencies too.
        deps = [
            hlcpp_dep,
            "//src/lib/fostr",
        ] + public_deps + deps,
        # Like the GN `source_set()`.
        alwayslink = True,
        testonly = testonly,
        visibility = visibility,
    )

_fostr_fidl = macro(
    doc = """Declares a fostr library. Use the `fostr_fidl()` wrapper instead.""",
    implementation = _fostr_fidl_impl,
    attrs = {
        "fidl_target": attr.label(
            doc = "The `fidl_library()` target to generate formatting code for.",
            mandatory = True,
            configurable = False,
        ),
        "hlcpp_dep": attr.label(
            doc = "The HLCPP bindings of `fidl_target`. Set by the wrapper macro.",
            mandatory = True,
            configurable = False,
        ),
        "include_dir": attr.string(
            doc = "The directory of the generated header in `#include` paths. Set by the wrapper macro.",
            mandatory = True,
            configurable = False,
        ),
        "template_path": attr.label(
            doc = "Optional fidlmerge template. Defaults to //src/lib/fostr/build:fostr.fidlmerge.",
            configurable = False,
        ),
        "amendments_path": attr.label(
            doc = "Optional JSON file with amendments to the FIDL IR.",
            configurable = False,
        ),
        "additional_include": attr.string(
            doc = "Optional file to `#include` in the generated header.",
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "The fostr libraries of the FIDL libraries that `fidl_target` depends on, and other dependencies.",
            default = [],
        ),
        "public_deps": attr.label_list(
            doc = "Dependencies of the generated header, e.g. of `additional_include`.",
            default = [],
        ),
        "testonly": attr.bool(
            doc = "Standard meaning.",
            default = False,
            configurable = False,
        ),
    },
)

def fostr_fidl(*, name, fidl_target, **kwargs):
    """Declares a fostr library with C++ ostream formatting for a FIDL library.

    The library is an `fx_cc_library()` named `name`. Include its header with
    `#include "<package with `.` replaced by `/`>/formatting.h"`, e.g.
    `#include "src/lib/fostr/fidl/fuchsia/ui/input/formatting.h"` for
    //src/lib/fostr/fidl/fuchsia.ui.input. So, like in GN, there can only be
    one `fostr_fidl()` per package.

    This is the Bazel equivalent of the GN `fostr_fidl()` template in
    //src/lib/fostr/build/fostr_fidl.gni, and is converted to it by bazel2gn.

    This wrapper computes the label of the HLCPP bindings of `fidl_target`, so
    that its visibility is checked against the calling package, and the
    `#include` path of the generated header.

    Args:
      name: Standard meaning.
      fidl_target: The `fidl_library()` target to generate formatting code
        for. The library depends on its HLCPP bindings.
      **kwargs: Passed to `_fostr_fidl()`: `template_path`, `amendments_path`,
        `additional_include`, `deps`, `public_deps`, `testonly` and
        `visibility`.
    """
    fidl_label = native.package_relative_label(fidl_target)
    _fostr_fidl(
        name = name,
        fidl_target = fidl_target,
        hlcpp_dep = fidl_label.same_package_label(fidl_label.name + "_hlcpp"),
        include_dir = native.package_name().replace(".", "/"),
        **kwargs
    )
