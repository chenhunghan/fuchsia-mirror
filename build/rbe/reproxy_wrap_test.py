# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.rbe.reproxy_wrap."""

import contextlib
import io
import pathlib
import tempfile
import unittest
from unittest import mock

from build.auth import auth_env, select_auth_method
from build.rbe import reproxy_wrap


class ReproxyArtifactsTest(unittest.TestCase):
    def test_reproxy_artifacts(self) -> None:
        log_dir = pathlib.Path("/tmp/reproxy_logs")
        artifacts = reproxy_wrap.reproxy_artifacts(log_dir)
        self.assertEqual(
            artifacts,
            {
                "metrics": log_dir / reproxy_wrap.METRICS_TXT_NAME,
                "rrpl": log_dir / reproxy_wrap.RRPL_NAME,
            },
        )


class ReproxyCacheDirTest(unittest.TestCase):
    def test_cache_dir_with_home(self) -> None:
        env = {"HOME": "/home/developer"}
        cache = reproxy_wrap._reproxy_cache_dir(env)
        self.assertEqual(
            cache, pathlib.Path("/home/developer/.cache/reproxy/deps")
        )

    def test_cache_dir_without_home(self) -> None:
        cache = reproxy_wrap._reproxy_cache_dir({})
        self.assertEqual(cache, pathlib.Path("/tmp/.cache/reproxy/deps"))


class ResolveAuthTypeTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        auth_env.detect_system_auth_type.cache_clear()
        self.addCleanup(auth_env.detect_system_auth_type.cache_clear)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()
        super().tearDown()

    def test_use_machine_credentials_overrides(self) -> None:
        auth = reproxy_wrap._resolve_auth_type(
            auth_type="loas",
            use_machine_credentials=True,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        self.assertEqual(auth, "machine")

    def test_explicit_valid_auth(self) -> None:
        for val in ("loas", "oauth", "machine", "none"):
            auth = reproxy_wrap._resolve_auth_type(
                auth_type=val,
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )
            self.assertEqual(auth, val)

    def test_auto_resolved_from_env(self) -> None:
        env = {auth_env.ENV_RBE_AUTH_TYPE: "oauth"}
        auth = reproxy_wrap._resolve_auth_type(
            auth_type="auto",
            use_machine_credentials=False,
            env=env,
            fuchsia_dir=self.fuchsia_dir,
        )
        self.assertEqual(auth, "oauth")

    def test_auto_fallback_to_detect_system_auth_type(self) -> None:
        with mock.patch.object(
            select_auth_method,
            "select_auth_method",
            return_value=select_auth_method.AuthType.LOAS,
        ):
            auth = reproxy_wrap._resolve_auth_type(
                auth_type="auto",
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )
            self.assertEqual(auth, "loas")

    def test_invalid_auth_type_raises(self) -> None:
        with self.assertRaises(ValueError):
            reproxy_wrap._resolve_auth_type(
                auth_type="invalid_type",
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )


class GenerateBootstrapEnvTest(unittest.TestCase):
    def test_bootstrap_env(self) -> None:
        log_dir = pathlib.Path("/tmp/reproxy_logs")
        tmp_dir = pathlib.Path("/tmp/reproxy_tmpdirs")
        cache_dir = pathlib.Path("/tmp/cache")
        env = reproxy_wrap._generate_bootstrap_env(
            server_address="unix:///tmp/sock",
            log_dir=log_dir,
            tmp_dir=tmp_dir,
            cache_dir=cache_dir,
            use_machine_credentials=True,
            is_unix_socket=True,
            casfs_virtualize=True,
            base_env={"CUSTOM_VAR": "value"},
        )
        self.assertEqual(env["CUSTOM_VAR"], "value")
        self.assertEqual(env["TMPDIR"], str(tmp_dir))
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_PROXY_LOG_DIR], str(log_dir)
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_SERVER_ADDRESS], "unix:///tmp/sock"
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_USE_APPLICATION_DEFAULT_CREDENTIALS],
            "false",
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_USE_GCE_CREDENTIALS], "true"
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_TLS_SERVER_NAME],
            "remotebuildexecution.googleapis.com",
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_XATTR_DIGEST], "casfs.rbe_ari_hash"
        )


