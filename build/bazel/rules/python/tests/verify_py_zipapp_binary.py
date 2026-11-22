#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verifies that bazel_py_host_tool() brings a py_zipapp_binary() into Ninja."""

import argparse
import json
import os
import pathlib
import subprocess
import sys

_SHEBANG = b"#!/usr/bin/env fuchsia-vendored-python\n"


def _verify(pyz: pathlib.Path, expected_output: str) -> str | None:
    """Returns an error message if `pyz` isn't the expected archive."""
    if not os.access(pyz, os.X_OK):
        return f"{pyz} is not executable"
    with pyz.open("rb") as f:
        shebang = f.readline()
    if shebang != _SHEBANG:
        return f"{pyz} starts with {shebang!r}, expected {_SHEBANG!r}"

    # Run the archive with this script's interpreter rather than through its
    # shebang, which relies on `fuchsia-vendored-python` being in PATH.
    result = subprocess.run(
        (sys.executable, str(pyz)), capture_output=True, text=True
    )
    if result.returncode != 0:
        return f"{pyz} exited with {result.returncode}:\n{result.stderr}"
    if result.stdout != expected_output:
        return f"{pyz} printed {result.stdout!r}, expected {expected_output!r}"
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pyz",
        type=pathlib.Path,
        action="append",
        required=True,
        help="Path of a copy of the archive.",
    )
    parser.add_argument(
        "--output-file",
        type=pathlib.Path,
        action="append",
        required=True,
        help="File written by a python_action() running the archive.",
    )
    parser.add_argument(
        "--tool-paths",
        type=pathlib.Path,
        required=True,
        help="JSON list of the host tool's `tool_paths` metadata.",
    )
    parser.add_argument("--cpu", required=True, help="GN `current_cpu`.")
    parser.add_argument("--os", required=True, help="GN `current_os`.")
    parser.add_argument(
        "--expected-tool-path",
        required=True,
        help="Expected `tool_paths` entry, as NAME=PATH=LABEL.",
    )
    parser.add_argument(
        "--expected-output",
        required=True,
        help="Expected output line, without the trailing newline.",
    )
    parser.add_argument("--stamp", type=pathlib.Path, required=True)
    args = parser.parse_args()

    expected_output = args.expected_output + "\n"
    errors = [
        error
        for pyz in args.pyz
        if (error := _verify(pyz, expected_output)) is not None
    ]
    for output_file in args.output_file:
        output = output_file.read_text()
        if output != expected_output:
            errors.append(
                f"{output_file} contains {output!r}, expected "
                f"{expected_output!r}"
            )
    name, path, label = args.expected_tool_path.split("=", 2)
    expected_tool_paths = [
        {
            "cpu": args.cpu,
            "label": label,
            "name": name,
            "os": args.os,
            "path": path,
        }
    ]
    tool_paths = json.loads(args.tool_paths.read_text())
    if tool_paths != expected_tool_paths:
        errors.append(
            f"{args.tool_paths} contains {tool_paths}, expected "
            f"{expected_tool_paths}"
        )

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    args.stamp.write_text("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
