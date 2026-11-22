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


class TestStackTraceIntegration(FakeDapServerMixin):
    async def test_stack_trace(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            f = StringIO()
            with contextlib.redirect_stdout(f):
                exit_code = await main(["stack-trace", "-t", "1"])
            self.assertEqual(exit_code, 0)

            output = f.getvalue()
            output_json = json.loads(output)
            self.assertTrue(output_json.get("success"))

            # Verify stack frame
            body = output_json.get("body")
            self.assertIsNotNone(body)
            frames = body.get("stack_frames")
            self.assertEqual(len(frames), 1)
            self.assertEqual(frames[0]["frame_index"], 0)
            self.assertEqual(frames[0]["name"], "main")
            self.assertEqual(frames[0]["source"]["name"], "main.cc")
            self.assertEqual(frames[0]["trust"], "CFI")

            # Verify unwinder and force-remote-unwind flags forward to DAP
            f_unwinder = StringIO()
            with contextlib.redirect_stdout(f_unwinder):
                exit_code = await main(
                    [
                        "stack-trace",
                        "-t",
                        "1",
                        "--force-remote-unwind",
                        "--unwinder",
                        "fp",
                    ]
                )
            self.assertEqual(exit_code, 0)
            last_stack_req = [
                r
                for r in self.received_dap_requests
                if r.get("command") == "stackTrace"
            ][-1]
            self.assertTrue(last_stack_req["arguments"].get("remoteUnwind"))
            self.assertEqual(last_stack_req["arguments"].get("unwinder"), "fp")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)
