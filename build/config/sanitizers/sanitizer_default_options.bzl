# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel macros for sanitizer default and extra runtime options."""

load(
    "@fuchsia_build_info//:args.bzl",
    "asan_default_options",
    "hwasan_default_options",
    "lsan_default_options",
    "scudo_default_options",
    "tsan_default_options",
    "ubsan_default_options",
)
load("@rules_cc//cc:defs.bzl", "cc_library")

visibility([
    "//build/config/fuchsia/...",
    "//build/config/sanitizers/...",
])

SANITIZER_VALUES = [
    "asan",
    "hwasan",
    "lsan",
    "scudo",
    "tsan",
    "ubsan",
]
_SANITIZER_VALUES = SANITIZER_VALUES

_SANITIZER_NAMES_WITHOUT_SCUDO = [
    "asan",
    "lsan",
    "ubsan",
    "tsan",
    "hwasan",
]

_DEFAULT_OPTIONS_BY_NAME = {
    "asan_default_options": ("asan", asan_default_options),
    "hwasan_default_options": ("hwasan", hwasan_default_options),
    "lsan_default_options": ("lsan", lsan_default_options),
    "scudo_default_options": ("scudo", scudo_default_options),
    "tsan_default_options": ("tsan", tsan_default_options),
    "ubsan_default_options": ("ubsan", ubsan_default_options),
}

# NOTE: This assumes is_kernel=false && zircon_toolchain=false in Bazel.
# LINT.IfChange(common_copts)
_COMMON_COPTS = [
    # On Fuchsia, the ASan runtime is dynamically linked and needs to have
    # the __asan_default_options symbol exported.
    "-fvisibility=default",
    # On non-Fuchsia systems, the flag parsing calls this function so early
    # in startup that instrumented code cannot reliably run at all yet.
    # Matches //build/config/sanitizers:no_sanitizers.
    "-fno-sanitize=all",
    "-fsanitize-coverage=0",
    "-fno-stack-protector",
    # Necessary to link into Rust binaries, which are PIE even on platforms
    # where C binaries are not PIE by default.
    "-fPIE",
]
# LINT.ThenChange(//build/config/sanitizers/sanitizer_default_options.gni:common_copts)

def _sanitizer_default_options_impl(name, visibility, deps, features = None, **kwargs):
    if name not in _DEFAULT_OPTIONS_BY_NAME:
        fail("sanitizer_default_options() target name must be one of %s, got %s" % (
            sorted(_DEFAULT_OPTIONS_BY_NAME.keys()),
            name,
        ))
    _sanitizer, args = _DEFAULT_OPTIONS_BY_NAME[name]
    options_string = ":".join(args)

    cc_library(
        name = name,
        srcs = ["//build/config/sanitizers:sanitizer_default_options.c"],
        copts = _COMMON_COPTS,
        features = (features or []) + ["-clang_sanitizer", "-instrumented"],
        local_defines = [
            "DEFINE_SANITIZER_DEFAULT_OPTIONS=1",
            "SANITIZER_DEFAULT_OPTIONS_NAME=%s" % name,
            'SANITIZER_DEFAULT_OPTIONS_STRING=\\"%s\\"' % options_string,
        ],
        deps = deps,
        visibility = visibility,
        alwayslink = True,
        **kwargs
    )

sanitizer_default_options = macro(
    doc = """Defines the `<sanitizer>_default_options` cc_library target for a sanitizer.

    The target name must be `<sanitizer>_default_options`, matching the GN build
    argument in `@fuchsia_build_info//:args.bzl`.
    """,
    implementation = _sanitizer_default_options_impl,
    inherit_attrs = "common",
    attrs = {
        "deps": attr.label_list(
            default = [],
            doc = "Other sanitizer_default_options() targets subsumed by this runtime.",
        ),
    },
)

