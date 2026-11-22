#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from build.python.modules import bazel_labels


class BazelLabelsTest(unittest.TestCase):
    def test_normalize_label(self) -> None:
        for label, expected in (
            ("//src/foo:bar", "//src/foo:bar"),
            ("@//src/foo:bar", "//src/foo:bar"),
            ("@@//src/foo:bar", "//src/foo:bar"),
            # Labels in other repositories are left alone.
            ("@foo//src/foo", "@foo//src/foo"),
            ("@@foo+//src/foo", "@@foo+//src/foo"),
        ):
            with self.subTest(label=label):
                self.assertEqual(bazel_labels.normalize_label(label), expected)

    def test_expand_shorthand_label(self) -> None:
        for label, expected in (
            ("//src/foo:bar", "//src/foo:bar"),
            ("@@//src/foo:bar", "//src/foo:bar"),
            ("@//src/foo", "//src/foo:foo"),
            ("//src/foo/", "//src/foo:foo"),
        ):
            with self.subTest(label=label):
                self.assertEqual(
                    bazel_labels.expand_shorthand_label(label), expected
                )

    def test_is_target_pattern(self) -> None:
        for label in (
            "//...",
            "@//src/...",
            "//src/...:all",
            "//src/foo:all",
            "@@//src/foo:*",
            "//src/foo:all-targets",
        ):
            with self.subTest(label=label):
                self.assertTrue(bazel_labels.is_target_pattern(label))
        # Relative patterns aren't absolute labels.
        for label in ("//src/foo", "//src/foo:bar", "...", ":all", "src/..."):
            with self.subTest(label=label):
                self.assertFalse(bazel_labels.is_target_pattern(label))


if __name__ == "__main__":
    unittest.main()
