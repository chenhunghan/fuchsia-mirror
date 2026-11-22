# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import PROTOCOL_VERSION
from shared.protocol.hello import HelloRequest
from shared.protocol.start import StartRequest
from shared.protocol.stop import StopRequest
from shared.tests.helpers import ProtocolTestCase


class TestHelloRequestSchema(ProtocolTestCase):
    def test_valid(self) -> None:
        req = HelloRequest(version=PROTOCOL_VERSION)
        self.assertEqual(req.version, PROTOCOL_VERSION)
        self.assertEqual(HelloRequest.response_type, dict[str, Any])

    def test_missing_version(self) -> None:
        with self.assertRaises(ValidationError):
            # Deliberately invalid, to test validation.
            # pyrefly: ignore[missing-argument]
            HelloRequest()

    def test_type_coercion(self) -> None:
        # Pydantic coerces valid integer-like strings to integers by default.
        req = HelloRequest(version="5")
        self.assertEqual(req.version, 5)

        with self.assertRaises(ValidationError):
            HelloRequest(version="not-an-int")


class TestStartAndStopSchemas(ProtocolTestCase):
    def test_parse_start(self) -> None:
        req = self.assert_request_parses(
            {"command": "start", "port": 15678, "connect": True},
            StartRequest,
        )
        self.assertEqual(req.port, 15678)
        self.assertTrue(req.connect)
        self.assertEqual(StartRequest.response_type, dict[str, Any])

    def test_parse_stop(self) -> None:
        req = self.assert_request_parses(
            {"command": "stop", "ack_seq": 10},
            StopRequest,
        )
        self.assertEqual(req.ack_seq, 10)
        self.assertIsNone(StopRequest.response_type)


if __name__ == "__main__":
    unittest.main()
