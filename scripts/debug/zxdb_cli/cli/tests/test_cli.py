# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from io import StringIO
from unittest.mock import AsyncMock, Mock, patch

from cli.cli import main, send_command
from cli.tests.helpers import CLITestCase
from shared.protocol.break_request import BreakRequest
from shared.protocol.evaluate import EvaluateRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.get_state import GetStateRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.stack_trace import StackTraceRequest
from shared.protocol.step_in import StepInRequest
from shared.protocol.variables import VariablesRequest
from shared.protocol.wait_for_event import WaitForEventRequest


class TestCLI(CLITestCase):
    @patch("cli.cli.send_command")
    async def test_json_option_mutual_exclusion(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "stop"}', "stop"])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.commands.stop.stop_daemon")
    @patch("cli.cli.send_command")
    async def test_json_option_valid(
        self, mock_send: Mock, mock_stop: Mock
    ) -> None:
        mock_stop.return_value = 0
        exit_code = await main(["--json", '{"command": "stop"}'])
        self.assertEqual(exit_code, 0)
        mock_stop.assert_called_once()
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_invalid(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "invalid"}'])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0

        # stack-trace aliases
        for alias in ["stack_trace", "stackTrace", "bt", "backtrace"]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "thread_id": 1}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(StackTraceRequest(thread_id=1))

        # step-in aliases
        for alias in [
            "step_in",
            "stepIn",
            "stepin",
            "step-into",
            "step_into",
            "stepinto",
            "step",
            "s",
        ]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "thread_id": 1}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(StepInRequest(thread_id=1))

        # get-state aliases
        for alias in ["get_state", "getState"]:
            mock_send.reset_mock()
            exit_code = await main(["--json", f'{{"command": "{alias}"}}'])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(GetStateRequest())

        # wait-for-event aliases
        for alias in ["wait_for_event", "waitForEvent"]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "last_seen_seq": 10}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                WaitForEventRequest(last_seen_seq=10)
            )

        # next aliases
        for alias in ["step-over", "step_over", "stepOver", "stepover", "n"]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "thread_id": 1}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(NextRequest(thread_id=1))

        # finish aliases
        for alias in ["step-out", "step_out", "stepOut", "stepout"]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "thread_id": 1}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(FinishRequest(thread_id=1))

        # variables aliases
        for alias in ["locals"]:
            mock_send.reset_mock()
            exit_code = await main(
                ["--json", f'{{"command": "{alias}", "thread_id": 1}}']
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(VariablesRequest(thread_id=1))

        # break aliases
        for alias in [
            "breakpoint",
            "b",
            "setBreakpoints",
            "set-breakpoints",
            "set_breakpoints",
        ]:
            mock_send.reset_mock()
            exit_code = await main(
                [
                    "--json",
                    f'{{"command": "{alias}", "file": "main.rs", "line": 10}}',
                ]
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                BreakRequest(file="main.rs", line=10)
            )

        # evaluate aliases
        for alias in ["print"]:
            mock_send.reset_mock()
            exit_code = await main(
                [
                    "--json",
                    f'{{"command": "{alias}", "thread_id": 1, "expression": "x"}}',
                ]
            )
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                EvaluateRequest(thread_id=1, expression="x")
            )


class TestSendCommand(CLITestCase):
    @patch("cli.cli.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_send_command_valid_response(
        self, mock_open_conn: Mock, mock_uds_path: Mock
    ) -> None:
        mock_uds_path.exists.return_value = True
        mock_reader = AsyncMock()
        mock_writer = AsyncMock()
        mock_reader.readline.return_value = (
            b'{"success": true, "message": null, "events": null, "body": '
            b'{"threads": [{"thread_id": 1, "name": "t1"}], "processes": null, "breakpoints": null}}\n'
        )
        mock_open_conn.return_value = (mock_reader, mock_writer)

        stdout = StringIO()
        with patch("sys.stdout", stdout):
            exit_code = await send_command(GetStateRequest())

        self.assertEqual(exit_code, 0)
        self.assertIn('"success":true', stdout.getvalue().replace(" ", ""))

    @patch("cli.cli.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_send_command_invalid_response(
        self, mock_open_conn: Mock, mock_uds_path: Mock
    ) -> None:
        mock_uds_path.exists.return_value = True
        mock_reader = AsyncMock()
        mock_writer = AsyncMock()
        mock_reader.readline.return_value = (
            b'{"success": true, "message": null, "events": null, "body": '
            b'{"threads": "invalid_threads_format"}}\n'
        )
        mock_open_conn.return_value = (mock_reader, mock_writer)

        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await send_command(GetStateRequest())

        self.assertEqual(exit_code, 1)
        self.assertIn("Invalid response from daemon", stderr.getvalue())

    @patch("cli.cli.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_send_command_socket_eof(
        self, mock_open_conn: Mock, mock_uds_path: Mock
    ) -> None:
        mock_uds_path.exists.return_value = True
        mock_reader = AsyncMock()
        mock_writer = AsyncMock()
        mock_reader.readline.return_value = b""
        mock_open_conn.return_value = (mock_reader, mock_writer)

        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await send_command(GetStateRequest())

        self.assertEqual(exit_code, 1)
        self.assertIn("No response received from daemon", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
