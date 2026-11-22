# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""icu_fuchsia_component() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "fx_component",
    "fx_component_manifest",
)
load(
    "//build/bazel/rules/icu:icu_names.bzl",
    "icu_fixup_deps",
    "icu_flavored_label",
    "icu_flavored_name",
)

visibility(["//..."])

def icu_fuchsia_component(
        *,
        name,
        component_name = None,
        manifest = None,
        compiled_manifest = None,
        includes = None,
        restricted_features = None,
        check_includes = True,
        deps = None,
        icu_deps = None,
        icu_public_deps = None,
        testonly = None,
        visibility = None):
    """Defines an ICU flavor-aware Fuchsia component via `fx_component()`.

    This is the Bazel counterpart of the GN `icu_fuchsia_component()` template defined in
    `//build/icu/icu_fuchsia_component.gni`.

    Args:
        name: Target name.
        component_name: Optional component name (defaults to `name`).
        manifest: Component manifest `.cml` file label or path. Mutually exclusive with
            `compiled_manifest`.
        compiled_manifest: Label of an `fx_component_manifest()` target. Mutually exclusive with
            `manifest`.
        includes: Optional list of `.cml` shard labels included by `manifest` (used in Bazel's
            sandboxed action; omitted by `bazel2gn`).
        restricted_features: Optional list of restricted CML features.
        check_includes: Whether to check CML includes (defaults to `True`).
        deps: Non-ICU component dependencies.
        icu_deps: ICU-flavored component dependencies.
        icu_public_deps: Public ICU-flavored component dependencies.
        testonly: Standard Bazel `testonly` attribute.
        visibility: Standard Bazel `visibility` attribute.
    """
    if (manifest == None) == (compiled_manifest == None):
        fail("icu_fuchsia_component %r requires exactly one of `manifest` or `compiled_manifest`" % name)

    cm_target = compiled_manifest
    if manifest != None:
        cm_name = "%s_manifest" % name
        fx_component_manifest(
            name = cm_name,
            component_name = component_name or name,
            manifest = manifest,
            includes = includes,
            restricted_features = restricted_features,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        cm_target = ":%s" % cm_name

    base_deps = list(deps) if deps else []
    extra_icu_deps = (list(icu_deps) if icu_deps else []) + (list(icu_public_deps) if icu_public_deps else [])

    for icu_flavor in icu_flavors:
        flavored_icu_deps = [str(icu_flavored_label(d, icu_flavor)) for d in extra_icu_deps]
        fx_component(
            name = icu_flavored_name(name, icu_flavor),
            compiled_manifest = cm_target,
            check_includes = check_includes,
            deps = icu_fixup_deps(base_deps, icu_flavor) + flavored_icu_deps,
            testonly = testonly,
            visibility = visibility,
        )

    fx_component(
        name = name,
        compiled_manifest = cm_target,
        check_includes = check_includes,
        deps = base_deps + extra_icu_deps,
        testonly = testonly,
        visibility = visibility,
    )
