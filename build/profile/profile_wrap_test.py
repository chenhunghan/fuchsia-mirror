# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.profile.profile_wrap."""

import pathlib
import tempfile
import unittest
from unittest import mock

from build.profile import profile_wrap
from build.scripts import signal_utils


class ProfileArtifactsTest(unittest.TestCase):
    def test_artifacts(self) -> None:
        log_dir = pathlib.Path("/tmp/logs/profile")
        artifacts = profile_wrap.profile_artifacts(log_dir)
        self.assertEqual(
            artifacts,
            {
                "system_profile": log_dir
                / profile_wrap.SYSTEM_PROFILE_JSON_NAME,
                "hardware_profile": log_dir
                / profile_wrap.HARDWARE_PROFILE_JSON_NAME,
            },
        )


class GenerateProfilePrefixTest(unittest.TestCase):
    def test_default_prefix(self) -> None:
        system_log = pathlib.Path("/tmp/sys.json")
        prefix = list(
            profile_wrap.generate_profile_prefix(
                system_log=system_log,
                wrapper_path=pathlib.Path("/custom/profile_wrap.py"),
            )
        )
        self.assertEqual(
            prefix,
            [
                "/custom/profile_wrap.py",
                "--system-log",
                "/tmp/sys.json",
                "--",
            ],
        )

    def test_custom_interval_and_verbose(self) -> None:
        system_log = pathlib.Path("/tmp/sys.json")
        prefix = list(
            profile_wrap.generate_profile_prefix(
                system_log=system_log,
                interval=0.5,
                verbose=True,
                wrapper_path=pathlib.Path("/custom/profile_wrap.py"),
            )
        )
        self.assertEqual(
            prefix,
            [
                "/custom/profile_wrap.py",
                "--system-log",
                "/tmp/sys.json",
                "-n",
                "0.5",
                "--verbose",
                "--",
            ],
        )


class ParseArgsTest(unittest.TestCase):
    def test_parse_options_and_command(self) -> None:
        argv = [
            "--system-log",
            "/tmp/sys.json",
            "-n",
            "2.5",
            "--verbose",
            "--",
            "ninja",
            "target",
        ]
        args, cmd = profile_wrap._parse_args(argv)
        self.assertEqual(args.system_log, pathlib.Path("/tmp/sys.json"))
        self.assertEqual(args.interval, 2.5)
        self.assertTrue(args.verbose)
        self.assertEqual(cmd, ["ninja", "target"])


class RunProfiledCommandTest(unittest.TestCase):
    @mock.patch.object(pathlib.Path, "is_file", return_value=True)
    @mock.patch.object(signal_utils, "run_command", return_value=0)
    @mock.patch("subprocess.Popen")
    def test_run_command_starts_and_terminates_profiler(
        self,
        mock_popen: mock.MagicMock,
        mock_run_command: mock.MagicMock,
        mock_is_file: mock.MagicMock,
    ) -> None:
        mock_profiler = mock.MagicMock()
        mock_profiler.poll.return_value = None
        mock_popen.return_value = mock_profiler

        with tempfile.TemporaryDirectory() as tmpdir:
            system_log = pathlib.Path(tmpdir) / "profile.json"
            ret = profile_wrap._run_profiled_command(
                command=["echo", "hello"],
                system_log=system_log,
                env={},
            )
            self.assertEqual(ret, 0)
            mock_run_command.assert_called_once()
            mock_profiler.terminate.assert_called_once()


class MainTest(unittest.TestCase):
    def test_missing_command_returns_error(self) -> None:
        with mock.patch("sys.stderr.write"):
            ret = profile_wrap.main(["--system-log", "/tmp/log.json"], {})
            self.assertEqual(ret, 2)


if __name__ == "__main__":
    unittest.main()
