# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/keep_sorted.star.

These tests exec the real prebuilt keep-sorted binary.
"""

load("//scripts/shac/keep_sorted.star", "keep_sorted")

# Split up so that keep-sorted doesn't treat the test data in this file as
# directives.
_START = "keep-sorted " + "start"
_END = "keep-sorted " + "end"

def test_keep_sorted_unsorted():
    """The finding spans the unsorted lines, and its fix sorts them."""
    res = testing.run(keep_sorted, files = {
        "foo.txt": "# %s\nb\na\n# %s\n" % (_START, _END),
    })
    asserts.eq(len(res.findings), 1)
    f = res.findings[0]
    asserts.eq((f.filepath, f.level, f.line, f.end_line), ("foo.txt", "error", 2, 3))
    asserts.contains(f.message, "Run `fx format-code` to fix.")
    asserts.eq(res.files["foo.txt"], "# %s\na\nb\n# %s\n" % (_START, _END))

def test_keep_sorted_sorted():
    res = testing.run(keep_sorted, files = {
        "foo.txt": "# %s\na\nb\n# %s\n" % (_START, _END),
        "bar.txt": "b\na\n",
    })
    asserts.eq(res.findings, ())
