#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for planter_version.sh."""

import json
import os
import subprocess
import unittest

SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "planter_version.sh"
)


def run_check(**env_vars):
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in ("PLANTER_CHECK_NAME", "PLANTER_RESOLVED_CHANGE_BASE")
    }
    env.update(env_vars)
    out = subprocess.run(
        ["bash", SCRIPT], env=env, capture_output=True, text=True, check=True
    )
    return json.loads(out.stdout)


class PlanterVersionTest(unittest.TestCase):
    def test_old_planter_is_blocked(self):
        findings = run_check(PLANTER_CHECK_NAME="planter_version")
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0]["category"], "planter_too_old")
        self.assertEqual(findings[0]["severity"], "ERROR")
        self.assertIn("cl/1010931925", findings[0]["message"])

    def test_current_planter_passes(self):
        self.assertEqual(
            run_check(
                PLANTER_CHECK_NAME="planter_version",
                PLANTER_RESOLVED_CHANGE_BASE="abc123",
            ),
            [],
        )
        # Planter exports the variable even when it resolves to nothing.
        self.assertEqual(
            run_check(
                PLANTER_CHECK_NAME="planter_version",
                PLANTER_RESOLVED_CHANGE_BASE="",
            ),
            [],
        )

    def test_manual_run_passes(self):
        self.assertEqual(run_check(), [])


if __name__ == "__main__":
    unittest.main()
