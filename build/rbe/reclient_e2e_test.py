#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.rbe.reclient_e2e."""

import contextlib
import io
import json
import pathlib
import subprocess
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from typing import Any
from unittest import mock

from build.auth import auth_env
from build.rbe import casfs, rbe_profile, reclient_e2e


def _write_probe_output(cmd: Sequence[str], roots: Mapping[str, str]) -> None:
    for arg in cmd:
        if arg.startswith("--probe-output="):
            output_path = pathlib.Path(arg.removeprefix("--probe-output="))
            output_path.write_text(json.dumps(dict(roots)), encoding="utf-8")
            return
    raise AssertionError(f"--probe-output missing in command: {cmd}")


class ReclientE2eTest(unittest.TestCase):
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

    @mock.patch.object(subprocess, "run", autospec=True)
    def test_run_pool_probes_writes_uncached_pwd_for_both_pools(
        self, mock_run: mock.Mock
    ) -> None:
        mock_run.side_effect = (
            subprocess.CompletedProcess(
                (), 0, stdout="banner line\r\n/tmp/buildbox-aaa\r\n"
            ),
            subprocess.CompletedProcess((), 0, stdout="/tmp/buildbox-bbb\n"),
        )
        probe_json = self.fuchsia_dir / "probes.json"
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = reclient_e2e.main(
                (
                    "--run-pool-probes",
                    f"--fuchsia-dir={self.fuchsia_dir}",
                    f"--probe-output={probe_json}",
                ),
                {"RBE_instance": "projects/test-proj/instances/default"},
            )

        self.assertEqual(exit_code, 0)
        self.assertEqual(mock_run.call_count, 2)
        first_cmd = mock_run.call_args_list[0].args[0]
        second_cmd = mock_run.call_args_list[1].args[0]
        for expected_flag in (
            "--exec_strategy=remote",
            "--remote_accept_cache=false",
            "--remote_update_cache=false",
            "/bin/pwd",
        ):
            self.assertIn(expected_flag, first_cmd)
            self.assertIn(expected_flag, second_cmd)
        self.assertIn("--remote-worker-pool=default", first_cmd)
        self.assertIn("--remote-worker-pool=large", second_cmd)
        self.assertEqual(
            json.loads(probe_json.read_text(encoding="utf-8")),
            {
                "default": "/tmp/buildbox-aaa",
                "large": "/tmp/buildbox-bbb",
            },
        )
        self.assertIn(
            "remote working dir (default): /tmp/buildbox-aaa",
            stdout.getvalue(),
        )
        self.assertIn(
            "remote working dir (large): /tmp/buildbox-bbb",
            stdout.getvalue(),
        )

    @mock.patch.object(subprocess, "run", autospec=True)
    def test_run_pool_probes_stops_on_first_failure(
        self, mock_run: mock.Mock
    ) -> None:
        mock_run.return_value = subprocess.CompletedProcess((), 5, stdout="")

        with contextlib.redirect_stdout(io.StringIO()):
            exit_code = reclient_e2e.main(
                ("--run-pool-probes", f"--fuchsia-dir={self.fuchsia_dir}"),
                {},
            )
        self.assertEqual(exit_code, 5)
        self.assertEqual(mock_run.call_count, 1)

    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_run_reclient_e2e_succeeds_with_matching_glob_roots(
        self, mock_load: mock.Mock, mock_run: mock.Mock
    ) -> None:
        mock_load.return_value = rbe_profile.fake(
            rbe_instance="projects/test-proj/instances/default",
            remote_exec_root="/tmp/buildbox-*",
            auth_type="google",
        )

        def fake_wrap_run(
            cmd: Sequence[str], **_kwargs: Any
        ) -> subprocess.CompletedProcess[str]:
            _write_probe_output(
                cmd,
                {
                    "default": "/tmp/buildbox-worker1",
                    "large": "/tmp/buildbox-worker2",
                },
            )
            return subprocess.CompletedProcess(cmd, 0)

        mock_run.side_effect = fake_wrap_run
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            probes = reclient_e2e.run_reclient_e2e(
                self.fuchsia_dir,
                {
                    "FX_BUILD_RBE_STATS": "1",
                    casfs.ENV_CASFS_VIRTUALIZE: "1",
                },
                xtrace=True,
            )

        self.assertSequenceEqual(
            probes,
            (
                reclient_e2e.PoolProbe(
                    pool="default",
                    remote_working_dir="/tmp/buildbox-worker1",
                ),
                reclient_e2e.PoolProbe(
                    pool="large",
                    remote_working_dir="/tmp/buildbox-worker2",
                ),
            ),
        )
        called_cmd = mock_run.call_args.args[0]
        self.assertSequenceEqual(
            called_cmd[:3],
            (
                "/bin/bash",
                "-x",
                str(self.fuchsia_dir / "build/rbe/fuchsia-reproxy-wrap.sh"),
            ),
        )
        called_env = mock_run.call_args.kwargs["env"]
        self.assertNotIn("FX_BUILD_RBE_STATS", called_env)
        self.assertNotIn(casfs.ENV_CASFS_VIRTUALIZE, called_env)
        self.assertEqual(called_env["FX_REMOTE_BUILD_METRICS"], "0")
        self.assertIn(
            "re-client+RBE authentication and profile validation successful",
            stdout.getvalue(),
        )

    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(auth_env, "resolve_and_cache_auth_type", autospec=True)
    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_remote_exec_root_mismatch_fails_fast_without_reauth(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile

        def fake_wrap_run(
            cmd: Sequence[str], **_kwargs: Any
        ) -> subprocess.CompletedProcess[str]:
            _write_probe_output(
                cmd,
                {
                    "default": "/b/f/w",
                    "large": "/",
                },
            )
            return subprocess.CompletedProcess(cmd, 0)

        mock_run.side_effect = fake_wrap_run
        with (
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(
                reclient_e2e.RemoteExecRootMismatchError,
                r"Pool 'large': Remote worker execution root '/' does not match",
            ),
        ):
            reclient_e2e.run_reclient_e2e(self.fuchsia_dir, {})

        mock_run.assert_called_once()
        mock_resolve_auth.assert_not_called()
        mock_ensure_creds.assert_not_called()

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
    def test_bootstrap_exit_code_2_still_triggers_reauth_and_retry(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
        _mock_resolve_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        call_index = 0

        def fake_wrap_run(
            cmd: Sequence[str], **_kwargs: Any
        ) -> subprocess.CompletedProcess[str]:
            nonlocal call_index
            call_index += 1
            if call_index == 1:
                # Simulate bootstrap startup failure returning exit code 2.
                return subprocess.CompletedProcess(cmd, 2)
            _write_probe_output(
                cmd,
                {
                    "default": "/b/f/w",
                    "large": "/b/f/w",
                },
            )
            return subprocess.CompletedProcess(cmd, 0)

        mock_run.side_effect = fake_wrap_run
        with contextlib.redirect_stdout(io.StringIO()):
            reclient_e2e.run_reclient_e2e(
                self.fuchsia_dir,
                {},
                gcloud_args=("--no-browser",),
            )

        self.assertEqual(mock_run.call_count, 2)
        mock_resolve_auth.assert_called_once()
        mock_ensure_creds.assert_called_once_with(
            "oauth",
            out_dir=self.fuchsia_dir / "out",
            quota_project="test-proj",
            gcloud_args=("--no-browser",),
            env=mock.ANY,
        )

    @mock.patch.object(
        auth_env, "resolve_credentials_file", autospec=True, return_value=None
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
    def test_run_reclient_e2e_raises_after_three_failures(
        self,
        mock_load: mock.Mock,
        mock_run: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
        _mock_resolve_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        mock_run.return_value = subprocess.CompletedProcess((), 1)

        with (
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(
                reclient_e2e.ReclientE2eError, "after 3 attempts"
            ),
        ):
            reclient_e2e.run_reclient_e2e(self.fuchsia_dir, {})

        self.assertEqual(mock_run.call_count, 3)
        self.assertEqual(mock_ensure_creds.call_count, 2)


if __name__ == "__main__":
    unittest.main()
