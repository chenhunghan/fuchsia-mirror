#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import subprocess
import unittest

import python.runfiles.runfiles as runfiles

# The host test wrapper script to invoke, which must be listed in the test's
# `test_data`.
_TEST_RLOCATION = (
    "_main/build/bazel/rules/host_tests/tests/py/test_with_unittests"
)


class ListUnittestsTest(unittest.TestCase):
    def test_list_host_python_unittests(self) -> None:
        r = runfiles.Create()
        self.assertIsNotNone(r, "Cannot find runfiles")
        test_script = r.Rlocation(_TEST_RLOCATION)
        self.assertTrue(
            test_script is not None and os.path.exists(test_script),
            f"Cannot find {_TEST_RLOCATION} in runfiles",
        )
        output = subprocess.check_output(
            [test_script, "--list_host_python_unittests"],
            env=r.EnvVars(),
            text=True,
        )
        self.assertEqual(
            output,
            "TestWithUnittests.test_something\n"
            + "TestWithUnittests.test_something_else\n",
        )
