# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utility functions for working with Bazel test targets."""

import copy
import dataclasses
import json
import os
import re
import tempfile
import typing as T
from pathlib import Path

from build.bazel.scripts import (
    bazel_action_utils,
    bazel_test_packages,
    build_utils,
    workspace_utils,
)
from build.bazel.scripts.build_utils import BazelLauncher, BazelPaths
from build.python.modules.bazel_labels import (
    expand_shorthand_label,
    is_target_pattern,
    normalize_label,
)

_SCRIPT_DIR = Path(__file__).parent

# A single tests.json entry.
TestSpec = dict[str, T.Any]


class TestDiscoveryError(Exception):
    """A problem with the labels passed to `discover_tests()`.

    Callers show these to the user without a traceback, since they're caused
    by bad input (e.g. a typo in a label) rather than a bug in this script.
    """


def _bazel_error_lines(stderr: str) -> str:
    """Return only the `ERROR:` lines from Bazel's stderr, without the prefix.

    Bazel's stderr also contains progress and warning lines (e.g. "Loading: 0
    packages loaded") that just bury the actual problem. The prefix is
    dropped because callers like `fx test` add their own.
    """
    errors = [
        line.removeprefix("ERROR:").strip()
        for line in stderr.splitlines()
        if line.startswith("ERROR:")
    ]
    # Fall back to the full output so that an unexpected failure format
    # doesn't produce an empty error message.
    return "\n".join(errors) if errors else stderr.strip()


@dataclasses.dataclass(frozen=True)
class BazelTestsJson:
    """tests.json entries generated from Bazel test targets."""

    # Tests that aren't in any `product_bundle_test_group()`.
    tests: list[TestSpec]

    # Maps each extra suite file (e.g. one per `product_bundle_test_group()`)
    # to the device tests reachable from the suites it lists.
    grouped_tests: dict[Path, list[TestSpec]]

    # Paths whose changes require regenerating tests.json.
    inputs: set[Path]


def generate_tests_json(
    bazel_paths: BazelPaths,
    command_runner: build_utils.CommandRunner | None = None,
    quiet: bool = True,
    extra_device_suite_files: T.Sequence[Path] = (),
) -> BazelTestsJson:
    """Generate tests.json entries corresponding to all Bazel test targets.

    Args:
        bazel_paths: The BazelPaths object to use for path resolution.
        command_runner: An optional CommandRunner instance.
        quiet: Whether to print status updates.
        extra_device_suite_files: Additional files listing `bazel_test_suite()`
            `target_tests` labels (e.g. one per `product_bundle_test_group()`).

    Returns:
        The host and device tests.json entries, with `grouped_tests` keyed by
        the paths in `extra_device_suite_files`.
    """
    if not command_runner:
        command_runner = build_utils.CommandRunner()

    # `fx build --fuchsia_platform <label>` runs `bazel build` outside of Ninja
    # and points @gn_targets at this directory (see tools/devshell/build), so
    # it has to exist even if the GN graph has no Bazel device tests.
    # Otherwise that command would fail for any target that uses cmc or
    # package-tool, e.g. any `fx_package()`.
    #
    # Point the workspace's `@gn_targets` symlink at this directory before
    # running either `cquery` (and leave it pointing there afterward) so that
    # Bazel does not see the `@gn_targets` local_path_override change between
    # the host and device queries or across consecutive regenerations.
    gn_targets_dir = _write_target_tests_gn_targets_dir(bazel_paths)
    bazel_action_utils.update_gn_targets_symlink(bazel_paths, gn_targets_dir)

    host_tests_json, host_inputs = _generate_host_tests_json(
        bazel_paths, command_runner, quiet
    )
    device_tests = _generate_device_tests_json(
        bazel_paths, command_runner, quiet, extra_device_suite_files
    )
    return BazelTestsJson(
        tests=host_tests_json + device_tests.tests,
        grouped_tests=device_tests.grouped_tests,
        inputs=host_inputs | device_tests.inputs,
    )


def _execroot_path_to_ninja_path(bazel_paths: BazelPaths, path: str) -> str:
    """Convert a path relative to the Bazel execroot to one relative to the Ninja build directory."""
    return os.path.relpath(
        bazel_paths.execroot / path, bazel_paths.ninja_build_dir
    )


# LINT.IfChange(debug_symbols)
def _debug_manifest_entry(
    bazel_paths: BazelPaths,
    label: str,
    cpu: str,
    os_val: str,
    unstripped_binary_execroot_path: str,
) -> dict[str, str]:
    """Create a debug manifest entry for a Bazel test binary.

    This is a simple wrapper function to avoid nested IFTTT.
    """
    return {
        "cpu": cpu,
        "debug": _execroot_path_to_ninja_path(
            bazel_paths, unstripped_binary_execroot_path
        ),
        "label": label,
        "os": os_val.lower(),
    }


# LINT.ThenChange(//BUILD.gn:debug_symbols)


class StarlarkCqueryError(RuntimeError):
    """A `cquery --output=starlark` invocation failed."""

    def __init__(self, stderr: str) -> None:
        super().__init__(f"Failed to run bazel query: {stderr}")
        self.stderr = stderr


