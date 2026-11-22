# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fixture with a type error, verifying the tags = ["no-pyrefly"] opt-out."""

import unittest


def mistyped() -> int:
    return "not an int"


class TypeErrorOptoutTest(unittest.TestCase):
    def test_runs_despite_type_error(self) -> None:
        self.assertEqual(mistyped(), "not an int")
