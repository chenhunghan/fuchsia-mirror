# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/mirror_blocklists.star."""

load("//scripts/shac/mirror_blocklists.star", "blocklist", "blocklist_mirrors")

def test_blocklist_mirrors():
    url = blocklist[0]
    res = testing.run(blocklist_mirrors, files = {
        "manifest": "<project remote=\"https://%s/\"/>\n" % url,
        "other": "<project remote=\"https://fuchsia.googlesource.com/foo\"/>\n",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "File contains a blocklisted mirror URL: %s" % url,
            filepath = "manifest",
        ),
    ))

def test_blocklist_mirrors_ignores_self():
    res = testing.run(blocklist_mirrors, files = {
        "scripts/shac/mirror_blocklists.star": "\n".join(blocklist),
    })
    asserts.eq(res.findings, ())

def test_blocklist_mirrors_unaffected_files():
    res = testing.run(blocklist_mirrors, files = {
        "manifest": testing.file(content = blocklist[0], affected = False),
    })
    asserts.eq(res.findings, ())
