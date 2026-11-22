#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest


class SimpleTest(unittest.TestCase):
    def test_simple(self) -> None:
        self.assertTrue(True)
