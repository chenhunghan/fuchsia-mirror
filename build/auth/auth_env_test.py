# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.auth.auth_env."""

import contextlib
import io
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from build.auth import (
    auth_env,
    gcloud,
    gcloud_creds,
    loas,
    select_auth_method,
)


class AuthTypeTest(unittest.TestCase):
    def test_values_and_str(self) -> None:
        self.assertEqual(str(auth_env.AuthType.LOAS), "loas")
        self.assertEqual(str(auth_env.AuthType.OAUTH), "oauth")
        self.assertEqual(str(auth_env.AuthType.MACHINE), "machine")
        self.assertEqual(str(auth_env.AuthType.NONE), "none")
        self.assertEqual(str(auth_env.AuthType.AUTO), "auto")

    def test_top_level_aliases(self) -> None:
        self.assertIs(auth_env.LOAS, auth_env.AuthType.LOAS)
        self.assertIs(auth_env.OAUTH, auth_env.AuthType.OAUTH)
        self.assertIs(auth_env.MACHINE, auth_env.AuthType.MACHINE)
        self.assertIs(auth_env.NONE, auth_env.AuthType.NONE)
        self.assertIs(auth_env.AUTO, auth_env.AuthType.AUTO)


class MakeAuthEnvTest(unittest.TestCase):
    def test_defaults(self) -> None:
        env = auth_env.make_auth_env()
        self.assertEqual(
            env,
            {
                auth_env.ENV_RBE_AUTH_TYPE: "none",
                auth_env.ENV_RS_AUTH_TYPE: "none",
            },
        )

    def test_populated(self) -> None:
        env = auth_env.make_auth_env(
            rbe_auth=auth_env.LOAS,
            rs_auth="oauth",
            user="test-user",
            credentials_file=Path("/tmp/creds.json"),
        )
        self.assertEqual(
            env,
            {
                auth_env.ENV_RBE_AUTH_TYPE: "loas",
                auth_env.ENV_RS_AUTH_TYPE: "oauth",
                auth_env.ENV_USER: "test-user",
                auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/tmp/creds.json",
            },
        )


class RbeAuthTypeTest(unittest.TestCase):
    def test_missing_returns_none(self) -> None:
        self.assertEqual(auth_env.rbe_auth_type({}), "none")

    def test_present_returns_value(self) -> None:
        self.assertEqual(
            auth_env.rbe_auth_type({auth_env.ENV_RBE_AUTH_TYPE: "loas"}),
            "loas",
        )


class RsAuthTypeTest(unittest.TestCase):
    def test_missing_returns_none(self) -> None:
        self.assertEqual(auth_env.rs_auth_type({}), "none")

    def test_present_returns_value(self) -> None:
        self.assertEqual(
            auth_env.rs_auth_type({auth_env.ENV_RS_AUTH_TYPE: "oauth"}),
            "oauth",
        )


class IsLoasTest(unittest.TestCase):
    def test_empty_env_returns_false(self) -> None:
        self.assertFalse(auth_env.is_loas({}))

    def test_rbe_loas_returns_true(self) -> None:
        self.assertTrue(auth_env.is_loas({auth_env.ENV_RBE_AUTH_TYPE: "loas"}))

    def test_rs_loas_returns_true(self) -> None:
        self.assertTrue(auth_env.is_loas({auth_env.ENV_RS_AUTH_TYPE: "loas"}))

    def test_non_loas_returns_false(self) -> None:
        env = {
            auth_env.ENV_RBE_AUTH_TYPE: "oauth",
            auth_env.ENV_RS_AUTH_TYPE: "machine",
        }
        self.assertFalse(auth_env.is_loas(env))


