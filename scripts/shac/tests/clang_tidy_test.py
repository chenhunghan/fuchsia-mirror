#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

# Add parent directory to path so we can import clang_tidy
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

import clang_tidy


class TestClangTidy(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = self.enterContext(tempfile.TemporaryDirectory())
        self.root = pathlib.Path(self.temp_dir)

    def test_resolve_files_to_check_source_only(self) -> None:
        files = clang_tidy.resolve_files_to_check(
            self.root, ["src/foo/bar.cc"], []
        )
        self.assertEqual(files, ("src/foo/bar.cc",))

    def test_resolve_files_to_check_header_already_covered(self) -> None:
        files = clang_tidy.resolve_files_to_check(
            self.root, ["src/foo/bar.cc"], ["src/foo/bar.h"]
        )
        self.assertEqual(files, ("src/foo/bar.cc",))

    def test_resolve_files_to_check_header_companion_on_disk(self) -> None:
        companion_path = self.root / "src" / "foo" / "bar_test.cc"
        companion_path.parent.mkdir(parents=True, exist_ok=True)
        companion_path.write_text("// test\n")

        files = clang_tidy.resolve_files_to_check(
            self.root, [], ["src/foo/bar.h"]
        )
        self.assertEqual(files, ("src/foo/bar_test.cc",))

    def test_resolve_files_to_check_header_companion_in_sibling_src(
        self,
    ) -> None:
        companion_path = self.root / "src" / "pkg" / "src" / "widget.cc"
        companion_path.parent.mkdir(parents=True, exist_ok=True)
        companion_path.write_text("// impl\n")

        files = clang_tidy.resolve_files_to_check(
            self.root, [], ["src/pkg/include/lib/pkg/widget.h"]
        )
        self.assertEqual(files, ("src/pkg/src/widget.cc",))

    def test_resolve_files_to_check_ignores_unrelated_directory_same_stem(
        self,
    ) -> None:
        unrelated = self.root / "src" / "other" / "types.cc"
        unrelated.parent.mkdir(parents=True, exist_ok=True)
        unrelated.write_text("// unrelated\n")

        files = clang_tidy.resolve_files_to_check(
            self.root, [], ["src/foo/include/types.h"]
        )
        self.assertEqual(files, ("src/foo/include/types.h",))

    def test_resolve_files_to_check_standalone_header_fallback(self) -> None:
        files = clang_tidy.resolve_files_to_check(
            self.root, [], ["src/foo/standalone.h"]
        )
        self.assertEqual(files, ("src/foo/standalone.h",))

    def test_build_base_cmd_without_headers(self) -> None:
        cmd = clang_tidy.build_base_cmd("/bin/clang-tidy", "/out/default", [])
        self.assertEqual(
            cmd,
            (
                "/bin/clang-tidy",
                "-p",
                "/out/default",
                "-quiet",
                "--header-filter=",
            ),
        )

    def test_build_base_cmd_with_headers_escapes_regex(self) -> None:
        cmd = clang_tidy.build_base_cmd(
            "/bin/clang-tidy",
            "/out/default",
            ["src/foo/bar.h", "src/foo/c++_utils.h"],
        )
        self.assertEqual(
            cmd,
            (
                "/bin/clang-tidy",
                "-p",
                "/out/default",
                "-quiet",
                r"--header-filter=(^|/)(src/foo/bar\.h|src/foo/c\+\+_utils\.h)$",
            ),
        )

    def test_parse_clang_tidy_output_deduplicates_and_filters(self) -> None:
        outputs = [
            (
                "../../src/foo/bar.h:12:5: warning: use nullptr [modernize-use-nullptr]\n"
                "  int* p = 0;\n"
                "           ^\n"
                "../../src/foo/bar.cc: warning: file-level warning [misc-file-check]\n"
                "../../src/unrelated/other.h:4:1: warning: ignored [misc-unused]\n"
                "Error while processing /fuchsia/src/foo/bar.cc.\n"
            ),
            (
                # Duplicate finding from second translation unit including bar.h
                "/fuchsia/src/foo/bar.h:12:5: warning: use nullptr [modernize-use-nullptr]\n"
            ),
        ]
        findings = clang_tidy.parse_clang_tidy_output(
            outputs=outputs,
            affected_targets=["src/foo/bar.cc", "src/foo/bar.h"],
            build_dir="/fuchsia/out/default",
            scm_root="/fuchsia",
        )
        self.assertEqual(
            findings,
            (
                {
                    "message": "use nullptr [modernize-use-nullptr]",
                    "filepath": "src/foo/bar.h",
                    "line": 12,
                    "col": 5,
                },
                {
                    "message": "file-level warning [misc-file-check]",
                    "filepath": "src/foo/bar.cc",
                },
            ),
        )

    def test_parse_clang_tidy_output_discards_tu_with_diagnostic_error(
        self,
    ) -> None:
        outputs = [
            (
                "../../src/foo/bar.h:1:10: error: 'fidl/foo/cpp/fidl.h' file not found [clang-diagnostic-error]\n"
                "../../src/foo/bar.cc:20:6: warning: method ' Run' can be made static [readability-convert-member-functions-to-static]\n"
            ),
            (
                "../../src/foo/clean.cc:10:5: warning: use nullptr [modernize-use-nullptr]\n"
            ),
        ]
        findings = clang_tidy.parse_clang_tidy_output(
            outputs=outputs,
            affected_targets=[
                "src/foo/bar.cc",
                "src/foo/bar.h",
                "src/foo/clean.cc",
            ],
            build_dir="/fuchsia/out/default",
            scm_root="/fuchsia",
        )
        self.assertEqual(
            findings,
            (
                {
                    "message": "use nullptr [modernize-use-nullptr]",
                    "filepath": "src/foo/clean.cc",
                    "line": 10,
                    "col": 5,
                },
            ),
        )

    def test_main_missing_compile_commands(self) -> None:
        out = io.StringIO()
        with mock.patch("sys.stdout", out):
            rc = clang_tidy.main(
                [
                    "--clang-tidy",
                    "/bin/clang-tidy",
                    "--build-dir",
                    str(self.root / "out" / "missing"),
                    "--root",
                    str(self.root),
                    "src/foo/bar.cc",
                ]
            )
        self.assertEqual(rc, 0)
        self.assertEqual(json.loads(out.getvalue()), [])

    def test_main_runs_clang_tidy_and_outputs_json(self) -> None:
        build_dir = self.root / "out" / "default"
        build_dir.mkdir(parents=True)
        (build_dir / "compile_commands.json").write_text("[]")

        fake_stdout = (
            "../../src/foo/bar.cc:42:10: error: avoid magic numbers "
            "[readability-magic-numbers]\n"
        )
        out = io.StringIO()
        with (
            mock.patch(
                "subprocess.run",
                return_value=subprocess.CompletedProcess(
                    args=[], returncode=0, stdout=fake_stdout, stderr=""
                ),
            ) as mock_run,
            mock.patch("sys.stdout", out),
        ):
            rc = clang_tidy.main(
                [
                    "--clang-tidy",
                    "/bin/clang-tidy",
                    "--build-dir",
                    str(build_dir),
                    "--root",
                    str(self.root),
                    "src/foo/bar.cc",
                ]
            )
        self.assertEqual(rc, 0)
        mock_run.assert_called_once()
        self.assertEqual(mock_run.call_args.kwargs["cwd"], str(self.root))
        self.assertEqual(
            json.loads(out.getvalue()),
            [
                {
                    "message": "avoid magic numbers [readability-magic-numbers]",
                    "filepath": "src/foo/bar.cc",
                    "line": 42,
                    "col": 10,
                }
            ],
        )

    def test_main_logs_stderr_when_tu_skipped_on_diagnostic_error(self) -> None:
        build_dir = self.root / "out" / "default"
        build_dir.mkdir(parents=True)
        (build_dir / "compile_commands.json").write_text("[]")

        fake_stdout = (
            "../../src/foo/bar.h:1:10: error: 'fidl.h' file not found [clang-diagnostic-error]\n"
            "../../src/foo/bar.cc:15:6: warning: method 'Foo' can be made static "
            "[readability-convert-member-functions-to-static]\n"
        )
        out = io.StringIO()
        err = io.StringIO()
        with (
            mock.patch(
                "subprocess.run",
                return_value=subprocess.CompletedProcess(
                    args=[], returncode=1, stdout=fake_stdout, stderr=""
                ),
            ),
            mock.patch("sys.stdout", out),
            mock.patch("sys.stderr", err),
        ):
            rc = clang_tidy.main(
                [
                    "--clang-tidy",
                    "/bin/clang-tidy",
                    "--build-dir",
                    str(build_dir),
                    "--root",
                    str(self.root),
                    "src/foo/bar.cc",
                ]
            )
        self.assertEqual(rc, 0)
        self.assertEqual(json.loads(out.getvalue()), [])
        self.assertIn("Skipped 1 file(s)", err.getvalue())


if __name__ == "__main__":
    unittest.main()
