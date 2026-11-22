# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import unittest
from io import StringIO
from unittest.mock import Mock, patch

from cli.cli import main
from cli.tests.helpers import CLITestCase
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.step_in import StepInRequest


class TestExecutionCommands(CLITestCase):
    @patch("cli.cli.send_command")
    async def test_json_option_continue(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "continue", "thread_id": 1, "single_thread": true}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ContinueRequest(thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_finish_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["finish", "1", "--single-thread"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            FinishRequest(command="finish", thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_finish_command_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["step-out", "step_out", "stepout"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                FinishRequest(command="finish", thread_id=1)
            )

    @patch("cli.cli.send_command")
    async def test_finish_command_single_thread(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["finish", "1", "--single-thread"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            FinishRequest(command="finish", thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_finish(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "finish", "thread_id": 1, "single_thread": true}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            FinishRequest(command="finish", thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_next_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["next", "1", "--single-thread", "--granularity", "line"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            NextRequest(
                command="next",
                thread_id=1,
                single_thread=True,
                granularity="line",
            )
        )

    @patch("cli.cli.send_command")
    async def test_next_command_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["n", "step-over", "step_over", "stepover"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                NextRequest(command="next", thread_id=1)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_next(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "next", "thread_id": 1, "single_thread": true, "granularity": "line"}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            NextRequest(thread_id=1, single_thread=True, granularity="line")
        )

    @patch("cli.cli.send_command")
    async def test_json_option_pause(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "pause", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_pause_command_thread_id(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["pause", "-t", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

        mock_send.reset_mock()
        exit_code = await main(["pause", "--thread-id", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_pause_command_pid(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["pause", "-p", "12345"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(pid=12345))

    @patch("cli.cli.send_command")
    async def test_pause_command_both_thread_id_and_pid_fails(
        self, mock_send: Mock
    ) -> None:
        with contextlib.redirect_stderr(StringIO()):
            with self.assertRaises(SystemExit):
                await main(["pause", "-t", "1", "-p", "12345"])
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_step_in_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "step-in",
                "1",
                "--single-thread",
                "--target-id",
                "0",
                "--granularity",
                "line",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StepInRequest(
                thread_id=1, single_thread=True, target_id=0, granularity="line"
            )
        )

    @patch("cli.cli.send_command")
    async def test_step_in_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["step-in", "step_in", "stepin", "s"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(StepInRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_step_in_command_single_thread(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["step-in", "1", "--single-thread"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StepInRequest(thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_step_in(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "step-in", "thread_id": 1, "target_id": 0}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StepInRequest(thread_id=1, target_id=0)
        )


if __name__ == "__main__":
    unittest.main()
