# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import collections
import concurrent.futures
import json
import os
import pathlib
import re
import statistics
import subprocess
import tempfile
import urllib.parse
from collections.abc import Collection, Set
from typing import Any, Callable, TextIO, TypeVar

_DEFAULT_STATE_FILE = (
    "/google/data/ro/teams/tq-performance/sample_builds_state.json"
)

# Matches the links to each test task's CAS outputs in build step summaries,
# capturing the CAS instance and the root digest of the task's output tree.
# Step summaries also link to other CAS uploads (such as the aggregated perf
# dataset), which this deliberately does not match.
_TASK_OUTPUTS_LINK_REGEX = re.compile(
    r"\[task outputs\]\(https://cas-viewer\.(?:appspot\.com|api\.luci\.app)/"
    r"(projects/[^/]+/instances/[^/]+)/blobs/([0-9a-fA-F]+/[0-9]+)/tree\)"
)

_PERF_RESULTS_SUFFIX = ".fuchsiaperf.json"

# Test tasks write their results under this directory. The aggregated perf
# dataset stores each task's results without this prefix.
_TASK_RESULTS_PREFIX = "out/"

# Maximum number of concurrent `cas` invocations.
_MAX_CAS_JOBS = 8

T = TypeVar("T")
R = TypeVar("R")


def _normalize_build_id(build_id: str) -> str:
    """Normalizes a Buildbucket build ID by stripping any leading 'b' or 'B' prefix."""
    return build_id.strip().removeprefix("b").removeprefix("B")


def download_metrics(args, out_fh: TextIO) -> None:
    """Downloads metrics artifacts from CAS."""
    bb_tool = _find_bb_tool()
    _check_bb_auth(bb_tool)
    _download_metrics(bb_tool, args.build_id, args.out_dir, out_fh)


def download_baseline(args, out_fh: TextIO) -> None:
    """Finds the most recent successful sample build for a given builder and downloads its metrics."""
    bb_tool = _find_bb_tool()
    _check_bb_auth(bb_tool)
    _download_baseline(
        bb_tool,
        args.builder_name,
        args.count,
        args.out_dir,
        args.state_file,
        out_fh,
    )


def extract_metrics(args, out_fh: TextIO) -> None:
    """Extracts metrics from Fuchsia performance results."""
    _extract_metrics(args.suite_prefix, args.data_dir, args.out_file, out_fh)


# TODO(https://github.com/python/cpython/issues/101503): Fix the visibility
# violation by changing the type specification of "subparsers" to whatever
# publicly-available type is determined for it to have.
def register_subparsers(
    subparsers: "argparse._SubParsersAction[argparse.ArgumentParser]",
    out_fh: TextIO,
) -> None:
    """Registers subparsers for workflow commands."""
    subparser = subparsers.add_parser(
        "download_metrics",
        help="Downloads metrics artifacts from CAS.",
    )
    subparser.add_argument(
        "build_id",
        help="The Buildbucket build ID for which to download metrics (with or without 'b' prefix).",
    )
    subparser.add_argument(
        "out_dir",
        help="The directory to save the downloaded CAS contents into.",
    )
    subparser.set_defaults(func=lambda args: download_metrics(args, out_fh))

    subparser = subparsers.add_parser(
        "download_baseline",
        help="Finds the most recent successful sample build and downloads its metrics.",
    )
    subparser.add_argument(
        "-n",
        "--count",
        type=int,
        default=5,
        help="The number of builds to check (default: 5).",
    )
    subparser.add_argument(
        "builder_name",
        help="The name of the builder by which to filter.",
    )
    subparser.add_argument(
        "out_dir",
        help="The directory to save the downloaded CAS contents into.",
    )
    subparser.add_argument(
        "--state_file",
        default=_DEFAULT_STATE_FILE,
        help="Path to the sample builds state file.",
    )
    subparser.set_defaults(func=lambda args: download_baseline(args, out_fh))

    subparser = subparsers.add_parser(
        "extract_metrics",
        help="Extracts metrics from Fuchsia performance results.",
    )
    subparser.add_argument(
        "suite_prefix",
        help="The test suite prefix by which to filter metrics.",
    )
    subparser.add_argument(
        "data_dir",
        nargs="?",
        default=".",
        help="Directory to process. Defaults to the current directory.",
    )
    subparser.add_argument(
        "--out_file",
        help="A path to save the resulting CSV.",
    )
    subparser.set_defaults(func=lambda args: extract_metrics(args, out_fh))


