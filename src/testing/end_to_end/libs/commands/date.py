# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import time

from libs.commands.command import LinuxCommand
from libs.proc.runner import Runner


class LinuxDateCommand(LinuxCommand):
    """Manage and synchronize system date and time on a Linux device."""

    def __init__(self, runner: Runner, binary: str = "date") -> None:
        super().__init__(runner, binary)

    def sync(self) -> None:
        """Synchronize system time.

        Allows for better synchronization between host logs and device
        logs. Useful for when the device does not have an internet connection.
        """
        # Use Unix timestamp (@<epoch>) rather than an ISO timestamp string.
        # Both GNU date (Raspberry Pi) and BusyBox date (OpenWrt) natively
        # support '@<epoch>' for timezone-independent date synchronization.
        now = f"@{int(time.time())}"
        self._run(["-s", now], sudo=True)
