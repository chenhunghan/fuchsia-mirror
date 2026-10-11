# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/docs.star."""

load("//scripts/shac/docs.star", "_codelinks")

# Split up so that the codelinks check doesn't flag this file.
_GOB_DOCS = "https://fuchsia.googlesource.com" + "/fuchsia/+/refs/heads/main/docs/"
_FUCHSIA_DEV = "https://fuchsia.dev/fuchsia-src/"

def test_codelinks():
    res = testing.run(_codelinks, files = {
        "src/foo.cc": "// See %sfoo/bar.md.\n" % _GOB_DOCS,
    })
    url_len = len(_GOB_DOCS + "foo/bar.md")
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = (
                "Documentation links should point to fuchsia.dev rather than " +
                "fuchsia.googlesource.com. Consider changing this to %sfoo/bar." % _FUCHSIA_DEV
            ),
            filepath = "src/foo.cc",
            line = 1,
            col = 8,
            end_col = 8 + url_len,
            replacements = [_FUCHSIA_DEV + "foo/bar"],
        ),
    ))
    asserts.eq(res.files["src/foo.cc"], "// See %sfoo/bar.\n" % _FUCHSIA_DEV)

def test_codelinks_ignores_docs_dir():
    res = testing.run(_codelinks, files = {
        "docs/foo.md": "See %sfoo/bar.md.\n" % _GOB_DOCS,
    })
    asserts.eq(res.findings, ())

def test_codelinks_fuchsia_dev():
    res = testing.run(_codelinks, files = {
        "src/foo.cc": "// See %sfoo/bar.\n" % _FUCHSIA_DEV,
    })
    asserts.eq(res.findings, ())
