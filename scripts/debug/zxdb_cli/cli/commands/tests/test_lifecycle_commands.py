# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import unittest
from io import StringIO
from unittest.mock import AsyncMock, Mock, patch

from cli.cli import main
from cli.commands.stop import stop_daemon
from cli.tests.helpers import CLITestCase
from daemon_manager.manager import (
    UDS_PATH,
    DaemonAlreadyRunningError,
    DaemonConnectionError,
    DaemonCrashError,
    DaemonHandshakeError,
    DaemonStartupTimeoutError,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.stop import StopRequest


class TestLifecycleCommands(CLITestCase):
    @patch("cli.commands.start.start_daemon")
    async def test_start_command(self, mock_start: Mock) -> None:
        mock_start.return_value = 0
        exit_code = await main(["start"])
        self.assertEqual(exit_code, 0)
        mock_start.assert_called_once()

    @patch("cli.commands.stop.stop_daemon")
    async def test_stop_command(self, mock_stop: Mock) -> None:
        mock_stop.return_value = 0
        exit_code = await main(["stop"])
        self.assertEqual(exit_code, 0)
        mock_stop.assert_called_once()

    @patch("cli.commands.stop.DaemonManager")
    async def test_stop_daemon_success(self, mock_manager_cls: Mock) -> None:
        mock_manager = AsyncMock()
        mock_manager_cls.return_value = mock_manager
        mock_manager.stop = AsyncMock()

        stdout = StringIO()
        with patch("sys.stdout", stdout):
            exit_code = await stop_daemon()

        self.assertEqual(exit_code, 0)
        mock_manager_cls.assert_called_once_with(socket_path=UDS_PATH)
        mock_manager.stop.assert_awaited_once()
        output = json.loads(stdout.getvalue().strip())
        self.assertEqual(
            output,
            {"success": True, "message": "Daemon stopped"},
        )

    @patch("cli.commands.stop.DaemonManager")
    async def test_stop_daemon_failure(self, mock_manager_cls: Mock) -> None:
        mock_manager = AsyncMock()
        mock_manager_cls.return_value = mock_manager
        mock_manager.stop = AsyncMock(side_effect=RuntimeError("stop failed"))

        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await stop_daemon()

        self.assertEqual(exit_code, 1)
        mock_manager_cls.assert_called_once_with(socket_path=UDS_PATH)
        mock_manager.stop.assert_awaited_once()
        self.assertIn("Error stopping daemon: stop failed", stderr.getvalue())

    @patch("cli.cli.send_command")
    async def test_attach_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["attach", "my_process"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AttachRequest(filter="my_process"))

    @patch("cli.cli.send_command")
    async def test_attach_command_flags(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "attach",
                "driver_host",
                "--weak",
                "--exact",
                "--recursive",
                "--job-only",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(
                filter="driver_host",
                weak=True,
                exact=True,
                recursive=True,
                job_only=True,
            )
        )

        mock_send.reset_mock()
        exit_code = await main(["attach", "driver_host", "-r"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(filter="driver_host", recursive=True)
        )

    @patch("cli.cli.send_command")
    async def test_attach_driver_and_node_flags(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["attach", "--driver", "xhci.cm", "--weak", "--wait"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(driver="xhci.cm", weak=True, wait=True)
        )

        mock_send.reset_mock()
        exit_code = await main(
            [
                "attach",
                "--driver",
                "xhci.cm",
                "--weak",
                "--wait",
                "-r",
                "--job-only",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(
                driver="xhci.cm",
                weak=True,
                wait=True,
                recursive=True,
                job_only=True,
            )
        )

        mock_send.reset_mock()
        with patch("sys.stderr", StringIO()) as mock_stderr:
            exit_code = await main(["attach", "--driver", "xhci.cm", "--exact"])
            self.assertEqual(exit_code, 1)
            self.assertIn(
                "--exact cannot be used with --driver or --node",
                mock_stderr.getvalue(),
            )
            mock_send.assert_not_called()

        mock_send.reset_mock()
        with patch("sys.stderr", StringIO()) as mock_stderr:
            exit_code = await main(
                ["attach", "--node", "dev.sys.platform.xhci", "--exact"]
            )
            self.assertEqual(exit_code, 1)
            self.assertIn(
                "--exact cannot be used with --driver or --node",
                mock_stderr.getvalue(),
            )
            mock_send.assert_not_called()

        mock_send.reset_mock()
        exit_code = await main(["attach", "--node", "dev.sys.platform.xhci"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(node="dev.sys.platform.xhci")
        )

        mock_send.reset_mock()
        exit_code = await main(
            ["attach", "--node", "dev.sys.platform.xhci", "--wait"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AttachRequest(node="dev.sys.platform.xhci", wait=True)
        )

        mock_send.reset_mock()
        with patch("sys.stderr", StringIO()):
            exit_code = await main(["attach"])
            self.assertEqual(exit_code, 1)
            mock_send.assert_not_called()

            exit_code = await main(["attach", "xhci", "--driver", "xhci.cm"])
            self.assertEqual(exit_code, 1)
            mock_send.assert_not_called()

    @patch("cli.cli.make_request")
    @patch("cli.cli.send_command")
    async def test_attach_command_receives_args(
        self, mock_send: Mock, mock_make: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_make.return_value = StopRequest()
        await main(["attach", "my_process"])

        mock_make.assert_called_once()
        args = mock_make.call_args[0][0]  # first argument
        self.assertIn("filter", args)
        self.assertEqual(args["filter"], "my_process")

    @patch("cli.cli.send_command")
    async def test_json_option_attach(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "attach", "filter": "my_process"}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AttachRequest(filter="my_process"))

    @patch("cli.commands.start.DaemonManager")
    async def test_start_command_errors_formatting(
        self, mock_manager_class: Mock
    ) -> None:
        mock_manager = mock_manager_class.return_value

        exceptions_to_test = [
            DaemonAlreadyRunningError("Daemon socket already exists"),
            DaemonConnectionError("Connection failed"),
            DaemonCrashError("Daemon exited prematurely"),
            DaemonHandshakeError("Protocol version mismatch"),
            DaemonStartupTimeoutError("Startup timed out"),
        ]

        for exc in exceptions_to_test:
            mock_manager.start = AsyncMock(side_effect=exc)
            stderr = StringIO()
            with patch("sys.stderr", stderr):
                exit_code = await main(["start"])

            self.assertEqual(exit_code, 1)
            output = json.loads(stderr.getvalue())
            self.assertFalse(output["success"])
            self.assertEqual(output["message"], str(exc))

    @patch("cli.commands.start.DaemonManager")
    async def test_start_command_generic_exception_formatting(
        self, mock_manager_class: Mock
    ) -> None:
        mock_manager = mock_manager_class.return_value
        mock_manager.start = AsyncMock(
            side_effect=RuntimeError("Unexpected error")
        )

        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await main(["start"])

        self.assertEqual(exit_code, 1)
        output = json.loads(stderr.getvalue())
        self.assertFalse(output["success"])
        self.assertIn(
            "Failed to start daemon: Unexpected error", output["message"]
        )


if __name__ == "__main__":
    unittest.main()
