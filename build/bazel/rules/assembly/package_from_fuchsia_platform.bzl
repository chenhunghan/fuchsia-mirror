# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule to use a platform-built Fuchsia package from the `fuchsia_sdk` configuration."""

load("@fuchsia_rules_common//debug_symbols:providers.bzl", "FuchsiaDebugSymbolInfo")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")
load("//build/bazel/platforms/transitions:transitions.bzl", "sdk_to_platform_transition")

visibility(["//products/..."])

def _package_from_fuchsia_platform_impl(ctx):
    # Product configurations are built in the 'fuchsia_sdk' Bazel platform,
    # but in-tree packages (e.g. `fx_package()`) must be built in the
    # non-SDK 'fuchsia_platform' configuration. Using this target from any
    # other platform is a mistake: reference the package directly instead.
    #
    # We don't use the standard 'target_compatible_with = ...' parameter
    # because it doesn't let us provide a custom error message.
    is_fuchsia = ctx.target_platform_has_constraint(
        ctx.attr._fuchsia_constraint[platform_common.ConstraintValueInfo],
    )
    is_with_sdk = ctx.target_platform_has_constraint(
        ctx.attr._with_sdk_rules_constraint[platform_common.ConstraintValueInfo],
    )
    if not is_fuchsia or not is_with_sdk:
        fail("package_from_fuchsia_platform transitions from the 'fuchsia_sdk' Bazel " +
             "platform to 'fuchsia_platform', and can only be used from 'fuchsia_sdk'. " +
             "Depend on '%s' directly instead." % ctx.attr.package[0].label)

    actual = ctx.attr.package[0]
    providers = [
        DefaultInfo(
            files = actual[DefaultInfo].files,
            runfiles = actual[DefaultInfo].default_runfiles,
        ),
        actual[FuchsiaPackageInfo],
    ]
    if FuchsiaDebugSymbolInfo in actual:
        providers.append(actual[FuchsiaDebugSymbolInfo])
    if OutputGroupInfo in actual:
        providers.append(actual[OutputGroupInfo])
    return providers

package_from_fuchsia_platform = rule(
    doc = """Builds an in-tree Fuchsia package in the 'fuchsia_platform' configuration, for use from 'fuchsia_sdk' rules.

    Product configurations (e.g. `fuchsia_product_configuration()`) are built
    in the 'fuchsia_sdk' Bazel platform, where in-tree packages with platform
    C/C++/Rust dependencies cannot be compiled. Wrap such a package with this
    rule and reference the wrapper from the product configuration instead.

    Example usage:

    ```
    package_from_fuchsia_platform(
        name = "my_package",
        package = "//src/my_component:my_package",
    )
    ```
    """,
    implementation = _package_from_fuchsia_platform_impl,
    provides = [FuchsiaPackageInfo],
    attrs = {
        "package": attr.label(
            doc = "The Fuchsia package to build in the 'fuchsia_platform' configuration.",
            mandatory = True,
            providers = [DefaultInfo, FuchsiaPackageInfo],
            cfg = sdk_to_platform_transition,
        ),
        "_fuchsia_constraint": attr.label(
            default = "@platforms//os:fuchsia",
        ),
        "_with_sdk_rules_constraint": attr.label(
            default = "//build/bazel/platforms:fuchsia_artifacts_build_with_sdk_rules",
        ),
    },
)
