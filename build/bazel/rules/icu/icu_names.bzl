# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper functions for ICU flavor-aware Bazel targets and dependencies."""

load("@fuchsia_icu_config//:constants.bzl", "icu_commits")

visibility([
    "//bundles/assembly/...",
    "//build/bazel/rules/icu/...",
])

_ICU_FIXUP_NAMES = {
    "data": True,
    "icu": True,
    "icudtl": True,
    "lib": True,
}

def icu_flavored_name(name, icu_flavor):
    """Given a name, create an new name with a suffix containing the flavor and commit."""
    return "%s.icu_%s_%s" % (name, icu_flavor, icu_commits[icu_flavor])

# buildifier: disable=unnamed-macro - Helper function returning transformed label.
def icu_flavored_label(label, icu_flavor):
    """Given a label, create an new label with a suffix containing the flavor and commit."""
    label = native.package_relative_label(label)
    name = label.name

    return label.same_package_label(icu_flavored_name(name, icu_flavor))

# buildifier: disable=unnamed-macro - Helper function returning transformed dependency list.
def icu_fixup_deps(deps, icu_flavor, add_icu_lib = False):
    """Fixes up dependencies for an ICU-flavored target.

    Drops labels resolving to `//src/lib/icu:{icu,lib,data,icudtl}` so
    unflavored ICU targets do not leak into flavored targets. When `add_icu_lib`
    is True, appends `//src/lib/icu:icu_<icu_flavor>`.

    Args:
        deps: List of dependency label strings, or None.
        icu_flavor: Configured ICU flavor name (e.g. `"default"` or `"latest"`).
        add_icu_lib: Whether to append `//src/lib/icu:icu_<icu_flavor>` (valid
            for C++ rules that consume `CcInfo` targets).

    Returns:
        A new list of fixed-up dependency label strings.
    """
    result = []
    for d in deps or []:
        label = native.package_relative_label(d)
        if label.package == "src/lib/icu" and label.name in _ICU_FIXUP_NAMES:
            continue
        result.append(d)
    if add_icu_lib:
        flavored_icu = "//src/lib/icu:icu_%s" % icu_flavor
        if flavored_icu not in result:
            result.append(flavored_icu)
    return result
