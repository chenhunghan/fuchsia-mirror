# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.handlers import start
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.start import StartRequest


class TestStartHandler(DaemonTestCase):
    @patch("daemon.handlers.start.portpicker.pick_unused_port")
    async def test_start_handler_port_allocation(self, mock_pick: Mock) -> None:
        mock_pick.return_value = 54321
        daemon = Daemon(port=None)

        # Select an unused port dynamically when starting a new session without
        # an explicit port.
        req1 = StartRequest(connect=False)
        with patch.object(
            daemon, "_check_already_running", return_value=None
        ), patch.object(
            daemon, "_start_dap_server", new_callable=AsyncMock
        ), patch.object(
            daemon, "_connect_to_dap", new_callable=AsyncMock, return_value=True
        ):
            resp1 = await start.handle(daemon, req1)
            self.assertTrue(resp1.success)
            self.assertEqual(daemon.port, 54321)

        # Fall back to DEFAULT_DAP_PORT when attaching without an explicit port.
        daemon2 = Daemon(port=None)
        req2 = StartRequest(connect=True)
        with patch.object(
            daemon2, "_check_already_running", return_value=None
        ), patch.object(
            daemon2,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp2 = await start.handle(daemon2, req2)
            self.assertTrue(resp2.success)
            self.assertEqual(daemon2.port, 15678)

        # Honor the user-specified custom port when connecting to an
        # existing DAP server.
        daemon3 = Daemon(port=None)
        req3 = StartRequest(connect=True, port=9999)
        with patch.object(
            daemon3, "_check_already_running", return_value=None
        ), patch.object(
            daemon3,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp3 = await start.handle(daemon3, req3)
            self.assertTrue(resp3.success)
            self.assertEqual(daemon3.port, 9999)

        # Respect explicitly provided port configuration.
        daemon4 = Daemon(port=None)
        req4 = StartRequest(connect=False, port=9999)
        with patch.object(
            daemon4, "_check_already_running", return_value=None
        ), patch.object(
            daemon4, "_start_dap_server", new_callable=AsyncMock
        ), patch.object(
            daemon4,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp4 = await start.handle(daemon4, req4)
            self.assertTrue(resp4.success)
            self.assertEqual(daemon4.port, 9999)


if __name__ == "__main__":
    unittest.main()
