#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import subprocess
import unittest

import python.runfiles.runfiles as runfiles

# The py_binary to invoke, which must be listed in the test's `test_data`.
_BINARY_RLOCATION = (
    "_main/build/bazel/rules/host_tests/tests/py/test_with_py_library_bin"
)


class TestWithPyBinary(unittest.TestCase):
    def test_run_py_binary(self) -> None:
        r = runfiles.Create()
        self.assertIsNotNone(r, "Cannot find runfiles")
        binary = r.Rlocation(_BINARY_RLOCATION)
        self.assertTrue(
            binary is not None and os.path.exists(binary),
            f"Cannot find {_BINARY_RLOCATION} in runfiles",
        )
        subprocess.check_call([binary], env=os.environ | r.EnvVars())
