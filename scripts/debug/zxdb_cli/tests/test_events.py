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


class TestEventsIntegration(FakeDapServerMixin):
    async def test_wait_for_event(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Trigger an event by sending pause command
            exit_code = await main(["pause", "-t", "1"])
            self.assertEqual(exit_code, 0)

            # Capture stdout to read the JSON response from wait-for-event
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            # Parse output
            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "stopped")
            self.assertEqual(events[0]["body"]["thread_id"], 1)
            self.assertNotIn("threadId", events[0]["body"])

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_process_event(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Trigger a process event by sending attach command
            exit_code = await main(["attach", "test_process"])
            self.assertEqual(exit_code, 0)

            # Capture stdout to read the JSON response from wait-for-event
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            # Parse output
            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)

            # Find process event
            process_event = None
            for event in events:
                if event.get("event") == "process":
                    process_event = event
                    break

            self.assertIsNotNone(
                process_event, "Process event was not received"
            )
            assert process_event is not None
            self.assertEqual(process_event["body"]["systemProcessId"], 1234)
            self.assertEqual(process_event["body"]["name"], "test_process")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)
