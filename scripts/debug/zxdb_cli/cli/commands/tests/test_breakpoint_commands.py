# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import unittest
from io import StringIO
from unittest.mock import AsyncMock, Mock, patch

from cli.cli import main
from cli.commands.location_utils import resolve_path
from cli.tests.helpers import CLITestCase
from shared.protocol.break_request import BreakRequest
from shared.protocol.tracepoint import TracepointRequest
from shared.protocol.watch_request import WatchRequest


class TestBreakpointCommands(CLITestCase):
    @patch("cli.commands.location_utils.resolve_path", autospec=True)
    @patch("cli.cli.send_command")
    async def test_break_command(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        exit_code = await main(["break", "src/main.rs:12"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_called_once_with("src/main.rs")
        mock_send.assert_called_once_with(
            BreakRequest(file="/path/to/fuchsia/src/main.rs", line=12)
        )

    @patch("cli.commands.location_utils.resolve_path", autospec=True)
    @patch("cli.cli.send_command")
    async def test_break_command_aliases(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        for alias in [
            "breakpoint",
            "b",
            "setBreakpoints",
            "set-breakpoints",
            "set_breakpoints",
        ]:
            mock_send.reset_mock()
            mock_resolve.reset_mock()
            exit_code = await main([alias, "src/main.rs:12"])
            self.assertEqual(exit_code, 0, f"Failed for alias: {alias}")
            mock_resolve.assert_called_once_with("src/main.rs")
            mock_send.assert_called_once_with(
                BreakRequest(file="/path/to/fuchsia/src/main.rs", line=12)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_break(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "break", "file": "/path/to/file.rs", "line": 12}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            BreakRequest(file="/path/to/file.rs", line=12)
        )

    @patch("cli.commands.location_utils.resolve_path", autospec=True)
    async def test_break_command_invalid_format(
        self, mock_resolve: Mock
    ) -> None:
        # Rejects target missing colon delimiter.
        exit_code = await main(["break", "src/main.rs"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # Rejects non-integer line number suffix.
        exit_code = await main(["break", "src/main.rs:abc"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # Rejects negative line number.
        exit_code = await main(["break", "src/main.rs:-5"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # Fails when path cannot be resolved in source tree.
        mock_resolve.return_value = None
        exit_code = await main(["break", "src/missing.rs:10"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_called_once_with("src/missing.rs")

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_fuchsia_dir(self, mock_isfile: Mock) -> None:
        mock_isfile.side_effect = lambda p: p == "/workspace/fuchsia/src/foo.rs"
        res = resolve_path("src/foo.rs")
        self.assertEqual(res, "/workspace/fuchsia/src/foo.rs")

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_directory(self, mock_isfile: Mock) -> None:
        mock_isfile.return_value = False
        res = resolve_path("src")
        self.assertIsNone(res)

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_unresolved(self, mock_isfile: Mock) -> None:
        mock_isfile.return_value = False
        res = resolve_path("missing/foo.rs")
        self.assertIsNone(res)

    @patch("cli.commands.location_utils.resolve_path", autospec=True)
    @patch("cli.cli.send_command")
    async def test_break_command_delete(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        exit_code = await main(["break", "-d", "src/main.rs:12"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_called_once_with("src/main.rs")
        mock_send.assert_called_once_with(
            BreakRequest(
                file="/path/to/fuchsia/src/main.rs", line=12, delete=True
            )
        )

    @patch("cli.cli.send_command")
    async def test_watch_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "watch",
                "-t",
                "1",
                "--frame-index",
                "2",
                "-a",
                "readWrite",
                "my_var",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            WatchRequest(
                name="my_var",
                thread_id=1,
                frame_index=2,
                access_type="readWrite",
            )
        )

    @patch("cli.cli.send_command")
    async def test_watch_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in [
            "watchpoint",
            "data-breakpoint",
            "setDataBreakpoints",
            "set-data-breakpoints",
            "set_data_breakpoints",
        ]:
            mock_send.reset_mock()
            exit_code = await main([alias, "-t", "1", "-a", "write", "my_var"])
            self.assertEqual(exit_code, 0, f"Failed for alias: {alias}")
            mock_send.assert_called_once_with(
                WatchRequest(name="my_var", thread_id=1, access_type="write")
            )

    @patch("cli.cli.send_command")
    async def test_watch_delete(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["watch", "-d", "1001:0x1234:4"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            WatchRequest(
                name="1001:0x1234:4",
                delete=True,
            )
        )

    @patch("cli.cli.send_command")
    async def test_watch_clear(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["watch", "--clear"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(WatchRequest(name="", clear=True))

    @patch("cli.cli.send_command")
    async def test_watch_invalid_args(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        cases = [
            (["watch"], "expression is required"),
            (["watch", "-d"], "data_id is required"),
            (["watch", "-a", "write", "my_var"], "-t/--thread-id"),
            (["watch", "-t", "1", "my_var"], "-a/--access-type"),
        ]
        for argv, expected in cases:
            mock_send.reset_mock()
            stderr = StringIO()
            with contextlib.redirect_stderr(stderr):
                exit_code = await main(argv)
            self.assertEqual(exit_code, 2, f"Failed for: {argv}")
            self.assertIn(expected, stderr.getvalue())
            mock_send.assert_not_called()

    async def test_watch_rejects_read_access_type(self) -> None:
        with contextlib.redirect_stderr(StringIO()):
            with self.assertRaises(SystemExit):
                await main(["watch", "-t", "1", "-a", "read", "my_var"])

    @patch("cli.cli.send_command")
    async def test_json_option_watch(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "watch", "name": "my_var", "thread_id": 1,'
                ' "frame_index": 2, "access_type": "write"}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            WatchRequest(
                name="my_var",
                thread_id=1,
                frame_index=2,
                access_type="write",
            )
        )

    @patch("cli.cli.send_command")
    async def test_json_option_watch_delete_and_clear(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "watch", "name": "1001:0x1234:4", "delete": true}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            WatchRequest(name="1001:0x1234:4", delete=True)
        )

        mock_send.reset_mock()
        exit_code = await main(
            ["--json", '{"command": "watch", "clear": true}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(WatchRequest(clear=True))

    @patch("cli.cli.send_command")
    async def test_json_option_watch_invalid(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        cases = [
            ('{"command": "watch", "name": "my_var", "thread_id": 1}', 2),
            (
                '{"command": "watch", "name": "my_var", "access_type": "write"}',
                2,
            ),
            (
                '{"command": "watch", "name": "my_var", "thread_id": 1,'
                ' "access_type": "read"}',
                1,
            ),
        ]
        for payload, expected_code in cases:
            mock_send.reset_mock()
            with contextlib.redirect_stderr(StringIO()):
                exit_code = await main(["--json", payload])
            self.assertEqual(exit_code, expected_code, f"Failed for: {payload}")
            mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_extended_break_commands(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["break", "-f", "usb_xhci::UsbXhci::Init", "-c", "status != 0"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            BreakRequest(
                function="usb_xhci::UsbXhci::Init", condition="status != 0"
            )
        )

    @patch("cli.cli.send_command")
    async def test_tracepoint_function_command(
        self, mock_send: AsyncMock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "tracepoint",
                "my_func",
                "-e",
                "x",
                "--locals",
                "--max-hits",
                "10",
                "--max-frames",
                "5",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertEqual(req.function, "my_func")
        self.assertEqual(req.expressions, ["x"])
        self.assertIn("locals", req.capture)
        self.assertEqual(req.max_hits, 10)
        self.assertEqual(req.max_matching_frames, 5)

    @patch("cli.cli.send_command")
    async def test_tracepoint_list_command(self, mock_send: AsyncMock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["tracepoint", "--list"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertTrue(req.list_all)

    @patch("cli.cli.send_command")
    async def test_tracepoint_delete_command(
        self, mock_send: AsyncMock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["tracepoint", "-d", "--id", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertTrue(req.delete)
        self.assertEqual(req.tracepoint_id, 1)

    @patch("cli.cli.send_command")
    async def test_tracepoint_raw_backtrace_implies_backtrace(
        self, mock_send: AsyncMock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["tracepoint", "my_func", "-r"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertTrue(req.raw_backtrace)
        self.assertIn("backtrace", req.capture)

    @patch("cli.cli.send_command")
    async def test_tracepoint_expr_flag(self, mock_send: AsyncMock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["tracepoint", "my_func", "--expr", "x"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertEqual(req.expressions, ["x"])

    @patch("cli.cli.send_command")
    async def test_tracepoint_delete_positional_id(
        self, mock_send: AsyncMock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["tracepoint", "1", "-d"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once()
        req = mock_send.call_args[0][0]
        self.assertIsInstance(req, TracepointRequest)
        self.assertTrue(req.delete)
        self.assertEqual(req.tracepoint_id, 1)

    @patch("cli.commands.location_utils.resolve_path", autospec=True)
    @patch("cli.cli.send_command")
    async def test_tracepoint_file_line_and_cpp_symbol(
        self, mock_send: AsyncMock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/workspace/src/main.cc"
        exit_code = await main(["tp", "src/main.cc:42"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_called_once_with("src/main.cc")
        self.assertEqual(
            mock_send.call_args[0][0],
            TracepointRequest(file="/workspace/src/main.cc", line=42),
        )

        mock_send.reset_mock()
        mock_resolve.reset_mock()
        exit_code = await main(["trace", "foo::Bar::Baz"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_not_called()
        self.assertEqual(
            mock_send.call_args[0][0],
            TracepointRequest(function="foo::Bar::Baz"),
        )

    @patch("cli.cli.send_command")
    async def test_tracepoint_watch_and_management_commands(
        self, mock_send: AsyncMock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["tp", "-w", "my_var", "-t", "2", "-a", "readWrite", "--size", "4"]
        )
        self.assertEqual(exit_code, 0)
        self.assertEqual(
            mock_send.call_args[0][0],
            TracepointRequest(
                watch="my_var", thread_id=2, access_type="readWrite", size=4
            ),
        )

        for argv, expected in (
            (["tp", "--clear"], TracepointRequest(clear=True)),
            (
                ["tp", "--enable", "3"],
                TracepointRequest(tracepoint_id=3, enabled=True),
            ),
            (
                ["tp", "--disable", "--id", "4"],
                TracepointRequest(tracepoint_id=4, enabled=False),
            ),
        ):
            mock_send.reset_mock()
            exit_code = await main(argv)
            self.assertEqual(exit_code, 0)
            self.assertEqual(mock_send.call_args[0][0], expected)

    @patch("cli.cli.send_command")
    async def test_tracepoint_conflicting_cli_flags(
        self, mock_send: AsyncMock
    ) -> None:
        invalid_cases = [
            ["tracepoint", "--list", "--delete"],
            ["tracepoint", "--list", "my_func"],
            ["tracepoint", "--clear", "--locals"],
            ["tracepoint", "-d", "--id", "1", "other_func"],
            ["tracepoint", "-d"],
            ["tracepoint", "--enable", "--disable", "--id", "1"],
            ["tracepoint", "--enable"],
            ["tracepoint", "--disable", "1", "--locals"],
        ]
        for argv in invalid_cases:
            mock_send.reset_mock()
            with contextlib.redirect_stderr(StringIO()):
                exit_code = await main(argv)
            self.assertEqual(exit_code, 1, f"Expected failure for {argv}")
            mock_send.assert_not_called()


if __name__ == "__main__":
    unittest.main()
