# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper rules for the tests in this package."""

load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageInfo",
)

visibility("private")

def _verify_names_in_fx_package_impl(ctx):
    pkg_info = ctx.attr.package[FuchsiaPackageInfo]
    if pkg_info.package_name != ctx.attr.expected_package_name:
        fail(
            "Expected package_name '{}', got '{}'.".format(
                ctx.attr.expected_package_name,
                pkg_info.package_name,
            ),
        )
    actual_components = sorted([
        c.component_info.name
        for c in pkg_info.packaged_components
    ])
    expected_components = sorted(ctx.attr.expected_component_names)
    if actual_components != expected_components:
        fail(
            "Expected component names {}, got {}.".format(
                expected_components,
                actual_components,
            ),
        )
    if ctx.attr.expected_far_name and pkg_info.far_file.basename != ctx.attr.expected_far_name:
        fail(
            "Expected far_file basename '{}', got '{}'.".format(
                ctx.attr.expected_far_name,
                pkg_info.far_file.basename,
            ),
        )
    return []

verify_names_in_fx_package = rule(
    doc = "Verifies `FuchsiaPackageInfo` name fields (`package_name`, packaged component names, and `far_file` basename) of an `fx_package()` target.",
    implementation = _verify_names_in_fx_package_impl,
    attrs = {
        "package": attr.label(
            providers = [FuchsiaPackageInfo],
            mandatory = True,
        ),
        "expected_package_name": attr.string(mandatory = True),
        "expected_component_names": attr.string_list(mandatory = True),
        "expected_far_name": attr.string(),
    },
)
