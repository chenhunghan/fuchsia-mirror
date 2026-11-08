# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.diagnose_hang import COMMAND_NAME


class Command(BaseCommand):
    """CLI command to capture a hang/concurrency diagnostic snapshot of a process."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["diagnose_hang", "diagnoseHang", "hang", "hang-analyze"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `diagnose-hang` subcommand and its arguments."""
        hang_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help=(
                "Capture a hang/concurrency diagnostic snapshot of a process"
                " (handles are sampled before a short pause to classify"
                " thread stacks and async tasks)"
            ),
        )
        hang_parser.add_argument(
            "-p",
            "--pid",
            default=None,
            type=int,
            help="Target process KOID (defaults to attached process)",
        )
        hang_parser.add_argument(
            "--auto-resume",
            dest="auto_resume",
            action=argparse.BooleanOptionalAction,
            default=True,
            help=(
                "Automatically resume threads paused during snapshot"
                " (default: true)"
            ),
        )
