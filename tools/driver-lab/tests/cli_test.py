# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the driver-lab CLI."""

import asyncio
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from driver_lab import cli
from driver_lab.api import EXIT_ACTIVATION
from driver_lab.discovery import (
    FakeNodeDiscovery,
    FakeProxyActivator,
    NodeDescription,
)
from driver_lab.models import AccessClass, Decision, ReadGrant
from driver_lab.permissions import load_grants, save_grants
from driver_lab.plans import plan_digest
from driver_lab.transport import FakeProxyTarget, ProxyDescription, ResourceInfo

DIGEST = "sha256:" + "ab" * 32


def make_grant(**overrides: object) -> ReadGrant:
    fields: dict[str, Any] = dict(
        schema_version=1,
        target_scope="scope",
        node_id="node",
        resource_digest=DIGEST,
        resource="control",
        offset=0x3C,
        width=4,
        access=AccessClass.READ_ONCE,
        decision=Decision.ALLOW,
        approved_at="2026-07-30T00:00:00Z",
    )
    fields.update(overrides)
    return ReadGrant(**fields)


def make_plan() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "run_id": "run-1",
        "case_id": "case-1",
        "target": {"selector": "lab-target"},
        "node": {"id": "node"},
        "access": {"mode": "proxy", "activation": "bind-unclaimed"},
        "operations": [
            {"kind": "mmio_read32", "resource": "control", "offset": "0x3c"},
        ],
    }


