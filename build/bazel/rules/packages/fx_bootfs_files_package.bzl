# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro and rules for packaging BootFS files for Fuchsia Assembly Input Bundles."""

load(
    "@fuchsia_rules_common//:utils.bzl",
    "make_resource_struct",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaCollectedPackageResourcesInfo",
)
load(
    "//build/bazel/rules/packages:fx_package.bzl",
    "build_fuchsia_package",
    "fx_package_common_impl",
)

visibility(["//..."])

def _rebase_bootfs_dest(dest):
    """Rebases `meta/...` destinations to `bootfs/meta/...`.

    `package-tool package build` packs any entry under `meta/` into `meta.far`,
    which Assembly's `bootfs_files_from_package()` ignores. Rebasing `meta/...`
    to `bootfs/meta/...` keeps BootFS component manifests and metadata files as
    standalone blobs in `package_manifest.json`; Assembly strips the `bootfs/`
    prefix when populating BootFS.
    """
    normalized = dest.removeprefix("/")
    if normalized == "meta" or normalized.startswith("meta/"):
        return "bootfs/" + normalized
    return normalized

def _bootfs_rebase_resources_impl(ctx):
    rebased = [
        make_resource_struct(
            src = r.src,
            dest = _rebase_bootfs_dest(r.dest),
        )
        for r in ctx.attr.collected_resources[FuchsiaCollectedPackageResourcesInfo].collected_resources.to_list()
    ]

    return [
        FuchsiaCollectedPackageResourcesInfo(
            collected_resources = depset(rebased),
        ),
    ]

_bootfs_rebase_resources = rule(
    doc = "Rebases `meta/...` package resources to `bootfs/meta/...`.",
    implementation = _bootfs_rebase_resources_impl,
    provides = [FuchsiaCollectedPackageResourcesInfo],
    attrs = {
        "collected_resources": attr.label(
            doc = "The `fuchsia_find_all_package_resources()` target for this BootFS package.",
            providers = [FuchsiaCollectedPackageResourcesInfo],
            mandatory = True,
        ),
    },
)

def _fx_bootfs_files_package_impl(
        name,
        package_name,
        archive_name,
        components,
        resources,
        testonly,
        tags,
        visibility,
        **kwargs):
    fx_package_common_impl(
        name = name,
        package_name = package_name,
        archive_name = archive_name,
        components = components,
        resources = resources,
        testonly = testonly,
        tags = tags,
        visibility = visibility,
        rebase_collected_resources = _bootfs_rebase_resources,
        **kwargs
    )

fx_bootfs_files_package = macro(
    implementation = _fx_bootfs_files_package_impl,
    inherit_attrs = build_fuchsia_package,
    doc = """Produces a Fuchsia package of BootFS files for `assembly_input_bundle(bootfs_files_package = ...)`.

    This is the Bazel equivalent of GN's `bootfs_files_for_assembly()` paired with
    `fuchsia_package()` for Assembly Input Bundles:
    - `components` accepts non-test `fx_component()` targets whose manifests are
      validated with `cmc validate-references` and rebased from `meta/<name>.cm`
      to `bootfs/meta/<name>.cm`.
    - `resources` accepts resource-like targets (`resource()`, `fx_packaged_binary()`,
      etc.). Resource entries are installed at their `dest` path in BootFS, with
      any `meta/...` destinations automatically rebased to `bootfs/meta/...` so
      `package-tool` emits them as standalone blobs rather than packing them into
      `meta.far` (Assembly strips the `bootfs/` prefix when populating BootFS).
    - Binaries and renamed binaries are wrapped in `fx_packaged_binary()` and passed
      in `resources` (or in `deps` of a component in `components`); they are stripped,
      paired with their runtime shared libraries and debug symbols, and installed at
      `bin/<binary_name>` in BootFS.

    Example usage:

    ```bazel
    fx_bootfs_files_package(
        name = "bootfs",
        components = [":my_bootfs_component"],
        resources = [
            ":config",
            ":my_packaged_bin",
        ],
    )
    ```
    """,
    attrs = {
        "package_name": attr.string(
            doc = """An optional name to use for this package, defaults to target `name`.""",
            configurable = False,
        ),
        # Hide internal or non-applicable `COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES` attributes.
        "collected_resources": None,
        "package_repository_name": None,
        "processed_binaries": None,
        "subpackages": None,
        "test_components": None,
        "tools": None,
    },
)
