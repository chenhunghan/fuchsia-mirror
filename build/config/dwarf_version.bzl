# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A custom rule to compute the build_flags() values for //build/config:dwarf_version.

A custom rule can easily extract the toolchain-scoped integer value at
analysis time and create a BuildFlagsInfo provider with the right value
for it.

The alternative is to create a series of build_flags() targets, each one
for a specific value with a wrapper target select()-ing through them as
in:

build_flags(
    name = "dwarf_version_4",
    cflags = [ "-gdwarf-4" ],
    ldflags = [ "-gdwarf-4" ],
)

build_flags(
    name = "dwarf_version_5",
    cflags = [ "-gdwarf-5" ],
    ldflags = [ "-gdwarf-5" ],
)

build_flags(
    name = "dwarf_version",
    subflags = select({
        "//build/bazel/config/select/dwarf_version:v4": [":dwarf_version_4"],
        "//build/bazel/config/select/dwarf_version:v5": [":dwarf_version_5"],
        "//conditions:default": [],
    }),
)

And the example above assumes that //build/bazel/config/select/dwarf_version/BUILD.bazel
contains config_setting() predicates to check against a specific hard-coded value
like the "v4" and "v5" target names above.

This load-time-only scheme is however far less flexible as it must support any
possible integer value in advance (so supporting a new value like 6 requires changing
build files).

The custom rule here avoids all that.
"""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("@fuchsia_rules_common//build_flags:providers.bzl", "BuildFlagsInfo")

def _dwarf_version_build_flags_impl(ctx):
    # LINT.IfChange(dwarf_version)
    cflags = ["-gdwarf-{}".format(ctx.attr._flag[BuildSettingInfo].value)]

    # NOTE: asmflags is not supported. Bazel uses cflags.
    # asmflags = cflags
    ldflags = cflags

    # LINT.ThenChange(BUILD.gn:dwarf_version)
    return [
        BuildFlagsInfo(
            label = ctx.label,
            cflags = cflags,
            ldflags = ldflags,
        ),
    ]

dwarf_version_build_flags = rule(
    doc = "Create a build_flags() target from the dwarf_version toolchain-specific variable.",
    implementation = _dwarf_version_build_flags_impl,
    attrs = {
        "_flag": attr.label(
            doc = "Label to the int_flag() target exposing the dwarf_version value.",
            default = "//build/bazel/config/variables:dwarf_version",
            providers = [BuildSettingInfo],
        ),
    },
)
