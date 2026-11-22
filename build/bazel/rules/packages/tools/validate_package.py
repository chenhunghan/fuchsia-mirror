#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs the build-time validations of a Bazel-built Fuchsia package.

These match the validations that //build/components/package_creation_wrapper.py
runs on GN-built packages:

- `elf`: verifies the ELF binaries in the package with
  //build/python/modules/elf/verify_manifest_elf_binaries.py.
- `structured-config`: runs `configc validate-package` on the package.

On success, an empty stamp file is written. With `--expect-failure`, the
result is inverted: the validation must fail with an error that contains the
given message. Tests use this to check that invalid packages are rejected.
"""

import argparse
import subprocess
import sys
from pathlib import Path

from elf.verify_manifest_elf_binaries import (
    VerificationFailure,
    verify_manifest_elf_binaries,
)


def _read_fini_manifest(path: Path) -> dict[str, str]:
    entries: dict[str, str] = {}
    for line in path.read_text().splitlines():
        if not line:
            continue
        dest, sep, src = line.partition("=")
        if not sep:
            raise ValueError(f"Invalid manifest line in {path}: [{line}]")
        entries[dest] = src
    return entries


def _validate_elf(args: argparse.Namespace) -> str | None:
    """Returns an error message on failure, or None on success."""
    try:
        verify_manifest_elf_binaries(
            _read_fini_manifest(args.fini_manifest),
            toolchain_lib_dirs=[],
            build_id_dirs=args.build_id_dir,
            debug_files=args.debug_file,
        )
    except VerificationFailure as e:
        return (
            f"ELF binary verification failed for {args.label}:\n{e}\n\n"
            "To disable this check, set `validate_elf_binaries = False` "
            "on this target."
        )
    return None


def _validate_structured_config(args: argparse.Namespace) -> str | None:
    """Returns an error message on failure, or None on success."""
    result = subprocess.run(
        [
            str(args.configc),
            "validate-package",
            str(args.package_manifest),
            "--stamp",
            "/dev/null",
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode == 0:
        return None
    return (
        f"Structured configuration validation failed for {args.label}:\n"
        f"{result.stdout}{result.stderr}\n"
        "If this is a test package (e.g. `fx_package()` with "
        "`test_components`) and RealmBuilder provides all configuration "
        "values at runtime, set `validate_structured_config = False` on this "
        "target to disable this check."
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--label", required=True, help="Label of the package target"
    )
    parser.add_argument(
        "--stamp", type=Path, required=True, help="Stamp file to write"
    )
    parser.add_argument(
        "--expect-failure",
        metavar="MESSAGE",
        help="Succeed only if the validation fails with MESSAGE in its error.",
    )
    subparsers = parser.add_subparsers(dest="validation", required=True)

    elf_parser = subparsers.add_parser(
        "elf", help="Verify the ELF binaries in the package."
    )
    elf_parser.add_argument(
        "--fini-manifest",
        type=Path,
        required=True,
        help="Package creation manifest, with one `dest=src` entry per line.",
    )
    elf_parser.add_argument(
        "--build-id-dir",
        default=[],
        action="append",
        help="A .build-id directory holding debug files. Can be repeated.",
    )
    elf_parser.add_argument(
        "--debug-file",
        default=[],
        action="append",
        help="An unstripped file of a prebuilt stripped binary. Can be repeated.",
    )

    sc_parser = subparsers.add_parser(
        "structured-config",
        help="Validate the structured config of the package's components.",
    )
    sc_parser.add_argument(
        "--configc", type=Path, required=True, help="Path to configc"
    )
    sc_parser.add_argument(
        "--package-manifest",
        type=Path,
        required=True,
        help="The package_manifest.json of the package.",
    )

    args = parser.parse_args()

    if args.validation == "elf":
        error = _validate_elf(args)
    else:
        error = _validate_structured_config(args)

    if args.expect_failure is None:
        if error:
            print(error, file=sys.stderr)
            return 1
    elif error is None:
        print(
            f"Expected the {args.validation} validation of {args.label} to "
            f"fail with:\n  {args.expect_failure}\nbut it passed.",
            file=sys.stderr,
        )
        return 1
    elif args.expect_failure not in error:
        print(
            f"Expected the {args.validation} validation of {args.label} to "
            f"fail with:\n  {args.expect_failure}\nbut it failed with:\n"
            f"{error}",
            file=sys.stderr,
        )
        return 1

    args.stamp.write_text("")
    return 0


if __name__ == "__main__":
    sys.exit(main())