class RequiresGcertTest(unittest.TestCase):
    def test_empty_env_returns_false(self) -> None:
        self.assertFalse(auth_env.requires_gcert({}))

    def test_rbe_loas_returns_true(self) -> None:
        self.assertTrue(
            auth_env.requires_gcert({auth_env.ENV_RBE_AUTH_TYPE: "loas"})
        )

    def test_rs_loas_returns_true(self) -> None:
        self.assertTrue(
            auth_env.requires_gcert({auth_env.ENV_RS_AUTH_TYPE: "loas"})
        )

    def test_non_loas_returns_false(self) -> None:
        env = {
            auth_env.ENV_RBE_AUTH_TYPE: "oauth",
            auth_env.ENV_RS_AUTH_TYPE: "machine",
        }
        self.assertFalse(auth_env.requires_gcert(env))


class IsMachineTest(unittest.TestCase):
    def test_empty_env_returns_false(self) -> None:
        self.assertFalse(auth_env.is_machine({}))

    def test_rbe_machine_returns_true(self) -> None:
        self.assertTrue(
            auth_env.is_machine({auth_env.ENV_RBE_AUTH_TYPE: "machine"})
        )

    def test_rs_machine_returns_true(self) -> None:
        self.assertTrue(
            auth_env.is_machine({auth_env.ENV_RS_AUTH_TYPE: "machine"})
        )

    def test_non_machine_returns_false(self) -> None:
        env = {
            auth_env.ENV_RBE_AUTH_TYPE: "oauth",
            auth_env.ENV_RS_AUTH_TYPE: "loas",
        }
        self.assertFalse(auth_env.is_machine(env))


class AuthPredicatesTest(unittest.TestCase):
    def test_is_user_auth(self) -> None:
        self.assertTrue(auth_env.is_user_auth(auth_env.LOAS))
        self.assertTrue(auth_env.is_user_auth("loas"))
        self.assertTrue(auth_env.is_user_auth(auth_env.OAUTH))
        self.assertTrue(auth_env.is_user_auth("oauth"))
        self.assertFalse(auth_env.is_user_auth(auth_env.MACHINE))
        self.assertFalse(auth_env.is_user_auth(auth_env.NONE))

    def test_uses_adc(self) -> None:
        self.assertTrue(auth_env._uses_adc(auth_env.OAUTH))
        self.assertTrue(auth_env._uses_adc("oauth"))
        self.assertFalse(auth_env._uses_adc(auth_env.LOAS))
        self.assertFalse(auth_env._uses_adc(auth_env.MACHINE))
        self.assertFalse(auth_env._uses_adc(auth_env.NONE))


class SelectQuotaProjectTest(unittest.TestCase):
    def test_rbe_takes_precedence_over_resultstore(self) -> None:
        self.assertEqual(
            auth_env._select_quota_project(
                auth_env.OAUTH,
                auth_env.OAUTH,
                rbe_quota_project="rbe-proj",
                rs_quota_project="rs-proj",
            ),
            "rbe-proj",
        )

    def test_resultstore_selected_when_rbe_uses_loas_or_none(self) -> None:
        for rbe_auth in (auth_env.LOAS, auth_env.NONE):
            with self.subTest(rbe_auth=rbe_auth):
                self.assertEqual(
                    auth_env._select_quota_project(
                        rbe_auth,
                        auth_env.OAUTH,
                        rbe_quota_project="rbe-proj",
                        rs_quota_project="rs-proj",
                    ),
                    "rs-proj",
                )

    def test_non_adc_auth_returns_none(self) -> None:
        self.assertIsNone(
            auth_env._select_quota_project(
                auth_env.LOAS,
                auth_env.LOAS,
                rbe_quota_project="rbe-proj",
                rs_quota_project="rs-proj",
            )
        )
        self.assertIsNone(
            auth_env._select_quota_project(
                auth_env.MACHINE,
                auth_env.NONE,
                rbe_quota_project="rbe-proj",
                rs_quota_project="rs-proj",
            )
        )


