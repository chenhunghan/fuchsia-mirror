# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from pydap.client import DapError
from shared.protocol.console import ConsoleRequest, ConsoleResponse
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from zxdb_dap import ZxdbConsoleCommandArguments


class TestEvaluateAndConsoleHandler(DaemonTestCase):
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_success_no_children(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_frame.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        # Mock evaluate
        mock_eval_resp = Mock()
        mock_eval_resp.success = True
        mock_eval_resp.body.result = "10"
        mock_eval_resp.body.type = "int"
        mock_eval_resp.body.variables_reference = 0
        mock_dap_client.evaluate = AsyncMock(return_value=mock_eval_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, EvaluateResponse)
        self.assertEqual(resp.body.result, "10")
        self.assertEqual(resp.body.type, "int")
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.evaluate.assert_called_once()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = []
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertFalse(resp.success)
        self.assertIn("No stack frames found", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_thread_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = False

        def on_pause(*_args: Any, **_kwargs: Any) -> dict[str, Any]:
            daemon.event_waiter.notify_thread_stop(
                1,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause"},
                },
            )
            return {"success": True}

        mock_dap_client.pause_thread = AsyncMock(side_effect=on_pause)

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_frame.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        # Mock evaluate
        mock_eval_resp = Mock()
        mock_eval_resp.success = True
        mock_eval_resp.body.result = "10"
        mock_eval_resp.body.type = "int"
        mock_eval_resp.body.variables_reference = 0
        mock_dap_client.evaluate = AsyncMock(return_value=mock_eval_resp)

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, EvaluateResponse)
        self.assertEqual(resp.body.result, "10")
        self.assertEqual(resp.body.type, "int")
        mock_dap_client.pause_thread.assert_called_once()
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.evaluate.assert_called_once()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_without_thread_id(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_cmd_resp = Mock()
        mock_cmd_resp.success = True
        mock_cmd_resp.body.output = "Process 1 symbol status\n"
        mock_dap_client.zxdb_console_command = AsyncMock(
            return_value=mock_cmd_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="sym-stat"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, ConsoleResponse)
        self.assertEqual(resp.body.output, "Process 1 symbol status\n")
        mock_dap_client.stack_trace.assert_not_called()
        mock_dap_client.zxdb_console_command.assert_called_once_with(
            ZxdbConsoleCommandArguments(command="sym-stat", frame_id=None)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_with_thread_and_frame(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame_0 = Mock()
        mock_frame_0.id = 41
        mock_frame_1 = Mock()
        mock_frame_1.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame_0, mock_frame_1]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        mock_cmd_resp = Mock()
        mock_cmd_resp.success = True
        mock_cmd_resp.body.output = "rax = 0x1\n"
        mock_dap_client.zxdb_console_command = AsyncMock(
            return_value=mock_cmd_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=1, frame_index=1),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, ConsoleResponse)
        self.assertEqual(resp.body.output, "rax = 0x1\n")
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.zxdb_console_command.assert_called_once_with(
            ZxdbConsoleCommandArguments(command="regs", frame_id=42)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_frame_out_of_range(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame_0 = Mock()
        mock_frame_0.id = 41
        mock_stack_resp.body.stack_frames = [mock_frame_0]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=1, frame_index=5),
        )

        self.assertFalse(resp.success)
        self.assertIn("out of range", resp.message or "")
        mock_dap_client.zxdb_console_command.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_frame_index_without_thread_id(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=None, frame_index=2),
        )

        self.assertFalse(resp.success)
        self.assertIn("thread_id is required", resp.message or "")
        mock_dap_client.zxdb_console_command.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.zxdb_console_command = AsyncMock(
            side_effect=DapError("not a valid verb")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="bad_cmd"),
        )

        self.assertFalse(resp.success)
        self.assertIn("not a valid verb", resp.message or "")


if __name__ == "__main__":
    unittest.main()
