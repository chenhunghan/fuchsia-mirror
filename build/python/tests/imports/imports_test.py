#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests importing python_library() targets that set `imports`."""

import unittest
from importlib import resources

import imports
import vendored
import vendored_module
from vendored import data  # type: ignore[attr-defined]
from vendored import helper


class ImportsTest(unittest.TestCase):
    def test_package_under_import_dir(self) -> None:
        self.assertEqual(vendored.PACKAGE_VALUE, "package")
        self.assertEqual(helper.helper_value(), "helper")

    def test_module_under_import_dir(self) -> None:
        self.assertEqual(vendored_module.MODULE_VALUE, "module")

    def test_parent_import_dir(self) -> None:
        self.assertEqual(imports.PARENT_VALUE, "parent")

    def test_data_sources_in_top_level_package(self) -> None:
        self.assertEqual(
            resources.files(data).joinpath("data.txt").read_text().strip(),
            "data",
        )


if __name__ == "__main__":
    unittest.main()
