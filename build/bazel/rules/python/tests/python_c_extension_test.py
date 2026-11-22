# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the Bazel python_c_extension() macro."""

import os
import unittest

import c_ext_minimal
import c_ext_with_stubs
import python.runfiles.runfiles as runfiles


class PythonCExtensionTest(unittest.TestCase):
    def test_extension_with_stubs_and_deps(self):
        # Calls into the static C++ library dependency.
        self.assertEqual(c_ext_with_stubs.add_ints(17, 25), 42)
        self.assertEqual(c_ext_with_stubs.add_ints(2147483647, 1), 2147483648)
        # Calls into the dynamic shared library dependency via _solib_* + RUNPATH.
        self.assertEqual(c_ext_with_stubs.mul_ints(6, 7), 42)
        self.assertEqual(c_ext_with_stubs.mul_ints(50000, 50000), 2500000000)
        # Reads compile-time define passed via python_c_extension(defines=...).
        self.assertEqual(c_ext_with_stubs.get_define_val(), 42)
        # Exercises Python C API error/exception symbols resolved against interpreter.
        with self.assertRaises(ValueError):
            c_ext_with_stubs.add_ints(-1, 5)

    def test_minimal_extension(self):
        # c_ext_minimal intentionally omits .pyi stubs, so its attributes are
        # resolved dynamically rather than statically typed via Pyrefly.
        self.assertEqual(getattr(c_ext_minimal, "ping")(), "pong")

    def test_staged_package_layout(self):
        # create_c_extension_import_hook.py updates the package globals with the
        # loaded C extension module's __dict__, so __file__ resolves to the .so
        # inside the isolated staging directory.
        self.assertIsNotNone(c_ext_with_stubs.__file__)
        assert c_ext_with_stubs.__file__ is not None
        self.assertTrue(
            c_ext_with_stubs.__file__.endswith(
                "/_c_ext_with_stubs_py_stage/c_ext_with_stubs/libc_ext_with_stubs.so"
            ),
            f"Unexpected __file__: {c_ext_with_stubs.__file__}",
        )
        stage_dir = os.path.dirname(c_ext_with_stubs.__file__)
        for rel_path in (
            "__init__.py",
            "__init__.pyi",
            os.path.join("sub", "__init__.pyi"),
        ):
            staged_file = os.path.join(stage_dir, rel_path)
            self.assertTrue(
                os.path.exists(staged_file),
                f"Expected staged file {staged_file} (stage_dir={os.listdir(stage_dir)})",
            )
            # Staged artifacts in *_py_stage must be regular files rather than
            # symlinks into gen/build/bazel/workspace/, which break LUCI CAS
            # downloads on Swarming bots. Follow any Bazel .runfiles hops and
            # verify the staged file itself is not a symlink.
            cur = staged_file
            while os.path.islink(cur):
                target = os.readlink(cur)
                self.assertNotIn(
                    "gen/build/bazel/workspace/",
                    target,
                    f"Staged {staged_file} symlink {cur} -> {target}",
                )
                in_runfiles = any(
                    part.endswith((".runfiles", ".runtime_dir"))
                    for part in cur.split(os.sep)
                )
                self.assertTrue(
                    in_runfiles,
                    f"Staged artifact {cur} must be a regular file, got symlink -> {target}",
                )
                cur = os.path.normpath(
                    os.path.join(os.path.dirname(cur), target)
                )

        self.assertIsNotNone(c_ext_minimal.__file__)
        assert c_ext_minimal.__file__ is not None
        self.assertTrue(
            c_ext_minimal.__file__.endswith(
                "/_c_ext_minimal_py_stage/c_ext_minimal/libc_ext_minimal.so"
            ),
            f"Unexpected __file__: {c_ext_minimal.__file__}",
        )
        minimal_stage_dir = os.path.dirname(c_ext_minimal.__file__)
        minimal_init = os.path.join(minimal_stage_dir, "__init__.py")
        self.assertTrue(
            os.path.exists(minimal_init),
            f"Expected staged file {minimal_init} (minimal_stage_dir={os.listdir(minimal_stage_dir)})",
        )
        minimal_pyi = os.path.join(minimal_stage_dir, "__init__.pyi")
        self.assertFalse(
            os.path.exists(minimal_pyi),
            f"Did not expect {minimal_pyi} (minimal_stage_dir={os.listdir(minimal_stage_dir)})",
        )

    def test_non_library_data_filtered_from_runfiles(self):
        r = runfiles.Create()
        if r is None:
            self.fail("Cannot find runfiles")
        shlib_path = r.Rlocation(
            "_main/build/bazel/rules/python/tests/"
            "_c_ext_with_stubs_py_stage/c_ext_with_stubs/"
            "libc_ext_with_stubs.so"
        )
        self.assertIsNotNone(
            shlib_path,
            "Rlocation returned None for libc_ext_with_stubs.so",
        )
        assert shlib_path is not None
        self.assertTrue(
            os.path.exists(shlib_path),
            f"Missing runfiles shlib at {shlib_path}",
        )

        leaked_data_path = r.Rlocation(
            "_main/build/bazel/rules/python/tests/"
            "c_extension/non_lib_data.txt"
        )
        self.assertTrue(
            leaked_data_path is None or not os.path.exists(leaked_data_path),
            f"Non-library data file leaked into runfiles: {leaked_data_path}",
        )


if __name__ == "__main__":
    unittest.main()
