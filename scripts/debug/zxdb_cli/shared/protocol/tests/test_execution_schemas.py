# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import make_request
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.step_in import StepInRequest
from shared.tests.helpers import ProtocolTestCase


class TestExecutionSchemas(ProtocolTestCase):
    def test_parse_pause_and_continue(self) -> None:
        pause_req = self.assert_request_parses(
            {"command": "pause", "thread_id": 1},
            PauseRequest,
        )
        self.assertEqual(pause_req.thread_id, 1)
        self.assertEqual(PauseRequest.response_type, dict[str, Any])

        cont_req = self.assert_request_parses(
            {"command": "continue", "thread_id": 1, "single_thread": True},
            ContinueRequest,
        )
        self.assertEqual(cont_req.thread_id, 1)
        self.assertTrue(cont_req.single_thread)
        self.assertEqual(ContinueRequest.response_type, dict[str, Any])

    def test_parse_finish(self) -> None:
        req = self.assert_request_parses(
            {"command": "finish", "thread_id": 1, "single_thread": True},
            FinishRequest,
        )
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)
        self.assertEqual(FinishRequest.response_type, dict[str, Any])

    def test_parse_next(self) -> None:
        req = self.assert_request_parses(
            {
                "command": "next",
                "thread_id": 1,
                "single_thread": True,
                "granularity": "line",
            },
            NextRequest,
        )
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)
        self.assertEqual(req.granularity, "line")
        self.assertEqual(NextRequest.response_type, dict[str, Any])

    def test_parse_next_invalid_granularity(self) -> None:
        with self.assertRaises(ValidationError):
            make_request(
                {
                    "command": "next",
                    "thread_id": 1,
                    "granularity": "invalid_granularity",
                }
            )

    def test_parse_step_in(self) -> None:
        req = self.assert_request_parses(
            {
                "command": "step-in",
                "thread_id": 1,
                "single_thread": True,
                "target_id": 0,
                "granularity": "line",
            },
            StepInRequest,
        )
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)
        self.assertEqual(req.target_id, 0)
        self.assertEqual(req.granularity, "line")
        self.assertEqual(StepInRequest.response_type, dict[str, Any])

    def test_parse_step_in_invalid_granularity(self) -> None:
        with self.assertRaises(ValidationError):
            make_request(
                {
                    "command": "step-in",
                    "thread_id": 1,
                    "granularity": "invalid_granularity",
                }
            )


if __name__ == "__main__":
    unittest.main()
