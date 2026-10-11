# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/bazel_migration.star."""

load(
    "//scripts/shac/bazel_migration.star",
    _check = "_enforce_bazel_build_file_for_new_fidl_library",
)

_FIDL_BUILD_GN = """\
import("//build/fidl/fidl.gni")

fidl("fuchsia.foo") {
  sources = [ "foo.fidl" ]
}
"""

def test_new_fidl_library_without_build_bazel():
    res = testing.run(_check, files = {
        "sdk/fidl/fuchsia.foo/BUILD.gn": testing.file(content = _FIDL_BUILD_GN, action = "A"),
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "BUILD.bazel file is missing in the same directory as the added FIDL BUILD.gn file.",
            filepath = "sdk/fidl/fuchsia.foo/BUILD.gn",
        ),
    ))

def test_new_fidl_library_with_build_bazel():
    res = testing.run(_check, files = {
        "sdk/fidl/fuchsia.foo/BUILD.gn": testing.file(content = _FIDL_BUILD_GN, action = "A"),
        "sdk/fidl/fuchsia.foo/BUILD.bazel": testing.file(affected = False),
    })
    asserts.eq(res.findings, ())

def test_modified_fidl_library():
    res = testing.run(_check, files = {
        "sdk/fidl/fuchsia.foo/BUILD.gn": testing.file(content = _FIDL_BUILD_GN, action = "M"),
    })
    asserts.eq(res.findings, ())

def test_new_fidl_library_outside_sdk_fidl():
    res = testing.run(_check, files = {
        "src/foo/BUILD.gn": testing.file(content = _FIDL_BUILD_GN, action = "A"),
    })
    asserts.eq(res.findings, ())

def test_new_build_gn_without_fidl():
    res = testing.run(_check, files = {
        "sdk/fidl/fuchsia.foo/BUILD.gn": testing.file(
            content = "# fidl(\"fuchsia.foo\")\ngroup(\"foo\") {}\n",
            action = "A",
        ),
    })
    asserts.eq(res.findings, ())
