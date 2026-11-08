# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.handlers.diagnose_hang import classify_thread_stack
from daemon.tests.helpers import DaemonTestCase
from pydap.dap_types import Source, StackFrame
from shared.protocol.diagnose_hang import (
    DiagnoseHangRequest,
    DiagnoseHangResponse,
)
from zxdb_dap import AsyncTaskNode


class TestDiagnoseHangHandler(DaemonTestCase):
    async def test_diagnose_hang_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("diagnose-hang", daemon.registry.handlers)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_diagnose_hang(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        proc = daemon.get_or_create_process(48291, name="driver_host:xhci")
        daemon.get_or_create_thread(
            101, process_id=48291, name="fdf-worker-0"
        ).is_stopped = True
        daemon.get_or_create_thread(
            102, process_id=48291, name="fdf-worker-1"
        ).is_stopped = True
        daemon.get_or_create_thread(
            103, process_id=48291, name="irq-thread"
        ).is_stopped = True

        mock_t1 = Mock(id=101, process_id=48291)
        mock_t1.name = "fdf-worker-0"
        mock_t2 = Mock(id=102, process_id=48291)
        mock_t2.name = "fdf-worker-1"
        mock_t3 = Mock(id=103, process_id=48291)
        mock_t3.name = "irq-thread"
        mock_dap_client.zxdb_threads = AsyncMock(
            return_value=Mock(body=Mock(threads=[mock_t1, mock_t2, mock_t3]))
        )

        async def stack_side_effect(args: Any) -> Mock:
            tid = args.thread_id
            if tid == 101:
                frames = [
                    StackFrame(
                        id=1001,
                        name="sync_mutex_lock",
                        line=42,
                        column=1,
                        source=Source(name="mutex.c"),
                    ),
                    StackFrame(
                        id=1002,
                        name="usb_xhci::UsbXhci::QueueTrb",
                        line=210,
                        column=1,
                        source=Source(name="usb-xhci.cc"),
                    ),
                ]
            elif tid == 102:
                frames = [
                    StackFrame(
                        id=2001,
                        name="_zx_channel_call",
                        line=10,
                        column=1,
                        source=Source(name="syscalls.cc"),
                    ),
                    StackFrame(
                        id=2002,
                        name="fidl::WireSyncClient<fuchsia_hardware_pci::Device>::GetBar",
                        line=55,
                        column=1,
                        source=Source(name="pci.h"),
                    ),
                ]
            else:
                frames = [
                    StackFrame(
                        id=3001,
                        name="_zx_port_wait",
                        line=15,
                        column=1,
                        source=Source(name="syscalls.cc"),
                    )
                ]
            return Mock(body=Mock(stack_frames=frames))

        mock_dap_client.stack_trace = AsyncMock(side_effect=stack_side_effect)
        mock_dap_client.continue_thread = AsyncMock(
            return_value=Mock(
                success=True, body=Mock(all_threads_continued=True)
            )
        )
        executed_console_commands: list[str] = []

        async def console_side_effect(args: Any) -> Mock:
            executed_console_commands.append(args.command)
            return Mock(
                success=True,
                body=Mock(
                    output=(
                        "  Handle  Type                        Koid\n"
                        "  0x1003  ZX_OBJ_TYPE_INTERRUPT       0x292e\n"
                        "  0x1005  ZX_OBJ_TYPE_CHANNEL         0x3010\n"
                        "  0x1009  ZX_OBJ_TYPE_PORT            0x4020\n"
                    )
                ),
            )

        mock_dap_client.zxdb_console_command = AsyncMock(
            side_effect=console_side_effect
        )
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=Mock(
                success=True,
                body=Mock(
                    tasks=[
                        AsyncTaskNode(
                            id="1",
                            name="usb_xhci::UsbXhci::HandleIrq",
                            file="usb-xhci.cc",
                            line=210,
                        )
                    ]
                ),
            )
        )

        # Target is already stopped, so the handler does not auto-resume it.
        hang_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertTrue(hang_resp.success)
        assert isinstance(hang_resp.body, DiagnoseHangResponse)
        self.assertFalse(hang_resp.body.auto_resumed)
        self.assertTrue(proc.all_threads_stopped)
        self.assertEqual(len(hang_resp.body.threads), 3)
        self.assertEqual(len(hang_resp.body.async_tasks), 1)
        classifications = {
            t.thread_id: t.classification for t in hang_resp.body.threads
        }
        self.assertEqual(
            classifications,
            {
                101: "blocked_lock",
                102: "blocked_ipc",
                103: "port_wait",
            },
        )
        self.assertEqual(len(hang_resp.body.interrupt_handles), 1)
        self.assertEqual(len(hang_resp.body.channel_handles), 1)
        self.assertEqual(len(hang_resp.body.port_handles), 1)
        self.assertTrue(
            any("LOCK CONTENTION" in s for s in hang_resp.body.summary)
        )
        self.assertTrue(
            any("SYNC IPC WAIT" in s for s in hang_resp.body.summary)
        )
        self.assertTrue(any("ASYNC TASKS" in s for s in hang_resp.body.summary))
        mock_dap_client.continue_thread.assert_not_called()
        self.assertNotIn("process 1 continue", executed_console_commands)

        # Target is running, so the handler pauses and auto-resumes it.
        executed_console_commands.clear()
        proc.resume()
        self.assertFalse(proc.all_threads_stopped)

        async def pause_side_effect(args: Any) -> Mock:
            daemon.event_waiter.notify_process_stop(args.process_id, {})
            return Mock(success=True)

        mock_dap_client.zxdb_pause_process = AsyncMock(
            side_effect=pause_side_effect
        )

        hang_resp2 = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertTrue(hang_resp2.success)
        assert isinstance(hang_resp2.body, DiagnoseHangResponse)
        self.assertTrue(hang_resp2.body.auto_resumed)
        self.assertFalse(proc.all_threads_stopped)
        mock_dap_client.zxdb_pause_process.assert_called_once()
        mock_dap_client.continue_thread.assert_not_called()
        self.assertIn("process 1 continue", executed_console_commands)

        # Disabling auto_resume leaves the running target stopped after inspection.
        mock_dap_client.zxdb_pause_process.reset_mock()
        executed_console_commands.clear()
        hang_resp_no_resume = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest(auto_resume=False)
        )
        self.assertTrue(hang_resp_no_resume.success)
        assert isinstance(hang_resp_no_resume.body, DiagnoseHangResponse)
        self.assertFalse(hang_resp_no_resume.body.auto_resumed)
        mock_dap_client.zxdb_pause_process.assert_called_once()
        self.assertNotIn("process 1 continue", executed_console_commands)

        # Unknown --pid is rejected without pausing or resuming anything.
        mock_dap_client.zxdb_pause_process.reset_mock()
        executed_console_commands.clear()
        unknown_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest(pid=99999)
        )
        self.assertFalse(unknown_resp.success)
        self.assertEqual(unknown_resp.message, "Process 99999 not found")
        mock_dap_client.zxdb_pause_process.assert_not_called()
        mock_dap_client.continue_thread.assert_not_called()
        self.assertEqual(executed_console_commands, [])

        # Missing attached process returns an error.
        empty_daemon = Daemon(port=15678)
        empty_daemon.zxdb_writer = Mock()
        no_proc_resp = await empty_daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertFalse(no_proc_resp.success)
        self.assertEqual(
            no_proc_resp.message,
            "No attached process; attach to a process first or specify --pid",
        )

        # Multiple active processes require an explicit --pid.
        proc2 = daemon.get_or_create_process(88888, name="driver_host:other")
        daemon.get_or_create_thread(
            201, process_id=88888, name="worker-other"
        ).is_stopped = True
        multi_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertFalse(multi_resp.success)
        self.assertIn("Multiple processes active", multi_resp.message or "")

        # Multi-process scoping resumes only the inspected target process.
        proc.resume()
        self.assertFalse(proc.all_threads_stopped)
        self.assertTrue(proc2.all_threads_stopped)

        mock_t_other = Mock(id=201, process_id=88888)
        mock_t_other.name = "worker-other"
        all_threads = [mock_t1, mock_t2, mock_t3, mock_t_other]
        mock_dap_client.zxdb_threads = AsyncMock(
            return_value=Mock(body=Mock(threads=all_threads))
        )
        mock_dap_client.zxdb_pause_process.reset_mock()
        executed_console_commands.clear()

        p1_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest(pid=48291)
        )
        self.assertTrue(p1_resp.success)
        assert isinstance(p1_resp.body, DiagnoseHangResponse)
        self.assertEqual(p1_resp.body.pid, 48291)
        self.assertTrue(p1_resp.body.auto_resumed)
        mock_dap_client.continue_thread.assert_not_called()
        self.assertEqual(
            executed_console_commands,
            ["process 1 handle -x", "process 1 continue"],
        )
        self.assertTrue(proc2.all_threads_stopped)

        # Falls back to cached process threads when DAP threads omit process_id.
        mock_t_no_pid = Mock(id=201, process_id=None)
        mock_t_no_pid.name = "worker-other"
        mock_dap_client.zxdb_threads = AsyncMock(
            return_value=Mock(body=Mock(threads=[mock_t_no_pid]))
        )
        proc2_stacks_collected: list[int] = []

        async def proc2_stack_side_effect(args: Any) -> Mock:
            proc2_stacks_collected.append(args.thread_id)
            return Mock(
                body=Mock(
                    stack_frames=[
                        StackFrame(
                            id=5001,
                            name="_zx_port_wait",
                            line=1,
                            column=1,
                            source=Source(name="port.cc"),
                        )
                    ]
                )
            )

        mock_dap_client.stack_trace = AsyncMock(
            side_effect=proc2_stack_side_effect
        )
        executed_console_commands.clear()
        p2_fallback_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest(pid=88888)
        )
        self.assertTrue(p2_fallback_resp.success)
        assert isinstance(p2_fallback_resp.body, DiagnoseHangResponse)
        self.assertEqual(p2_fallback_resp.body.pid, 88888)
        self.assertIn(201, proc2_stacks_collected)
        self.assertIn("process 2 handle -x", executed_console_commands)
        self.assertFalse(p2_fallback_resp.body.auto_resumed)
        self.assertNotIn("process 2 continue", executed_console_commands)
        self.assertTrue(proc2.all_threads_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_diagnose_hang_fdf_dispatcher_tasks(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Merges per-thread fdf dispatcher roots and ignores idle ones."""
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(48291, name="driver_host:virtio")
        mock_threads = []
        for tid in (101, 102, 103):
            daemon.get_or_create_thread(
                tid, process_id=48291, name=f"fdf-worker-{tid}"
            ).is_stopped = True
            mock_thread = Mock(id=tid, process_id=48291)
            mock_thread.name = f"fdf-worker-{tid}"
            mock_threads.append(mock_thread)
        mock_dap_client.zxdb_threads = AsyncMock(
            return_value=Mock(body=Mock(threads=mock_threads))
        )
        mock_dap_client.stack_trace = AsyncMock(
            return_value=Mock(
                body=Mock(
                    stack_frames=[
                        StackFrame(
                            id=1,
                            name="_zx_port_wait",
                            line=15,
                            column=1,
                            source=Source(name="syscalls.cc"),
                        )
                    ]
                )
            )
        )
        mock_dap_client.zxdb_console_command = AsyncMock(
            return_value=Mock(success=True, body=Mock(output=""))
        )

        read_task = AsyncTaskNode(
            name="virtio::Device::ReadBlock", file="block.cc", line=40
        )
        irq_task = AsyncTaskNode(
            name="virtio::Device::IrqWorker", file="device.cc", line=88
        )
        per_thread_tasks: dict[int, list[AsyncTaskNode]] = {}

        async def abt_side_effect(args: Any) -> Mock:
            return Mock(
                success=True,
                body=Mock(tasks=per_thread_tasks.get(args.thread_id, [])),
            )

        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            side_effect=abt_side_effect
        )

        # Threads sharing a dispatcher each report part of its queue.
        per_thread_tasks.update(
            {
                101: [
                    AsyncTaskNode(
                        id="0xd1", name="fdf::Dispatcher", children=[read_task]
                    )
                ],
                102: [
                    AsyncTaskNode(
                        id="0xd1",
                        name="fdf::Dispatcher",
                        children=[read_task, irq_task],
                    )
                ],
                103: [AsyncTaskNode(id="0xd2", name="fdf::Dispatcher")],
            }
        )
        hang_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertTrue(hang_resp.success)
        assert isinstance(hang_resp.body, DiagnoseHangResponse)
        roots = {t.id: t for t in hang_resp.body.async_tasks}
        self.assertEqual(set(roots), {"0xd1", "0xd2"})
        self.assertEqual(roots["0xd1"].children, [read_task, irq_task])
        self.assertEqual(roots["0xd2"].children, [])
        self.assertIn(
            "ASYNC TASKS: 2 top-level async task(s) active or suspended in"
            " executor.",
            hang_resp.body.summary,
        )

        # An idle driver host reports only empty dispatcher scopes.
        per_thread_tasks.clear()
        per_thread_tasks.update(
            {
                101: [AsyncTaskNode(id="0xd1", name="fdf::Dispatcher")],
                102: [AsyncTaskNode(id="0xd1", name="fdf::Dispatcher")],
                103: [AsyncTaskNode(id="0xd2", name="fdf::Dispatcher")],
            }
        )
        idle_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertTrue(idle_resp.success)
        assert isinstance(idle_resp.body, DiagnoseHangResponse)
        self.assertEqual(len(idle_resp.body.async_tasks), 2)
        self.assertEqual(
            idle_resp.body.summary,
            [
                "No blocked locks, sync IPCs, or unserviced interrupts"
                " detected (3/3 thread(s) idle in zx_port_wait)."
            ],
        )

        # Distinct tasks without IDs are kept rather than collapsed.
        per_thread_tasks.clear()
        per_thread_tasks.update(
            {101: [read_task], 102: [irq_task], 103: [read_task]}
        )
        no_id_resp = await daemon.registry.handle(
            "diagnose-hang", DiagnoseHangRequest()
        )
        self.assertTrue(no_id_resp.success)
        assert isinstance(no_id_resp.body, DiagnoseHangResponse)
        self.assertEqual(no_id_resp.body.async_tasks, [read_task, irq_task])

    def test_classify_thread_stack_caller_frame_indexing(self) -> None:
        """Verifies _find_caller_frame searches caller frames after matched index."""
        frames = [
            StackFrame(
                id=1,
                name="inner_unrelated_wrapper",
                line=5,
                column=1,
                source=Source(name="wrapper.cc"),
            ),
            StackFrame(
                id=2,
                name="sync_mutex_lock",
                line=42,
                column=1,
                source=Source(name="mutex.c"),
            ),
            StackFrame(
                id=3,
                name="fbl::Mutex::Acquire",
                line=12,
                column=1,
                source=Source(name="mutex.h"),
            ),
            StackFrame(
                id=4,
                name="usb_xhci::UsbXhci::QueueTrb",
                line=210,
                column=1,
                source=Source(name="usb-xhci.cc"),
            ),
        ]
        classified = classify_thread_stack(101, "worker", frames)
        self.assertEqual(classified.classification, "blocked_lock")
        self.assertEqual(classified.blocking_symbol, "sync_mutex_lock")
        self.assertIn(
            "called from usb_xhci::UsbXhci::QueueTrb", classified.summary
        )
        self.assertNotIn(
            "called from inner_unrelated_wrapper", classified.summary
        )


if __name__ == "__main__":
    unittest.main()
