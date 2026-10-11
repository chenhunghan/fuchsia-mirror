# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/underscore_vs_dash.star."""

load("//scripts/shac/underscore_vs_dash.star", "underscore_vs_dash")

def _existing_files(dirpath, names):
    return {
        "%s/%s" % (dirpath, name): testing.file(affected = False)
        for name in names
    }

def test_dash_among_underscores():
    files = _existing_files("src/foo", ["a_%d.cc" % i for i in range(10)])
    files["src/foo/new-file.cc"] = testing.file(action = "A")
    res = testing.run(underscore_vs_dash, files = files)
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = "\n".join([
                r"filename contains a '-' character. Similar files tend to use '\_'. Consider using '\_' instead.",
                "",
                "Of other '.cc' files under 'src/foo':",
                r"* 10 use '\_'",
                "* 0 use '-'",
                "",
            ]),
            filepath = "src/foo/new-file.cc",
        ),
    ))

def test_underscore_among_dashes():
    files = _existing_files("src/foo", ["a-%d" % i for i in range(10)])
    files["src/foo/new_file"] = testing.file(action = "A")
    res = testing.run(underscore_vs_dash, files = files)
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = "\n".join([
                r"filename contains a '\_' character. Similar files tend to use '-'. Consider using '-' instead.",
                "",
                "Of other extensionless files under 'src/foo':",
                r"* 0 use '\_'",
                "* 10 use '-'",
                "",
            ]),
            filepath = "src/foo/new_file",
        ),
    ))

def test_underscore_preferred_when_close():
    # "_" is only discouraged when "-" substantially outnumbers it.
    files = _existing_files("src/foo", ["a-%d.cc" % i for i in range(6)] + ["b_%d.cc" % i for i in range(4)])
    files["src/foo/new_file.cc"] = testing.file(action = "A")
    res = testing.run(underscore_vs_dash, files = files)
    asserts.eq(res.findings, ())

def test_falls_back_to_parent_dir():
    """A new file in a new directory is compared with its parent's files."""
    files = _existing_files("src", ["a_%d.cc" % i for i in range(10)])
    files["src/foo/new-file.cc"] = testing.file(action = "A")
    res = testing.run(underscore_vs_dash, files = files)
    asserts.eq([f.filepath for f in res.findings], ["src/foo/new-file.cc"])
    asserts.contains(res.findings[0].message, "files under 'src':")

def test_only_added_files():
    files = _existing_files("src/foo", ["a_%d.cc" % i for i in range(10)])
    files["src/foo/new-file.cc"] = testing.file(action = "M")
    res = testing.run(underscore_vs_dash, files = files)
    asserts.eq(res.findings, ())
