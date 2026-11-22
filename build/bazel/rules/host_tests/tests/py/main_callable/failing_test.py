# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A host_py_test() main with a failing test case and no __main__ block."""

import unittest


class FailingTest(unittest.TestCase):
    def test_fails(self) -> None:
        self.assertEqual(1, 2)
