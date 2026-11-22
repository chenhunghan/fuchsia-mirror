# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for grouping a Vulkan layer shared object and its JSON config resource."""

load(
    "@fuchsia_rules_common//debug_symbols:debug_symbols.bzl",
    "merge_debug_symbol_infos",
)
load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    "FuchsiaUnstrippedBinariesInfo",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    "FuchsiaPackageResourcesInfo",
)
load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")

visibility(["//src/lib/vulkan/..."])

def _vulkan_layer_group_impl(ctx):
    resources = []
    unstripped_binaries = []
    for dep in ctx.attr.deps:
        if FuchsiaPackageResourcesInfo in dep:
            resources.extend(dep[FuchsiaPackageResourcesInfo].resources)
        if FuchsiaUnstrippedBinariesInfo in dep:
            unstripped_binaries.extend(dep[FuchsiaUnstrippedBinariesInfo].binaries)

    return [
        DefaultInfo(),
        CcInfo(),
        FuchsiaPackageResourcesInfo(resources = resources),
        FuchsiaUnstrippedBinariesInfo(binaries = unstripped_binaries),
        merge_debug_symbol_infos(ctx.attr.deps),
    ]

vulkan_layer_group = rule(
    doc = """Groups a Vulkan layer module and its JSON resource so the target can be used in C++ deps, fx_component deps, or fx_package resources.""",
    implementation = _vulkan_layer_group_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "Packaged binary and resource targets comprising the Vulkan layer.",
            default = [],
        ),
    },
)
