#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for register_subagents.py.

Run with:
    python3 src/devices/skills/autoda/scripts/register_subagents_test.py
"""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import register_subagents  # pylint: disable=wrong-import-position

_SHARED_ROLES = (
    "linux-driver-expert",
    "datasheet-researcher",
    "hardware-prober",
)


class RegistryTest(unittest.TestCase):
    def test_every_workflow_role_is_defined(self) -> None:
        for workflow, spec in register_subagents.WORKFLOWS.items():
            for role in spec["roles"]:
                self.assertIn(
                    role, register_subagents.ROLES, f"{workflow}/{role}"
                )
            for role in spec.get("overrides", {}):
                self.assertIn(
                    role, spec["roles"], f"{workflow} override {role}"
                )
            for role in spec["addenda"]:
                self.assertIn(
                    role, spec["roles"], f"{workflow} addendum {role}"
                )

    def test_prompts_end_with_active_workflow_banner(self) -> None:
        for workflow, spec in register_subagents.WORKFLOWS.items():
            merged = register_subagents.subagents_for(workflow)
            self.assertEqual(sorted(merged), sorted(spec["roles"]))
            for role, config in merged.items():
                self.assertIn(
                    f"ACTIVE WORKFLOW: {spec['skill']} ({workflow}).",
                    config["system_prompt"],
                    f"{workflow}/{role}",
                )

    def test_shared_roles_carry_both_workflow_sections(self) -> None:
        for role in _SHARED_ROLES:
            prompt = register_subagents.ROLES[role]["system_prompt"]
            self.assertIn("FIX WORKFLOW (autoda-fix)", prompt, role)
            self.assertIn("RECON WORKFLOW (autoda-recon)", prompt, role)
            self.assertNotIn("In the bug-fix workflow", prompt, role)

    def test_zero_parametric_memory_markers(self) -> None:
        lde = register_subagents.ROLES["linux-driver-expert"]["system_prompt"]
        self.assertIn("STATUS: BLOCKED_MISSING_LINUX_SOURCE", lde)
        self.assertIn("STATUS: VERIFIED_FROM_SOURCE", lde)
        self.assertIn("branch@commit", lde)
        dsr = register_subagents.ROLES["datasheet-researcher"]["system_prompt"]
        self.assertIn("STATUS: UNAVAILABLE_NO_DATASHEET_FOUND", dsr)
        self.assertIn("STATUS: VERIFIED_FROM_DATASHEET", dsr)

    def test_scope_and_prober_rules_present(self) -> None:
        for role in ("linux-driver-expert", "datasheet-researcher"):
            self.assertIn(
                "SCOPE BEFORE FACTS",
                register_subagents.ROLES[role]["system_prompt"],
                role,
            )
        prober = register_subagents.ROLES["hardware-prober"]["system_prompt"]
        for token in ("in-situ", "proxy", "direct", "state_origin", "liveness"):
            self.assertIn(token, prober, token)

    def test_recon_read_roles_enable_guarded_shell_access(self) -> None:
        recon = register_subagents.subagents_for("recon")
        for role in (
            "linux-driver-expert",
            "datasheet-researcher",
            "session-auditor",
        ):
            self.assertTrue(
                recon[role]["enable_write_tools"],
                f"recon/{role} must enable shell access",
            )
            self.assertIn(
                "git status --porcelain",
                recon[role]["system_prompt"],
                f"recon/{role} must enforce clean checkout guardrail",
            )
        self.assertFalse(recon["adversarial-reviewer"]["enable_write_tools"])
        fix = register_subagents.subagents_for("fix")
        self.assertTrue(fix["session-auditor"]["enable_write_tools"])
        self.assertFalse(fix["linux-driver-expert"]["enable_write_tools"])
        self.assertFalse(fix["datasheet-researcher"]["enable_write_tools"])

    def test_recon_quote_budget_guidance(self) -> None:
        recon_lde = register_subagents.subagents_for("recon")[
            "linux-driver-expert"
        ]["system_prompt"]
        self.assertIn("1-3", recon_lde)
        self.assertIn("<artifact_dir>/sources/", recon_lde)
        self.assertIn("5-15 lines in `autoda-fix`", recon_lde)

    def test_unknown_workflow_rejected(self) -> None:
        with self.assertRaises(KeyError):
            register_subagents.subagents_for("nope")

    def test_backward_compatible_alias(self) -> None:
        self.assertEqual(
            register_subagents.SUBAGENTS,
            register_subagents.subagents_for("fix"),
        )


if __name__ == "__main__":
    unittest.main()