class CliTest(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.base = Path(self._dir.name)
        self.grants = self.base / "grants.toml"

    def run_cli(self, *argv: str) -> tuple[int, dict[str, Any], str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
            stderr
        ):
            code = cli.main(list(argv))
        payload = json.loads(stdout.getvalue()) if stdout.getvalue() else {}
        return code, payload, stderr.getvalue()

    def test_permissions_list(self) -> None:
        save_grants(self.grants, [make_grant()])
        code, payload, _ = self.run_cli(
            "permissions", "list", "--grants", str(self.grants)
        )
        self.assertEqual(code, 0)
        [row] = payload["grants"]
        self.assertEqual(row["offset"], 0x3C)
        self.assertTrue(row["grant_id"].startswith("grant-"))

    def test_permissions_revoke(self) -> None:
        save_grants(self.grants, [make_grant()])
        grant_id = make_grant().grant_id
        code, payload, _ = self.run_cli(
            "permissions",
            "revoke",
            "--grants",
            str(self.grants),
            "--grant-id",
            grant_id,
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["revoked"], grant_id)
        self.assertEqual(load_grants(self.grants), [])

        code, _, stderr = self.run_cli(
            "permissions",
            "revoke",
            "--grants",
            str(self.grants),
            "--grant-id",
            grant_id,
        )
        self.assertEqual(code, 2)
        self.assertIn("no grant", stderr)

    def test_permissions_explain(self) -> None:
        save_grants(self.grants, [make_grant()])
        code, payload, _ = self.run_cli(
            "permissions",
            "explain",
            "--grants",
            str(self.grants),
            "--target-scope",
            "scope",
            "--node-id",
            "node",
            "--resource-digest",
            DIGEST,
            "--resource",
            "control",
            "--offset",
            "0x3c",
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["outcome"], "allowed")

        code, payload, _ = self.run_cli(
            "permissions",
            "explain",
            "--grants",
            str(self.grants),
            "--target-scope",
            "scope",
            "--node-id",
            "node",
            "--resource-digest",
            DIGEST,
            "--resource",
            "control",
            "--offset",
            "0x40",
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["outcome"], "undecided")

    def test_plan_digest_matches_library(self) -> None:
        plan_path = self.base / "plan.json"
        plan_path.write_text(json.dumps(make_plan()))
        code, payload, _ = self.run_cli(
            "plan", "digest", "--plan", str(plan_path)
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["digest"], plan_digest(make_plan()))

    def test_plan_validate_rejects_takeover(self) -> None:
        plan = make_plan()
        plan["access"] = {"mode": "proxy", "activation": "takeover"}
        plan_path = self.base / "plan.json"
        plan_path.write_text(json.dumps(plan))
        code, _, stderr = self.run_cli(
            "plan", "validate", "--plan", str(plan_path)
        )
        self.assertEqual(code, 2)
        self.assertIn("phase 2", stderr)

    def test_malformed_grants_file_is_an_error(self) -> None:
        self.grants.write_text("not toml [")
        code, _, stderr = self.run_cli(
            "permissions", "list", "--grants", str(self.grants)
        )
        self.assertEqual(code, 2)
        self.assertIn("error", stderr)

    def test_permissions_add_persists_and_prints_exact_rule(self) -> None:
        code, payload, _ = self.run_cli(
            "permissions",
            "add",
            "--grants",
            str(self.grants),
            "--target-scope",
            "scope",
            "--node-id",
            "node",
            "--resource-digest",
            DIGEST,
            "--resource",
            "control",
            "--offset",
            "0x3c",
            "--decision",
            "allow",
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["added"]["offset"], 0x3C)
        self.assertEqual(payload["added"]["decision"], "allow")
        self.assertEqual(payload["added"]["approval_source"], "cli")
        [stored] = load_grants(self.grants)
        self.assertEqual(stored.grant_id, payload["added"]["grant_id"])

        # A duplicate exact rule is rejected, not silently merged.
        code, _, stderr = self.run_cli(
            "permissions",
            "add",
            "--grants",
            str(self.grants),
            "--target-scope",
            "scope",
            "--node-id",
            "node",
            "--resource-digest",
            DIGEST,
            "--resource",
            "control",
            "--offset",
            "0x3c",
            "--decision",
            "deny",
        )
        self.assertEqual(code, 2)
        self.assertIn("error", stderr)

    def _run_fixture(self) -> FakeProxyTarget:
        description = ProxyDescription(
            protocol_major=1,
            protocol_minor=0,
            proxy_generation=7,
            boot_id="boot-1",
            resource_digest="sha256:" + "ee" * 32,
            policy_digest="sha256:" + "ff" * 32,
            resources=(
                ResourceInfo(
                    id=1, name="control", logical_size=0x100, digest=DIGEST
                ),
            ),
            max_snapshot_items=64,
            audit_capacity=1024,
        )
        fake = FakeProxyTarget(description)
        fake.set_value(1, 0x3C, 0xDEAD_BEEF)
        return fake

    def run_cli_run(
        self, fake: FakeProxyTarget, run_id: str
    ) -> tuple[int, dict[str, Any], str]:
        plan = make_plan()
        plan["run_id"] = run_id
        plan_path = self.base / f"{run_id}.json"
        plan_path.write_text(json.dumps(plan))
        with (
            mock.patch.object(cli, "_connect_transport", return_value=fake),
            mock.patch.object(cli, "_connect_discovery", return_value=None),
            mock.patch.object(cli, "_connect_activator", return_value=None),
        ):
            return self.run_cli(
                "run",
                "--plan",
                str(plan_path),
                "--evidence-dir",
                str(self.base / "evidence"),
                "--grants",
                str(self.grants),
                "--target-scope",
                "scope",
                "--node-id",
                "node",
                "--moniker",
                "bootstrap/driver-lab",
            )

    def test_run_executes_plan_to_finalized_evidence(self) -> None:
        save_grants(self.grants, [make_grant()])
        fake = self._run_fixture()
        code, payload, _ = self.run_cli_run(fake, "run-1")
        self.assertEqual(code, 0)
        self.assertEqual(payload["reads"][0]["value"], 0xDEAD_BEEF)
        manifest_path = Path(payload["evidence_dir"]) / "manifest.json"
        self.assertTrue(manifest_path.exists())

    def test_run_unattended_fails_closed(self) -> None:
        fake = self._run_fixture()
        code, payload, _ = self.run_cli_run(fake, "run-1")
        self.assertEqual(code, 2)
        self.assertIn("fails closed", payload["failure"])
        self.assertEqual(fake.open_attempts, 0)

    def test_run_evidence_reuse_maps_to_evidence_category(self) -> None:
        save_grants(self.grants, [make_grant()])
        fake = self._run_fixture()
        code, _, _ = self.run_cli_run(fake, "run-1")
        self.assertEqual(code, 0)
        code, _, stderr = self.run_cli_run(fake, "run-1")
        self.assertEqual(code, 7)
        self.assertIn("no reuse", stderr)

    def test_permissions_explain_plan(self) -> None:
        save_grants(self.grants, [make_grant()])
        plan = make_plan()
        plan["target"]["selector"] = "scope"
        plan["node"]["expected_resource_digest"] = DIGEST
        plan_path = self.base / "explain_plan.json"
        plan_path.write_text(json.dumps(plan))

        # With grant present:
        code, payload, _ = self.run_cli(
            "permissions",
            "explain",
            "--grants",
            str(self.grants),
            "--plan",
            str(plan_path),
        )
        self.assertEqual(code, 0)
        self.assertTrue(payload["all_allowed"])
        self.assertEqual(len(payload["resolutions"]), 1)
        self.assertEqual(payload["resolutions"][0]["outcome"], "allowed")

        # With no grants:
        save_grants(self.grants, [])
        code, payload, _ = self.run_cli(
            "permissions",
            "explain",
            "--grants",
            str(self.grants),
            "--plan",
            str(plan_path),
        )
        self.assertEqual(code, 0)
        self.assertFalse(payload["all_allowed"])
        self.assertEqual(payload["resolutions"][0]["outcome"], "undecided")

    def test_list_nodes(self) -> None:
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(moniker="node-1", bound_driver_url=None),
                NodeDescription(
                    moniker="node-2", bound_driver_url="fuchsia-boot:///driver"
                ),
            ]
        )
        with mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ):
            code, payload, _ = self.run_cli("list")
            self.assertEqual(code, 0)
            self.assertEqual(len(payload["nodes"]), 2)

            code, payload_unclaimed, _ = self.run_cli("list", "--unclaimed")
            self.assertEqual(code, 0)
            self.assertEqual(len(payload_unclaimed["nodes"]), 1)
            self.assertEqual(payload_unclaimed["nodes"][0]["moniker"], "node-1")

    def test_describe_node(self) -> None:
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="node-1",
                    bound_driver_url=None,
                    offers=("fuchsia.examples.Echo",),
                )
            ]
        )
        with mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ):
            code, payload, _ = self.run_cli("describe", "--node", "node-1")
            self.assertEqual(code, 0)
            self.assertEqual(payload["node"]["moniker"], "node-1")
            self.assertEqual(
                payload["node"]["offers"], ["fuchsia.examples.Echo"]
            )

            code, _, stderr = self.run_cli("describe", "--node", "nonexistent")
            self.assertEqual(code, 2)
            self.assertIn("not found", stderr)

    def test_direct_mode_cli(self) -> None:
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="node-1",
                    bound_driver_url="fuchsia-boot:///driver",
                    offers=("fuchsia.examples.Echo",),
                )
            ]
        )
        with mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ):
            code, payload, _ = self.run_cli(
                "direct",
                "--node",
                "node-1",
                "--protocol",
                "fuchsia.examples.Echo",
            )
            self.assertEqual(code, 0)
            self.assertEqual(payload["status"], "verified")

            code, _, stderr = self.run_cli(
                "direct",
                "--node",
                "node-1",
                "--protocol",
                "fuchsia.hardware.other",
            )
            self.assertEqual(code, 2)
            self.assertIn("does not offer protocol", stderr)

    def test_bind_proxy_and_end_proxy_cli(self) -> None:
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(moniker="node-1", bound_driver_url=None),
                NodeDescription(
                    moniker="node-2", bound_driver_url="fuchsia-boot:///driver"
                ),
            ]
        )
        activator = FakeProxyActivator(discovery)
        with mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ), mock.patch.object(cli, "_connect_activator", return_value=activator):
            # Cannot bind already bound node
            code, _, stderr = self.run_cli("bind-proxy", "--node", "node-2")
            self.assertEqual(code, EXIT_ACTIVATION)
            self.assertIn("is not unclaimed", stderr)

            # Bind unclaimed node
            code, payload, _ = self.run_cli("bind-proxy", "--node", "node-1")
            self.assertEqual(code, 0)
            self.assertTrue(payload["bound"])
            node = asyncio.run(discovery.describe_node("node-1"))
            self.assertIsNotNone(node)
            assert node is not None
            self.assertFalse(node.is_unclaimed)

            # End proxy access
            code, payload, _ = self.run_cli("end-proxy", "--node", "node-1")
            self.assertEqual(code, 0)
            self.assertTrue(payload["unclaimed"])
            node = asyncio.run(discovery.describe_node("node-1"))
            self.assertIsNotNone(node)
            assert node is not None
            self.assertTrue(node.is_unclaimed)

    def test_run_with_automated_bind_and_verified_teardown(self) -> None:
        save_grants(self.grants, [make_grant()])
        fake = self._run_fixture()
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="node",
                    bound_driver_url=None,
                )
            ]
        )
        activator = FakeProxyActivator(discovery)
        plan = make_plan()
        plan_path = self.base / "run_bind.json"
        plan_path.write_text(json.dumps(plan))
        with mock.patch.object(
            cli, "_connect_transport", return_value=fake
        ), mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ), mock.patch.object(
            cli, "_connect_activator", return_value=activator
        ):
            code, payload, _ = self.run_cli(
                "run",
                "--plan",
                str(plan_path),
                "--evidence-dir",
                str(self.base / "evidence_bind"),
                "--grants",
                str(self.grants),
                "--target-scope",
                "scope",
                "--node-id",
                "node",
                "--moniker",
                "bootstrap/driver-lab",
            )
            self.assertEqual(code, 0)
            self.assertEqual(payload["reads"][0]["value"], 0xDEAD_BEEF)
            self.assertEqual(len(activator.bind_calls), 1)
            self.assertEqual(len(activator.end_calls), 1)
            # Verified teardown left node unclaimed
            node = asyncio.run(discovery.describe_node("node"))
            self.assertIsNotNone(node)
            assert node is not None
            self.assertTrue(node.is_unclaimed)

    def test_list_debug_capable_filters_active_drivers_exposing_service(
        self,
    ) -> None:
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="dev.sys.platform.unclaimed",
                    bound_driver_url=None,
                    offers=("fuchsia.driver.lab.Service",),
                ),
                NodeDescription(
                    moniker="dev.sys.platform.normal-driver",
                    bound_driver_url="fuchsia-pkg://fuchsia.com/normal#meta/normal.cm",
                    offers=("fuchsia.hardware.gpio.Service",),
                ),
                NodeDescription(
                    moniker="dev.sys.platform.sample-device",
                    bound_driver_url="fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm",
                    offers=("fuchsia.driver.lab.Service",),
                ),
            ]
        )
        with mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ):
            code, payload, _ = self.run_cli("list", "--debug-capable")
            self.assertEqual(code, 0)
            self.assertEqual(len(payload["nodes"]), 1)
            self.assertEqual(
                payload["nodes"][0]["moniker"],
                "dev.sys.platform.sample-device",
            )
            self.assertTrue(payload["nodes"][0]["debug_capable"])

    def test_describe_with_moniker_fetches_embedded_proxy_description(
        self,
    ) -> None:
        fake = self._run_fixture()
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="dev.sys.platform.sample-device",
                    bound_driver_url="fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm",
                    offers=("fuchsia.driver.lab.Service",),
                )
            ]
        )
        with mock.patch.object(
            cli, "_connect_transport", return_value=fake
        ), mock.patch.object(cli, "_connect_discovery", return_value=discovery):
            code, payload, _ = self.run_cli(
                "describe",
                "--moniker",
                "dev.sys.platform.sample-device",
            )
            self.assertEqual(code, 0)
            self.assertEqual(
                payload["moniker"], "dev.sys.platform.sample-device"
            )
            self.assertEqual(payload["description"]["boot_id"], "boot-1")
            self.assertEqual(
                payload["node"]["bound_driver_url"],
                "fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm",
            )

    def test_inspect_moniker_reads_register_and_drains_audit(self) -> None:
        fake = self._run_fixture()
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="dev.sys.platform.sample-device",
                    bound_driver_url="fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm",
                    offers=("fuchsia.driver.lab.Service",),
                )
            ]
        )
        with mock.patch.object(
            cli, "_connect_transport", return_value=fake
        ), mock.patch.object(cli, "_connect_discovery", return_value=discovery):
            code, payload, _ = self.run_cli(
                "inspect",
                "--moniker",
                "dev.sys.platform.sample-device",
                "--resource",
                "control",
                "--offset",
                "0x3c",
            )
            self.assertEqual(code, 0)
            self.assertEqual(
                payload["driver_url"],
                "fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm",
            )
            self.assertEqual(payload["read"]["value"], 0xDEAD_BEEF)
            self.assertTrue(len(payload["audit"]) >= 1)

    def test_in_situ_run_preserves_bound_driver_and_records_manifest_metadata(
        self,
    ) -> None:
        save_grants(self.grants, [make_grant()])
        fake = self._run_fixture()
        driver_url = (
            "fuchsia-pkg://fuchsia.com/lab_root#meta/lab_sample_driver.cm"
        )
        discovery = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="node",
                    bound_driver_url=driver_url,
                    offers=("fuchsia.driver.lab.Service",),
                )
            ]
        )
        activator = FakeProxyActivator(discovery)
        plan = make_plan()
        plan["access"] = {"mode": "in-situ"}
        plan["node"] = {
            "id": "node",
            "driver_moniker": "/bootstrap/boot-drivers:dev.sys.platform.sample-device",
            "expected_unclaimed": False,
        }
        plan_path = self.base / "run_in_situ.json"
        plan_path.write_text(json.dumps(plan))
        with mock.patch.object(
            cli, "_connect_transport", return_value=fake
        ), mock.patch.object(
            cli, "_connect_discovery", return_value=discovery
        ), mock.patch.object(
            cli, "_connect_activator", return_value=activator
        ):
            code, payload, _ = self.run_cli(
                "run",
                "--plan",
                str(plan_path),
                "--evidence-dir",
                str(self.base / "evidence_in_situ"),
                "--grants",
                str(self.grants),
                "--target-scope",
                "scope",
                "--node-id",
                "node",
                "--moniker",
                "/bootstrap/boot-drivers:dev.sys.platform.sample-device",
            )
            self.assertEqual(code, 0)
            self.assertEqual(payload["reads"][0]["value"], 0xDEAD_BEEF)
            # In-situ mode never calls bind_proxy or end_proxy
            self.assertEqual(len(activator.bind_calls), 0)
            self.assertEqual(len(activator.end_calls), 0)
            # Driver remains actively bound after in-situ session
            node = asyncio.run(discovery.describe_node("node"))
            self.assertIsNotNone(node)
            assert node is not None
            self.assertFalse(node.is_unclaimed)
            self.assertEqual(node.bound_driver_url, driver_url)
            # Evidence manifest records driver_moniker and driver_url
            manifest = json.loads(
                (Path(payload["evidence_dir"]) / "manifest.json").read_text()
            )
            self.assertEqual(
                manifest["driver_moniker"],
                "/bootstrap/boot-drivers:dev.sys.platform.sample-device",
            )
            self.assertEqual(manifest["driver_url"], driver_url)


if __name__ == "__main__":
    unittest.main()
