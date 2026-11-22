# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""icu_fuchsia_package() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load(
    "//build/bazel/rules/icu:icu_names.bzl",
    "icu_fixup_deps",
    "icu_flavored_label",
    "icu_flavored_name",
)
load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")

visibility(["//..."])

def icu_fuchsia_package(
        *,
        name,
        package_name = None,
        components = None,
        resources = None,
        icu_components = None,
        icu_resources = None,
        subpackages = None,
        validate_elf_binaries = None,
        validate_structured_config = None,
        testonly = None,
        visibility = None,
        **kwargs):
    """Defines an ICU flavor-aware Fuchsia package via `fx_package()`.

    This is the Bazel counterpart of the GN `icu_fuchsia_package()` template defined in
    `//build/icu/icu_fuchsia_package.gni`. Components and resources are passed in
    separate attributes for `fx_package()`, and `bazel2gn` merges `components`+`resources`
    into GN `deps` and `icu_components`+`icu_resources` into GN `icu_deps`.

    Args:
        name: Target name.
        package_name: Optional Fuchsia package name (defaults to target name in `fx_package`).
        components: Non-ICU component targets.
        resources: Non-ICU resource or structured config value targets.
        icu_components: ICU-flavored component targets.
        icu_resources: ICU-flavored resource targets.
        subpackages: Optional list of subpackage labels.
        validate_elf_binaries: Passed to `fx_package()` (`False` maps to
            `disable_elf_binaries_checks = true` in GN).
        validate_structured_config: Passed to `fx_package()`.
        testonly: Standard Bazel `testonly` attribute.
        visibility: Standard Bazel `visibility` attribute.
        **kwargs: Additional keyword arguments forwarded to `fx_package()`.
    """
    base_components = list(components) if components else []
    base_resources = list(resources) if resources else []
    extra_icu_components = list(icu_components) if icu_components else []
    extra_icu_resources = list(icu_resources) if icu_resources else []

    extra_kwargs = dict(kwargs)
    if package_name != None:
        extra_kwargs["package_name"] = package_name
    if subpackages != None:
        extra_kwargs["subpackages"] = subpackages
    if validate_elf_binaries != None:
        extra_kwargs["validate_elf_binaries"] = validate_elf_binaries
    if validate_structured_config != None:
        extra_kwargs["validate_structured_config"] = validate_structured_config

    for icu_flavor in icu_flavors:
        flavored_components = icu_fixup_deps(base_components, icu_flavor) + [
            str(icu_flavored_label(c, icu_flavor))
            for c in extra_icu_components
        ]
        flavored_resources = icu_fixup_deps(base_resources, icu_flavor) + [
            str(icu_flavored_label(r, icu_flavor))
            for r in extra_icu_resources
        ]
        fx_package(
            name = icu_flavored_name(name, icu_flavor),
            components = flavored_components,
            resources = flavored_resources,
            testonly = testonly,
            visibility = visibility,
            **extra_kwargs
        )

    fx_package(
        name = name,
        components = base_components + extra_icu_components,
        resources = base_resources + extra_icu_resources,
        testonly = testonly,
        visibility = visibility,
        **extra_kwargs
    )
