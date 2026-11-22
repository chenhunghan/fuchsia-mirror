# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for OpenThread CLI parsers."""

from __future__ import annotations

import unittest

from honeydew.affordances.connectivity.thread.errors import ThreadCommandError
from honeydew.affordances.connectivity.thread.types import ThreadRole
from honeydew.affordances.connectivity.thread.utils import ot_cli


class OtCliParserTests(unittest.TestCase):
    """Unit tests for transport-agnostic OpenThread CLI parsers."""

    def test_parse_response_strips_prompt_and_done(self) -> None:
        raw = "> state\r\nleader\r\nDone\r\n"
        self.assertEqual(ot_cli.parse_response("state", raw), ["leader"])
        self.assertEqual(ot_cli.parse_role("state", raw), ThreadRole.LEADER)
        self.assertEqual(
            ot_cli.parse_response(
                "state", ">\r\n> \r\nleader\r\n>\r\nDone\r\n"
            ),
            ["leader"],
        )
        self.assertEqual(
            ot_cli.parse_response("state", "> leader\r\nDone\r\n"),
            ["leader"],
        )

    def test_parse_response_raises_on_error(self) -> None:
        raw = "Error 7: InvalidArgs\r\n"
        with self.assertRaises(ThreadCommandError) as ctx:
            ot_cli.parse_response("dataset channel 99", raw)
        self.assertIn("Error 7: InvalidArgs", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
