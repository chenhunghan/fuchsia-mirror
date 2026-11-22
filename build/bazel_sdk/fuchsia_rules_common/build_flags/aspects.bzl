# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Aspects for inspecting the build_flags() applied to a target."""

load(":providers.bzl", "BuildFlagsListInfo")

BuildFlagsLabelsInfo = provider(
    doc = "The ordered list of build_flags() labels applied to a target.",
    fields = {
        "labels": """(list[Label]) Labels of the final BuildFlagsInfo values
            applied to the target, in order, after deduplication and removal
            of `disable_build_flags`.""",
    },
)

def _collect_build_flags_labels_aspect_impl(target, ctx):
    # Targets wrapped with build_flags() support (see
    # wrap_cc_macro_args_with_build_flags() in cc.bzl) attach their
    # `<name>.final_build_flags` target in `deps` or `additional_linker_inputs`.
    # `deps` is checked before `additional_linker_inputs` because cc_binary
    # places `:<name>.final_build_flags` in both, whereas cc_shared_library
    # cannot take it in `deps` and only attaches it to `additional_linker_inputs`
    # (see "Supporting `cc_shared_library()` targets" in build_flags/README.md).
    for attr_name in ("deps", "additional_linker_inputs"):
        attr_val = getattr(ctx.rule.attr, attr_name, None)
        if not attr_val:
            continue
        lists = [dep[BuildFlagsListInfo] for dep in attr_val if BuildFlagsListInfo in dep]
        if lists:
            if len(lists) != 1:
                fail("Expected exactly 1 {} target providing BuildFlagsListInfo on {}, got {}".format(
                    attr_name,
                    target.label,
                    len(lists),
                ))
            return [
                BuildFlagsLabelsInfo(
                    labels = [info.label for info in lists[0].infos],
                ),
            ]
    fail("Expected a dep or additional_linker_inputs target providing BuildFlagsListInfo on {}".format(target.label))

collect_build_flags_labels_aspect = aspect(
    doc = """Collects the final build_flags() labels applied to a target.

    Provides BuildFlagsLabelsInfo from the `deps` or
    `additional_linker_inputs` target that provides BuildFlagsListInfo.
    """,
    implementation = _collect_build_flags_labels_aspect_impl,
    provides = [BuildFlagsLabelsInfo],
)
