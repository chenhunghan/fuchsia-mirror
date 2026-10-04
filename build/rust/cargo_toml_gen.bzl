# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule to generate Cargo.toml files for Rust crates in Fuchsia."""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")

def _cargo_toml_gen_impl(ctx):
    output_dir = ctx.actions.declare_directory(ctx.label.name)

    args = ctx.actions.args()
    args.add("--project_json", ctx.file.project_json)
    args.add("--output_dir", output_dir.path)
    args.add("--cargo_toml", ctx.file.cargo_toml)
    args.add("--api_level_cfg_flags", ctx.file.api_level_cfg_flags)

    ctx.actions.run(
        outputs = [output_dir],
        inputs = [
            ctx.file.project_json,
            ctx.file.cargo_toml,
            ctx.file.api_level_cfg_flags,
        ],
        executable = ctx.executable._generator,
        arguments = [args],
        mnemonic = "CargoTomlGen",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return [
        DefaultInfo(files = depset([output_dir])),
    ]

cargo_toml_gen = rule(
    implementation = _cargo_toml_gen_impl,
    doc = "Generates Cargo.toml files for Rust crates in Fuchsia.",
    attrs = {
        "project_json": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The GN project.json file",
        ),
        "cargo_toml": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The third_party/rust_crates/Cargo.toml file",
        ),
        "api_level_cfg_flags": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The rust_api_level_cfg_flags.txt file",
        ),
        "_generator": attr.label(
            default = ":generate_cargo",
            executable = True,
            cfg = "exec",
        ),
    },
)
