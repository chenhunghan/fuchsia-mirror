# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Bazel stub for GN's expect_includes() template."""

load("@rules_cc//cc:defs.bzl", "cc_library")

def expect_includes(name, includes, **kwargs):
    """A Bazel stub for GN's expect_includes() template.

    In GN, expect_includes() requires every component that transitively depends
    on this target to have `includes` in its component manifest, and fails the
    build otherwise. See //tools/cmc/build/expect_includes.gni.

    TODO(https://fxbug.dev/563978780): Implement real enforcement in Bazel and
    replace this stub.

    Args:
        name: The target name.
        includes: Unused by Bazel; exists so that bazel2gn can emit the real
            GN expect_includes() target.
        **kwargs: Forwarded to the underlying target, e.g. visibility.
    """

    _ = includes  # @unused

    cc_library(
        name = name,
        **kwargs
    )
