# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from pydantic import ValidationError
from shared.protocol.async_backtrace import (
    AsyncBacktraceRequest,
    AsyncBacktraceResponse,
)
from shared.protocol.stack_trace import (
    ProcessStackTraceResponse,
    StackTraceRequest,
    ThreadStackTraceResponse,
)
from shared.tests.helpers import ProtocolTestCase
from zxdb_dap import AsyncTaskNode


class TestAsyncBacktraceRequestSchema(ProtocolTestCase):
    def test_valid_request_explicit_pid(self) -> None:
        req = AsyncBacktraceRequest(pid=1234)
        self.assertEqual(req.command, "async-backtrace")
        self.assertEqual(req.pid, 1234)
        self.assertEqual(
            AsyncBacktraceRequest.response_type, AsyncBacktraceResponse
        )

    def test_valid_request_no_pid(self) -> None:
        req = AsyncBacktraceRequest()
        self.assertEqual(req.command, "async-backtrace")
        self.assertIsNone(req.pid)

    def test_parse_async_backtrace(self) -> None:
        req = self.assert_request_parses(
            {"command": "async-backtrace", "pid": 1234},
            AsyncBacktraceRequest,
        )
        self.assertEqual(req.pid, 1234)


class TestAsyncBacktraceResponseSchema(ProtocolTestCase):
    def test_response_serialization_with_tasks(self) -> None:
        child = AsyncTaskNode(
            id="task-2", name="child_task", file="bar.rs", line=20
        )
        parent = AsyncTaskNode(
            id="task-1",
            name="parent_task",
            file="foo.rs",
            line=10,
            children=[child],
        )
        resp = AsyncBacktraceResponse(
            process_id=1234,
            tasks=[parent],
        )
        dumped = resp.model_dump(by_alias=True)
        self.assertEqual(dumped["process_id"], 1234)
        self.assertEqual(len(dumped["tasks"]), 1)
        self.assertEqual(dumped["tasks"][0]["id"], "task-1")
        self.assertEqual(dumped["tasks"][0]["name"], "parent_task")
        self.assertEqual(len(dumped["tasks"][0]["children"]), 1)
        self.assertEqual(dumped["tasks"][0]["children"][0]["id"], "task-2")

    def test_deserialize_async_backtrace_response(self) -> None:
        req = AsyncBacktraceRequest(pid=1234)
        body = self.assert_deserializes_body(
            req,
            {"process_id": 1234, "tasks": []},
            AsyncBacktraceResponse,
        )
        self.assertEqual(body.process_id, 1234)
        self.assertEqual(body.tasks, [])


class TestStackTraceRequestSchema(ProtocolTestCase):
    def test_valid_request_thread_id(self) -> None:
        req = StackTraceRequest(thread_id=1)
        self.assertEqual(req.thread_id, 1)
        self.assertIsNone(req.pid)
        self.assertFalse(req.raw)
        self.assertFalse(req.force_remote_unwind)
        self.assertIsNone(req.unwinder)
        self.assertEqual(
            StackTraceRequest.response_type,
            ThreadStackTraceResponse | ProcessStackTraceResponse,
        )

    def test_valid_request_pid(self) -> None:
        req = StackTraceRequest(pid=1234, raw=True)
        self.assertIsNone(req.thread_id)
        self.assertEqual(req.pid, 1234)
        self.assertTrue(req.raw)
        self.assertFalse(req.force_remote_unwind)
        self.assertIsNone(req.unwinder)

    def test_valid_request_unwinder_and_remote_unwind(self) -> None:
        req = StackTraceRequest(
            thread_id=1, force_remote_unwind=True, unwinder="fp"
        )
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.force_remote_unwind)
        self.assertEqual(req.unwinder, "fp")

    def test_malformed_request_invalid_unwinder(self) -> None:
        with self.assertRaises(ValidationError):
            # Deliberately invalid, to test validation.
            # pyrefly: ignore[bad-argument-type]
            StackTraceRequest(thread_id=1, unwinder="invalid")

    def test_malformed_request_both(self) -> None:
        with self.assertRaises(ValidationError):
            StackTraceRequest(thread_id=1, pid=1234)

    def test_malformed_request_neither(self) -> None:
        with self.assertRaises(ValidationError):
            StackTraceRequest()

    def test_deserialize_union_response_thread_and_process(self) -> None:
        req = StackTraceRequest(thread_id=1)
        self.assert_deserializes_body(
            req,
            {
                "thread_id": 1,
                "stack_frames": [
                    {"frame_index": 0, "name": "foo", "line": 10, "column": 1}
                ],
                "total_frames": 1,
            },
            ThreadStackTraceResponse,
        )
        self.assert_deserializes_body(
            req,
            {
                "process_id": 1234,
                "stacks": [
                    {"thread_id": 1, "stack_frames": [], "total_frames": 0}
                ],
            },
            ProcessStackTraceResponse,
        )


if __name__ == "__main__":
    unittest.main()
