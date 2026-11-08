# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import io
import json
import unittest
from typing import Any

from pydantic import ValidationError
from pydap.client import DapError, StreamWriterProtocol
from zxdb_dap import (
    AsyncBacktraceUpdate,
    ThreadEvent,
    ZxdbAsyncBacktraceArguments,
    ZxdbConsoleCommandArguments,
    ZxdbDapClient,
    ZxdbDetachArguments,
    ZxdbPauseArguments,
    ZxdbProcessArguments,
    ZxdbProcessStoppedEvent,
    ZxdbSetTracepointsArguments,
    ZxdbStackTraceArguments,
    ZxdbThread,
    ZxdbThreadEvent,
    ZxdbThreadsResponse,
    ZxdbTracepointEvent,
    ZxdbTracepointSpec,
    ZxdbTracepointWatchSpec,
)


class MockWriter(StreamWriterProtocol):
    def __init__(self) -> None:
        self.buffer = io.BytesIO()
        self.drained = asyncio.Event()

    def write(self, data: bytes) -> None:
        self.buffer.write(data)

    async def drain(self) -> None:
        self.drained.set()


def feed_dap_response(
    reader: asyncio.StreamReader, response: dict[str, Any]
) -> None:
    body = json.dumps(response, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
    reader.feed_data(header + body)


class TestZxdbDapMixin(unittest.IsolatedAsyncioTestCase):
    def _start_client(
        self, client: ZxdbDapClient
    ) -> tuple[asyncio.StreamReader, MockWriter]:
        reader = asyncio.StreamReader()
        writer = MockWriter()
        event_queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        client.run(reader, writer, event_queue)
        return reader, writer

    async def test_zxdb_detach_pid(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbDetachArguments(pid=1234)

        send_task = asyncio.create_task(client.zxdb_detach(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Detach",
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(req_val["command"], "zxdb.Detach")
        self.assertEqual(req_val["arguments"]["pid"], 1234)
        self.assertIsNone(req_val["arguments"].get("all"))

    async def test_zxdb_detach_all(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbDetachArguments(detach_all=True)

        send_task = asyncio.create_task(client.zxdb_detach(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Detach",
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp["success"])
        self.assertEqual(req_val["command"], "zxdb.Detach")
        self.assertIsNone(req_val["arguments"].get("pid"))
        self.assertTrue(req_val["arguments"]["all"])

    def test_zxdb_detach_invalid_args(self) -> None:
        with self.assertRaises(ValueError):
            ZxdbDetachArguments(pid=1234, detach_all=True)
        with self.assertRaises(ValueError):
            ZxdbDetachArguments(pid=None, detach_all=None)

    async def test_zxdb_process_all(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)

        send_task = asyncio.create_task(client.zxdb_process())

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Process",
            "body": {
                "processes": [
                    {
                        "id": 1001,
                        "name": "proc1",
                        "threads": [{"id": 2001, "name": "t1"}],
                    },
                    {
                        "id": 1002,
                        "name": "proc2",
                        "threads": [{"id": 2002, "name": "t2"}],
                    },
                ],
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "zxdb.Process")
        self.assertEqual(len(resp.body.processes), 2)
        self.assertEqual(resp.body.processes[0].id, 1001)
        self.assertEqual(resp.body.processes[0].name, "proc1")
        self.assertEqual(len(resp.body.processes[0].threads), 1)
        self.assertEqual(resp.body.processes[0].threads[0].id, 2001)

    async def test_zxdb_process_pid(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbProcessArguments(pid=1001)

        send_task = asyncio.create_task(client.zxdb_process(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.Process",
            "body": {
                "processes": [
                    {
                        "id": 1001,
                        "name": "proc1",
                        "threads": [{"id": 2001, "name": "t1"}],
                    }
                ],
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "zxdb.Process")
        self.assertEqual(req_val["arguments"]["pid"], 1001)
        self.assertEqual(len(resp.body.processes), 1)
        self.assertEqual(resp.body.processes[0].id, 1001)

    async def test_zxdb_process_error(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbProcessArguments(pid=9999)

        send_task = asyncio.create_task(client.zxdb_process(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": False,
            "command": "zxdb.Process",
            "message": "Process not found",
        }

        feed_dap_response(reader, response)

        with self.assertRaises(DapError) as ctx:
            await send_task
        self.assertIn("Process not found", str(ctx.exception))

    async def test_zxdb_stack_trace(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbStackTraceArguments(
            thread_id=5678, remote_unwind=True, unwinder="fp"
        )

        send_task = asyncio.create_task(client.zxdb_stack_trace(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "stackTrace",
            "body": {
                "stackFrames": [
                    {
                        "id": 1,
                        "name": "main",
                        "line": 10,
                        "column": 1,
                        "trust": "FP",
                    }
                ],
                "totalFrames": 1,
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "stackTrace")
        self.assertEqual(req_val["arguments"]["threadId"], 5678)
        self.assertTrue(req_val["arguments"]["remoteUnwind"])
        self.assertEqual(req_val["arguments"]["unwinder"], "fp")
        self.assertEqual(len(resp.body.stack_frames), 1)
        self.assertEqual(resp.body.stack_frames[0].trust, "FP")

    def test_zxdb_stack_trace_invalid_unwinder(self) -> None:
        with self.assertRaises(ValidationError):
            ZxdbStackTraceArguments(thread_id=5678, unwinder="invalid")

    async def test_threads_response_process_id(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        send_task = asyncio.create_task(client.zxdb_threads())

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "threads",
            "body": {
                "threads": [
                    {"id": 1234, "name": "main", "processId": 5678},
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertTrue(isinstance(resp, ZxdbThreadsResponse))
        self.assertEqual(len(resp.body.threads), 1)
        self.assertEqual(resp.body.threads[0].id, 1234)
        self.assertEqual(resp.body.threads[0].name, "main")
        self.assertEqual(resp.body.threads[0].process_id, 5678)

    async def test_threads_response_process_id_optional(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        send_task = asyncio.create_task(client.zxdb_threads())

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "threads",
            "body": {
                "threads": [
                    {"id": 1234, "name": "main"},
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertTrue(isinstance(resp, ZxdbThreadsResponse))
        self.assertEqual(len(resp.body.threads), 1)
        self.assertEqual(resp.body.threads[0].id, 1234)
        self.assertEqual(resp.body.threads[0].name, "main")
        self.assertIsNone(resp.body.threads[0].process_id)

    def test_thread_event_process_id(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "thread",
            "body": {
                "reason": "started",
                "threadId": 1234,
                "processId": 5678,
            },
        }
        event = ZxdbThreadEvent.model_validate(event_dict)
        self.assertEqual(event.event, "thread")
        self.assertEqual(event.body.reason, "started")
        self.assertEqual(event.body.thread_id, 1234)
        self.assertEqual(event.body.process_id, 5678)

    def test_standard_thread_event_without_process_id(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "thread",
            "body": {
                "reason": "started",
                "threadId": 1234,
            },
        }
        event = ThreadEvent.model_validate(event_dict)
        self.assertEqual(event.event, "thread")
        self.assertEqual(event.body.reason, "started")
        self.assertEqual(event.body.thread_id, 1234)

    def test_thread_event_is_stopped(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "thread",
            "body": {
                "reason": "started",
                "threadId": 1234,
                "processId": 5678,
                "isStopped": True,
            },
        }
        event = ZxdbThreadEvent.model_validate(event_dict)
        self.assertEqual(event.event, "thread")
        self.assertEqual(event.body.reason, "started")
        self.assertEqual(event.body.thread_id, 1234)
        self.assertEqual(event.body.process_id, 5678)
        self.assertEqual(event.body.is_stopped, True)
        dap_dict = event.body.dump_dap()
        self.assertEqual(dap_dict["isStopped"], True)

    def test_zxdb_thread_dump_dap_process_id(self) -> None:
        thread = ZxdbThread(id=1, name="test", process_id=1234)
        dap_dict = thread.dump_dap()
        self.assertEqual(dap_dict["id"], 1)
        self.assertEqual(dap_dict["name"], "test")
        self.assertEqual(dap_dict["processId"], 1234)

    def test_thread_event_validation_invalid_event(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "stopped",
            "body": {
                "reason": "started",
                "threadId": 1234,
                "processId": 5678,
            },
        }
        with self.assertRaises(ValidationError):
            ThreadEvent.model_validate(event_dict)

    def test_thread_event_validation_custom_reason(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "thread",
            "body": {
                "reason": "unknown_reason",
                "threadId": 1234,
            },
        }
        event = ThreadEvent.model_validate(event_dict)
        self.assertEqual(event.body.reason, "unknown_reason")
        self.assertEqual(event.body.thread_id, 1234)

    def test_zxdb_thread_event_validation_custom_reason(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "thread",
            "body": {
                "reason": "custom_reason",
                "threadId": 1234,
                "processId": 5678,
            },
        }
        event = ZxdbThreadEvent.model_validate(event_dict)
        self.assertEqual(event.body.reason, "custom_reason")
        self.assertEqual(event.body.thread_id, 1234)
        self.assertEqual(event.body.process_id, 5678)

    async def test_zxdb_pause_process(self) -> None:
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbPauseArguments(process_id=12345)

        send_task = asyncio.create_task(client.zxdb_pause_process(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 10,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "pause",
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(req_val["command"], "pause")
        self.assertEqual(req_val["arguments"]["processId"], 12345)

    def test_zxdb_pause_invalid_args(self) -> None:
        with self.assertRaises(ValueError):
            ZxdbPauseArguments(process_id=None)

    def test_zxdb_process_stopped_event(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "processStopped",
            "body": {
                "processId": 999,
                "name": "my_process",
                "threads": [1234, 5678],
            },
        }
        event = ZxdbProcessStoppedEvent.model_validate(event_dict)
        self.assertEqual(event.event, "processStopped")
        self.assertEqual(event.body.process_id, 999)
        self.assertEqual(event.body.name, "my_process")
        self.assertEqual(event.body.threads, [1234, 5678])

    def test_async_backtrace_update_event(self) -> None:
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "zxdb.updateAsyncBacktrace",
            "body": {
                "id": 1234,
                "name": "main",
                "processId": 5678,
                "tasks": [
                    {
                        "id": "0x1",
                        "name": "Task 1",
                        "file": "foo.cc",
                        "line": 42,
                        "children": [
                            {
                                "id": "0x2",
                                "name": "Task 2",
                                "file": "bar.cc",
                                "line": 24,
                                "children": [],
                            }
                        ],
                    }
                ],
            },
        }
        event = AsyncBacktraceUpdate.model_validate(event_dict)
        self.assertEqual(event.event, "zxdb.updateAsyncBacktrace")
        self.assertEqual(event.body.thread_id, 1234)
        self.assertEqual(event.body.name, "main")
        self.assertEqual(event.body.process_id, 5678)
        self.assertEqual(len(event.body.tasks), 1)

        task1 = event.body.tasks[0]
        self.assertEqual(task1.id, "0x1")
        self.assertEqual(task1.name, "Task 1")
        self.assertEqual(task1.file, "foo.cc")
        self.assertEqual(task1.line, 42)
        self.assertEqual(len(task1.children), 1)

        task2 = task1.children[0]
        self.assertEqual(task2.id, "0x2")
        self.assertEqual(task2.name, "Task 2")
        self.assertEqual(task2.file, "bar.cc")
        self.assertEqual(task2.line, 24)
        self.assertEqual(len(task2.children), 0)

    def test_async_backtrace_update_event_optional_defaults(self) -> None:
        """Tests deserializing a payload with omitted optional fields."""
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "zxdb.updateAsyncBacktrace",
            "body": {
                "id": 1234,
                "name": "main",
                "processId": 5678,
                "tasks": [
                    {
                        "name": "Task Minimal",
                    }
                ],
            },
        }
        event = AsyncBacktraceUpdate.model_validate(event_dict)
        self.assertEqual(event.body.thread_id, 1234)
        self.assertEqual(event.body.process_id, 5678)
        self.assertEqual(len(event.body.tasks), 1)

        task = event.body.tasks[0]
        self.assertIsNone(task.id)
        self.assertEqual(task.name, "Task Minimal")
        self.assertIsNone(task.file)
        self.assertIsNone(task.line)
        self.assertEqual(task.children, [])

    def test_async_backtrace_update_event_invalid_event(self) -> None:
        """Tests that mismatched event discriminators fail validation."""
        event_dict = {
            "seq": 1,
            "type": "event",
            "event": "invalid.event",
            "body": {
                "id": 1234,
                "name": "main",
                "tasks": [],
            },
        }
        with self.assertRaises(ValidationError):
            AsyncBacktraceUpdate.model_validate(event_dict)

    async def test_zxdb_async_backtrace(self) -> None:
        """Tests sending zxdb.AsyncBacktrace request and parsing response."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbAsyncBacktraceArguments(thread_id=5678)

        send_task = asyncio.create_task(client.zxdb_async_backtrace(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        self.assertEqual(req_val["command"], "zxdb.AsyncBacktrace")
        self.assertEqual(req_val["arguments"]["threadId"], 5678)
        self.assertNotIn("processId", req_val["arguments"])

        response = {
            "seq": 11,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.AsyncBacktrace",
            "body": {
                "tasks": [
                    {
                        "id": "task_1",
                        "name": "my_async_fn",
                        "file": "foo.rs",
                        "line": 100,
                        "children": [
                            {
                                "id": "task_2",
                                "name": "child_fn",
                                "file": "bar.rs",
                                "line": 200,
                                "children": [],
                            }
                        ],
                    }
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(len(resp.body.tasks), 1)
        self.assertEqual(resp.body.tasks[0].id, "task_1")
        self.assertEqual(resp.body.tasks[0].name, "my_async_fn")
        self.assertEqual(resp.body.tasks[0].file, "foo.rs")
        self.assertEqual(resp.body.tasks[0].line, 100)
        self.assertEqual(len(resp.body.tasks[0].children), 1)
        self.assertEqual(resp.body.tasks[0].children[0].id, "task_2")
        self.assertEqual(resp.body.tasks[0].children[0].name, "child_fn")

    async def test_zxdb_async_backtrace_failure(self) -> None:
        """Tests sending zxdb.AsyncBacktrace request when server returns error."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbAsyncBacktraceArguments(thread_id=9999)

        send_task = asyncio.create_task(client.zxdb_async_backtrace(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 12,
            "type": "response",
            "request_seq": seq,
            "success": False,
            "command": "zxdb.AsyncBacktrace",
            "message": "All threads must be stopped before requesting async-backtrace.",
        }

        feed_dap_response(reader, response)

        with self.assertRaises(DapError) as ctx:
            await send_task
        self.assertIn("All threads must be stopped", str(ctx.exception))

    async def test_zxdb_console_command_without_frame_id(self) -> None:
        """Tests sending zxdb.ConsoleCommand without frameId."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbConsoleCommandArguments(command="sym-stat")

        send_task = asyncio.create_task(client.zxdb_console_command(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        self.assertEqual(req_val["command"], "zxdb.ConsoleCommand")
        self.assertEqual(req_val["arguments"]["command"], "sym-stat")
        self.assertNotIn("frameId", req_val["arguments"])

        response = {
            "seq": 13,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.ConsoleCommand",
            "body": {"output": "Process 1 symbol status\n"},
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(resp.body.output, "Process 1 symbol status\n")

    async def test_zxdb_console_command_with_frame_id(self) -> None:
        """Tests sending zxdb.ConsoleCommand with frameId."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbConsoleCommandArguments(command="regs", frame_id=42)

        send_task = asyncio.create_task(client.zxdb_console_command(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        self.assertEqual(req_val["command"], "zxdb.ConsoleCommand")
        self.assertEqual(req_val["arguments"]["command"], "regs")
        self.assertEqual(req_val["arguments"]["frameId"], 42)

        response = {
            "seq": 14,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.ConsoleCommand",
            "body": {"output": "rax = 0x0\n"},
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(resp.body.output, "rax = 0x0\n")

    async def test_zxdb_console_command_failure(self) -> None:
        """Tests sending zxdb.ConsoleCommand when server returns error."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbConsoleCommandArguments(command="invalid_cmd")

        send_task = asyncio.create_task(client.zxdb_console_command(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        response = {
            "seq": 15,
            "type": "response",
            "request_seq": seq,
            "success": False,
            "command": "zxdb.ConsoleCommand",
            "message": 'The string "invalid_cmd" is not a valid verb.',
        }

        feed_dap_response(reader, response)

        with self.assertRaises(DapError) as ctx:
            await send_task
        self.assertIn("not a valid verb", str(ctx.exception))

    async def test_zxdb_set_tracepoints_success(self) -> None:
        """Tests sending zxdb.SetTracepoints request successfully."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        specs = [
            ZxdbTracepointSpec(
                function="my_func",
                max_matching_frames=5,
                max_hits=5,
                expressions=["x", "y"],
                locals=True,
                registers=True,
            )
        ]

        send_task = asyncio.create_task(client.zxdb_set_tracepoints(specs))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        self.assertEqual(req_val["command"], "zxdb.SetTracepoints")
        self.assertEqual(len(req_val["arguments"]["tracepoints"]), 1)
        self.assertEqual(
            req_val["arguments"]["tracepoints"][0]["function"], "my_func"
        )
        self.assertEqual(
            req_val["arguments"]["tracepoints"][0]["maxMatchingFrames"], 5
        )
        self.assertEqual(req_val["arguments"]["tracepoints"][0]["maxHits"], 5)
        seq = req_val["seq"]

        response = {
            "seq": 13,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.SetTracepoints",
            "body": {
                "tracepoints": [
                    {
                        "id": 1,
                        "verified": True,
                        "maxMatchingFrames": 5,
                        "hitCount": 0,
                        "maxHits": 5,
                        "enabled": True,
                        "autoDisabled": False,
                    }
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(len(resp.body.tracepoints), 1)
        self.assertEqual(resp.body.tracepoints[0].id, 1)
        self.assertTrue(resp.body.tracepoints[0].verified)
        self.assertEqual(resp.body.tracepoints[0].max_matching_frames, 5)
        self.assertEqual(resp.body.tracepoints[0].max_hits, 5)

    async def test_zxdb_set_tracepoints_arguments_model(self) -> None:
        """Tests sending zxdb.SetTracepoints using arguments model."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        args = ZxdbSetTracepointsArguments(
            tracepoints=[
                ZxdbTracepointSpec(
                    file="main.cc",
                    line=42,
                ),
                ZxdbTracepointSpec(
                    watch=ZxdbTracepointWatchSpec(
                        address=0x1000,
                        expression="*ptr",
                        byte_size=4,
                        access_type="write",
                    ),
                ),
            ]
        )

        send_task = asyncio.create_task(client.zxdb_set_tracepoints(args))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        self.assertEqual(req_val["command"], "zxdb.SetTracepoints")
        self.assertEqual(len(req_val["arguments"]["tracepoints"]), 2)
        tp0 = req_val["arguments"]["tracepoints"][0]
        self.assertEqual(tp0["file"], "main.cc")
        self.assertEqual(tp0["line"], 42)
        self.assertNotIn("expressions", tp0)
        tp1 = req_val["arguments"]["tracepoints"][1]
        self.assertEqual(tp1["watch"]["address"], 0x1000)
        self.assertEqual(tp1["watch"]["byteSize"], 4)
        self.assertEqual(tp1["watch"]["accessType"], "write")
        seq = req_val["seq"]

        response = {
            "seq": 14,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.SetTracepoints",
            "body": {
                "tracepoints": [
                    {
                        "id": 2,
                        "verified": True,
                        "hitCount": 0,
                        "maxHits": 20,
                        "enabled": True,
                        "autoDisabled": False,
                    },
                    {
                        "id": 3,
                        "verified": False,
                        "hitCount": 0,
                        "maxHits": 20,
                        "enabled": True,
                        "autoDisabled": False,
                        "message": "Invalid watchpoint size.",
                    },
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(len(resp.body.tracepoints), 2)
        self.assertEqual(resp.body.tracepoints[0].id, 2)
        self.assertTrue(resp.body.tracepoints[0].verified)
        self.assertIsNone(resp.body.tracepoints[0].message)
        self.assertEqual(resp.body.tracepoints[1].id, 3)
        self.assertFalse(resp.body.tracepoints[1].verified)
        self.assertEqual(
            resp.body.tracepoints[1].message, "Invalid watchpoint size."
        )

    async def test_zxdb_set_tracepoints_with_new_fields(self) -> None:
        """Tests sending zxdb.SetTracepoints with id, rawBacktrace, and dataId."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        specs = (
            ZxdbTracepointSpec(
                id=42,
                function="compute",
                raw_backtrace=True,
            ),
            ZxdbTracepointSpec(
                id=43,
                watch=ZxdbTracepointWatchSpec(
                    data_id="1234:0x2000:8",
                    access_type="readWrite",
                ),
            ),
        )

        send_task = asyncio.create_task(client.zxdb_set_tracepoints(specs))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        self.assertEqual(req_val["command"], "zxdb.SetTracepoints")
        self.assertEqual(len(req_val["arguments"]["tracepoints"]), 2)
        tp0 = req_val["arguments"]["tracepoints"][0]
        self.assertEqual(tp0["id"], 42)
        self.assertEqual(tp0["function"], "compute")
        self.assertTrue(tp0["rawBacktrace"])
        tp1 = req_val["arguments"]["tracepoints"][1]
        self.assertEqual(tp1["id"], 43)
        self.assertEqual(tp1["watch"]["dataId"], "1234:0x2000:8")
        self.assertEqual(tp1["watch"]["accessType"], "readWrite")
        seq = req_val["seq"]

        response = {
            "seq": 15,
            "type": "response",
            "request_seq": seq,
            "success": True,
            "command": "zxdb.SetTracepoints",
            "body": {
                "tracepoints": [
                    {
                        "id": 42,
                        "verified": True,
                        "hitCount": 1,
                        "maxHits": 20,
                        "enabled": True,
                        "autoDisabled": False,
                    },
                    {
                        "id": 43,
                        "verified": True,
                        "hitCount": 0,
                        "maxHits": 20,
                        "enabled": True,
                        "autoDisabled": False,
                    },
                ]
            },
        }

        feed_dap_response(reader, response)

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(len(resp.body.tracepoints), 2)
        self.assertEqual(resp.body.tracepoints[0].id, 42)
        self.assertEqual(resp.body.tracepoints[0].hit_count, 1)
        self.assertEqual(resp.body.tracepoints[1].id, 43)

    async def test_zxdb_set_tracepoints_empty_sequence(self) -> None:
        """Tests clearing tracepoints with an empty sequence."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)

        send_task = asyncio.create_task(client.zxdb_set_tracepoints([]))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        self.assertEqual(req_val["command"], "zxdb.SetTracepoints")
        self.assertEqual(req_val["arguments"]["tracepoints"], [])
        seq = req_val["seq"]

        feed_dap_response(
            reader,
            {
                "seq": 16,
                "type": "response",
                "request_seq": seq,
                "success": True,
                "command": "zxdb.SetTracepoints",
                "body": {"tracepoints": []},
            },
        )

        resp = await send_task
        self.assertTrue(resp.success)
        self.assertEqual(resp.body.tracepoints, [])

    def test_zxdb_tracepoint_watch_spec_rejects_invalid_access_type(
        self,
    ) -> None:
        """Tests that ZxdbTracepointWatchSpec rejects unsupported access_type values."""
        with self.assertRaises(ValidationError):
            ZxdbTracepointWatchSpec.model_validate({"accessType": "read"})

    def test_zxdb_tracepoint_event_model(self) -> None:
        """Tests parsing a zxdb.Tracepoint event payload."""
        event_dict = {
            "seq": 99,
            "type": "event",
            "event": "zxdb.Tracepoint",
            "body": {
                "tracepointId": 1,
                "location": "foo.cc:10",
                "hitCount": 1,
                "maxHits": 5,
                "autoDisabled": False,
                "truncated": True,
                "threadKoid": 1234,
                "dwellMs": 2.5,
                "capture": {
                    "matched": True,
                    "frame": {
                        "name": "foo",
                        "file": "foo.cc",
                        "line": 10,
                        "ip": 4096,
                        "sp": 8192,
                        "trust": "context",
                    },
                    "backtrace": [
                        {
                            "frameIndex": 0,
                            "name": "foo",
                            "file": "foo.cc",
                            "line": 10,
                            "ip": 4096,
                            "sp": 8192,
                            "trust": "context",
                            "elided": False,
                        },
                        {
                            "frameIndex": 1,
                            "name": "bar",
                            "file": "bar.cc",
                            "line": 20,
                            "ip": 8192,
                            "sp": 16384,
                            "trust": "cfi",
                            "elided": True,
                            "elideDescription": "elided async runtime frames",
                        },
                    ],
                    "locals": {
                        "var": {
                            "value": "42",
                            "type": "int",
                        }
                    },
                    "registers": {
                        "rax": "0x1",
                    },
                    "evaluations": {
                        "x + 1": {
                            "result": "43",
                            "type": "int",
                            "error": None,
                        }
                    },
                    "errors": [],
                },
            },
        }

        event = ZxdbTracepointEvent.model_validate(event_dict)
        self.assertEqual(event.event, "zxdb.Tracepoint")
        self.assertEqual(event.body.tracepoint_id, 1)
        self.assertEqual(event.body.location, "foo.cc:10")
        self.assertEqual(event.body.dwell_ms, 2.5)
        self.assertTrue(event.body.truncated)
        self.assertIsNotNone(event.body.capture.frame)
        assert event.body.capture.frame is not None
        self.assertEqual(event.body.capture.frame.name, "foo")
        self.assertEqual(event.body.capture.frame.trust, "context")
        self.assertIsNotNone(event.body.capture.backtrace)
        assert event.body.capture.backtrace is not None
        self.assertEqual(len(event.body.capture.backtrace), 2)
        self.assertEqual(event.body.capture.backtrace[1].trust, "cfi")
        self.assertTrue(event.body.capture.backtrace[1].elided)
        self.assertEqual(
            event.body.capture.backtrace[1].elide_description,
            "elided async runtime frames",
        )
        assert event.body.capture.locals is not None
        self.assertEqual(event.body.capture.locals["var"].value, "42")
        assert event.body.capture.registers is not None
        self.assertEqual(event.body.capture.registers["rax"], "0x1")
        assert event.body.capture.evaluations is not None
        self.assertEqual(event.body.capture.evaluations["x + 1"].result, "43")

    def test_minimal_zxdb_tracepoint_event_model(self) -> None:
        """Tests parsing a minimal zxdb.Tracepoint event with optional fields omitted."""
        event_dict = {
            "seq": 100,
            "type": "event",
            "event": "zxdb.Tracepoint",
            "body": {
                "tracepointId": 2,
                "location": "bar.cc:20",
                "hitCount": 1,
                "maxHits": 1,
                "autoDisabled": True,
                "threadKoid": 5678,
                "dwellMs": 0.5,
                "capture": {
                    "matched": False,
                    "errors": ["Condition evaluation failed."],
                },
            },
        }
        event = ZxdbTracepointEvent.model_validate(event_dict)
        self.assertEqual(event.body.tracepoint_id, 2)
        self.assertTrue(event.body.auto_disabled)
        self.assertFalse(event.body.truncated)
        self.assertFalse(event.body.capture.matched)
        self.assertIsNone(event.body.capture.frame)
        self.assertIsNone(event.body.capture.backtrace)
        self.assertIsNone(event.body.capture.locals)
        self.assertIsNone(event.body.capture.registers)
        self.assertIsNone(event.body.capture.evaluations)
        self.assertEqual(
            event.body.capture.errors, ["Condition evaluation failed."]
        )

    def test_invalid_event_discriminator(self) -> None:
        """Tests that ZxdbTracepointEvent rejects non-matching event names."""
        event_dict = {
            "seq": 101,
            "type": "event",
            "event": "wrong.event",
            "body": {
                "tracepointId": 3,
                "location": "main.cc:1",
                "hitCount": 1,
                "maxHits": 10,
                "autoDisabled": False,
                "threadKoid": 1001,
                "dwellMs": 1.0,
                "capture": {"matched": True},
            },
        }
        with self.assertRaises(ValidationError):
            ZxdbTracepointEvent.model_validate(event_dict)

    async def test_zxdb_set_tracepoints_error_response(self) -> None:
        """Tests handling an error response from zxdb.SetTracepoints."""
        client = ZxdbDapClient()
        reader, writer = self._start_client(client)
        specs = [ZxdbTracepointSpec(id=-1)]

        send_task = asyncio.create_task(client.zxdb_set_tracepoints(specs))

        await asyncio.wait_for(writer.drained.wait(), timeout=2.0)

        buffer_val = writer.buffer.getvalue()
        _headers, body = buffer_val.split(b"\r\n\r\n", 1)
        req_val = json.loads(body.decode("utf-8"))
        seq = req_val["seq"]

        error_response = {
            "seq": 20,
            "type": "response",
            "request_seq": seq,
            "success": False,
            "command": "zxdb.SetTracepoints",
            "message": "Tracepoint id must be positive.",
        }

        feed_dap_response(reader, error_response)

        with self.assertRaises(DapError) as cm:
            await send_task
        self.assertIn("Tracepoint id must be positive.", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
