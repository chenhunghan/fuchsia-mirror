#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""End-to-end tests for `py_data_package()` in runfiles and `.pyz` archives."""

import importlib.resources
import os
import subprocess
import sys
import unittest
import zipfile

import dotted_pkg.nested
import pkg_with_data.reader as reader
import python.runfiles.runfiles as runfiles
import runfiles_carrier_pkg
import test_data

_ZIPAPP_RLOCATION = (
    "_main/build/bazel/rules/python/tests/py_data_package_zipapp_bin.pyz"
)
_CARRIER_DATA_RLOCATION = "_main/build/bazel/rules/python/tests/data/extra.txt"


class PyDataPackageTest(unittest.TestCase):
    def _resolve_rlocation(self, rlocation: str) -> str:
        r = runfiles.Create()
        if r is None:
            self.fail("Cannot find runfiles")
        path = r.Rlocation(rlocation)
        if path is None:
            self.fail(f"Cannot find {rlocation} in runfiles")
        return path

    def test_subpackage_in_runfiles(self):
        self.assertEqual(reader.read_greeting(), "hello from data")
        self.assertEqual(reader.read_untyped_py_data(), "x: int = 'bad'")
        # Verify executable permissions were set automatically on the staged
        # file for `cc_binary_host_tool` before any runtime chmod.
        self.assertEqual(
            reader.run_echo_tool("from_runfiles", require_pre_executable=True),
            "echo_tool:from_runfiles",
        )

    def test_isolated_test_data_no_collision(self):
        # `host_test_isolated_data` and `zipapp_isolated_data` are defined in
        # the same BUILD.bazel and both expose `test_data` via distinct
        # `package_dir` / `imports` prefixes (`isolated.txt` vs `extra.txt`).
        self.assertEqual(
            importlib.resources.files(test_data)
            .joinpath("isolated.txt")
            .read_text()
            .strip(),
            "hello from data",
        )
        with importlib.resources.as_file(
            importlib.resources.files(test_data).joinpath("isolated.txt")
        ) as isolated_path:
            # Staged artifacts in bazel-out must be materialized copies rather
            # than symlinks back to the original source file (`greeting.txt`),
            # which traverse `gen/build/bazel/workspace/` and break Swarming
            # CAS downloads.
            self.assertEqual(isolated_path.resolve().name, "isolated.txt")
        self.assertEqual(
            importlib.resources.files(test_data)
            .joinpath("gen_bad_data.py")
            .read_text()
            .strip(),
            "x: int = 'bad'",
        )
        self.assertEqual(
            importlib.resources.files(test_data)
            .joinpath("predeclared.py")
            .read_text()
            .strip(),
            "x: int = 'bad'",
        )
        self.assertFalse(
            importlib.resources.files(test_data).joinpath("extra.txt").is_file()
        )
        self.assertEqual(
            importlib.resources.files(dotted_pkg.nested)
            .joinpath("custom.txt")
            .read_text()
            .strip(),
            "hello from data",
        )

    def test_staged_target_default_runfiles_merged(self):
        # `carrier_cc_bin` declares `data = ["data/extra.txt"]`, which reaches
        # this test only through `runfiles_carrier_pkg`'s merged
        # `default_runfiles`.
        extra_path = self._resolve_rlocation(_CARRIER_DATA_RLOCATION)
        with open(extra_path) as f:
            self.assertEqual(f.read().strip(), "extra from data")
        with importlib.resources.as_file(
            importlib.resources.files(runfiles_carrier_pkg).joinpath(
                "carrier_cc_bin"
            )
        ) as bin_path:
            self.assertTrue(os.access(bin_path, os.X_OK))
            out = subprocess.check_output(
                (str(bin_path), "carrier"),
                text=True,
            )
            self.assertEqual(out.strip(), "echo_tool:carrier")

    def test_zipapp_packages_transitive_py_data_package(self):
        pyz_path = self._resolve_rlocation(_ZIPAPP_RLOCATION)
        # Strip runfiles env vars so the zipapp cannot fall back to this test's
        # `.runfiles` directory and must load everything from the `.pyz`.
        clean_env = {
            k: v for k, v in os.environ.items() if not k.startswith("RUNFILES_")
        }
        result = subprocess.run(
            (sys.executable, pyz_path),
            capture_output=True,
            text=True,
            env=clean_env,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "hello from data|x: int = 'bad'|echo_tool:from_zip|extra from data|hello from data\n",
        )

        with zipfile.ZipFile(pyz_path) as zf:
            names = set(zf.namelist())
        self.assertIn("pkg_with_data/data/__init__.py", names)
        self.assertIn("pkg_with_data/data/greeting.txt", names)
        self.assertIn("pkg_with_data/data/gen_bad_data.py", names)
        self.assertIn("pkg_with_data/data/renamed_echo_tool", names)
        self.assertIn("test_data/__init__.py", names)
        self.assertIn("test_data/extra.txt", names)
        self.assertIn("dotted_pkg/", names)
        self.assertIn("dotted_pkg/nested/__init__.py", names)
        self.assertIn("dotted_pkg/nested/custom.txt", names)


if __name__ == "__main__":
    unittest.main()
