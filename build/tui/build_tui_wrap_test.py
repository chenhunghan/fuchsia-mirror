#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build_tui_wrap.py."""

import pathlib
import tempfile
import unittest
from unittest import mock

from build.bazel.scripts import build_utils
from build.tui import build_tui_wrap


class RsclientBinDirTest(unittest.TestCase):
    def test_rsclient_bin_dir_path(self) -> None:
        fuchsia_dir = pathlib.Path("/workspace")
        host_tag = build_utils.get_host_tag()
        expected = fuchsia_dir / "prebuilt" / "rsclient" / host_tag / "bin"
        self.assertEqual(
            build_tui_wrap._rsclient_bin_dir(fuchsia_dir), expected
        )


class GenerateBuildminderPrefixTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.log_dir = self.fuchsia_dir / "tui_logs"
        self.rsclient_bin = build_tui_wrap._rsclient_bin_dir(self.fuchsia_dir)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_prefix_without_log_dir_or_options(self) -> None:
        res = list(build_tui_wrap.generate_buildminder_prefix(self.fuchsia_dir))
        expected = [
            str(self.rsclient_bin / "buildminder-wrap.sh"),
            "--buildminder",
            str(self.rsclient_bin / "buildminder"),
            "--mode",
            "integrated",
            "--",
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_log_dir_and_options(self) -> None:
        options = ["--print_failed_actions", "--headless"]
        res = list(
            build_tui_wrap.generate_buildminder_prefix(
                self.fuchsia_dir,
                log_dir=self.log_dir,
                buildminder_options=options,
            )
        )
        expected = [
            str(self.rsclient_bin / "buildminder-wrap.sh"),
            "--buildminder",
            str(self.rsclient_bin / "buildminder"),
            "--mode",
            "integrated",
            "--log-dir",
            str(self.log_dir),
            "--buildminder_options",
            "--print_failed_actions",
            "--headless",
            "--",
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_verbose(self) -> None:
        res = list(
            build_tui_wrap.generate_buildminder_prefix(
                self.fuchsia_dir,
                verbose=True,
            )
        )
        expected = [
            str(self.rsclient_bin / "buildminder-wrap.sh"),
            "--buildminder",
            str(self.rsclient_bin / "buildminder"),
            "--mode",
            "integrated",
            "--verbose",
            "--",
        ]
        self.assertEqual(res, expected)


class ComposeBuildminderCommandTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.log_dir = self.fuchsia_dir / "tui_logs"

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_compose_buildminder_command(self) -> None:
        cmd = ["ninja", "-C", "out/default"]
        options = ["--headless"]
        res = build_tui_wrap._compose_buildminder_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            log_dir=self.log_dir,
            buildminder_options=options,
        )
        expected_prefix = list(
            build_tui_wrap.generate_buildminder_prefix(
                self.fuchsia_dir,
                log_dir=self.log_dir,
                buildminder_options=options,
            )
        )
        self.assertEqual(res, expected_prefix + cmd)


class BuildminderEnvTest(unittest.TestCase):
    def test_buildminder_env_sets_besproxy_flag(self) -> None:
        env = build_tui_wrap.buildminder_env(
            {"FOO": "bar"},
            fuchsia_dir=pathlib.Path("/workspace"),
            python_dir=pathlib.Path("/opt/python/bin"),
        )
        self.assertEqual(env["FOO"], "bar")
        self.assertEqual(
            env[build_tui_wrap.ENV_FX_INTERNAL_BAZEL_BESPROXY], "1"
        )

    def test_buildminder_env_prepends_fuchsia_dir_to_pythonpath(self) -> None:
        fuchsia_dir = pathlib.Path("/workspace")
        env = build_tui_wrap.buildminder_env(
            {"PYTHONPATH": "/user/path"},
            fuchsia_dir=fuchsia_dir,
            python_dir=pathlib.Path("/opt/python/bin"),
        )
        self.assertEqual(env["PYTHONPATH"], f"{fuchsia_dir}:/user/path")

    def test_buildminder_env_prepends_python_dir_to_path(self) -> None:
        py_dir = pathlib.Path("/opt/python/bin")
        env = build_tui_wrap.buildminder_env(
            {"PATH": "/usr/bin:/bin"},
            fuchsia_dir=pathlib.Path("/workspace"),
            python_dir=py_dir,
        )
        self.assertEqual(env["PATH"], f"{py_dir}:/usr/bin:/bin")


class ParseArgsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.log_dir = self.fuchsia_dir / "logs"

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_parse_args_with_options_and_command(self) -> None:
        argv = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--log-dir",
            str(self.log_dir),
            "--headless",
            "--print_failed_actions",
            "--",
            "ninja",
            "-C",
            "out/default",
        ]
        args, bm_opts, cmd = build_tui_wrap._parse_args(argv)
        self.assertEqual(args.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(args.log_dir, self.log_dir)
        self.assertEqual(bm_opts, ["--headless", "--print_failed_actions"])
        self.assertEqual(cmd, ["ninja", "-C", "out/default"])

    def test_missing_command_raises_system_exit(self) -> None:
        with self.assertRaises(SystemExit):
            build_tui_wrap._parse_args(["--log-dir", "/tmp"])


class MainTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    @mock.patch("os.execvpe")
    def test_main_delegates_to_execvpe(self, mock_execvpe: mock.Mock) -> None:
        argv = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--",
            "echo",
            "hello",
        ]
        ret = build_tui_wrap.main(argv)
        self.assertEqual(ret, 0)
        self.assertTrue(mock_execvpe.called)
        cmd_arg, _, env_arg = mock_execvpe.call_args[0]
        self.assertTrue(cmd_arg.endswith("buildminder-wrap.sh"))
        self.assertEqual(
            env_arg[build_tui_wrap.ENV_FX_INTERNAL_BAZEL_BESPROXY], "1"
        )


if __name__ == "__main__":
    unittest.main()
