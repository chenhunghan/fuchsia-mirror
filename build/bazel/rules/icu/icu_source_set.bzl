# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""icu_source_set() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load(
    "//build/bazel/rules/icu:icu_names.bzl",
    "icu_fixup_deps",
    "icu_flavored_label",
    "icu_flavored_name",
)

visibility(["//..."])

def icu_source_set(
        *,
        name,
        srcs = None,
        hdrs = None,
        deps = None,
        implementation_deps = None,
        icu_deps = None,
        icu_public_deps = None,
        alwayslink = True,
        **kwargs):
    """Defines an ICU flavor-aware C++ source set (`fx_cc_library` with `alwayslink = True`).

    This is the Bazel counterpart of the GN `icu_source_set()` template defined in
    `//build/icu/icu_source_set.gni`. It generates an unflavored target `name` and
    an ICU-flavored target `<name>.icu_<flavor>_<commit>` for each configured ICU flavor.

    Args:
        name: Target name.
        srcs: Source files (`.cc` and `.h`; `.h` files in `srcs` are automatically exported in
            `hdrs` for Bazel `cc_library` compatibility).
        hdrs: Public header files.
        deps: Public non-ICU dependencies (mapped to `public_deps` in GN).
        implementation_deps: Private non-ICU dependencies (mapped to `deps` in GN).
        icu_deps: Private ICU-flavored dependencies.
        icu_public_deps: Public ICU-flavored dependencies.
        alwayslink: Passed to `fx_cc_library` (defaults to `True`).
        **kwargs: Additional keyword arguments forwarded to `fx_cc_library`.
    """
    cc_srcs = srcs
    cc_hdrs = hdrs
    if type(srcs) == type([]):
        extracted_hdrs = [s for s in srcs if type(s) == type("") and s.endswith(".h")]
        if extracted_hdrs:
            cc_srcs = [s for s in srcs if not (type(s) == type("") and s.endswith(".h"))]
            if cc_hdrs == None:
                cc_hdrs = extracted_hdrs
            elif type(cc_hdrs) == type([]):
                cc_hdrs = cc_hdrs + [h for h in extracted_hdrs if h not in cc_hdrs]

    base_deps = list(deps) if deps else []
    base_impl_deps = list(implementation_deps) if implementation_deps else []
    extra_icu_deps = list(icu_deps) if icu_deps else []
    extra_icu_public_deps = list(icu_public_deps) if icu_public_deps else []

    for icu_flavor in icu_flavors:
        flavored_icu_public_deps = [str(icu_flavored_label(d, icu_flavor)) for d in extra_icu_public_deps]
        flavored_icu_deps = [str(icu_flavored_label(d, icu_flavor)) for d in extra_icu_deps]
        fx_cc_library(
            name = icu_flavored_name(name, icu_flavor),
            srcs = cc_srcs,
            hdrs = cc_hdrs,
            deps = icu_fixup_deps(
                base_deps,
                icu_flavor,
                add_icu_lib = bool(extra_icu_public_deps),
            ) + flavored_icu_public_deps,
            implementation_deps = icu_fixup_deps(
                base_impl_deps,
                icu_flavor,
                add_icu_lib = bool(extra_icu_deps),
            ) + flavored_icu_deps,
            alwayslink = alwayslink,
            **kwargs
        )

    fx_cc_library(
        name = name,
        srcs = cc_srcs,
        hdrs = cc_hdrs,
        deps = base_deps + extra_icu_public_deps,
        implementation_deps = base_impl_deps + extra_icu_deps,
        alwayslink = alwayslink,
        **kwargs
    )
