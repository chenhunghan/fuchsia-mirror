# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared default compiler flag constants for both in-tree build_flags() and OOT cc_features.bzl."""

# LINT.IfChange(default_warnings)
DEFAULT_WARNINGS_DEFINES = [
    # TODO(https://fxbug.dev/542707525): Suppress unused variable warnings in libc++ containers.
    "_LIBCPP_DISABLE_UNUSED_STRUCT_WARNINGS",
]

# Shared warning flags preceding -Wno-sign-conversion.
# Note: -Wconversion MUST precede -Wno-sign-conversion because -Wconversion is
# a diagnostic group in Clang that enables -Wsign-conversion.
_BASE_WARNINGS_CFLAGS_PREFIX = [
    "-Wall",
    "-Wextra",
]

_BASE_WARNINGS_CFLAGS_SUFFIX = [
    "-Wnewline-eof",
    # TODO(b/315062126) Some in-tree builds are failing because we
    # are shadowing variables.
    #"-Wshadow",
    "-Wstrict-prototypes",
    "-Wwrite-strings",
    "-Wno-sign-conversion",
    "-Wno-unused-parameter",
    "-Wnonportable-system-include-path",

    # TODO(https://fxbug.dev/42142861): Disable "-Wmissing-field-initializers"
    # which warns when designated initializers are used on some (but not all)
    # fields in C++, but not C.
    "-Wno-missing-field-initializers",

    # This is still needed for -Wmaybe-uninitialized which is added as a pragma in source code.
    # Clang hasn't implemented this yet.
    "-Wno-unknown-warning-option",
]

# Used by OOT cc_features.bzl until OOT migrates to build_flags().
# TODO(https://fxbug.dev/477786942): Fix violations in OOT and enable
# -Wconversion and -Wimplicit-fallthrough (must precede -Wno-sign-conversion!).
DEFAULT_WARNINGS_CFLAGS = (
    _BASE_WARNINGS_CFLAGS_PREFIX +
    ["-Wextra-semi"] +
    _BASE_WARNINGS_CFLAGS_SUFFIX + [
        # TODO(https://fxbug.dev/534361287): Clean up unused templates in first-party code.
        # This is currently applied globally to unblock the toolchain update.
        # Eventually, this should be removed from default_warnings and only applied
        # to third_party targets. (In-tree adds this via subflags = [":Wno-unused-template"].)
        "-Wno-unused-template",
    ]
)

# Used by in-tree //build/config:default_warnings to match //build/config:default_warnings in GN.
IN_TREE_DEFAULT_WARNINGS_CFLAGS = (
    _BASE_WARNINGS_CFLAGS_PREFIX + [
        "-Wconversion",
        "-Wextra-semi",
        "-Wimplicit-fallthrough",
    ] + _BASE_WARNINGS_CFLAGS_SUFFIX
)
# LINT.ThenChange(
#     //build/config/BUILD.bazel:default_warnings,
#     //build/config/BUILD.gn:default_warnings,
# )

# LINT.IfChange(thread_safety_annotations)
THREAD_SAFETY_DEFINES = [
    "_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS=1",
]

THREAD_SAFETY_CFLAGS = [
    "-Wthread-safety",

    # TODO(https://fxbug.dev/42085252): Clang is catching instances of these in the kernel and drivers.
    # Temporarily disable them for now to facilitate the roll then come back and
    # fix them.
    "-Wno-unknown-warning-option",
    "-Wno-thread-safety-reference-return",
]
# LINT.ThenChange(
#     //build/config/BUILD.bazel:thread_safety_annotations,
#     //build/config/BUILD.gn:thread_safety_annotations,
# )

# LINT.IfChange(cpu_baseline)
# C/C++ flags of the Fuchsia x86-64 platform baseline (cflags only, as in GN).
X64_BASELINE_CFLAGS = [
    "-march=x86-64-v2",
    "-mtune=generic",
]

# C/C++ flags that mitigate the Skylake Jump Conditional Code erratum, part of
# the Fuchsia x86-64 platform baseline (cflags only, as in GN).
X64_MITIGATE_SKYLAKE_JCC_CFLAGS = [
    "-mbranches-within-32B-boundaries",
]

# C/C++ flags of the Fuchsia AArch64 platform baseline, used for both
# compiling and linking (c_family_flags in GN).
ARM64_BASELINE_C_FAMILY_FLAGS = [
    "-mtune=generic",
    "-march=armv8-a+simd+crc+crypto",
]
# LINT.ThenChange(
#     //build/config/arm64/BUILD.bazel:arm64_configs,
#     //build/config/arm64/BUILD.gn:arm64_configs,
#     //build/config/x64/BUILD.bazel:x64_configs,
#     //build/config/x64/BUILD.gn:x64_configs,
# )
