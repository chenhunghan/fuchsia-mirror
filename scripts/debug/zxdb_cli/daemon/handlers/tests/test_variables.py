# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.variables import VariablesRequest


class TestVariablesHandler(DaemonTestCase):
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_success(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_frame.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        # Mock scopes
        mock_scopes_resp = Mock()
        mock_scope1 = Mock()
        mock_scope1.name = "Locals"
        mock_scope1.variables_reference = 100
        mock_scope2 = Mock()
        mock_scope2.name = "Arguments"
        mock_scope2.variables_reference = 101
        mock_scope3 = Mock()
        mock_scope3.name = "Globals"
        mock_scope3.variables_reference = 102
        mock_scopes_resp.body.scopes = [mock_scope1, mock_scope2, mock_scope3]
        mock_dap_client.scopes = AsyncMock(return_value=mock_scopes_resp)

        # Mock variables for Locals (100)
        mock_vars_resp1 = Mock()
        mock_var1 = Mock()
        mock_var1.name = "x"
        mock_var1.value = "1"
        mock_var1.type = "int"
        mock_vars_resp1.body.variables = [mock_var1]

        # Mock variables for Arguments (101)
        mock_vars_resp2 = Mock()
        mock_var2 = Mock()
        mock_var2.name = "y"
        mock_var2.value = "2"
        mock_var2.type = "int"
        mock_vars_resp2.body.variables = [mock_var2]

        # variables is called twice, once for 100, once for 101
        mock_dap_client.variables = AsyncMock(
            side_effect=[mock_vars_resp1, mock_vars_resp2]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=0)
            )

            self.assertTrue(resp.success)
            self.assertEqual(
                resp.body,
                {
                    "variables": [
                        {"name": "x", "value": "1", "type": "int"},
                        {"name": "y", "value": "2", "type": "int"},
                    ]
                },
            )
            mock_ensure_stopped.assert_called_once_with(1)
            mock_dap_client.stack_trace.assert_called_once()
            mock_dap_client.scopes.assert_called_once()
            self.assertEqual(mock_dap_client.variables.call_count, 2)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_no_frames(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = []
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=0)
            )

            self.assertFalse(resp.success)
            self.assertIn("No stack frames found", resp.message or "")
            mock_ensure_stopped.assert_called_once_with(1)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_frame_out_of_bounds(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=5)
            )

            self.assertFalse(resp.success)
            self.assertIn("Frame index 5 out of range", resp.message or "")
            mock_ensure_stopped.assert_called_once_with(1)


if __name__ == "__main__":
    unittest.main()