def _check_starlark_cquery_result(ret: build_utils.CommandResult) -> None:
    """Raise if a `cquery --output=starlark` invocation failed.

    When the Starlark output function fails for a target (e.g. because it
    reads a provider field that wasn't set), Bazel logs an error, emits no
    output line for that target, and still exits 0. Without this check, such
    targets would silently disappear from tests.json.
    """
    if ret.returncode != 0 or "ERROR: Starlark evaluation error" in ret.stderr:
        raise StarlarkCqueryError(ret.stderr)


def _run_starlark_cquery(
    bazel_launcher: BazelLauncher,
    config_flag: str,
    starlark_input: Path,
    query_expr: str,
    extra_args: T.Sequence[str] = (),
) -> build_utils.CommandResult:
    """Run a `--output=starlark` cquery from a temp file and check its result.

    Args:
        bazel_launcher: The BazelLauncher to run the cquery with.
        config_flag: The `--config` flag to configure targets with, e.g.
            `--config=host`.
        starlark_input: The .cquery file that formats each target.
        query_expr: The query expression, e.g. `set(//src/foo:bar_test)`.
        extra_args: Other arguments to pass to the cquery.

    Returns:
        The result of the cquery.

    Raises:
        StarlarkCqueryError: The cquery or its output function failed.
    """
    with tempfile.NamedTemporaryFile(mode="w") as query_file:
        # Expressions can list hundreds or thousands of labels, so pass them
        # in a file to avoid hitting the command-line length limit.
        query_file.write(query_expr)
        query_file.flush()
        ret = bazel_launcher.run_query(
            "cquery",
            [
                config_flag,
                *extra_args,
                "--output=starlark",
                f"--starlark:file={starlark_input}",
                f"--query_file={query_file.name}",
            ],
            False,
        )
    _check_starlark_cquery_result(ret)
    return ret


def _query_tests(
    bazel_launcher: BazelLauncher, labels: T.Sequence[str]
) -> set[str]:
    """Return the normalized labels of the tests in `tests(set(<labels>))`."""
    with tempfile.NamedTemporaryFile(mode="w") as query_file:
        # Groups can list hundreds of labels, so pass the expression in a
        # file to avoid hitting the command-line length limit.
        query_file.write("tests(set(" + " ".join(labels) + "))")
        query_file.flush()
        ret = bazel_launcher.run_query(
            "query", [f"--query_file={query_file.name}"], False
        )
    if ret.returncode != 0:
        raise RuntimeError(f"Failed to run bazel query: {ret.stderr}")
    return {
        normalize_label(stripped)
        for line in ret.stdout.splitlines()
        if (stripped := line.strip())
    }


def _generate_host_tests_json(
    bazel_paths: BazelPaths,
    command_runner: build_utils.CommandRunner | None = None,
    quiet: bool = True,
) -> tuple[list[TestSpec], set[Path]]:
    """Generate a tests.json file corresponding to all Bazel host test targets

    Args:
        bazel_paths: The BazelPaths object to use for path resolution.
        command_runner: An optional CommandRunner instance.
        quiet: Whether to print status updates.

    Returns:
        A pair of two values which are:

        - A list of dictionaries, describing each Bazel host_test() reachable
          from the root_host_targets, according to the tests.json schema.

        - A set of input paths, whose changes would require a regeneration of
          the tests.json file.
    """
    if not command_runner:
        command_runner = build_utils.CommandRunner()

    bazel_launcher = BazelLauncher(bazel_paths.launcher, runner=command_runner)
    starlark_input = _SCRIPT_DIR / "../starlark/FuchsiaHostTestInfo.cquery"

    # Read the text file enumerating all the Bazel targets listed in
    # `bazel_test_suite` GN targets.
    bazel_host_test_suites_file = (
        bazel_paths.ninja_build_dir / "bazel_host_test_suites.txt"
    )

    # LINT.IfChange(bazel_host_tests_debug_symbols_json)
    bazel_host_tests_debug_manifest = (
        bazel_paths.ninja_build_dir / "bazel_host_tests.debug_symbols.json"
    )
    # LINT.ThenChange(//build/api/build_api_filter.py:bazel_host_tests_debug_symbols_json)

    # We must guarantee this file exists on disk even if no Bazel tests are discovered.
    # GN's metadata phase unconditionally outputs a pointer to this file in debug_symbols.json,
    # and LastBuildApiFilter propagates it. Initializing it with an empty structure prevents
    # FileNotFoundError in downstream infra scripts.
    bazel_host_tests_debug_manifest.write_text("[]\n")

    suites = bazel_host_test_suites_file.read_text().splitlines()
    if not suites:
        # Skip running `bazel cquery` to get the full list of tests if no Bazel
        # test suites are included in the build graph, to save time on regen.
        return [], {starlark_input}

    if not quiet:
        print(
            f"Running Bazel cquery to populate `tests.json` because there are Bazel tests "
            f"({len(suites)} bazel_test_suite{'' if len(suites) == 1 else 's'}) in your GN graph."
        )
    ret = _run_starlark_cquery(
        bazel_launcher,
        "--config=host",
        starlark_input,
        "tests(set(" + " ".join(suites) + "))",
    )

    (
        tests_json,
        host_test_debug_manifests,
        targets_missing_test_info,
        incompatible_targets,
    ) = _parse_host_cquery_output(bazel_paths, ret.stdout)
    targets_missing_test_info |= incompatible_targets

    # Write out the top-level debug_symbols.json manifest for all Bazel host
    # tests so GN debug_symbol_manifests metadata on //:bazel_host_test_suites
    # can reference it.
    bazel_host_tests_debug_manifest.write_text(
        json.dumps(host_test_debug_manifests, indent=2) + "\n"
    )

    if targets_missing_test_info:
        if len(targets_missing_test_info) == 1:
            raise RuntimeError(
                f"Target '{next(iter(targets_missing_test_info))}' included in the bazel_host_test_suites GN group is a test target "
                f"but does not provide FuchsiaHostTestInfo. "
                f"Wrap it with host_cc_test(), host_go_test(), host_rustc_test(), host_py_test(), or host_test()."
            )
        else:
            targets_list = "\n".join(
                f"  - {t}" for t in sorted(targets_missing_test_info)
            )
            raise RuntimeError(
                f"The following targets included in the bazel_host_test_suites GN group are test targets "
                f"but do not provide FuchsiaHostTestInfo:\n{targets_list}\n"
                f"Wrap them with host_cc_test(), host_go_test(), host_rustc_test(), host_py_test(), or host_test()."
            )

    return tests_json, {starlark_input}


