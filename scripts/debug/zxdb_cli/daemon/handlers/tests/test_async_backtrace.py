# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.async_backtrace import (
    AsyncBacktraceRequest,
    AsyncBacktraceResponse,
)
from zxdb_dap import AsyncTaskNode


class TestAsyncBacktraceHandler(DaemonTestCase):
    def test_async_backtrace_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("async-backtrace", daemon.registry.handlers)

    async def test_handle_async_backtrace_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn("Not connected to zxdb DAP server", resp.message or "")

    async def test_handle_async_backtrace_no_processes(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn("No active processes found", resp.message or "")

    async def test_handle_async_backtrace_ambiguous_processes(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_process(1002, name="proc2")
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn(
            "Multiple processes active; please specify a PID with --pid / -p",
            resp.message or "",
        )

    async def test_handle_async_backtrace_pid_not_found(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest(pid=9999)
        )
        self.assertFalse(resp.success)
        self.assertIn("Process 9999 not found", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_no_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock(body=Mock(threads=[]))
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(resp.body.tasks, [])

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_syncs_threads_when_empty(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")

        mock_thread = Mock()
        mock_thread.id = 3001
        mock_thread.name = "initial-thread"
        mock_thread.process_id = 1001

        mock_threads_resp = Mock()
        mock_threads_resp.body = Mock()
        mock_threads_resp.body.threads = [mock_thread]
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = [
            AsyncTaskNode(
                id="t_synced",
                name="synced_task",
                file="main.rs",
                line=10,
                children=[],
            )
        ]
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            mock_ensure_stopped.assert_called_once_with(1001)
            mock_dap_client.zxdb_threads.assert_called_once()
            mock_dap_client.zxdb_async_backtrace.assert_called_once()
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(len(resp.body.tasks), 1)
            self.assertEqual(resp.body.tasks[0].id, "t_synced")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_success_explicit_pid(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = [
            AsyncTaskNode(
                id="t1",
                name="root_task",
                file="main.rs",
                line=42,
                children=[],
            )
        ]
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            mock_ensure_stopped.assert_called_once_with(1001)
            mock_dap_client.zxdb_async_backtrace.assert_called_once()
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(len(resp.body.tasks), 1)
            self.assertEqual(resp.body.tasks[0].id, "t1")
            self.assertEqual(resp.body.tasks[0].name, "root_task")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_success_default_single_process(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = []
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest()
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(resp.body.tasks, [])

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_returns_first_successful_thread(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)
        daemon.get_or_create_thread(2002, name="thread2", process_id=1001)

        mock_resp_1 = Mock()
        mock_resp_1.success = True
        mock_resp_1.body = Mock()
        mock_resp_1.body.tasks = [
            AsyncTaskNode(
                id="t1",
                name="parent_task",
                file="foo.rs",
                line=10,
                children=[
                    AsyncTaskNode(
                        id="t2",
                        name="child_task",
                        file="bar.rs",
                        line=20,
                        children=[],
                    )
                ],
            )
        ]

        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_resp_1
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(mock_dap_client.zxdb_async_backtrace.call_count, 1)
            self.assertEqual(len(resp.body.tasks), 1)
            parent = resp.body.tasks[0]
            self.assertIsInstance(parent, AsyncTaskNode)
            self.assertEqual(parent.name, "parent_task")
            self.assertEqual(len(parent.children), 1)
            child = parent.children[0]
            self.assertIsInstance(child, AsyncTaskNode)
            self.assertEqual(child.name, "child_task")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_fallback_to_second_thread_when_first_empty(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)
        daemon.get_or_create_thread(2002, name="thread2", process_id=1001)

        mock_resp_1 = Mock()
        mock_resp_1.success = True
        mock_resp_1.body = Mock()
        mock_resp_1.body.tasks = []

        mock_resp_2 = Mock()
        mock_resp_2.success = True
        mock_resp_2.body = Mock()
        mock_resp_2.body.tasks = [
            AsyncTaskNode(
                id="t3",
                name="thread2_task",
                file="baz.rs",
                line=30,
                children=[],
            )
        ]

        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            side_effect=[mock_resp_1, mock_resp_2]
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(mock_dap_client.zxdb_async_backtrace.call_count, 2)
            self.assertEqual(len(resp.body.tasks), 1)
            task = resp.body.tasks[0]
            self.assertEqual(task.name, "thread2_task")


if __name__ == "__main__":
    unittest.main()
