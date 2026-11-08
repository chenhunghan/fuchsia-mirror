# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from unittest.mock import AsyncMock, patch

from daemon.daemon import Daemon
from daemon.tests.helpers import DaemonTestCase
from pydap.client import READER_STOPPED_EVENT
from pydap.dap_types import Source, StackFrame
from pydap.models import (
    DataBreakpointInfoResponse,
    DataBreakpointInfoResponseBody,
    StackTraceResponse,
    StackTraceResponseBody,
)
from shared.protocol import (
    TracepointInfo,
    TracepointRequest,
)
from shared.protocol.get_state import GetStateRequest
from shared.protocol.wait_for_event import WaitForEventRequest
from zxdb_dap import (
    ZxdbSetTracepointsResponse,
    ZxdbSetTracepointsResponseBody,
    ZxdbThreadsResponse,
    ZxdbThreadsResponseBody,
    ZxdbTracepointInfo,
)


class TestTracepointHandler(DaemonTestCase):
    @staticmethod
    def _make_set_tracepoints_resp(
        tracepoints: list[ZxdbTracepointInfo],
    ) -> ZxdbSetTracepointsResponse:
        return ZxdbSetTracepointsResponse(
            seq=1,
            type="response",
            request_seq=1,
            success=True,
            command="zxdb.SetTracepoints",
            body=ZxdbSetTracepointsResponseBody(tracepoints=tracepoints),
        )

    async def test_tracepoint_list_empty(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        req = TracepointRequest(list_all=True)
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertTrue(resp.success)
        assert resp.body is not None
        self.assertEqual(resp.body.tracepoints, [])

    async def test_tracepoint_add_and_sync(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=True, hit_count=0, max_hits=5
                    )
                ]
            )
        )
        daemon.dap_client = mock_dap

        req = TracepointRequest(
            function="foo::Bar",
            expressions=["val"],
            capture=["frame", "locals"],
            max_matching_frames=5,
            max_hits=5,
        )
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertTrue(resp.success)
        assert resp.body is not None
        self.assertEqual(len(resp.body.tracepoints), 1)
        self.assertEqual(resp.body.tracepoints[0].function, "foo::Bar")
        self.assertEqual(resp.body.tracepoints[0].max_matching_frames, 5)
        self.assertTrue(resp.body.tracepoints[0].verified)
        mock_dap.zxdb_set_tracepoints.assert_called_once()
        specs = mock_dap.zxdb_set_tracepoints.call_args[0][0]
        self.assertEqual(specs[0].max_matching_frames, 5)

    async def test_tracepoint_watchpoint_address_skips_data_info(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=True, hit_count=0, max_hits=20
                    )
                ]
            )
        )
        daemon.dap_client = mock_dap

        req = TracepointRequest(watch="0x1234", size=4, thread_id=1)
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertTrue(resp.success)
        assert resp.body is not None
        self.assertEqual(len(resp.body.tracepoints), 1)
        self.assertIsNone(resp.body.tracepoints[0].data_id)
        mock_dap.data_breakpoint_info.assert_not_called()
        specs = mock_dap.zxdb_set_tracepoints.call_args[0][0]
        self.assertIsNone(specs[0].watch.data_id)
        self.assertEqual(specs[0].watch.address, 0x1234)
        self.assertEqual(specs[0].watch.byte_size, 4)

    async def test_tracepoint_watchpoint_hex_bounds_rejected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        daemon.dap_client = AsyncMock()

        for bad_addr in ("0xffffffffffffffff", "-0x1000"):
            resp = await daemon.registry.handle(
                "tracepoint", TracepointRequest(watch=bad_addr, size=4)
            )
            self.assertFalse(resp.success)
            self.assertEqual(len(daemon.tracepoints), 0)

    async def test_tracepoint_watchpoint_expression(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=True, hit_count=0, max_hits=20
                    )
                ]
            )
        )
        mock_dap.data_breakpoint_info.return_value = DataBreakpointInfoResponse(
            seq=1,
            type="response",
            request_seq=1,
            success=True,
            command="dataBreakpointInfo",
            body=DataBreakpointInfoResponseBody(
                data_id="1001:0x2000:4", description="my_var"
            ),
        )
        mock_dap.stack_trace.return_value = StackTraceResponse(
            seq=2,
            type="response",
            request_seq=2,
            success=True,
            command="stackTrace",
            body=StackTraceResponseBody(
                stack_frames=[
                    StackFrame(
                        id=42,
                        name="main",
                        line=10,
                        column=0,
                        source=Source(path="main.cc"),
                    )
                ]
            ),
        )
        daemon.dap_client = mock_dap

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            req = TracepointRequest(watch="my_var", thread_id=1)
            resp = await daemon.registry.handle("tracepoint", req)
            self.assertTrue(resp.success)
            assert resp.body is not None
            self.assertEqual(len(resp.body.tracepoints), 1)
            self.assertEqual(resp.body.tracepoints[0].data_id, "1001:0x2000:4")

        mock_dap.data_breakpoint_info.reset_mock()
        mock_dap.stack_trace.reset_mock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=True, hit_count=0, max_hits=50
                    )
                ]
            )
        )
        update_resp = await daemon.registry.handle(
            "tracepoint",
            TracepointRequest(tracepoint_id=1, watch="my_var", max_hits=50),
        )
        self.assertTrue(update_resp.success)
        assert update_resp.body is not None
        self.assertEqual(
            update_resp.body.tracepoints[0].data_id, "1001:0x2000:4"
        )
        self.assertEqual(update_resp.body.tracepoints[0].max_hits, 50)
        mock_dap.data_breakpoint_info.assert_not_called()
        mock_dap.stack_trace.assert_not_called()

    async def test_tracepoint_watch_expression_requires_thread_id(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        daemon.dap_client = AsyncMock()

        req = TracepointRequest(watch="my_var")
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertFalse(resp.success)
        self.assertIn("thread_id", resp.message or "")
        self.assertEqual(len(daemon.tracepoints), 0)

    async def test_tracepoint_watch_malformed_hex_requires_thread_id(
        self,
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        daemon.dap_client = AsyncMock()

        req = TracepointRequest(watch="0xZZZ")
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertFalse(resp.success)
        self.assertIn("thread_id", resp.message or "")
        self.assertEqual(len(daemon.tracepoints), 0)

    async def test_tracepoint_delete_failure_rollback(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.side_effect = RuntimeError(
            "DAP delete sync failed"
        )
        daemon.dap_client = mock_dap

        tp = TracepointInfo(
            id=1,
            location="my_func",
            function="my_func",
            max_hits=10,
            hit_count=3,
        )
        daemon.tracepoints[1] = tp

        req = TracepointRequest(delete=True, tracepoint_id=1)
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertFalse(resp.success)
        self.assertIn("DAP delete sync failed", resp.message or "")
        self.assertIn(1, daemon.tracepoints)
        self.assertEqual(daemon.tracepoints[1].max_hits, 10)

    async def test_tracepoint_event_updates_state_and_wakes_waiter(
        self,
    ) -> None:
        daemon = Daemon(port=15678)
        tp = TracepointInfo(id=1, location="foo.cc:10", file="foo.cc", line=10)
        daemon.tracepoints[1] = tp

        wait_task = asyncio.create_task(
            daemon.registry.handle(
                "wait-for-event",
                WaitForEventRequest(last_seen_seq=0, timeout=2.0),
            )
        )
        await asyncio.sleep(0)
        await daemon.event_queue.put(
            {
                "type": "event",
                "event": "zxdb.Tracepoint",
                "body": {
                    "tracepointId": 1,
                    "hitCount": 5,
                    "autoDisabled": True,
                },
            }
        )
        await daemon.event_queue.put({"type": READER_STOPPED_EVENT})
        await daemon._process_events()

        wait_resp = await wait_task
        self.assertTrue(wait_resp.success)
        assert wait_resp.events is not None
        self.assertEqual(len(wait_resp.events), 1)
        self.assertEqual(wait_resp.events[0]["event"], "zxdb.Tracepoint")
        self.assertEqual(tp.hit_count, 5)
        self.assertFalse(tp.enabled)
        self.assertTrue(tp.auto_disabled)

    async def test_get_state_includes_tracepoints(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_threads.return_value = ZxdbThreadsResponse(
            seq=1,
            type="response",
            request_seq=1,
            success=True,
            command="threads",
            body=ZxdbThreadsResponseBody(threads=[]),
        )
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1, location="foo.cc:10", file="foo.cc", line=10
        )

        resp = await daemon.registry.handle("get-state", GetStateRequest())
        self.assertTrue(resp.success)
        assert resp.body is not None
        self.assertEqual(len(resp.body.tracepoints), 1)
        self.assertEqual(resp.body.tracepoints[0].id, 1)

    async def test_tracepoint_delete_and_clear(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp([])
        )
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1, location="my_func", function="my_func"
        )
        daemon.tracepoints[2] = TracepointInfo(
            id=2, location="other_func", function="other_func"
        )

        resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(delete=True, tracepoint_id=1)
        )
        self.assertTrue(resp.success)
        self.assertNotIn(1, daemon.tracepoints)
        self.assertIn(2, daemon.tracepoints)

        clear_resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(clear=True)
        )
        self.assertTrue(clear_resp.success)
        self.assertEqual(daemon.tracepoints, {})
        mock_dap.zxdb_set_tracepoints.assert_called_with([])

    async def test_tracepoint_delete_non_existent(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        req = TracepointRequest(delete=True, tracepoint_id=99)
        resp = await daemon.registry.handle("tracepoint", req)
        self.assertFalse(resp.success)
        self.assertEqual(
            resp.message, "No matching tracepoint found to delete."
        )

    async def test_tracepoint_update_and_enable_toggle(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1,
                        verified=True,
                        hit_count=7,
                        max_hits=20,
                        enabled=True,
                        auto_disabled=False,
                    )
                ]
            )
        )
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1,
            location="my_func",
            function="my_func",
            max_hits=10,
            hit_count=7,
        )

        resp = await daemon.registry.handle(
            "tracepoint",
            TracepointRequest(tracepoint_id=1, function="my_func", max_hits=20),
        )
        self.assertTrue(resp.success)
        self.assertEqual(daemon.tracepoints[1].max_hits, 20)
        self.assertEqual(daemon.tracepoints[1].hit_count, 7)

        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=True, enabled=False, auto_disabled=False
                    )
                ]
            )
        )
        dis_resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(tracepoint_id=1, enabled=False)
        )
        self.assertTrue(dis_resp.success)
        self.assertFalse(daemon.tracepoints[1].enabled)

    async def test_tracepoint_rejects_cross_id_location_collision(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        daemon.dap_client = AsyncMock()
        daemon.tracepoints[1] = TracepointInfo(
            id=1, location="foo", function="foo"
        )
        daemon.tracepoints[2] = TracepointInfo(
            id=2, location="bar", function="bar"
        )

        resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(tracepoint_id=1, function="bar")
        )
        self.assertFalse(resp.success)
        self.assertIn("tracepoint 2", resp.message or "")
        self.assertEqual(daemon.tracepoints[1].function, "foo")

    async def test_tracepoint_per_item_validation_error_rolls_back(
        self,
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        daemon.dap_client = mock_dap

        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1,
                        verified=False,
                        message="Watchpoints must be 1, 2, 4, or 8 bytes.",
                    )
                ]
            )
        )
        new_resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(watch="0x1000", size=3)
        )
        self.assertFalse(new_resp.success)
        self.assertIn("Watchpoints must be", new_resp.message or "")
        self.assertEqual(daemon.tracepoints, {})

        daemon.tracepoints[1] = TracepointInfo(
            id=1, location="watch:0x1000", watch="0x1000", size=4, verified=True
        )
        mock_dap.zxdb_set_tracepoints.side_effect = [
            self._make_set_tracepoints_resp(
                [
                    ZxdbTracepointInfo(
                        id=1, verified=False, message="Invalid size."
                    )
                ]
            ),
            self._make_set_tracepoints_resp(
                [ZxdbTracepointInfo(id=1, verified=True)]
            ),
        ]
        upd_resp = await daemon.registry.handle(
            "tracepoint",
            TracepointRequest(tracepoint_id=1, watch="0x1000", size=3),
        )
        self.assertFalse(upd_resp.success)
        self.assertEqual(daemon.tracepoints[1].size, 4)

    async def test_clear_process_evicts_data_id_tracepoints(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.return_value = (
            self._make_set_tracepoints_resp(
                [ZxdbTracepointInfo(id=2, verified=True)]
            )
        )
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1,
            location="watch:my_var",
            watch="my_var",
            data_id="1001:0x2000:4",
        )
        daemon.tracepoints[2] = TracepointInfo(
            id=2, location="foo", function="foo"
        )
        daemon.tracepoints[3] = TracepointInfo(
            id=3,
            location="watch:other",
            watch="other",
            data_id="2002:0x3000:4",
        )

        await daemon.event_queue.put(
            {"type": "event", "event": "detached", "body": {"pid": 1001}}
        )
        await daemon.event_queue.put({"type": READER_STOPPED_EVENT})
        await daemon._process_events()

        self.assertNotIn(1, daemon.tracepoints)
        self.assertIn(2, daemon.tracepoints)
        self.assertIn(3, daemon.tracepoints)
        mock_dap.zxdb_set_tracepoints.assert_called_once()

        daemon.clear_all_processes()
        self.assertEqual(list(daemon.tracepoints.keys()), [2])

    async def test_exited_event_keeps_data_id_tracepoints(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1,
            location="watch:my_var",
            watch="my_var",
            data_id="1001:0x2000:4",
        )

        await daemon.event_queue.put(
            {"type": "event", "event": "exited", "body": {"exitCode": 0}}
        )
        await daemon.event_queue.put({"type": READER_STOPPED_EVENT})
        await daemon._process_events()

        self.assertIn(1, daemon.tracepoints)
        mock_dap.zxdb_set_tracepoints.assert_not_called()

    def _mock_watch_dap(self, data_ids: list[str]) -> AsyncMock:
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.side_effect = lambda specs: (
            self._make_set_tracepoints_resp(
                [ZxdbTracepointInfo(id=s.id, verified=True) for s in specs]
            )
        )
        mock_dap.data_breakpoint_info.side_effect = [
            DataBreakpointInfoResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                command="dataBreakpointInfo",
                body=DataBreakpointInfoResponseBody(
                    data_id=data_id, description="x"
                ),
            )
            for data_id in data_ids
        ]
        return mock_dap

    async def test_tracepoint_same_watch_expression_on_two_threads(
        self,
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        daemon.dap_client = self._mock_watch_dap(
            ["1001:0x2000:4", "1001:0x3000:4"]
        )

        with patch(
            "daemon.handlers.tracepoint.resolve_frame_id",
            new_callable=AsyncMock,
            return_value=42,
        ):
            first = await daemon.registry.handle(
                "tracepoint", TracepointRequest(watch="x", thread_id=1)
            )
            second = await daemon.registry.handle(
                "tracepoint", TracepointRequest(watch="x", thread_id=2)
            )

        self.assertTrue(first.success)
        self.assertTrue(second.success)
        self.assertEqual(
            {tp.data_id for tp in daemon.tracepoints.values()},
            {"1001:0x2000:4", "1001:0x3000:4"},
        )

        del_resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(delete=True, watch="x")
        )
        self.assertFalse(del_resp.success)
        self.assertIn("Delete by tracepoint ID", del_resp.message or "")
        self.assertEqual(len(daemon.tracepoints), 2)

    async def test_tracepoint_sync_keeps_server_auto_disabled_state(
        self,
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.side_effect = lambda specs: (
            self._make_set_tracepoints_resp(
                [ZxdbTracepointInfo(id=s.id, verified=True) for s in specs]
            )
        )
        daemon.dap_client = mock_dap
        daemon.tracepoints[1] = TracepointInfo(
            id=1,
            location="foo",
            function="foo",
            enabled=False,
            auto_disabled=True,
        )
        daemon.tracepoints[2] = TracepointInfo(
            id=2, location="bar", function="bar"
        )
        daemon.tracepoints[3] = TracepointInfo(
            id=3, location="baz", function="baz", enabled=False
        )
        daemon.next_tracepoint_id = 4

        resp = await daemon.registry.handle(
            "tracepoint", TracepointRequest(function="qux")
        )
        self.assertTrue(resp.success)

        specs = {s.id: s for s in mock_dap.zxdb_set_tracepoints.call_args[0][0]}
        # Auto-disabled and enabled tracepoints omit the field so that the
        # server keeps its own state, including a hit-limit disable that the
        # daemon has not seen yet.
        self.assertIsNone(specs[1].enabled)
        self.assertIsNone(specs[2].enabled)
        self.assertFalse(specs[3].enabled)
        self.assertTrue(specs[4].enabled)

        await daemon.registry.handle(
            "tracepoint", TracepointRequest(tracepoint_id=1, enabled=True)
        )
        specs = {s.id: s for s in mock_dap.zxdb_set_tracepoints.call_args[0][0]}
        self.assertTrue(specs[1].enabled)
        self.assertIsNone(specs[4].enabled)

    async def test_tracepoint_sync_failure_rollback(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = AsyncMock()
        mock_dap = AsyncMock()
        mock_dap.zxdb_set_tracepoints.side_effect = RuntimeError(
            "DAP sync failed"
        )
        daemon.dap_client = mock_dap

        tp = TracepointInfo(
            id=1,
            location="my_func",
            function="my_func",
            max_hits=10,
            hit_count=3,
        )
        daemon.tracepoints[1] = tp

        req_update = TracepointRequest(
            tracepoint_id=1,
            function="my_func",
            max_hits=99,
        )
        resp = await daemon.registry.handle("tracepoint", req_update)
        self.assertFalse(resp.success)
        self.assertIn("DAP sync failed", resp.message or "")
        self.assertEqual(daemon.tracepoints[1].max_hits, 10)
        self.assertEqual(daemon.tracepoints[1].hit_count, 3)

        req_new = TracepointRequest(function="new_func", max_hits=5)
        resp_new = await daemon.registry.handle("tracepoint", req_new)
        self.assertFalse(resp_new.success)
        self.assertNotIn(daemon.next_tracepoint_id, daemon.tracepoints)


if __name__ == "__main__":
    unittest.main()
