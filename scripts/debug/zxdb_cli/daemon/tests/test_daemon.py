# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from typing import Any
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import CommandHandlerRegistry, Daemon
from daemon.state import Process, Thread
from daemon.tests.helpers import DaemonTestCase
from pydap.models import PauseArguments
from pydap.models import Response as DapResponse
from shared.protocol import BaseRequest, Response
from shared.protocol.attach import AttachRequest
from shared.protocol.continue_request import ContinueRequest
from zxdb_dap import ZxdbPauseArguments


class TestDaemonCore(DaemonTestCase):
    async def test_register_and_handle(self) -> None:
        registry = CommandHandlerRegistry()

        async def mock_handler(_req: BaseRequest[Any]) -> Response[Any]:
            return Response(success=True, body={"data": "handled"})

        registry.register("test_cmd", mock_handler)

        resp = await registry.handle(
            "test_cmd", BaseRequest[Any](command="test_cmd")
        )

        self.assertTrue(resp.success)
        self.assertEqual(resp.body, {"data": "handled"})

    async def test_unknown_command(self) -> None:
        registry = CommandHandlerRegistry()
        resp = await registry.handle(
            "unknown", BaseRequest[Any](command="unknown")
        )

        self.assertFalse(resp.success)
        self.assertIsNotNone(resp.message)
        self.assertIn("Unknown command", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_request_waits_for_startup_lock_instead_of_failing(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies requests arriving while start is executing wait on _command_lock.

        Otherwise, it would show "Not connected to zxdb DAP server" because of an uninitialized zxdb_writer.
        """
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None  # DAP server not yet connected

        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.attach = AsyncMock(
            return_value=Mock(dump_dap=lambda: {"success": True})
        )

        # Acquire the command lock during simulated start initialization
        await daemon._command_lock.acquire()

        # Release the lock after simulated DAP connection delay
        async def complete_dap_startup_after_delay() -> None:
            await asyncio.sleep(0.05)
            daemon.zxdb_writer = Mock()
            daemon._command_lock.release()

        startup_task = asyncio.create_task(complete_dap_startup_after_delay())

        # Dispatch an operational command while startup is still locked.
        # It should block for 50ms, acquire the lock after startup completes, and succeed.
        req = AttachRequest(filter="my_process")
        async with daemon._command_lock:
            resp = await daemon.registry.handle("attach", req)

        self.assertTrue(resp.success)
        mock_dap_client.attach.assert_called_once()
        await startup_task

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_commands_execute_in_strict_fifo_order(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies multiple commands arriving concurrently execute in FIFO order without interleaving."""
        execution_log: list[str] = []

        async def delayed_attach_side_effect(
            *_args: Any, **_kwargs: Any
        ) -> Mock:
            execution_log.append("attach_start")
            await asyncio.sleep(0.05)
            execution_log.append("attach_end")
            return Mock(dump_dap=lambda: {"success": True})

        async def continue_side_effect(*_args: Any, **_kwargs: Any) -> Mock:
            execution_log.append("continue_start")
            await asyncio.sleep(0.01)
            execution_log.append("continue_end")
            return Mock(dump_dap=lambda: {"success": True})

        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.attach = AsyncMock(
            side_effect=delayed_attach_side_effect
        )
        mock_dap_client.continue_thread = AsyncMock(
            side_effect=continue_side_effect
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        async def dispatch(cmd: str, req: Any) -> Response[Any]:
            async with daemon._command_lock:
                return await daemon.registry.handle(cmd, req)

        # Launch attach first (takes 50ms)
        task1 = asyncio.create_task(
            dispatch("attach", AttachRequest(filter="proc1"))
        )
        # Ensure task1 acquires the lock first
        await asyncio.sleep(0.005)
        # Launch continue second (takes 10ms); should wait for attach to finish
        task2 = asyncio.create_task(
            dispatch("continue", ContinueRequest(thread_id=1))
        )

        resp1, resp2 = await asyncio.gather(task1, task2)

        self.assertTrue(resp1.success)
        self.assertTrue(resp2.success)
        self.assertEqual(
            execution_log,
            ["attach_start", "attach_end", "continue_start", "continue_end"],
        )

    def test_update_resumed_threads_single_thread_none_raises(self) -> None:
        daemon = Daemon(port=15678)
        with self.assertRaises(ValueError) as ctx:
            daemon.update_resumed_threads(None, single_thread=True)
        self.assertIn(
            "Single thread resumed without providing thread_id!",
            str(ctx.exception),
        )

    def test_process_resume(self) -> None:
        daemon = Daemon(port=15678)
        proc = daemon.get_or_create_process(1234)
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True
        self.assertTrue(proc.all_threads_stopped)

        proc.resume()
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)
        self.assertFalse(proc.all_threads_stopped)

    def test_thread_resume(self) -> None:
        daemon = Daemon(port=15678)
        thread = daemon.get_or_create_thread(1, process_id=1234)
        thread.is_stopped = True

        thread.resume()
        self.assertFalse(thread.is_stopped)

    def test_thread_resume_without_process(self) -> None:
        daemon = Daemon(port=15678)
        thread = daemon.get_or_create_thread(1)
        thread.is_stopped = True

        thread.resume()
        self.assertFalse(thread.is_stopped)

    async def test_update_thread_cache_evicts_stale_threads(self) -> None:
        daemon = Daemon(port=15678)
        daemon.get_or_create_thread(1, process_id=1234)
        daemon.get_or_create_thread(2, process_id=1234)
        daemon.get_or_create_thread(3, process_id=5678)

        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.process_id = 1234

        daemon.update_thread_cache([mock_thread1])

        self.assertEqual(list(daemon.threads.keys()), [1])
        assert daemon.threads[1].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)

    def test_thread_and_process_equality(self) -> None:
        p1 = Process(id=123, name="p1")
        t1 = Thread(id=1, name="t1", is_stopped=False, process=p1)
        p1.threads[1] = t1

        p2 = Process(id=123, name="p2")
        t2 = Thread(id=1, name="t2", is_stopped=True, process=p2)
        p2.threads[1] = t2

        # Verify comparisons succeed without recursion error and match on ID only.
        self.assertEqual(t1, t2)
        self.assertEqual(p1, p2)

        t3 = Thread(id=2, process=p1)
        p3 = Process(id=456)
        self.assertNotEqual(t1, t3)
        self.assertNotEqual(p1, p3)

    def test_update_resumed_threads_clears_orphan_threads(self) -> None:
        """Verifies whole-system resume clears stopped state on orphan threads."""
        daemon = Daemon(port=15678)
        bound_thread = daemon.get_or_create_thread(
            101, process_id=1001, is_stopped=True
        )
        orphan_thread = daemon.get_or_create_thread(
            102, process_id=None, is_stopped=True
        )
        self.assertTrue(bound_thread.is_stopped)
        self.assertTrue(orphan_thread.is_stopped)

        daemon.update_resumed_threads(thread_id=101, single_thread=False)

        self.assertFalse(bound_thread.is_stopped)
        self.assertFalse(orphan_thread.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_already_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.get_or_create_process(12345)
        daemon.get_or_create_thread(1, process_id=12345).is_stopped = True

        await daemon.ensure_process_stopped(12345)
        mock_dap_client.zxdb_pause_process.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.zxdb_pause_process = AsyncMock(
            return_value=DapResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=False,
                message="Process not found",
            )
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn("Process not found", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_when_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        proc = daemon.get_or_create_process(12345)
        thread = daemon.get_or_create_thread(1, process_id=12345)
        self.assertFalse(proc.all_threads_stopped)

        async def mock_pause_side_effect(
            args: ZxdbPauseArguments,
        ) -> DapResponse:
            assert args.process_id is not None
            thread.is_stopped = True
            daemon.event_waiter.notify_process_stop(
                args.process_id,
                {
                    "type": "event",
                    "event": "processStopped",
                    "body": {"processId": args.process_id, "name": "p1"},
                },
            )
            return DapResponse(
                seq=1, type="response", request_seq=1, success=True
            )

        mock_dap_client.zxdb_pause_process = AsyncMock(
            side_effect=mock_pause_side_effect
        )

        await daemon.ensure_process_stopped(12345)
        mock_dap_client.zxdb_pause_process.assert_called_once_with(
            ZxdbPauseArguments(process_id=12345)
        )
        self.assertTrue(proc.all_threads_stopped)

    async def test_ensure_process_stopped_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn("Not connected to zxdb DAP server", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    @patch("daemon.daemon.asyncio.wait_for", side_effect=asyncio.TimeoutError)
    async def test_ensure_process_stopped_timeout(
        self, _mock_wait_for: Mock, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.zxdb_pause_process = AsyncMock(
            return_value=DapResponse(
                seq=1, type="response", request_seq=1, success=True
            )
        )
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn(
            "Timed out waiting for process 12345 to stop", str(ctx.exception)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_stopped_already_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.get_or_create_thread(1).is_stopped = True

        await daemon.ensure_stopped(1)
        mock_dap_client.pause_thread.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_stopped_when_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1)
        self.assertFalse(thread.is_stopped)

        async def mock_pause_thread_side_effect(
            args: PauseArguments,
        ) -> dict[str, Any]:
            assert args.thread_id is not None
            thread.is_stopped = True
            daemon.event_waiter.notify_thread_stop(
                args.thread_id,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause", "threadId": args.thread_id},
                },
            )
            return {"success": True}

        mock_dap_client.pause_thread = AsyncMock(
            side_effect=mock_pause_thread_side_effect
        )

        await daemon.ensure_stopped(1)
        mock_dap_client.pause_thread.assert_called_once_with(
            PauseArguments(threadId=1)
        )
        self.assertTrue(thread.is_stopped)

    async def test_ensure_stopped_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_stopped(1)
        self.assertIn("Not connected to zxdb DAP server", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    @patch("daemon.daemon.asyncio.wait_for", side_effect=asyncio.TimeoutError)
    async def test_ensure_stopped_timeout(
        self, _mock_wait_for: Mock, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.pause_thread = AsyncMock(return_value={"success": True})
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_stopped(1)
        self.assertIn(
            "Timed out waiting for thread 1 to stop", str(ctx.exception)
        )

    @patch("daemon.daemon.UDS_PATH")
    @patch("daemon.daemon.asyncio.start_unix_server")
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_run_cleanup_detach_on_existing_session(
        self,
        mock_dap_client_class: Mock,
        mock_start_unix_server: Mock,
        mock_uds_path: Mock,
    ) -> None:
        mock_uds_path.exists.return_value = False
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.zxdb_detach = AsyncMock()
        mock_dap_client.close = AsyncMock()

        daemon = Daemon(port=15678)
        daemon.connect_to_existing = True
        daemon.zxdb_writer = Mock()  # Simulate active connection

        # Mock the unix server
        mock_server = AsyncMock()
        mock_server.close = Mock()  # close is synchronous
        mock_start_unix_server.return_value = mock_server

        # Start run() in a task
        run_task = asyncio.create_task(daemon.run())

        # Let it run and reach the wait
        await asyncio.sleep(0.05)

        # Trigger stop
        daemon.stop_event.set()

        # Wait for run to complete
        await run_task

        # Verify zxdb_detach was called with all=True
        mock_dap_client.zxdb_detach.assert_called_once()
        args, _ = mock_dap_client.zxdb_detach.call_args
        self.assertTrue(args[0].detach_all)

    @patch("daemon.daemon.FfxCmd")
    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_uses_ephemeral_repository_name(
        self, mock_package_server: Mock, mock_ffx_cmd: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-deadbeef12345678"
        )
        mock_package_server.start = AsyncMock(return_value=Mock())
        mock_ffx_cmd.return_value.start = AsyncMock(
            side_effect=RuntimeError("stop before signal wait")
        )

        daemon = Daemon(port=15678)
        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertEqual(daemon.repo_name, "zxdb-deadbeef12345678")
        mock_package_server.ephemeral_repository_name.assert_called_once_with(
            "zxdb"
        )
        mock_package_server.start.assert_awaited_once_with(
            "zxdb-deadbeef12345678"
        )

    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_does_not_set_repo_name_on_start_failure(
        self, mock_package_server: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-deadbeef12345678"
        )
        mock_package_server.start = AsyncMock(
            side_effect=RuntimeError("package server failed")
        )

        daemon = Daemon(port=15678)
        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertIsNone(daemon.repo_name)
        self.assertIsNone(daemon.package_server_proc)

    @patch("daemon.daemon.FfxCmd")
    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_early_exit(
        self, mock_package_server: Mock, mock_ffx_cmd: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-123"
        )
        mock_package_server.start = AsyncMock()

        # Simulate early exit by having run_to_completion() return quickly
        # without sending SIGUSR1.
        mock_dap_proc = AsyncMock()
        mock_output = Mock()
        mock_output.return_code = 123
        mock_dap_proc.run_to_completion = AsyncMock(return_value=mock_output)
        mock_ffx_cmd.return_value.start = AsyncMock(return_value=mock_dap_proc)

        daemon = Daemon(port=15678)

        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertEqual(resp.message, "zxdb exited early with code 123")


if __name__ == "__main__":
    unittest.main()
