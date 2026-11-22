#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.auth.loas."""

import contextlib
import io
import subprocess
import unittest
from unittest import mock

from build.auth import loas


class CheckLoasRestrictionsTest(unittest.TestCase):
    @mock.patch.object(loas, "shutil", autospec=True)
    def test_missing_tools_raise(self, mock_shutil: mock.Mock) -> None:
        mock_shutil.which.return_value = None
        with self.assertRaises(loas.LoasError):
            loas.check_loas_restrictions(env={"PATH": ""}, platform="linux")

    @mock.patch.object(loas, "shutil", autospec=True)
    def test_non_linux_fails_before_probing_tools(
        self, mock_shutil: mock.Mock
    ) -> None:
        for non_linux in ("darwin", "win32"):
            with self.subTest(platform=non_linux):
                with self.assertRaises(loas.LoasError):
                    loas.check_loas_restrictions(
                        env={"USER": "alice"}, platform=non_linux
                    )
        mock_shutil.which.assert_not_called()

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_failed_renewal_returns_restricted(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/tool"
        mock_subprocess.run.side_effect = [
            subprocess.CompletedProcess((), 1, stdout=""),
            subprocess.CompletedProcess((), 1),
        ]
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            restriction = loas.check_loas_restrictions(
                env={"USER": "alice"}, platform="linux"
            )
        self.assertEqual(restriction, loas.RestrictionType.RESTRICTED)
        self.assertEqual(stderr.getvalue(), "")

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "_is_executable", autospec=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_gcertstatus_loas2_classification(
        self,
        mock_subprocess: mock.Mock,
        mock_exec: mock.Mock,
        mock_shutil: mock.Mock,
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/tool"
        cases = (
            (
                'role: "alice"\n[AcctRestrictions]:  {}\n',
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n  destination_permissions: {\n    type: CORP\n  }\n}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions] {\n  destination_permissions {\n    type: CORP\n  }\n}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {\n"
                "    destination_permissions: {\n"
                "      type: CORP\n"
                "    }\n"
                "    destination_permissions: {\n"
                "      type: PROD\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions] {\n"
                "  destination_restriction {\n"
                "    destination_permissions {\n"
                "      type: PROD\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {\n"
                "    destination_permissions: {\n"
                "      type: CORP\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {\n"
                "    destination_permissions: {\n"
                "      type: PROD\n"
                "    }\n"
                "  }\n"
                "  restricted_role: true\n"
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n  restricted_role: true\n  destination_permissions: {\n    type: CORP\n  }\n}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                "[AcctRestrictions]: {\n  restricted_role: true\n}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            # Missing [AcctRestrictions] block.
            (
                'role: "alice"\n',
                loas.RestrictionType.RESTRICTED,
            ),
            # Extra sibling fields and reordered fields inside [AcctRestrictions]
            # and destination_permissions (CORP).
            (
                "[AcctRestrictions]: {\n"
                '  other_field: "extra"\n'
                "  restricted_role: false\n"
                "  destination_permissions: {\n"
                '    label: "corp_default"\n'
                "    type: CORP\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            # Extra sibling fields and reordered fields inside [AcctRestrictions],
            # destination_restriction, and destination_permissions (PROD).
            (
                "[AcctRestrictions]: {\n"
                '  metadata: "before"\n'
                "  destination_restriction: {\n"
                '    reason: "allow_prod"\n'
                "    destination_permissions: {\n"
                '      scope: "global"\n'
                "      type: PROD\n"
                "    }\n"
                "  }\n"
                '  trailing_field: "after"\n'
                "}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            # Extra sibling fields in [AcctRestrictions] without CORP/PROD permissions.
            (
                "[AcctRestrictions]: {\n"
                "  restricted_role: false\n"
                '  other_field: "only_metadata"\n'
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            # Empty destination_restriction followed by a sibling block containing
            # destination_permissions with type: PROD (must not bleed across braces).
            (
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {}\n"
                "  unrelated_block: {\n"
                "    destination_permissions: {\n"
                "      type: PROD\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            # Repeated [AcctRestrictions] blocks where one is unrestricted and
            # another is restricted must fail closed to RESTRICTED.
            (
                "[AcctRestrictions]: {}\n"
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {\n"
                "    destination_permissions: {\n"
                "      type: CORP\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            # Comments or quoted strings containing fake fields must be ignored.
            (
                "[AcctRestrictions]: {\n"
                "  destination_restriction: {\n"
                "    # destination_permissions: { type: PROD }\n"
                "    // destination_permissions: { type: PROD }\n"
                "    destination_permissions: {\n"
                "      type: CORP\n"
                "    }\n"
                "  }\n"
                "}\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                'note: "restricted_role: true"\n'
                "[AcctRestrictions]: {\n"
                "  # restricted_role: true\n"
                "  // restricted_role: true\n"
                "}\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            # Malformed [AcctRestrictions] textpb inputs must fail closed to RESTRICTED.
            (
                "[AcctRestrictions]: {\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                "[AcctRestrictions]: 123\n",
                loas.RestrictionType.RESTRICTED,
            ),
            # Realistic gcertstatus -format=loas2 nested output with trailing // comments.
            (
                "Credentials: <\n"
                "  Cert: <\n"
                "    ThinMintDescription: <\n"
                "      not_before: 1794639622886\n"
                "      restrictions: {\n"
                "        [AcctRestrictions]: {}\n"
                "        [security_loas_l2.ProcessCertificate]: {}\n"
                "        [security_loas_l2.RoleCertificate]: {\n"
                "          revocation_ids: 2522015791582419034\n"
                "        }\n"
                "      }\n"
                "      revocation_id: 2594073385620346970\n"
                "    >  // ThinMintDescription\n"
                "    ThinMintDescription: <\n"
                "      restrictions: {\n"
                "        [security_loas_l2.SubjectPublicKey]: {\n"
                "          handshake: {\n"
                "            ciphers: CURVE25519_SHA512\n"
                "          }\n"
                "        }\n"
                "      }\n"
                "    >  // ThinMintDescription\n"
                "    Key: <\n"
                "      curve: CURVE25519\n"
                '      serialized_private_key: "Private key data redacted"\n'
                "    >  // Key\n"
                "  >  // Cert\n"
                ">  // Credentials\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "Credentials: <\n"
                "  Cert: <\n"
                "    ThinMintDescription: <\n"
                "      restrictions: {\n"
                "        [AcctRestrictions]: {\n"
                "          restricted_role: true\n"
                "        }\n"
                "      }\n"
                "    >  // ThinMintDescription\n"
                "  >  // Cert\n"
                ">  // Credentials\n",
                loas.RestrictionType.RESTRICTED,
            ),
            (
                "Credentials: <\n"
                "  Cert: <\n"
                "    ThinMintDescription: <\n"
                "      restrictions: {\n"
                "        [AcctRestrictions]: {\n"
                "          destination_restriction: {\n"
                "            destination_permissions: {\n"
                "              type: CORP\n"
                "            }\n"
                "          }\n"
                "        }\n"
                "      }\n"
                "    >  // ThinMintDescription\n"
                "  >  // Cert\n"
                ">  // Credentials\n",
                loas.RestrictionType.RESTRICTED,
            ),
        )
        for loas2_out, expected in cases:
            with self.subTest(loas2_out=loas2_out):
                mock_subprocess.run.return_value = subprocess.CompletedProcess(
                    (), 0, stdout=loas2_out
                )
                self.assertEqual(
                    loas.check_loas_restrictions(
                        env={"USER": "alice"}, platform="linux"
                    ),
                    expected,
                )
        mock_exec.assert_not_called()

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(
        loas, "_is_executable", autospec=True, return_value=False
    )
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_inaccessible_cred_printer_returns_restricted(
        self,
        mock_subprocess: mock.Mock,
        mock_exec: mock.Mock,
        mock_shutil: mock.Mock,
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/tool"
        mock_subprocess.CalledProcessError = subprocess.CalledProcessError
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 0, stdout=""
        )
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            restriction = loas.check_loas_restrictions(
                env={"USER": "alice"}, platform="linux"
            )
        self.assertEqual(restriction, loas.RestrictionType.RESTRICTED)
        self.assertEqual(stderr.getvalue(), "")

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "_is_executable", autospec=True, return_value=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_cred_printer_classification(
        self,
        mock_subprocess: mock.Mock,
        mock_exec: mock.Mock,
        mock_shutil: mock.Mock,
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/tool"
        mock_subprocess.CalledProcessError = subprocess.CalledProcessError
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            (), 0, stdout=""
        )
        cases = (
            (
                "header: 1\n[AcctRestrictions] {\n}\nfooter: 2\n",
                loas.RestrictionType.UNRESTRICTED,
            ),
            (
                "[AcctRestrictions] {\n  restricted_role: true\n}\n",
                loas.RestrictionType.RESTRICTED,
            ),
        )
        for output, expected in cases:
            with self.subTest(expected=expected):
                mock_subprocess.check_output.return_value = output
                stdout = io.StringIO()
                stderr = io.StringIO()
                with (
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                ):
                    self.assertEqual(
                        loas.check_loas_restrictions(
                            env={"USER": "alice"},
                            verbose=True,
                            platform="linux",
                        ),
                        expected,
                    )
                self.assertEqual(stdout.getvalue(), "")
                self.assertIn("Examining credentials file", stderr.getvalue())


class EnsureLoasTest(unittest.TestCase):
    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_ensure_loas_skips_gcert_when_valid(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/gcertstatus"
        mock_subprocess.run.return_value = subprocess.CompletedProcess((), 0)
        loas.ensure_loas(env={"PATH": "/usr/bin"})
        mock_subprocess.run.assert_called_once()

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_ensure_loas_force_runs_gcert_directly(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = "/usr/bin/gcert"
        mock_subprocess.run.return_value = subprocess.CompletedProcess((), 0)
        loas.ensure_loas(force=True, env={"PATH": "/usr/bin"})
        mock_subprocess.run.assert_called_once_with(
            ("/usr/bin/gcert",), env={"PATH": "/usr/bin"}, check=False
        )

    @mock.patch.object(loas, "shutil", autospec=True)
    @mock.patch.object(loas, "subprocess", autospec=True)
    def test_ensure_loas_raises_on_missing_or_failed_gcert(
        self, mock_subprocess: mock.Mock, mock_shutil: mock.Mock
    ) -> None:
        mock_shutil.which.return_value = None
        with self.assertRaises(loas.LoasError):
            loas.ensure_loas(force=True, env={"PATH": ""})

        mock_shutil.which.return_value = "/usr/bin/gcert"
        mock_subprocess.run.return_value = subprocess.CompletedProcess((), 1)
        with self.assertRaises(loas.LoasError):
            loas.ensure_loas(force=True, env={"PATH": "/usr/bin"})


if __name__ == "__main__":
    unittest.main()
