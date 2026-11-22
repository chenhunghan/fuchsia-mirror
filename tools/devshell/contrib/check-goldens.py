#!/usr/bin/env fuchsia-vendored-python
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import subprocess
import sys

# Golden file build API module.
GOLDEN_FILES_JSON = "golden_files.json"

DESCRIPTION = """
Reruns golden file checks in the current build, relying on the
"golden_files" build API module. That is,  unless `--list` is passed, in
which case this command only prints out the associated goldens.

The goldens may be filtered by a substring of their source path (e.g., a
full source-relative directory) or by a regular expression; if no search
term is provided, all accessible golden files will be matched.

Bazel golden_test() targets (//build/bazel/rules/testing:golden_test.bzl) in the
current build's host test suites are found with `fx bazel query`. They are
run as tests, or updated with `--update` when `update_goldens=true` is set in
GN args, matching what rebuilding does for GN goldens. Like rebuilding a GN
golden_files() target, updating a golden_test() updates every golden it
checks, not just the ones that match the search term.

TODO(crbug.com/gn/301): This command may deal in stale metadata. Until this
bug is resolved, users must take care to make sure their build metadata is
up to data (e.g., via `fx gen`) before invoking this command.
"""


def main():
    parser = argparse.ArgumentParser(
        prog="fx check-goldens",
        formatter_class=argparse.RawTextHelpFormatter,
        description=DESCRIPTION,
    )
    parser.add_argument(
        "--list",
        help="Whether to just list the goldens instead of updating them",
        action="store_true",
    )
    parser.add_argument(
        "--regex",
        help="if set, the search term will be matched as a regular expression",
        action="store_true",
    )
    parser.add_argument(
        "--all",
        help="if set, includes otherwise ignored, dynamically-specified goldens (i.e., those specified via a build-time manifest)",
        action="store_true",
    )
    parser.add_argument(
        "search_term",
        help="search term to match golden files against",
        nargs="?",
    )
    args = parser.parse_args()

    build_dir = os.path.relpath(os.getenv("FUCHSIA_BUILD_DIR"))
    if build_dir is None:
        print("FUCHSIA_BUILD_DIR not set")
        return 1

    golden_files_json = os.path.join(build_dir, GOLDEN_FILES_JSON)
    if not os.path.exists(golden_files_json):
        subprocess.check_call(["fx", "gen"])

    with open(golden_files_json) as f:
        entries = json.load(f)

    # Account for comparisons given indirectly via build-time manifests,
    # ensuring that they are built before proceeding.
    if args.all:
        manifests = []
        for entry in entries:
            if "comparison_manifest" in entry:
                manifests.append(entry["comparison_manifest"])
        if manifests:
            subprocess.check_call(["fx", "build", "--"] + manifests)

    # Reload `entries`, as the JSON file may have been regenerated.
    with open(golden_files_json) as f:
        entries = json.load(f)

    def is_match(filename):
        if not args.search_term:
            return True
        if args.regex:
            return re.search(args.search_term, filename) != None
        return args.search_term in filename

    stamps = []
    matches = []
    for entry in entries:
        comparisons = []
        if "comparisons" in entry:
            comparisons = entry["comparisons"]
        elif args.all:
            manifest = os.path.join(build_dir, entry["comparison_manifest"])
            with open(manifest) as f:
                comparisons = json.load(f)

        matched = False
        for comparison in comparisons:
            golden = comparison["golden"]
            if is_match(golden):
                matched = True
                matches.append(golden)
        if matched:
            stamps.append(entry["stamp"])

    bazel_tests = []
    for label, golden in bazel_golden_tests(build_dir):
        if is_match(golden):
            matches.append(golden)
            # One golden_test() can check several goldens.
            if label not in bazel_tests:
                bazel_tests.append(label)

    if args.list:
        print("\n".join(matches))
        return 0
    # A failing build or test has already printed why, so pass on its exit
    # code rather than adding a traceback.
    commands = []
    if stamps:
        commands.append(["fx", "build", "--"] + stamps)
    if bazel_tests:
        if update_goldens(build_dir):
            commands += [
                ["fx", "bazel", "run", "--config=host", label, "--", "--update"]
                for label in bazel_tests
            ]
        else:
            commands.append(
                ["fx", "bazel", "test", "--config=host", "--test_output=errors"]
                + bazel_tests
            )
    for command in commands:
        returncode = subprocess.run(command).returncode
        if returncode:
            return returncode
    return 0


def update_goldens(build_dir: str) -> bool:
    with open(os.path.join(build_dir, "args.json")) as f:
        return bool(json.load(f).get("update_goldens", False))


def bazel_golden_tests(build_dir: str) -> list[tuple[str, str]]:
    """Returns (label, golden source path) pairs for Bazel golden_test()s.

    A test that checks several goldens appears once per golden.
    """
    # Only look at host test suites in the current build: `//...` includes
    # packages that don't load in every checkout, and this matches what
    # `fx test` can see.
    suites_file = os.path.join(build_dir, "bazel_host_test_suites.txt")
    if not os.path.exists(suites_file):
        return []
    with open(suites_file) as f:
        suites = f.read().split()
    if not suites:
        return []

    # The golden path is recovered from the test's arguments, which
    # golden_test() sets, so this needs no build.
    # attr() matches list attributes against their "[a, b]" string form.
    # LINT.IfChange(golden_test_query)
    query = 'attr(tags, "[\\[ ]golden_test[,\\]]", tests(set(%s)))' % " ".join(
        suites
    )
    # LINT.ThenChange(//build/bazel/rules/testing/golden_test.bzl:golden_test_query)
    result = subprocess.run(
        [
            "fx",
            "bazel",
            "query",
            "--config=host",
            "--output=streamed_jsonproto",
            query,
        ],
        capture_output=True,
        text=True,
    )
    # Don't let a broken Bazel graph block GN golden checks.
    if result.returncode != 0:
        print(
            "Warning: skipping Bazel golden tests, `fx bazel query` failed:\n"
            + result.stderr,
            file=sys.stderr,
        )
        return []
    results = []
    for line in result.stdout.splitlines():
        try:
            rule = json.loads(line).get("rule")
        except json.JSONDecodeError:
            continue
        if not rule:
            continue
        for attr in rule.get("attribute", []):
            if attr.get("name") != "test_args":
                continue
            for arg in attr.get("stringListValue", []):
                # LINT.IfChange(golden_test_args)
                if arg.startswith("--golden-source-path="):
                    golden = arg.split("=", 1)[1]
                    results.append((rule["name"], golden))
                # LINT.ThenChange(//build/bazel/rules/testing/golden_test.bzl:golden_test_args)
    return results


if __name__ == "__main__":
    sys.exit(main())
