#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verifies that bazel_tool_paths() copies binaries and emits tool_paths metadata."""

import argparse
import json
import os
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--cpu", required=True)
    parser.add_argument("--os", required=True)
    parser.add_argument(
        "--expected-tool",
        action="append",
        default=[],
        help="Expected tool spec in NAME=REL_PATH=GN_LABEL format.",
    )
    parser.add_argument("--stamp", type=Path, required=True)
    args = parser.parse_args()

    entries = json.loads(args.manifest.read_text())
    entries_by_name = {entry["name"]: entry for entry in entries}

    for spec in args.expected_tool:
        name, rel_path, expected_label = spec.split("=", 2)
        if name not in entries_by_name:
            print(
                f"ERROR: Missing tool_paths entry for {name!r} in {entries}",
                file=sys.stderr,
            )
            return 1
        actual = entries_by_name[name]
        expected = {
            "cpu": args.cpu,
            "label": expected_label,
            "name": name,
            "os": args.os,
            "path": rel_path,
        }
        if actual != expected:
            print(
                f"ERROR: Mismatched tool_paths entry for {name!r}:\n"
                f"  actual:   {actual}\n"
                f"  expected: {expected}",
                file=sys.stderr,
            )
            return 1

        bin_path = Path(rel_path)
        if not bin_path.is_file() or bin_path.stat().st_size == 0:
            print(
                f"ERROR: Expected non-empty binary at {bin_path}",
                file=sys.stderr,
            )
            return 1
        if not os.access(bin_path, os.X_OK):
            print(
                f"ERROR: Expected executable binary at {bin_path}",
                file=sys.stderr,
            )
            return 1

    args.stamp.parent.mkdir(parents=True, exist_ok=True)
    args.stamp.write_text("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
