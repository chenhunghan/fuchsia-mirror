# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common utilities for transition functions."""

load(
    "@fuchsia_rules_common//:transition_utils.bzl",
    _FEATURES_LIST_TRANSITION_ATTRS = "FEATURES_LIST_TRANSITION_ATTRS",
    _TRANSITIVE_FEATURES_ATTRS = "TRANSITIVE_FEATURES_ATTRS",
    _apply_features_list = "apply_features_list",
    _features_list_transition = "features_list_transition",
    _normalize_features_list = "normalize_features_list",
    _remove_command_line_option_values = "remove_command_line_option_values",
    _set_command_line_option_value = "set_command_line_option_value",
)

set_command_line_option_value = _set_command_line_option_value
remove_command_line_option_values = _remove_command_line_option_values
normalize_features_list = _normalize_features_list
FEATURES_LIST_TRANSITION_ATTRS = _FEATURES_LIST_TRANSITION_ATTRS
TRANSITIVE_FEATURES_ATTRS = _TRANSITIVE_FEATURES_ATTRS
features_list_transition = _features_list_transition
apply_features_list = _apply_features_list
