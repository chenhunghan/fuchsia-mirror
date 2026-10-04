#!/usr/bin/env fuchsia-vendored-python

# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import sys
from collections.abc import Sequence
from enum import Enum
from typing import IO, AbstractSet, TypedDict

SizeCheckMode = Enum("SizeCheckMode", ["EQUAL", "BAZEL_SMALLER"])


class Blob(TypedDict):
    """A single entry in a package manifest's "blobs" list."""

    source_path: str
    path: str
    merkle: str
    size: int


class Package:
    def __init__(
        self, manifest: IO[str], ignored_blobs: Sequence[str] | None = None
    ) -> None:
        pkg = json.load(manifest)
        self.repository: str = pkg.get("repository", "")
        self.blobs: dict[str, Blob] = {}
        self.driver_blobs: dict[str, Blob] = {}

        ignored_blobs = ignored_blobs or []

        for blob in pkg.get("blobs", []):
            path = blob["path"]
            # Special case the meta/ blob
            if path == "meta/":
                self.meta_merkle = blob
            elif path not in ignored_blobs:
                if path.startswith("driver/"):
                    self.driver_blobs[path] = blob
                else:
                    self.blobs[path] = blob

    def blob_paths(self) -> AbstractSet[str]:
        return set(self.blobs.keys())

    def driver_paths(self) -> AbstractSet[str]:
        return set(self.driver_blobs.keys())

    def merkle_for_blob(self, path: str) -> str:
        if path in self.blobs:
            return str(self.blobs[path]["merkle"])
        elif path in self.driver_blobs:
            return str(self.driver_blobs[path]["merkle"])
        else:
            raise KeyError(path)

    def size_for_blob(self, path: str) -> int:
        if path in self.blobs:
            return int(self.blobs[path]["size"])
        elif path in self.driver_blobs:
            return int(self.driver_blobs[path]["size"])
        else:
            raise KeyError(path)


def calculate_diff(
    gn_package: Package,
    bazel_package: Package,
    size_check_mode: SizeCheckMode,
) -> Sequence[str]:
    # If the blobs have the same merkle for their meta/ directory then they can
    # be considered the same and we will return no findings. However, if they
    # are not the same we need to check each individual blob. We need to do this
    # for 2 reasons:
    #  1) If the meta files differ we can only find out what the differences are
    #     by extracting the far contents and looking at each file. We already have
    #     most of this information, with the exception of cml files and bind objects,
    #     so we can just look at our package manifest to report our the diffs.
    #  2) We know that there are packages that have different content but we choose
    #     to ignore those files for the purposes of this verification via the
    #     ignored_blobs. The meta.far file contains a contents file which holds
    #     all of the blobs which means that even if we ignore a file the meta.far
    #     files will differ.
    if gn_package.meta_merkle == bazel_package.meta_merkle:
        return []

    findings: list[str] = []
    if gn_package.repository != bazel_package.repository:
        findings.append(
            f"Repositories do not match '{gn_package.repository}' != '{bazel_package.repository}'"
        )

    # Check to make sure that the driver blobs are the same and have the same size.
    bazel_drivers: AbstractSet[str] = bazel_package.driver_paths()
    gn_drivers: AbstractSet[str] = gn_package.driver_paths()
    common_drivers: AbstractSet[str] = gn_drivers & bazel_drivers

    def compare_driver_size(path: str) -> None:
        gn_size: int = gn_package.size_for_blob(path)
        bazel_size: int = bazel_package.size_for_blob(path)
        if size_check_mode == SizeCheckMode.EQUAL:
            if gn_size != bazel_size:
                findings.append(
                    f"Drivers at '{path}' have different sizes '{gn_size}' != '{bazel_size}'"
                )
        elif size_check_mode == SizeCheckMode.BAZEL_SMALLER:
            if bazel_size > gn_size:
                findings.append(
                    f"Driver blob at '{path}' is larger than GN driver '{bazel_size}' > '{gn_size}'"
                )
        else:
            findings.append(
                "Unknown size check mode passed to compare_driver_size"
            )

    for driver in common_drivers:
        compare_driver_size(driver)

    for driver in gn_drivers - common_drivers:
        findings.append(f"Driver at '{driver}' only exists in gn package")

    for driver in bazel_drivers - common_drivers:
        findings.append(f"Driver at '{driver}' only exists in bazel package")

    # find all the blob diffs - this does not include the drivers which are checked above
    bazel_blobs: AbstractSet[str] = bazel_package.blob_paths()
    gn_blobs: AbstractSet[str] = gn_package.blob_paths()
    common_blobs: AbstractSet[str] = gn_blobs & bazel_blobs

    def compare_blob_merkles(path: str) -> None:
        gn_merkle: str = gn_package.merkle_for_blob(path)
        bazel_merkle: str = bazel_package.merkle_for_blob(path)
        if gn_merkle != bazel_merkle:
            findings.append(
                f"Blobs at '{path}' have different merkle roots '{gn_merkle}' != '{bazel_merkle}'"
            )

    for blob in common_blobs:
        compare_blob_merkles(blob)

    for blob in gn_blobs - common_blobs:
        findings.append(f"Blob at '{blob}' only exists in gn package")

    for blob in bazel_blobs - common_blobs:
        findings.append(f"Blob at '{blob}' only exists in bazel package")

    return findings


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description="Compares drivers")
    parser.add_argument(
        "--gn-package-manifest", type=argparse.FileType("r"), required=True
    )
    parser.add_argument(
        "--bazel-package-manifest", type=argparse.FileType("r"), required=True
    )
    parser.add_argument("--output", type=argparse.FileType("w"), required=True)
    parser.add_argument(
        "--blobs-to-ignore",
        nargs="*",
        default=[],
        help="List of blob install paths to ignore.",
        required=False,
    )
    parser.add_argument(
        "--size-check-blobs",
        nargs="*",
        default=[],
        help="List of blob install paths to ignore.",
        required=False,
    )
    parser.add_argument(
        "--require-exact-sizes",
        action="store_true",
        help="""Whether sizes should be compared exactly.
        If false, size checks will assert that bazel is always smaller""",
    )
    args = parser.parse_args(argv)

    gn_package = Package(
        args.gn_package_manifest, ignored_blobs=args.blobs_to_ignore
    )
    bazel_package = Package(
        args.bazel_package_manifest, ignored_blobs=args.blobs_to_ignore
    )

    size_check_mode = (
        SizeCheckMode.EQUAL
        if args.require_exact_sizes
        else SizeCheckMode.BAZEL_SMALLER
    )
    findings = calculate_diff(gn_package, bazel_package, size_check_mode)

    if len(findings) > 0:
        findings_string = "\n".join(findings) + "\n"
        args.output.write(findings_string)
        print(
            """---------------
        Found diffs when comparing bazel and gn built driver.
        {}
        """.format(
                findings_string
            )
        )
        return 1
    else:
        args.output.write("no issues\n")
        return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
