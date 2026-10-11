# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/gn.star."""

load("//scripts/shac/gn.star", "_gn_format", "gn_no_print")

_MSG = "Avoid print() in GN files. It pollutes stdout and breaks automated tools (like gndoc). Consider using temporary prints and removing them before landing."

def test_gn_no_print():
    res = testing.run(gn_no_print, files = {
        "BUILD.gn": "group(\"foo\") {\n  print(\"a\")\n  print(\"b\")\n}\n",
        "foo.gni": "x = 1\n  print (x)\n",
    })
    asserts.eq(res.findings, (
        # Only the first print() in each file is reported.
        testing.finding(level = "warning", message = _MSG, filepath = "BUILD.gn", line = 2, col = 3),
        testing.finding(level = "warning", message = _MSG, filepath = "foo.gni", line = 2, col = 3),
    ))

def test_gn_no_print_comments():
    res = testing.run(gn_no_print, files = {
        "BUILD.gn": "  # print(\"a\")\ngroup(\"foo\") {}\n",
    })
    asserts.eq(res.findings, ())

def test_gn_no_print_other_files():
    res = testing.run(gn_no_print, files = {
        "foo.py": "print(\"a\")\n",
    })
    asserts.eq(res.findings, ())

# The gn_format tests exec the real prebuilt gn binary.

def test_gn_format():
    res = testing.run(_gn_format, files = {
        "BUILD.gn": "group(\"foo\"){deps=[\"b\",\"a\"]}\n",
        "ok.gni": "x = 1\n",
        "foo.cc": "",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "File not formatted. Run `fx format-code` to fix.",
            filepath = "BUILD.gn",
            replacements = ["group(\"foo\") {\n  deps = [\n    \"a\",\n    \"b\",\n  ]\n}\n"],
        ),
    ))

def test_gn_format_syntax_error():
    asserts.fails(
        lambda: testing.run(_gn_format, files = {
            "bad.gn": "group(\"foo\" {\n",
            "BUILD.gn": "group(\"foo\"){}\n",
        }),
        "bad.gn:",
    )
