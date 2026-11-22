# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Errors raised by end-to-end test libraries."""

from collections.abc import Mapping


class TestError(Exception):
    """An unexpected error in an end-to-end test library.

    Attributes:
        details: Description of the error.
        extras: Optional extra information about the error, such as command
            output.
    """

    def __init__(
        self, details: str, extras: Mapping[str, object] | None = None
    ) -> None:
        super().__init__(details)
        self.details = details
        self.extras = extras

    def __str__(self) -> str:
        if self.extras is None:
            return self.details
        return f"{self.details} (extras: {self.extras})"
