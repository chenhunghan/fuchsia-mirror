# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a Fuchsia C++ binary."""

load(
    "@fuchsia_rules_common//:transition_utils.bzl",
    "TRANSITIVE_FEATURES_ATTRS",
    "cc_apply_transitive_features",
)
load(
    "@fuchsia_rules_common//build_flags:cc.bzl",
    "BUILD_FLAGS_CC_MACRO_ATTRS",
    "wrap_cc_macro_args_with_build_flags",
)
load("@rules_cc//cc:cc_rules_extension_support.bzl", _cc_binary_for_inherit_attrs = "cc_binary")
load("@rules_cc//cc:defs.bzl", "cc_binary")
load(
    "//build/bazel/rules/cc:fx_cc_shared_library.bzl",
    "get_shared_library_name_and_soname",
)
load(
    "//build/bazel/rules/cc:instrumentation.bzl",
    "CC_INSTRUMENTATION_ATTRS",
    "get_cc_instrumentation_features",
)

def _fx_cc_binary_impl(
        name,
        linkshared,
        public_configs,  # buildifier: disable=unused-variable - For GN conversion only.
        gn_output_name,  # buildifier: disable=unused-variable - For GN conversion only.
        disable_syslog_backend,  # buildifier: disable=unused-variable - For GN conversion only.
        build_flags,
        disable_build_flags,
        disable_instrumentation,
        features,
        transitive_features,
        **kwargs):
    """Implementation for the fx_cc_binary() macro."""

    if type(transitive_features) != "list":
        fail("In %s, transitive_features is not configurable and must be a list of strings, got %s" % (name, type(transitive_features)))

    features = get_cc_instrumentation_features(features, disable_instrumentation)

    shared_lib_name, soname = get_shared_library_name_and_soname(name) if linkshared else (None, None)

    if transitive_features:
        if linkshared:
            # Place the inner cc_binary in a `<name>.actual/` subdirectory with
            # the original basename so @rules_cc produces `<name>.actual/lib<name>.so`
            # rather than `lib<name>.actual.so`.
            cc_binary_name = "{}.actual/{}".format(name, name.split("/")[-1])
        else:
            cc_binary_name = name + ".actual"
        cc_binary_visibility = ["//visibility:private"]
        cc_apply_transitive_features(
            name = name,
            actual = ":" + cc_binary_name,
            feature_names = transitive_features,
            output_name = shared_lib_name,
            tags = kwargs.get("tags"),
            target_compatible_with = kwargs.get("target_compatible_with"),
            testonly = kwargs.get("testonly"),
            visibility = kwargs.pop("visibility", None),
        )
    else:
        cc_binary_name = name
        cc_binary_visibility = kwargs.pop("visibility", None)

    wrapped_kwargs = wrap_cc_macro_args_with_build_flags(
        kwargs = kwargs | {"features": features, "linkshared": linkshared},
        name = cc_binary_name,
        cc_rule_name = "cc_binary",
        # LINT.IfChange(fdio_config)
        # Link libfdio.so into Fuchsia executables by default. This is a no-op
        # for host binaries (fdio_config has no deps there) and for shared
        # libraries (executable_link_deps are ignored for them). Targets can
        # opt out with disable_build_flags.
        build_flags = [
            Label("//build/config/fuchsia:fdio_config"),
            Label("//build/config/fuchsia:maybe_scudo_default_options"),
        ] + build_flags,
        # LINT.ThenChange(//build/bazel/config/BUILD.gn:fdio_config)
        disable_build_flags = disable_build_flags,

        # cc_binary() produces a shared library if linkshared=True, or
        # an executable if it is False (the default).
        target_type = "cxx_shared_library" if linkshared else "cxx_executable",
    )

    if linkshared:
        # Prepend default -Wl,-soname=<soname> before the build_flags response
        # file and linkopts so that custom soname flags in build_flags(ldflags = ...)
        # or linkopts appear later on the linker command line and override it.
        wrapped_kwargs["linkopts"] = [
            "-Wl,-soname={}".format(soname),
        ] + (wrapped_kwargs.get("linkopts") or [])

    cc_binary(
        name = cc_binary_name,
        visibility = cc_binary_visibility,
        **wrapped_kwargs
    )

fx_cc_binary = macro(
    doc = """Wrapper for cc_binary() binaries for Fuchsia.

    Toolchain overrides can be specified using build_flags and
    disable_build_flags. These will not affect dependencies.

    If both transitive_features and features are defined, transitive_features
    is applied first to change the build configuration used to build the native
    target, then features will be applied to the cc_binary() itself (overriding
    any transitive_features values), but not to its dependencies.
    """,
    implementation = _fx_cc_binary_impl,
    inherit_attrs = _cc_binary_for_inherit_attrs,
    attrs = {
        # Override the configurable `linkshared` inherited from cc_binary():
        # symbolic macros see configurable values as select() objects (which
        # are always truthy), but target_type must be known at loading time.
        "linkshared": attr.bool(
            doc = "Produce a shared library instead of an executable.",
            default = False,
            configurable = False,
        ),
        "public_configs": attr.string_list(
            doc = "Unused in Bazel, for GN conversion only.",
            default = [],
        ),
        "gn_output_name": attr.string(
            doc = """Unused in Bazel, for GN conversion only.

            Sets `output_name` on the GN target that bazel2gn generates, e.g.
            to avoid duplicate output files in GN's flat `$root_out_dir`. The
            Bazel binary is always named after the target; use
            `fx_packaged_binary(binary_name = ...)` to choose its name in a
            Fuchsia package.
            """,
            default = "",
            configurable = False,
        ),
        "disable_syslog_backend": attr.bool(
            doc = """Unused in Bazel, for GN conversion only.

            In GN, this asserts that the executable does not depend on the
            syslog backend. Bazel binaries do not get an implicit syslog
            backend dependency.
            """,
            default = False,
            configurable = False,
        ),
    } | CC_INSTRUMENTATION_ATTRS | BUILD_FLAGS_CC_MACRO_ATTRS | TRANSITIVE_FEATURES_ATTRS,
)
