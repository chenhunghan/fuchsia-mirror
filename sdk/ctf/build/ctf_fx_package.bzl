# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel equivalent of the GN `ctf_fuchsia_package()` template."""

load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")
load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")

# CTF tests live with the code they test, under these top-level directories.
visibility([
    "//sdk/ctf/...",
    "//src/...",
])

CtfPackageArchivesInfo = provider(
    doc = "The package archives that a target contributes to a CTF release.",
    fields = {
        "archives": """A depset of structs, one per package archive, with the fields:
            - `label`: The label of the `ctf_fx_package()` archive target, as a string.
            - `archive`: The package archive (`.far`) file.
            - `archive_name`: The base name of the archive in the CTF release.
            - `package_name`: The name of the package in the archive.
            - `test_component_names`: The names of the components in the
              package that are tests.""",
    },
)

def _ctf_package_archive_impl(ctx):
    package_info = ctx.attr.package[FuchsiaPackageInfo]

    component_names = [
        component.component_info.name
        for component in package_info.packaged_components
    ]
    for test_component_name in ctx.attr.test_component_names:
        if test_component_name not in component_names:
            fail(
                "`test_component_names` lists `{}`, but package `{}` only has the components {}.".format(
                    test_component_name,
                    package_info.package_name,
                    component_names,
                ),
            )

    archive = struct(
        label = str(ctx.label),
        archive = package_info.far_file,
        # The GN `ctf_fuchsia_package()` template also names the archive
        # after the package.
        archive_name = package_info.package_name,
        package_name = package_info.package_name,
        test_component_names = ctx.attr.test_component_names,
    )

    return [
        DefaultInfo(files = depset([package_info.far_file])),
        CtfPackageArchivesInfo(archives = depset([archive])),
    ]

_ctf_package_archive = rule(
    doc = "Declares the archive of a Fuchsia package as an artifact of a CTF release.",
    implementation = _ctf_package_archive_impl,
    attrs = {
        "package": attr.label(
            doc = "The Fuchsia package to release in CTF.",
            mandatory = True,
            providers = [FuchsiaPackageInfo],
        ),
        "test_component_names": attr.string_list(
            doc = "The names of the components in the package that are tests.",
        ),
    },
    provides = [CtfPackageArchivesInfo],
)

def _ctf_fx_package_impl(
        name,
        package_name,
        test_component_names,
        testonly,
        tags,
        visibility,
        **kwargs):
    if testonly != None and not testonly:
        fail("`ctf_fx_package()` targets are always testonly.")

    fx_package(
        name = name,
        package_name = package_name,
        testonly = True,
        tags = tags,
        visibility = visibility,
        **kwargs
    )

    _ctf_package_archive(
        name = name + "_archive",
        package = ":" + name,
        test_component_names = test_component_names,
        testonly = True,
        tags = tags,
        visibility = visibility,
    )

ctf_fx_package = macro(
    doc = """Produces a Fuchsia package that can be released in CTF.

    The equivalent of the GN `ctf_fuchsia_package()` template. `name` is an
    `fx_package()`, and `{name}_archive` is its package archive, which
    provides `CtfPackageArchivesInfo`.

    Like for `fx_package()`, `package_name` defaults to `name`. The package
    name is also the name of the archive in CTF releases.

    Example usage:

    ```bazel
    ctf_fx_package(
        name = "pkg",
        package_name = "my-ctf-tests",
        components = [":my_component"],
    )
    ```
    """,
    implementation = _ctf_fx_package_impl,
    inherit_attrs = fx_package,
    attrs = {
        "test_component_names": attr.string_list(
            doc = """The names of the components in the package that are tests.

            A component named `foo` is expected at `meta/foo.cm`.""",
            configurable = False,
        ),
        # The archive of a CTF package is always named after the package.
        "archive_name": None,
    },
)
