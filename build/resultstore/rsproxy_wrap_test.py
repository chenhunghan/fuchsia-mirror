# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.resultstore.rsproxy_wrap."""

import pathlib
import tempfile
import unittest
from unittest import mock

from build.auth import auth_env, select_auth_method
from build.resultstore import rs_profile, rsproxy_wrap


class IsResultstoreEnabledTest(unittest.TestCase):
    def test_disabled_by_default(self) -> None:
        self.assertFalse(rsproxy_wrap._is_resultstore_enabled({}))

    def test_disabled_when_zero(self) -> None:
        self.assertFalse(
            rsproxy_wrap._is_resultstore_enabled(
                {rs_profile.ENV_RESULTSTORE_NINJA: "0"}
            )
        )

    def test_enabled_via_env(self) -> None:
        self.assertTrue(
            rsproxy_wrap._is_resultstore_enabled(
                {rs_profile.ENV_RESULTSTORE_NINJA: "1"}
            )
        )

    def test_enabled_via_rs_service(self) -> None:
        self.assertTrue(
            rsproxy_wrap._is_resultstore_enabled(
                {"RS_rs_service": "unix:///tmp/rs.sock"}
            )
        )


class ExtractSubbuildBaseTest(unittest.TestCase):
    def test_default_returns_top(self) -> None:
        self.assertEqual(
            rsproxy_wrap._extract_subbuild_base(["ninja", "target"]), "top"
        )

    def test_separate_c_flag(self) -> None:
        self.assertEqual(
            rsproxy_wrap._extract_subbuild_base(
                ["ninja", "-C", "out/core.x64-release", "target"]
            ),
            "core.x64-release",
        )

    def test_combined_c_flag(self) -> None:
        self.assertEqual(
            rsproxy_wrap._extract_subbuild_base(
                ["ninja", "-Cout/subbuild_arm64", "target"]
            ),
            "subbuild_arm64",
        )


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
        auth = rsproxy_wrap._resolve_auth_type(
            auth_type="loas",
            use_machine_credentials=True,
            env={},
            fuchsia_dir=self.fuchsia_dir,
        )
        self.assertEqual(auth, "machine")

    def test_explicit_valid_auth(self) -> None:
        for val in ("loas", "oauth", "machine", "none"):
            auth = rsproxy_wrap._resolve_auth_type(
                auth_type=val,
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )
            self.assertEqual(auth, val)

    def test_auto_resolved_from_env(self) -> None:
        env = {auth_env.ENV_RS_AUTH_TYPE: "oauth"}
        auth = rsproxy_wrap._resolve_auth_type(
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
            auth = rsproxy_wrap._resolve_auth_type(
                auth_type="auto",
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )
            self.assertEqual(auth, "loas")

    def test_invalid_auth_type_raises(self) -> None:
        with self.assertRaises(ValueError):
            rsproxy_wrap._resolve_auth_type(
                auth_type="invalid_type",
                use_machine_credentials=False,
                env={},
                fuchsia_dir=self.fuchsia_dir,
            )


class ComposeRsproxyCommandTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.bin_dir = (
            self.fuchsia_dir / "prebuilt" / "rsclient" / "test-platform" / "bin"
        )
        self.bin_dir.mkdir(parents=True)
        self.log_dir = self.fuchsia_dir / "logs"
        self.log_dir.mkdir()

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_disabled_returns_original_command(self) -> None:
        cmd = ["ninja", "default"]
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env={},
            host_tag="test-platform",
        )
        self.assertEqual(wrapped.command, cmd)

    def test_loas_auth_flags(self) -> None:
        cmd = ["ninja", "default"]
        env = {
            rs_profile.ENV_RESULTSTORE_NINJA: "1",
            auth_env.ENV_RS_AUTH_TYPE: "loas",
        }
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
            log_dir=self.log_dir,
            host_tag="test-platform",
        )
        self.assertIn("--rsproxy_options", wrapped.command)
        self.assertTrue(any("--cfg=" in x for x in wrapped.command))
        self.assertTrue(
            any("--credentials_helper=" in x for x in wrapped.command)
        )
        self.assertIn(str(self.bin_dir / "rsproxy-wrap.sh"), wrapped.command[0])

    def test_oauth_auth_flags(self) -> None:
        cmd = ["ninja", "default"]
        env = {
            rs_profile.ENV_RESULTSTORE_NINJA: "1",
            auth_env.ENV_RS_AUTH_TYPE: "oauth",
        }
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
            host_tag="test-platform",
        )
        self.assertTrue(
            any("fuchsia-resultstore.cfg" in x for x in wrapped.command)
        )

    def test_machine_auth_env(self) -> None:
        cmd = ["ninja", "default"]
        env = {
            rs_profile.ENV_RESULTSTORE_NINJA: "1",
            auth_env.ENV_RS_AUTH_TYPE: "machine",
        }
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
            host_tag="test-platform",
        )
        self.assertEqual(
            wrapped.env.get("RS_use_application_default_credentials"), "false"
        )
        self.assertEqual(wrapped.env.get("RS_use_gce_credentials"), "true")

    def test_unix_service_socket_tls_overrides(self) -> None:
        cmd = ["ninja", "default"]
        env = {
            rs_profile.ENV_RESULTSTORE_NINJA: "1",
            "RS_rs_service": "unix:///tmp/rs.sock",
            "RS_cas_service": "unix:///tmp/cas.sock",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
            host_tag="test-platform",
        )
        self.assertIn(
            "--rs_tls_server_name=resultstore.googleapis.com", wrapped.command
        )
        self.assertIn(
            "--cas_tls_server_name=remotebuildexecution.googleapis.com",
            wrapped.command,
        )

    def test_pre_and_post_build_uploads(self) -> None:
        cmd = ["ninja", "default"]
        env = {
            rs_profile.ENV_RESULTSTORE_NINJA: "1",
            auth_env.ENV_RS_AUTH_TYPE: "none",
        }
        wrapped = rsproxy_wrap._compose_rsproxy_command(
            cmd,
            fuchsia_dir=self.fuchsia_dir,
            env=env,
            pre_build_uploads=["/tmp/args.gn"],
            post_build_uploads=["/tmp/metrics.json"],
            host_tag="test-platform",
        )
        self.assertIn("/tmp/args.gn", wrapped.command)
        self.assertIn("/tmp/metrics.json", wrapped.command)


class GenerateRsproxyPrefixTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self.tmpdir.name)
        self.bin_dir = (
            self.fuchsia_dir / "prebuilt" / "rsclient" / "test-platform" / "bin"
        )
        self.bin_dir.mkdir(parents=True)
        self.log_dir = self.fuchsia_dir / "logs"
        self.log_dir.mkdir()

    def tearDown(self) -> None:
        self.tmpdir.cleanup()

    def test_default_prefix(self) -> None:
        prefix = list(
            rsproxy_wrap.generate_rsproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                env={},
                log_dir=self.log_dir,
                auth_type=auth_env.NONE,
                host_tag="test-platform",
            )
        )
        self.assertEqual(prefix[0], str(self.bin_dir / "rsproxy-wrap.sh"))
        self.assertIn("--rsproxy", prefix)
        self.assertIn(str(self.bin_dir / "rsproxy"), prefix)
        self.assertIn("--log-dir", prefix)
        self.assertIn(str(self.log_dir), prefix)
        self.assertEqual(prefix[-1], "--")

    def test_loas_prefix(self) -> None:
        prefix = list(
            rsproxy_wrap.generate_rsproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                env={},
                auth_type=auth_env.LOAS,
                host_tag="test-platform",
            )
        )
        self.assertTrue(any("--cfg=" in x for x in prefix))
        self.assertTrue(any("--credentials_helper=" in x for x in prefix))

    def test_unix_service_socket_tls_overrides(self) -> None:
        env = {
            "RS_rs_service": "unix:///tmp/rs.sock",
            "RS_cas_service": "unix:///tmp/cas.sock",
        }
        prefix = list(
            rsproxy_wrap.generate_rsproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                env=env,
                auth_type=auth_env.NONE,
                host_tag="test-platform",
            )
        )
        self.assertIn("--rs_tls_server_name=resultstore.googleapis.com", prefix)
        self.assertIn(
            "--cas_tls_server_name=remotebuildexecution.googleapis.com",
            prefix,
        )

    def test_pre_and_post_build_uploads(self) -> None:
        prefix = list(
            rsproxy_wrap.generate_rsproxy_prefix(
                fuchsia_dir=self.fuchsia_dir,
                env={},
                auth_type=auth_env.NONE,
                pre_build_uploads=["/tmp/pre.txt"],
                post_build_uploads=["/tmp/post.txt"],
                host_tag="test-platform",
            )
        )
        self.assertIn("/tmp/pre.txt", prefix)
        self.assertIn("/tmp/post.txt", prefix)


class RsproxyEnvTest(unittest.TestCase):
    def test_machine_auth_env(self) -> None:
        env = rsproxy_wrap._rsproxy_env(
            base_env={},
            auth_type=auth_env.MACHINE,
        )
        self.assertEqual(
            env.get("RS_use_application_default_credentials"), "false"
        )
        self.assertEqual(env.get("RS_use_gce_credentials"), "true")

    def test_log_dir_env(self) -> None:
        env = rsproxy_wrap._rsproxy_env(
            base_env={},
            log_dir=pathlib.Path("/tmp/rsproxy_logs"),
        )
        self.assertEqual(env.get("RS_log_dir"), "/tmp/rsproxy_logs")

    def test_gce_metadata_host_env(self) -> None:
        env = rsproxy_wrap._rsproxy_env(
            base_env={"GCE_METADATA_HOST": "localhost:8080"},
        )
        self.assertEqual(env.get("GCE_METADATA_HOST"), "127.0.0.1:8080")

    def test_python_bindir_in_path(self) -> None:
        py_dir = pathlib.Path("/custom/python/bin")
        env = rsproxy_wrap._rsproxy_env(
            base_env={"PATH": "/usr/bin"},
            python_dir=py_dir,
        )
        self.assertEqual(env.get("PATH"), "/custom/python/bin:/usr/bin")


class ParseArgsTest(unittest.TestCase):
    def test_parse_options_and_command(self) -> None:
        argv = [
            "--auth-type",
            "oauth",
            "--log-dir",
            "/tmp/logs",
            "--unknown-proxy-flag",
            "--",
            "ninja",
            "target",
        ]
        args, extra_opts, cmd = rsproxy_wrap._parse_args(argv)
        self.assertEqual(args.auth_type, "oauth")
        self.assertEqual(args.log_dir, pathlib.Path("/tmp/logs"))
        self.assertEqual(extra_opts, ["--unknown-proxy-flag"])
        self.assertEqual(cmd, ["ninja", "target"])


class MainTest(unittest.TestCase):
    def test_missing_command_returns_error(self) -> None:
        with mock.patch("sys.stderr.write"):
            ret = rsproxy_wrap.main(["--log-dir", "/tmp/logs"], {})
            self.assertEqual(ret, 2)


if __name__ == "__main__":
    unittest.main()
