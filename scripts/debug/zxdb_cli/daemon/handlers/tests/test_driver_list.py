# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.driver_utils import match_driver_host, match_host_by_node
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.driver_list import (
    DriverListRequest,
    DriverListResponse,
)


class TestDriverListHandler(DaemonTestCase):
    async def test_driver_list_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("driver-list", daemon.registry.handlers)

    @patch("daemon.driver_utils.run_ffx_json", new_callable=AsyncMock)
    async def test_handle_driver_list_and_matching(
        self, mock_ffx_json: AsyncMock
    ) -> None:
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

        list_resp = await daemon.registry.handle(
            "driver-list", DriverListRequest(filter="xhci")
        )
        self.assertTrue(list_resp.success)
        assert isinstance(list_resp.body, DriverListResponse)
        self.assertEqual(len(list_resp.body.driver_hosts), 1)
        host = list_resp.body.driver_hosts[0]
        self.assertEqual(host.host_koid, 48291)
        self.assertTrue(host.colocated)
        self.assertTrue(host.transport_hazard)
        self.assertIn("dev.sys.platform.xhci", host.devices)

        # match_host_by_node empty and whitespace queries return (None, None)
        matched, drv = match_host_by_node(list_resp.body.driver_hosts, "")
        self.assertIsNone(matched)
        self.assertIsNone(drv)
        matched, drv = match_host_by_node(list_resp.body.driver_hosts, "   ")
        self.assertIsNone(matched)
        self.assertIsNone(drv)
        matched, drv = match_driver_host(list_resp.body.driver_hosts, node="")
        self.assertIsNone(matched)
        self.assertIsNone(drv)
        matched, drv = match_driver_host(
            list_resp.body.driver_hosts, node="   "
        )
        self.assertIsNone(matched)
        self.assertIsNone(drv)


if __name__ == "__main__":
    unittest.main()
