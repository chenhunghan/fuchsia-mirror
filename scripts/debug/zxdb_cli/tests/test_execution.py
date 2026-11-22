# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from cli.cli import main
from tests.fake_dap_server import (
    FakeDapServerMixin,
    _cleanup_process_group,
)


class TestExecutionIntegration(FakeDapServerMixin):
    async def test_pause_continue(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Test Pause
            exit_code = await main(["pause", "-t", "1"])
            self.assertEqual(exit_code, 0)

            # Test Continue
            exit_code = await main(["continue", "1"])
            self.assertEqual(exit_code, 0)

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)
