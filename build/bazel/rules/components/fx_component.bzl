# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macros for defining a Fuchsia component and component manifest."""

load(
    "@fuchsia_rules_common//components:component_manifest.bzl",
    "compile_component_manifest",
)
load(
    "@fuchsia_rules_common//components:fuchsia_component_common.bzl",
    "fuchsia_component_common",
)

# Maps `test_type` values to the realm moniker the test component runs in.
# LINT.IfChange(type_moniker_map)
_TYPE_MONIKER_MAP = {
    # keep-sorted start
    "bootstrap_driver_system": "/bootstrap/testing/driver-system-tests",
    "chromium": "/core/testing/chromium-tests",
    "component_framework": "/core/testing/component-framework-tests",
    "ctf": "/core/testing/ctf-tests",
    "device": "/core/testing/devices-tests",
    "driver_system": "/core/testing/driver-system-tests",
    "drm": "/core/testing/drm-tests",
    "starnix": "/core/testing/starnix-tests",
    "storage": "/core/testing/storage-tests",
    "system": "/core/testing/system-tests",
    "system_validation": "/core/testing/system-validation-tests",
    "test_arch": "/core/testing/test-arch-tests",
    "vfs_compliance": "/core/testing/vfs-compliance-tests",
    "vulkan": "/core/testing/vulkan-tests",
    # keep-sorted end
}
# LINT.ThenChange(//build/components/fuchsia_test_component.gni:type_moniker_map)

def resolve_test_type_realm(test_type):
    """Maps a `test_type` string to its test realm moniker.

    Args:
        test_type: A key of `_TYPE_MONIKER_MAP`, or an empty string or `None`
            for the default hermetic test realm.

    Returns:
        The test realm moniker, or `None` if `test_type` is not set.
    """
    if not test_type:
        return None
    if test_type not in _TYPE_MONIKER_MAP:
        fail(
            "Invalid `test_type` {}. Valid values are: {}".format(
                repr(test_type),
                ", ".join(sorted(_TYPE_MONIKER_MAP.keys())),
            ),
        )
    return _TYPE_MONIKER_MAP[test_type]

def _fx_component_manifest_impl(ctx):
    manifest_in = ctx.file.manifest
    component_name = ctx.attr.component_name or ctx.label.name

    cmc = ctx.executable._cmc_tool

    return compile_component_manifest(
        ctx = ctx,
        cmc_tool = cmc,
        manifest_in = manifest_in,
        component_name = component_name,
        includes = ctx.files.includes,
        include_paths = [".", "sdk/lib"],
    )

fx_component_manifest = rule(
    doc = """Compiles a Fuchsia component manifest (.cml) into a binary component manifest (.cm).

This rule executes the component manifest compiler (cmc) tool to validate
and compile the input manifest file, including any specified dependency shards.
""",
    implementation = _fx_component_manifest_impl,
    attrs = {
        "manifest": attr.label(
            doc = "The component manifest file (.cml) to compile.",
            allow_single_file = [".cml"],
            mandatory = True,
        ),
        "component_name": attr.string(
            doc = "The name of the component. Defaults to the label name of the target.",
        ),
        "includes": attr.label_list(
            doc = """Other manifest shard files (.shard.cml) to include during compilation.
Explicitly setting this is necessary for sandboxed build action execution.""",
            allow_files = [".shard.cml"],
        ),
        "_cmc_tool": attr.label(
            doc = "The path to the component manifest compiler (cmc) tool.",
            default = "//tools/cmc:cmc",
            executable = True,
            cfg = "exec",
        ),
    },
)

def _fx_component_impl(
        name,
        component_name,
        compiled_manifest,
        deps,
        testonly,
        visibility,
        **kwargs):
    fuchsia_component_common(
        name = name,
        compiled_manifest = compiled_manifest,
        component_name = component_name or name,
        deps = deps,
        testonly = testonly,
        visibility = visibility,

        # Attributes not supported by the in-tree macro.
        moniker = None,
        is_driver = False,
        is_test = False,

        # Forward extra attributes.
        **kwargs
    )

_COMMON_COMPONENT_ATTRS = {
    # The behavior is different from that documented for the inherited attribute.
    "component_name": attr.string(
        doc = """The name of the component.

        This value will override the component name value in the `compiled_manifest`.
        Defaults to the `name` of this target.
        """,
        mandatory = False,
    ),

    # This inherited attribute is set by the individual macros. Prevent callers from setting it.
    "is_test": None,

    # TODO(https://fxbug.dev/520207779): Determine whether we need these attributes for platform
    # packages.
    "moniker": None,
    "is_driver": None,
}

fx_component = macro(
    doc = """Creates a Fuchsia component which can be added to a package.

This macro will take a component manifest and compile it into a form that
is suitable to be included in a package. The component can include any
number of dependencies which will be included in the final package.
""",
    implementation = _fx_component_impl,
    inherit_attrs = fuchsia_component_common,
    attrs = _COMMON_COMPONENT_ATTRS,
)

def _fx_test_component_impl(
        name,
        component_name,
        compiled_manifest,
        deps,
        test_type,
        testonly,
        visibility,
        **kwargs):
    # Inherited attributes that are not set default to None, so only an explicit
    # `testonly = False` is an error.
    # See https://bazel.build/extending/macros#attribute-inheritance.
    if testonly != None and not testonly:
        fail("`fx_test_component()` targets are always testonly.")

    fuchsia_component_common(
        name = name,
        compiled_manifest = compiled_manifest,
        component_name = component_name or name,
        deps = deps,
        testonly = True,
        visibility = visibility,

        # Attributes not supported by the in-tree macro.
        moniker = None,
        is_driver = False,

        # Marks the component as a test, which is required by
        # `fx_package(test_components = ...)` and used by `fx_test()` to
        # distinguish test components from other components in the package.
        is_test = True,
        test_realm = resolve_test_type_realm(test_type),

        # Forward extra attributes.
        **kwargs
    )

fx_test_component = macro(
    doc = """Creates a Fuchsia test component which can be added to an `fx_package()`.

This is the test equivalent of `fx_component()`: the resulting component is
always testonly, and is marked as a test component, which means it must be
listed in the `test_components` attribute of `fx_package()` (and not in
`components`).
""",
    implementation = _fx_test_component_impl,
    inherit_attrs = fuchsia_component_common,
    attrs = _COMMON_COMPONENT_ATTRS | {
        # Set from `test_type`.
        "test_realm": None,
        "test_type": attr.string(
            doc = """The non-hermetic test realm type to run the test component in (e.g. `"starnix"` or `"system"`).

            Must be a key of `_TYPE_MONIKER_MAP` in
            //build/bazel/rules/components/fx_component.bzl, which maps it to
            the test realm moniker. If omitted, the test runs in the default
            hermetic test realm.
            See https://fuchsia.dev/fuchsia-src/development/testing/components/test_runner_framework#non-hermetic_tests
            for valid types.
            """,
            configurable = False,
        ),
    },
)