def _parse_host_cquery_output(
    bazel_paths: BazelPaths, stdout: str
) -> tuple[list[TestSpec], list[dict[str, str]], set[str], set[str]]:
    """Parse the output of a host `cquery` using FuchsiaHostTestInfo.cquery.

    Returns:
        The tests.json entries, the debug symbol manifest entries, the labels
        of test targets that don't provide FuchsiaHostTestInfo, and the labels
        of targets marked incompatible with the host platform.
    """
    target_cpu = "x64"
    args_json_path = bazel_paths.ninja_build_dir / "args.json"
    if args_json_path.exists():
        args_json = json.loads(args_json_path.read_text())
        if "target_cpu" in args_json:
            target_cpu = args_json["target_cpu"]

    tests_json: list[TestSpec] = []
    host_test_debug_manifests: list[dict[str, str]] = []
    targets_missing_test_info: set[str] = set()
    incompatible_targets: set[str] = set()

    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue

        # The line is a JSON-encoded object that follows the tests.json schema with
        # the following exceptions:
        #  - The 'bazel_execroot_path' and 'bazel_execroot_runtime_deps_path' fields
        #    are present instead of 'path' and 'runtime_deps_path', and they contain
        #    paths relative to the Bazel execroot instead of the Ninja build directory.
        cquery_test = json.loads(line)

        if cquery_test.get("incompatible"):
            incompatible_targets.add(
                normalize_label(cquery_test.get("label", "unknown"))
            )
            continue

        if "error" in cquery_test:
            if cquery_test["error"] != "missing_fuchsia_host_test_info":
                raise RuntimeError(
                    f"Unexpected error in cquery output: {cquery_test}"
                )
            targets_missing_test_info.add(
                normalize_label(cquery_test.get("label", "unknown"))
            )
            continue

        # LINT.IfChange(cquery_output_schema)
        label = cquery_test["label"]
        cpu_map = {"x86_64": "x64", "aarch64": "arm64"}
        cpu = cpu_map.get(cquery_test["cpu"], cquery_test["cpu"])
        os_val = (
            cquery_test["os"].capitalize() if cquery_test["os"] else "Linux"
        )

        test_spec: TestSpec = {
            "environments": [],
            "expects_ssh": False,
            "test": {
                "name": normalize_label(label),
                "label": label,
                # The source label indicates the location in the tree of the
                # source code. For labels in the main workspace, ensure they
                # start with "//".
                "source_label": normalize_label(label),
                "path": _execroot_path_to_ninja_path(
                    bazel_paths, cquery_test["launcher_execroot_path"]
                ),
                "runtime_deps": _execroot_path_to_ninja_path(
                    bazel_paths, cquery_test["runtime_deps_json_execroot_path"]
                ),
                "os": cquery_test["os"],
                "cpu": cquery_test["cpu"],
            },
        }

        # Only run host tests in infra on x64, because most host tests are for
        # host tools that never need to run on arm64, so it would be wasteful to
        # run them on arm64.
        # TODO(https://fxbug.dev/542710387): Make this more flexible to support
        # running host tests on arm64 on an opt-in basis.
        if target_cpu == "x64":
            test_spec["environments"].append(
                {"dimensions": {"os": os_val, "cpu": cpu}}
            )

        if cquery_test["list_cases_argument"]:
            assert isinstance(test_spec["test"], dict)  # make mypy happy
            test_spec["test"]["list_cases_argument"] = cquery_test[
                "list_cases_argument"
            ]

        if cquery_test.get("unstripped_binary_execroot_path"):
            host_test_debug_manifests.append(
                _debug_manifest_entry(
                    bazel_paths,
                    label,
                    cpu,
                    os_val,
                    cquery_test["unstripped_binary_execroot_path"],
                )
            )

        tests_json.append(test_spec)
        # LINT.ThenChange(//build/bazel/starlark/FuchsiaHostTestInfo.cquery:cquery_output_schema)

    return (
        tests_json,
        host_test_debug_manifests,
        targets_missing_test_info,
        incompatible_targets,
    )


