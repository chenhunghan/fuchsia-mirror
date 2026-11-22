# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rule for running Rust FIDL code generators with rustfmt for golden tests."""

visibility(["//tools/fidl/..."])

def _fidlgen_rustfmt_golden_impl(ctx):
    ir = ctx.file.fidl_ir_json
    rustfmt_config = ctx.file._rustfmt_config

    rust_toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    rustfmt = rust_toolchain.rustfmt

    arguments = [
        "--json",
        ir.path,
        "--output-filename",
        ctx.outputs.out.path,
        "--rustfmt",
        rustfmt.path,
        "--rustfmt-config",
        rustfmt_config.path,
    ]

    inputs = [ir, rustfmt, rustfmt_config]
    if ctx.file.config:
        inputs.append(ctx.file.config)
        arguments.extend([
            "--config",
            ctx.file.config.path,
        ])

    arguments.extend(ctx.attr.extra_args)

    ctx.actions.run(
        executable = ctx.executable.tool,
        arguments = arguments,
        inputs = inputs,
        tools = rust_toolchain.all_files,
        outputs = [ctx.outputs.out],
        mnemonic = "FidlGenGoldenRust",
        progress_message = "Generating Rust FIDL golden candidate %{output}",
    )

    return [DefaultInfo(files = depset([ctx.outputs.out]))]

fidlgen_rustfmt_golden = rule(
    doc = "Runs a Rust FIDL code generator with rustfmt to produce a golden candidate.",
    implementation = _fidlgen_rustfmt_golden_impl,
    toolchains = ["@rules_rust//rust:toolchain_type"],
    attrs = {
        "tool": attr.label(
            doc = "The fidlgen_rust* executable to invoke.",
            executable = True,
            cfg = "exec",
            mandatory = True,
        ),
        "fidl_ir_json": attr.label(
            doc = "The FIDL IR JSON file for which to generate code.",
            allow_single_file = True,
            mandatory = True,
        ),
        "config": attr.label(
            doc = "Optional JSON configuration file passed via --config.",
            allow_single_file = True,
        ),
        "extra_args": attr.string_list(
            doc = "Additional command-line flags passed to the tool.",
            default = [],
        ),
        "out": attr.output(
            doc = "Generated candidate Rust source file.",
            mandatory = True,
        ),
        "_rustfmt_config": attr.label(
            allow_single_file = True,
            default = Label("//:rustfmt.toml"),
        ),
    },
)
