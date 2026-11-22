# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""icu_executable() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load(
    "//build/bazel/rules/icu:icu_names.bzl",
    "icu_fixup_deps",
    "icu_flavored_label",
    "icu_flavored_name",
)

visibility(["//..."])

def icu_executable(
        *,
        name,
        deps = None,
        icu_deps = None,
        icu_public_deps = None,
        output_name = None,  # buildifier: disable=unused-variable - For GN conversion only.
        gn_output_name = "",  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    """Defines an ICU flavor-aware C++ binary (`fx_cc_binary`).

    This is the Bazel counterpart of the GN `icu_executable()` template defined in
    `//build/icu/icu_executable.gni`. It generates an unflavored target `name` and
    an ICU-flavored target `<name>.icu_<flavor>_<commit>` for each configured ICU flavor.

    Args:
        name: Target name.
        deps: Non-ICU dependencies.
        icu_deps: ICU-flavored dependencies.
        icu_public_deps: Public ICU-flavored dependencies.
        output_name: Optional GN output binary name (forwarded to GN by `bazel2gn`, unused in Bazel).
        gn_output_name: Optional GN output binary name (mapped to `output_name` in GN by `bazel2gn`).
        **kwargs: Additional keyword arguments forwarded to `fx_cc_binary`.
    """
    base_deps = list(deps) if deps else []
    extra_icu_deps = (list(icu_deps) if icu_deps else []) + (list(icu_public_deps) if icu_public_deps else [])

    for icu_flavor in icu_flavors:
        flavored_icu_deps = [str(icu_flavored_label(d, icu_flavor)) for d in extra_icu_deps]
        fx_cc_binary(
            name = icu_flavored_name(name, icu_flavor),
            deps = icu_fixup_deps(
                base_deps,
                icu_flavor,
                add_icu_lib = bool(extra_icu_deps),
            ) + flavored_icu_deps,
            **kwargs
        )

    fx_cc_binary(
        name = name,
        deps = base_deps + extra_icu_deps,
        **kwargs
    )
