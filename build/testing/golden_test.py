#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Compares generated files against checked-in golden files.

This is the test binary behind the golden_test() Bazel macro in
//build/bazel/rules/testing:golden_test.bzl. Run with `--update` (via `fx bazel run`)
to overwrite stale goldens with the generated files.
"""

import argparse
import difflib
import filecmp
import os
import shutil
import sys
from pathlib import Path

MAX_DIFF_LINES = 50


def source_golden(golden_source_path: str) -> Path | None:
    """Returns the checked-in golden to update, or None outside `bazel run`.

    `bazel run` sets BUILD_WORKSPACE_DIRECTORY to the build's Bazel
    workspace, whose top-level entries link to the checkout that the build
    belongs to, so this updates that checkout even in a worktree.
    """
    workspace = os.environ.get("BUILD_WORKSPACE_DIRECTORY")
    if not workspace:
        return None
    return Path(workspace) / golden_source_path


def read_lines(path: Path) -> list[str]:
    return path.read_text(encoding="utf-8", errors="replace").splitlines(
        keepends=True
    )


def invisible_difference(golden: bytes, candidate: bytes) -> str | None:
    """Describes a mismatch that a line diff wouldn't show, if there is one.

    The text diff normalizes line endings and replaces undecodable bytes, and
    difflib doesn't mark a missing trailing newline, so these mismatches would
    otherwise print an empty or confusing diff.
    """
    if golden.replace(b"\r\n", b"\n") == candidate.replace(b"\r\n", b"\n"):
        return "The files differ only in line endings."
    if golden.rstrip(b"\r\n") == candidate.rstrip(b"\r\n"):
        return "The files differ only in trailing newlines."
    if golden.decode("utf-8", errors="replace") == candidate.decode(
        "utf-8", errors="replace"
    ):
        return "The files differ only in bytes that aren't valid UTF-8."
    return None


def check(
    candidate: Path, golden: Path, golden_source_path: str, update: bool
) -> bool:
    """Checks one golden, or updates it if `update` is set.

    Returns whether the golden matches, or was updated to match.
    """
    if filecmp.cmp(candidate, golden, shallow=False):
        if update:
            print(f"{golden_source_path} is up to date")
        return True

    if update:
        destination = source_golden(golden_source_path)
        if destination is None:
            print(
                f"Cannot update {golden_source_path} outside of "
                "`fx bazel run`.",
                file=sys.stderr,
            )
            return False
        shutil.copyfile(candidate, destination)
        print(f"Updated {golden_source_path}")
        return True

    diff = list(
        difflib.unified_diff(
            read_lines(golden),
            read_lines(candidate),
            f"{golden_source_path} (golden)",
            f"{golden_source_path} (generated)",
        )
    )
    print(f"Golden file mismatch: {golden_source_path}\n")
    note = invisible_difference(golden.read_bytes(), candidate.read_bytes())
    if note:
        print(note + "\n")
    if len(diff) > MAX_DIFF_LINES:
        print("Diff (-golden +generated, truncated):")
        print("".join(diff[:MAX_DIFF_LINES]))
    elif diff:
        print("Diff (-golden +generated):")
        print("".join(diff))
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    # The three lists below are parallel: the Nth --candidate is compared
    # with the Nth --golden.
    parser.add_argument(
        "--candidate",
        type=Path,
        action="append",
        required=True,
        help="Generated file, relative to the test's runtime directory",
    )
    parser.add_argument(
        "--golden",
        type=Path,
        action="append",
        required=True,
        help="Golden file, relative to the test's runtime directory",
    )
    parser.add_argument(
        "--golden-source-path",
        action="append",
        required=True,
        help="Golden file path relative to the root of the checkout",
    )
    parser.add_argument(
        "--label", required=True, help="Bazel label of the golden test"
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="Overwrite stale goldens in the checkout with the generated files",
    )
    args = parser.parse_args()
    if not (
        len(args.candidate) == len(args.golden) == len(args.golden_source_path)
    ):
        parser.error(
            "--candidate, --golden, and --golden-source-path must be "
            "passed the same number of times"
        )

    # Check every golden rather than stopping at the first mismatch, so one
    # run reports (or updates) all of them.
    results = [
        check(candidate, golden, source_path, args.update)
        for candidate, golden, source_path in zip(
            args.candidate, args.golden, args.golden_source_path
        )
    ]
    if all(results):
        return 0
    if not args.update:
        print(
            "If the change is intended, update the goldens with:\n\n"
            f"  fx bazel run --config=host {args.label} -- --update\n"
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
