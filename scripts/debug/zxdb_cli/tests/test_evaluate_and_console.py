# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import json
from io import StringIO

from cli.cli import main
from tests.fake_dap_server import (
    FakeDapServerMixin,
    _cleanup_process_group,
)


class TestEvaluateAndConsoleIntegration(FakeDapServerMixin):
    async def test_evaluate_command(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            exit_code = await main(["pause", "-t", "1"])
            self.assertEqual(exit_code, 0)

            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(
                    ["evaluate", "--thread-id", "1", "1", "+", "1"]
                )
            self.assertEqual(exit_code, 0)

            output = f.getvalue()
            output_json = json.loads(output)
            self.assertTrue(output_json.get("success"))
            body = output_json.get("body")
            self.assertIsNotNone(body)
            self.assertEqual(body.get("result"), "42")
            self.assertEqual(body.get("type"), "int")

            # Verify that the fake DAP server received the evaluate command
            eval_req = next(
                (
                    r
                    for r in self.received_dap_requests
                    if r.get("command") == "evaluate"
                ),
                None,
            )
            self.assertIsNotNone(eval_req)
            assert eval_req is not None
            self.assertEqual(eval_req["arguments"]["expression"], "1 + 1")
            self.assertEqual(eval_req["arguments"]["frameId"], 1)

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_console_command(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(["console", "sym-stat"])
            self.assertEqual(exit_code, 0)

            output = f.getvalue()
            output_json = json.loads(output)
            self.assertTrue(output_json.get("success"))
            body = output_json.get("body")
            self.assertIsNotNone(body)
            self.assertEqual(body.get("output"), "Console output OK\n")

            console_req = next(
                (
                    r
                    for r in self.received_dap_requests
                    if r.get("command") == "zxdb.ConsoleCommand"
                ),
                None,
            )
            self.assertIsNotNone(console_req)
            assert console_req is not None
            self.assertEqual(console_req["arguments"]["command"], "sym-stat")
            self.assertNotIn("frameId", console_req["arguments"])

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)
