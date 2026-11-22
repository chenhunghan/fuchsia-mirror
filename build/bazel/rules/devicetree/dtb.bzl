# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for compiling a devicetree source (`.dts` or `.dts.S`) into a `.dtb`."""

load("@fuchsia_rules_common//devicetree:preprocess.bzl", "preprocess_dts")
load("@fuchsia_rules_common//devicetree:providers.bzl", "FuchsiaDeviceTreeSegmentInfo")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")

visibility([
    "//boards/...",
    "//build/bazel/rules/devicetree/...",
    "//examples/...",
    "//sdk/...",
    "//src/...",
    "//vendor/...",
    "//zircon/...",
])

def _preprocess_dts(
        ctx,
        source,
        cc_includes,
        cc_quote_includes,
        cc_system_includes,
        cc_defines,
        cc_headers,
        fragment_includes,
        fragment_files):
    """Preprocesses a `.dts.S` source with Clang into a `.dts` file."""
    preprocessed_dts = ctx.actions.declare_file(
        ctx.label.name + "/" + source.basename.removesuffix(".S"),
    )
    return preprocess_dts(
        ctx,
        preprocessor = ctx.executable._preprocessor,
        source = source,
        output_file = preprocessed_dts,
        extra_include_dirs = [".", ctx.bin_dir.path],
        includes = depset(transitive = [cc_includes, fragment_includes]),
        quote_includes = depset(
            direct = [h.dirname or "." for h in ctx.files.hdrs],
            transitive = [cc_quote_includes],
        ),
        system_includes = cc_system_includes,
        defines = cc_defines,
        inputs = depset(
            direct = ctx.files.hdrs,
            transitive = cc_headers + fragment_files,
        ),
        mnemonic = "DtsPreprocess",
        progress_message = "Preprocessing devicetree %{label}",
    )

def _dtb_impl(ctx):
    if len(ctx.files.srcs) != 1:
        fail("dtb() requires exactly one `.dts` or `.dts.S` file in `srcs`, got %d." % len(ctx.files.srcs))

    source = ctx.files.srcs[0]
    if not (source.basename.endswith(".dts") or source.basename.endswith(".dts.S")):
        fail("dtb() `srcs` file must end with `.dts` or `.dts.S`, got `%s`." % source.basename)

    cc_includes = []
    cc_quote_includes = []
    cc_system_includes = []
    cc_defines = []
    cc_headers = []
    fragment_includes = []
    fragment_files = []

    for dep in ctx.attr.deps:
        if FuchsiaDeviceTreeSegmentInfo in dep:
            fragment_includes.append(dep[FuchsiaDeviceTreeSegmentInfo].includes)
            fragment_files.append(dep[FuchsiaDeviceTreeSegmentInfo].files)
        if CcInfo in dep:
            cc_ctx = dep[CcInfo].compilation_context
            cc_includes.append(cc_ctx.includes)
            cc_quote_includes.append(cc_ctx.quote_includes)
            cc_system_includes.append(cc_ctx.system_includes)
            cc_defines.append(cc_ctx.defines)
            cc_headers.append(cc_ctx.headers)

    transitive_fragment_includes = depset(transitive = fragment_includes)

    if source.basename.endswith(".dts.S"):
        dts_file = _preprocess_dts(
            ctx,
            source,
            depset(transitive = cc_includes),
            depset(transitive = cc_quote_includes),
            depset(transitive = cc_system_includes),
            depset(transitive = cc_defines),
            cc_headers,
            transitive_fragment_includes,
            fragment_files,
        )
    else:
        dts_file = source

    dtb_filename = dts_file.basename.removesuffix(".dts") + ".dtb"
    output_dtb = ctx.actions.declare_file(ctx.label.name + "/" + dtb_filename)

    includes_file = ctx.actions.declare_file(ctx.label.name + "/" + ctx.label.name + ".includes.txt")
    includes_args = ctx.actions.args()
    includes_args.set_param_file_format("multiline")
    includes_args.add_all(depset(
        direct = [source.dirname] if dts_file != source else [],
        transitive = fragment_includes,
    ))
    ctx.actions.write(
        output = includes_file,
        content = includes_args,
    )

    dtc_args = ctx.actions.args()
    dtc_args.add("compile")
    dtc_args.add(ctx.file._dtc)
    dtc_args.add(dts_file)
    dtc_args.add(output_dtb)
    dtc_args.add("-")
    dtc_args.add(includes_file)
    dtc_args.add_all(ctx.attr.dtcflags)

    ctx.actions.run(
        executable = ctx.executable._dtc_py,
        arguments = [dtc_args],
        inputs = depset(
            direct = [dts_file, includes_file] + ctx.files.hdrs,
            transitive = fragment_files,
        ),
        tools = [ctx.file._dtc],
        outputs = [output_dtb],
        mnemonic = "DtbCompile",
        progress_message = "Compiling devicetree %{label}",
    )

    return [
        DefaultInfo(
            files = depset([output_dtb]),
            runfiles = ctx.runfiles(files = [output_dtb]),
        ),
    ]

dtb = rule(
    doc = """Compiles a devicetree source file (`.dts` or `.dts.S`) into a `.dtb` blob.

    Equivalent to GN `dtb()` in //build/devicetree/devicetree.gni. Files ending in
    `.dts.S` are preprocessed with Clang before invoking `dtc`.
    """,
    implementation = _dtb_impl,
    attrs = {
        "srcs": attr.label_list(
            doc = "Singleton list containing the `.dts` or `.dts.S` source file to compile.",
            allow_files = [".dts", ".dts.S"],
            mandatory = True,
        ),
        "hdrs": attr.label_list(
            doc = "Optional header or `.dtsi` files included directly by `srcs`.",
            allow_files = True,
            default = [],
        ),
        "deps": attr.label_list(
            doc = "Dependencies providing C headers (`CcInfo`) or devicetree fragments (`FuchsiaDeviceTreeSegmentInfo`).",
            providers = [[CcInfo], [FuchsiaDeviceTreeSegmentInfo]],
            default = [],
        ),
        "dtcflags": attr.string_list(
            doc = "Additional flags passed to `dtc`.",
            default = [],
        ),
        "_preprocessor": attr.label(
            default = "@prebuilt_clang//:bin/clang-cpp",
            executable = True,
            allow_single_file = True,
            cfg = "exec",
        ),
        "_dtc": attr.label(
            default = "//build/prebuilt/cipd:dtc",
            allow_single_file = True,
            cfg = "exec",
        ),
        "_dtc_py": attr.label(
            default = "//build/devicetree:dtc_py",
            executable = True,
            cfg = "exec",
        ),
    },
)
