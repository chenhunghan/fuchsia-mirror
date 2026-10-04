# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the interactive consent flow."""

import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

from driver_lab.api import EXIT_PERMISSION, DriverLab
from driver_lab.consent import ConsentDecision
from driver_lab.models import AccessClass, AccessRequest, Decision
from driver_lab.permissions import load_grants
from driver_lab.transport import FakeProxyTarget, ProxyDescription, ResourceInfo

CTRL_DIGEST = "sha256:" + "ab" * 32
DESCRIPTION_DIGEST = "sha256:" + "ee" * 32
POLICY_DIGEST = "sha256:" + "ff" * 32
TARGET_SCOPE = "example-engineering-target"
NODE_ID = "example-device"


def make_description() -> ProxyDescription:
    return ProxyDescription(
        protocol_major=1,
        protocol_minor=0,
        proxy_generation=7,
        boot_id="boot-1",
        resource_digest=DESCRIPTION_DIGEST,
        policy_digest=POLICY_DIGEST,
        resources=(
            ResourceInfo(
                id=1, name="control", logical_size=0x100, digest=CTRL_DIGEST
            ),
        ),
        max_snapshot_items=64,
        audit_capacity=1024,
    )


def make_plan(**overrides: object) -> dict[str, Any]:
    plan: dict[str, Any] = {
        "schema_version": 1,
        "run_id": "run-1",
        "case_id": "case-1",
        "target": {"selector": "lab-target"},
        "node": {"id": NODE_ID, "expected_resource_digest": DESCRIPTION_DIGEST},
        "access": {"mode": "proxy", "activation": "bind-unclaimed"},
        "operations": [
            {"kind": "mmio_read32", "resource": "control", "offset": "0x3c"},
        ],
    }
    plan.update(overrides)
    return plan


class ScriptedPrompt:
    """Consent prompt returning scripted decisions and recording calls."""

    def __init__(self, decision: ConsentDecision) -> None:
        self.decision = decision
        self.requests: list[AccessRequest] = []
        self.warnings: list[str] = []

    async def request_consent(
        self, request: AccessRequest, warning: str
    ) -> ConsentDecision:
        self.requests.append(request)
        self.warnings.append(warning)
        return self.decision


class ConsentFlowTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        base = Path(self._dir.name)
        self.grants_path = base / "grants.toml"
        self.evidence_root = base / "evidence"
        self.fake = FakeProxyTarget(make_description())
        self.fake.set_value(1, 0x3C, 0xDEAD_BEEF)

    def make_lab(self, prompt: ScriptedPrompt | None) -> DriverLab:
        return DriverLab(
            self.fake,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope=TARGET_SCOPE,
            node_id=NODE_ID,
            consent=prompt,
        )

    def resolution_sources(self, evidence_dir: Path) -> list[str]:
        rows = json.loads(
            (evidence_dir / "permission-resolution.json").read_text()
        )
        return [row["source"] for row in rows]

    async def test_prompt_receives_exact_rule_and_warning(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        await self.make_lab(prompt).run_plan(make_plan())
        [request] = prompt.requests
        self.assertEqual(request.resource, "control")
        self.assertEqual(request.offset, 0x3C)
        self.assertEqual(request.access, AccessClass.READ_ONCE)
        self.assertEqual(request.resource_digest, CTRL_DIGEST)
        self.assertIn("clear status bits", prompt.warnings[0])

    async def test_allow_once_succeeds_without_persisting(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        result = await self.make_lab(prompt).run_plan(make_plan())
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(result.reads[0].value, 0xDEAD_BEEF)
        self.assertEqual(load_grants(self.grants_path), [])
        self.assertIn(
            "allow_once", self.resolution_sources(result.evidence_dir)
        )

    async def test_always_allow_persists_exact_rule(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALWAYS_ALLOW)
        result = await self.make_lab(prompt).run_plan(make_plan())
        self.assertTrue(result.ok, result.failure)
        [grant] = load_grants(self.grants_path)
        self.assertEqual(grant.offset, 0x3C)
        self.assertEqual(grant.decision, Decision.ALLOW)
        self.assertEqual(grant.approval_source, "interactive")

        # A second run resolves from the persistent grant without prompting.
        second_prompt = ScriptedPrompt(ConsentDecision.DENY_ONCE)
        second = await self.make_lab(second_prompt).run_plan(
            make_plan(run_id="run-2")
        )
        self.assertTrue(second.ok, second.failure)
        self.assertEqual(second_prompt.requests, [])
        self.assertIn(
            "persistent", self.resolution_sources(second.evidence_dir)
        )

    async def test_deny_once_fails_without_persisting(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.DENY_ONCE)
        result = await self.make_lab(prompt).run_plan(make_plan())
        self.assertEqual(result.exit_category, EXIT_PERMISSION)
        self.assertEqual(result.failure, "denied by operator")
        self.assertEqual(self.fake.open_attempts, 0)
        self.assertEqual(load_grants(self.grants_path), [])

    async def test_always_deny_persists_and_skips_future_prompts(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALWAYS_DENY)
        result = await self.make_lab(prompt).run_plan(make_plan())
        self.assertEqual(result.exit_category, EXIT_PERMISSION)
        [grant] = load_grants(self.grants_path)
        self.assertEqual(grant.decision, Decision.DENY)

        second_prompt = ScriptedPrompt(ConsentDecision.ALWAYS_ALLOW)
        second = await self.make_lab(second_prompt).run_plan(
            make_plan(run_id="run-2")
        )
        self.assertEqual(second.exit_category, EXIT_PERMISSION)
        self.assertEqual(second.failure, "denied by persistent grant")
        self.assertEqual(second_prompt.requests, [])

    async def test_duplicate_offsets_prompt_once(self) -> None:
        prompt = ScriptedPrompt(ConsentDecision.ALLOW_ONCE)
        plan = make_plan(
            operations=[
                {
                    "kind": "mmio_read32",
                    "resource": "control",
                    "offset": "0x3c",
                },
                {
                    "kind": "mmio_read32",
                    "resource": "control",
                    "offset": "0x3c",
                },
            ]
        )
        result = await self.make_lab(prompt).run_plan(plan)
        self.assertTrue(result.ok, result.failure)
        self.assertEqual(len(prompt.requests), 1)

    async def test_no_prompt_fails_closed(self) -> None:
        result = await self.make_lab(None).run_plan(make_plan())
        self.assertEqual(result.exit_category, EXIT_PERMISSION)
        self.assertEqual(
            result.failure,
            "consent required; unattended operation fails closed",
        )
        self.assertIn(
            "fail_closed", self.resolution_sources(result.evidence_dir)
        )


if __name__ == "__main__":
    unittest.main()
