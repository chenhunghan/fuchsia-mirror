# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Definition of the feature_flag() rule."""

load(
    "@fuchsia_rules_common//build_flags:feature_flag.bzl",
    _feature_flag = "feature_flag",
)

feature_flag = _feature_flag
