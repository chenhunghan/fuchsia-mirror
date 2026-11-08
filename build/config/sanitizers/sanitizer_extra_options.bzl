# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Re-exports sanitizer_extra_options() and SANITIZER_VALUES."""

load(
    "//build/config/sanitizers:sanitizer_default_options.bzl",
    _SANITIZER_VALUES_IMPL = "SANITIZER_VALUES",
    _sanitizer_extra_options = "sanitizer_extra_options",
)

visibility([
    "//build/config/...",
    "//sdk/...",
    "//src/...",
    "//tools/...",
])

sanitizer_extra_options = _sanitizer_extra_options
SANITIZER_VALUES = _SANITIZER_VALUES_IMPL
