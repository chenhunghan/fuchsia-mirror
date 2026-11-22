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


class TestAttachDetachIntegration(FakeDapServerMixin):
    async def test_detach_command(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Attach to trigger process event
            exit_code = await main(["attach", "test_process"])
            self.assertEqual(exit_code, 0)

            # Wait for the process event (seq 1) instead of sleeping
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0", "--timeout", "5"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "process")
            self.assertEqual(events[0]["body"]["systemProcessId"], 1234)

            # Detach via CLI
            exit_code = await main(["detach", "1234"])
            self.assertEqual(exit_code, 0)

            # Wait for synthesized detached event (seq 2) instead of sleeping
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "1", "--timeout", "5"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "detached")
            self.assertEqual(events[0]["body"]["pid"], 1234)

            # Verify zxdb.Detach was received by fake server
            detach_req = next(
                (
                    r
                    for r in self.received_dap_requests
                    if r.get("command") == "zxdb.Detach"
                ),
                None,
            )
            self.assertIsNotNone(
                detach_req,
                "zxdb.Detach command was not received by fake server",
            )
            assert detach_req is not None
            self.assertEqual(detach_req.get("arguments", {}).get("pid"), 1234)

            # Verify active processes cleaned up in state
            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(["get-state"])
            self.assertEqual(exit_code, 0)
            state_resp = json.loads(f.getvalue())
            self.assertTrue(state_resp.get("success"))
            self.assertEqual(state_resp.get("body", {}).get("processes"), {})

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_detach_all_command(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Attach to trigger process event
            exit_code = await main(["attach", "test_process"])
            self.assertEqual(exit_code, 0)

            # Wait for the process event (seq 1)
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "0", "--timeout", "5"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "process")
            self.assertEqual(events[0]["body"]["systemProcessId"], 1234)

            # Detach all via CLI
            exit_code = await main(["detach", "--all"])
            self.assertEqual(exit_code, 0)

            # Wait for synthesized detached event (seq 2)
            out = StringIO()
            with contextlib.redirect_stdout(out):
                exit_code = await main(
                    ["wait-for-event", "--last-seen-seq", "1", "--timeout", "5"]
                )
            self.assertEqual(exit_code, 0)
            output = out.getvalue().strip()

            resp = json.loads(output)
            self.assertTrue(resp.get("success"))
            events = resp.get("events", [])
            self.assertGreater(len(events), 0)
            self.assertEqual(events[0]["event"], "detached")
            self.assertTrue(events[0]["body"]["all"])

            # Verify zxdb.Detach (all: True) was received by fake server
            detach_req = next(
                (
                    r
                    for r in self.received_dap_requests
                    if r.get("command") == "zxdb.Detach"
                ),
                None,
            )
            self.assertIsNotNone(
                detach_req,
                "zxdb.Detach command was not received by fake server",
            )
            assert detach_req is not None
            self.assertTrue(detach_req.get("arguments", {}).get("all"))

            # Verify active processes cleaned up in state
            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(["get-state"])
            self.assertEqual(exit_code, 0)
            state_resp = json.loads(f.getvalue())
            self.assertTrue(state_resp.get("success"))
            self.assertEqual(state_resp.get("body", {}).get("processes"), {})

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)
