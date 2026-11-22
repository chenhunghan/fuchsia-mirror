# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A host_py_test() main without a __main__ block, run by the default main_callable.

The test lives in a sub-directory and imports a sibling module, to verify that the
generated entry point preserves the main module's import path.
"""

import unittest

import helper_lib


class DefaultMainCallableTest(unittest.TestCase):
    def test_sibling_import(self) -> None:
        self.assertEqual(helper_lib.VALUE, 42)

    def test_something(self) -> None:
        self.assertEqual(1 + 1, 2)
