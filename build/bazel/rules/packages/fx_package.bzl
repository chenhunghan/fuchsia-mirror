# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Non-SDK version of fuchsia_package rule for platform building."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "find_and_process_unstripped_binaries",
    "merge_debug_symbol_infos",
)
load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaCollectedUnstrippedBinariesInfo",
)
load(
    "@fuchsia_rules_common//packages:package.bzl",
    "BUILD_FUCHSIA_PACKAGE_COMMON_ATTRS",
    "build_fuchsia_package_common_impl",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "fuchsia_find_all_package_resources",
)
load(
    "//build/bazel/aspects:component_manifests.bzl",
    "FuchsiaComponentRequiredPackageComponentsInfo",
    "collect_component_manifests_aspect",
)
load(
    ":package_validations.bzl",
    "FuchsiaPackageValidationInfo",
    "validate_package_elf_binaries",
    "validate_package_structured_config",
)

def _prebuilt_unstripped_files(processed_binaries):
    """Returns the unstripped files of binaries packaged from a prebuilt stripped file.

    `find_and_process_unstripped_binaries()` only adds the binaries that it
    strips itself to its `.build-id` directories.
    """
    if not processed_binaries:
        return []
    collected = processed_binaries[FuchsiaCollectedUnstrippedBinariesInfo]
    return [
        binary.unstripped_file
        for binaries in collected.source_search_root_to_unstripped_binaries.values()
        for binary in binaries.to_list()
        if binary.stripped_file != None
    ]

def _build_fuchsia_package_impl(ctx):
    fuchsia_debug_symbol_info = merge_debug_symbol_infos(
        ctx.attr.subpackages,
        ctx.attr.test_components,
        ctx.attr.components,
        ctx.attr.resources,
        ctx.attr.processed_binaries,
        ctx.attr.tools,
    )

    prebuilt_unstripped_files = _prebuilt_unstripped_files(ctx.attr.processed_binaries)

    def create_validations(package_info):
        validations = []
        if ctx.attr.validate_elf_binaries:
            validations.append(validate_package_elf_binaries(
                ctx,
                validate_package_tool = ctx.executable._validate_package_tool,
                package_info = package_info,
                debug_symbol_info = fuchsia_debug_symbol_info,
                prebuilt_unstripped_files = prebuilt_unstripped_files,
            ))
        if ctx.attr.validate_structured_config:
            validations.append(validate_package_structured_config(
                ctx,
                validate_package_tool = ctx.executable._validate_package_tool,
                configc = ctx.executable._configc,
                package_info = package_info,
            ))
        return validations

    return build_fuchsia_package_common_impl(
        ctx,
        ffx_package = ctx.executable._package_tool,
        ffx_package_is_ffx = False,
        cmc_tool = ctx.executable._cmc_tool,
        fuchsia_debug_symbol_info = fuchsia_debug_symbol_info,
        api_level = ctx.attr._current_api_level[BuildSettingInfo].value,
        create_validations = create_validations,
    ) + [
        FuchsiaPackageValidationInfo(
            prebuilt_unstripped_files = prebuilt_unstripped_files,
        ),
    ]

def _format_label(label):
    # Omit the `@@` of labels in the main repository.
    if label.repo_name:
        return str(label)
    return "//{}:{}".format(label.package, label.name)

def _fx_check_required_package_components_impl(ctx):
    packaged = {component.label: True for component in ctx.attr.components}
    missing = {}
    for component in ctx.attr.components:
        if FuchsiaComponentRequiredPackageComponentsInfo not in component:
            continue
        info = component[FuchsiaComponentRequiredPackageComponentsInfo]
        for required in info.required_package_components.to_list():
            if required not in packaged:
                missing.setdefault(required, []).append(component.label)
    if missing:
        fail("\n".join([
            "Package {} is missing components required by its components:".format(
                _format_label(ctx.label.same_package_label(ctx.attr.package_name)),
            ),
        ] + [
            "  {} (required by {})".format(
                _format_label(required),
                ", ".join([_format_label(label) for label in requirers]),
            )
            for required, requirers in missing.items()
        ] + [
            "Add them to the `components` of the package. These requirements come from " +
            "`expect_includes(required_package_components = ...)` targets in the " +
            "dependencies of the requiring components.",
        ]))
    return []

