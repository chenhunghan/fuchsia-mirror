# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from pydap.client import DapError
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.step_in import StepInRequest


class TestExecutionHandlers(DaemonTestCase):
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_continue(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.body = Mock()
        mock_continue_resp.body.all_threads_continued = True
        mock_continue_resp.dump_dap.return_value = {"allThreadsContinued": True}
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1, process_id=1234)
        thread.is_stopped = True

        resp = await daemon.registry.handle(
            "continue", ContinueRequest(thread_id=1)
        )

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()
        self.assertFalse(thread.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_continue_single_thread(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.body = Mock()
        mock_continue_resp.body.all_threads_continued = False
        mock_continue_resp.dump_dap.return_value = {
            "allThreadsContinued": False
        }
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "continue", ContinueRequest(thread_id=1, single_thread=True)
        )

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_finish(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_finish_resp = Mock()
        mock_finish_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.step_out = AsyncMock(return_value=mock_finish_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "finish",
            FinishRequest(command="finish", thread_id=1),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.step_out.assert_called_once()
        args = mock_dap_client.step_out.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_finish_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_finish_resp = Mock()
        mock_finish_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.step_out = AsyncMock(return_value=mock_finish_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "finish",
            FinishRequest(command="finish", thread_id=1, single_thread=False),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.step_out.assert_called_once()
        args = mock_dap_client.step_out.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_next_resp = Mock()
        mock_next_resp.success = True
        mock_next_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.next = AsyncMock(return_value=mock_next_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "next",
            NextRequest(
                command="next",
                thread_id=1,
                granularity="line",
            ),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.next.assert_called_once()
        args = mock_dap_client.next.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_next_resp = Mock()
        mock_next_resp.success = True
        mock_next_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.next = AsyncMock(return_value=mock_next_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "next",
            NextRequest(
                command="next",
                thread_id=1,
                single_thread=False,
            ),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.next.assert_called_once()
        args = mock_dap_client.next.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.next = AsyncMock(
            side_effect=DapError("Thread not stopped")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "next", NextRequest(command="next", thread_id=1)
        )
        self.assertFalse(resp.success)
        self.assertIn("Thread not stopped", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_response = Mock()
        mock_dap_response.success = True
        mock_dap_response.dump_dap.return_value = {"success": True}
        mock_dap_client.step_in = AsyncMock(return_value=mock_dap_response)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "step-in",
            StepInRequest(command="step-in", thread_id=1),
        )

        self.assertTrue(resp.success)
        mock_dap_client.step_in.assert_called_once()
        args = mock_dap_client.step_in.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_response = Mock()
        mock_dap_response.success = True
        mock_dap_response.dump_dap.return_value = {"success": True}
        mock_dap_client.step_in = AsyncMock(return_value=mock_dap_response)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "step-in",
            StepInRequest(command="step-in", thread_id=1, single_thread=False),
        )

        self.assertTrue(resp.success)
        mock_dap_client.step_in.assert_called_once()
        args = mock_dap_client.step_in.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.step_in = AsyncMock(
            side_effect=DapError("Thread not stopped")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "step-in", StepInRequest(command="step-in", thread_id=1)
        )

        self.assertFalse(resp.success)
        self.assertIn("Thread not stopped", resp.message or "")


if __name__ == "__main__":
    unittest.main()
