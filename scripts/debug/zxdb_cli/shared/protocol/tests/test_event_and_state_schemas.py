# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from pydantic import ValidationError
from shared.protocol.get_state import GetStateRequest, GetStateResponse
from shared.protocol.threads import ThreadsRequest, ThreadsResponse
from shared.protocol.wait_for_event import WaitForEventRequest
from shared.tests.helpers import ProtocolTestCase


class TestWaitForEventRequestSchema(ProtocolTestCase):
    def test_valid(self) -> None:
        req = WaitForEventRequest(last_seen_seq=10, timeout=5)
        self.assertEqual(req.last_seen_seq, 10)
        self.assertEqual(req.timeout, 5)
        self.assertIsNone(WaitForEventRequest.response_type)

    def test_optional_timeout(self) -> None:
        req = WaitForEventRequest(last_seen_seq=10)
        self.assertEqual(req.last_seen_seq, 10)
        self.assertIsNone(req.timeout)

    def test_missing_last_seen_seq(self) -> None:
        with self.assertRaises(ValidationError):
            # Deliberately invalid, to test validation.
            # pyrefly: ignore[missing-argument]
            WaitForEventRequest(timeout=5)


class TestGetStateAndThreadsSchemas(ProtocolTestCase):
    def test_threads_request_and_response_schema(self) -> None:
        req = self.assert_request_parses({"command": "threads"}, ThreadsRequest)
        self.assertEqual(req.command, "threads")
        self.assertEqual(ThreadsRequest.response_type, ThreadsResponse)
        body = self.assert_deserializes_body(
            req,
            {
                "threads": [
                    {"thread_id": 1, "name": "main", "process_id": 1234},
                    {"thread_id": 2, "name": "worker"},
                ]
            },
            ThreadsResponse,
        )
        self.assertEqual(len(body.threads), 2)
        self.assertEqual(body.threads[0].thread_id, 1)
        self.assertEqual(body.threads[0].name, "main")
        self.assertEqual(body.threads[0].process_id, 1234)
        self.assertEqual(body.threads[1].thread_id, 2)
        self.assertIsNone(body.threads[1].process_id)

        with self.assertRaises(ValidationError):
            ThreadsResponse.model_validate(
                {"threads": [{"id": 1, "name": "main"}]}
            )

    def test_deserialize_get_state_with_watchpoints(self) -> None:
        req = GetStateRequest()
        self.assertEqual(GetStateRequest.response_type, GetStateResponse)
        body = self.assert_deserializes_body(
            req,
            {
                "threads": [{"thread_id": 1, "name": "t1"}],
                "processes": None,
                "breakpoints": None,
                "watchpoints": {
                    "1001:0x1234:4": {
                        "data_id": "1001:0x1234:4",
                        "expression": "my_var",
                        "thread_id": 1,
                        "frame_index": 0,
                        "access_type": "write",
                        "verified": True,
                        "message": None,
                    }
                },
            },
            GetStateResponse,
        )
        self.assertEqual(len(body.threads), 1)
        self.assertEqual(body.threads[0].thread_id, 1)
        self.assertEqual(body.threads[0].name, "t1")
        self.assertIsNotNone(body.watchpoints)
        assert body.watchpoints is not None
        wp = body.watchpoints["1001:0x1234:4"]
        self.assertEqual(wp.data_id, "1001:0x1234:4")
        self.assertEqual(wp.expression, "my_var")
        self.assertEqual(wp.thread_id, 1)
        self.assertEqual(wp.frame_index, 0)
        self.assertEqual(wp.access_type, "write")
        self.assertTrue(wp.verified)
        self.assertIsNone(wp.message)

        with self.assertRaises(ValidationError):
            GetStateResponse.model_validate(
                {"threads": [{"id": 1, "name": "t1"}]}
            )


if __name__ == "__main__":
    unittest.main()
