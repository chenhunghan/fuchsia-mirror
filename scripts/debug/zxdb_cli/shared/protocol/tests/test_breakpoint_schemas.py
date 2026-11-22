# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import make_request
from shared.protocol.break_request import BreakRequest
from shared.protocol.watch_request import WatchRequest
from shared.tests.helpers import ProtocolTestCase


class TestBreakRequestSchema(ProtocolTestCase):
    def test_valid_source_breakpoint(self) -> None:
        req = BreakRequest(file="src/foo.cc", line=42, condition="x > 0")
        self.assertEqual(req.file, "src/foo.cc")
        self.assertEqual(req.line, 42)
        self.assertEqual(req.condition, "x > 0")
        self.assertEqual(BreakRequest.response_type, dict[str, Any])

    def test_valid_function_breakpoint(self) -> None:
        req = BreakRequest(
            function="usb_xhci::UsbXhci::Init", condition="i == 1"
        )
        self.assertEqual(req.function, "usb_xhci::UsbXhci::Init")
        self.assertIsNone(req.file)
        self.assertIsNone(req.line)

    def test_invalid_multiple_modes(self) -> None:
        with self.assertRaises(ValidationError):
            BreakRequest(file="src/foo.cc", line=42, function="bar")

    def test_invalid_incomplete_source(self) -> None:
        with self.assertRaises(ValidationError):
            BreakRequest(file="src/foo.cc")


class TestWatchRequestSchema(ProtocolTestCase):
    def test_make_watch_request(self) -> None:
        self.assertEqual(WatchRequest.response_type, dict[str, Any])
        req = self.assert_request_parses(
            {
                "command": "watch",
                "name": "my_var",
                "thread_id": 1,
                "frame_index": 2,
                "access_type": "readWrite",
            },
            WatchRequest,
        )
        self.assertEqual(req.name, "my_var")
        self.assertEqual(req.thread_id, 1)
        self.assertEqual(req.frame_index, 2)
        self.assertEqual(req.access_type, "readWrite")
        self.assertFalse(req.delete)

        with self.assertRaises(ValidationError):
            make_request(
                {
                    "command": "watch",
                    "name": "my_var",
                    "thread_id": 1,
                    "access_type": "read",
                }
            )

        del_req = self.assert_request_parses(
            {
                "command": "watch",
                "name": "1001:0x1234:4",
                "delete": True,
            },
            WatchRequest,
        )
        self.assertEqual(del_req.name, "1001:0x1234:4")
        self.assertTrue(del_req.delete)


if __name__ == "__main__":
    unittest.main()