class GenerateRewrapperEnvTest(unittest.TestCase):
    def test_rewrapper_env(self) -> None:
        log_dir = pathlib.Path("/tmp/reproxy_logs")
        env = reproxy_wrap._generate_rewrapper_env(
            server_address="unix:///tmp/sock",
            log_dir=log_dir,
            build_uuid="uuid-1234",
            casfs_virtualize=True,
            base_env={"PATH": "/bin"},
        )
        self.assertEqual(env["PATH"], "/bin")
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_SERVER_ADDRESS], "unix:///tmp/sock"
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_LOG_DIR],
            str(log_dir / "rewrapper-logs"),
        )
        self.assertEqual(
            env[reproxy_wrap._ENV_RECLIENT_INVOCATION_ID], "user:uuid-1234"
        )
        self.assertEqual(env["FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE"], "1")


class LinkDeterministicRrplTest(unittest.TestCase):
    def test_symlinks_found_rrpl(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            log_dir = pathlib.Path(tmpdir)
            rrpl_file = log_dir / "invocation_123.rrpl"
            rrpl_file.write_text("data")
            reproxy_wrap._link_deterministic_rrpl(log_dir)
            symlink = log_dir / reproxy_wrap.RRPL_NAME
            self.assertTrue(symlink.is_symlink())
            self.assertEqual(symlink.read_text(), "data")


class ParseArgsTest(unittest.TestCase):
    def test_parse_options_and_command(self) -> None:
        argv = [
            "--auth-type",
            "oauth",
            "--logdir",
            "/tmp/logs",
            "--extra-bootstrap-flag",
            "--",
            "ninja",
            "target",
        ]
        args, extra_opts, cmd = reproxy_wrap._parse_args(argv)
        self.assertEqual(args.auth_type, "oauth")
        self.assertEqual(args.logdir, pathlib.Path("/tmp/logs"))
        self.assertEqual(extra_opts, ["--extra-bootstrap-flag"])
        self.assertEqual(cmd, ["ninja", "target"])


class ReproxyActiveTest(unittest.TestCase):
    def test_inactive_when_empty_env(self) -> None:
        self.assertFalse(reproxy_wrap.is_active({}))
        self.assertIsNone(reproxy_wrap.reproxy_log_dir({}))
        self.assertIsNone(reproxy_wrap.rewrapper_log_dir({}))

    def test_active_when_both_dirs_set(self) -> None:
        env = {
            reproxy_wrap._ENV_RECLIENT_PROXY_LOG_DIR: "/tmp/proxy_logs",
            reproxy_wrap._ENV_RECLIENT_LOG_DIR: "/tmp/rewrapper_logs",
        }
        self.assertTrue(reproxy_wrap.is_active(env))
        self.assertEqual(
            reproxy_wrap.reproxy_log_dir(env), pathlib.Path("/tmp/proxy_logs")
        )
        self.assertEqual(
            reproxy_wrap.rewrapper_log_dir(env),
            pathlib.Path("/tmp/rewrapper_logs"),
        )


class RelaunchCommandTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_relaunch_cmd_and_env_propagates_machine_auth(self) -> None:
        script = pathlib.Path("/path/to/script.py")
        cmd, env = reproxy_wrap.relaunch_cmd_and_env(
            script=script,
            argv=["--opt", "val"],
            fuchsia_dir=self.fuchsia_dir,
            env={auth_env.ENV_RBE_AUTH_TYPE: "machine"},
            verbose=True,
        )
        self.assertIn("python", cmd[0])
        self.assertEqual(cmd[1], "-S")
        self.assertTrue(cmd[2].endswith("reproxy_wrap.py"))
        self.assertIn("-v", cmd)
        self.assertIn("--auth-type", cmd)
        self.assertIn("machine", cmd)
        self.assertIn("--use-machine-credentials", cmd)
        self.assertIn("--", cmd)
        self.assertEqual(cmd[-1], "val")
        self.assertEqual(env["PYTHONPATH"], str(self.fuchsia_dir))


class MaybeUploadReproxyLogsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.log_dir = pathlib.Path(self.tmpdir.name) / "logs"
        self.log_dir.mkdir(parents=True)
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name) / "fuchsia"
        self.fuchsia_dir.mkdir(parents=True)
        self.bindir = pathlib.Path("/mock/reclient/bin")
        self.uuid = "mock-uuid-1234"

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def _enable_build_metrics(self) -> None:
        config_file = self.fuchsia_dir / reproxy_wrap._BUILD_METRICS_CONFIG
        config_file.parent.mkdir(parents=True, exist_ok=True)
        config_file.write_text("BUILD_METRICS_ENABLED=1\n")

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=True
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_disabled_by_default(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        reproxy_wrap._maybe_upload_reproxy_logs(
            log_dir=self.log_dir,
            bindir=self.bindir,
            build_uuid=self.uuid,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        mock_subprocess.run.assert_not_called()

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=True
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_disabled_when_config_not_enabled(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        config_file = self.fuchsia_dir / reproxy_wrap._BUILD_METRICS_CONFIG
        config_file.parent.mkdir(parents=True, exist_ok=True)
        config_file.write_text("BUILD_METRICS_ENABLED=0\n")
        reproxy_wrap._maybe_upload_reproxy_logs(
            log_dir=self.log_dir,
            bindir=self.bindir,
            build_uuid=self.uuid,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        mock_subprocess.run.assert_not_called()

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=True
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_disabled_explicit_zero_env_override(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        self._enable_build_metrics()
        reproxy_wrap._maybe_upload_reproxy_logs(
            log_dir=self.log_dir,
            bindir=self.bindir,
            build_uuid=self.uuid,
            env={"FX_REMOTE_BUILD_METRICS": "0"},
            fuchsia_dir=self.fuchsia_dir,
        )
        mock_subprocess.run.assert_not_called()

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=False
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_disabled_script_missing(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        self._enable_build_metrics()
        reproxy_wrap._maybe_upload_reproxy_logs(
            log_dir=self.log_dir,
            bindir=self.bindir,
            build_uuid=self.uuid,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        mock_subprocess.run.assert_not_called()

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=True
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_enabled_when_opted_in(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        self._enable_build_metrics()
        reproxy_wrap._maybe_upload_reproxy_logs(
            log_dir=self.log_dir,
            bindir=self.bindir,
            build_uuid=self.uuid,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        mock_subprocess.run.assert_called_once()
        args, kwargs = mock_subprocess.run.call_args
        cmd = args[0]
        self.assertTrue(cmd[0].endswith("upload_reproxy_logs.sh"))
        self.assertIn(f"--reclient-bindir={self.bindir}", cmd)
        self.assertIn(f"--uuid={self.uuid}", cmd)
        self.assertIn(str(self.log_dir), cmd)
        self.assertFalse(kwargs.get("check", True))

    @mock.patch.object(
        pathlib.Path, "is_file", autospec=True, return_value=True
    )
    @mock.patch.object(reproxy_wrap, "subprocess", autospec=True, spec_set=True)
    def test_warning_on_oserror(
        self, mock_subprocess: mock.MagicMock, mock_is_file: mock.MagicMock
    ) -> None:
        self._enable_build_metrics()
        mock_subprocess.run.side_effect = OSError("permission denied")
        stderr_buf = io.StringIO()
        with contextlib.redirect_stderr(stderr_buf):
            reproxy_wrap._maybe_upload_reproxy_logs(
                log_dir=self.log_dir,
                bindir=self.bindir,
                build_uuid=self.uuid,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )
        output = stderr_buf.getvalue()
        self.assertIn("Warning: Failed to invoke metrics upload script", output)
        self.assertIn("permission denied", output)


class MainTest(unittest.TestCase):
    def test_missing_command_returns_error(self) -> None:
        stderr_buf = io.StringIO()
        with contextlib.redirect_stderr(stderr_buf):
            ret = reproxy_wrap.main(["--logdir", "/tmp/logs"], {})
        self.assertEqual(ret, 2)
        self.assertIn(
            "error: A command must be specified after '--'",
            stderr_buf.getvalue(),
        )


if __name__ == "__main__":
    unittest.main()
