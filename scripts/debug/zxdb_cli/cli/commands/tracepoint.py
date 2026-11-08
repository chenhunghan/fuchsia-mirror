# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any, get_args

from cli.commands.base import BaseCommand
from cli.commands.location_utils import parse_and_resolve_file_line
from shared.protocol.tracepoint import COMMAND_NAME, CaptureItem
from shared.protocol.watch_request import WatchAccessType


def _has_cli_capture_or_watch_flags(args: argparse.Namespace) -> bool:
    """Returns True if any capture, watch, or threshold CLI flag is non-default."""
    return (
        getattr(args, "collect_locals", False)
        or getattr(args, "collect_registers", False)
        or getattr(args, "collect_backtrace", False)
        or getattr(args, "raw_backtrace", False)
        or args.condition is not None
        or bool(args.expressions)
        or args.size is not None
        or args.access_type != "write"
        or args.thread_id is not None
        or args.frame_index != 0
        or args.max_matching_frames != 10
        or args.max_hits != 20
    )


def _pop_capture_flags(args: argparse.Namespace) -> list[CaptureItem]:
    """Converts CLI `--locals`/`--registers`/`--backtrace` flags into `capture`."""
    capture: list[CaptureItem] = ["frame"]
    if getattr(args, "collect_backtrace", False) or getattr(
        args, "raw_backtrace", False
    ):
        capture.append("backtrace")
    if getattr(args, "collect_locals", False):
        capture.append("locals")
    if getattr(args, "collect_registers", False):
        capture.append("registers")

    for attr in (
        "collect_locals",
        "collect_registers",
        "collect_backtrace",
    ):
        if hasattr(args, attr):
            delattr(args, attr)

    return capture


def _apply_explicit_file_line(
    args: argparse.Namespace,
    target: str | None,
    file_flag: str,
    line_flag: int,
) -> int | None:
    """Validates and resolves explicit `--file` and `--line` CLI flags."""
    if (
        target is not None
        or args.function is not None
        or args.watch is not None
    ):
        print(
            "Error: Cannot combine --file/--line with positional target,"
            " --function, or --watch.",
            file=sys.stderr,
        )
        return 1
    parsed = parse_and_resolve_file_line(f"{file_flag}:{line_flag}")
    if parsed is None:
        return 1
    args.file, args.line = parsed
    return None


def _apply_target_flags(
    args: argparse.Namespace,
    target: str | None,
    file_flag: str | None,
    line_flag: int | None,
) -> int | None:
    """Resolves positional and flag-based target specs into args.file/line/function."""
    if (file_flag is not None) != (line_flag is not None):
        print(
            "Error: Both --file and --line must be specified together.",
            file=sys.stderr,
        )
        return 1

    if file_flag is not None and line_flag is not None:
        return _apply_explicit_file_line(args, target, file_flag, line_flag)

    if args.function is not None or args.watch is not None:
        if target is not None or (
            args.function is not None and args.watch is not None
        ):
            print(
                "Error: Cannot specify multiple targets (positional target,"
                " --function, or --watch).",
                file=sys.stderr,
            )
            return 1
        return None

    if target is None:
        print(
            "Error: Must specify target '<file>:<line>', '--file <path> --line"
            " <n>', function symbol, '--watch <expr>', '--list', or '--clear'.",
            file=sys.stderr,
        )
        return 1

    if ":" in target and "::" not in target:
        parsed = parse_and_resolve_file_line(target)
        if parsed is None:
            return 1
        args.file, args.line = parsed
    else:
        args.function = target
    return None


