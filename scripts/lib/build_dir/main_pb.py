#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Resolves the active main product bundle name for a Fuchsia build directory.

Uses only the Python standard library so it can be imported by host tools or
invoked directly under system python3 with low startup latency and without
requiring downloaded prebuilts.
"""

from __future__ import annotations

import os
import sys


def resolve_product_bundle_name(
    build_dir: str | os.PathLike[str], main_pb_label: str
) -> str | None:
    """Maps a GN product bundle label to its name in product_bundles.json."""
    clean_label = main_pb_label.split("(", 1)[0]
    if not clean_label:
        return None
    import json

    try:
        with open(
            os.path.join(os.fspath(build_dir), "product_bundles.json"),
            encoding="utf-8",
        ) as f:
            pbs = json.load(f)
    except (OSError, ValueError):
        return None

    if not isinstance(pbs, list):
        return None
    for pb in pbs:
        if not isinstance(pb, dict):
            continue
        pb_label = pb.get("label")
        if (
            isinstance(pb_label, str)
            and pb_label.split("(", 1)[0] == clean_label
        ):
            name = pb.get("name")
            if isinstance(name, str) and name:
                return name
    return None


def get_main_pb_name(
    build_dir: str | os.PathLike[str],
    fallback_to_label_name: bool = True,
) -> str | None:
    """Returns the main product bundle name configured in args.json."""
    bdir = os.fspath(build_dir)
    try:
        with open(os.path.join(bdir, "args.json"), encoding="utf-8") as f:
            raw_args = f.read()
        if '"main_pb_label"' not in raw_args:
            return None
        import json

        args = json.loads(raw_args)
    except (OSError, ValueError):
        return None

    if not isinstance(args, dict):
        return None
    label = args.get("main_pb_label")
    if not isinstance(label, str) or not label:
        return None

    resolved = resolve_product_bundle_name(bdir, label)
    if resolved:
        return resolved
    if fallback_to_label_name:
        fallback = label.split("(", 1)[0].rsplit(":", 1)[-1]
        return fallback or None
    return None


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--build-dir",
        default=os.environ.get("FUCHSIA_BUILD_DIR")
        or os.environ.get("FUCHSIA_BUILD_DIR_FROM_FX"),
        help="Path to the Fuchsia build directory.",
    )
    parsed = parser.parse_args(argv)
    if not parsed.build_dir:
        return 1

    name = get_main_pb_name(parsed.build_dir)
    if not name:
        return 1
    print(name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
