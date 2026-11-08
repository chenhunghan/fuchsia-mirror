# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys


def resolve_path(filepath: str) -> str | None:
    """Resolve a file path to an absolute path.

    Paths must be fully qualified from the workspace root (FUCHSIA_DIR), or
    be an absolute path, or exist relative to the current working directory.
    """
    abs_path = os.path.abspath(filepath)
    if os.path.isfile(abs_path):
        return abs_path

    if os.path.isabs(filepath):
        return None

    fuchsia_dir = os.environ.get("FUCHSIA_DIR")
    if not fuchsia_dir:
        return None

    fuchsia_path = os.path.abspath(os.path.join(fuchsia_dir, filepath))
    if os.path.isfile(fuchsia_path):
        return fuchsia_path

    return None


def parse_and_resolve_file_line(file_line: str) -> tuple[str, int] | None:
    """Parses '<file>:<line>' and resolves the file path against the workspace."""
    if ":" not in file_line:
        print(
            f"Error: Invalid format '{file_line}'. Expected '<file>:<line>'.",
            file=sys.stderr,
        )
        return None

    parts = file_line.rsplit(":", 1)
    file_part = parts[0]
    line_part = parts[1]

    try:
        line = int(line_part)
        if line <= 0:
            raise ValueError()
    except ValueError:
        print(
            f"Error: Invalid line number '{line_part}'. Must be a positive integer.",
            file=sys.stderr,
        )
        return None

    resolved_file = resolve_path(file_part)
    if not resolved_file:
        print(
            f"Error: Could not resolve file path '{file_part}'. "
            "Paths must be fully qualified from the workspace root, "
            "an absolute path, or relative to the current working directory. "
            "Ensure FUCHSIA_DIR is set or run from the workspace root.",
            file=sys.stderr,
        )
        return None

    return resolved_file, line
