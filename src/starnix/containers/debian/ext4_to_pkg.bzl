# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules for extracting an ext4 image and packaging it into a Fuchsia package."""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")
load(
    "@fuchsia_rules_common//:utils.bzl",
    "fuchsia_cpu_from_ctx",
    "make_resource_struct",
    "stub_executable",
)
load(
    "@fuchsia_rules_common//components:providers.bzl",
    "FuchsiaComponentInfo",
    "FuchsiaPackagedComponentInfo",
)
load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "find_and_process_unstripped_binaries",
    "merge_debug_symbol_infos",
)
load(
    "@fuchsia_rules_common//packages:package.bzl",
    "BUILD_FUCHSIA_PACKAGE_COMMON_ATTRS",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaCollectedPackageResourcesInfo",
    "FuchsiaPackageInfo",
    "FuchsiaPackageResourcesInfo",
    "FuchsiaStructuredConfigInfo",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "fuchsia_find_all_package_resources",
    "package_resources_providers",
)

visibility(["//src/starnix/containers/debian/..."])

def _ext4_to_pkg_impl(ctx):
    out_dir = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run(
        executable = ctx.executable._ext4_to_pkg,
        inputs = [ctx.file.input],
        outputs = [out_dir],
        arguments = [
            ctx.file.input.path,
            out_dir.path,
            ctx.attr.prefix,
        ],
        env = {"RUST_BACKTRACE": "1"},
        mnemonic = "Ext4ToPkg",
        progress_message = "Running ext4_to_pkg for %{label}",
    )

    return package_resources_providers(
        ctx,
        [
            make_resource_struct(
                src = out_dir,
                dest = ctx.attr.prefix.removesuffix("/"),
            ),
        ],
    )