def _download_metrics(
    bb_tool: pathlib.Path,
    build_id: str,
    out_dir: str,
    out_fh: TextIO,
) -> None:
    build_id = _normalize_build_id(build_id)
    os.makedirs(out_dir, exist_ok=True)

    cas_tool = _find_cas_tool()

    try:
        build_info = _get_build_info(bb_tool, build_id, out_fh)
        # Every build that ran tests has output properties, so a KeyError
        # here means we cannot proceed. It is reported by the handler below.
        properties = build_info["output"]["properties"]
        perf_dataset = _get_perf_dataset(properties)
        if perf_dataset is not None:
            instance, digest = perf_dataset
            _download_from_cas(cas_tool, instance, digest, out_dir, out_fh)
        else:
            # Builds do not publish the aggregated dataset if any test
            # failed, but the results from each test task are still in CAS.
            out_fh.write(
                f"Build {build_id} has no aggregated perf dataset. "
                "Collecting results from individual test tasks...\n"
            )
            failed_tests = properties.get("failed_test_names", [])
            if failed_tests:
                out_fh.write("Excluding results from failed tests:\n")
                for test in failed_tests:
                    out_fh.write(f"  {test}\n")
            _download_task_results(
                cas_tool,
                build_id,
                # Buildbucket omits "steps" when it is empty.
                build_info.get("steps", []),
                set(failed_tests),
                out_dir,
                out_fh,
            )
    except (subprocess.CalledProcessError, RuntimeError, KeyError) as e:
        raise RuntimeError(f"Download failed for build {build_id}") from e


def _download_baseline(
    bb_tool: pathlib.Path,
    builder_name: str,
    count: int,
    out_dir: str,
    state_file: str,
    out_fh: TextIO,
) -> None:
    if not os.path.exists(state_file):
        raise FileNotFoundError(f"Error: State file not found at {state_file}")

    with open(state_file, "r") as f:
        state = json.load(f)

    filtered_dates = [
        date
        for date, entry in state.items()
        if isinstance(entry.get("launched"), dict)
    ]

    sorted_dates = sorted(filtered_dates, reverse=True)
    last_n_dates = sorted_dates[:count]

    out_fh.write(
        f"Finding most recent successful sample build for {builder_name} "
        f"(checking last {count} entries)...\n"
    )

    for date in last_n_dates:
        launched = state[date].get("launched", {})
        build_id = launched.get(builder_name, {}).get("build_id", "")
        if build_id:
            status = _get_build_status(bb_tool, build_id)

            if status == "SUCCESS":
                out_fh.write(
                    f"  {date}: {build_id} (SUCCESS) - Triggering download to {out_dir}...\n"
                )

                _download_metrics(bb_tool, build_id, out_dir, out_fh)
                break

            out_fh.write(f"  {date}: {build_id} ({status}) - Skipping...\n")
    else:
        out_fh.write(
            f"No successful sample build IDs found for {builder_name} in the last "
            f"{count} entries.\n"
        )


def _extract_metrics(
    suite_prefix: str, data_dir: str, out_file: str, out_fh: TextIO
) -> None:
    target_dir = pathlib.Path(data_dir)
    perfcompare_dirs = (target_dir / "with_cl", target_dir / "without_cl")
    if all(p.is_dir() for p in perfcompare_dirs):
        if not out_file:
            raise ValueError(
                "Error: --out_file is required when data_dir contains results from multiple builds."
            )
        with open(perfcompare_dirs[0] / out_file, "w") as f:
            _process_files(suite_prefix, perfcompare_dirs[0], f, out_fh)
        with open(perfcompare_dirs[1] / out_file, "w") as f:
            _process_files(suite_prefix, perfcompare_dirs[1], f, out_fh)

    elif out_file:
        with open(out_file, "w") as f:
            _process_files(suite_prefix, target_dir, f, out_fh)

    else:
        _process_files(suite_prefix, target_dir, out_fh, out_fh)


def _process_files(
    suite_prefix: str,
    search_dir: pathlib.Path,
    out_file: TextIO,
    out_fh: TextIO,
) -> None:
    """Finds and processes all *.fuchsiaperf.json files in a single pass."""
    fuchsia_perf_files = sorted(search_dir.rglob("*.fuchsiaperf.json"))

    aggregated_results = collections.defaultdict(
        lambda: collections.defaultdict(list)
    )

    for file_path in fuchsia_perf_files:
        try:
            with open(file_path) as f:
                data = json.load(f)
                for record in data:
                    suite = record.get("test_suite")
                    label = record.get("label")

                    if suite and suite.startswith(suite_prefix):
                        # We just take the mean if there are multiple values.
                        # This is lossy, but it's the most sensical thing we can
                        # do when aggregating multiple runs for output in CSV.
                        value = statistics.mean(record["values"])
                        aggregated_results[suite][label].append(value)
        except json.JSONDecodeError as e:
            raise RuntimeError(
                f"Malformed JSON in file {file_path}: {e}"
            ) from e

    for suite, metrics in sorted(aggregated_results.items()):
        for metric, values in sorted(metrics.items()):
            values_str = ",".join(map(str, values))
            out_file.write(f"{suite},{metric},{values_str}\n")


