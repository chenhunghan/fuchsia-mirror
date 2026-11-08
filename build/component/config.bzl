# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for generating persistent FIDL binary component_manager config resources."""

load("@fuchsia_rules_common//:utils.bzl", "make_resource_struct")
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)

visibility(["//src/..."])

def _component_config_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + "_config.fidlbin")
    args = ctx.actions.args()
    args.add("--output", out)
    for src in ctx.files.srcs:
        args.add("--input", src)
    for product in ctx.files.product_configs:
        args.add("--product", product)

    ctx.actions.run(
        outputs = [out],
        inputs = ctx.files.srcs + ctx.files.product_configs,
        executable = ctx.executable._tool,
        arguments = [args],
        mnemonic = "ComponentConfig",
        progress_message = "Generating component_manager config %{label}",
    )

    dest = ctx.attr.dest if ctx.attr.dest else ("data/" + ctx.label.name)
    return [
        DefaultInfo(files = depset([out])),
        FuchsiaPackageResourcesInfo(
            resources = [
                make_resource_struct(
                    src = out,
                    dest = dest,
                ),
            ],
        ),
    ]

_component_config = rule(
    doc = "Generates a binary FIDL component_manager configuration and packages it as a resource.",
    implementation = _component_config_impl,
    attrs = {
        "srcs": attr.label_list(
            allow_files = [".json", ".json5"],
            doc = "JSON/JSON5 files with config values.",
        ),
        "product_configs": attr.label_list(
            allow_files = [".json", ".json5"],
            doc = "JSON/JSON5 files containing product policy.",
        ),
        "dest": attr.string(
            doc = "Packaged path for the generated binary config file. Defaults to `data/<name>`.",
        ),
        "_tool": attr.label(
            default = "//tools/component_manager_config:generate_config_bin",
            executable = True,
            cfg = "exec",
        ),
    },
)

def component_config(name, srcs = None, product_configs = None, build_type = None, dest = "", **kwargs):
    """Generates a binary FIDL component_manager configuration resource.

    Mirrors `component_config()` in `//build/component/config.gni`.

    Args:
        name: Target name.
        srcs: List of JSON/JSON5 config value files.
        product_configs: Optional list of JSON/JSON5 product policy files.
        build_type: Optional string enum (`eng`, `userdebug`, `user`).
        dest: Packaged destination path; defaults to `data/<name>`.
        **kwargs: Standard Bazel attributes (`testonly`, `visibility`, etc.).
    """
    resolved_srcs = list(srcs or [])
    if build_type != None:
        if build_type == "eng":
            resolved_srcs.extend([
                "//src/security/policy:component_manager_policy.json5",
                "//src/security/policy:component_manager_policy_eng.json5",
            ])
        elif build_type == "userdebug":
            resolved_srcs.append("//src/security/policy:component_manager_policy_userdebug.json5")
        elif build_type == "user":
            resolved_srcs.append("//src/security/policy:component_manager_policy_user.json5")
        else:
            fail("build_type must be one of 'eng', 'userdebug', or 'user', got: %s" % build_type)
        resolved_srcs.extend([
            "//src/security/policy:component_manager_policy_base.json5",
            "//src/security/policy:component_manager_policy_build_type_base.json5",
            "//src/sys/component_manager/configs:bootfs_config.json5",
        ])

    _component_config(
        name = name,
        srcs = resolved_srcs,
        product_configs = product_configs or [],
        dest = dest,
        **kwargs
    )