_fx_check_required_package_components = rule(
    doc = """Fails analysis if a package lacks components that its components require.

    Applies `collect_component_manifests_aspect` to `components` to read each
    component's `FuchsiaComponentRequiredPackageComponentsInfo`, which holds the
    `required_package_components` of the `expect_includes()` targets in its
    dependencies (e.g. the Realm Builder server for components that use
    `//src/lib/fuchsia-component-test`). Each required component must be one of
    `components`: a component in a subpackage does not satisfy the requirement,
    because the requiring manifest refers to it by a relative URL.
    """,
    implementation = _fx_check_required_package_components_impl,
    attrs = {
        "components": attr.label_list(
            doc = "The `components` and `test_components` of the package.",
            aspects = [collect_component_manifests_aspect],
        ),
        "package_name": attr.string(
            doc = "The target name of the package, for error messages.",
            mandatory = True,
        ),
    },
)

build_fuchsia_package = rule(
    implementation = _build_fuchsia_package_impl,
    doc = """
    Generates actions to build and archive a Fuchsia package containing components,
    resources, tools, and subpackages.

    This rule invokes the common implementation with platform-built tools.

    Like GN's `fuchsia_package()`, it verifies the package's ELF binaries and
    validates its structured config. These run as Bazel validation actions,
    in parallel with the actions that consume the package.
    """,
    attrs = BUILD_FUCHSIA_PACKAGE_COMMON_ATTRS | {
        "validate_elf_binaries": attr.bool(
            doc = """Set to False to skip the verification of the ELF binaries in this package.

            The verification checks that every ELF binary outside of `data/`,
            `lib/firmware/` and `meta/` finds its shared libraries in the
            package, is stripped, and has its debug symbols available.
            Corresponds to `!disable_elf_binaries_checks` on GN's
            `fuchsia_package()`.""",
            default = True,
        ),
        "required_package_components_check": attr.label(
            doc = """An internal `_fx_check_required_package_components()` target.

            Depending on it runs its analysis-time check whenever this package
            is analyzed.""",
        ),
        "validate_structured_config": attr.bool(
            doc = """Set to False to skip the validation of the structured config of this package.

            The validation checks that every component in the package that
            declares structured config has its configuration values in the
            package. Test packages that provide all values through RealmBuilder
            set this to False. Matches the GN `fuchsia_package()` parameter of
            the same name.""",
            default = True,
        ),
        "_package_tool": attr.label(
            default = "//src/sys/pkg/bin/package-tool",
            executable = True,
            cfg = "exec",
        ),
        "_cmc_tool": attr.label(
            default = "//tools/cmc:cmc",
            executable = True,
            cfg = "exec",
        ),
        "_configc": attr.label(
            default = "//tools/configc",
            executable = True,
            cfg = "exec",
        ),
        "_validate_package_tool": attr.label(
            default = "//build/bazel/rules/packages/tools:validate_package",
            executable = True,
            cfg = "exec",
        ),
        "_current_api_level": attr.label(
            default = "@//build/bazel/versioning:api_level",
        ),
    },
)

