# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from shared.protocol.get_state import GetStateRequest, GetStateResponse
from shared.protocol.threads import ThreadsRequest


class TestThreadsAndStateHandler(DaemonTestCase):
    def test_threads_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("threads", daemon.registry.handlers)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_threads(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_thread1.process_id = 1234
        mock_thread2 = Mock()
        mock_thread2.id = 2
        mock_thread2.name = "worker"
        mock_thread2.process_id = 1234
        mock_body.threads = [mock_thread1, mock_thread2]
        mock_body.model_dump.return_value = {
            "threads": [
                {"id": 1, "name": "main", "processId": 1234},
                {"id": 2, "name": "worker", "processId": 1234},
            ]
        }
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle("threads", ThreadsRequest())

        if not resp.success:
            print(f"Test failed with message: {resp.message}")
        self.assertTrue(resp.success)
        assert resp.body is not None

        threads = resp.body["threads"]
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0]["id"], 1)
        self.assertEqual(threads[0]["name"], "main")
        self.assertEqual(threads[1]["id"], 2)
        self.assertEqual(threads[1]["name"], "worker")

        assert daemon.threads[1].process is not None
        assert daemon.threads[2].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)
        self.assertEqual(daemon.threads[2].process.id, 1234)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_threads_empty_prunes_cache(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle(threads) prunes cached threads when DAP returns an empty list."""
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_body.threads = []
        mock_body.model_dump.return_value = {"threads": []}
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1234)

        resp = await daemon.registry.handle("threads", ThreadsRequest())

        self.assertTrue(resp.success)
        self.assertEqual(daemon.threads, {})

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state(self, mock_dap_client_class: Mock) -> None:
        """Verifies handle_get_state successfully queries threads and returns
        GetStateResponse.
        """
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_thread1.process_id = 1234
        mock_body.threads = [mock_thread1]
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1234, name="test_process")
        daemon.active_breakpoints = {"/path/to/file.rs": {24, 12}}

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(len(state_resp.threads), 1)
        self.assertEqual(state_resp.threads[0].id, 1)
        self.assertEqual(state_resp.threads[0].name, "main")
        self.assertEqual(state_resp.processes, {1234: "test_process"})
        self.assertEqual(state_resp.breakpoints, {"/path/to/file.rs": [12, 24]})

        assert daemon.threads[1].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state_defensive(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle_get_state gracefully handles None threads response
        body.
        """
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_threads_resp.body = None  # Simulate missing DAP body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1234, name="test_process")

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(
            len(state_resp.threads), 0
        )  # Successfully defaulted to empty list without crashing
        self.assertEqual(state_resp.processes, {1234: "test_process"})

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state_empty_threads_prunes_cache(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle_get_state prunes cached threads when DAP returns an empty list."""
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_body.threads = []
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1234)
        daemon.get_or_create_process(1234, name="test_process")

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(len(state_resp.threads), 0)
        self.assertEqual(daemon.threads, {})


if __name__ == "__main__":
    unittest.main()