ext4_to_pkg = rule(
    doc = """Converts an ext4 image into Fuchsia package resources for Starnix remote_bundle mounts.

    Bazel counterpart of `ext4_to_pkg()` in //src/storage/ext4/ext4_to_pkg.gni.""",
    implementation = _ext4_to_pkg_impl,
    attrs = {
        "input": attr.label(
            doc = "The ext4 image file to extract.",
            allow_single_file = True,
            mandatory = True,
        ),
        "prefix": attr.string(
            doc = "Package destination directory prefix (e.g. 'data/system').",
            mandatory = True,
        ),
        "_ext4_to_pkg": attr.label(
            default = "//src/storage/ext4/bin:ext4_to_pkg",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _build_ext4_fuchsia_package_impl(ctx):
    fuchsia_debug_symbol_info = merge_debug_symbol_infos(
        ctx.attr.subpackages,
        ctx.attr.test_components,
        ctx.attr.components,
        ctx.attr.resources,
        ctx.attr.processed_binaries,
        ctx.attr.tools,
    )
    ffx_package = ctx.executable._package_tool

    archive_name = ctx.attr.archive_name or ctx.attr.package_name
    if not archive_name.endswith(".far"):
        archive_name += ".far"

    pkg_dir = ctx.label.name + "_pkg/"
    base_manifest = ctx.actions.declare_file(pkg_dir + "base_manifest")
    manifest = ctx.actions.declare_file(pkg_dir + "manifest")
    meta_package = ctx.actions.declare_file(pkg_dir + "meta/package")
    meta_far = ctx.actions.declare_file(pkg_dir + "meta.far")
    output_package_manifest = ctx.actions.declare_file(pkg_dir + "package_manifest.json")
    far_file = ctx.actions.declare_file(pkg_dir + archive_name)

    package_resources = [
        make_resource_struct(
            src = meta_package,
            dest = "meta/package",
        ),
    ]
    package_resources.extend(
        ctx.attr.collected_resources[FuchsiaCollectedPackageResourcesInfo].collected_resources.to_list(),
    )

    packaged_components = []
    for dep in ctx.attr.test_components + ctx.attr.components:
        if FuchsiaStructuredConfigInfo in dep:
            sc_info = dep[FuchsiaStructuredConfigInfo]
            package_resources.append(
                make_resource_struct(
                    src = sc_info.cvf_source,
                    dest = sc_info.cvf_dest,
                ),
            )
        component_info = dep[FuchsiaComponentInfo]
        packaged_components.append(FuchsiaPackagedComponentInfo(
            component_info = component_info,
            dest = "meta/%s.cm" % component_info.name,
        ))

    if ctx.attr.processed_binaries:
        package_resources.extend(
            ctx.attr.processed_binaries[FuchsiaPackageResourcesInfo].resources,
        )

    file_resources = [r for r in package_resources if not r.src.is_directory]
    dir_resources = sorted(
        [r for r in package_resources if r.src.is_directory],
        key = lambda s: (s.dest, s.src.path),
    )

    content = "\n".join([
        "%s=%s" % (r.dest, r.src.path)
        for r in sorted(file_resources, key = lambda s: (s.dest, s.src.path))
    ]) + "\n"
    ctx.actions.write(
        output = base_manifest,
        content = content,
    )

    ctx.actions.run_shell(
        inputs = [base_manifest] + [d.src for d in dir_resources],
        outputs = [manifest],
        command = 'out="$1"; shift; LC_ALL=C sort "$@" > "$out"',
        arguments = [manifest.path, base_manifest.path] + [
            d.src.path + "/manifest.fini"
            for d in dir_resources
        ],
        mnemonic = "MergeExt4PackageManifest",
        progress_message = "Merging package manifest for %{label}",
    )

    ctx.actions.write(
        meta_package,
        content = json.encode_indent({
            "name": ctx.attr.package_name,
            "version": "0",
        }),
    )

    build_inputs = [r.src for r in package_resources] + [
        manifest,
        meta_package,
    ]

    ctx.actions.run(
        executable = ffx_package,
        arguments = [
            "package",
            "build",
            manifest.path,
            "-o",
            output_package_manifest.dirname,
            "--published-name",
            ctx.attr.package_name,
            "--api-level",
            "PLATFORM",
        ],
        inputs = build_inputs,
        outputs = [output_package_manifest, meta_far],
        mnemonic = "FuchsiaPackageBuild",
        progress_message = "Building package for %s" % ctx.label,
        **LOCAL_ONLY_ACTION_KWARGS
    )

    ctx.actions.run(
        executable = ffx_package,
        arguments = [
            "package",
            "archive",
            "create",
            output_package_manifest.path,
            "-o",
            far_file.path,
        ],
        inputs = [r.src for r in package_resources] + [
            output_package_manifest,
            meta_far,
        ],
        outputs = [far_file],
        mnemonic = "FuchsiaPackageArchiveCreate",
        progress_message = "Archiving package for %{label}",
        **LOCAL_ONLY_ACTION_KWARGS
    )

    package_files = depset([output_package_manifest, meta_far] + build_inputs)

    return [
        DefaultInfo(
            files = depset([far_file], transitive = [package_files]),
            executable = stub_executable(ctx),
        ),
        FuchsiaPackageInfo(
            fuchsia_cpu = fuchsia_cpu_from_ctx(ctx),
            far_file = far_file,
            package_manifest = output_package_manifest,
            files = package_files.to_list(),
            package_name = ctx.attr.package_name,
            meta_far = meta_far,
            package_resources = package_resources,
            packaged_components = packaged_components,
            build_id_dirs = fuchsia_debug_symbol_info.build_id_dirs_mapping.values(),
        ),
        fuchsia_debug_symbol_info,
        OutputGroupInfo(
            build_id_dirs = depset(transitive = fuchsia_debug_symbol_info.build_id_dirs_mapping.values()),
        ),
    ]

build_ext4_fuchsia_package = rule(
    implementation = _build_ext4_fuchsia_package_impl,
    attrs = {
        k: v
        for k, v in BUILD_FUCHSIA_PACKAGE_COMMON_ATTRS.items()
        if k != "_validate_component_manifests"
    } | {
        "_package_tool": attr.label(
            default = "//src/sys/pkg/bin/package-tool",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _fx_package_impl(
        name,
        package_name,
        archive_name,
        components,
        test_components,
        resources,
        tools,
        subpackages,
        testonly,
        tags,
        visibility,
        **kwargs):
    tags = tags or []
    deps_to_search = (
        (components or []) + (test_components or []) + (resources or []) + (tools or [])
    )

    processed_binaries = "%s_fuchsia_package.elf_binaries" % name
    find_and_process_unstripped_binaries(
        name = processed_binaries,
        deps = deps_to_search,
        testonly = testonly,
        tags = tags + ["manual"],
    )

    collected_resources = "%s_fuchsia_package.resources" % name
    fuchsia_find_all_package_resources(
        name = collected_resources,
        deps = deps_to_search,
        testonly = testonly,
        tags = tags + ["manual"],
    )

    build_ext4_fuchsia_package(
        name = name,
        components = components,
        test_components = test_components,
        resources = resources,
        processed_binaries = ":" + processed_binaries,
        collected_resources = ":" + collected_resources,
        tools = tools,
        subpackages = subpackages,
        package_name = package_name or name,
        archive_name = archive_name,
        testonly = testonly,
        tags = tags,
        visibility = visibility,
        **kwargs
    )

fx_package = macro(
    implementation = _fx_package_impl,
    inherit_attrs = build_ext4_fuchsia_package,
    attrs = {
        "package_name": attr.string(configurable = False),
        "collected_resources": None,
        "processed_binaries": None,
    },
)
