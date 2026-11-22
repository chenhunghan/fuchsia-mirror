# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Transport-agnostic OpenThread CLI response parsers."""

from __future__ import annotations

import re

from honeydew.affordances.connectivity.thread.errors import (
    HoneydewThreadError,
    ThreadCommandError,
)
from honeydew.affordances.connectivity.thread.types import ThreadRole

_ERROR_LINE_RE = re.compile(r"^Error\s+\d+\s*:.*|^Error:.*")


def parse_response(command: str, raw_output: str) -> list[str]:
    """Validates an OpenThread CLI response, strips prompts/Done, and returns lines."""
    normalized = raw_output.replace("\r\n", "\n").replace("\r", "\n")
    raw_lines = [line.rstrip() for line in normalized.split("\n")]

    cleaned_lines: list[str] = []
    cmd_stripped = command.strip()

    for idx, line in enumerate(raw_lines):
        stripped = line.strip()
        if not stripped or stripped == ">":
            continue

        if idx == 0 and stripped.startswith(">"):
            after_prompt = stripped.lstrip(">").strip()
            if not after_prompt or after_prompt == cmd_stripped:
                continue
            line = after_prompt
            stripped = after_prompt
        elif idx == 0 and stripped == cmd_stripped:
            continue

        if _ERROR_LINE_RE.match(stripped):
            raise ThreadCommandError(command, stripped)

        if stripped == "Done":
            break

        cleaned_lines.append(line)

    return cleaned_lines


def parse_role(command: str, raw_output: str) -> ThreadRole:
    """Parses `state` output into a `ThreadRole`."""
    lines = parse_response(command, raw_output)
    if not lines:
        raise HoneydewThreadError(
            f"Empty response for '{command}': {raw_output!r}"
        )
    role_str = lines[-1].strip().lower()
    try:
        return ThreadRole(role_str)
    except ValueError as err:
        raise HoneydewThreadError(
            f"Unknown Thread role '{role_str}' from '{command}'"
        ) from err
