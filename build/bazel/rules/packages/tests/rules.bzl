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
    if ctx.attr.expected_far_short_path and pkg_info.far_file.short_path != ctx.attr.expected_far_short_path:
        fail(
            "Expected far_file short_path '{}', got '{}'.".format(
                ctx.attr.expected_far_short_path,
                pkg_info.far_file.short_path,
            ),
        )
    return []

verify_names_in_fx_package = rule(
    doc = (
        "Verifies `FuchsiaPackageInfo` name fields (`package_name`, packaged " +
        "component names, and `far_file` path/basename) of an `fx_package()` target."
    ),
    implementation = _verify_names_in_fx_package_impl,
    attrs = {
        "package": attr.label(
            providers = [FuchsiaPackageInfo],
            mandatory = True,
        ),
        "expected_package_name": attr.string(mandatory = True),
        "expected_component_names": attr.string_list(mandatory = True),
        "expected_far_name": attr.string(),
        "expected_far_short_path": attr.string(),
    },
)

def _verify_fx_test_subpackage_files_impl(ctx):
    pkg_info = ctx.attr.package[FuchsiaPackageInfo]
    pkg_files = {f: True for f in pkg_info.files}
    if len(pkg_files) != len(pkg_info.files):
        fail(
            "Expected no duplicate entries in FuchsiaPackageInfo.files of {} ({} unique vs {} total).".format(
                ctx.attr.package.label,
                len(pkg_files),
                len(pkg_info.files),
            ),
        )

    pkg_default_files = {f: True for f in ctx.attr.package[DefaultInfo].files.to_list()}
    test_default_files = {f: True for f in ctx.attr.test[DefaultInfo].files.to_list()}

    for subpkg in ctx.attr.subpackages:
        subpkg_info = subpkg[FuchsiaPackageInfo]
        expected_files = (
            [subpkg_info.package_manifest, subpkg_info.meta_far] +
            [r.src for r in subpkg_info.package_resources] +
            subpkg_info.files
        )
        for f in expected_files:
            if f not in pkg_files:
                fail(
                    "Expected subpackage file '{}' from {} in FuchsiaPackageInfo.files of {}.".format(
                        f.short_path,
                        subpkg.label,
                        ctx.attr.package.label,
                    ),
                )
            if f not in pkg_default_files:
                fail(
                    "Expected subpackage file '{}' from {} in DefaultInfo.files of {}.".format(
                        f.short_path,
                        subpkg.label,
                        ctx.attr.package.label,
                    ),
                )
            if f not in test_default_files:
                fail(
                    "Expected subpackage file '{}' from {} in DefaultInfo.files of {}.".format(
                        f.short_path,
                        subpkg.label,
                        ctx.attr.test.label,
                    ),
                )
    return []

verify_fx_test_subpackage_files = rule(
    doc = "Verifies that direct and transitive subpackage manifests and blobs are propagated to `FuchsiaPackageInfo.files` and `DefaultInfo.files` of an `fx_package()` and its `fx_test()`.",
    implementation = _verify_fx_test_subpackage_files_impl,
    attrs = {
        "package": attr.label(
            providers = [FuchsiaPackageInfo],
            mandatory = True,
        ),
        "subpackages": attr.label_list(
            providers = [FuchsiaPackageInfo],
            mandatory = True,
        ),
        "test": attr.label(
            mandatory = True,
        ),
    },
)

_TargetCompatibleWithInfo = provider(
    doc = "Captures `target_compatible_with` labels via an aspect for verification.",
    fields = ["labels"],
)

def _collect_target_compatible_with_aspect_impl(_target, ctx):
    compat = []
    if hasattr(ctx.rule.attr, "target_compatible_with"):
        compat = [t.label for t in ctx.rule.attr.target_compatible_with]
    return [_TargetCompatibleWithInfo(labels = compat)]

_collect_target_compatible_with_aspect = aspect(
    implementation = _collect_target_compatible_with_aspect_impl,
)

def _verify_target_compatible_with_impl(ctx):
    actual = sorted(ctx.attr.target[_TargetCompatibleWithInfo].labels)
    expected = sorted([t.label for t in ctx.attr.expected])
    if actual != expected:
        fail(
            "Expected target_compatible_with {} on {}, got {}.".format(
                expected,
                ctx.attr.target.label,
                actual,
            ),
        )
    return []

verify_target_compatible_with = rule(
    doc = "Verifies that `target` resolves to the `expected` `target_compatible_with` constraints.",
    implementation = _verify_target_compatible_with_impl,
    attrs = {
        "target": attr.label(
            mandatory = True,
            aspects = [_collect_target_compatible_with_aspect],
        ),
        "expected": attr.label_list(mandatory = True),
    },
)
