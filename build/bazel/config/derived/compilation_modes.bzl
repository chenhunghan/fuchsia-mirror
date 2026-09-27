# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_build_info//:args.bzl", "compilation_mode")

# LINT.IfChange(compilation_mode_predicates)
is_debug = compilation_mode == "debug"
is_balanced = compilation_mode == "balanced"
is_release = compilation_mode == "release"
is_sanitizer = compilation_mode == "sanitizer"
# LINT.ThenChange(//build/config/compilation_modes.gni:compilation_mode_predicates)
