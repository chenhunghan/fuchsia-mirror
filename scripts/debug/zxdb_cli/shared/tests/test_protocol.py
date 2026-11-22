# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import importlib
import pkgutil
import types
import typing
import unittest
from typing import Any, ClassVar

import shared.protocol
from pydantic import BaseModel, ValidationError
from shared.protocol import (
    BaseRequest,
    ConsoleRequest,
    ContinueRequest,
    EvaluateRequest,
    FinishRequest,
    MemReadRequest,
    NextRequest,
    PauseRequest,
    RequestType,
    Response,
    ResponseType,
    StackTraceRequest,
    StepInRequest,
    TracepointRequest,
    VariablesRequest,
    WatchRequest,
    deserialize_response,
    make_request,
    serialize,
)
from shared.tests.helpers import ProtocolTestCase


class _SampleBody(BaseModel):
    count: int
    label: str


class _TypedStubRequest(BaseRequest[_SampleBody]):
    command: str = "stub-typed"
    response_type: ClassVar[Any] = _SampleBody


class _DictStubRequest(BaseRequest[dict[str, Any]]):
    command: str = "stub-dict"
    response_type: ClassVar[Any] = dict[str, Any]


class _NoneStubRequest(BaseRequest[None]):
    command: str = "stub-none"
    response_type: ClassVar[Any] = None


class TestProtocolRegistryAndFraming(ProtocolTestCase):
    def test_all_requests_and_responses_registered(self) -> None:
        for module_info in pkgutil.iter_modules(shared.protocol.__path__):
            if not module_info.ispkg:
                importlib.import_module(f"shared.protocol.{module_info.name}")

        request_union = typing.get_args(RequestType)[0]
        registered_requests = set(typing.get_args(request_union))
        registered_responses = set(typing.get_args(ResponseType))

        protocol_requests: list[type[BaseRequest[Any]]] = []
        pending = list(BaseRequest.__subclasses__())
        while pending:
            cls = pending.pop()
            pending.extend(cls.__subclasses__())
            if (
                cls.__module__.startswith("shared.protocol.")
                and cls.__module__ != "shared.protocol.base"
            ):
                protocol_requests.append(cls)
        self.assertGreater(len(protocol_requests), 0)

        for req_cls in protocol_requests:
            self.assertIn(
                req_cls,
                registered_requests,
                f"{req_cls.__name__} is missing from RequestType",
            )
            resp_type = req_cls.response_type
            if resp_type is None:
                self.assertIn(type(None), registered_responses)
            elif isinstance(resp_type, types.UnionType):
                for member in typing.get_args(resp_type):
                    self.assertIn(
                        member,
                        registered_responses,
                        f"{member} for {req_cls.__name__} is missing from ResponseType",
                    )
            else:
                self.assertIn(
                    resp_type,
                    registered_responses,
                    f"{resp_type} for {req_cls.__name__} is missing from ResponseType",
                )

    def test_parse_unknown_command(self) -> None:
        with self.assertRaises(ValidationError):
            make_request({"command": "unknown-cmd"})

    def test_deserialize_typed_response(self) -> None:
        req = _TypedStubRequest()
        body = self.assert_deserializes_body(
            req,
            {"count": 3, "label": "ok"},
            _SampleBody,
        )
        self.assertEqual(body.count, 3)
        self.assertEqual(body.label, "ok")

    def test_deserialize_dict_response(self) -> None:
        req = _DictStubRequest()
        body = self.assert_deserializes_body(
            req,
            {"threads": [{"id": 1, "name": "t1"}]},
            dict,
        )
        self.assertEqual(body, {"threads": [{"id": 1, "name": "t1"}]})

    def test_deserialize_none_response(self) -> None:
        req = _NoneStubRequest()
        json_line = self.make_response_json(body=None, message="stopped")
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsNone(resp.body)
        self.assertEqual(resp.message, "stopped")

    def test_deserialize_error_response_typed_request(self) -> None:
        req = _TypedStubRequest()
        json_line = self.make_response_json(
            body=None, success=False, message="Handler failed"
        )
        resp = deserialize_response(json_line, req)
        self.assertFalse(resp.success)
        self.assertIsNone(resp.body)
        self.assertEqual(resp.message, "Handler failed")

    def test_thread_id_input_alias_across_requests(self) -> None:
        cases: list[tuple[dict[str, Any], type[BaseRequest[Any]]]] = [
            (
                {"command": "console", "input": "bt", "threadId": 7},
                ConsoleRequest,
            ),
            ({"command": "continue", "threadId": 7}, ContinueRequest),
            (
                {"command": "evaluate", "expression": "x", "threadId": 7},
                EvaluateRequest,
            ),
            ({"command": "finish", "threadId": 7}, FinishRequest),
            (
                {"command": "mem-read", "address": "0x1000", "threadId": 7},
                MemReadRequest,
            ),
            ({"command": "next", "threadId": 7}, NextRequest),
            ({"command": "pause", "threadId": 7}, PauseRequest),
            ({"command": "stack-trace", "threadId": 7}, StackTraceRequest),
            ({"command": "step-in", "threadId": 7}, StepInRequest),
            (
                {"command": "tracepoint", "watch": "x", "threadId": 7},
                TracepointRequest,
            ),
            ({"command": "variables", "threadId": 7}, VariablesRequest),
            (
                {
                    "command": "watch",
                    "name": "x",
                    "access_type": "write",
                    "threadId": 7,
                },
                WatchRequest,
            ),
        ]
        for payload, expected_cls in cases:
            with self.subTest(command=payload["command"]):
                req = self.assert_request_parses(payload, expected_cls)
                self.assertEqual(getattr(req, "thread_id"), 7)
                wire_json = serialize(req)
                self.assertIn('"thread_id":7', wire_json.replace(" ", ""))
                self.assertNotIn('"threadId"', wire_json)

    def test_response_events_normalize_thread_id_on_init_and_assignment(
        self,
    ) -> None:
        raw_stopped_body = {"threadId": 3, "reason": "breakpoint"}
        raw_stopped = {
            "seq": 1,
            "event": "stopped",
            "body": raw_stopped_body,
        }
        raw_tracepoint = {
            "seq": 2,
            "event": "zxdb.Tracepoint",
            "body": {"tracepointId": 1, "threadKoid": 9999},
        }
        resp = Response[None](
            success=True, events=[raw_stopped, raw_tracepoint]
        )
        assert resp.events is not None
        self.assertEqual(
            resp.events[0]["body"], {"thread_id": 3, "reason": "breakpoint"}
        )
        self.assertEqual(resp.events[1]["body"]["threadKoid"], 9999)
        self.assertNotIn("thread_id", resp.events[1]["body"])
        self.assertEqual(raw_stopped_body["threadId"], 3)

        assigned = Response[None](success=True)
        assigned.events = [raw_stopped]
        assert assigned.events is not None
        self.assertEqual(
            assigned.events[0]["body"], {"thread_id": 3, "reason": "breakpoint"}
        )
        self.assertIn('"thread_id":3', serialize(assigned).replace(" ", ""))
        self.assertNotIn('"threadId"', serialize(assigned))

        appended = Response[None](success=True)
        appended.events = []
        appended.events.append(raw_stopped)
        serialized_appended = serialize(appended)
        self.assertIn('"thread_id":3', serialized_appended.replace(" ", ""))
        self.assertNotIn('"threadId"', serialized_appended)
        self.assertEqual(raw_stopped_body["threadId"], 3)


if __name__ == "__main__":
    unittest.main()
