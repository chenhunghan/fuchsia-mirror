# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import unittest
from typing import Any, TypeVar

from shared.protocol import (
    BaseRequest,
    Response,
    deserialize_response,
    make_request,
)

T = TypeVar("T")


class ProtocolTestCase(unittest.TestCase):
    """Base test class with shared helpers for protocol schema tests."""

    def make_response_json(
        self,
        body: Any = None,
        *,
        success: bool = True,
        message: str | None = None,
        events: list[dict[str, Any]] | None = None,
    ) -> str:
        """Serializes a protocol response envelope dictionary to a JSON line."""
        return json.dumps(
            {
                "success": success,
                "message": message,
                "events": events,
                "body": body,
            }
        )

    def assert_deserializes_body(
        self,
        req: BaseRequest[Any],
        body_payload: dict[str, Any] | None,
        expected_type: type[T],
    ) -> T:
        """Deserializes a response payload for req and verifies the body type."""
        json_line = self.make_response_json(body=body_payload)
        resp: Response[Any] = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, expected_type)
        assert isinstance(resp.body, expected_type)
        return resp.body

    def assert_request_parses(
        self, payload: dict[str, Any], expected_type: type[T]
    ) -> T:
        """Parses payload with make_request and verifies the request type."""
        req = make_request(payload)
        self.assertIsInstance(req, expected_type)
        assert isinstance(req, expected_type)
        return req
