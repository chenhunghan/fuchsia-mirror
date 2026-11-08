# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia C++ shared library."""

load(
    "@fuchsia_rules_common//:transition_utils.bzl",
    "TRANSITIVE_FEATURES_ATTRS",
    "cc_shared_library_apply_transitive_features",
)
load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "BUILD_FLAGS_CC_MACRO_ATTRS",
    "wrap_cc_macro_args_with_build_flags",
)
load(
    "@rules_cc//cc:cc_rules_extension_support.bzl",
    _cc_shared_library_for_inherit_attrs = "cc_shared_library",
)
load("@rules_cc//cc:defs.bzl", "cc_shared_library")
load(
    "//build/bazel/rules/cc:instrumentation.bzl",
    "CC_INSTRUMENTATION_ATTRS",
    "get_cc_instrumentation_features",
)

visibility(["//..."])

def get_shared_library_name_and_soname(name, shared_lib_name = None):
    """Compute the output shared library path and DT_SONAME for a target.

    When `shared_lib_name` is not provided, this matches the output filename
    produced by `@rules_cc` for `cc_binary(linkshared = True)` (and applies
    the same naming to `fx_cc_shared_library()` by setting `shared_lib_name`):
    - If the target basename ends with ".so" or contains ".so." (e.g.
      "libfoo.so", "foo.so", or "libfoo.so.1"), the output filename is
      `name` as-is.
    - Otherwise (e.g. "foo"), the toolchain's DYNAMIC_LIBRARY pattern
      ("lib<basename>.so") is used, producing "libfoo.so".

    Args:
        name: The target name (may include a relative subdirectory prefix).
        shared_lib_name: Optional explicit shared library output path.

    Returns:
        A tuple of `(shared_lib_name, soname)`.
    """
    if not shared_lib_name:
        base_name = name.split("/")[-1]
        prefix = name[:-len(base_name)]
        is_so_name = base_name.endswith(".so") or ".so." in base_name
        file_name = base_name if is_so_name else "lib{}.so".format(base_name)
        shared_lib_name = prefix + file_name
    soname = shared_lib_name.split("/")[-1]
    return shared_lib_name, soname

def _fx_cc_shared_library_impl(
        name,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        build_flags,
        disable_build_flags,
        disable_instrumentation,
        features,
        transitive_features,
        **kwargs):
    """Implementation for the fx_cc_shared_library() macro."""

    if type(transitive_features) != "list":
        fail("In %s, transitive_features is not configurable and must be a list of strings, got %s" % (name, type(transitive_features)))

    features = get_cc_instrumentation_features(features, disable_instrumentation)

    shared_lib_name, soname = get_shared_library_name_and_soname(
        name,
        kwargs.get("shared_lib_name"),
    )

    if transitive_features:
        cc_shlib_name = name + ".actual"
        cc_shlib_visibility = ["//visibility:private"]
        cc_shared_library_apply_transitive_features(
            name = name,
            actual = ":" + cc_shlib_name,
            feature_names = transitive_features,
            tags = kwargs.get("tags"),
            target_compatible_with = kwargs.get("target_compatible_with"),
            testonly = kwargs.get("testonly"),
            visibility = kwargs.pop("visibility", None),
        )
    else:
        cc_shlib_name = name
        cc_shlib_visibility = kwargs.pop("visibility", None)

    kwargs["shared_lib_name"] = shared_lib_name
    kwargs["features"] = features

    wrapped_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs,
        name = cc_shlib_name,
        cc_rule_name = "cc_shared_library",
        build_flags = build_flags,
        disable_build_flags = disable_build_flags,
        target_type = "cxx_shared_library",
    )

    # Prepend default -Wl,-soname=<soname> before the build_flags response file
    # and user_link_flags so that custom soname flags in build_flags(ldflags = ...)
    # or user_link_flags appear later on the linker command line and override it.
    wrapped_kwargs["user_link_flags"] = [
        "-Wl,-soname={}".format(soname),
    ] + (wrapped_kwargs.get("user_link_flags") or [])

    cc_shared_library(
        name = cc_shlib_name,
        visibility = cc_shlib_visibility,
        **wrapped_kwargs
    )

fx_cc_shared_library = macro(
    doc = """Wrapper for cc_shared_library() shared libraries for Fuchsia.

    Toolchain overrides can be specified using build_flags and
    disable_build_flags. These will not affect dependencies.

    If both transitive_features and features are defined, transitive_features
    is applied first to change the build configuration used to build the native
    target, then features will be applied to the cc_shared_library() itself
    (overriding any transitive_features values), but not to its dependencies.
    """,
    implementation = _fx_cc_shared_library_impl,
    inherit_attrs = _cc_shared_library_for_inherit_attrs,
    attrs = {
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
    } | CC_INSTRUMENTATION_ATTRS | BUILD_FLAGS_CC_MACRO_ATTRS | TRANSITIVE_FEATURES_ATTRS,
)
