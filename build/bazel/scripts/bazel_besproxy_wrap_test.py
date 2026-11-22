#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for bazel_besproxy_wrap.py."""

import os
import pathlib
import tempfile
import unittest
from unittest import mock

from build.auth import auth_env
from build.bazel.scripts import (
    bazel_besproxy_wrap,
    bazel_invocation,
    build_utils,
)
from build.resultstore import rsclient


class ShouldWrapWithBesproxyTest(unittest.TestCase):
    def test_empty_env_returns_false(self) -> None:
        self.assertFalse(bazel_besproxy_wrap.should_wrap_with_besproxy({}))

    def test_zero_or_empty_returns_false(self) -> None:
        self.assertFalse(
            bazel_besproxy_wrap.should_wrap_with_besproxy(
                {rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: ""}
            )
        )
        self.assertFalse(
            bazel_besproxy_wrap.should_wrap_with_besproxy(
                {rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: "0"}
            )
        )

    def test_set_returns_true(self) -> None:
        self.assertTrue(
            bazel_besproxy_wrap.should_wrap_with_besproxy(
                {rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: "1"}
            )
        )


class GenerateBesproxyPrefixTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.rsclient_bin = rsclient.bin_dir(
            self.fuchsia_dir, build_utils.get_host_tag()
        )
        self.rsclient_bin.mkdir(parents=True)
        (self.rsclient_bin / rsclient.BESPROXY_WRAP_SH).write_text(
            "#!/bin/sh\n"
        )
        (self.rsclient_bin / rsclient.BESPROXY_BIN).write_text("#!/bin/sh\n")
        (self.rsclient_bin / rsclient.RS_BAZEL_SH).write_text("#!/bin/sh\n")
        self.log_dir = self.fuchsia_dir / "logs"
        self.log_dir.mkdir(parents=True)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_default_prefix_with_sink_mode(self) -> None:
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env={auth_env.ENV_RS_AUTH_TYPE: "none"},
            )
        )
        expected = [
            str(self.rsclient_bin / "besproxy-wrap.sh"),
            "--besproxy",
            str(self.rsclient_bin / "besproxy"),
            "--log-dir",
            str(self.log_dir / "besproxy"),
            "--besproxy_options",
            "--cas_backend=remotebuildexecution.googleapis.com:443",
            "--service_no_auth",
            "--bes_sink",
            "--",
            str(self.rsclient_bin / "rs-bazel.sh"),
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_rbe_and_resultstore_sockets(self) -> None:
        env = {
            bazel_invocation.ENV_BAZEL_RBE_SOCKET: "/tmp/rbe.sock",
            bazel_invocation.ENV_BAZEL_RESULTSTORE_SOCKET: "/tmp/rs.sock",
            bazel_invocation.ENV_RESULTSTORE_BAZEL: "resultstore_infra",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        expected = [
            str(self.rsclient_bin / "besproxy-wrap.sh"),
            "--besproxy",
            str(self.rsclient_bin / "besproxy"),
            "--log-dir",
            str(self.log_dir / "besproxy"),
            "--besproxy_options",
            "--cas_backend=remotebuildexecution.googleapis.com:443",
            "--cas_proxy=unix:///tmp/rbe.sock",
            "--cas_tls_server_name=remotebuildexecution.googleapis.com",
            "--service_no_auth",
            "--bes_backend=buildeventservice.googleapis.com:443",
            "--bes_proxy=unix:///tmp/rs.sock",
            "--bes_tls_server_name=buildeventservice.googleapis.com",
            "--",
            str(self.rsclient_bin / "rs-bazel.sh"),
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_bes_socket(self) -> None:
        env = {
            bazel_invocation.ENV_BAZEL_RBE_SOCKET: "/tmp/rbe.sock",
            bazel_invocation.ENV_BAZEL_BES_SOCKET: "/tmp/bes.sock",
            bazel_invocation.ENV_RESULTSTORE_BAZEL: "resultstore_infra",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        expected = [
            str(self.rsclient_bin / "besproxy-wrap.sh"),
            "--besproxy",
            str(self.rsclient_bin / "besproxy"),
            "--log-dir",
            str(self.log_dir / "besproxy"),
            "--besproxy_options",
            "--cas_backend=remotebuildexecution.googleapis.com:443",
            "--cas_proxy=unix:///tmp/rbe.sock",
            "--cas_tls_server_name=remotebuildexecution.googleapis.com",
            "--service_no_auth",
            "--bes_backend=buildeventservice.googleapis.com:443",
            "--bes_proxy=unix:///tmp/bes.sock",
            "--bes_tls_server_name=buildeventservice.googleapis.com",
            "--",
            str(self.rsclient_bin / "rs-bazel.sh"),
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_resultstore_enabled_without_socket(self) -> None:
        env = {
            bazel_invocation.ENV_RESULTSTORE_BAZEL: "resultstore",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        expected = [
            str(self.rsclient_bin / "besproxy-wrap.sh"),
            "--besproxy",
            str(self.rsclient_bin / "besproxy"),
            "--log-dir",
            str(self.log_dir / "besproxy"),
            "--besproxy_options",
            "--cas_backend=remotebuildexecution.googleapis.com:443",
            "--service_no_auth",
            "--bes_backend=buildeventservice.googleapis.com:443",
            "--bes_tls_server_name=buildeventservice.googleapis.com",
            "--",
            str(self.rsclient_bin / "rs-bazel.sh"),
        ]
        self.assertEqual(res, expected)

    def test_prefix_with_none(self) -> None:
        env = {auth_env.ENV_RS_AUTH_TYPE: "none"}
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        self.assertIn("--service_no_auth", res)

    def test_prefix_with_oauth(self) -> None:
        env = {auth_env.ENV_RS_AUTH_TYPE: "oauth"}
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        self.assertIn("--use_application_default_credentials", res)

    def test_prefix_with_machine(self) -> None:
        env = {auth_env.ENV_RS_AUTH_TYPE: "machine"}
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        self.assertIn("--use_gce_credentials", res)

    def test_prefix_with_loas(self) -> None:
        tag = build_utils.get_host_tag()
        credshelper = auth_env.reclient_credshelper_path(self.fuchsia_dir, tag)
        credshelper.parent.mkdir(parents=True, exist_ok=True)
        credshelper.write_text("#!/bin/sh\n")

        env = {auth_env.ENV_RS_AUTH_TYPE: "loas"}
        res = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                log_dir=self.log_dir,
                env=env,
            )
        )
        self.assertIn(f"--credentials_helper={credshelper}", res)
        self.assertIn(
            "--credentials_helper_args=--auth_source=automaticAuth --gcert_refresh_timeout=10",
            res,
        )


class ComposeBesproxyCommandTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.log_dir = self.fuchsia_dir / "logs"
        self.log_dir.mkdir(parents=True)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_inactive_returns_original_command(self) -> None:
        cmd = ["bazel", "build", "//..."]
        res = bazel_besproxy_wrap.compose_besproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            log_dir=self.log_dir,
            env={"CUSTOM": "value"},
        )
        self.assertEqual(res.command, cmd)
        self.assertEqual(res.env, {"CUSTOM": "value"})

    def test_active_prepends_generated_prefix(self) -> None:
        cmd = ["bazel", "build", "//src:foo"]
        env = {
            rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: "1",
            bazel_invocation.ENV_RESULTSTORE_BAZEL: "0",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        res = bazel_besproxy_wrap.compose_besproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            log_dir=self.log_dir,
            env=env,
        )
        expected_prefix = list(
            bazel_besproxy_wrap._generate_besproxy_prefix(
                self.fuchsia_dir, env=env, log_dir=self.log_dir
            )
        )
        self.assertEqual(res.command, expected_prefix + cmd)
        self.assertEqual(res.env, env)

    def test_compose_besproxy_command_removes_pythonpath(self) -> None:
        cmd = ["bazel", "build", "//..."]
        env = {
            "PYTHONPATH": "/usr/local/fuchsia",
            "CUSTOM": "value",
        }
        res = bazel_besproxy_wrap.compose_besproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
        )
        self.assertNotIn("PYTHONPATH", res.env)
        self.assertEqual(res.env["CUSTOM"], "value")


class ParseArgsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.log_dir = self.fuchsia_dir / "logs"
        self.log_dir.mkdir(parents=True)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_valid_args_with_command(self) -> None:
        argv = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--log-dir",
            str(self.log_dir),
            "--",
            "bazel",
            "build",
            ":target",
        ]
        args, cmd = bazel_besproxy_wrap._parse_args(argv)
        self.assertEqual(args.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(args.log_dir, self.log_dir)
        self.assertEqual(cmd, ["bazel", "build", ":target"])

    def test_missing_command_raises_system_exit(self) -> None:
        with self.assertRaises(SystemExit):
            bazel_besproxy_wrap._parse_args(["--fuchsia-dir", "/tmp"])


class MainTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    @mock.patch.object(os, "execvpe")
    def test_main_delegates_to_execvpe(self, mock_execvpe: mock.Mock) -> None:
        argv = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--",
            "bazel",
            "version",
        ]
        with mock.patch.dict(
            os.environ,
            {rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: "0"},
            clear=False,
        ):
            ret = bazel_besproxy_wrap.main(argv)
        self.assertEqual(ret, 0)
        expected_env = dict(os.environ)
        expected_env.pop("PYTHONPATH", None)
        expected_env[rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY] = "0"
        mock_execvpe.assert_called_once_with(
            "bazel", ["bazel", "version"], expected_env
        )

    @mock.patch.object(os, "execvpe")
    def test_main_delegates_to_wrapped_when_active(
        self, mock_execvpe: mock.Mock
    ) -> None:
        argv = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--",
            "bazel",
            "version",
        ]
        env = {
            rsclient.ENV_FX_INTERNAL_BAZEL_BESPROXY: "1",
            bazel_invocation.ENV_RESULTSTORE_BAZEL: "0",
        }
        with mock.patch.dict(os.environ, env, clear=False):
            ret = bazel_besproxy_wrap.main(argv)
        self.assertEqual(ret, 0)
        mock_execvpe.assert_called_once()
        called_cmd = mock_execvpe.call_args[0][1]
        self.assertIn("besproxy-wrap.sh", called_cmd[0])


if __name__ == "__main__":
    unittest.main()
