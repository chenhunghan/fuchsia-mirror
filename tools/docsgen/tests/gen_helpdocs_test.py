#!/usr/bin/env fuchsia-vendored-python
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import gen_helpdocs


class GenReferenceDocsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.temp_dir)

    @mock.patch.object(gen_helpdocs, "subprocess", autospec=True)
    def test_run_helpdocs_success_suppresses_output(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        out_path = os.path.join(self.temp_dir, "gen", "test.tar.gz")
        src_dir = os.path.join(self.temp_dir, "src")
        depfile = os.path.join(self.temp_dir, "depfile")
        helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=[helpdoc_script, "--depfile", depfile, "--archive", out_path],
            returncode=0,
            stdout="Processing build\n",
            stderr="WARNING: fx metrics\n/bin/sh: 1: xargs: not found\n",
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with (
            contextlib.redirect_stdout(stdout_buf),
            contextlib.redirect_stderr(stderr_buf),
        ):
            exit_code = gen_helpdocs.run_fx_helpdoc(src_dir, out_path, depfile)

        self.assertEqual(exit_code, 0)
        mock_subprocess.run.assert_called_once_with(
            [helpdoc_script, "--depfile", depfile, "--archive", out_path],
            capture_output=True,
            text=True,
        )
        self.assertEqual(stdout_buf.getvalue(), "")
        self.assertEqual(stderr_buf.getvalue(), "")

    @mock.patch.object(gen_helpdocs, "subprocess", autospec=True)
    def test_run_helpdocs_writes_log_to_file(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        out_path = os.path.join(self.temp_dir, "gen", "test.tar.gz")
        src_dir = os.path.join(self.temp_dir, "src")
        depfile = os.path.join(self.temp_dir, "depfile")
        log_path = os.path.join(self.temp_dir, "helpdoc.log")
        helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
        expected_log = "Processing build\n./helpdoc/index.md\n"
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=[helpdoc_script, "--depfile", depfile, "--archive", out_path],
            returncode=0,
            stdout=expected_log,
            stderr="/bin/sh: 1: column: not found\n",
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with (
            contextlib.redirect_stdout(stdout_buf),
            contextlib.redirect_stderr(stderr_buf),
        ):
            exit_code = gen_helpdocs.run_fx_helpdoc(
                src_dir, out_path, depfile, log_to_file=log_path
            )

        self.assertEqual(exit_code, 0)
        with open(log_path) as log_file:
            self.assertEqual(log_file.read(), expected_log)
        self.assertEqual(stdout_buf.getvalue(), "")
        self.assertEqual(stderr_buf.getvalue(), "")

    @mock.patch.object(gen_helpdocs, "subprocess", autospec=True)
    def test_run_helpdocs_failure_surfaces_stderr_and_returns_nonzero(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        out_path = os.path.join(self.temp_dir, "gen", "test.tar.gz")
        src_dir = os.path.join(self.temp_dir, "src")
        depfile = os.path.join(self.temp_dir, "depfile")
        log_path = os.path.join(self.temp_dir, "helpdoc.log")
        helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=[helpdoc_script, "--depfile", depfile, "--archive", out_path],
            returncode=2,
            stdout="usage: fx helpdoc [OPTIONS] OUTPUT_PATH\n",
            stderr="ERROR: Invalid syntax",
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with (
            contextlib.redirect_stdout(stdout_buf),
            contextlib.redirect_stderr(stderr_buf),
        ):
            exit_code = gen_helpdocs.run_fx_helpdoc(
                src_dir, out_path, depfile, log_to_file=log_path
            )

        self.assertEqual(exit_code, 2)
        self.assertEqual(stdout_buf.getvalue(), "")
        self.assertEqual(
            stderr_buf.getvalue(),
            "usage: fx helpdoc [OPTIONS] OUTPUT_PATH\nERROR: Invalid syntax\n",
        )

    @mock.patch.object(gen_helpdocs, "subprocess", autospec=True)
    def test_main_success(self, mock_subprocess: mock.MagicMock) -> None:
        out_path = os.path.join(self.temp_dir, "test.tar.gz")
        src_dir = os.path.join(self.temp_dir, "src")
        depfile = os.path.join(self.temp_dir, "depfile.d")
        with open(depfile, "w") as stale_depfile:
            stale_depfile.write("stale: dep\n")
        log_path = os.path.join(self.temp_dir, "helpdoc.log")
        helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=[helpdoc_script, "--depfile", depfile, "--archive", out_path],
            returncode=0,
            stdout="Processing test\n",
            stderr="xargs: command not found\n",
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "gen_helpdocs.py",
                    "--out-path",
                    out_path,
                    "--src-dir",
                    src_dir,
                    "--log-to-file",
                    log_path,
                    "--depfile",
                    depfile,
                ],
            ),
            contextlib.redirect_stdout(stdout_buf),
            contextlib.redirect_stderr(stderr_buf),
        ):
            exit_code = gen_helpdocs.main()

        self.assertEqual(exit_code, 0)
        with open(log_path) as log_file:
            self.assertEqual(log_file.read(), "Processing test\n")
        with open(depfile) as dep_file:
            self.assertEqual(dep_file.read(), "")
        self.assertEqual(stdout_buf.getvalue(), "")
        self.assertEqual(stderr_buf.getvalue(), "")

    @mock.patch.object(gen_helpdocs, "subprocess", autospec=True)
    def test_main_failure_returns_nonzero_and_surfaces_stderr(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        out_path = os.path.join(self.temp_dir, "test.tar.gz")
        src_dir = os.path.join(self.temp_dir, "src")
        depfile = os.path.join(self.temp_dir, "depfile.d")
        helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=[helpdoc_script, "--depfile", depfile, "--archive", out_path],
            returncode=1,
            stdout="",
            stderr="ERROR: No output path specified\n",
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "gen_helpdocs.py",
                    "--out-path",
                    out_path,
                    "--src-dir",
                    src_dir,
                    "--depfile",
                    depfile,
                ],
            ),
            contextlib.redirect_stdout(stdout_buf),
            contextlib.redirect_stderr(stderr_buf),
        ):
            exit_code = gen_helpdocs.main()

        self.assertEqual(exit_code, 1)
        self.assertEqual(stdout_buf.getvalue(), "")
        self.assertEqual(
            stderr_buf.getvalue(), "ERROR: No output path specified\n"
        )


if __name__ == "__main__":
    unittest.main()