def _validate_management_cli_flags(
    args: argparse.Namespace,
    target: str | None,
    file_flag: str | None,
    line_flag: int | None,
    has_extra_opts: bool,
) -> tuple[bool, int | None]:
    """Validates `--list`, `--clear`, `--enable`/`--disable`, and `--delete` flags."""
    has_target = any(
        val is not None
        for val in (target, file_flag, line_flag, args.function, args.watch)
    )
    mode_count = sum(
        [args.list_all, args.clear, args.delete, args.enabled is not None]
    )
    if mode_count > 1:
        print(
            "Error: Cannot combine --list, --clear, --delete, --enable, or"
            " --disable.",
            file=sys.stderr,
        )
        return True, 1

    if args.list_all or args.clear:
        if args.tracepoint_id is not None or has_target or has_extra_opts:
            print(
                "Error: --list and --clear cannot be combined with --id,"
                " target, or capture options.",
                file=sys.stderr,
            )
            return True, 1
        return True, None

    if args.enabled is not None:
        if args.tracepoint_id is None:
            print(
                "Error: --id (or positional ID) is required with"
                " --enable/--disable.",
                file=sys.stderr,
            )
            return True, 1
        if has_target or has_extra_opts:
            print(
                "Error: --enable/--disable cannot be combined with target or"
                " capture options.",
                file=sys.stderr,
            )
            return True, 1
        return True, None

    if args.delete:
        if has_extra_opts:
            print(
                "Error: --delete cannot be combined with capture or watch"
                " options.",
                file=sys.stderr,
            )
            return True, 1
        if args.tracepoint_id is not None:
            if has_target:
                print(
                    "Error: Cannot combine --id with a target when deleting.",
                    file=sys.stderr,
                )
                return True, 1
            return True, None

    return False, None


def _add_target_arguments(tp_parser: argparse.ArgumentParser) -> None:
    """Registers target and watchpoint arguments on `tp_parser`."""
    tp_parser.add_argument(
        "target",
        nargs="?",
        default=None,
        type=str,
        help="Target '<file>:<line>' or function symbol (e.g. 'foo::Bar::Baz')",
    )
    tp_parser.add_argument(
        "-f",
        "--function",
        default=None,
        type=str,
        help="Explicit function symbol name",
    )
    tp_parser.add_argument(
        "--file",
        dest="file_flag",
        default=None,
        type=str,
        help="Source file path (requires --line)",
    )
    tp_parser.add_argument(
        "--line",
        dest="line_flag",
        default=None,
        type=int,
        help="1-based source line number (requires --file)",
    )
    tp_parser.add_argument(
        "-w",
        "--watch",
        default=None,
        type=str,
        help="Expression or hex address for a hardware watchpoint tracepoint",
    )
    tp_parser.add_argument(
        "--size",
        default=None,
        type=int,
        help="Byte size for hardware watchpoint (e.g. 1, 2, 4, 8)",
    )
    tp_parser.add_argument(
        "-a",
        "--access-type",
        default="write",
        choices=list(get_args(WatchAccessType)),
        help="Access type for hardware watchpoint (default: write)",
    )
    tp_parser.add_argument(
        "-t",
        "--thread-id",
        default=None,
        type=int,
        help="Thread ID for evaluating watchpoint expression",
    )
    tp_parser.add_argument(
        "--frame-index",
        default=0,
        type=int,
        help="Stack frame index for watchpoint expression (default: 0)",
    )


