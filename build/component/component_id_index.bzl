# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for merging and encoding component ID index files into a packaged FIDL resource."""

load("@fuchsia_rules_common//:utils.bzl", "make_resource_struct")
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)

visibility(["//src/..."])

def _component_id_index_config_impl(ctx):
    input_manifest = ctx.actions.declare_file(ctx.label.name + "_merge_input_manifest")
    ctx.actions.write(
        output = input_manifest,
        content = "\n".join([f.path for f in ctx.files.sources]) + "\n",
    )

    out_json = ctx.actions.declare_file(ctx.label.name + "_merged_index.json")
    out_fidl = ctx.actions.declare_file(ctx.label.name + "_merged_index.fidlbin")
    depfile = ctx.actions.declare_file(ctx.label.name + ".d")

    args = ctx.actions.args()
    args.add("--input-manifest", input_manifest)
    args.add("--output-index-json", out_json)
    args.add("--output-index-fidl", out_fidl)
    args.add("--depfile", depfile)

    ctx.actions.run(
        outputs = [out_json, out_fidl, depfile],
        inputs = [input_manifest] + ctx.files.sources,
        executable = ctx.executable._tool,
        arguments = [args],
        mnemonic = "ComponentIdIndex",
        progress_message = "Merging component ID index %{label}",
    )

    return [
        DefaultInfo(files = depset([out_fidl])),
        FuchsiaPackageResourcesInfo(
            resources = [
                make_resource_struct(
                    src = out_fidl,
                    dest = ctx.attr.dest,
                ),
            ],
        ),
    ]

component_id_index_config = rule(
    doc = """Merges component ID index JSON5 files into a FIDL-encoded resource.

    Mirrors `component_id_index_config()` in `//build/component/component_id_index.gni`.
    """,
    implementation = _component_id_index_config_impl,
    attrs = {
        "sources": attr.label_list(
            allow_files = [".json", ".json5"],
            mandatory = True,
            doc = "Component ID index JSON/JSON5 files to validate and merge.",
        ),
        "dest": attr.string(
            default = "config/component_id_index",
            doc = "Packaged path for the merged FIDL binary index.",
        ),
        "_tool": attr.label(
            default = "//tools/component_id_index",
            executable = True,
            cfg = "exec",
        ),
    },
)
