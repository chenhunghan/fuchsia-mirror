#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for select_auth_method.py."""

import contextlib
import io
import pathlib
import tempfile
import unittest
from unittest import mock

from build.auth import gcloud, loas, select_auth_method


class SelectAuthMethodTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.cred_helper = pathlib.Path(self.temp_dir.name) / "credhelper"

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_machine_auth_on_bot(self) -> None:
        env = {"BUILDBUCKET_ID": "123"}
        method = select_auth_method.select_auth_method(
            env=env,
            cred_helper=self.cred_helper,
        )
        self.assertEqual(method, "machine")

    def test_error_when_all_auth_methods_missing(self) -> None:
        env = {"PATH": ""}
        with self.assertRaises(ValueError) as cm:
            select_auth_method.select_auth_method(
                env=env,
                cred_helper=self.cred_helper,
            )
        self.assertIn(
            "We could not find a valid corporate LOAS credential",
            str(cm.exception),
        )

    @mock.patch.object(
        loas,
        "check_loas_restrictions",
        autospec=True,
        return_value=loas.RestrictionType.UNRESTRICTED,
    )
    @mock.patch.object(
        select_auth_method, "_is_executable", autospec=True, return_value=True
    )
    def test_loas_unrestricted_with_helper(
        self, mock_is_exec: mock.Mock, mock_loas: mock.Mock
    ) -> None:
        method = select_auth_method.select_auth_method(
            env={"HOME": self.temp_dir.name},
            cred_helper=self.cred_helper,
        )
        self.assertEqual(method, "loas")

    @mock.patch.object(
        loas,
        "check_loas_restrictions",
        autospec=True,
        return_value=loas.RestrictionType.UNRESTRICTED,
    )
    @mock.patch.object(
        select_auth_method, "_is_executable", autospec=True, return_value=False
    )
    def test_loas_unrestricted_without_helper_falls_back_to_oauth(
        self, mock_is_exec: mock.Mock, mock_loas: mock.Mock
    ) -> None:
        env = {"HOME": self.temp_dir.name}
        adc_file = pathlib.Path(self.temp_dir.name) / gcloud.ADC_SUBPATH
        adc_file.parent.mkdir(parents=True, exist_ok=True)
        adc_file.touch()

        method = select_auth_method.select_auth_method(
            env=env,
            cred_helper=self.cred_helper,
        )
        self.assertEqual(method, "oauth")

    @mock.patch.object(
        loas,
        "check_loas_restrictions",
        autospec=True,
        side_effect=loas.LoasError("missing gcert"),
    )
    def test_oauth_fallback_when_no_loas_but_has_local_creds(
        self, mock_loas: mock.Mock
    ) -> None:
        env = {"HOME": self.temp_dir.name}
        adc_file = pathlib.Path(self.temp_dir.name) / gcloud.ADC_SUBPATH
        adc_file.parent.mkdir(parents=True, exist_ok=True)
        adc_file.touch()

        method = select_auth_method.select_auth_method(
            env=env,
            cred_helper=self.cred_helper,
        )
        self.assertEqual(method, "oauth")

    @mock.patch.object(
        loas,
        "check_loas_restrictions",
        autospec=True,
        return_value=loas.RestrictionType.RESTRICTED,
    )
    def test_loas_restricted_falls_back_to_oauth_or_raises(
        self, mock_loas: mock.Mock
    ) -> None:
        with self.assertRaises(ValueError):
            select_auth_method.select_auth_method(
                env={"HOME": self.temp_dir.name, "PATH": ""},
                cred_helper=self.cred_helper,
            )

        adc_file = pathlib.Path(self.temp_dir.name) / gcloud.ADC_SUBPATH
        adc_file.parent.mkdir(parents=True, exist_ok=True)
        adc_file.touch()
        self.assertEqual(
            select_auth_method.select_auth_method(
                env={"HOME": self.temp_dir.name, "PATH": ""},
                cred_helper=self.cred_helper,
            ),
            "oauth",
        )

    @mock.patch.object(
        loas,
        "check_loas_restrictions",
        autospec=True,
        side_effect=loas.LoasError("missing gcert"),
    )
    def test_oauth_fallback_with_google_application_credentials_env(
        self, _mock_loas: mock.Mock
    ) -> None:
        custom_adc = pathlib.Path(self.temp_dir.name) / "custom_adc.json"
        env = {
            "GOOGLE_APPLICATION_CREDENTIALS": str(custom_adc),
            "PATH": "",
        }
        with self.assertRaises(ValueError):
            select_auth_method.select_auth_method(
                env=env,
                cred_helper=self.cred_helper,
            )

        custom_adc.write_text("{}")
        self.assertEqual(
            select_auth_method.select_auth_method(
                env=env,
                cred_helper=self.cred_helper,
            ),
            "oauth",
        )

    def test_is_user_based_property(self) -> None:
        self.assertTrue(select_auth_method.AuthType.LOAS.is_user_based)
        self.assertTrue(select_auth_method.AuthType.OAUTH.is_user_based)
        self.assertFalse(select_auth_method.AuthType.MACHINE.is_user_based)

    def test_main_machine_auth(self) -> None:
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            rc = select_auth_method.main([], {"BUILDBUCKET_ID": "123"})
        self.assertEqual(rc, 0)
        self.assertEqual(stdout.getvalue().strip(), "machine")


if __name__ == "__main__":
    unittest.main()
