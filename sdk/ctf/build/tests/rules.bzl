# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for the tests in this package."""

load("//sdk/ctf/build:ctf_fx_package.bzl", "CtfPackageArchivesInfo")

visibility("private")

def _verify_ctf_package_archive_impl(ctx):
    archives = ctx.attr.archive[CtfPackageArchivesInfo].archives.to_list()
    if len(archives) != 1:
        fail("Expected one archive, got {}.".format(len(archives)))
    archive = archives[0]

    actual = {
        "archive_basename": archive.archive.basename,
        "archive_name": archive.archive_name,
        "package_name": archive.package_name,
        "test_component_names": archive.test_component_names,
    }
    expected = {
        "archive_basename": ctx.attr.expected_package_name + ".far",
        "archive_name": ctx.attr.expected_package_name,
        "package_name": ctx.attr.expected_package_name,
        "test_component_names": ctx.attr.expected_test_component_names,
    }
    if actual != expected:
        fail("Expected archive {}, got {}.".format(expected, actual))

    files = ctx.attr.archive[DefaultInfo].files.to_list()
    if files != [archive.archive]:
        fail("Expected the default output to be the archive, got {}.".format(files))
    return []

verify_ctf_package_archive = rule(
    doc = "Verifies the `CtfPackageArchivesInfo` of the `_archive` target of a `ctf_fx_package()`.",
    implementation = _verify_ctf_package_archive_impl,
    attrs = {
        "archive": attr.label(
            providers = [CtfPackageArchivesInfo],
            mandatory = True,
        ),
        "expected_package_name": attr.string(mandatory = True),
        "expected_test_component_names": attr.string_list(),
    },
)