def _generate_device_tests_json(
    bazel_paths: BazelPaths,
    command_runner: build_utils.CommandRunner,
    quiet: bool = True,
    extra_suite_files: T.Sequence[Path] = (),
) -> BazelTestsJson:
    """Generate tests.json entries for all Bazel fx_test() targets.

    Also writes `bazel_test_packages.list`, which enumerates the package
    manifests that must be published before the tests can run.

    Args:
        bazel_paths: The BazelPaths object to use for path resolution.
        command_runner: The CommandRunner instance to run `bazel` with.
        quiet: Whether to print status updates.
        extra_suite_files: Additional files listing `bazel_test_suite()`
            `target_tests` labels (e.g. one per `product_bundle_test_group()`).

    Returns:
        The device tests.json entries, one per test component, with
        `grouped_tests` keyed by the paths in `extra_suite_files`.
    """
    bazel_launcher = BazelLauncher(bazel_paths.launcher, runner=command_runner)
    starlark_input = _SCRIPT_DIR / "../starlark/FuchsiaTestInfo.cquery"

    # Read the text files enumerating ungrouped, per-group, and all built
    # Bazel target test suites. `bazel_target_test_suites_to_build.txt`
    # includes suites behind a non-PB `tests_barrier` (such as
    # `//bundles/buildbot/fuchsia:additional_build_targets`) that are built by
    # `//build/bazel/target_tests` and still need their package manifests in
    # `bazel_test_packages.list` and their BUILD files tracked as regen inputs.
    bazel_test_suites_file = (
        bazel_paths.ninja_build_dir / "ungrouped_bazel_target_test_suites.txt"
    )
    all_bazel_test_suites_file = (
        bazel_paths.ninja_build_dir / "bazel_target_test_suites_to_build.txt"
    )
    top_level_suites = tuple(bazel_test_suites_file.read_text().splitlines())
    all_build_suites = (
        tuple(all_bazel_test_suites_file.read_text().splitlines())
        if all_bazel_test_suites_file.exists()
        else ()
    )
    extra_suites_by_file = {
        suite_file: tuple(suite_file.read_text().splitlines())
        for suite_file in extra_suite_files
    }
    unique_suite_tuples = list(
        dict.fromkeys(
            suites
            for suites in [top_level_suites, *extra_suites_by_file.values()]
            if suites
        )
    )
    all_suites = list(
        dict.fromkeys(
            suite
            for suites in [*unique_suite_tuples, all_build_suites]
            for suite in suites
        )
    )

    # LINT.IfChange(bazel_device_tests_debug_symbols_json)
    bazel_device_tests_debug_manifest = (
        bazel_paths.ninja_build_dir / "bazel_device_tests.debug_symbols.json"
    )
    # LINT.ThenChange(//build/bazel/scripts/copy_bazel_debug_symbols.py:bazel_device_tests_debug_symbols_json)

    if not all_suites:
        # Skip running `bazel cquery` to get the full list of tests if no Bazel
        # device tests are included in the build graph, to save time on regen.
        bazel_device_tests_debug_manifest.write_text("[]\n")
        write_bazel_test_packages_list(bazel_paths.ninja_build_dir, [])
        return BazelTestsJson(
            tests=[],
            grouped_tests={suite_file: [] for suite_file in extra_suite_files},
            inputs={starlark_input},
        )

    if not quiet:
        print(
            f"Running Bazel cquery to populate `tests.json` because there are Bazel device "
            f"tests ({len(all_suites)} bazel_test_suite{'' if len(all_suites) == 1 else 's'}) "
            f"in your GN graph."
        )

    # TODO(https://fxbug.dev/564574581): `tests()` silently discards
    # non-test targets, so e.g. a bare `fx_package()` listed in
    # `target_tests` is dropped from tests.json instead of hitting the
    # "Wrap them with fx_test()" error below. Report requested labels
    # that are neither `test_suite()`s nor matched by `tests()`. The
    # host test path above has the same gap.
    ret = _run_starlark_cquery(
        bazel_launcher,
        "--config=fuchsia_platform",
        starlark_input,
        "tests(set(" + " ".join(all_suites) + "))",
    )

    (
        tests_by_label,
        package_manifests,
        targets_missing_test_info,
        incompatible_targets,
        device_test_debug_manifests,
    ) = _parse_device_cquery_output(bazel_paths, ret.stdout)
    targets_missing_test_info |= incompatible_targets
    all_tests_json = [
        test for tests in tests_by_label.values() for test in tests
    ]

    if targets_missing_test_info:
        targets_list = "\n".join(
            f"  - {t}" for t in sorted(targets_missing_test_info)
        )
        raise RuntimeError(
            f"The following targets included in the `target_tests` of a bazel_test_suite() "
            f"GN target are test targets but do not provide FuchsiaTestInfo:\n"
            f"{targets_list}\n"
            f"Wrap them with fx_test()."
        )

    bazel_device_tests_debug_manifest.write_text(
        json.dumps(device_test_debug_manifests, indent=2) + "\n"
    )

    write_bazel_test_packages_list(
        bazel_paths.ninja_build_dir, sorted(set(package_manifests))
    )

    # Without these, adding an `fx_test()` to a suite would leave tests.json and
    # bazel_test_packages.list stale until something else triggers `fx gen`.
    # TODO(https://fxbug.dev/564574581): This misses edits in other packages,
    # e.g. to an `fx_package()`'s `test_components` defined in a different
    # BUILD file than its `fx_test()`.
    build_files = _main_workspace_build_files(
        bazel_paths.fuchsia_dir,
        all_suites + list(tests_by_label.keys()),
    )

    all_suites_set = {normalize_label(s) for s in all_suites}
    tests_by_suites: dict[tuple[str, ...], list[TestSpec]] = {}
    for suites in unique_suite_tuples:
        normalized_suites = {normalize_label(s) for s in suites}
        if normalized_suites == all_suites_set:
            tests_by_suites[suites] = all_tests_json
            continue

        if all(s in tests_by_label for s in normalized_suites):
            # `bazel_test_suite()` lists `fx_test()` labels directly, so
            # there's nothing to expand.
            suite_test_labels = normalized_suites
        else:
            # Expand the whole group in one loading-phase `bazel query`
            # rather than one per `test_suite()`, because each Bazel
            # invocation has a fixed overhead and groups can contain dozens
            # of suites.
            suite_test_labels = _query_tests(bazel_launcher, suites)

        # Keep the cquery's order, which is also the order used when a group
        # includes every suite.
        tests_by_suites[suites] = [
            test
            for label, tests in tests_by_label.items()
            if label in suite_test_labels
            for test in tests
        ]

    # Return independent copies per suite file so in-place mutation of test
    # entries by `build_tests_json.py` (for environment resolution and
    # product_bundle_name suffixing) does not bleed across groups.
    top_level_tests = copy.deepcopy(tests_by_suites.get(top_level_suites, []))
    extra_tests_by_file = {
        suite_file: copy.deepcopy(tests_by_suites.get(suites, []))
        for suite_file, suites in extra_suites_by_file.items()
    }

    return BazelTestsJson(
        tests=top_level_tests,
        grouped_tests=extra_tests_by_file,
        inputs={starlark_input} | build_files,
    )


