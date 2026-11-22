#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Run host_py_test() wrapper scripts and check their exit code and output."""

import os
import subprocess
import unittest

import python.runfiles.runfiles as runfiles

# Each test wrapper script below must be listed in the test's `test_data`.
_PACKAGE_RLOCATION = "_main/build/bazel/rules/host_tests/tests/py"


class MainCallableTest(unittest.TestCase):
    def _check(
        self, test_name: str, exit_code: int, expected_output: str
    ) -> None:
        r = runfiles.Create()
        self.assertIsNotNone(r, "Cannot find runfiles")
        rlocation = f"{_PACKAGE_RLOCATION}/{test_name}"
        test_script = r.Rlocation(rlocation)
        self.assertTrue(
            test_script is not None and os.path.exists(test_script),
            f"Cannot find {rlocation} in runfiles",
        )
        result = subprocess.run(
            [test_script],
            env=r.EnvVars(),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        self.assertEqual(result.returncode, exit_code, result.stdout)
        self.assertIn(expected_output, result.stdout)

    def test_default_runs_unittest_main(self) -> None:
        self._check("default_main_callable_test", 0, "Ran 2 tests")

    def test_default_fails_without_tests(self) -> None:
        self._check("no_tests_main_callable_test", 5, "NO TESTS RAN")

    def test_default_reports_failures(self) -> None:
        self._check("failing_main_callable_test", 1, "FAILED (failures=1)")

    def test_custom_callable(self) -> None:
        self._check("custom_main_callable_test", 0, "custom main called")

    def test_async_callable(self) -> None:
        self._check("async_main_callable_test", 0, "async main called")
