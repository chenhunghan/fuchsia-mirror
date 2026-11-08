# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.attach import AttachRequest


class TestAttachHandler(DaemonTestCase):
    async def test_attach_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("attach", daemon.registry.handlers)

    async def test_handle_attach_success(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach_resp = Mock()
            mock_attach_resp.dump_dap.return_value = {"success": True}
            mock_attach.return_value = mock_attach_resp

            req = AttachRequest(filter="my_process")
            resp = await daemon.registry.handle("attach", req)

            self.assertTrue(resp.success)
            self.assertEqual(resp.body, {"success": True})
            mock_attach.assert_called_once()
            attach_args = mock_attach.call_args[0][0]
            self.assertEqual(
                attach_args.extra_fields, {"process": "my_process"}
            )

    async def test_handle_attach_with_flags(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach_resp = Mock()
            mock_attach_resp.dump_dap.return_value = {"success": True}
            mock_attach.return_value = mock_attach_resp

            req = AttachRequest(
                filter="driver_host",
                weak=True,
                exact=True,
                recursive=True,
                job_only=True,
            )
            resp = await daemon.registry.handle("attach", req)

            self.assertTrue(resp.success)
            mock_attach.assert_called_once()
            attach_args = mock_attach.call_args[0][0]
            self.assertEqual(
                attach_args.extra_fields,
                {
                    "process": "driver_host",
                    "weak": True,
                    "exact": True,
                    "recursive": True,
                    "job_only": True,
                },
            )

    async def test_handle_attach_failure(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach.side_effect = Exception("Failed to attach")

            req = AttachRequest(filter="my_process")
            resp = await daemon.registry.handle("attach", req)

            self.assertFalse(resp.success)
            self.assertIn("Failed to attach", resp.message or "")

    async def test_handle_attach_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None

        req = AttachRequest(filter="my_process")
        resp = await daemon.registry.handle("attach", req)

        self.assertFalse(resp.success)
        self.assertIn("Not connected", resp.message or "")

    @patch("daemon.driver_utils.run_ffx_json", new_callable=AsyncMock)
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_attach_driver_and_node(
        self, mock_dap_client_class: Mock, mock_ffx_json: AsyncMock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.attach = AsyncMock(
            return_value=Mock(dump_dap=lambda: {"success": True})
        )

        async def ffx_side_effect(*args: str) -> object:
            if args == ("driver", "host", "list"):
                return [
                    {
                        "koid": 48291,
                        "name": "driver_host:pdev:05:00:f",
                        "drivers": [
                            "fuchsia-boot:///xhci#meta/xhci.cm",
                            "fuchsia-boot:///usb-bus#meta/usb-bus.cm",
                        ],
                    }
                ]
            if args == ("driver", "node", "list"):
                return [
                    {
                        "moniker": "dev.sys.platform.xhci",
                        "owner": "fuchsia-boot:///xhci#meta/xhci.cm",
                    }
                ]
            return []

        mock_ffx_json.side_effect = ffx_side_effect

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        attach_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(
                driver="xhci",
                weak=True,
                recursive=True,
                job_only=True,
            ),
        )
        self.assertTrue(attach_resp.success)
        assert isinstance(attach_resp.body, dict)
        self.assertEqual(attach_resp.body["driver_host_koid"], 48291)
        self.assertEqual(attach_resp.body["attach_mode"], "weak")
        self.assertTrue(attach_resp.body["transport_hazard"])
        self.assertIsNotNone(attach_resp.body["hazard_warning"])
        self.assertIn("modules, aspace", attach_resp.body["hazard_warning"])
        self.assertNotIn("tracepoints", attach_resp.body["hazard_warning"])
        self.assertIsNotNone(attach_resp.body["colocated_warning"])
        attach_call_arg = mock_dap_client.attach.call_args[0][0]
        self.assertEqual(
            attach_call_arg.extra_fields,
            {"process": "48291", "weak": True},
        )
        self.assertIsInstance(attach_call_arg.extra_fields["process"], str)

        # Wait filter when driver is not running yet
        wait_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(
                driver="nonexistent_drv",
                wait=True,
                weak=True,
                recursive=True,
                job_only=True,
            ),
        )
        self.assertTrue(wait_resp.success)
        assert isinstance(wait_resp.body, dict)
        self.assertEqual(wait_resp.body["attach_mode"], "filter")
        self.assertEqual(wait_resp.body["filter_pattern"], "driver_host")
        self.assertEqual(wait_resp.body["requested_target"], "nonexistent_drv")
        self.assertIsNotNone(wait_resp.body["filter_warning"])
        self.assertIn("nonexistent_drv", wait_resp.body["filter_warning"])
        self.assertIn("--weak", wait_resp.body["filter_warning"])
        wait_call_arg = mock_dap_client.attach.call_args[0][0]
        self.assertEqual(
            wait_call_arg.extra_fields,
            {
                "process": "driver_host",
                "weak": True,
                "recursive": True,
                "job_only": True,
            },
        )

        # Numeric --driver matching a running driver_host KOID attaches with hazard checks
        mock_dap_client.attach.reset_mock()
        koid_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(driver="48291", weak=True),
        )
        self.assertTrue(koid_resp.success)
        assert isinstance(koid_resp.body, dict)
        self.assertEqual(koid_resp.body["driver_host_koid"], 48291)
        self.assertTrue(koid_resp.body["transport_hazard"])
        self.assertIsNotNone(koid_resp.body["hazard_warning"])

        # Numeric --driver (e.g. "8250") when no host matches and wait=True installs filter
        mock_dap_client.attach.reset_mock()
        wait_num_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(driver="8250", wait=True, weak=True),
        )
        self.assertTrue(wait_num_resp.success)
        assert isinstance(wait_num_resp.body, dict)
        self.assertEqual(wait_num_resp.body["attach_mode"], "filter")
        self.assertEqual(wait_num_resp.body["filter_pattern"], "driver_host")
        self.assertEqual(wait_num_resp.body["requested_target"], "8250")
        wait_num_call_arg = mock_dap_client.attach.call_args[0][0]
        self.assertEqual(
            wait_num_call_arg.extra_fields,
            {"process": "driver_host", "weak": True},
        )

        # Numeric --driver when no host matches and wait=False fails without DAP attach
        mock_dap_client.attach.reset_mock()
        missing_num_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(driver="8250", wait=False),
        )
        self.assertFalse(missing_num_resp.success)
        self.assertIn(
            "No running driver_host matched '8250'",
            missing_num_resp.message or "",
        )
        mock_dap_client.attach.assert_not_called()

        # Wait filter when node is not running yet
        wait_node_resp = await daemon.registry.handle(
            "attach",
            AttachRequest(node="dev.sys.nonexistent", wait=True),
        )
        self.assertTrue(wait_node_resp.success)
        assert isinstance(wait_node_resp.body, dict)
        self.assertEqual(wait_node_resp.body["attach_mode"], "filter")
        self.assertEqual(wait_node_resp.body["filter_pattern"], "driver_host")
        self.assertEqual(
            wait_node_resp.body["requested_target"], "dev.sys.nonexistent"
        )
        self.assertIsNotNone(wait_node_resp.body["filter_warning"])
        self.assertIn(
            "dev.sys.nonexistent", wait_node_resp.body["filter_warning"]
        )
        wait_node_call_arg = mock_dap_client.attach.call_args[0][0]
        self.assertEqual(
            wait_node_call_arg.extra_fields,
            {"process": "driver_host"},
        )

        # Rejection of --exact with --driver / --node in daemon handler
        exact_drv_req = AttachRequest.model_construct(
            command="attach",
            driver="xhci",
            exact=True,
        )
        exact_drv_resp = await daemon.registry.handle("attach", exact_drv_req)
        self.assertFalse(exact_drv_resp.success)
        self.assertIn(
            "--exact cannot be used with --driver or --node",
            exact_drv_resp.message or "",
        )

        exact_node_req = AttachRequest.model_construct(
            command="attach",
            node="dev.sys.platform.xhci",
            exact=True,
        )
        exact_node_resp = await daemon.registry.handle("attach", exact_node_req)
        self.assertFalse(exact_node_resp.success)
        self.assertIn(
            "--exact cannot be used with --driver or --node",
            exact_node_resp.message or "",
        )


if __name__ == "__main__":
    unittest.main()
