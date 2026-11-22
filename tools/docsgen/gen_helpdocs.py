#!/usr/bin/env fuchsia-vendored-python
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generate helpdoc reference docs for fx commands and subcommands.

This script calls tools/devshell/helpdoc and generates reference docs for fx
commands and subcommands.
"""

import argparse
import os
import subprocess
import sys


def run_fx_helpdoc(
    src_dir: str,
    out_path: str,
    dep_file: str,
    log_to_file: str | None = None,
) -> int:
    helpdoc_script = os.path.join(src_dir, "tools/devshell/helpdoc")
    completed_process = subprocess.run(
        [helpdoc_script, "--depfile", dep_file, "--archive", out_path],
        capture_output=True,
        text=True,
    )
    if log_to_file:
        with open(log_to_file, "w") as log_file:
            if completed_process.stdout:
                log_file.write(completed_process.stdout)

    if completed_process.returncode:
        for output in (completed_process.stdout, completed_process.stderr):
            if output:
                print(output.rstrip("\n"), file=sys.stderr)
        return completed_process.returncode

    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,  # Prepend helpdoc with this file's docstring.
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "-o",
        "--out-path",
        type=str,
        required=True,
        help="Output location where generated docs should go",
    )
    parser.add_argument(
        "-s",
        "--src-dir",
        type=str,
        required=True,
        help="Home location of Fuchsia relative to build",
    )
    parser.add_argument(
        "-l", "--log-to-file", type=str, help="File for logging stdout."
    )
    parser.add_argument(
        "--depfile",
        type=str,
        required=True,
        help="Location for generating a depfile.",
    )

    args = parser.parse_args()
    with open(args.depfile, "w"):
        pass
    return run_fx_helpdoc(
        args.src_dir, args.out_path, args.depfile, args.log_to_file
    )


if __name__ == "__main__":
    sys.exit(main())
