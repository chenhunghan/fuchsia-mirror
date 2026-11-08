#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Hermetic Bazel wrapper for running Pyrefly static type checking on a target."""

import argparse
import json
import os
import pathlib
import subprocess
import sys
from collections.abc import Sequence


class _PyreflyOutputError(Exception):
    """Pyrefly's output is not a JSON error report."""


def _format_pyrefly_errors(output: str) -> str | None:
    """Formats the type errors in Pyrefly's JSON output.

    Args:
        output: Pyrefly's stdout (`--output-format json`).

    Returns:
        Formatted error lines, or None if Pyrefly reported no errors.

    Raises:
        _PyreflyOutputError: `output` is not a JSON error report (for example,
            because Pyrefly failed due to a CLI or config error).
    """
    cwd = os.path.abspath(os.getcwd()) + os.sep
    try:
        lines = [
            f"{err['severity'].upper()} {err['path'].removeprefix(cwd)}:"
            f"{err['line']}:{err['column']}: {err['description']} [{err['name']}]"
            for err in json.loads(output)["errors"]
        ]
    except (ValueError, KeyError, TypeError, AttributeError) as e:
        raise _PyreflyOutputError(f"Unexpected Pyrefly output: {e!r}") from e
    return "\n".join(lines) if lines else None


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pyrefly",
        type=pathlib.Path,
        required=True,
        help="Path to the pyrefly binary.",
    )
    parser.add_argument(
        "--config",
        type=pathlib.Path,
        required=True,
        help="Path to pyproject.toml containing [tool.pyrefly].",
    )
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        required=True,
        help="Stamp file to write when type checking succeeds.",
    )
    parser.add_argument(
        "--search-path",
        action="append",
        default=[],
        dest="search_paths",
        help="Import search path (may be passed multiple times).",
    )
    parser.add_argument(
        "sources",
        nargs="*",
        help="Target-owned .py/.pyi source files to type check.",
    )
    args = parser.parse_args(argv)

    files_to_check = [s for s in args.sources if s.endswith((".py", ".pyi"))]
    if not files_to_check:
        args.output.write_text("ok\n")
        return 0

    deduped_search_paths = []
    seen = set()
    for sp in args.search_paths:
        norm = os.path.normpath(sp)
        if norm not in seen and os.path.isdir(norm):
            seen.add(norm)
            deduped_search_paths.append(norm)

    cmd = [
        str(args.pyrefly),
        "check",
        "--config",
        str(args.config),
    ]
    for sp in deduped_search_paths:
        cmd.extend(["--search-path", sp])
    cmd.extend(
        [
            "--disable-search-path-heuristics",
            "--skip-interpreter-query",
            "--use-ignore-files=false",
            "--output-format",
            "json",
            "--summary=none",
        ]
    )
    cmd.extend(files_to_check)

    # Strip ambient PYTHONPATH so host packages never leak into type checking.
    env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
    result = subprocess.run(cmd, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        stdout = result.stdout.strip()
        stderr = result.stderr.strip()
        try:
            type_errors = _format_pyrefly_errors(stdout)
        except _PyreflyOutputError:
            type_errors = None
        if type_errors:
            message = f"Please fix the following Pyrefly errors:\n{type_errors}"
            if stderr:
                message += f"\n\nPyrefly also reported:\n{stderr}"
        else:
            combined = "\n".join(o for o in (stdout, stderr) if o)
            message = (
                "Error occurred while running Pyrefly "
                f"(exit code {result.returncode}):\n{combined}"
            )
        print(f"\n{message}\n", file=sys.stderr)
        return result.returncode

    args.output.write_text("ok\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
