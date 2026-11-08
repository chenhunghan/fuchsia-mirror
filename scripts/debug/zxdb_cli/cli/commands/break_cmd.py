# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any

from cli.commands.base import BaseCommand
from cli.commands.location_utils import parse_and_resolve_file_line
from shared.protocol.break_request import COMMAND_NAME

__all__ = ["Command"]


class Command(BaseCommand):
    """CLI command definition for `break`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = [
        "breakpoint",
        "b",
        "setBreakpoints",
        "set-breakpoints",
        "set_breakpoints",
    ]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `break` subparser and arguments."""
        break_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Set or delete a source or function breakpoint",
            description=(
                "Set or delete a breakpoint at a <file>:<line> or --function"
                " <symbol>."
            ),
        )
        break_parser.add_argument(
            "file_line",
            nargs="?",
            default=None,
            type=str,
            help="File and line number (e.g. 'main.cc:23' or 'src/foo.rs:10')",
        )
        break_parser.add_argument(
            "--file",
            dest="file_flag",
            default=None,
            type=str,
            help="Source file path (requires --line)",
        )
        break_parser.add_argument(
            "--line",
            dest="line_flag",
            default=None,
            type=int,
            help="1-based source line number (requires --file)",
        )
        break_parser.add_argument(
            "-f",
            "--function",
            default=None,
            type=str,
            help="Function or method symbol name (e.g. 'usb_xhci::UsbXhci::Init')",
        )
        break_parser.add_argument(
            "-c",
            "--condition",
            default=None,
            type=str,
            help="Optional condition expression for the breakpoint",
        )
        break_parser.add_argument(
            "-d",
            "--delete",
            action="store_true",
            help="Delete the specified breakpoint instead of adding it",
        )

    @staticmethod
    def _parse_and_resolve_file_line(
        file_line: str,
    ) -> tuple[str, int] | None:
        """Parses '<file>:<line>' and resolves the path in the workspace."""
        return parse_and_resolve_file_line(file_line)

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        """Validates and normalizes `break` CLI arguments."""
        # In JSON mode (--json), cli.py invokes execute() with a namespace
        # converted from an already-deserialized BreakRequest (which has 'file'
        # and 'line', but no 'file_line').
        if not hasattr(args, "file_line"):
            return None

        file_flag = getattr(args, "file_flag", None)
        line_flag = getattr(args, "line_flag", None)
        if hasattr(args, "file_flag"):
            delattr(args, "file_flag")
        if hasattr(args, "line_flag"):
            delattr(args, "line_flag")

        if (file_flag is not None) != (line_flag is not None):
            print(
                "Error: Both --file and --line must be specified together.",
                file=sys.stderr,
            )
            return 1

        if file_flag is not None and line_flag is not None:
            if args.file_line is not None or args.function is not None:
                print(
                    "Error: Cannot combine --file/--line with positional '<file>:<line>' or --function.",
                    file=sys.stderr,
                )
                return 1

            parsed = Command._parse_and_resolve_file_line(
                f"{file_flag}:{line_flag}"
            )
            if parsed is None:
                return 1

            args.file, args.line = parsed
            delattr(args, "file_line")
            return None

        if args.function is not None:
            if args.file_line is not None:
                print(
                    "Error: Cannot specify positional '<file>:<line>' together with --function.",
                    file=sys.stderr,
                )
                return 1
            args.file = None
            args.line = None
            delattr(args, "file_line")
            return None

        if args.file_line is None:
            print(
                "Error: Must specify '<file>:<line>', '--file <path> --line <n>', or '--function <symbol>'.",
                file=sys.stderr,
            )
            return 1

        parsed = Command._parse_and_resolve_file_line(args.file_line)
        if parsed is None:
            return 1

        args.file, args.line = parsed
        delattr(args, "file_line")
        return None