def _parse_device_cquery_output(
    bazel_paths: BazelPaths, stdout: str
) -> tuple[
    dict[str, list[TestSpec]],
    list[str],
    set[str],
    set[str],
    list[dict[str, str]],
]:
    """Parse the output of a device `cquery` using FuchsiaTestInfo.cquery.

    Returns:
        The tests.json entries keyed by normalized `fx_test()` label (one
        entry per test component), the package manifests that must be
        published to run them, the labels of test targets that don't
        provide FuchsiaTestInfo, the labels of targets marked incompatible
        with the Fuchsia platform, and the debug manifest entries.
    """
    tests_by_label: dict[str, list[TestSpec]] = {}
    package_manifests: list[str] = []
    targets_missing_test_info: set[str] = set()
    incompatible_targets: set[str] = set()
    device_test_debug_manifests: list[dict[str, str]] = []
    seen_unstripped_paths: set[str] = set()

    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue

        cquery_test = json.loads(line)

        if cquery_test.get("incompatible"):
            incompatible_targets.add(
                normalize_label(cquery_test.get("label", "unknown"))
            )
            continue

        if "error" in cquery_test:
            if cquery_test["error"] != "missing_fuchsia_test_info":
                raise RuntimeError(
                    f"Unexpected error in cquery output: {cquery_test}"
                )
            targets_missing_test_info.add(
                normalize_label(cquery_test.get("label", "unknown"))
            )
            continue

        # LINT.IfChange(device_cquery_output_schema)
        label = cquery_test["label"]
        normalized_label = normalize_label(label)
        # Point consumers at the copy of the package that //build/bazel/target_tests
        # exports to the Ninja build directory, rather than at the Bazel
        # output base, where a later Bazel invocation may delete its files.
        package_manifest = bazel_test_packages.export_path_for_manifest(
            cquery_test["package_manifest_execroot_path"]
        )
        package_manifests.append(package_manifest)

        # Unlike the host path above, no CPU remapping is needed here:
        # `CurrentPlatformInfo` already reports Fuchsia's names (x64, arm64,
        # riscv64) rather than Bazel's (k8, aarch64).
        #
        # An empty `environments` list means build_tests_json.py fills in
        # this build's default environments.
        envs = cquery_test.get("environments", [])
        target_tests_json: list[TestSpec] = []
        for test_component in cquery_test["test_components"]:
            package_url = test_component["package_url"]
            test_entry: dict[str, T.Any] = {
                "build_rule": "fx_test",
                "cpu": cquery_test["cpu"],
                "label": label,
                # The source label indicates the location in the tree of
                # the source code. For labels in the main workspace,
                # ensure they start with "//".
                "source_label": normalized_label,
                "name": package_url,
                "os": cquery_test["os"],
                "package_url": package_url,
                "package_manifests": [package_manifest],
                # TODO(https://fxbug.dev/564574581): Support overriding
                # other test spec fields.
                "log_settings": {
                    "max_severity": cquery_test.get("max_log_severity", "WARN")
                },
            }
            if cquery_test.get("min_log_severity"):
                test_entry["log_settings"]["min_severity"] = cquery_test[
                    "min_log_severity"
                ]
            if test_component.get("realm"):
                test_entry["realm"] = test_component["realm"]
            if cquery_test.get("create_no_exception_channel"):
                test_entry["create_no_exception_channel"] = True
            if cquery_test.get("parallel"):
                test_entry["parallel"] = cquery_test["parallel"]
            test_dict: TestSpec = {
                "expects_ssh": True,
                "test": test_entry,
            }
            if cquery_test.get("build_only"):
                test_dict["build_only"] = True
            else:
                test_dict["environments"] = envs
            target_tests_json.append(test_dict)

        for unstripped_path in cquery_test.get(
            "unstripped_binary_execroot_paths", []
        ):
            if unstripped_path not in seen_unstripped_paths:
                seen_unstripped_paths.add(unstripped_path)
                device_test_debug_manifests.append(
                    _debug_manifest_entry(
                        bazel_paths,
                        label,
                        cquery_test["cpu"],
                        cquery_test["os"],
                        unstripped_path,
                    )
                )
        # LINT.ThenChange(//build/bazel/starlark/FuchsiaTestInfo.cquery:cquery_output_schema)
        tests_by_label[normalized_label] = target_tests_json

    return (
        tests_by_label,
        package_manifests,
        targets_missing_test_info,
        incompatible_targets,
        device_test_debug_manifests,
    )