def _add_capture_arguments(tp_parser: argparse.ArgumentParser) -> None:
    """Registers condition, capture, and hit-budget arguments on `tp_parser`."""
    tp_parser.add_argument(
        "-c",
        "--condition",
        default=None,
        type=str,
        help="Condition expression required for tracepoint to trigger",
    )
    tp_parser.add_argument(
        "-e",
        "--expr",
        "--expression",
        dest="expressions",
        action="append",
        default=[],
        help="Expression to evaluate on hit (may be passed multiple times)",
    )
    tp_parser.add_argument(
        "--locals",
        dest="collect_locals",
        action="store_true",
        default=False,
        help="Capture local variables and arguments on hit",
    )
    tp_parser.add_argument(
        "--registers",
        dest="collect_registers",
        action="store_true",
        default=False,
        help="Capture CPU registers on hit",
    )
    tp_parser.add_argument(
        "--backtrace",
        dest="collect_backtrace",
        action="store_true",
        default=False,
        help="Capture multi-frame backtrace on hit",
    )
    tp_parser.add_argument(
        "-r",
        "--raw-backtrace",
        action="store_true",
        default=False,
        help="Do not filter or elide stack frames when capturing backtrace",
    )
    tp_parser.add_argument(
        "--max-frames",
        "--max-matching-frames",
        dest="max_matching_frames",
        default=10,
        type=int,
        help="Maximum matching stack frames to capture (default: 10)",
    )
    tp_parser.add_argument(
        "--max-hits",
        default=20,
        type=int,
        help="Auto-disable tracepoint after this many hits (default: 20)",
    )


def _add_management_arguments(tp_parser: argparse.ArgumentParser) -> None:
    """Registers delete, clear, enable, disable, ID, and list flags."""
    tp_parser.add_argument(
        "-d",
        "--delete",
        action="store_true",
        default=False,
        help="Delete the matching tracepoint",
    )
    tp_parser.add_argument(
        "--clear",
        action="store_true",
        default=False,
        help="Clear all active tracepoints",
    )
    tp_parser.add_argument(
        "--enable",
        dest="enable_flag",
        action="store_true",
        default=False,
        help="Enable tracepoint matching --id",
    )
    tp_parser.add_argument(
        "--disable",
        dest="disable_flag",
        action="store_true",
        default=False,
        help="Disable tracepoint matching --id",
    )
    tp_parser.add_argument(
        "--id",
        dest="tracepoint_id",
        default=None,
        type=int,
        help="Tracepoint ID to update, delete, enable, or disable",
    )
    tp_parser.add_argument(
        "--list",
        dest="list_all",
        action="store_true",
        default=False,
        help="List all configured tracepoints",
    )


class Command(BaseCommand):
    """CLI command definition for `tracepoint`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["tp", "trace"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `tracepoint` subparser and arguments."""
        tp_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Set, manage, delete, or list non-stopping tracepoints",
            description=(
                "Configure non-stopping tracepoints at a <file>:<line>,"
                " function symbol, or watchpoint. When hit, the backend"
                " captures requested state and immediately resumes the thread."
            ),
        )
        _add_target_arguments(tp_parser)
        _add_capture_arguments(tp_parser)
        _add_management_arguments(tp_parser)

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        """Validates and normalizes `tracepoint` CLI arguments."""
        if not hasattr(args, "target") and not hasattr(args, "collect_locals"):
            return None

        enable_flag = getattr(args, "enable_flag", False)
        disable_flag = getattr(args, "disable_flag", False)
        for attr in ("enable_flag", "disable_flag"):
            if hasattr(args, attr):
                delattr(args, attr)
        if enable_flag and disable_flag:
            print(
                "Error: Cannot specify both --enable and --disable.",
                file=sys.stderr,
            )
            return 1
        args.enabled = (
            True if enable_flag else (False if disable_flag else None)
        )

        has_extra_opts = _has_cli_capture_or_watch_flags(args)
        args.capture = _pop_capture_flags(args)
        target = getattr(args, "target", None)
        file_flag = getattr(args, "file_flag", None)
        line_flag = getattr(args, "line_flag", None)
        for attr in ("target", "file_flag", "line_flag"):
            if hasattr(args, attr):
                delattr(args, attr)
        args.file = None
        args.line = None

        if (
            (args.delete or args.enabled is not None)
            and args.tracepoint_id is None
            and target is not None
            and target.isdigit()
        ):
            args.tracepoint_id = int(target)
            target = None

        handled, err_code = _validate_management_cli_flags(
            args, target, file_flag, line_flag, has_extra_opts
        )
        if handled:
            return err_code

        return _apply_target_flags(args, target, file_flag, line_flag)
