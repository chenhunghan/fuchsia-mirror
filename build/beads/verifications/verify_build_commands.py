#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Verifies build commands for a list of GN and Bazel target pairs defined in a manifest file.
"""

import argparse
import json
import pathlib
import sys
import typing as T

_DEBUG = False

_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

sys.path.insert(0, str(_FUCHSIA_DIR / "build/bazel/scripts"))
import build_utils

sys.path.insert(0, str(_FUCHSIA_DIR / "build/beads/scripts"))
import compare_utils
import flags_differences
from compare_utils import CompareCommandsResult


def debug(s: T.Any) -> None:
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify GN and Bazel build commands for a list of targets."
    )

    build_utils.BuildPaths.add_parser_arguments(parser)

    parser.add_argument(
        "--manifest",
        required=True,
        type=pathlib.Path,
        help="Path to the manifest file (JSON)",
    )
    parser.add_argument(
        "--read_response_files",
        action="store_true",
        default=False,
        help="Read response files directly from the Bazel execroot instead of querying Starlark providers.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output",
    )
    parser.add_argument(
        "--report",
        type=pathlib.Path,
        help="Write detailed differences to report file",
    )
    parser.add_argument("--stamp", type=pathlib.Path, help="Stamp file path")

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    try:
        paths = build_utils.BuildPaths.from_parser_args(args)
    except ValueError as e:
        parser.error(str(e))

    with open(args.manifest) as f:
        targets = json.load(f)

    # TODO(https://fxbug.dev/502754609): Add support for clang targets.
    target_queries = [
        compare_utils.CompareCommandsQuery.from_dict(t) for t in targets
    ]

    if not target_queries:
        print("No targets to compare.")
        return 0

    debug(f"Fuchsia Dir: {paths.fuchsia_dir}")
    debug(f"Build Dir: {paths.build_dir}")
    debug(f"Manifest Path: {args.manifest}")
    debug(f"Target queries:")
    for target_query in target_queries:
        debug(f"  {str(target_query)}")

    bazel_paths = build_utils.BazelPaths(paths.fuchsia_dir, paths.build_dir)

    results: list[
        CompareCommandsResult
    ] = compare_utils.compare_gn_and_bazel_commands_for(
        target_queries,
        bazel_paths,
        read_response_files=args.read_response_files,
        debug=debug,
    )

    all_success = True
    differences_count = 0

    report_text = ""

    for idx, result in enumerate(results):
        if result.error:
            print(result.error)
            all_success = False
            continue

        normalized_gn_args = result.normalized_gn_args
        normalized_bazel_args = result.normalized_bazel_args

        if _DEBUG:
            debug(f"GN normalized rustc command:\n{normalized_gn_args}\n")
            debug(f"Bazel normalized rustc command:\n{normalized_bazel_args}\n")

        gn_label = result.query.gn
        bazel_label = result.query.bazel
        action_type = result.query.action_type
        description = f"{gn_label} vs {bazel_label}, action type {action_type}"

        differences = flags_differences.FlagsDifferences.new_from_lists(
            normalized_gn_args, normalized_bazel_args
        )

        if differences.has_differences:
            debug(f"Mismatch for {description}")
            differences_count += 1

            flag_categorizer = (
                flags_differences.categorize_rust_flag
                if action_type == compare_utils.ACTION_RUSTC
                else flags_differences.categorize_clang_flag
            )

            report_text += "\n\n" + differences.generate_summary(
                title=description, flag_categorizer=flag_categorizer
            )
            if not result.query.allow_differences:
                all_success = False
        else:
            debug(f"Match for {description}")

    debug(f"Found {differences_count} targets with differences.")

    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(report_text)

    if all_success and args.stamp:
        with open(args.stamp, "w") as f:
            f.write("")

    return 0 if all_success else 1


if __name__ == "__main__":
    sys.exit(main())
