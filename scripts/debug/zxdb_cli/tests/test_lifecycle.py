# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import signal

from cli.cli import main
from daemon.constants import UDS_PATH
from shared.protocol import PROTOCOL_VERSION, serialize
from shared.protocol.hello import HelloRequest
from shared.protocol.start import StartRequest
from tests.fake_dap_server import (
    FakeDapServerMixin,
    _cleanup_process_group,
)


class TestDaemonLifecycleIntegration(FakeDapServerMixin):
    async def test_daemon_lifecycle(self) -> None:
        """Tests that the daemon starts and stops correctly."""
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

            # Wait for process to exit
            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=5.0
                )
            except asyncio.TimeoutError:
                self.fail("Daemon process did not exit after stop")

            # Verify socket is deleted
            self.assertFalse(UDS_PATH.exists(), "Socket file was not deleted")

        finally:
            await _cleanup_process_group(proc)

    async def test_daemon_idempotent_start(self) -> None:
        """Tests that sending StartRequest twice is handled cleanly."""
        # _setup_daemon_and_server() already sends an initial StartRequest.
        proc, port = await self._setup_daemon_and_server()

        try:
            # Send StartRequest a second time
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            start_req = StartRequest(port=port, connect=True)
            writer.write(serialize(start_req).encode("utf-8"))
            await writer.drain()
            response = await asyncio.wait_for(reader.readline(), timeout=5.0)
            writer.close()
            await writer.wait_closed()

            resp_dict = json.loads(response.decode("utf-8"))
            self.assertTrue(resp_dict.get("success"))
            self.assertEqual(resp_dict.get("message"), "Daemon already started")

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_daemon_start_different_port(self) -> None:
        """Tests that sending StartRequest with a different port fails if already running."""
        # _setup_daemon_and_server() already sends an initial StartRequest.
        proc, port = await self._setup_daemon_and_server()

        try:
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            # Use a different port
            different_port = port + 1
            start_req = StartRequest(port=different_port, connect=True)
            writer.write(serialize(start_req).encode("utf-8"))
            await writer.drain()
            response = await asyncio.wait_for(reader.readline(), timeout=5.0)
            writer.close()
            await writer.wait_closed()

            resp_dict = json.loads(response.decode("utf-8"))
            self.assertFalse(resp_dict.get("success"))
            self.assertIn("cannot switch to", resp_dict.get("message", ""))

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_daemon_start_different_connect(self) -> None:
        """Tests that sending StartRequest with a different connect value fails if already running."""
        # _setup_daemon_and_server() already sends an initial StartRequest.
        proc, port = await self._setup_daemon_and_server()

        try:
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            # Use a different connect value (already running with connect=True, so use False)
            start_req = StartRequest(port=port, connect=False)
            writer.write(serialize(start_req).encode("utf-8"))
            await writer.drain()
            response = await asyncio.wait_for(reader.readline(), timeout=5.0)
            writer.close()
            await writer.wait_closed()

            resp_dict = json.loads(response.decode("utf-8"))
            self.assertFalse(resp_dict.get("success"))
            self.assertIn("cannot switch to", resp_dict.get("message", ""))

            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

        finally:
            await _cleanup_process_group(proc)

    async def test_daemon_hello(self) -> None:
        """Tests the versioned hello handshake."""

        async def run_test() -> None:
            proc, _port = await self._setup_daemon_and_server()

            try:
                # Test valid version
                reader, writer = await asyncio.open_unix_connection(UDS_PATH)
                req = HelloRequest(version=PROTOCOL_VERSION)
                writer.write(serialize(req).encode("utf-8"))
                await writer.drain()

                try:
                    # Intentionally kept longer than handle_hello timeout.
                    response = await asyncio.wait_for(
                        reader.readline(), timeout=15.0
                    )
                except asyncio.TimeoutError:
                    self.fail("Timed out waiting for response from daemon")
                writer.close()
                await writer.wait_closed()

                resp_dict = json.loads(response.decode("utf-8"))
                self.assertTrue(resp_dict.get("success"))
                self.assertEqual(
                    resp_dict.get("body", {}).get("protocol_version"),
                    PROTOCOL_VERSION,
                )

                # Test invalid version
                reader, writer = await asyncio.open_unix_connection(UDS_PATH)
                req = HelloRequest(version=PROTOCOL_VERSION + 1)
                writer.write(serialize(req).encode("utf-8"))
                await writer.drain()

                try:
                    response = await asyncio.wait_for(
                        reader.readline(), timeout=5.0
                    )
                except asyncio.TimeoutError:
                    self.fail("Timed out waiting for response from daemon")
                writer.close()
                await writer.wait_closed()

                resp_dict = json.loads(response.decode("utf-8"))
                self.assertFalse(resp_dict.get("success"))
                self.assertIn("version mismatch", resp_dict.get("message", ""))

            finally:
                await _cleanup_process_group(proc)
                if self.fake_dap_server:
                    self.fake_dap_server.close()
                    await self.fake_dap_server.wait_closed()
                    self.fake_dap_server = None

        # Run the test twice to ensure that the daemon can be started and stopped
        # repeatedly without leaving stale state or leaking resources.
        try:
            await asyncio.wait_for(run_test(), timeout=30.0)
        except asyncio.TimeoutError:
            self.fail("Test timed out")

        try:
            await asyncio.wait_for(run_test(), timeout=30.0)
        except asyncio.TimeoutError:
            self.fail("Test timed out")

    async def test_daemon_sigterm(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            proc.send_signal(signal.SIGTERM)

            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=5.0
                )
            except asyncio.TimeoutError:
                self.fail("Daemon process did not exit after SIGTERM")

            self.assertFalse(
                UDS_PATH.exists(), "Socket file was not deleted after SIGTERM"
            )

        finally:
            await _cleanup_process_group(proc)

    async def test_daemon_sigint(self) -> None:
        proc, _port = await self._setup_daemon_and_server()

        try:
            proc.send_signal(signal.SIGINT)

            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=5.0
                )
            except asyncio.TimeoutError:
                self.fail("Daemon process did not exit after SIGINT")

            self.assertFalse(
                UDS_PATH.exists(), "Socket file was not deleted after SIGINT"
            )

        finally:
            await _cleanup_process_group(proc)

    async def test_stop_detaches_existing_session(self) -> None:
        """Tests that stopping the daemon detaches from all processes if connected to existing."""
        proc, _port = await self._setup_daemon_and_server()

        try:
            # Stop via CLI
            exit_code = await main(["stop"])
            self.assertEqual(exit_code, 0)

            # Wait for daemon to exit
            await asyncio.wait_for(asyncio.to_thread(proc.wait), timeout=5.0)

            # Verify that the fake DAP server received "zxdb.Detach" request
            detach_request = None
            for req in self.received_dap_requests:
                if req.get("command") == "zxdb.Detach":
                    detach_request = req
                    break

            self.assertIsNotNone(
                detach_request, "zxdb.Detach was not received by DAP server"
            )
            assert detach_request is not None
            self.assertTrue(
                detach_request["arguments"].get("all"),
                "zxdb.Detach did not specify 'all'",
            )

        finally:
            await _cleanup_process_group(proc)
