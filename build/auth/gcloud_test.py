# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for gcloud.py."""

import pathlib
import shutil
import subprocess
import unittest
from unittest import mock

from build.auth import gcloud


class GcloudTest(unittest.TestCase):
    """Unit tests for gcloud.py module-scoped functions."""

    def setUp(self) -> None:
        # Clear the module's functools cache to prevent cross-contamination between test cases
        gcloud.path.cache_clear()
        # path() falls back to looking for a system package install, which
        # would make results depend on whether gcloud is installed on the host.
        patcher = mock.patch.object(
            gcloud.apt, "get_binary_path", return_value=None
        )
        patcher.start()
        self.addCleanup(patcher.stop)

    @mock.patch.object(shutil, "which", return_value="/usr/bin/gcloud")
    def test_path_true(self, mock_which: mock.Mock) -> None:
        self.assertEqual(gcloud.path(), pathlib.Path("/usr/bin/gcloud"))

    @mock.patch.object(shutil, "which", return_value=None)
    def test_path_false(self, mock_which: mock.Mock) -> None:
        self.assertIsNone(gcloud.path())

    @mock.patch.object(shutil, "which", return_value="/bin/gcloud")
    @mock.patch.object(subprocess, "run")
    def test_login_success(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_run.return_value = mock_proc

        gcloud.login()

        mock_run.assert_called_once_with(
            ("/bin/gcloud", "auth", "application-default", "login"),
            stdin=mock.ANY,
            stdout=mock.ANY,
            stderr=mock.ANY,
            check=True,
        )

    @mock.patch.object(shutil, "which", return_value=None)
    def test_login_raises_runtime_error_if_gcloud_missing(
        self, mock_which: mock.Mock
    ) -> None:
        with self.assertRaises(RuntimeError) as ctx:
            gcloud.login()
        self.assertIn(
            "Google Cloud SDK ('gcloud') is not installed.", str(ctx.exception)
        )

    @mock.patch.object(shutil, "which", return_value="/bin/gcloud")
    @mock.patch.object(subprocess, "run")
    def test_login_raises_runtime_error_on_failure(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_run.side_effect = subprocess.CalledProcessError(1, "gcloud")

        with self.assertRaises(RuntimeError) as ctx:
            gcloud.login()
        self.assertIn("Interactive gcloud login failed", str(ctx.exception))

    @mock.patch.object(gcloud, "shutil", autospec=True)
    @mock.patch.object(gcloud, "subprocess", autospec=True)
    def test_login_forwards_extra_args(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/bin/gcloud"
        mock_subprocess.run.return_value = subprocess.CompletedProcess((), 0)
        gcloud.login(("--no-launch-browser",))
        mock_subprocess.run.assert_called_once_with(
            (
                "/bin/gcloud",
                "auth",
                "application-default",
                "login",
                "--no-launch-browser",
            ),
            stdin=mock.ANY,
            stdout=mock.ANY,
            stderr=mock.ANY,
            check=True,
        )

    @mock.patch.object(gcloud, "shutil", autospec=True)
    @mock.patch.object(gcloud, "subprocess", autospec=True)
    def test_has_valid_adc_token(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = None
        self.assertFalse(gcloud.has_valid_adc_token())

        gcloud.path.cache_clear()
        mock_shutil.which.return_value = "/bin/gcloud"
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 0, stdout="fake-access-token\n"
        )
        self.assertTrue(gcloud.has_valid_adc_token())

        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 0, stdout="\n"
        )
        self.assertFalse(gcloud.has_valid_adc_token())

        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 0, stdout="unexpected multi word output\n"
        )
        self.assertFalse(gcloud.has_valid_adc_token())

        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 1, stdout="fake-access-token\n"
        )
        self.assertFalse(gcloud.has_valid_adc_token())


if __name__ == "__main__":
    unittest.main()