def fx_package_common_impl(
        name,
        package_name,
        archive_name,
        components = None,
        test_components = None,
        resources = None,
        tools = None,
        subpackages = None,
        testonly = None,
        tags = None,
        visibility = None,
        target_compatible_with = None,
        rebase_collected_resources = None,
        **kwargs):
    """Shared macro implementation for `fx_package()` and `fx_bootfs_files_package()`.

    Args:
      name: Target name.
      package_name: Optional Fuchsia package name override.
      archive_name: Optional FAR archive name override.
      components: Optional list of `fx_component()` targets.
      test_components: Optional list of `fx_test_component()` targets.
      resources: Optional list of `resource()` or `fx_packaged_binary()` targets.
      tools: Optional list of `fx_tool()` targets.
      subpackages: Optional list of subpackage targets.
      testonly: Whether the target is testonly.
      tags: Optional list of Bazel tags.
      visibility: Target visibility.
      target_compatible_with: Optional platform compatibility constraints.
        Defaults to `["@platforms//os:fuchsia"]`.
      rebase_collected_resources: Optional rule/callable to transform collected resources.
      **kwargs: Additional keyword arguments forwarded to `build_fuchsia_package`.
    """

    if package_name == name:
        fail("The specified `package_name` (`%s`) matches the default. `package_name` only needs to be specified when overriding the default." % package_name)

    # The default value of non-mandatory inherited attributes is always overridden to be None,
    # regardless of the original attribute definition's default value. Thus, `testonly` must be
    # checked against `None` to distinguish an explicit `testonly = False` from an omitted value,
    # and inherited list attributes below require `or []` before concatenation.
    #
    # See https://bazel.build/extending/macros#attribute-inheritance.
    if test_components:
        if testonly != None and not testonly:
            fail("`fx_package()` targets with `test_components` are always testonly.")
        testonly = True
    tags = tags or []
    _deps_to_search = (
        (components or []) + (test_components or []) + (resources or []) + (tools or [])
    )

    processed_binaries = "%s_fuchsia_package.elf_binaries" % name
    find_and_process_unstripped_binaries(
        name = processed_binaries,
        deps = _deps_to_search,
        testonly = testonly,
        tags = tags + ["manual"],
    )

    collected_resources = "%s_fuchsia_package.resources" % name
    if rebase_collected_resources:
        raw_collected_resources = "%s_fuchsia_package.raw_resources" % name
        fuchsia_find_all_package_resources(
            name = raw_collected_resources,
            deps = _deps_to_search,
            testonly = testonly,
            tags = tags + ["manual"],
        )
        rebase_collected_resources(
            name = collected_resources,
            collected_resources = ":" + raw_collected_resources,
            testonly = testonly,
            tags = tags + ["manual"],
        )
    else:
        fuchsia_find_all_package_resources(
            name = collected_resources,
            deps = _deps_to_search,
            testonly = testonly,
            tags = tags + ["manual"],
        )

    required_package_components_check = "%s_fuchsia_package.required_package_components" % name
    _fx_check_required_package_components(
        name = required_package_components_check,
        components = (components or []) + (test_components or []),
        package_name = name,
        testonly = testonly,
        tags = tags + ["manual"],
    )

    if target_compatible_with == None:
        target_compatible_with = ["@platforms//os:fuchsia"]

    build_fuchsia_package(
        name = name,
        components = components,
        test_components = test_components,
        resources = resources,
        processed_binaries = ":" + processed_binaries,
        collected_resources = ":" + collected_resources,
        required_package_components_check = ":" + required_package_components_check,
        tools = tools,
        subpackages = subpackages,
        package_name = package_name or name,
        archive_name = archive_name,
        testonly = testonly,
        tags = tags,
        visibility = visibility,
        target_compatible_with = target_compatible_with,
        **kwargs
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
    fx_package_common_impl(
        name = name,
        package_name = package_name,
        archive_name = archive_name,
        components = components,
        test_components = test_components,
        resources = resources,
        tools = tools,
        subpackages = subpackages,
        testonly = testonly,
        tags = tags,
        visibility = visibility,
        **kwargs
    )

fx_package = macro(
    implementation = _fx_package_impl,
    inherit_attrs = build_fuchsia_package,
    doc = """Produces a Fuchsia package that can be published to a package server and loaded on a device.

    Defaults `target_compatible_with` to `["@platforms//os:fuchsia"]` unless explicitly
    overridden (for example with `target_compatible_with = []`).

    Example usage:

    ```
    fx_package(
        name = "pkg",
        components = [":my_component"],
        tools = [":my_tool"]
    )
    ```""",
    attrs = {
        "package_name": attr.string(
            doc = """An optional name to use for this package, defaults to target name.

            Defaults to `name` to match the behavior of the GN `fuchsia_package()` template.
            Note that this is different from the Bazel SDK behavior. Only set
            this when it differs from `name`.""",
            # Otherwise an unset value reaches the impl as an always-truthy
            # select(), and the `name` fallback never applies.
            configurable = False,
        ),
        # Do not inherit implementation details passed to `_build_fuchsia_package()`.
        "collected_resources": None,
        "processed_binaries": None,
        "required_package_components_check": None,
    },
)