def _get_build_info(
    bb_tool: pathlib.Path, build_id: str, out_fh: TextIO
) -> dict[str, Any]:
    """Uses the `bb` tool to get a build's output properties and steps."""
    cmd = [
        bb_tool,
        "get",
        "-fields",
        "output.properties,steps",
        "-json",
        build_id,
    ]
    out_fh.write(f"Fetching build info for {build_id} using bb...\n")
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)

    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"Failed to parse bb output as JSON: {e}") from e


def _get_perf_dataset(properties: dict[str, Any]) -> tuple[str, str] | None:
    """Returns the (CAS instance, digest) of a build's aggregated perf dataset.

    Returns None if the build did not publish an aggregated dataset.
    """
    digest = properties.get("perf_dataset_digest") or properties.get(
        "perfcompare_dataset_digest"
    )
    if not digest:
        return None
    return properties["cas_instance"], digest


def _download_from_cas(
    cas_tool: pathlib.Path,
    instance: str,
    digest: str,
    out_dir: str,
    out_fh: TextIO,
) -> None:
    """Uses the hermetic `cas` binary to download the specified digest."""
    cmd = [
        cas_tool,
        "download",
        "-digest",
        digest,
        "-cas-instance",
        instance,
        "-dir",
        out_dir,
    ]
    out_fh.write(f"Downloading from cas to {out_dir}...\n")
    subprocess.run(cmd, check=True)


def _extract_task_outputs_from_steps(
    steps: Collection[dict[str, Any]],
) -> list[tuple[str, str]]:
    """Extracts CAS (instance, digest) pairs of test task outputs.

    Returns them sorted so that boot directories are numbered
    deterministically. A task linked from more than one step is returned
    once, so that its results are not counted twice.
    """
    return sorted(
        {
            (match.group(1), match.group(2))
            for step in steps
            for match in _TASK_OUTPUTS_LINK_REGEX.finditer(
                step.get("summaryMarkdown", "")
            )
        }
    )


def _download_task_results(
    cas_tool: pathlib.Path,
    build_id: str,
    steps: Collection[dict[str, Any]],
    failed_tests: Set[str],
    out_dir: str,
    out_fh: TextIO,
) -> None:
    """Downloads perf results from a build's test tasks into out_dir/by_boot.

    This reproduces the layout of the aggregated perf dataset: each task that
    produced perf results gets its own boot directory, holding only its
    *.fuchsiaperf.json files from passing tests. Other task outputs (traces,
    logs, snapshots) can be gigabytes per task and are not downloaded.
    """
    task_outputs = _extract_task_outputs_from_steps(steps)
    if not task_outputs:
        raise RuntimeError(
            f"No task outputs found in build steps for {build_id}"
        )

    by_boot_dir = os.path.join(out_dir, "by_boot")
    # Check this before downloading anything, rather than letting the
    # os.rename() below fail after all the downloads.
    if os.path.lexists(by_boot_dir):
        raise RuntimeError(
            f"{by_boot_dir} already exists. "
            "Use an empty output directory to avoid mixing results."
        )

    out_fh.write(
        f"Listing perf results in {len(task_outputs)} task outputs...\n"
    )
    results_per_task = _parallel_map(
        lambda task: _list_perf_results(
            cas_tool, task[0], task[1], failed_tests
        ),
        task_outputs,
    )
    boots = [
        (instance, results)
        for (instance, _), results in zip(task_outputs, results_per_task)
        if results
    ]
    if not boots:
        raise RuntimeError(
            f"No {_PERF_RESULTS_SUFFIX} files found in task outputs for "
            f"build {build_id}"
        )

    # Download into a staging directory so that a failed download does not
    # leave an incomplete by_boot directory that looks like a valid dataset.
    with tempfile.TemporaryDirectory(
        prefix=".by_boot.", dir=out_dir
    ) as staging_dir:
        staged_by_boot_dir = os.path.join(staging_dir, "by_boot")
        os.mkdir(staged_by_boot_dir)
        downloads = []
        for boot_idx, (instance, results) in enumerate(boots):
            boot_dir = os.path.join(staged_by_boot_dir, f"boot{boot_idx:06d}")
            for rel_path, digest in results:
                downloads.append(
                    (instance, digest, os.path.join(boot_dir, rel_path))
                )
        out_fh.write(
            f"Downloading {len(downloads)} perf results files from "
            f"{len(boots)} tasks to {by_boot_dir}...\n"
        )
        _parallel_map(
            lambda download: _download_file_from_cas(
                cas_tool, download[0], download[1], download[2]
            ),
            downloads,
        )
        os.rename(staged_by_boot_dir, by_boot_dir)


