#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generate a debug symbols manifest file from one or more Fuchsia packages.

This tool finds all ELF binaries from the packages, extract their build-ID
value, then tries to match them with the debug symbols available from a given
input .build-id directory.
"""

import argparse
import json
import os
import sys
from pathlib import Path

# Import //build/api/debug_symbols.py. Assume this script is under //build/packages.
_SCRIPT_DIR = Path(__file__).parent
sys.path.insert(0, f"{_SCRIPT_DIR}/../api")
from debug_symbols import extract_gnu_build_id


def write_file_if_changed(path: Path, content: str) -> None:
    if not path.exists() or path.read_text() != content:
        path.write_text(content)


def main() -> int:
    parser = argparse.ArgumentParser()
    manifest_group = parser.add_mutually_exclusive_group(required=True)
    manifest_group.add_argument(
        "--package-manifest",
        type=Path,
        help="Input package manifest",
    )
    manifest_group.add_argument(
        "--package-manifests-list",
        type=Path,
        help=(
            "Input file listing package manifests, using the same schema as "
            "all_package_manifests.list"
        ),
    )
    parser.add_argument(
        "--package-label", required=True, help="Package target label"
    )
    parser.add_argument(
        "--debug-symbols-dir",
        type=Path,
        required=True,
        help="Input directory containing a .build-id/ directory",
    )
    parser.add_argument("--target-cpu", default="x64", help="Target cpu name")
    parser.add_argument(
        "--check-missing",
        action="store_true",
        help="Raise error if some debug symbols are missing from prebuilt dir",
    )
    parser.add_argument(
        "--output-build-ids-txt", type=Path, help="Output build-ids.txt file"
    )
    parser.add_argument(
        "--output-debug-manifest",
        type=Path,
        help="Output debug symbols manifest (see //:debug_symbols target)",
    )
    parser.add_argument("--depfile", type=Path, help="Ninja output depfile")
    parser.add_argument(
        "-v", "--verbose", action="count", default=0, help="Increase verbosity"
    )
    args = parser.parse_args()

    implicit_inputs: set[str] = set()

    if args.verbose > 0:

        def log(msg: str) -> None:
            print(f"LOG: {msg}", file=sys.stderr)

    else:

        def log(msg: str) -> None:
            pass

    if args.package_manifests_list:
        implicit_inputs.add(str(args.package_manifests_list))
        with open(args.package_manifests_list, "rt") as f:
            package_manifest_paths = [
                Path(p) for p in json.load(f)["content"]["manifests"]
            ]
    else:
        package_manifest_paths = [args.package_manifest]

    debug_manifest = []
    build_ids_map = {}
    missing_symbols = {}

    for package_manifest_path in package_manifest_paths:
        # The --package-manifest file is already an explicit action input, but
        # manifests named by a list file are only discovered here.
        if args.package_manifests_list:
            implicit_inputs.add(str(package_manifest_path))
        with open(package_manifest_path, "rt") as f:
            package_manifest = json.load(f)

        package_manifest_dir = package_manifest_path.parent

        sources_relative = package_manifest.get("blob_sources_relative")
        if not sources_relative:
            sources_relative = package_manifest.get(
                "sources_relative", "working_dir"
            )

        for blob in package_manifest["blobs"]:
            dest_path = blob["path"]
            file_path_relative = blob["source_path"]
            if sources_relative == "file":
                file_path_relative = os.path.relpath(
                    package_manifest_dir / file_path_relative
                )

            implicit_inputs.add(file_path_relative)
            build_id = extract_gnu_build_id(file_path_relative)
            if not build_id:
                # Skip non-ELF files and ELF files without a build-ID note.
                log(f"No ELF Build ID: {file_path_relative}")
                continue

            log(f"Found ELF file:  {file_path_relative}\n  build_id {build_id}")
            # Packages in a list commonly share libraries (e.g. libc++), which
            # must only appear once in the outputs.
            if build_id in build_ids_map:
                continue
            build_ids_map[build_id] = file_path_relative

            # Find the path of the corresponding debug symbol file
            debug_file_path = (
                args.debug_symbols_dir
                / ".build-id"
                / build_id[0:2]
                / f"{build_id[2:]}.debug"
            )
            if not debug_file_path.exists():
                log(
                    f"  MISSING SYMBOL FILE: {debug_file_path} PATH {dest_path}"
                )
                missing_symbols[file_path_relative] = build_id
                continue

            debug_manifest.append(
                {
                    "os": "fuchsia",
                    "cpu": args.target_cpu,
                    "label": args.package_label,
                    "debug": os.path.relpath(debug_file_path),
                    "elf_build_id": build_id,
                    "dest_path": dest_path,
                }
            )

    if args.output_build_ids_txt:
        build_ids_content = "".join(
            f"{build_id} {file}\n" for build_id, file in build_ids_map.items()
        )
        write_file_if_changed(args.output_build_ids_txt, build_ids_content)

    if args.check_missing and missing_symbols:
        print(
            f"ERROR: Missing debug symbols for the following files:\n%s\n"
            % "\n".join(
                sorted(
                    f"{file}  (build_id: {build_id})"
                    for file, build_id in missing_symbols.items()
                )
            ),
            file=sys.stderr,
        )
        return 1

    if args.output_debug_manifest:
        manifest_content = json.dumps(debug_manifest, sort_keys=True, indent=2)
        write_file_if_changed(args.output_debug_manifest, manifest_content)

    if args.depfile:
        with open(args.depfile, "wt") as df:
            df.write(f"{args.output_debug_manifest}: ")
            for input_file in sorted(implicit_inputs):
                df.write(f"  {input_file}")
            df.write("\n")

    return 0


if __name__ == "__main__":
    sys.exit(main())
