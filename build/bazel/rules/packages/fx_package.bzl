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
    "@fuchsia_rules_common//packages:package.bzl",
    "COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES",
    "common_build_fuchsia_package_impl",
)
load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "fuchsia_find_all_package_resources",
)

def _build_fuchsia_package_impl(ctx):
    fuchsia_debug_symbol_info = merge_debug_symbol_infos(
        ctx.attr.subpackages,
        ctx.attr.test_components,
        ctx.attr.components,
        ctx.attr.resources,
        ctx.attr.processed_binaries,
        ctx.attr.tools,
    )

    return common_build_fuchsia_package_impl(
        ctx,
        ffx_package = ctx.executable._package_tool,
        ffx_package_is_ffx = False,
        cmc_tool = ctx.executable._cmc_tool,
        fuchsia_debug_symbol_info = fuchsia_debug_symbol_info,
        api_level = ctx.attr._current_api_level[BuildSettingInfo].value,
    )

build_fuchsia_package = rule(
    implementation = _build_fuchsia_package_impl,
    doc = """
    Generates actions to build and archive a Fuchsia package containing components,
    resources, tools, and subpackages.

    This rule invokes the common implementation with platform-built tools.
    """,
    attrs = COMMON_BUILD_FUCHSIA_PACKAGE_ATTRIBUTES | {
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
      rebase_collected_resources: Optional rule/callable to transform collected resources.
      **kwargs: Additional keyword arguments forwarded to `build_fuchsia_package`.
    """

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

    build_fuchsia_package(
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
            Note that this is different from the Bazel SDK behavior.""",
            # Otherwise an unset value reaches the impl as an always-truthy
            # select(), and the `name` fallback never applies.
            configurable = False,
        ),
        # Do not inherit implementation details passed to `_build_fuchsia_package()`.
        "collected_resources": None,
        "processed_binaries": None,
    },
)
