# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Errors raised by Thread affordance and OpenThread CLI helpers."""

from __future__ import annotations

from honeydew import errors


class HoneydewThreadError(errors.HoneydewError):
    """Base error raised by Thread affordance."""


class ThreadDeviceNotFoundError(HoneydewThreadError):
    """Raised when the LoWPAN/Thread service or interface cannot be found."""


class ThreadCommandError(HoneydewThreadError):
    """Raised when an OpenThread CLI command returns an Error response."""

    def __init__(self, command: str, error_line: str) -> None:
        super().__init__(
            f"OpenThread CLI command '{command}' failed: {error_line}"
        )
        self.command = command
        self.error_line = error_line
