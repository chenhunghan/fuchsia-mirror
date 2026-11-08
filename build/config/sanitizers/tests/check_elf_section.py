#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verify ELF section contents using llvm-readelf."""

import argparse
import os
import subprocess
import sys

import python.runfiles.runfiles as runfiles


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify ELF section contents using llvm-readelf"
    )
    parser.add_argument(
        "--readelf", required=True, help="Rlocation path to llvm-readelf"
    )
    parser.add_argument(
        "--binary", required=True, help="Rlocation path to binary to inspect"
    )
    parser.add_argument("--section", required=True, help="Name of ELF section")
    parser.add_argument(
        "--expected",
        action="append",
        required=True,
        help="Expected string in section",
    )
    args = parser.parse_args()

    r = runfiles.Create()
    assert r, "Cannot find runfiles"
    readelf_path = r.Rlocation(args.readelf)
    binary_path = r.Rlocation(args.binary)

    if not readelf_path or not os.path.exists(readelf_path):
        print(
            f"Error: llvm-readelf not found at '{readelf_path}' (rlocation: {args.readelf})",
            file=sys.stderr,
        )
        return 1

    if not binary_path or not os.path.exists(binary_path):
        print(
            f"Error: binary not found at '{binary_path}' (rlocation: {args.binary})",
            file=sys.stderr,
        )
        return 1

    cmd = [readelf_path, "-p", args.section, binary_path]
    res = subprocess.run(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    if res.returncode != 0:
        print(
            f"Error running llvm-readelf ({res.returncode}):\n{res.stderr}",
            file=sys.stderr,
        )
        return 1

    output = res.stdout
    print(f"Section '{args.section}' dump from {binary_path}:\n{output}")

    missing = []
    for exp in args.expected:
        if exp not in output:
            missing.append(exp)

    if missing:
        print(
            f"FAIL: Missing expected strings in section '{args.section}': {missing}",
            file=sys.stderr,
        )
        return 1

    print("PASS: All expected strings found in ELF section.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
