#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Creates a deterministic tarball of ADB vendor keys."""

import argparse
import glob
import os
import sys
import tarfile


def _reset_tarinfo(tarinfo: tarfile.TarInfo) -> tarfile.TarInfo:
    """Resets file metadata in TarInfo for reproducible builds."""
    tarinfo.uid = 0
    tarinfo.gid = 0
    tarinfo.uname = ""
    tarinfo.gname = ""
    tarinfo.mtime = 0
    return tarinfo


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Creates a tarball of ADB vendor keys."
    )
    parser.add_argument(
        "--input-dir",
        required=True,
        help="Directory containing *.adb_key files.",
    )
    parser.add_argument(
        "--output",
        required=True,
        help="Path to output .tar file.",
    )
    parser.add_argument(
        "--depfile",
        required=True,
        help="Path to write Ninja depfile.",
    )
    args = parser.parse_args()

    key_files = sorted(
        glob.glob(
            os.path.join(args.input_dir, "**", "*.adb_key"), recursive=True
        )
    )
    if not key_files:
        print(
            f"Error: No *.adb_key files found in {args.input_dir}",
            file=sys.stderr,
        )
        return 1

    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with tarfile.open(
        args.output, "w", dereference=True, format=tarfile.GNU_FORMAT
    ) as tar:
        for key_path in key_files:
            rel_path = os.path.relpath(key_path, args.input_dir)
            arcname = rel_path.replace(os.sep, "_")
            tar.add(
                key_path,
                arcname=arcname,
                filter=_reset_tarinfo,
            )

    os.makedirs(os.path.dirname(args.depfile), exist_ok=True)
    with open(args.depfile, "w") as f:
        f.write(f"{args.output}: {' '.join(key_files)}\n")

    return 0


if __name__ == "__main__":
    sys.exit(main())
