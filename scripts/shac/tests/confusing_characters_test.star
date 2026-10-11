# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/confusing_characters.star."""

load("//scripts/shac/confusing_characters.star", "confusing_characters")

# Escaped so that the confusing_characters check doesn't flag this file.
_LEFT_QUOTE = "\u201c"
_RIGHT_QUOTE = "\u201d"
_EN_DASH = "\u2013"
_EM_DASH = "\u2014"

def _finding(char, line, col, replacement):
    return testing.finding(
        level = "warning",
        message = "Avoid using confusing characters: %s" % char,
        filepath = "foo.md",
        line = line,
        col = col,
        end_col = col + len(char),
        replacements = [replacement],
    )

def test_confusing_characters_smart_quotes():
    res = testing.run(confusing_characters, files = {
        "foo.md": "ok\nsay %shi%s\n" % (_LEFT_QUOTE, _RIGHT_QUOTE),
    })
    asserts.eq(res.findings, (
        _finding(_LEFT_QUOTE, 2, 5, "\""),
        _finding(_RIGHT_QUOTE, 2, 10, "\""),
    ))
    asserts.eq(res.files["foo.md"], "ok\nsay \"hi\"\n")

def test_confusing_characters_dashes():
    res = testing.run(confusing_characters, files = {
        "foo.md": "a%sb\nc%sd\n" % (_EN_DASH, _EM_DASH),
    })
    asserts.eq(res.findings, (
        _finding(_EN_DASH, 1, 2, "-"),
        _finding(_EM_DASH, 2, 2, "-"),
    ))
    asserts.eq(res.files["foo.md"], "a-b\nc-d\n")

def test_confusing_characters_ascii():
    res = testing.run(confusing_characters, files = {
        "foo.md": "say \"hi\" - bye\n",
    })
    asserts.eq(res.findings, ())
