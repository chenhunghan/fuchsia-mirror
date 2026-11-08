#!/usr/bin/env python3
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import re
import shutil
import subprocess
import sys
from collections import defaultdict
from typing import Any, Dict, List

FAILED_STATUSES = ("FAIL", "CRASH", "ABORT")


def check_prerequisites() -> None:
    missing = []
    for cmd in ("bb", "rdb"):
        if shutil.which(cmd) is None:
            missing.append(cmd)
    if missing:
        print(
            f"Error: Missing required commands: {', '.join(missing)}",
            file=sys.stderr,
        )
        print(
            "Please install depot_tools by following the instructions at go/depottools#_setting_up",
            file=sys.stderr,
        )
        sys.exit(1)


def run_cmd(cmd: List[str], input_data: str = None) -> str:
    try:
        res = subprocess.run(
            cmd, input=input_data, capture_output=True, text=True, check=True
        )
        return res.stdout
    except subprocess.CalledProcessError as e:
        print(f"Error running {' '.join(cmd)}: {e.stderr}", file=sys.stderr)
        return ""


def fetch_builds(builder: str, bucket: str, cutoff_date: str) -> List[str]:
    """Fetches relevant build IDs for a specific builder since the cutoff date, including SUCCESS to catch flaky runs."""
    print(f"Fetching builds for {builder} in {bucket} since {cutoff_date}...")
    relevant_builds = []

    cmd = [
        "bb",
        "ls",
        f"turquoise/{bucket}/{builder}",
        "-json",
        "-nopage",
    ]
    try:
        proc = subprocess.Popen(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        for line in proc.stdout:
            if not line.strip():
                continue
            try:
                build = json.loads(line)
            except json.JSONDecodeError:
                continue

            create_time = build.get("createTime", "")
            if create_time:
                date_str = create_time[:10]
                if date_str < cutoff_date:
                    break
                # Filter out incomplete (SCHEDULED, STARTED), CANCELED, and
                # INFRA_FAILURE builds, keeping only completed test runs.
                if build.get("status") in ("SUCCESS", "FAILURE"):
                    relevant_builds.append(build["id"])

        # If we broke out early because the cutoff date was reached, terminate the
        # background `bb ls` process to avoid leaving an orphaned task running.
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
    except Exception as e:
        print(f"Error running {' '.join(cmd)}: {e}", file=sys.stderr)

    return relevant_builds


def fetch_results_with_bot_ids(
    build_ids: List[str], test_pattern: str
) -> List[Dict[str, Any]]:
    """Queries ResultDB to fetch specific test results along with their swarming bot IDs."""
    print(
        f"Querying ResultDB for {test_pattern} results in {len(build_ids)} builds..."
    )
    all_results = []
    pattern_re = re.compile(test_pattern)

    for build_id in build_ids:
        cmd = [
            "rdb",
            "query",
            "-json",
            "-tr-fields",
            "testId,status,name,failureReason,summaryHtml,tags",
        ]
        if test_pattern and test_pattern != ".*":
            rdb_test_arg = test_pattern
            if not rdb_test_arg.startswith(".*"):
                rdb_test_arg = f".*{rdb_test_arg}"
            if not rdb_test_arg.endswith(".*"):
                rdb_test_arg = f"{rdb_test_arg}.*"
            cmd.extend(["-test", rdb_test_arg])

        cmd.append(f"build-{build_id}")
        out = run_cmd(cmd)
        if not out:
            continue

        for line in out.splitlines():
            if not line.strip():
                continue
            try:
                data = json.loads(line)
                tr = data.get("testResult", {})
                if not tr:
                    continue
                if pattern_re.search(tr.get("testId", "")):
                    all_results.append(tr)
            except json.JSONDecodeError:
                continue

    return all_results


def filter_leaf_results(results: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Drops skipped runs and suite wrappers that have sub-test results."""
    # ResultDB `name` has the form:
    #   "invocations/u-swarming-.../tests/<encoded_test_id>/results/<id>"
    # Grouping by invocation isolates separate Swarming runs (e.g. retries).
    #
    # Mobly E2E tests emit both a top-level suite wrapper result, e.g.:
    #   testId = "host_x64/.../ping_test.sh-for-testing-wlan-platform.astro"
    # and sub-test case results prefixed with "<suite_testId>/", e.g.:
    #   testId = ".../ping_test.sh.../PingTest:test_simultaneous_pings"
    all_ids = defaultdict(set)
    for r in results:
        inv = r.get("name", "").split("/tests/", 1)[0]
        tid = r.get("testId", "")
        if tid:
            all_ids[inv].add(tid)

    results_without_skips = [r for r in results if r.get("status") != "SKIP"]
    active_ids = defaultdict(set)
    for r in results_without_skips:
        inv = r.get("name", "").split("/tests/", 1)[0]
        tid = r.get("testId", "")
        if tid:
            active_ids[inv].add(tid)

    filtered = []
    for r in results_without_skips:
        inv = r.get("name", "").split("/tests/", 1)[0]
        prefix = f"{r.get('testId', '')}/"
        # Drop the suite wrapper when non-skipped sub-test results exist in the
        # same invocation so runs are not double-counted.
        if any(t.startswith(prefix) for t in active_ids[inv]):
            continue
        # If all sub-tests were SKIP, keep the suite wrapper only if it failed
        # (e.g. Mobly setup_class failed before any test case ran); drop it if
        # it passed so an all-skipped suite is not counted as a passing run.
        if r.get("status") not in FAILED_STATUSES and any(
            t.startswith(prefix) for t in all_ids[inv]
        ):
            continue
        filtered.append(r)
    return filtered


def main() -> None:
    check_prerequisites()

    parser = argparse.ArgumentParser(description="LUCI Triage Utility")
    parser.add_argument(
        "--builder",
        default="fuchsia_internal.arm64-release-fyi",
        help="Builder name to analyze",
    )
    parser.add_argument(
        "--bucket",
        default="global.ci",
        help="Bucket name (e.g., global.ci, smart.ci)",
    )
    parser.add_argument(
        "--cutoff-date",
        required=True,
        help="YYYY-MM-DD date to search back until",
    )
    parser.add_argument(
        "--test-pattern",
        default=".*",
        help="Regex pattern to filter specific test IDs",
    )

    args = parser.parse_args()

    builds = fetch_builds(args.builder, args.bucket, args.cutoff_date)
    results = filter_leaf_results(
        fetch_results_with_bot_ids(builds, args.test_pattern)
    )

    bot_stats = defaultdict(lambda: {"total": 0, "failed": 0})
    for r in results:
        bot_id = "unknown"
        for t in r.get("tags", []):
            if t.get("key") == "swarming_bot_id":
                bot_id = t.get("value")
                break

        bot_stats[bot_id]["total"] += 1
        if r.get("status") in FAILED_STATUSES:
            bot_stats[bot_id]["failed"] += 1

    print("\n=== Bot ID Failure Distribution ===")
    for bot, stats in sorted(
        bot_stats.items(), key=lambda x: x[1]["failed"], reverse=True
    ):
        total = stats["total"]
        failed = stats["failed"]
        rate = (failed / total) * 100 if total > 0 else 0
        print(f"{bot}: {failed}/{total} failures ({rate:.1f}%)")


if __name__ == "__main__":
    main()
