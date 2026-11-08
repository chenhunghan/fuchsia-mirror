# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from pydap.client import DapError
from pydap.dap_types import Breakpoint, StackFrame
from pydap.models import (
    DataBreakpointInfoResponse,
    DataBreakpointInfoResponseBody,
    SetDataBreakpointsResponse,
    SetDataBreakpointsResponseBody,
    StackTraceResponse,
    StackTraceResponseBody,
)
from shared.protocol import WatchpointInfo
from shared.protocol.break_request import BreakRequest
from shared.protocol.get_state import GetStateRequest, GetStateResponse
from shared.protocol.watch_request import WatchRequest


class TestBreakpointsHandler(DaemonTestCase):
    async def test_handle_break_success(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [{"id": 1, "verified": True, "line": 12}]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=12)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                resp.body,
                {"breakpoints": [{"id": 1, "verified": True, "line": 12}]},
            )
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {12}
            )
            mock_set_breakpoints.assert_called_once()

    async def test_handle_break_additive(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_breakpoints["/path/to/file.rs"] = {12}

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [
                    {"id": 1, "verified": True, "line": 12},
                    {"id": 2, "verified": True, "line": 24},
                ]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=24)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {12, 24}
            )
            mock_set_breakpoints.assert_called_once()
            args, _ = mock_set_breakpoints.call_args
            dap_args = args[0]
            self.assertEqual(len(dap_args.breakpoints), 2)
            self.assertEqual(dap_args.breakpoints[0].line, 12)
            self.assertEqual(dap_args.breakpoints[1].line, 24)

    async def test_handle_break_delete(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_breakpoints["/path/to/file.rs"] = {12, 24}

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [{"id": 1, "verified": True, "line": 24}]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=12, delete=True)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {24}
            )
            mock_set_breakpoints.assert_called_once()
            args, _ = mock_set_breakpoints.call_args
            dap_args = args[0]
            self.assertEqual(len(dap_args.breakpoints), 1)
            self.assertEqual(dap_args.breakpoints[0].line, 24)

    async def test_handle_watch_add_and_delete(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1, name="t1", process_id=1001)
        thread.is_stopped = True

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
            patch.object(
                daemon.dap_client, "zxdb_threads", new_callable=AsyncMock
            ) as mock_threads,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x1234:4",
                    description="my_var (4 bytes @ 0x1234)",
                    access_types=["write", "readWrite"],
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=3,
                type="response",
                request_seq=3,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(
                            id=1,
                            verified=True,
                            instruction_reference="0x1234",
                        )
                    ]
                ),
            )

            add_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                ),
            )
            self.assertTrue(add_resp.success)
            assert add_resp.body is not None
            self.assertEqual(add_resp.body["dataId"], "1001:0x1234:4")
            self.assertIn("1001:0x1234:4", daemon.active_watchpoints)
            self.assertEqual(
                daemon.active_watchpoints["1001:0x1234:4"],
                WatchpointInfo(
                    data_id="1001:0x1234:4",
                    expression="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                    verified=True,
                    message=None,
                ),
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {1: "1001:0x1234:4"}
            )

            # Verify get-state surfaces active watchpoints
            mock_threads.return_value = Mock(body=Mock(threads=[]))
            state_resp = await daemon.registry.handle(
                "get-state", GetStateRequest()
            )
            self.assertTrue(state_resp.success)
            assert isinstance(state_resp.body, GetStateResponse)
            self.assertEqual(
                state_resp.body.watchpoints,
                {
                    "1001:0x1234:4": WatchpointInfo(
                        data_id="1001:0x1234:4",
                        expression="my_var",
                        thread_id=1,
                        frame_index=0,
                        access_type="write",
                        verified=True,
                        message=None,
                    )
                },
            )

            # Delete watchpoint by data_id without re-querying dataBreakpointInfo
            mock_info.reset_mock()
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=4,
                type="response",
                request_seq=4,
                success=True,
                body=SetDataBreakpointsResponseBody(breakpoints=[]),
            )
            del_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="1001:0x1234:4", delete=True),
            )
            self.assertTrue(del_resp.success)
            mock_info.assert_not_called()
            self.assertEqual(len(daemon.active_watchpoints), 0)

    async def test_handle_watch_edge_cases(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        proc = daemon.get_or_create_process(1001, name="test_proc")
        t1 = daemon.get_or_create_thread(1, process_id=1001)
        t1.is_stopped = True
        proc.threads[1] = t1

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )

            # Omitting thread_id or access_type when setting is rejected
            missing_tid_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", access_type="write"),
            )
            self.assertFalse(missing_tid_resp.success)
            self.assertIn("thread_id", missing_tid_resp.message or "")

            missing_access_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", thread_id=1),
            )
            self.assertFalse(missing_access_resp.success)
            self.assertIn("access_type", missing_access_resp.message or "")
            mock_info.assert_not_called()

            # Address dereference expression delegates to data_breakpoint_info
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x2000:4",
                    description="*(uint32_t*)0x2000 (0x2000, 4 bytes)",
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[Breakpoint(id=1, verified=True)]
                ),
            )
            addr_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="*(uint32_t*)0x2000",
                    thread_id=1,
                    access_type="write",
                ),
            )
            self.assertTrue(addr_resp.success)
            mock_info.assert_called_once()
            self.assertIn("1001:0x2000:4", daemon.active_watchpoints)

            # Unverified watchpoint is recorded with verified=False. zxdb
            # recreates every data breakpoint on each setDataBreakpoints call,
            # so the existing watchpoint also gets a new id.
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x3000:4",
                    description="bad_var (0x3000, 4 bytes)",
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=3,
                type="response",
                request_seq=3,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(id=2, verified=True),
                        Breakpoint(
                            id=3,
                            verified=False,
                            message="No free hardware watchpoint slots.",
                        ),
                    ]
                ),
            )
            bad_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="bad_var", thread_id=1, access_type="readWrite"
                ),
            )
            self.assertTrue(bad_resp.success)
            self.assertIn("1001:0x3000:4", daemon.active_watchpoints)
            bad_wp = daemon.active_watchpoints["1001:0x3000:4"]
            self.assertFalse(bad_wp.verified)
            self.assertEqual(
                bad_wp.message, "No free hardware watchpoint slots."
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id,
                {2: "1001:0x2000:4", 3: "1001:0x3000:4"},
            )

            # Deleting a watchpoint refreshes the remaining ones.
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=4,
                type="response",
                request_seq=4,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(id=4, verified=False, message="lost slot")
                    ]
                ),
            )
            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name="1001:0x3000:4", delete=True)
            )
            self.assertTrue(del_resp.success)
            self.assertEqual(list(daemon.active_watchpoints), ["1001:0x2000:4"])
            remaining = daemon.active_watchpoints["1001:0x2000:4"]
            self.assertFalse(remaining.verified)
            self.assertEqual(remaining.message, "lost slot")
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {4: "1001:0x2000:4"}
            )

            # Process cleanup purges watchpoints belonging to that process
            daemon.clear_process(1001)
            self.assertEqual(len(daemon.active_watchpoints), 0)
            self.assertEqual(daemon.watchpoint_ids_by_bp_id, {})

    def test_breakpoint_changed_event_updates_watchpoint(self) -> None:
        daemon = Daemon(port=15678)
        daemon.set_watchpoints(
            {
                "1001:0x2000:4": WatchpointInfo(
                    data_id="1001:0x2000:4",
                    expression="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                )
            },
            [Breakpoint(id=7, verified=True)],
        )

        # Events for other breakpoint ids or other reasons are ignored.
        daemon._handle_breakpoint_changed(
            {"reason": "changed", "breakpoint": {"id": 8, "verified": False}}
        )
        daemon._handle_breakpoint_changed(
            {"reason": "new", "breakpoint": {"id": 7, "verified": False}}
        )
        self.assertTrue(daemon.active_watchpoints["1001:0x2000:4"].verified)

        # An async install failure marks the matching watchpoint unverified.
        daemon._handle_breakpoint_changed(
            {
                "reason": "changed",
                "breakpoint": {
                    "id": 7,
                    "verified": False,
                    "message": "Failed to install watchpoint",
                },
            }
        )
        wp = daemon.active_watchpoints["1001:0x2000:4"]
        self.assertFalse(wp.verified)
        self.assertEqual(wp.message, "Failed to install watchpoint")

    async def test_handle_watch_dap_error_reports_operation(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1001).is_stopped = True

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            # DapClient raises DapError for adapter rejections (success=false)
            # and timeouts; its message already names the request.
            mock_info.side_effect = DapError(
                "DAP request dataBreakpointInfo failed: no such variable"
            )
            set_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", thread_id=1, access_type="write"),
            )
            self.assertFalse(set_resp.success)
            self.assertEqual(
                set_resp.message,
                "Failed to set watchpoint on 'my_var': "
                "DAP request dataBreakpointInfo failed: no such variable",
            )
            mock_set.assert_not_called()
            self.assertEqual(daemon.active_watchpoints, {})

            mock_set.side_effect = DapError(
                "DAP request setDataBreakpoints failed: connection closed"
            )
            clear_resp = await daemon.registry.handle(
                "watch", WatchRequest(clear=True)
            )
            self.assertFalse(clear_resp.success)
            self.assertEqual(
                clear_resp.message,
                "Failed to clear watchpoints: "
                "DAP request setDataBreakpoints failed: connection closed",
            )

            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name="1001:0x9999:4", delete=True)
            )
            self.assertFalse(del_resp.success)
            self.assertIn("not found", del_resp.message or "")
            self.assertIn("active data_ids: none", del_resp.message or "")

    async def test_handle_watch_dap_error_preserves_state(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1001).is_stopped = True
        existing = WatchpointInfo(
            data_id="1001:0x1000:4",
            expression="a",
            thread_id=1,
            frame_index=0,
            access_type="write",
        )
        daemon.set_watchpoints(
            {existing.data_id: existing}, [Breakpoint(id=1, verified=True)]
        )

        def assert_state_unchanged() -> None:
            self.assertEqual(
                list(daemon.active_watchpoints), [existing.data_id]
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {1: existing.data_id}
            )

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x2000:4",
                    description="b (0x2000, 4 bytes)",
                ),
            )
            mock_set.side_effect = DapError(
                "DAP request setDataBreakpoints failed: no free slots"
            )

            # Failure in setDataBreakpoints while adding a watchpoint.
            set_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="b", thread_id=1, access_type="write"),
            )
            self.assertFalse(set_resp.success)
            self.assertEqual(
                set_resp.message,
                "Failed to set watchpoint on 'b': "
                "DAP request setDataBreakpoints failed: no free slots",
            )
            assert_state_unchanged()

            # Failure in setDataBreakpoints while deleting a watchpoint.
            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name=existing.data_id, delete=True)
            )
            self.assertFalse(del_resp.success)
            self.assertEqual(
                del_resp.message,
                "Failed to delete watchpoint: "
                "DAP request setDataBreakpoints failed: no free slots",
            )
            assert_state_unchanged()

            # Failure in setDataBreakpoints while clearing watchpoints.
            clear_resp = await daemon.registry.handle(
                "watch", WatchRequest(clear=True)
            )
            self.assertFalse(clear_resp.success)
            assert_state_unchanged()

            # Failure in stackTrace before dataBreakpointInfo is sent.
            mock_info.reset_mock()
            mock_stack.side_effect = DapError(
                "DAP request stackTrace failed: thread gone"
            )
            stack_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="b", thread_id=1, access_type="write"),
            )
            self.assertFalse(stack_resp.success)
            self.assertEqual(
                stack_resp.message,
                "Failed to set watchpoint on 'b': "
                "DAP request stackTrace failed: thread gone",
            )
            mock_info.assert_not_called()
            assert_state_unchanged()

            # Failure when pausing the thread.
            mock_stack.reset_mock()
            with patch.object(
                daemon,
                "ensure_stopped",
                new_callable=AsyncMock,
                side_effect=DapError("DAP request pause failed: busy"),
            ):
                pause_resp = await daemon.registry.handle(
                    "watch",
                    WatchRequest(name="b", thread_id=1, access_type="write"),
                )
            self.assertFalse(pause_resp.success)
            self.assertEqual(
                pause_resp.message,
                "Failed to set watchpoint on 'b': "
                "pause thread 1 failed: DAP request pause failed: busy",
            )
            mock_stack.assert_not_called()
            assert_state_unchanged()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_break_function(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(48291, name="driver_host:pdev")

        mock_fn_resp = Mock(
            success=True,
            body=Mock(
                breakpoints=[Mock(id=201, verified=True)],
                dump_dap=lambda: {
                    "breakpoints": [{"id": 201, "verified": True}]
                },
            ),
        )
        mock_dap_client.set_function_breakpoints = AsyncMock(
            return_value=mock_fn_resp
        )

        resp = await daemon.registry.handle(
            "break",
            BreakRequest(
                function="usb_xhci::UsbXhci::Init", condition="status != 0"
            ),
        )
        self.assertTrue(resp.success)
        self.assertEqual(
            daemon.active_function_breakpoints["usb_xhci::UsbXhci::Init"],
            "status != 0",
        )
        mock_dap_client.set_function_breakpoints.assert_called_once()


if __name__ == "__main__":
    unittest.main()