class EnsureCredentialsTest(unittest.TestCase):
    @mock.patch.object(loas, "ensure_loas", autospec=True)
    def test_loas_delegates_to_ensure_loas(self, mock_loas: mock.Mock) -> None:
        self.assertIsNone(
            auth_env.ensure_credentials(auth_env.LOAS, force=True)
        )
        mock_loas.assert_called_once_with(force=True, env=None)

    @mock.patch.object(
        loas,
        "ensure_loas",
        autospec=True,
        side_effect=loas.LoasError("gcert failed"),
    )
    def test_loas_error_raises_runtime_error(
        self, _mock_loas: mock.Mock
    ) -> None:
        with self.assertRaises(RuntimeError):
            auth_env.ensure_credentials(auth_env.LOAS)

    @mock.patch.object(
        gcloud_creds,
        "ensure_oauth",
        autospec=True,
        return_value=Path("/tmp/out/adc.json"),
    )
    def test_oauth_delegates_to_ensure_oauth(
        self, mock_oauth: mock.Mock
    ) -> None:
        self.assertEqual(
            auth_env.ensure_credentials(
                auth_env.OAUTH,
                out_dir=Path("/tmp/out"),
                quota_project="rbe-proj",
                gcloud_args=("--no-browser",),
            ),
            Path("/tmp/out/adc.json"),
        )
        mock_oauth.assert_called_once_with(
            force=False,
            out_dir=Path("/tmp/out"),
            quota_project="rbe-proj",
            gcloud_args=("--no-browser",),
        )

    def test_machine_and_none_return_none(self) -> None:
        self.assertIsNone(auth_env.ensure_credentials(auth_env.MACHINE))
        self.assertIsNone(auth_env.ensure_credentials(auth_env.NONE))

    def test_unhandled_type_raises_value_error(self) -> None:
        with self.assertRaises(ValueError):
            auth_env.ensure_credentials("bogus")


