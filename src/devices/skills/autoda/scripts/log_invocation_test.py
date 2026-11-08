#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for log_invocation.py.

Run with:
    python3 src/devices/skills/autoda/scripts/log_invocation_test.py
"""

import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import log_invocation  # pylint: disable=wrong-import-position


class LogInvocationTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)
        self.app_dir = self.root / "app_data"
        self.telemetry_file = self.root / "telemetry" / "invocations.jsonl"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _run_main(
        self, argv: list[str], metrics_enabled: bool = True
    ) -> dict[str, object]:
        buf = io.StringIO()
        env = {
            "ANTIGRAVITY_APP_DATA_DIR": str(self.app_dir),
            "USER": "testuser",
        }
        with (
            mock.patch.dict(os.environ, env, clear=False),
            mock.patch.object(
                log_invocation,
                "query_session_telemetry",
                return_value=("traj-1234", "test-model", "mod-id", "mod-v1"),
            ),
            mock.patch.object(
                log_invocation,
                "is_fx_metrics_enabled",
                return_value=metrics_enabled,
            ),
            contextlib.redirect_stdout(buf),
        ):
            rc = log_invocation.main(argv)
        self.assertEqual(0, rc)
        return json.loads(buf.getvalue())

    def test_recon_custom_spec_and_devlog_artifact_dir(self) -> None:
        custom_dir = self.root / "campaign_out" / "example-timer-0"
        custom_dir.mkdir(parents=True)
        spec = custom_dir / "recon_spec.md"
        devlog = custom_dir / "recon_devlog.md"
        spec.write_text(
            textwrap.dedent(
                """\
                ---
                skill: autoda-recon
                skill_version: "1.0.0"
                conversation_id: "conv-custom-001"
                campaign_id: "campaign-timers-v1"
                system_id: "example-timer-0"
                hardware_verification: "disabled"
                outcome: "STATIC_COMPLETE"
                claims_total: 42
                claims_verified: 0
                claims_contradicted: 1
                ---
                # Recon spec
                """
            ),
            encoding="utf-8",
        )
        devlog.write_text(
            textwrap.dedent(
                """\
                ---
                skill: autoda-recon
                system_id: "example-timer-0"
                ---
                ## Phase 0
                """
            ),
            encoding="utf-8",
        )

        out = self._run_main(
            [
                "--workflow",
                "recon",
                "--spec",
                str(spec),
                "--devlog",
                str(devlog),
                "--telemetry-path",
                str(self.telemetry_file),
            ]
        )
        self.assertEqual("recon", out["workflow"])
        self.assertEqual("autoda-recon", out["skill"])
        self.assertEqual("conv-custom-001", out["conversation_id"])
        self.assertEqual("campaign-timers-v1", out["campaign_id"])
        self.assertEqual("example-timer-0", out["system_id"])
        self.assertEqual("disabled", out["hardware_verification"])
        self.assertEqual("STATIC_COMPLETE", out["outcome"])
        self.assertEqual(42, out["claims_total"])
        self.assertEqual(0, out["claims_verified"])
        self.assertEqual(1, out["claims_contradicted"])
        self.assertTrue(out["logged_to_shared"])

        # Upsert the same session when hardware verification finishes.
        spec.write_text(
            spec.read_text(encoding="utf-8")
            .replace('"disabled"', '"enabled"')
            .replace('"STATIC_COMPLETE"', '"HARDWARE_VERIFIED"')
            .replace("claims_verified: 0", "claims_verified: 5"),
            encoding="utf-8",
        )
        out2 = self._run_main(
            [
                "--workflow",
                "recon",
                "--spec",
                str(spec),
                "--devlog",
                str(devlog),
                "--telemetry-path",
                str(self.telemetry_file),
            ]
        )
        self.assertEqual("HARDWARE_VERIFIED", out2["outcome"])
        self.assertEqual(5, out2["claims_verified"])

        lines = [
            json.loads(line)
            for line in self.telemetry_file.read_text(
                encoding="utf-8"
            ).splitlines()
            if line.strip()
        ]
        self.assertEqual(1, len(lines))
        self.assertEqual("HARDWARE_VERIFIED", lines[0]["outcome"])
        self.assertEqual(5, lines[0]["claims_verified"])

    def test_recon_devlog_fallback_and_cli_override(self) -> None:
        custom_dir = self.root / "custom"
        custom_dir.mkdir(parents=True)
        missing_spec = custom_dir / "recon_spec.md"
        devlog = custom_dir / "recon_devlog.md"
        devlog.write_text(
            textwrap.dedent(
                """\
                ---
                skill: autoda-recon
                conversation_id: "conv-devlog-only"
                campaign_id: "campaign-fallback"
                system_id: "example-wdt-0"
                hardware_verification: "enabled"
                outcome: "IN_PROGRESS"
                ---
                ## Phase 0
                """
            ),
            encoding="utf-8",
        )

        out = self._run_main(
            [
                "--workflow",
                "recon",
                "--spec",
                str(missing_spec),
                "--devlog",
                str(devlog),
                "--outcome",
                "BLOCKED",
                "--telemetry-path",
                str(self.telemetry_file),
            ],
            metrics_enabled=False,
        )
        self.assertEqual("conv-devlog-only", out["conversation_id"])
        self.assertEqual("campaign-fallback", out["campaign_id"])
        self.assertEqual("example-wdt-0", out["system_id"])
        self.assertEqual("enabled", out["hardware_verification"])
        self.assertEqual("BLOCKED", out["outcome"])
        self.assertFalse(out["logged_to_shared"])
        self.assertFalse(self.telemetry_file.exists())

    def test_fix_custom_spec_and_brain_fallback(self) -> None:
        custom_dir = self.root / "fix_out"
        custom_dir.mkdir(parents=True)
        bug_spec = custom_dir / "bug_spec.md"
        bug_spec.write_text(
            textwrap.dedent(
                """\
                ---
                skill: autoda-fix
                bug_id: "b/123456789"
                conversation_id: "conv-fix-001"
                verification_mode: "Mode A"
                adversarial_review: "enabled"
                adversarial_review_rounds: 2
                ---
                # Bug spec
                """
            ),
            encoding="utf-8",
        )

        out = self._run_main(
            [
                "--workflow",
                "fix",
                "--bug-spec",
                str(bug_spec),
                "--telemetry-path",
                str(self.telemetry_file),
            ],
            metrics_enabled=False,
        )
        self.assertEqual("123456789", out["bug_id"])
        self.assertEqual("conv-fix-001", out["conversation_id"])
        self.assertEqual("Mode A", out["verification_mode"])
        self.assertEqual("enabled", out["adversarial_review"])
        self.assertEqual(2, out["adversarial_review_rounds"])

        # Default <brain>/<conversation_id>/bug_spec.md path still works.
        brain_conv = self.app_dir / "brain" / "conv-brain-002"
        brain_conv.mkdir(parents=True)
        (brain_conv / "bug_spec.md").write_text(
            textwrap.dedent(
                """\
                ---
                verification_mode: "Mode C"
                adversarial_review: "disabled"
                ---
                """
            ),
            encoding="utf-8",
        )
        out_brain = self._run_main(
            [
                "--workflow",
                "fix",
                "--conversation-id",
                "conv-brain-002",
                "--bug-id",
                "987654",
                "--telemetry-path",
                str(self.telemetry_file),
            ],
            metrics_enabled=False,
        )
        self.assertEqual("Mode C", out_brain["verification_mode"])
        self.assertEqual("disabled", out_brain["adversarial_review"])


if __name__ == "__main__":
    unittest.main()
