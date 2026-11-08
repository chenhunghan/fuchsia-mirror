# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Cycle-free feature_flag() rule for C++ and Rust toolchain features."""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

visibility("public")

# Supported toolchain features that can be queried via feature_flag().
# Because feature_flag() does not depend on cc_toolchain (to avoid dependency
# cycles when used in default_build_flags), it does not evaluate C++ toolchain
# `feature(implies = ...)` relationships and only supports explicitly
# toggleable variant/instrumentation features.
_SUPPORTED_FEATURES = [
    "asan",
    "coverage",
    "coverage-rust",
    "hwasan",
    "lsan",
    "msan",
    "profile",
    "profile-rust",
    "sancov",
    "tsan",
    "ubsan",
]

def _feature_flag_impl(ctx):
    """Evaluate whether a toolchain feature is enabled in the current configuration.

    Args:
        ctx: Rule context.

    Returns:
        List of providers (FeatureFlagInfo and BuildSettingInfo).
    """
    feature_name = ctx.attr.feature_name
    enabled_features = ctx.attr._enabled_features[BuildSettingInfo].value
    disabled_features = ctx.disabled_features
    features = ctx.features

    is_enabled = (feature_name not in disabled_features) and (
        feature_name in enabled_features or feature_name in features
    )

    return [
        config_common.FeatureFlagInfo(value = str(is_enabled)),
        BuildSettingInfo(value = is_enabled),
    ]

feature_flag = rule(
    implementation = _feature_flag_impl,
    doc = "A cycle-free flag whose value ('True' or 'False') reflects whether a toolchain feature is enabled via `--features`, `transitive_features`, or `@fuchsia_rules_common//build_flags:enabled_features`.",
    attrs = {
        "feature_name": attr.string(
            mandatory = True,
            values = _SUPPORTED_FEATURES,
        ),
        "_enabled_features": attr.label(
            default = "@fuchsia_rules_common//build_flags:enabled_features",
            providers = [BuildSettingInfo],
        ),
    },
)