class ResolveCredentialsFileTest(unittest.TestCase):
    def test_default_adc_resolution(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home_path = Path(temp_dir)
            with mock.patch.object(
                Path, "home", autospec=True, return_value=home_path
            ):
                self.assertIsNone(
                    auth_env.resolve_credentials_file(
                        auth_env.OAUTH,
                        auth_env.NONE,
                        out_dir=home_path / "out",
                        env={},
                    )
                )
                default_adc = home_path / gcloud.ADC_SUBPATH
                default_adc.parent.mkdir(parents=True, exist_ok=True)
                default_adc.write_text("{}")
                self.assertEqual(
                    auth_env.resolve_credentials_file(
                        auth_env.OAUTH,
                        auth_env.NONE,
                        out_dir=home_path / "out",
                        env={},
                    ),
                    default_adc,
                )

    @mock.patch.object(
        gcloud_creds,
        "isolate_credentials_safe",
        autospec=True,
        return_value=Path("/tmp/out/adc.json"),
    )
    def test_isolates_with_quota_project(self, mock_isolate: mock.Mock) -> None:
        resolved_path = auth_env.resolve_credentials_file(
            auth_env.OAUTH,
            auth_env.NONE,
            out_dir=Path("/tmp/out"),
            rbe_quota_project="rbe-proj",
            env={},
        )
        self.assertEqual(resolved_path, Path("/tmp/out/adc.json"))
        mock_isolate.assert_called_once_with(
            out_dir=Path("/tmp/out"),
            quota_project="rbe-proj",
            source_credentials_path=None,
        )

    def test_loas_returns_none_even_with_stale_adc(self) -> None:
        self.assertIsNone(
            auth_env.resolve_credentials_file(
                auth_env.LOAS,
                auth_env.LOAS,
                out_dir=Path("/tmp/out"),
                rbe_quota_project="rbe-proj",
                env={
                    auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/tmp/stale_adc.json"
                },
            )
        )

    @mock.patch.object(
        gcloud_creds,
        "isolate_credentials_safe",
        autospec=True,
        return_value=Path("/tmp/out/rs_adc.json"),
    )
    def test_loas_rbe_with_oauth_resultstore_isolates_rs_quota_project(
        self, mock_rs_isolate: mock.Mock
    ) -> None:
        self.assertEqual(
            auth_env.resolve_credentials_file(
                auth_env.LOAS,
                auth_env.OAUTH,
                out_dir=Path("/tmp/out"),
                rbe_quota_project="rbe-proj",
                rs_quota_project="rs-proj",
                env={},
            ),
            Path("/tmp/out/rs_adc.json"),
        )
        mock_rs_isolate.assert_called_once_with(
            out_dir=Path("/tmp/out"),
            quota_project="rs-proj",
            source_credentials_path=None,
        )

    def test_machine_returns_explicit_credentials(self) -> None:
        self.assertEqual(
            auth_env.resolve_credentials_file(
                auth_env.MACHINE,
                auth_env.NONE,
                out_dir=Path("/tmp/out"),
                env={auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/sa.json"},
            ),
            Path("/sa.json"),
        )


class AuthEnvCliTest(unittest.TestCase):
    @mock.patch.object(
        auth_env,
        "ensure_credentials",
        autospec=True,
        return_value=Path("/tmp/out/adc.json"),
    )
    def test_ensure_subcommand(self, mock_ensure: mock.Mock) -> None:
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = auth_env.main(
                [
                    "ensure",
                    "--auth-type=oauth",
                    "--quota-project=rbe-proj",
                    "--force",
                    "--",
                    "--no-browser",
                ],
                {},
            )
        self.assertEqual(exit_code, 0)
        self.assertEqual(stdout.getvalue(), "")
        mock_ensure.assert_called_once_with(
            "oauth",
            force=True,
            out_dir=None,
            quota_project="rbe-proj",
            gcloud_args=["--no-browser"],
            env={},
        )

    @mock.patch.object(
        auth_env,
        "resolve_credentials_file",
        autospec=True,
        return_value=Path("/tmp/out/adc.json"),
    )
    def test_resolve_creds_subcommand(self, mock_resolve: mock.Mock) -> None:
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exit_code = auth_env.main(
                (
                    "resolve-creds",
                    "--auth-type=oauth",
                    "--out-dir=/tmp/out",
                    "--quota-project=rbe-proj",
                ),
                {},
            )
        self.assertEqual(exit_code, 0)
        self.assertEqual(stdout.getvalue().strip(), "/tmp/out/adc.json")
        mock_resolve.assert_called_once_with(
            "oauth",
            auth_env.AuthType.NONE,
            out_dir=Path("/tmp/out").resolve(),
            rbe_quota_project="rbe-proj",
            env={},
        )

    def test_invalid_auth_type_choice_exits(self) -> None:
        stderr = io.StringIO()
        with (
            contextlib.redirect_stderr(stderr),
            self.assertRaises(SystemExit),
        ):
            auth_env.main(("ensure", "--auth-type=auto"), {})


class CredentialsFileTest(unittest.TestCase):
    def test_missing_returns_none(self) -> None:
        self.assertIsNone(auth_env.credentials_file({}))

    def test_present_returns_path(self) -> None:
        self.assertEqual(
            auth_env.credentials_file(
                {auth_env.ENV_GOOGLE_APPLICATION_CREDENTIALS: "/path/to/creds"}
            ),
            Path("/path/to/creds"),
        )


class ReclientCredshelperPathTest(unittest.TestCase):
    def test_path_construction(self) -> None:
        fuchsia_dir = Path("/fuchsia/root")
        path = auth_env.reclient_credshelper_path(fuchsia_dir, "linux-x64")
        self.assertEqual(
            path,
            Path(
                "/fuchsia/root/prebuilt/third_party/reclient/linux-x64/credshelper"
            ),
        )


class DetectSystemAuthTypeTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        auth_env.detect_system_auth_type.cache_clear()
        self.addCleanup(auth_env.detect_system_auth_type.cache_clear)

    def test_detection_caches_result(self) -> None:
        with mock.patch.object(
            select_auth_method,
            "select_auth_method",
            return_value=select_auth_method.AuthType.LOAS,
        ) as mock_select:
            res1 = auth_env.detect_system_auth_type()
            res2 = auth_env.detect_system_auth_type()
            self.assertEqual(res1, auth_env.AuthType.LOAS)
            self.assertEqual(res2, auth_env.AuthType.LOAS)
            self.assertEqual(mock_select.call_count, 1)


class CachedAuthTypeTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.fuchsia_dir = Path(temp_dir.name)
        self.config = self.fuchsia_dir / ".fx/config/build-auth"
        self.config.parent.mkdir(parents=True)

    def test_returns_cached_value(self) -> None:
        self.config.write_text("# comment\nauth_type=oauth\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "oauth")

    def test_missing_file_or_key(self) -> None:
        self.assertIsNone(auth_env.cached_auth_type(self.fuchsia_dir))
        self.config.write_text("other_key=value\n")
        self.assertIsNone(auth_env.cached_auth_type(self.fuchsia_dir))

    def test_legacy_loas_cert_type_fallback(self) -> None:
        self.config.write_text("loas_cert_type=unrestricted\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "loas")

        self.config.write_text("loas_cert_type=restricted\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "oauth")

        self.config.write_text("loas_cert_type=unknown\n")
        self.assertIsNone(auth_env.cached_auth_type(self.fuchsia_dir))

        self.config.write_text("loas_cert_type=unrestricted\nauth_type=oauth\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "oauth")

        self.config.write_text("auth_type=oauth\nloas_cert_type=unrestricted\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "oauth")

        self.config.write_text(
            "auth_type=auto\nloas_cert_type=unrestricted\n# trailing\n"
        )
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "loas")

    def test_strips_whitespace_comments_and_quotes(self) -> None:
        for content in (
            "auth_type = oauth\n",
            "auth_type=oauth  # set by hand\n",
            "auth_type='oauth'\n",
            ' auth_type="oauth" \n',
        ):
            self.config.write_text(content)
            self.assertEqual(
                auth_env.cached_auth_type(self.fuchsia_dir), "oauth", content
            )

    def test_invalid_utf8(self) -> None:
        self.config.write_bytes(b"# \xff\nauth_type=loas\n")
        self.assertEqual(auth_env.cached_auth_type(self.fuchsia_dir), "loas")


class ResolveAndCacheAuthTypeTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.fuchsia_dir = Path(temp_dir.name)
        self.config = self.fuchsia_dir / ".fx/config/build-auth"

    @mock.patch.object(select_auth_method, "select_auth_method", autospec=True)
    def test_cached_hit_skips_probe(self, mock_select: mock.Mock) -> None:
        self.config.parent.mkdir(parents=True)
        self.config.write_text("auth_type=loas\n", encoding="utf-8")

        resolved = auth_env.resolve_and_cache_auth_type(
            self.fuchsia_dir, env={}
        )
        self.assertEqual(resolved, "loas")
        mock_select.assert_not_called()

    @mock.patch.object(select_auth_method, "select_auth_method", autospec=True)
    def test_migrates_legacy_auth_config_file_and_loas_cert_type(
        self, mock_select: mock.Mock
    ) -> None:
        legacy_file = self.fuchsia_dir / ".fx/auth-config"
        legacy_file.parent.mkdir(parents=True)
        legacy_file.write_text(
            "loas_cert_type=unrestricted\n", encoding="utf-8"
        )
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            resolved = auth_env.resolve_and_cache_auth_type(
                self.fuchsia_dir, env={}
            )
        self.assertEqual(resolved, "loas")
        self.assertFalse(legacy_file.exists())
        self.assertEqual(
            self.config.read_text(encoding="utf-8"), "auth_type=loas\n"
        )
        self.assertIn("Moving", stderr.getvalue())
        mock_select.assert_not_called()

    @mock.patch.object(select_auth_method, "select_auth_method", autospec=True)
    def test_rewrites_restricted_loas_cert_type_to_oauth(
        self, mock_select: mock.Mock
    ) -> None:
        self.config.parent.mkdir(parents=True)
        self.config.write_text("loas_cert_type=restricted\n", encoding="utf-8")

        resolved = auth_env.resolve_and_cache_auth_type(
            self.fuchsia_dir, env={}
        )
        self.assertEqual(resolved, "oauth")
        self.assertEqual(
            self.config.read_text(encoding="utf-8"), "auth_type=oauth\n"
        )
        mock_select.assert_not_called()

    @mock.patch.object(
        select_auth_method,
        "select_auth_method",
        autospec=True,
        return_value=select_auth_method.AuthType.OAUTH,
    )
    def test_cache_miss_probes_and_writes_cache(
        self, mock_select: mock.Mock
    ) -> None:
        resolved = auth_env.resolve_and_cache_auth_type(
            self.fuchsia_dir, env={"PATH": "/usr/bin"}
        )
        self.assertEqual(resolved, "oauth")
        self.assertEqual(
            self.config.read_text(encoding="utf-8"), "auth_type=oauth\n"
        )
        mock_select.assert_called_once_with(env={"PATH": "/usr/bin"})

    @mock.patch.object(subprocess, "run", autospec=True)
    @mock.patch.object(
        shutil, "which", autospec=True, return_value="/usr/bin/loas_destroy"
    )
    @mock.patch.object(
        select_auth_method,
        "select_auth_method",
        autospec=True,
        return_value=select_auth_method.AuthType.LOAS,
    )
    def test_force_runs_loas_destroy_and_overwrites_cache(
        self,
        mock_select: mock.Mock,
        mock_which: mock.Mock,
        mock_run: mock.Mock,
    ) -> None:
        self.config.parent.mkdir(parents=True)
        self.config.write_text("auth_type=oauth\n", encoding="utf-8")
        custom_env = {"PATH": "/usr/bin"}

        resolved = auth_env.resolve_and_cache_auth_type(
            self.fuchsia_dir, force=True, env=custom_env
        )
        self.assertEqual(resolved, "loas")
        self.assertEqual(
            self.config.read_text(encoding="utf-8"), "auth_type=loas\n"
        )
        mock_which.assert_called_once_with("loas_destroy", path="/usr/bin")
        mock_run.assert_called_once_with(
            ("/usr/bin/loas_destroy",), env=custom_env, check=False
        )
        mock_select.assert_called_once_with(env=custom_env)


class ResolveBuildAuthTypeTest(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        auth_env.detect_system_auth_type.cache_clear()
        self.addCleanup(auth_env.detect_system_auth_type.cache_clear)

    def test_cached_auth_type_used_when_auto(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fuchsia_dir = Path(temp_dir)
            cfg = fuchsia_dir / ".fx/config/build-auth"
            cfg.parent.mkdir(parents=True)
            cfg.write_text("auth_type=oauth\n")

            with mock.patch.object(
                select_auth_method, "select_auth_method"
            ) as mock_select:
                res = auth_env.resolve_build_auth_type(
                    auth_type="auto", env={}, fuchsia_dir=fuchsia_dir
                )
                mock_select.assert_not_called()
            self.assertEqual(res, "oauth")

            # The inherited parent environment still takes precedence.
            res = auth_env.resolve_build_auth_type(
                auth_type="auto",
                env={auth_env.ENV_RBE_AUTH_TYPE: "loas"},
                fuchsia_dir=fuchsia_dir,
            )
            self.assertEqual(res, "loas")

    def test_cached_auto_falls_back_to_detection(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fuchsia_dir = Path(temp_dir)
            cfg = fuchsia_dir / ".fx/config/build-auth"
            cfg.parent.mkdir(parents=True)
            cfg.write_text("auth_type=auto\n")

            with mock.patch.object(
                select_auth_method,
                "select_auth_method",
                return_value=select_auth_method.AuthType.LOAS,
            ) as mock_select:
                res = auth_env.resolve_build_auth_type(
                    auth_type="auto", env={}, fuchsia_dir=fuchsia_dir
                )
                mock_select.assert_called_once()
            self.assertEqual(res, "loas")

    def test_machine_credentials_precedence(self) -> None:
        res = auth_env.resolve_build_auth_type(
            auth_type=auth_env.LOAS,
            use_machine_credentials=True,
            env={auth_env.ENV_RBE_AUTH_TYPE: "oauth"},
        )
        self.assertEqual(res, "machine")

    def test_explicit_auth_type_used(self) -> None:
        for val in ("loas", "oauth", "machine", "none"):
            res = auth_env.resolve_build_auth_type(
                auth_type=val,
                use_machine_credentials=False,
                env={},
            )
            self.assertEqual(res, val)

    def test_inherited_env_used_when_auto(self) -> None:
        res = auth_env.resolve_build_auth_type(
            auth_type="auto",
            use_machine_credentials=False,
            env={auth_env.ENV_RS_AUTH_TYPE: "oauth"},
            service_env_var=auth_env.ENV_RS_AUTH_TYPE,
        )
        self.assertEqual(res, "oauth")

    def test_system_detection_fallback(self) -> None:
        with (
            mock.patch.object(
                select_auth_method,
                "select_auth_method",
                return_value=select_auth_method.AuthType.LOAS,
            ),
            mock.patch.dict(os.environ, {}, clear=True),
        ):
            res = auth_env.resolve_build_auth_type(
                auth_type="auto",
                use_machine_credentials=False,
                env=None,
            )
            self.assertEqual(res, "loas")

    def test_system_detection_uses_explicit_env(self) -> None:
        custom_env = {"SWARMING_BOT_ID": "bot-123"}
        with mock.patch.object(
            select_auth_method,
            "select_auth_method",
            return_value=select_auth_method.AuthType.MACHINE,
        ) as mock_select:
            res = auth_env.resolve_build_auth_type(
                auth_type="auto",
                use_machine_credentials=False,
                env=custom_env,
            )
            self.assertEqual(res, "machine")
            mock_select.assert_called_once()
            self.assertEqual(mock_select.call_args.kwargs["env"], custom_env)

    def test_invalid_type_raises(self) -> None:
        with self.assertRaises(ValueError):
            auth_env.resolve_build_auth_type(
                auth_type="bogus_type",
                use_machine_credentials=False,
                env={},
            )


class ResolveRbeAuthEnvTest(unittest.TestCase):
    @mock.patch.object(
        auth_env, "resolve_build_auth_type", autospec=True, return_value="loas"
    )
    def test_returns_resolved_auth_env(self, mock_resolve: mock.Mock) -> None:
        res = auth_env.resolve_rbe_auth_env(
            env={"FOO": "bar"}, fuchsia_dir=Path("/fuchsia")
        )
        self.assertEqual(
            res,
            {
                auth_env.ENV_RBE_AUTH_TYPE: "loas",
                auth_env.ENV_RS_AUTH_TYPE: "none",
            },
        )
        mock_resolve.assert_called_once_with(
            env={"FOO": "bar"}, fuchsia_dir=Path("/fuchsia")
        )

    @mock.patch.object(
        auth_env,
        "resolve_build_auth_type",
        autospec=True,
        side_effect=RuntimeError("probe failed"),
    )
    def test_fallback_on_error(self, _mock_resolve: mock.Mock) -> None:
        res = auth_env.resolve_rbe_auth_env(env={})
        self.assertEqual(
            res,
            {
                auth_env.ENV_RBE_AUTH_TYPE: "oauth",
                auth_env.ENV_RS_AUTH_TYPE: "none",
            },
        )


if __name__ == "__main__":
    unittest.main()
