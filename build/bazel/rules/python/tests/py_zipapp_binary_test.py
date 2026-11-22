#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests the archives produced by py_zipapp_binary()."""

import os
import subprocess
import sys
import unittest
import zipfile

import python.runfiles.runfiles as runfiles

_MAIN_RLOCATION = "_main/build/bazel/rules/python/tests/zipapp_test_bin.pyz"
_ASYNC_RLOCATION = "_main/build/bazel/rules/python/tests/zipapp_test_async.pyz"

_EXPECTED_MAIN_OUTPUT = "zipapp_test_helper: hello from lib, hello from data\n"


class PyZipappBinaryTest(unittest.TestCase):
    def _resolve_pyz(self, rlocation: str) -> str:
        r = runfiles.Create()
        if r is None:
            self.fail("Cannot find runfiles")
        path = r.Rlocation(rlocation)
        if path is None:
            self.fail(f"Cannot find {rlocation} in runfiles")
        return path

    def test_main_runs(self) -> None:
        result = subprocess.run(
            (sys.executable, self._resolve_pyz(_MAIN_RLOCATION)),
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, _EXPECTED_MAIN_OUTPUT)

    def test_async_main_runs(self) -> None:
        result = subprocess.run(
            (sys.executable, self._resolve_pyz(_ASYNC_RLOCATION)),
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "async: hello from lib\n")

    def test_shebang(self) -> None:
        with open(self._resolve_pyz(_MAIN_RLOCATION), "rb") as f:
            self.assertEqual(
                f.readline(), b"#!/usr/bin/env fuchsia-vendored-python\n"
            )
        with open(self._resolve_pyz(_ASYNC_RLOCATION), "rb") as f:
            self.assertEqual(
                f.readline(),
                b"#!/usr/bin/env -S fuchsia-vendored-python -u\n",
            )

    def test_executable(self) -> None:
        self.assertTrue(os.access(self._resolve_pyz(_MAIN_RLOCATION), os.X_OK))
        self.assertTrue(os.access(self._resolve_pyz(_ASYNC_RLOCATION), os.X_OK))

    def test_layout(self) -> None:
        with zipfile.ZipFile(self._resolve_pyz(_MAIN_RLOCATION)) as zf:
            names = zf.namelist()
        # Entries are sorted, so the archive is deterministic.
        self.assertSequenceEqual(names, sorted(names))
        expected_files = {
            "__main__.py",
            # Sources of the binary, at the archive root.
            "zipapp_test_main.py",
            "zipapp_test_helper.py",
            # The same sources, under the repository root.
            "build/bazel/rules/python/tests/zipapp_test_main.py",
            "build/bazel/rules/python/tests/zipapp_test_helper.py",
            # The library source, under its `imports` root and the
            # repository root.
            "zipapp_test_lib.py",
            "build/bazel/rules/python/tests/lib/zipapp_test_lib.py",
            # The data package.
            "zipapp_test_data/__init__.py",
            "zipapp_test_data/greeting.txt",
        }
        self.assertEqual(
            {name for name in names if not name.endswith("/")}, expected_files
        )
        # Directory entries, which zipimport needs to find namespace packages
        # before Python 3.14.
        self.assertEqual(
            {name for name in names if name.endswith("/")},
            {
                "build/",
                "build/bazel/",
                "build/bazel/rules/",
                "build/bazel/rules/python/",
                "build/bazel/rules/python/tests/",
                "build/bazel/rules/python/tests/lib/",
                "zipapp_test_data/",
            },
        )

    def test_deterministic_metadata(self) -> None:
        with zipfile.ZipFile(self._resolve_pyz(_MAIN_RLOCATION)) as zf:
            for info in zf.infolist():
                self.assertEqual(
                    info.date_time, (1980, 1, 1, 0, 0, 0), info.filename
                )
                if not info.is_dir():
                    self.assertEqual(
                        info.compress_type, zipfile.ZIP_DEFLATED, info.filename
                    )


if __name__ == "__main__":
    unittest.main()
