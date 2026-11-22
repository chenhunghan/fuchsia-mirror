#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.rbe.bazel_rbe_e2e."""

import contextlib
import io
import pathlib
import subprocess
import tempfile
import time
import unittest
from unittest import mock

from build.auth import auth_env
from build.rbe import bazel_rbe_e2e, rbe_profile


class BazelRbeE2eTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.fuchsia_dir = pathlib.Path(temp_dir.name)
        self.profile = rbe_profile.fake(
            rbe_instance="projects/test-proj/instances/default",
            remote_exec_root="/b/f/w",
            auth_type="google",
        )

    @mock.patch.object(
        auth_env,
        "resolve_credentials_file",
        autospec=True,
        return_value=None,
    )
    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="loas",
    )
    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_loas_first_attempt_succeeds(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        _mock_ensure_creds: mock.Mock,
        _mock_resolve_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        mock_run.side_effect = (
            FileNotFoundError("no credhelper"),
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 0),
        )

        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            updated_env = bazel_rbe_e2e.run_bazel_rbe_e2e(
                self.fuchsia_dir,
                {
                    "PREBUILT_BAZEL": "/opt/bazel/bin/bazel",
                    auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/stale.json",
                },
            )

        self.assertNotIn(
            auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS, updated_env
        )
        self.assertIn("bazel+RBE authentication successful", stdout.getvalue())
        setup_cmd = mock_run.call_args_list[1].args[0]
        self.assertIn("--bazel-bin=/opt/bazel/bin/bazel", setup_cmd)

        test_cmd = mock_run.call_args_list[3].args[0]
        self.assertIn("--config=gcertauth", test_cmd)
        self.assertIn("--config=resultstore", test_cmd)

    @mock.patch.object(
        auth_env,
        "resolve_credentials_file",
        autospec=True,
        return_value=pathlib.Path("/tmp/isolated_adc.json"),
    )
    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="oauth",
    )
    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_oauth_no_resultstore_sets_credentials_and_bes_flag(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        _mock_ensure_creds: mock.Mock,
        _mock_resolve_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        mock_run.return_value = subprocess.CompletedProcess((), 0)

        with contextlib.redirect_stdout(io.StringIO()):
            updated_env = bazel_rbe_e2e.run_bazel_rbe_e2e(
                self.fuchsia_dir,
                {},
                no_resultstore=True,
            )

        self.assertEqual(
            updated_env[auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS],
            "/tmp/isolated_adc.json",
        )
        test_cmd = mock_run.call_args_list[2].args[0]
        self.assertIn("--bes_backend=", test_cmd)
        self.assertNotIn("--config=gcertauth", test_cmd)

    @mock.patch.object(
        time, "perf_counter", autospec=True, side_effect=(10.0, 12.5)
    )
    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="loas",
    )
    def test_cred_helper_slow_prints_warning(
        self,
        _mock_resolve_auth: mock.Mock,
        mock_run: mock.Mock,
        _mock_perf_counter: mock.Mock,
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess((), 1)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            bazel_rbe_e2e._check_credential_helper_latency(self.fuchsia_dir, {})
        self.assertIn("extremely slow", stderr.getvalue())

    @mock.patch.object(
        auth_env, "resolve_credentials_file", autospec=True, return_value=None
    )
    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="oauth",
    )
    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_three_failed_attempts_prompts_auth_and_raises(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
        _mock_resolve_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        mock_run.side_effect = (
            # Attempt 1: setup ok, shutdown ok, test fails
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 1),
            # Attempt 2: setup ok, shutdown ok, test fails
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 1),
            # Attempt 3: setup ok, shutdown ok, test fails
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 0),
            subprocess.CompletedProcess((), 1),
        )

        with (
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(
                bazel_rbe_e2e.BazelRbeE2eError, "Failed to use Bazel RBE"
            ),
        ):
            bazel_rbe_e2e.run_bazel_rbe_e2e(
                self.fuchsia_dir, {}, gcloud_args=("--no-browser",)
            )

        # 3 per-attempt ensure_credentials + 1 final interactive ensure_credentials
        self.assertEqual(mock_ensure_creds.call_count, 4)


if __name__ == "__main__":
    unittest.main()
