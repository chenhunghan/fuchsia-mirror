#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for tools/devshell/lib/shac_runner.py."""

import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import scm
import shac_runner


class ShacRunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        temp_dir = self.enterContext(tempfile.TemporaryDirectory())
        self.fuchsia_dir = pathlib.Path(temp_dir)
        self.build_dir = self.fuchsia_dir / "out" / "default"
        self.build_dir.mkdir(parents=True)
        self.shac_binary = self.fuchsia_dir / "shac"
        self.shac_binary.touch()
        self.enterContext(
            mock.patch.dict(
                os.environ, {"PREBUILT_SHAC": str(self.shac_binary)}
            )
        )

    def test_run_shac_validates_explicit_files(self) -> None:
        existing = self.fuchsia_dir / "foo.cc"
        existing.touch()
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ) as mock_get_groups,
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ),
            mock.patch.object(
                sys, "stderr", new_callable=io.StringIO
            ) as mock_stderr,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
                explicit_files=("//foo.cc", "//missing.cc"),
            )

        mock_get_groups.assert_called_once_with(
            self.fuchsia_dir,
            self.fuchsia_dir,
            explicit_files=("//foo.cc",),
        )
        self.assertIn("ERROR: no such file:", mock_stderr.getvalue())

    def test_run_shac_returns_early_when_all_explicit_files_missing(
        self,
    ) -> None:
        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True
            ) as mock_get_groups,
            mock.patch.object(
                sys, "stderr", new_callable=io.StringIO
            ) as mock_stderr,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
                explicit_files=("//missing.cc", "  "),
            )

        mock_get_groups.assert_not_called()
        self.assertIn("ERROR: no such file:", mock_stderr.getvalue())

    def test_run_shac_raises_when_shac_binary_missing(self) -> None:
        missing_shac = self.fuchsia_dir / "nonexistent_shac"
        with (
            mock.patch.dict(os.environ, {"PREBUILT_SHAC": str(missing_shac)}),
            self.assertRaises(FileNotFoundError),
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

    def _populate_static_check_tools(self) -> None:
        host_os, host_cpu = shac_runner._host_platform_and_cpu()
        host_dir = self.build_dir / f"host_{host_cpu}"
        host_dir.mkdir(parents=True, exist_ok=True)
        entries = []
        for tool_name in sorted(shac_runner._STATIC_CHECK_TOOLS):
            relative_path = f"host_{host_cpu}/{tool_name}"
            tool_binary = self.build_dir / relative_path
            tool_binary.touch(mode=0o755)
            tool_binary.chmod(0o755)
            entries.append(
                {
                    "name": tool_name,
                    "path": relative_path,
                    "os": host_os,
                    "cpu": host_cpu,
                }
            )
        (self.build_dir / "tool_paths.json").write_text(json.dumps(entries))

    def test_run_shac_skips_build_when_no_repo_has_shac(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=False,
                files=(),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(subprocess, "run", autospec=True) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        mock_run.assert_not_called()

    def test_run_shac_skips_build_when_static_check_tools_present(self) -> None:
        self._populate_static_check_tools()
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        mock_run.assert_called_once()
        command = mock_run.call_args.args[0]
        self.assertEqual(command[0], str(self.shac_binary))

    def test_run_shac_builds_when_static_check_tool_missing_from_disk(
        self,
    ) -> None:
        self._populate_static_check_tools()
        _, host_cpu = shac_runner._host_platform_and_cpu()
        (self.build_dir / f"host_{host_cpu}" / "cmc").unlink()
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cml",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        self.assertEqual(mock_run.call_count, 2)

    def test_run_shac_builds_when_tool_paths_json_malformed(self) -> None:
        (self.build_dir / "tool_paths.json").write_text("{not valid json")
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        self.assertEqual(mock_run.call_count, 2)

    def test_run_shac_uses_prebuilt_shac_env(self) -> None:
        custom_shac = self.fuchsia_dir / "custom_shac"
        custom_shac.touch()
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.dict(os.environ, {"PREBUILT_SHAC": str(custom_shac)}),
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        self.assertEqual(mock_run.call_count, 2)
        command = mock_run.call_args_list[1].args[0]
        self.assertEqual(command[0], str(custom_shac))

    def test_run_shac_command_construction(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
                all_files=True,
                only_checks=("clang_format", "commit_msg"),
                skip_checks=("rustfmt",),
                extra_args=("--no-color",),
            )

        self.assertEqual(mock_run.call_count, 2)
        build_call = mock_run.call_args_list[0]
        self.assertSequenceEqual(
            build_call.args[0],
            (
                str(self.fuchsia_dir / "scripts" / "fx"),
                "build",
                "--host",
                "//bundles/buildbot/static_checks",
            ),
        )
        self.assertEqual(
            build_call.kwargs["env"]["FUCHSIA_DIR"],
            str(self.fuchsia_dir),
        )
        self.assertEqual(
            build_call.kwargs["env"]["FUCHSIA_BUILD_DIR"],
            str(self.build_dir),
        )
        shac_command = mock_run.call_args_list[1].args[0]
        self.assertSequenceEqual(
            shac_command,
            (
                str(self.shac_binary),
                "check",
                "-C",
                str(self.fuchsia_dir),
                "--var=fuchsia_dir=.",
                "--var=fuchsia_build_dir=out/default",
                "--all",
                "--quiet",
                "--only=clang_format,commit_msg",
                "--skip=rustfmt",
                "--no-color",
                "foo.cc",
            ),
        )

    def test_run_shac_verbose_command_construction(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
            mock.patch.object(
                sys, "stdout", new_callable=io.StringIO
            ) as mock_stdout,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
                verbose=True,
            )

        self.assertEqual(mock_run.call_count, 2)
        shac_command = mock_run.call_args_list[1].args[0]
        self.assertSequenceEqual(
            shac_command,
            (
                str(self.shac_binary),
                "check",
                "-C",
                str(self.fuchsia_dir),
                "--var=fuchsia_dir=.",
                "--var=fuchsia_build_dir=out/default",
                "--verbose",
                "foo.cc",
            ),
        )
        self.assertIn(
            "build --host //bundles/buildbot/static_checks",
            mock_stdout.getvalue(),
        )

    def test_run_shac_dry_run_skips_execution(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(subprocess, "run", autospec=True) as mock_run,
            mock.patch.object(
                sys, "stdout", new_callable=io.StringIO
            ) as mock_stdout,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
                dry_run=True,
            )

        mock_run.assert_not_called()
        stdout = mock_stdout.getvalue()
        self.assertIn(
            "[dry-run] Would execute:"
            f" {self.fuchsia_dir / 'scripts' / 'fx'} build --host"
            " //bundles/buildbot/static_checks",
            stdout,
        )
        self.assertIn(
            f"[dry-run] Would execute: {self.shac_binary} check",
            stdout,
        )

    def test_run_shac_propagates_build_error(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                side_effect=subprocess.CalledProcessError(
                    returncode=1, cmd=["fx", "build"]
                ),
            ) as mock_run,
            self.assertRaises(subprocess.CalledProcessError),
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        mock_run.assert_called_once()

    def test_run_shac_propagates_subprocess_error(self) -> None:
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("foo.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                side_effect=(
                    subprocess.CompletedProcess(args=[], returncode=0),
                    subprocess.CalledProcessError(
                        returncode=1, cmd=["shac", "check"]
                    ),
                ),
            ) as mock_run,
            self.assertRaises(subprocess.CalledProcessError),
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        self.assertEqual(mock_run.call_count, 2)

    def test_run_shac_executes_across_multiple_repos(self) -> None:
        subrepo = self.fuchsia_dir / "vendor" / "google"
        subrepo.mkdir(parents=True)
        repo_groups = (
            scm.RepoGroup(
                repo_root=self.fuchsia_dir,
                has_shac=True,
                files=("src/main.cc",),
            ),
            scm.RepoGroup(
                repo_root=subrepo,
                has_shac=True,
                files=("foo/bar.cc",),
            ),
        )

        with (
            mock.patch.object(
                scm, "get_repo_groups", autospec=True, return_value=repo_groups
            ),
            mock.patch.object(
                subprocess,
                "run",
                autospec=True,
                return_value=subprocess.CompletedProcess(args=[], returncode=0),
            ) as mock_run,
        ):
            shac_runner.run_shac(
                "check",
                self.fuchsia_dir,
                self.build_dir,
                self.fuchsia_dir,
            )

        self.assertEqual(mock_run.call_count, 3)
        build_command = mock_run.call_args_list[0].args[0]
        first_shac_command = mock_run.call_args_list[1].args[0]
        second_shac_command = mock_run.call_args_list[2].args[0]
        self.assertSequenceEqual(
            build_command,
            (
                str(self.fuchsia_dir / "scripts" / "fx"),
                "build",
                "--host",
                "//bundles/buildbot/static_checks",
            ),
        )
        self.assertSequenceEqual(
            first_shac_command,
            (
                str(self.shac_binary),
                "check",
                "-C",
                str(self.fuchsia_dir),
                "--var=fuchsia_dir=.",
                "--var=fuchsia_build_dir=out/default",
                "--quiet",
                "src/main.cc",
            ),
        )
        self.assertSequenceEqual(
            second_shac_command,
            (
                str(self.shac_binary),
                "check",
                "-C",
                str(subrepo),
                "--var=fuchsia_dir=../..",
                "--var=fuchsia_build_dir=out/default",
                "--quiet",
                "foo/bar.cc",
            ),
        )

    def test_split_comma_separated(self) -> None:
        self.assertSequenceEqual(
            shac_runner.split_comma_separated(()),
            (),
        )
        self.assertSequenceEqual(
            shac_runner.split_comma_separated(
                ("clang_format, rustfmt ", "", "commit_msg , ")
            ),
            ("clang_format", "rustfmt", "commit_msg"),
        )

    def test_resolve_workspace_paths_defaults(self) -> None:
        env_without_dirs = {
            key: value
            for key, value in os.environ.items()
            if key not in ("FUCHSIA_DIR", "FUCHSIA_BUILD_DIR")
        }
        with mock.patch.dict(os.environ, env_without_dirs, clear=True):
            fuchsia_dir, build_dir, _ = shac_runner.resolve_workspace_paths(
                self.fuchsia_dir
            )
        self.assertEqual(fuchsia_dir, self.fuchsia_dir.resolve())
        self.assertEqual(
            build_dir, (self.fuchsia_dir / "out" / "default").resolve()
        )

    def test_resolve_workspace_paths_from_env(self) -> None:
        custom_build_dir = self.fuchsia_dir / "out" / "custom"
        with mock.patch.dict(
            os.environ,
            {
                "FUCHSIA_DIR": str(self.fuchsia_dir),
                "FUCHSIA_BUILD_DIR": str(custom_build_dir),
            },
        ):
            fuchsia_dir, build_dir, _ = shac_runner.resolve_workspace_paths(
                pathlib.Path("/unused/fallback")
            )
        self.assertEqual(fuchsia_dir, self.fuchsia_dir.resolve())
        self.assertEqual(build_dir, custom_build_dir.resolve())


if __name__ == "__main__":
    unittest.main()
