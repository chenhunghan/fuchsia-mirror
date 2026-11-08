#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for pyrefly_runner.py."""

import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

from build.bazel.aspects import pyrefly_runner


def _error(**overrides):
    return {
        "severity": "error",
        "path": "a.py",
        "line": 1,
        "column": 1,
        "description": "boom",
        "name": "internal",
    } | overrides


class FormatPyreflyErrorsTest(unittest.TestCase):
    def test_formats_valid_errors_and_strips_cwd(self):
        cwd = os.path.abspath(os.getcwd())
        payload = json.dumps(
            {
                "errors": [
                    _error(
                        path=os.path.join(cwd, "build/foo/bar.py"),
                        line=12,
                        column=5,
                        description="Returned `str`, expected `int`",
                        name="bad-return",
                    ),
                    _error(
                        severity="warn",
                        path="/external/other.py",
                        line=3,
                        description="Unused ignore",
                        name="unused-ignore",
                    ),
                ]
            }
        )
        self.assertEqual(
            pyrefly_runner._format_pyrefly_errors(payload),
            (
                "ERROR build/foo/bar.py:12:5: Returned `str`, expected `int` [bad-return]\n"
                "WARN /external/other.py:3:1: Unused ignore [unused-ignore]"
            ),
        )

    def test_returns_none_when_no_errors(self):
        self.assertIsNone(
            pyrefly_runner._format_pyrefly_errors(json.dumps({"errors": []}))
        )

    def test_raises_on_unexpected_output(self):
        for name, output in [
            ("non_json", "fatal: invalid config toml"),
            ("missing_errors_key", json.dumps({})),
            (
                "missing_error_fields",
                json.dumps({"errors": [{"severity": "error", "path": "a.py"}]}),
            ),
            ("null_severity", json.dumps({"errors": [_error(severity=None)]})),
            ("non_string_path", json.dumps({"errors": [_error(path=1)]})),
        ]:
            with self.subTest(name):
                with self.assertRaises(pyrefly_runner._PyreflyOutputError):
                    pyrefly_runner._format_pyrefly_errors(output)


class PyreflyRunnerMainTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp_path = pathlib.Path(tmp.name)
        self.stamp = self.tmp_path / "out.pyrefly.ok"

    def _write_fake_pyrefly(self, script):
        fake_pyrefly = self.tmp_path / "fake_pyrefly.sh"
        fake_pyrefly.write_text("#!/bin/sh\n" + script)
        fake_pyrefly.chmod(0o755)
        return fake_pyrefly

    def _argv(self, pyrefly, *extra_args):
        return [
            "--pyrefly",
            str(pyrefly),
            "--config",
            str(self.tmp_path / "pyproject.toml"),
            "--output",
            str(self.stamp),
            *extra_args,
        ]

    @mock.patch.object(pyrefly_runner, "subprocess")
    def test_no_python_sources_writes_stamp_without_spawning(
        self, mock_subprocess
    ):
        rc = pyrefly_runner.main(self._argv("/unused/pyrefly", "README.md"))

        self.assertEqual(rc, 0)
        mock_subprocess.run.assert_not_called()
        self.assertEqual(self.stamp.read_text(), "ok\n")

    @mock.patch.dict(os.environ, {"PYTHONPATH": "/ambient/leak"})
    def test_spawns_pyrefly_scrubs_pythonpath_and_dedupes_search_paths(self):
        dir_a = self.tmp_path / "pkg_a"
        dir_b = self.tmp_path / "pkg_b"
        dir_a.mkdir()
        dir_b.mkdir()
        src = dir_a / "mod.py"
        src.write_text("def f() -> int:\n    return 1\n")
        captured_args = self.tmp_path / "captured_args.txt"
        captured_env = self.tmp_path / "captured_env.txt"
        fake_pyrefly = self._write_fake_pyrefly(
            f'printf "%s\\n" "$@" > "{captured_args}"\n'
            f'printf "%s" "${{PYTHONPATH+set}}" > "{captured_env}"\n'
            "exit 0\n"
        )

        rc = pyrefly_runner.main(
            self._argv(
                fake_pyrefly,
                "--search-path",
                str(dir_a),
                "--search-path",
                str(dir_a) + "/.",
                "--search-path",
                str(self.tmp_path / "does_not_exist"),
                "--search-path",
                str(dir_b),
                str(src),
            )
        )

        self.assertEqual(rc, 0)
        self.assertEqual(self.stamp.read_text(), "ok\n")
        self.assertEqual(captured_env.read_text(), "")
        self.assertEqual(
            captured_args.read_text().splitlines(),
            [
                "check",
                "--config",
                str(self.tmp_path / "pyproject.toml"),
                "--search-path",
                os.path.normpath(str(dir_a)),
                "--search-path",
                os.path.normpath(str(dir_b)),
                "--disable-search-path-heuristics",
                "--skip-interpreter-query",
                "--use-ignore-files=false",
                "--output-format",
                "json",
                "--summary=none",
                str(src),
            ],
        )

    @mock.patch.object(sys, "stderr", new_callable=io.StringIO)
    def test_type_errors_print_diagnostics_and_skip_stamp(self, mock_stderr):
        src = self.tmp_path / "bad.py"
        src.write_text("x: int = 'oops'\n")
        err_json_file = self.tmp_path / "err.json"
        err_json_file.write_text(
            json.dumps(
                {
                    "errors": [
                        _error(
                            path=str(src),
                            column=10,
                            description="`str` is not assignable to `int`",
                            name="bad-assignment",
                        )
                    ]
                }
            )
        )
        fake_pyrefly = self._write_fake_pyrefly(
            f'cat "{err_json_file}"\n' 'printf "extra note\\n" >&2\n' "exit 1\n"
        )

        rc = pyrefly_runner.main(self._argv(fake_pyrefly, str(src)))

        self.assertEqual(rc, 1)
        self.assertFalse(self.stamp.exists())
        err_out = mock_stderr.getvalue()
        self.assertIn("Please fix the following Pyrefly errors:", err_out)
        self.assertIn("[bad-assignment]", err_out)
        self.assertIn("extra note", err_out)

    @mock.patch.object(sys, "stderr", new_callable=io.StringIO)
    def test_cli_error_reports_raw_output(self, mock_stderr):
        src = self.tmp_path / "bad.py"
        src.write_text("pass\n")
        fake_pyrefly = self._write_fake_pyrefly(
            'printf "invalid CLI flag\\n" >&2\nexit 2\n'
        )

        rc = pyrefly_runner.main(self._argv(fake_pyrefly, str(src)))

        self.assertEqual(rc, 2)
        self.assertFalse(self.stamp.exists())
        err_out = mock_stderr.getvalue()
        self.assertIn(
            "Error occurred while running Pyrefly (exit code 2):", err_out
        )
        self.assertIn("invalid CLI flag", err_out)


if __name__ == "__main__":
    unittest.main()
