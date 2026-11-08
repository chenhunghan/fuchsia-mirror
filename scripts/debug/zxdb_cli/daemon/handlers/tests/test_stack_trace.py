# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from pydap.dap_types import Source
from shared.protocol.stack_trace import (
    ProcessStackTraceResponse,
    StackTraceRequest,
    ThreadStackTraceResponse,
)
from zxdb_dap import ZxdbDapClient, ZxdbStackFrame, ZxdbStackTraceArguments


class TestStackTraceHandler(DaemonTestCase):
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_elides_subtle_frames(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
        ]
        mock_stack_resp.body.total_frames = 2
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            self.assertEqual(resp.body.thread_id, 1)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 1)
            self.assertIn("0…1 «Rust panic»", frames[0].name)
            self.assertEqual(frames[0].frame_index, 0)

            # Raw flag returns uncollapsing frames
            resp_raw = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=True)
            )
            self.assertTrue(resp_raw.success)
            assert isinstance(resp_raw.body, ThreadStackTraceResponse)
            self.assertEqual(len(resp_raw.body.stack_frames), 2)
            self.assertEqual(resp_raw.body.stack_frames[0].frame_index, 0)
            self.assertEqual(resp_raw.body.stack_frames[1].frame_index, 1)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_subtle_non_subtle_subtle(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=3,
                name="f3",
                line=3,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=4,
                name="f4",
                line=4,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="C++ stdlib"),
            ),
            ZxdbStackFrame(
                id=5,
                name="f5",
                line=5,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="C++ stdlib"),
            ),
        ]
        mock_stack_resp.body.total_frames = 5
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 3)
            self.assertIn("0…1 «Rust panic»", frames[0].name)
            self.assertEqual(frames[0].frame_index, 0)
            self.assertEqual(frames[1].name, "f3")
            self.assertEqual(frames[1].frame_index, 2)
            self.assertIn("3…4 «C++ stdlib»", frames[2].name)
            self.assertEqual(frames[2].frame_index, 3)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_non_subtle_subtle_non_subtle(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=3,
                name="f3",
                line=3,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=4,
                name="f4",
                line=4,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=5,
                name="f5",
                line=5,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp.body.total_frames = 5
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 4)
            self.assertEqual(frames[0].name, "f1")
            self.assertEqual(frames[0].frame_index, 0)
            self.assertEqual(frames[1].name, "f2")
            self.assertEqual(frames[1].frame_index, 1)
            self.assertIn("2…3 «Rust panic»", frames[2].name)
            self.assertEqual(frames[2].frame_index, 2)
            self.assertEqual(frames[3].name, "f5")
            self.assertEqual(frames[3].frame_index, 4)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_unwinder_and_trust(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=10,
                column=1,
                presentation_hint="normal",
                source=None,
                trust="FP",
            ),
        ]
        mock_stack_resp.body.total_frames = 1
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace",
                StackTraceRequest(
                    thread_id=1,
                    force_remote_unwind=True,
                    unwinder="fp",
                ),
            )

            self.assertTrue(resp.success)
            mock_dap_client.zxdb_stack_trace.assert_called_once_with(
                ZxdbStackTraceArguments(
                    thread_id=1,
                    remote_unwind=True,
                    unwinder="fp",
                )
            )
            assert isinstance(resp.body, ThreadStackTraceResponse)
            self.assertEqual(len(resp.body.stack_frames), 1)
            self.assertEqual(resp.body.stack_frames[0].trust, "FP")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_exception(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        # Mock _send_request returning invalid response structure that fails model_validate
        mock_dap_client._send_request = AsyncMock(
            return_value={"invalid": "response_structure"}
        )
        # Real zxdb_stack_trace calls _send_request and model_validate
        mock_dap_client.zxdb_stack_trace = (
            lambda args: ZxdbDapClient.zxdb_stack_trace(mock_dap_client, args)
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1)
            )

            self.assertFalse(resp.success)
            self.assertIsNotNone(resp.message)
            self.assertIn("Failed to get stack trace", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_success(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp_1 = Mock()
        mock_stack_resp_1.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
        ]
        mock_stack_resp_1.body.total_frames = 2

        mock_stack_resp_2 = Mock()
        mock_stack_resp_2.body.stack_frames = [
            ZxdbStackFrame(
                id=3,
                name="worker_func",
                line=3,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp_2.body.total_frames = 1

        mock_dap_client.zxdb_stack_trace = AsyncMock(
            side_effect=[mock_stack_resp_1, mock_stack_resp_2]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")
        daemon.get_or_create_thread(
            101, name="initial-thread", process_id=12345
        ).is_stopped = True
        daemon.get_or_create_thread(
            102, name="worker-thread", process_id=12345
        ).is_stopped = True

        resp = await daemon.registry.handle(
            "stack-trace", StackTraceRequest(pid=12345, raw=False)
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)

        assert isinstance(resp.body, ProcessStackTraceResponse)
        self.assertEqual(resp.body.process_id, 12345)
        threads = resp.body.stacks
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0].thread_id, 101)
        self.assertEqual(len(threads[0].stack_frames), 1)
        self.assertIn("0…1 «Rust panic»", threads[0].stack_frames[0].name)
        self.assertEqual(threads[0].stack_frames[0].frame_index, 0)
        self.assertEqual(threads[1].thread_id, 102)
        self.assertEqual(len(threads[1].stack_frames), 1)
        self.assertEqual(threads[1].stack_frames[0].name, "worker_func")
        self.assertEqual(threads[1].stack_frames[0].frame_index, 0)

    async def test_handle_stack_trace_pid_not_found(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(pid=99999, raw=False)
            )
            self.assertFalse(resp.success)
            self.assertIn(
                "No threads found for process 99999", resp.message or ""
            )
            mock_ensure_stopped.assert_called_once_with(99999)

    async def test_handle_stack_trace_pid_no_threads(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(pid=12345)
            )
            self.assertFalse(resp.success)
            self.assertIn(
                "No threads found for process 12345", resp.message or ""
            )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_partial_thread_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp_1 = Mock()
        mock_stack_resp_1.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="func_ok",
                line=1,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp_1.body.total_frames = 1

        mock_dap_client.zxdb_stack_trace = AsyncMock(
            side_effect=[mock_stack_resp_1, Exception("Thread exited")]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")
        daemon.get_or_create_thread(
            101, name="thread-ok", process_id=12345
        ).is_stopped = True
        daemon.get_or_create_thread(
            102, name="thread-exited", process_id=12345
        ).is_stopped = True

        resp = await daemon.registry.handle(
            "stack-trace", StackTraceRequest(pid=12345, raw=False)
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)

        assert isinstance(resp.body, ProcessStackTraceResponse)
        self.assertEqual(len(resp.body.stacks), 2)
        self.assertEqual(resp.body.stacks[0].thread_id, 101)
        self.assertEqual(len(resp.body.stacks[0].stack_frames), 1)
        self.assertEqual(resp.body.stacks[1].thread_id, 102)
        self.assertEqual(len(resp.body.stacks[1].stack_frames), 0)
        self.assertEqual(resp.body.stacks[1].total_frames, 0)


if __name__ == "__main__":
    unittest.main()
