# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Typed test fixture importing valid_lib for pyrefly_aspect tests."""

import unittest

from build.bazel.aspects.tests.testdata import valid_lib


class ValidTest(unittest.TestCase):
    def test_greet(self):
        self.assertEqual(valid_lib.greet("fuchsia"), "hello, fuchsia")


if __name__ == "__main__":
    unittest.main()