# Rule kinds of the targets that provide FuchsiaTestInfo or
# FuchsiaHostTestInfo.
# LINT.IfChange(test_rule_kinds)
_DEVICE_TEST_RULE_KINDS = ("fx_test",)
_HOST_TEST_RULE_KINDS = ("host_test",)
# LINT.ThenChange(
#     //build/bazel/rules/testing/fx_test.bzl:fx_test_rule_kind,
#     //build/bazel/rules/host_tests/host_test.bzl:host_test_rule_kind,
# )

_UNQUOTED_QUERY_WORD = re.compile(r"^[A-Za-z0-9_/.:@-]+$")


def _set_expr(words: T.Sequence[str]) -> str:
    """Build a Bazel query `set(...)` expression, quoting words when needed."""
    quoted = (
        w
        if _UNQUOTED_QUERY_WORD.match(w)
        else '"' + w.replace("\\", "\\\\").replace('"', '\\"') + '"'
        for w in words
    )
    return f"set({' '.join(quoted)})"


def _run_test_info_cquery(
    bazel_launcher: BazelLauncher,
    config_flag: str,
    starlark_input: Path,
    query_expr: str,
    extra_args: T.Sequence[str],
) -> build_utils.CommandResult:
    """Like `_run_starlark_cquery()`, but for `discover_tests()`.

    Analysis errors in a test (e.g. a `fail()` in its BUILD file) are the
    user's to fix, so they're reported without a traceback.
    """
    try:
        return _run_starlark_cquery(
            bazel_launcher,
            config_flag,
            starlark_input,
            query_expr,
            extra_args=extra_args,
        )
    except StarlarkCqueryError as e:
        raise TestDiscoveryError(_bazel_error_lines(e.stderr)) from e


