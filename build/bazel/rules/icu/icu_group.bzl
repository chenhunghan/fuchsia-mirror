# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""icu_group() macro definition."""

load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageResourcesInfo")
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")
load(
    "//build/bazel/rules/icu:icu_names.bzl",
    "icu_fixup_deps",
    "icu_flavored_label",
    "icu_flavored_name",
)

visibility(["//..."])

def _icu_group_impl(ctx):
    files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps])
    resources = []
    cc_infos = []
    for d in ctx.attr.deps:
        if FuchsiaPackageResourcesInfo in d:
            resources.extend(d[FuchsiaPackageResourcesInfo].resources)
        if CcInfo in d:
            cc_infos.append(d[CcInfo])
    return [
        DefaultInfo(
            files = files,
            runfiles = ctx.runfiles(transitive_files = files),
        ),
        FuchsiaPackageResourcesInfo(resources = resources),
        cc_common.merge_cc_infos(cc_infos = cc_infos),
    ]

_icu_group = rule(
    implementation = _icu_group_impl,
    attrs = {
        "deps": attr.label_list(),
    },
)

def icu_group(
        *,
        name,
        deps = None,
        public_deps = None,
        icu_deps = None,
        icu_public_deps = None,
        testonly = None,
        visibility = None,
        **kwargs):
    """Defines an ICU flavor-aware target group.

    This is the Bazel counterpart of the GN `icu_group()` template defined in
    `//build/icu/icu_group.gni`.

    Args:
        name: Target name.
        deps: Non-ICU dependencies.
        public_deps: Public non-ICU dependencies.
        icu_deps: ICU-flavored dependencies.
        icu_public_deps: Public ICU-flavored dependencies.
        testonly: Standard Bazel `testonly` attribute.
        visibility: Standard Bazel `visibility` attribute.
        **kwargs: Additional keyword arguments forwarded to the underlying rule.
    """
    base_deps = (list(deps) if deps else []) + (list(public_deps) if public_deps else [])
    extra_icu_deps = (list(icu_deps) if icu_deps else []) + (list(icu_public_deps) if icu_public_deps else [])

    for icu_flavor in icu_flavors:
        flavored_icu_deps = [str(icu_flavored_label(d, icu_flavor)) for d in extra_icu_deps]
        _icu_group(
            name = icu_flavored_name(name, icu_flavor),
            deps = icu_fixup_deps(base_deps, icu_flavor) + flavored_icu_deps,
            testonly = testonly,
            visibility = visibility,
            **kwargs
        )

    _icu_group(
        name = name,
        deps = base_deps + extra_icu_deps,
        testonly = testonly,
        visibility = visibility,
        **kwargs
    )