def _list_perf_results(
    cas_tool: pathlib.Path,
    instance: str,
    digest: str,
    failed_tests: Set[str],
) -> list[tuple[str, str]]:
    """Lists the perf results files in a task's CAS output tree.

    Returns sorted (path, file digest) pairs, with paths relative to the
    task's results directory, excluding results from failed_tests.
    """
    cmd = [cas_tool, "tree", "-cas-instance", instance, "-digest", digest]
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    tree = json.loads(result.stdout)
    # testrunner names each test's output directory after the test with
    # colons removed and then URL-escaped (see Record() in
    # //tools/testing/testrunner/outputs.go). Unescaping cannot restore the
    # colons, so remove them from the failed test names before comparing.
    failed_tests_munged = {name.replace(":", "") for name in failed_tests}
    results = []
    for path, entry in tree["files"].items():
        if not path.endswith(_PERF_RESULTS_SUFFIX):
            continue
        rel_path = path.removeprefix(_TASK_RESULTS_PREFIX)
        test_name = urllib.parse.unquote(rel_path.split("/", 1)[0])
        if test_name not in failed_tests_munged:
            results.append((rel_path, entry["digest"]))
    return sorted(results)


def _download_file_from_cas(
    cas_tool: pathlib.Path, instance: str, digest: str, dest: str
) -> None:
    """Uses the hermetic `cas` binary to download a single file."""
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    cmd = [
        cas_tool,
        "download",
        "-cas-instance",
        instance,
        "-digest",
        digest,
        "-file",
        dest,
    ]
    subprocess.run(cmd, check=True)


def _parallel_map(fn: Callable[[T], R], items: Collection[T]) -> list[R]:
    """Returns [fn(item) for item in items], running calls concurrently.

    If any call raises, calls that have not started are cancelled, and the
    exception is re-raised once the running calls finish.
    """
    with concurrent.futures.ThreadPoolExecutor(_MAX_CAS_JOBS) as executor:
        futures = [executor.submit(fn, item) for item in items]
        try:
            for future in concurrent.futures.as_completed(futures):
                future.result()
            return [future.result() for future in futures]
        finally:
            for future in futures:
                future.cancel()


def _get_build_status(bb_tool: pathlib.Path, build_id: str) -> str:
    """Uses the `bb` tool to get the status of a build."""
    build_id = _normalize_build_id(build_id)
    cmd = [bb_tool, "get", "-json", build_id]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        build_info = json.loads(result.stdout)
        return build_info.get("status", "UNKNOWN")
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return "ERROR"


def _check_bb_auth(bb_tool: pathlib.Path) -> None:
    """Checks if the user is authenticated with buildbucket."""
    result = subprocess.run(
        [bb_tool, "auth-info"], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(
            'Error: "bb auth-info" returned an error, indicating that you are '
            "not authenticated with Buildbucket.\n"
            f"Please run: {bb_tool} auth-login"
        )


def _find_bb_tool() -> pathlib.Path:
    """Finds the path to the hermetic `bb` binary in the Fuchsia checkout."""
    fuchsia_dir = _ensure_fuchsia_dir()
    bb_path = fuchsia_dir / "prebuilt" / "tools" / "buildbucket" / "bb"

    if not bb_path.exists():
        raise RuntimeError(
            f"Could not find `bb` tool at expected location: {bb_path}"
        )

    return bb_path


def _find_cas_tool() -> pathlib.Path:
    """Finds the path to the hermetic `cas` binary in the Fuchsia checkout."""
    fuchsia_dir = _ensure_fuchsia_dir()
    cas_path = fuchsia_dir / "prebuilt" / "tools" / "cas" / "cas"

    if not cas_path.exists():
        raise RuntimeError(
            f"Could not find `cas` tool at expected location: {cas_path}"
        )

    return cas_path


def _ensure_fuchsia_dir() -> pathlib.Path:
    fuchsia_dir = os.environ.get("FUCHSIA_DIR")
    if not fuchsia_dir:
        raise RuntimeError(
            "FUCHSIA_DIR environment variable is not set. Are you running via `fx`?"
        )
    return pathlib.Path(fuchsia_dir)