def discover_tests(
    bazel_paths: BazelPaths,
    labels: T.Sequence[str],
    test_os: T.Literal["host", "fuchsia"] | None = None,
    command_runner: build_utils.CommandRunner | None = None,
) -> list[TestSpec]:
    """Query Bazel for tests by label, formatted like tests.json.

    Unlike `generate_tests_json()`, this doesn't require the tests to be
    reachable from any `bazel_test_suite()`, so `fx test` can run a Bazel
    test that isn't in tests.json. It doesn't write tests.json or
    `bazel_test_packages.list`, so it doesn't change what `fx build` does.

    Args:
        bazel_paths: The BazelPaths object to use for path resolution.
        labels: Bazel labels of the tests, e.g. `//src/foo:bar_test`, or
            `//src/foo` for `//src/foo:foo`.
        test_os: Optional filter ("host" or "fuchsia") so `fx test --host`
            or `--device` only queries and configures matching tests.
        command_runner: An optional CommandRunner instance.

    Returns:
        An entry for each discovered test, in the same format as tests.json
        but without the `environments` that build_tests_json.py would
        normally add.
    """
    if not labels:
        return []
    # TODO(https://fxbug.dev/585649932): Support target patterns.
    for label in labels:
        if is_target_pattern(label):
            raise TestDiscoveryError(
                f"Target patterns like '{label}' aren't supported yet. "
                "Pass individual test labels instead."
            )
    expanded_labels = [expand_shorthand_label(label) for label in labels]
    for label, expanded in zip(labels, expanded_labels):
        # The results of `bazel query` are matched by label, which only works
        # for labels in the main repository written the way it prints them.
        if not expanded.startswith("//") or expanded.endswith(":"):
            raise TestDiscoveryError(
                f"'{label}' must be an absolute label in the main repository, "
                "like //src/foo:bar_test."
            )

    if not command_runner:
        command_runner = build_utils.CommandRunner()
    bazel_launcher = BazelLauncher(bazel_paths.launcher, runner=command_runner)

    # Every Bazel command here, and the `fx build --host` or
    # `fx build --fuchsia_platform` that `fx test` runs afterwards, must
    # override @gn_targets the same way. Otherwise each command that changes
    # the override makes Bazel reload every package that depends on
    # @gn_targets, which made the device cquery about 1.6s slower. Targets in
    # the fuchsia_platform graph can also depend on @gn_targets inputs.
    # Overriding the repository, rather than repointing the workspace symlink,
    # avoids breaking a concurrent `fx build`.
    gn_targets_override = (
        "--override_repository=gn_targets+="
        f"{_write_target_tests_gn_targets_dir(bazel_paths)}"
    )

    # Unconfigured query in strict mode (no --keep_going), so a nonexistent
    # label fails, which is almost always a typo on the command line.
    with tempfile.NamedTemporaryFile(mode="w") as query_file:
        query_file.write(_set_expr(expanded_labels))
        query_file.flush()
        ret = bazel_launcher.run_query(
            "query",
            [
                gn_targets_override,
                "--output=label_kind",
                f"--query_file={query_file.name}",
            ],
            False,
        )
    if ret.returncode != 0:
        raise TestDiscoveryError(_bazel_error_lines(ret.stderr))

    kinds: dict[str, str] = {}
    for line in ret.stdout.splitlines():
        # Each line looks like "fx_test rule //src/foo:bar_test" or
        # "source file //src/foo:bar.cc".
        parts = line.split()
        if len(parts) != 3:
            raise RuntimeError(
                f"Unexpected line in `bazel query --output=label_kind` "
                f"output: {line!r}"
            )
        kinds[normalize_label(parts[2])] = parts[0]

    host_labels: list[str] = []
    device_labels: list[str] = []
    for label, expanded in zip(labels, expanded_labels):
        kind = kinds.get(expanded, "non-test")
        if kind in _DEVICE_TEST_RULE_KINDS and test_os != "host":
            device_labels.append(expanded)
        elif kind in _HOST_TEST_RULE_KINDS and test_os != "fuchsia":
            host_labels.append(expanded)
        elif kind in _DEVICE_TEST_RULE_KINDS:
            raise TestDiscoveryError(f"'{label}' is a device test.")
        elif kind in _HOST_TEST_RULE_KINDS:
            raise TestDiscoveryError(f"'{label}' is a host test.")
        else:
            raise TestDiscoveryError(
                f"'{label}' is a {kind} target, not an fx_test() or host_test()."
            )

    tests: list[TestSpec] = []
    targets_missing_test_info: set[str] = set()
    incompatible_targets: set[str] = set()

    if host_labels:
        ret = _run_test_info_cquery(
            bazel_launcher,
            "--config=host",
            _SCRIPT_DIR / "../starlark/FuchsiaHostTestInfo.cquery",
            _set_expr(host_labels),
            extra_args=(gn_targets_override,),
        )
        host_tests, _, missing, incompatible = _parse_host_cquery_output(
            bazel_paths, ret.stdout
        )
        tests.extend(host_tests)
        targets_missing_test_info |= missing
        incompatible_targets |= incompatible

    if device_labels:
        ret = _run_test_info_cquery(
            bazel_launcher,
            "--config=fuchsia_platform",
            _SCRIPT_DIR / "../starlark/FuchsiaTestInfo.cquery",
            _set_expr(device_labels),
            extra_args=(gn_targets_override,),
        )
        (
            tests_by_label,
            _,
            missing,
            incompatible,
            _,
        ) = _parse_device_cquery_output(bazel_paths, ret.stdout)
        tests.extend(t for ts in tests_by_label.values() for t in ts)
        targets_missing_test_info |= missing
        incompatible_targets |= incompatible

    if targets_missing_test_info:
        targets_list = "\n".join(
            f"  - {t}" for t in sorted(targets_missing_test_info)
        )
        raise RuntimeError(
            f"The following targets are test targets but do not provide "
            f"FuchsiaTestInfo or FuchsiaHostTestInfo:\n{targets_list}"
        )

    for label, expanded in zip(labels, expanded_labels):
        if expanded in incompatible_targets:
            raise TestDiscoveryError(
                f"Target '{label}' is incompatible with the current build "
                "configuration."
            )

    return tests


