# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import deserialize_response
from shared.protocol.console import ConsoleRequest, ConsoleResponse
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from shared.protocol.variables import VariablesRequest
from shared.tests.helpers import ProtocolTestCase


class TestEvaluateAndConsoleSchema(ProtocolTestCase):
    def test_variables_request_schema(self) -> None:
        req = self.assert_request_parses(
            {"command": "variables", "thread_id": 1, "frame_index": 0},
            VariablesRequest,
        )
        self.assertEqual(req.thread_id, 1)
        self.assertEqual(req.frame_index, 0)
        self.assertEqual(VariablesRequest.response_type, dict[str, Any])

    def test_deserialize_evaluate_forbid_extra(self) -> None:
        req = EvaluateRequest(thread_id=1, expression="x")
        self.assertEqual(EvaluateRequest.response_type, EvaluateResponse)
        json_line = self.make_response_json(
            body={"result": "123", "type": "int", "unknown_field": 42}
        )
        with self.assertRaises(ValidationError):
            deserialize_response(json_line, req)

    def test_parse_console(self) -> None:
        req = self.assert_request_parses(
            {"command": "console", "input": "sym-stat"},
            ConsoleRequest,
        )
        self.assertEqual(req.input, "sym-stat")
        self.assertIsNone(req.thread_id)
        self.assertEqual(req.frame_index, 0)
        self.assertEqual(ConsoleRequest.response_type, ConsoleResponse)

    def test_parse_console_with_thread_and_frame(self) -> None:
        req = self.assert_request_parses(
            {
                "command": "console",
                "input": "regs",
                "thread_id": 1001,
                "frame_index": 2,
            },
            ConsoleRequest,
        )
        self.assertEqual(req.input, "regs")
        self.assertEqual(req.thread_id, 1001)
        self.assertEqual(req.frame_index, 2)

    def test_deserialize_console_response(self) -> None:
        req = ConsoleRequest(input="sym-stat")
        body = self.assert_deserializes_body(
            req,
            {"output": "Process 1 symbol status\n"},
            ConsoleResponse,
        )
        self.assertEqual(body.output, "Process 1 symbol status\n")

    def test_deserialize_console_forbid_extra(self) -> None:
        req = ConsoleRequest(input="sym-stat")
        json_line = self.make_response_json(body={"output": "ok", "extra": 1})
        with self.assertRaises(ValidationError):
            deserialize_response(json_line, req)


if __name__ == "__main__":
    unittest.main()
