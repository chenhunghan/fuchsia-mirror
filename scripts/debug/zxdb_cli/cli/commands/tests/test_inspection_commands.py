# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import unittest
from io import StringIO
from unittest.mock import Mock, patch

from cli.cli import main
from cli.tests.helpers import CLITestCase
from shared.protocol.aspace import AspaceRequest
from shared.protocol.async_backtrace import AsyncBacktraceRequest
from shared.protocol.console import ConsoleRequest
from shared.protocol.diagnose_hang import DiagnoseHangRequest
from shared.protocol.driver_list import DriverListRequest
from shared.protocol.evaluate import EvaluateRequest
from shared.protocol.get_state import GetStateRequest
from shared.protocol.handles import HandlesRequest
from shared.protocol.mem_read import MemReadRequest
from shared.protocol.modules import ModulesRequest
from shared.protocol.stack_trace import StackTraceRequest
from shared.protocol.threads import ThreadsRequest
from shared.protocol.variables import VariablesRequest
from shared.protocol.wait_for_event import WaitForEventRequest


class TestInspectionCommands(CLITestCase):
    @patch("cli.cli.send_command")
    async def test_threads_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["threads"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(ThreadsRequest())

    @patch("cli.cli.send_command")
    async def test_json_option_stack_trace(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "stack-trace", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(thread_id=1, raw=False)
        )

    @patch("cli.cli.send_command")
    async def test_stack_trace_command_thread_id(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for cmd in ["stack-trace", "stack_trace", "stackTrace"]:
            mock_send.reset_mock()
            exit_code = await main([cmd, "-t", "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                StackTraceRequest(thread_id=1, raw=False)
            )

        mock_send.reset_mock()
        exit_code = await main(["stack-trace", "--thread-id", "1", "-r"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(thread_id=1, raw=True)
        )

        mock_send.reset_mock()
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
        mock_send.assert_called_once_with(
            StackTraceRequest(
                thread_id=1,
                raw=False,
                force_remote_unwind=True,
                unwinder="fp",
            )
        )

    @patch("cli.cli.send_command")
    async def test_stack_trace_invalid_unwinder(self, mock_send: Mock) -> None:
        with contextlib.redirect_stderr(StringIO()):
            with self.assertRaises(SystemExit):
                await main(["stack-trace", "-t", "1", "--unwinder", "invalid"])
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_stack_trace_command_pid_flag(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["stack-trace", "-p", "12345"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(pid=12345, raw=False)
        )

    @patch("cli.cli.send_command")
    async def test_stack_trace_command_pid_and_raw_flag(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["stack-trace", "--pid", "12345", "-r"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(pid=12345, raw=True)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_stack_trace_pid(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "stack-trace", "pid": 12345}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(pid=12345, raw=False)
        )

    @patch("cli.cli.send_command")
    async def test_stack_trace_missing_args(self, mock_send: Mock) -> None:
        with contextlib.redirect_stderr(StringIO()):
            with self.assertRaises(SystemExit):
                await main(["stack-trace"])
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_variables_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["variables", "--thread-id", "1", "--frame-index", "2"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_variables_command_short_flag(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["variables", "-t", "1", "--frame-index", "2"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_variables_command_default_frame_index(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["variables", "-t", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=0)
        )

    @patch("cli.cli.send_command")
    async def test_locals_alias_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["locals", "-t", "1", "--frame-index", "2"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_variables(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "variables", "thread_id": 1, "frame_index": 2}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_evaluate_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["evaluate", "--thread-id", "1", "x", "+", "y"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1,
                frame_index=0,
                expression="x + y",
                start=0,
                count=50,
            )
        )

    @patch("cli.cli.send_command")
    async def test_evaluate_command_short_flag(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["evaluate", "-t", "1", "x", "+", "y"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1,
                frame_index=0,
                expression="x + y",
                start=0,
                count=50,
            )
        )

    @patch("cli.cli.send_command")
    async def test_print_alias_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["print", "--thread-id", "1", "x", "+", "y"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1,
                frame_index=0,
                expression="x + y",
                start=0,
                count=50,
            )
        )

    @patch("cli.cli.send_command")
    async def test_evaluate_command_options(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "evaluate",
                "--thread-id",
                "1",
                "x",
                "--frame-index",
                "2",
                "--start",
                "5",
                "--count",
                "10",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1, frame_index=2, expression="x", start=5, count=10
            )
        )

    @patch("cli.cli.send_command")
    async def test_json_option_evaluate(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "evaluate", "thread_id": 1, "frame_index": 2, "expression": "x", "start": 5, "count": 10}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1, frame_index=2, expression="x", start=5, count=10
            )
        )

    @patch("cli.cli.send_command")
    async def test_console_command_without_thread_id(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["console", "sym-stat"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ConsoleRequest(
                input="sym-stat",
                thread_id=None,
                frame_index=0,
            )
        )

    @patch("cli.cli.send_command")
    async def test_console_command_with_thread_and_frame(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "console",
                "-t",
                "1",
                "--frame-index",
                "2",
                "disassemble",
                "--num=5",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ConsoleRequest(
                input="disassemble --num=5",
                thread_id=1,
                frame_index=2,
            )
        )

    @patch("cli.cli.send_command")
    async def test_console_command_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["cmd", "console-command", "console_command"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "ps"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                ConsoleRequest(input="ps", thread_id=None, frame_index=0)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_console(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "console", "input": "regs", "thread_id": 1, "frame_index": 2}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ConsoleRequest(input="regs", thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_console_command_missing_input(self, mock_send: Mock) -> None:
        with contextlib.redirect_stderr(StringIO()):
            exit_code = await main(["console"])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

        with contextlib.redirect_stderr(StringIO()):
            exit_code = await main(
                ["--json", '{"command": "console", "input": "   "}']
            )
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

        with contextlib.redirect_stderr(StringIO()):
            exit_code = await main(["console", "--frame-index", "2", "regs"])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_get_state_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for cmd in ["get-state", "get_state", "getState"]:
            mock_send.reset_mock()
            exit_code = await main([cmd])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(GetStateRequest())

    @patch("cli.cli.send_command")
    async def test_json_option_get_state(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["--json", '{"command": "get-state"}'])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(GetStateRequest())

    @patch("cli.cli.send_command")
    async def test_wait_for_event_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for cmd in ["wait-for-event", "wait_for_event", "waitForEvent"]:
            mock_send.reset_mock()
            exit_code = await main([cmd, "--last-seen-seq", "10"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                WaitForEventRequest(last_seen_seq=10, timeout=10)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_wait_for_event(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "wait-for-event", "last_seen_seq": 10}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(WaitForEventRequest(last_seen_seq=10))

    @patch("cli.cli.send_command")
    async def test_async_backtrace_command_default(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["async-backtrace"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AsyncBacktraceRequest(pid=None))

    @patch("cli.cli.send_command")
    async def test_async_backtrace_command_with_pid(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["async-backtrace", "-p", "1234"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AsyncBacktraceRequest(pid=1234))

        mock_send.reset_mock()
        exit_code = await main(["async-backtrace", "--pid", "5678"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AsyncBacktraceRequest(pid=5678))

    @patch("cli.cli.send_command")
    async def test_async_backtrace_command_aliases(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        for alias in [
            "abt",
            "async_backtrace",
            "asyncBacktrace",
            "async-tasks",
            "async_tasks",
            "asyncTasks",
            "tasks",
        ]:
            mock_send.reset_mock()
            exit_code = await main([alias, "-p", "1234"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(AsyncBacktraceRequest(pid=1234))

    @patch("cli.cli.send_command")
    async def test_json_option_async_backtrace(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "async-backtrace", "pid": 1234}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AsyncBacktraceRequest(pid=1234))

    @patch("cli.cli.send_command")
    async def test_process_inspection_commands(self, mock_send: Mock) -> None:
        mock_send.return_value = 0

        exit_code = await main(
            ["modules", "-p", "48291", "-f", "xhci", "--reload"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ModulesRequest(pid=48291, filter="xhci", force_reload=True)
        )

        mock_send.reset_mock()
        exit_code = await main(
            ["aspace", "0x530b010dc000", "-p", "48291", "-f", "mmio"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            AspaceRequest(pid=48291, address="0x530b010dc000", filter="mmio")
        )

        mock_send.reset_mock()
        exit_code = await main(
            ["handles", "-p", "48291", "--type", "interrupt", "--detail"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            HandlesRequest(pid=48291, type="interrupt", detail=True)
        )

        mock_send.reset_mock()
        exit_code = await main(
            ["mem-read", "0x7ffee1234000", "-s", "32", "-p", "48291"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            MemReadRequest(address="0x7ffee1234000", size=32, pid=48291)
        )

        # Non-zero --frame-index without --thread-id is rejected.
        mock_send.reset_mock()
        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await main(
                ["mem-read", "0x7ffee1234000", "--frame-index", "2"]
            )
        self.assertEqual(exit_code, 1)
        self.assertIn(
            "--thread-id is required when specifying a non-zero --frame-index",
            stderr.getvalue(),
        )
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_driver_list_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0

        exit_code = await main(["driver-list"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(DriverListRequest(filter=None))

        mock_send.reset_mock()
        exit_code = await main(["driver-list", "usb"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(DriverListRequest(filter="usb"))

        mock_send.reset_mock()
        exit_code = await main(["driver-hosts", "--filter", "xhci"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(DriverListRequest(filter="xhci"))

        mock_send.reset_mock()
        with patch("sys.stderr", StringIO()):
            exit_code = await main(["driver-list", "usb", "--filter", "xhci"])
            self.assertEqual(exit_code, 1)
            mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_diagnose_hang_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["diagnose-hang", "-p", "48291", "--no-auto-resume"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            DiagnoseHangRequest(pid=48291, auto_resume=False)
        )


if __name__ == "__main__":
    unittest.main()