def _main_workspace_build_files(
    fuchsia_dir: Path, labels: T.Iterable[str]
) -> set[Path]:
    """Return the BUILD files defining the given main-workspace Bazel labels."""
    build_files: set[Path] = set()
    for label in labels:
        # `_normalize_label` strips `@@//` and `@//` to `//` for main-workspace
        # labels while leaving external repo labels (`@repo//...`, `@@repo//...`)
        # prefixed with `@`, which the check below skips.
        label = normalize_label(label)
        if not label.startswith("//"):
            continue
        package_dir = fuchsia_dir / label.removeprefix("//").split(":")[0]
        for name in ("BUILD.bazel", "BUILD"):
            if (package_dir / name).is_file():
                build_files.add(package_dir / name)
                break
    return build_files


# TODO(https://fxbug.dev/519243783, https://fxbug.dev/519244675): Remove
# `_write_target_tests_gn_targets_dir` once `cmc` and `package-tool` are
# migrated to Bazel and `fx_package()` no longer depends on `@gn_targets`.
def _write_target_tests_gn_targets_dir(bazel_paths: BazelPaths) -> Path:
    """Populate `build/bazel/tests_json.gn_targets` from `//build/bazel/{target,host}_tests`."""
    target_infos_file = bazel_paths.ninja_build_dir / "bazel_target_infos.json"
    bazel_target_infos = json.loads(target_infos_file.read_text())
    # LINT.IfChange(target_tests_bazel_target)
    target_tests_label = "//build/bazel/target_tests:target_tests_stamp"
    # LINT.ThenChange(//build/bazel/target_tests/BUILD.gn:target_tests_bazel_target)
    # LINT.IfChange(host_tests_bazel_target)
    host_tests_label = "//build/bazel/host_tests:host_tests_stamp"
    # LINT.ThenChange(//build/bazel/host_tests/BUILD.gn:host_tests_bazel_target)
    target_tests_manifests = {
        bazel_paths.ninja_build_dir / info["gn_targets_manifest"]
        for info in bazel_target_infos
        if info["bazel_target"] == target_tests_label
    }
    if not target_tests_manifests:
        raise RuntimeError(
            f"No entry for {target_tests_label} found in {target_infos_file}"
        )
    host_tests_manifests = {
        bazel_paths.ninja_build_dir / info["gn_targets_manifest"]
        for info in bazel_target_infos
        if info["bazel_target"] == host_tests_label
    }
    manifests = sorted(target_tests_manifests | host_tests_manifests)

    # LINT.IfChange(tests_json_gn_targets_dir)
    gn_targets_dir = (
        bazel_paths.ninja_build_dir / "build/bazel/tests_json.gn_targets"
    )
    # LINT.ThenChange(//tools/devshell/build:tests_json_gn_targets_dir)
    # `record_gn_targets_dir_from_entries()` requires this file to exist and
    # symlinks `all_licenses.spdx.json` to it. Place it outside `gn_targets_dir`
    # so creating it does not make `gn_targets_dir.is_dir()` true before
    # `update_if_needed()` runs.
    licenses_file = Path(f"{gn_targets_dir}.placeholder_licenses.spdx.json")
    licenses_content = (
        "This is a placeholder file, only used to satisfy Bazel analysis of "
        "test packages during `tests.json` generation."
    )
    if not licenses_file.exists():
        licenses_file.parent.mkdir(parents=True, exist_ok=True)
        licenses_file.write_text(licenses_content)

    # Parse `//build/bazel/target_tests`'s manifest into the entry map expected
    # by `record_gn_targets_dir_from_entries()`.
    generated = workspace_utils.GeneratedWorkspaceFiles()
    entries = workspace_utils.merge_gn_target_manifests(manifests)
    workspace_utils.record_gn_targets_dir_from_entries(
        generated,
        bazel_paths.ninja_build_dir,
        entries,
        licenses_file,
    )
    # Only rewrite the directory when its contents change, so consecutive
    # regenerations do not bump `MODULE.bazel`'s mtime and force Bazel to
    # re-run Bzlmod module resolution during `cquery`.
    generated.update_if_needed(
        gn_targets_dir, Path(f"{gn_targets_dir}.generated-info.json")
    )
    return gn_targets_dir


def write_bazel_test_packages_list(
    ninja_build_dir: Path, package_manifests: list[str]
) -> None:
    """Write the list of package manifests for all Bazel test packages.

    The file uses the same schema as `all_package_manifests.list`, and is read
    by `fx build` (via `all_package_manifests.list`) and `fx test` to publish
    Bazel test packages before running them.

    It is written even when there are no Bazel test packages, so that
    downstream consumers can always load it as valid JSON.
    """
    output = ninja_build_dir / bazel_test_packages.PACKAGES_LIST
    content = (
        json.dumps(
            {"content": {"manifests": package_manifests}, "version": "1"},
            indent=2,
        )
        + "\n"
    )
    # Only overwrite when the contents changed so that `fx gen` does not bump
    # the file's mtime and force `all_package_manifests.list` and `amber-files`
    # to rebuild on the next `fx build`.
    if not output.exists() or output.read_text() != content:
        output.write_text(content)
