# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared helpers for merging and compiling Fuchsia component manifests."""

load(
    ":providers.bzl",
    "FuchsiaComponentManifestInfo",
)

def merge_component_manifests(
        ctx,
        cmc_tool,
        srcs,
        output_name = None,
        toolchain = None):
    """Merges component manifest files (`.cml`, `.shard.cml`, or `.json`) with `cmc merge`.

    Args:
        ctx: The rule context.
        cmc_tool: The `cmc` executable.
        srcs: A non-empty list of `.cml`, `.shard.cml`, or `.json` `File`s to merge, in order.
        output_name: Output file path relative to the package's output directory.
            Defaults to `<name>.cml`.
        toolchain: The toolchain type to run `cmc` with, if any.

    Returns:
        The merged `File`.
    """
    if not srcs:
        fail("`srcs` must not be empty.")

    # LINT.IfChange(cmc_merge_output_name)
    merged_output = ctx.actions.declare_file(output_name or "{}.cml".format(ctx.label.name))
    # LINT.ThenChange(//build/tools/bazel2gn/cmc_merge.go:cmc_merge_output_name)

    args = [
        "merge",
        "--output",
        merged_output.path,
    ] + [f.path for f in srcs]

    ctx.actions.run(
        executable = cmc_tool,
        arguments = args,
        inputs = list(srcs),
        outputs = [merged_output],
        mnemonic = "CmcMerge",
        progress_message = "Merging component manifests for %{label}",
        **({"toolchain": toolchain} if toolchain else {})
    )

    return merged_output

def compile_component_manifest(
        ctx,
        cmc_tool,
        manifest_in,
        component_name,
        includes,
        include_paths,
        toolchain = None,
        extra_args = [],
        output_name = None):
    """Compiles a `.cml` component manifest into a `.cm` file with `cmc compile`.

    Args:
        ctx: The rule context.
        cmc_tool: The `cmc` executable.
        manifest_in: The `.cml` File to compile.
        component_name: The name of the component.
        includes: A list of `.cml` shard Files that `manifest_in` may include.
        include_paths: A list of directories to search for included shards.
        toolchain: The toolchain type to run `cmc` with, if any.
        extra_args: Additional arguments to pass to `cmc compile`, such as
            `--features` or `--must-offer-dictionary`.
        output_name: The path of the compiled manifest relative to the
            package's output directory. Defaults to `meta/<component_name>.cm`.

    Returns:
        A list of `DefaultInfo` and `FuchsiaComponentManifestInfo` providers.
    """

    # output should have the .cm extension
    manifest_out = ctx.actions.declare_file(output_name or "meta/{}.cm".format(component_name))
    config_package_path = "meta/%s.cvf" % component_name

    include_path_args = []
    for w in include_paths:
        include_path_args.extend(["--includepath", w])

    config_values_package_path_args = [
        "--config-package-path",
        config_package_path,
    ]

    args = [
        "compile",
        "--output",
        manifest_out.path,
        manifest_in.path,
        "--includepath",
        manifest_in.dirname,
    ] + include_path_args + config_values_package_path_args + extra_args

    ctx.actions.run(
        executable = cmc_tool,
        arguments = args,
        inputs = depset([manifest_in] + includes),
        outputs = [manifest_out],
        mnemonic = "CmcCompile",
        **({"toolchain": toolchain} if toolchain else {})
    )

    return [
        DefaultInfo(files = depset([manifest_out])),
        FuchsiaComponentManifestInfo(
            compiled_manifest = manifest_out,
            component_name = component_name,
            config_package_path = config_package_path,
        ),
    ]
