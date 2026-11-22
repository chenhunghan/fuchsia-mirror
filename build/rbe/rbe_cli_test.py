#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.rbe.rbe_cli."""

import contextlib
import io
import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

from build.auth import auth_env
from build.rbe import (
    bazel_rbe_e2e,
    rbe_cli,
    rbe_diagnose,
    rbe_profile,
    rbe_settings,
    reclient_e2e,
)


class RbeCliTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.fuchsia_dir = pathlib.Path(temp_dir.name)
        self.profile = rbe_profile.fake(
            rbe_instance="projects/test-proj/instances/default",
            remote_exec_root="/b/f/w",
            profile_name="default",
            auth_type="google",
        )

    @mock.patch.object(reclient_e2e, "run_reclient_e2e", autospec=True)
    @mock.patch.object(bazel_rbe_e2e, "run_bazel_rbe_e2e", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_auth_skips_when_profile_auth_type_is_none(
        self,
        mock_load: mock.Mock,
        mock_bazel_e2e: mock.Mock,
        mock_reclient_e2e: mock.Mock,
    ) -> None:
        mock_load.return_value = rbe_profile.fake(
            profile_name="local-grid", auth_type="none"
        )
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "auth"),
                {},
            )

        self.assertEqual(exit_code, 0)
        self.assertIn("does not require authentication", stdout.getvalue())
        mock_bazel_e2e.assert_not_called()
        mock_reclient_e2e.assert_not_called()

    @mock.patch.object(reclient_e2e, "run_reclient_e2e", autospec=True)
    @mock.patch.object(bazel_rbe_e2e, "run_bazel_rbe_e2e", autospec=True)
    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="oauth",
    )
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_auth_forwards_profile_force_no_rs_and_gcloud_flags(
        self,
        mock_load: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
        mock_bazel_e2e: mock.Mock,
        mock_reclient_e2e: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        mock_bazel_e2e.return_value = {
            "FX_BUILD_RBE_PROFILE": "custom",
            "FX_BUILD_RS_PROFILE": "custom",
            auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/tmp/adc.json",
        }

        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = rbe_cli.main(
                (
                    "--fuchsia-dir",
                    str(self.fuchsia_dir),
                    "--xtrace",
                    "auth",
                    "-f",
                    "-p",
                    "custom",
                    "--no-rs",
                    "--no-browser",
                ),
                {},
            )

        self.assertEqual(exit_code, 0)
        loaded_env = mock_load.call_args.args[1]
        self.assertEqual(loaded_env["FX_BUILD_RBE_PROFILE"], "custom")
        self.assertEqual(loaded_env["FX_BUILD_RS_PROFILE"], "custom")
        mock_ensure_creds.assert_called_once_with(
            "oauth",
            force=True,
            out_dir=self.fuchsia_dir / "out",
            quota_project="test-proj",
            gcloud_args=("--no-browser",),
            env=mock.ANY,
        )
        mock_bazel_e2e.assert_called_once_with(
            self.fuchsia_dir,
            mock.ANY,
            no_resultstore=True,
            gcloud_args=("--no-browser",),
        )
        mock_reclient_e2e.assert_called_once_with(
            self.fuchsia_dir,
            mock_bazel_e2e.return_value,
            xtrace=True,
            gcloud_args=("--no-browser",),
        )
        self.assertIn("You can now 'fx build' with RBE.", stdout.getvalue())

    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="loas",
    )
    def test_check_loas_and_check_loas_type(
        self, mock_resolve: mock.Mock
    ) -> None:
        stdout_loas = io.StringIO()
        with contextlib.redirect_stdout(stdout_loas):
            code1 = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "check_loas", "-f"),
                {},
            )
        self.assertEqual(code1, 0)
        self.assertIn(
            "Checking build authentication method:", stdout_loas.getvalue()
        )
        self.assertIn("loas", stdout_loas.getvalue())
        mock_resolve.assert_called_once_with(
            self.fuchsia_dir, force=True, env={}
        )

        stdout_type = io.StringIO()
        with contextlib.redirect_stdout(stdout_type):
            code2 = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "_check_loas_type"),
                {},
            )
        self.assertEqual(code2, 0)
        self.assertEqual(stdout_type.getvalue().strip(), "loas")

    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        side_effect=RuntimeError("gcert expired"),
    )
    def test_check_loas_includes_exception_details_on_failure(
        self, _mock_resolve: mock.Mock
    ) -> None:
        for action in ("check_loas", "_check_loas_type"):
            with self.subTest(action=action):
                stdout = io.StringIO()
                stderr = io.StringIO()
                with (
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                ):
                    exit_code = rbe_cli.main(
                        ("--fuchsia-dir", str(self.fuchsia_dir), action),
                        {},
                    )
                self.assertEqual(exit_code, 1)
                self.assertIn(
                    "Error trying to select build authentication method: gcert expired",
                    stderr.getvalue(),
                )

    def test_check_loas_rejects_unknown_flag(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            exit_code = rbe_cli.main(
                (
                    "--fuchsia-dir",
                    str(self.fuchsia_dir),
                    "_check_loas_type",
                    "--bogus",
                ),
                {},
            )
        self.assertEqual(exit_code, 1)
        self.assertIn("Unknown option --bogus", stderr.getvalue())

    def test_cleanlogs_removes_reproxy_logs_dir(self) -> None:
        logs_dir = self.fuchsia_dir / "out/.reproxy_logs/default/reproxy.123"
        logs_dir.mkdir(parents=True)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "cleanlogs"),
                {},
            )
        self.assertEqual(exit_code, 0)
        self.assertFalse((self.fuchsia_dir / "out/.reproxy_logs").exists())

    def test_last_logdir_prefers_build_logs_and_falls_back_to_reproxy_logs(
        self,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True)
        env = {"FUCHSIA_BUILD_DIR": str(build_dir)}

        old_dir_1 = self.fuchsia_dir / "out/.reproxy_logs/default/reproxy.2"
        old_dir_2 = self.fuchsia_dir / "out/.reproxy_logs/default/reproxy.10"
        old_dir_1.mkdir(parents=True)
        old_dir_2.mkdir(parents=True)

        stdout_old = io.StringIO()
        with contextlib.redirect_stdout(stdout_old):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "last_logdir"),
                env,
            )
        self.assertEqual(exit_code, 0)
        self.assertEqual(stdout_old.getvalue().strip(), str(old_dir_2))

        new_dir_1 = (
            self.fuchsia_dir / "out/.build_logs/default/build.20261119-090000"
        )
        new_dir_2 = (
            self.fuchsia_dir / "out/.build_logs/default/build.20261119-100000"
        )
        new_dir_1.mkdir(parents=True)
        new_dir_2.mkdir(parents=True)

        stdout_new = io.StringIO()
        with contextlib.redirect_stdout(stdout_new):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "last_logdir"),
                env,
            )
        self.assertEqual(exit_code, 0)
        self.assertEqual(stdout_new.getvalue().strip(), str(new_dir_2))

    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="oauth",
    )
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_preflight_noop_when_build_does_not_need_auth(
        self,
        mock_load: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True)
        settings_data = {
            "final": rbe_settings.fake(needs_auth=False).__dict__,
        }
        (build_dir / "rbe_settings.json").write_text(
            json.dumps(settings_data), encoding="utf-8"
        )

        exit_code = rbe_cli.main(
            ("--fuchsia-dir", str(self.fuchsia_dir), "preflight"),
            {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "CONFIG_RESULTSTORE_ENABLED": "none",
            },
        )
        self.assertEqual(exit_code, 0)
        mock_load.assert_not_called()
        mock_ensure_creds.assert_not_called()

    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(
        auth_env,
        "resolve_and_cache_auth_type",
        autospec=True,
        return_value="oauth",
    )
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_preflight_runs_when_rbe_needs_auth_and_profile_is_google(
        self,
        mock_load: mock.Mock,
        _mock_resolve_auth: mock.Mock,
        mock_ensure_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = self.profile
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True)
        settings_data = {
            "final": rbe_settings.fake(needs_auth=True).__dict__,
        }
        (build_dir / "rbe_settings.json").write_text(
            json.dumps(settings_data), encoding="utf-8"
        )
        env = {
            "FUCHSIA_BUILD_DIR": str(build_dir),
            "CONFIG_RESULTSTORE_ENABLED": "none",
        }

        exit_code = rbe_cli.main(
            ("--fuchsia-dir", str(self.fuchsia_dir), "preflight"),
            env,
        )
        self.assertEqual(exit_code, 0)
        mock_ensure_creds.assert_called_once_with(
            "oauth",
            out_dir=self.fuchsia_dir / "out",
            quota_project="test-proj",
            env=env,
        )

    @mock.patch.object(auth_env, "ensure_credentials", autospec=True)
    @mock.patch.object(rbe_profile, "load", autospec=True)
    def test_preflight_skips_when_profile_auth_type_is_none(
        self,
        mock_load: mock.Mock,
        mock_ensure_creds: mock.Mock,
    ) -> None:
        mock_load.return_value = rbe_profile.fake(auth_type="none")
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True)

        exit_code = rbe_cli.main(
            ("--fuchsia-dir", str(self.fuchsia_dir), "preflight"),
            {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "FX_BUILD_RESULTSTORE_OVERRIDE": "all",
            },
        )
        self.assertEqual(exit_code, 0)
        mock_ensure_creds.assert_not_called()

    @mock.patch.object(rbe_diagnose, "main", autospec=True, return_value=0)
    def test_diagnose_delegates_to_rbe_diagnose(
        self, mock_diagnose: mock.Mock
    ) -> None:
        exit_code = rbe_cli.main(
            ("--fuchsia-dir", str(self.fuchsia_dir), "diagnose"),
            {"FOO": "bar"},
        )
        self.assertEqual(exit_code, 0)
        mock_diagnose.assert_called_once_with((), {"FOO": "bar"})

    @mock.patch.object(rbe_cli.subprocess, "run", autospec=True)
    def test_version_invokes_reproxy(self, mock_run: mock.Mock) -> None:
        reclient_dir = self.fuchsia_dir / "reclient_bin"
        reclient_dir.mkdir(parents=True)
        reproxy_bin = reclient_dir / "reproxy"
        reproxy_bin.write_text("", encoding="utf-8")
        env = {"PREBUILT_RECLIENT_DIR": str(reclient_dir)}
        mock_run.return_value = subprocess.CompletedProcess((), 0)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "version"),
                env,
            )
        self.assertEqual(exit_code, 0)
        self.assertIn("reclient version:", stdout.getvalue())
        mock_run.assert_called_once_with(
            (str(reproxy_bin), "--version"),
            env=env,
            check=False,
        )

    @mock.patch.object(rbe_cli.subprocess, "run", autospec=True)
    def test_version_missing_reproxy_binary_returns_error(
        self, mock_run: mock.Mock
    ) -> None:
        missing_dir = self.fuchsia_dir / "missing_reclient_bin"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            exit_code = rbe_cli.main(
                ("--fuchsia-dir", str(self.fuchsia_dir), "version"),
                {"PREBUILT_RECLIENT_DIR": str(missing_dir)},
            )
        self.assertEqual(exit_code, 1)
        self.assertIn("reproxy binary not found", stderr.getvalue())
        mock_run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