def _sanitizer_extra_options_impl(name, visibility, sanitizer, args, deps = [], variant_tags = [], features = None, **kwargs):
    for a in args:
        if ":" in a:
            fail("sanitizer_extra_options() args elements cannot contain ':', got: %s" % a)
        if "=" not in a:
            fail("sanitizer_extra_options() args elements must have 'key=value' form, got: %s" % a)

    options_string = ":".join(args)
    output_name = "%s_default_options" % sanitizer
    default_options_target = (
        Label("//build/config/fuchsia:scudo_default_options") if sanitizer == "scudo" else Label("//build/config/sanitizers:%s" % output_name)
    )

    if variant_tags:
        srcs_select = {
            Label("//build/bazel/config/toolchain_tags/%s:true" % tag): [
                Label("//build/config/sanitizers:sanitizer_default_options.c"),
            ]
            for tag in variant_tags
        }
        srcs_select["//conditions:default"] = []
        srcs = select(srcs_select)

        deps_select = {
            Label("//build/bazel/config/toolchain_tags/%s:true" % tag): [
                default_options_target,
            ]
            for tag in variant_tags
        }
        deps_select["//conditions:default"] = []
        deps = deps + select(deps_select)
    else:
        srcs = [Label("//build/config/sanitizers:sanitizer_default_options.c")]
        deps = deps + [default_options_target]

    cc_library(
        name = name,
        srcs = srcs,
        copts = _COMMON_COPTS,
        features = (features or []) + ["-clang_sanitizer", "-instrumented"],
        local_defines = [
            "DEFINE_SANITIZER_DEFAULT_OPTIONS=0",
            "SANITIZER_DEFAULT_OPTIONS_NAME=%s" % output_name,
            'SANITIZER_DEFAULT_OPTIONS_STRING=\\"%s\\"' % options_string,
        ],
        deps = deps,
        visibility = visibility,
        alwayslink = True,
        **kwargs
    )

sanitizer_extra_options = macro(
    doc = """Provides additional default runtime options for a sanitizer.

    Produces a `cc_library` with `alwayslink = True` that places the extra
    options string into the `san_def_options` ELF section read by
    `__<sanitizer>_default_options()` at startup.
    """,
    implementation = _sanitizer_extra_options_impl,
    inherit_attrs = "common",
    attrs = {
        "sanitizer": attr.string(
            mandatory = True,
            values = _SANITIZER_VALUES,
            configurable = False,
            doc = "Name of the sanitizer to affect or 'scudo'.",
        ),
        "args": attr.string_list(
            mandatory = True,
            allow_empty = False,
            configurable = False,
            doc = "List of 'option=value' strings for the sanitizer runtime.",
        ),
        "deps": attr.label_list(
            default = [],
            doc = "Additional dependencies.",
        ),
        "variant_tags": attr.string_list(
            default = [],
            configurable = False,
            doc = """Optional list of toolchain variant tags (e.g. ['asan']).

            When non-empty, this target only compiles and links the options in
            builds where at least one of the listed toolchain tags is active,
            acting as an empty no-op cc_library otherwise. When empty (the
            default), the options are unconditionally compiled and linked.
            """,
        ),
    },
)

def _extra_options_for_all_sanitizers_impl(name, visibility, args, testonly = False, **kwargs):
    helper_targets = []
    for san in _SANITIZER_NAMES_WITHOUT_SCUDO:
        helper_name = "%s.%s" % (name, san)
        sanitizer_extra_options(
            name = helper_name,
            sanitizer = san,
            args = args,
            variant_tags = [san],
            visibility = ["//visibility:private"],
            testonly = testonly,
        )
        helper_targets.append(":%s" % helper_name)

    cc_library(
        name = name,
        deps = helper_targets,
        visibility = visibility,
        testonly = testonly,
        **kwargs
    )

extra_options_for_all_sanitizers = macro(
    doc = "Provides additional default options for all sanitizer runtimes.",
    implementation = _extra_options_for_all_sanitizers_impl,
    inherit_attrs = "common",
    attrs = {
        "args": attr.string_list(
            mandatory = True,
            allow_empty = False,
            configurable = False,
            doc = "List of 'option=value' strings for the sanitizer runtimes.",
        ),
    },
)
