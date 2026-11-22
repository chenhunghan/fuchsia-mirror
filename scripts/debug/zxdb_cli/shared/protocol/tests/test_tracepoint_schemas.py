# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import deserialize_response
from shared.protocol.tracepoint import (
    TracepointInfo,
    TracepointRequest,
    TracepointResponse,
)
from shared.tests.helpers import ProtocolTestCase


class TestTracepointSchema(ProtocolTestCase):
    def test_valid_function_tracepoint(self) -> None:
        req = TracepointRequest(
            function="foo::Bar",
            expressions=["x", "y"],
            capture=["frame", "locals"],
            max_hits=10,
        )
        self.assertEqual(req.command, "tracepoint")
        self.assertEqual(req.function, "foo::Bar")
        self.assertEqual(req.max_hits, 10)
        self.assertEqual(TracepointRequest.response_type, TracepointResponse)

    def test_raw_backtrace_implies_backtrace(self) -> None:
        req = TracepointRequest(
            function="foo::Bar",
            raw_backtrace=True,
        )
        self.assertTrue(req.raw_backtrace)
        self.assertIn("backtrace", req.capture)

    def test_valid_list_all(self) -> None:
        req = TracepointRequest(list_all=True)
        self.assertTrue(req.list_all)

    def test_valid_id_alias(self) -> None:
        req = TracepointRequest(id=1, delete=True)
        self.assertEqual(req.tracepoint_id, 1)

    def test_tracepoint_info_with_data_id(self) -> None:
        info = TracepointInfo(
            id=1,
            location="watch:foo",
            watch="foo",
            data_id="123:0x1000:4",
        )
        self.assertEqual(info.data_id, "123:0x1000:4")

    def test_max_matching_frames_validation(self) -> None:
        req = TracepointRequest(
            function="foo::Bar",
            max_matching_frames=5,
        )
        self.assertEqual(req.max_matching_frames, 5)

        with self.assertRaises(ValidationError):
            TracepointRequest(function="foo::Bar", max_matching_frames=0)

    def test_invalid_missing_target(self) -> None:
        with self.assertRaises(ValidationError):
            TracepointRequest()

    def test_deserialize_tracepoint_response(self) -> None:
        req = TracepointRequest(list_all=True)
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"tracepoints": [{"id": 1, "location": "foo.cc:10", "hit_count": 5, '
            '"max_hits": 5, "enabled": false, "auto_disabled": true, "verified": true}]}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, TracepointResponse)
        assert resp.body is not None
        self.assertEqual(len(resp.body.tracepoints), 1)
        self.assertEqual(resp.body.tracepoints[0].id, 1)
        self.assertEqual(resp.body.tracepoints[0].location, "foo.cc:10")
        self.assertFalse(resp.body.tracepoints[0].enabled)
        self.assertTrue(resp.body.tracepoints[0].auto_disabled)
        self.assertTrue(resp.body.tracepoints[0].verified)

    def test_tracepoint_management_modes(self) -> None:
        clear_req = TracepointRequest(clear=True)
        self.assertTrue(clear_req.clear)

        en_req = TracepointRequest(tracepoint_id=3, enabled=True)
        self.assertEqual(en_req.tracepoint_id, 3)
        self.assertEqual(en_req.enabled, True)

        dis_req = TracepointRequest(id=4, enabled=False)
        self.assertEqual(dis_req.tracepoint_id, 4)
        self.assertEqual(dis_req.enabled, False)

    def test_tracepoint_conflicting_flags_and_id_aliases(self) -> None:
        invalid_kwargs: list[dict[str, Any]] = [
            {"list_all": True, "delete": True},
            {"list_all": True, "clear": True},
            {"list_all": True, "function": "foo"},
            {"list_all": True, "tracepoint_id": 1},
            {"clear": True, "tracepoint_id": 1},
            {"clear": True, "function": "foo"},
            {"delete": True},
            {"delete": True, "tracepoint_id": 1, "function": "other"},
            {"delete": True, "function": "foo", "raw_backtrace": True},
            {"enabled": True},
            {"enabled": False, "tracepoint_id": 1, "function": "foo"},
            {"id": 1, "tracepoint_id": 2, "function": "foo"},
        ]
        for kwargs in invalid_kwargs:
            with self.assertRaises(ValidationError, msg=f"Failed for {kwargs}"):
                TracepointRequest(**kwargs)

    def test_tracepoint_validation_numeric_bounds(self) -> None:
        with self.assertRaises(ValueError):
            TracepointRequest(function="foo", tracepoint_id=0)
        with self.assertRaises(ValueError):
            TracepointRequest(function="foo", tracepoint_id=-1)
        with self.assertRaises(ValueError):
            TracepointRequest(file="a.cc", line=0)
        with self.assertRaises(ValueError):
            TracepointRequest(file="a.cc", line=-1)
        with self.assertRaises(ValueError):
            TracepointRequest(watch="0x1000", size=0)
        with self.assertRaises(ValueError):
            TracepointRequest(watch="0x1000", size=-1)
        with self.assertRaises(ValueError):
            TracepointRequest(function="foo", frame_index=-1)
        with self.assertRaises(ValueError):
            TracepointRequest(function="foo", max_hits=0)
        with self.assertRaises(ValueError):
            TracepointRequest(function="foo", max_matching_frames=0)
        with self.assertRaises(ValueError):
            TracepointRequest(function="")
        with self.assertRaises(ValueError):
            TracepointRequest(function="   ")
        with self.assertRaises(ValueError):
            TracepointRequest(file="", line=10)
        with self.assertRaises(ValueError):
            TracepointRequest(file="   ", line=10)
        with self.assertRaises(ValueError):
            TracepointRequest(watch="")
        with self.assertRaises(ValueError):
            TracepointRequest(watch="   ")


if __name__ == "__main__":
    unittest.main()
