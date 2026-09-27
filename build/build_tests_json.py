#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Generate tests.json.
"""

import json
import pprint
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent / "bazel/scripts"))
sys.path.insert(0, str(Path(__file__).parent / "python/modules"))
import bazel_tests_utils
import build_utils
from build_utils import CommandRunner
from serialization import (
    JSONValue,
    instance_from_dict,
    instance_to_dict,
    serialize_dict,
)


@serialize_dict
@dataclass(frozen=True)
class Dimensions:
    """Swarming dimensions for a test environment or platform."""

    access_points: str | None = None
    attenuators: str | None = None
    cpu: str | None = None
    device_type: str | None = None
    dimensions: JSONValue | None = None
    host_device_type: str | None = None
    iperf_servers: str | None = None
    os: str | None = None
    pool: str | None = None
    sherlocks: str | None = None
    sorrels: str | None = None
    tags: JSONValue | None = None
    testbed: str | None = None
    vim3s: str | None = None

    def __post_init__(self) -> None:
        if self.tags is not None:
            raise ValueError(
                "tags are only valid in an environments scope, not in dimensions"
            )
        if self.dimensions is not None:
            raise ValueError(
                "found nested dimensions environment field. Did you set "
                + "`dimensions = some_env`? It should be `dimensions = some_env.dimensions`"
            )

    def get(self, key: str, default: str | None = None) -> str | None:
        val: str | None = getattr(self, key, None)
        return val if val is not None else default

    def is_subset_of(self, other: "Dimensions") -> bool:
        """Return True if all non-None dimensions in `self` are present with the same values in `other`.

        `instance_to_dict()` omits fields whose value is `None`, returning a dict of
        only the explicitly specified dimensions. Because `dict.items()` returns a
        set-like `dict_items` view of `(key, value)` pairs, the `<=` operator performs
        a set subset comparison, verifying that every dimension requirement in `self`
        is satisfied by `other` (which may also specify additional dimensions).
        """
        return instance_to_dict(self).items() <= instance_to_dict(other).items()


@serialize_dict
@dataclass(frozen=True)
class EmulatorConfig:
    """Emulator-specific configuration for a test environment."""

    accel: str | None = None
    device: str | None = None
    kernel_args: tuple[str, ...] | None = None
    name: str = ""
    uefi: bool | None = None
    vbmeta_key: str | None = None
    vbmeta_key_metadata: str | None = None

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("The `emulator` scope requires a unique `name`")
        if self.uefi and (not self.vbmeta_key or not self.vbmeta_key_metadata):
            raise ValueError(
                "Emulator environments with `uefi` set to true must provide "
                + "a `vbmeta_key` and `vbmeta_key_metadata`"
            )
        if isinstance(self.kernel_args, list):
            object.__setattr__(self, "kernel_args", tuple(self.kernel_args))


@serialize_dict
@dataclass(frozen=True)
class Environment:
    """Full device environment specification in which a test should run."""

    dimensions: Dimensions = Dimensions()
    emulator: EmulatorConfig | None = None
    netboot: bool | None = None
    service_account: str | None = None
    tags: tuple[str, ...] | None = None

    def __post_init__(self) -> None:
        if not instance_to_dict(self.dimensions):
            raise ValueError("each environment must specify dimensions")
        if isinstance(self.tags, list):
            object.__setattr__(self, "tags", tuple(self.tags))

    def __lt__(self, other: "Environment") -> bool:
        return json.dumps(instance_to_dict(self), sort_keys=True) < json.dumps(
            instance_to_dict(other), sort_keys=True
        )


def partition_platforms(
    platforms: list[dict[str, Any]], target_cpu: str
) -> tuple[set[Dimensions], set[Dimensions]]:
    """Partition platform definitions into target_cpu platforms and other platforms."""
    target_platforms: set[Dimensions] = set()
    other_platforms: set[Dimensions] = set()
    for p_dict in platforms:
        p = instance_from_dict(Dimensions, p_dict)
        if p.cpu is None or p.cpu == target_cpu:
            target_platforms.add(p)
        else:
            other_platforms.add(p)
    return target_platforms, other_platforms


def validate_known_platform(
    env: Environment,
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> None:
    """Validate that env matches at least one known platform (target or other)."""
    if not any(
        env.dimensions.is_subset_of(p)
        for p in (target_platforms | other_platforms)
    ):
        raise ValueError(
            f"Could not match environment specifications: {instance_to_dict(env)}\n"
            + "Consult //build/testing/platforms.gni for all allowable specifications"
        )


def matches_target_platform(
    env: Environment,
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> bool:
    """Validate that env matches a known platform, and return True if it matches target_platforms."""
    if any(env.dimensions.is_subset_of(p) for p in target_platforms):
        return True
    if any(env.dimensions.is_subset_of(p) for p in other_platforms):
        return False
    raise ValueError(
        f"Could not match environment specifications: {instance_to_dict(env)}\n"
        + "Consult //build/testing/platforms.gni for all allowable specifications"
    )


def resolve_test_environments(
    test: dict[str, Any],
    default_envs: list[Environment],
    allowed_device_types: set[str],
    allowed_host_device_types: set[str],
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
    host_env: Environment | None = None,
    restrict_to_default_envs: bool = False,
    override_environments: bool = False,
) -> None:
    """Resolve, validate, filter, and deduplicate environments for a single test spec.

    Mutates `test["environments"]` and `test["build_only"]` in place.
    Raises ValueError if any environment specification is invalid or unknown.
    """
    if test.get("build_only") is True:
        test["environments"] = []
        return

    test_info = test.get("test", {})
    test_specified_envs = test.get("environments")
    candidate_envs: list[Environment] = []

    # If not overriding environments, add the test-specified environments to the candidate list.
    if test_specified_envs and not override_environments:
        for e in test_specified_envs:
            env = instance_from_dict(Environment, e)
            candidate_envs.append(env)

    # Otherwise, use the default environments.
    else:
        if (
            not override_environments
            and host_env is not None
            and test_info.get("os") == "linux"
            and not test.get("expects_ssh", True)
        ):
            test_cpu = test_info.get("cpu")
            if test_cpu and test_cpu != host_env.dimensions.cpu:
                candidate_envs = [
                    Environment(dimensions=Dimensions(os="Linux", cpu=test_cpu))
                ]
            else:
                candidate_envs = [host_env]
        else:
            candidate_envs = list(default_envs)

    # If we're restricting to the default set of environments, then we need to filter
    # them based on what matches.  If not, we use the test's environments that match
    # the target platforms.

    platform_matched_envs: list[Environment] = []
    if restrict_to_default_envs:
        # The environments specified by a test are a set of dimension requirements
        # for a match, rather than an exact match on all Environment fields (such as
        # tags or emulator configurations). A candidate environment matches if its
        # dimensions are a subset of any of the default environments' dimensions.
        for env in candidate_envs:
            validate_known_platform(env, target_platforms, other_platforms)
            if any(
                env.dimensions.is_subset_of(default_env.dimensions)
                for default_env in default_envs
            ):
                platform_matched_envs.append(env)
    else:
        for env in candidate_envs:
            if matches_target_platform(env, target_platforms, other_platforms):
                platform_matched_envs.append(env)

    # Use a set comprehension (not a list) to deduplicate environments, then sort for a stable output order.
    filtered_envs = sorted(
        {
            env
            for env in platform_matched_envs
            if (
                env.dimensions.device_type is None
                or env.dimensions.device_type in allowed_device_types
            )
            and (
                env.dimensions.host_device_type is None
                or env.dimensions.host_device_type in allowed_host_device_types
            )
        }
    )

    if not filtered_envs:
        test["build_only"] = True
        test["environments"] = []
    else:
        test["environments"] = [instance_to_dict(e) for e in filtered_envs]
        if "build_only" in test:
            test["build_only"] = False


def build_tests_json(
    build_dir: Path,
    with_bazel_tests: bool = False,
    command_runner: CommandRunner | None = None,
    quiet: bool = True,
) -> set[Path]:
    """Generate the tests.json file.

    tests.json is created by merging two things:

    1) tests_from_metadata.json
       A collection of test specs found from a GN metadata walk. These test
       specs have their default environments populated, validated against
       platforms, and filtered by allowed device types before being written
       into the final tests.json.

    2) product_bundle_test_groups.json
       A file that declares a mapping of product bundle name to a specific set
       of tests found in another tests.json. These test specs will be modified
       to include `product_bundle: <name>` and filtered/defaulted against the
       group's environments before merging into the final tests.json.

    Args:
        build_dir: Fuchsia build directory.
        with_bazel_tests: Whether to export Bazel tests.
        command_runner: Optional command runner to use for running bazel commands.
        quiet: Whether to print status updates.

    Returns:
        A set of Path values for the input files read by this function.
    """
    tests_json_path = build_dir / "tests.json"

    # Read the list of tests that were collected from a GN metadata walk.
    tests_from_metadata_path = build_dir / "tests_from_metadata.json"
    tests = json.loads(tests_from_metadata_path.read_text())

    # Read the mapping between test sets and product bundle name.
    test_groups_path = (
        build_dir / "obj" / "tests" / "product_bundle_test_groups.json"
    )
    test_groups = json.loads(test_groups_path.read_text())

    # Read the environment constants and platform definitions.
    environments_path = (
        build_dir / "obj" / "tests" / "all_test_environments.json"
    )
    all_test_environments = json.loads(environments_path.read_text())

    # Read the builder-set default environments and allowed device types.
    default_environments_path = (
        build_dir / "obj" / "tests" / "default_test_environments.json"
    )
    default_test_environments = json.loads(
        default_environments_path.read_text()
    )

    # Read the list of product bundles that were collected from a GN metadata
    # walk.
    product_bundles_json_path = build_dir / "product_bundles.json"
    product_bundles = json.loads(product_bundles_json_path.read_text())
    product_bundle_names = [pb["name"] for pb in product_bundles]

    target_cpu: str = default_test_environments["target_cpu"]
    default_envs: list[Environment] = [
        instance_from_dict(Environment, e)
        for e in default_test_environments["default_environments"]
    ]
    allowed_device_types_set: set[str] = set(
        default_test_environments["allowed_device_types"]
    )
    allowed_host_device_types_set: set[str] = set(
        default_test_environments["allowed_host_device_types"]
    )
    host_env: Environment = instance_from_dict(
        Environment, all_test_environments["host_env"]
    )

    target_platforms, other_platforms = partition_platforms(
        all_test_environments["platforms"], target_cpu
    )

    validation_errors: list[str] = []

    # Resolve, validate, and filter environments for tests from metadata.
    for test in tests:
        test_info = test.get("test", {})
        test_name = test_info.get("name", "<unknown>")
        test_label = test_info.get("label")
        test_id = f"{test_name} ({test_label})" if test_label else test_name

        try:
            resolve_test_environments(
                test,
                default_envs=default_envs,
                allowed_device_types=allowed_device_types_set,
                allowed_host_device_types=allowed_host_device_types_set,
                target_platforms=target_platforms,
                other_platforms=other_platforms,
                host_env=host_env,
                restrict_to_default_envs=False,
            )
        except ValueError as err:
            validation_errors.append(f"{test_id}: {err}")

    # For every group of tests that are supposed to target a specific product
    # bundle, we parse the tests, add `product_bundle: <name>` and add the test
    # to `tests`. When infra reads the final tests.json file, it will read that
    # field and know to flash the product bundle with <name> before running the
    # test.
    #
    # We also assert that the product bundle name is found in
    # product_bundles.json.
    for test_group in test_groups:
        product_bundle_name = test_group["product_bundle_name"]
        if product_bundle_name not in product_bundle_names:
            print(
                f"ERROR: {product_bundle_name} is not a valid product_bundle_name."
            )
            print("Available names are:")
            pprint.pp(product_bundle_names)
            sys.exit(1)

        raw_group_envs = test_group.get("environments", [])
        override_test_environments = test_group.get(
            "override_test_environments", True
        )
        group_environments: list[Environment] = []
        try:
            for e in raw_group_envs:
                env = instance_from_dict(Environment, e)
                validate_known_platform(env, target_platforms, other_platforms)
                group_environments.append(env)
        except ValueError as err:
            validation_errors.append(
                f"product_bundle_test_group:{product_bundle_name}: {err}"
            )
            continue

        group_allowed_device_types = {
            e.dimensions.device_type
            for e in group_environments
            if e.dimensions.device_type is not None
        }
        group_allowed_host_device_types = {
            e.dimensions.host_device_type
            for e in group_environments
            if e.dimensions.host_device_type is not None
        }

        # Read the tests.json that is assigned to this specific product bundle.
        product_bundle_tests_file = build_dir / test_group["tests_json"]
        product_bundle_tests = json.loads(product_bundle_tests_file.read_text())

        # Update the test spec to include the product bundle target and
        # environments.
        for test in product_bundle_tests:
            test_info = test.get("test", {})
            name = test_info["name"] + "-" + product_bundle_name
            test_info["name"] = name
            test_label = test_info.get("label")
            test_id = f"{name} ({test_label})" if test_label else name
            test["product_bundle"] = product_bundle_name

            try:
                resolve_test_environments(
                    test,
                    default_envs=group_environments,
                    allowed_device_types=group_allowed_device_types,
                    allowed_host_device_types=group_allowed_host_device_types,
                    target_platforms=target_platforms,
                    other_platforms=other_platforms,
                    restrict_to_default_envs=True,
                    override_environments=override_test_environments,
                )
            except ValueError as err:
                validation_errors.append(f"{test_id}: {err}")

        tests += product_bundle_tests

    if validation_errors:
        raise ValueError(
            "Invalid test environment specifications found:\n"
            + "\n".join(f"  - {err}" for err in validation_errors)
        )

    if with_bazel_tests:
        # Now get the list of all Bazel tests.
        # TODO(digit): This is just host tests for now, also get the list of
        # Fuchsia Bazel tests.
        fuchsia_dir = Path(__file__).parent.parent
        assert (
            fuchsia_dir / ".jiri_manifest"
        ).exists(), f"Invalid Fuchsia source directory: {fuchsia_dir}"

        # NOTE: Do not use fuchsia_dir here, since unit tests run with a different source dir,
        # that BazelPaths.new() will find by walking up from build_dir.
        bazel_paths = build_utils.BazelPaths.new(build_dir=build_dir)
        bazel_tests_json, bazel_inputs = bazel_tests_utils.generate_tests_json(
            bazel_paths,
            command_runner,
            quiet=quiet,
        )
        tests += bazel_tests_json
    else:
        bazel_inputs = set()

    # Write the final list of tests to tests.json if the contents changed.
    contents_changed = True
    if tests_json_path.exists():
        previous_tests = json.loads(tests_json_path.read_text())
        if previous_tests == tests:
            contents_changed = False
    if contents_changed:
        tests_json_path.write_text(json.dumps(tests, indent=2))

    return {
        tests_from_metadata_path,
        test_groups_path,
        environments_path,
        default_environments_path,
    } | bazel_inputs
